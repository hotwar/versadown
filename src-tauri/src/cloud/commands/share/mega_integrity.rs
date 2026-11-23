//! MEGA downloads are verified after the fact: aria2 only ever sees the
//! plaintext the gateway decrypts, so the MAC in the file key is checked
//! against the finished file. Two sources carry such a key:
//!
//! * a pasted link (`cloud-share://`): the key is read from the encrypted share
//!   plan;
//! * a file of the signed-in account (`cloud://mega/<node>`): the key is read
//!   from the account's own node listing.
//!
//! The task record never carries key material.

use super::*;

/// Tasks whose file is being verified right now, so repeated completion
/// events cannot start a second pass over a large file.
static IN_FLIGHT: Mutex<Option<HashSet<uuid::Uuid>>> = Mutex::new(None);

fn claim(id: uuid::Uuid) -> bool {
    IN_FLIGHT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(id)
}

fn release(id: uuid::Uuid) {
    if let Some(set) = IN_FLIGHT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        set.remove(&id);
    }
}

/// The single local file a completed MEGA task produced.
fn downloaded_file(store: &TaskStore, id: uuid::Uuid) -> Option<PathBuf> {
    let mut files = store
        .files(id)
        .ok()?
        .into_iter()
        .filter(|file| file.selected)
        .map(|file| PathBuf::from(file.path));
    let path = files.next().filter(|path| path.is_absolute())?;
    files.next().is_none().then_some(path)
}

/// Where a MEGA task's file key can be found again.
#[derive(Debug, PartialEq, Eq)]
enum KeySource {
    /// A pasted link: the share plan holds the key.
    SharePlan,
    /// A node of the signed-in account.
    Account(String),
}

fn key_source(source: &str) -> Option<KeySource> {
    if let Ok((CloudProvider::Mega, _, _)) = share::parse_stable_cloud_share_source(source) {
        return Some(KeySource::SharePlan);
    }
    match parse_stable_cloud_source(source) {
        Ok((CloudProvider::Mega, node)) => Some(KeySource::Account(node)),
        _ => None,
    }
}

/// Starts the integrity check of a completed MEGA task, if it is one.
pub(super) fn schedule_mega_integrity_check(app: AppHandle, task: &DownloadTask) {
    if task.status != TaskStatus::Completed {
        return;
    }
    let Some(origin) = key_source(&task.source) else {
        return;
    };
    let id = task.id;
    if !claim(id) {
        return;
    }
    let source = task.source.clone();
    tauri::async_runtime::spawn(async move {
        let worker = app.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            verify_task(&worker, id, &source, &origin)
        })
        .await;
        release(id);
        if let Ok(Some(failure)) = outcome {
            fail_task(&app, id, failure);
        }
    });
}

/// `Some` only when the file was read and does not match its MAC; a check that
/// could not run (file moved, plan gone, signed out) leaves the completed task
/// alone.
fn verify_task(
    app: &AppHandle,
    id: uuid::Uuid,
    source: &str,
    origin: &KeySource,
) -> Option<crate::cloud::mega::MegaFailure> {
    let state = app.try_state::<AppState>()?;
    let cloud = app.try_state::<Arc<CloudState>>()?;
    let path = downloaded_file(&state.store, id)?;
    let result = match origin {
        KeySource::SharePlan => {
            let entry = cloud.share_plans.resolve_entry(source).ok()?;
            crate::cloud::mega::verify_downloaded_file(&entry, &path)
        }
        KeySource::Account(node) => {
            crate::cloud::mega::account::MegaAccountClient::new(&cloud.http, &cloud.store)
                .verify_download(node, &path)
        }
    };
    match result {
        Err(QuarkError::Mega(failure @ crate::cloud::mega::MegaFailure::IntegrityFailed)) => {
            Some(failure)
        }
        _ => None,
    }
}

fn fail_task(app: &AppHandle, id: uuid::Uuid, failure: crate::cloud::mega::MegaFailure) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(mut task) = state.store.find(id) else {
        return;
    };
    if task.status != TaskStatus::Completed {
        return;
    }
    task.status = TaskStatus::Failed;
    task.error = Some(failure.message());
    task.failure_code = Some(failure.code().to_owned());
    let Ok(task) = state.store.apply_transfer_snapshot(task) else {
        return;
    };
    let cloud = app.try_state::<Arc<CloudState>>();
    let _ = crate::commands::emit_task_update_for_task(
        app,
        &state.store,
        cloud.as_ref().map(|cloud| &cloud.share_plans),
        task,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mega_sources_carry_a_key_to_verify_against() {
        assert_eq!(
            key_source("cloud://mega/AbCd1234"),
            Some(KeySource::Account("AbCd1234".into()))
        );
        assert_eq!(key_source("cloud://quark/AbCd1234"), None);
        assert_eq!(key_source("https://example.com/file.bin"), None);
        assert_eq!(key_source("cloud://mega/"), None);
    }
}
