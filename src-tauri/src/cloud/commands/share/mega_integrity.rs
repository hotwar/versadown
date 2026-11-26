//! MEGA bytes remain Processing until their MAC has been checked. The store
//! installs that durable barrier before publishing an engine completion.
//! Share plans and account listings retain keys; task records contain no keys.

use super::*;
use crate::cloud::mega::MegaFailure;

/// Separate attempts can share a native GID, so claim the durable marker too.
static IN_FLIGHT: Mutex<Option<HashSet<(uuid::Uuid, String)>>> = Mutex::new(None);

fn claim(attempt: &(uuid::Uuid, String)) -> bool {
    IN_FLIGHT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashSet::new).insert(attempt.clone())
}

fn release(attempt: &(uuid::Uuid, String)) {
    if let Some(set) = IN_FLIGHT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_mut() {
        set.remove(attempt);
    }
}

/// The one selected, absolute output path; any ambiguity fails verification.
fn downloaded_file(store: &TaskStore, id: uuid::Uuid) -> Option<PathBuf> {
    let mut files = store.files(id).ok()?.into_iter()
        .filter(|file| file.selected).map(|file| PathBuf::from(file.path));
    let path = files.next().filter(|path| path.is_absolute())?;
    files.next().is_none().then_some(path)
}

#[derive(Debug, PartialEq, Eq)]
enum KeySource {
    SharePlan,
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

/// Used after AppState and the event sink exist, even when no engine GID was
/// restored. Only unfinished verification is resumed, never completed tasks.
pub(crate) fn resume_pending_mega_integrity_checks(app: AppHandle) {
    let Some(state) = app.try_state::<AppState>() else { return; };
    let Ok(tasks) = state.store.list() else { return; };
    for task in tasks {
        schedule_mega_integrity_check(app.clone(), &task);
    }
}

pub(super) fn schedule_mega_integrity_check(app: AppHandle, task: &DownloadTask) {
    if !crate::store::is_mega_integrity_pending(task) {
        return;
    }
    let attempt = (task.id, task.failure_code.clone().expect("pending marker"));
    if !claim(&attempt) {
        return;
    }
    let pending = task.clone();
    tauri::async_runtime::spawn(async move {
        let worker = app.clone();
        let checked = pending.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || verify_task(&worker, &checked))
            .await.unwrap_or(Err(MegaFailure::IntegrityUnavailable));
        let finished = app.try_state::<AppState>()
            .and_then(|state| state.store.finish_mega_integrity(&pending, outcome).ok().flatten());
        release(&attempt);
        if let Some(task) = finished {
            // The same completion path releases dispatch/gateway slots and
            // refreshes directory aggregates as well as publishing GUI events.
            if let Some(events) = app.try_state::<Arc<crate::TauriDownloadEvents>>() {
                crate::aria2::EventSink::task_updated(events.inner().as_ref(), &task);
            }
        }
    });
}

/// A missing key, missing/unreadable file, provider failure or worker failure
/// is not proof of a valid download, and is distinct from a MAC mismatch.
fn verification_outcome(result: Result<(), QuarkError>) -> Result<(), MegaFailure> {
    match result {
        Ok(()) => Ok(()),
        Err(QuarkError::Mega(MegaFailure::IntegrityFailed)) => Err(MegaFailure::IntegrityFailed),
        Err(_) => Err(MegaFailure::IntegrityUnavailable),
    }
}

fn verify_task(app: &AppHandle, task: &DownloadTask) -> Result<(), MegaFailure> {
    let state = app.try_state::<AppState>().ok_or(MegaFailure::IntegrityUnavailable)?;
    let cloud = app.try_state::<Arc<CloudState>>().ok_or(MegaFailure::IntegrityUnavailable)?;
    let path = downloaded_file(&state.store, task.id).ok_or(MegaFailure::IntegrityUnavailable)?;
    match key_source(&task.source).ok_or(MegaFailure::IntegrityUnavailable)? {
        KeySource::SharePlan => {
            let entry = cloud.share_plans.resolve_entry(&task.source)
                .map_err(|_| MegaFailure::IntegrityUnavailable)?;
            verification_outcome(crate::cloud::mega::verify_downloaded_file(&entry, &path))
        }
        KeySource::Account(node) => verification_outcome(
            crate::cloud::mega::account::MegaAccountClient::new(&cloud.http, &cloud.store)
                .verify_download(&node, &path),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mega_sources_carry_a_key_to_verify_against() {
        assert_eq!(key_source("cloud://mega/AbCd1234"), Some(KeySource::Account("AbCd1234".into())));
        assert_eq!(key_source("cloud://quark/AbCd1234"), None);
        assert_eq!(key_source("https://example.com/file.bin"), None);
        assert_eq!(key_source("cloud://mega/"), None);
    }

    #[test]
    fn missing_keys_and_io_failures_never_count_as_verified_or_as_a_mac_mismatch() {
        assert_eq!(verification_outcome(Ok(())), Ok(()));
        assert_eq!(
            verification_outcome(Err(QuarkError::Mega(MegaFailure::IntegrityFailed))),
            Err(MegaFailure::IntegrityFailed)
        );
        for error in [QuarkError::Storage, QuarkError::NotAuthorized,
            QuarkError::SourceFileMissing, QuarkError::InvalidResponse]
        {
            assert_eq!(verification_outcome(Err(error)), Err(MegaFailure::IntegrityUnavailable));
        }
    }
}
