//! The embedded browser window of providers whose public shares are captured
//! (`ProviderDescriptor::public_shares_browser_capture`, today Gofile).
//!
//! The provider's own page runs in the window and the user operates it. The
//! window only watches for the final file request: it cancels the browser's
//! own download and queues the same request as an ordinary download, with the
//! visitor cookie and user agent the page used. It never calls the provider's
//! API and never builds a request of its own.

use super::*;
use crate::cloud::gofile::{
    is_file_request, is_page_navigation_allowed, share_page_url, CapturedDownload,
};
use serde::Serialize;
use std::time::{Duration, Instant};
use tauri::webview::DownloadEvent;
use url::Url;

const WINDOW_LABEL_PREFIX: &str = "browser-share-";
const MAX_TITLE_CHARS: usize = 160;
/// One click can surface the same file request through several window hooks.
const DUPLICATE_REQUEST_WINDOW: Duration = Duration::from_secs(5);
const CAPTURE_EVENT: &str = "cloud-browser-share-capture";
const CLOSED_EVENT: &str = "cloud-browser-share-closed";
const NOTICE_SECONDS: u32 = 8;

/// What happened to one file request the user started in the window.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserShareCaptureEvent {
    pub provider: CloudProvider,
    /// The canonical share page the window shows; the new-task dialog matches
    /// its draft for that share by it.
    pub share_page: String,
    /// `queued`, `duplicate` (already in the list) or `failed`.
    pub outcome: &'static str,
    pub name: String,
    /// A localization key or a short Chinese text; never provider text.
    pub message: Option<String>,
}

/// The share window went away, whether or not a file was taken over first.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserShareClosedEvent {
    pub share_page: String,
}

impl BrowserShareCaptureEvent {
    /// A taken-over file is in the download list (new or already there): the
    /// share window has done its job. A failure leaves it open for a retry.
    fn finishes_window(&self) -> bool {
        matches!(self.outcome, "queued" | "duplicate")
    }
}

/// Script that shows `message` as a short-lived banner inside the share
/// window. The text is app-supplied (localized) and passed as a JSON string
/// literal, never spliced into code; the page's own scripts are not involved.
fn notice_script(message: &str) -> Option<String> {
    let text = clean_title(message);
    if text.is_empty() {
        return None;
    }
    let literal = serde_json::to_string(&text).ok()?;
    Some(format!(
        "(function(m){{var id='__versadown_notice',e=document.getElementById(id);\
if(e)e.remove();e=document.createElement('div');e.id=id;e.textContent=m;\
e.style.cssText='position:fixed;left:50%;top:16px;transform:translateX(-50%);\
z-index:2147483647;max-width:80vw;padding:10px 16px;border-radius:8px;\
background:#b42318;color:#fff;font:14px/1.4 sans-serif;\
box-shadow:0 4px 16px rgba(0,0,0,.4)';document.documentElement.appendChild(e);\
setTimeout(function(){{e.remove()}},{NOTICE_SECONDS}000)}})({literal});"
    ))
}

/// The one provider whose share pages are captured, with its canonical page.
pub(crate) fn browser_share_page(raw: &str) -> Option<(CloudProvider, Url)> {
    share_page_url(raw).map(|page| (CloudProvider::Gofile, page))
}

/// Validated by the application before the untrusted share page is opened.
#[derive(Clone)]
struct CaptureContext {
    configured: UriSubmissionOptions,
    private_download: bool,
    reopened_task: Option<uuid::Uuid>,
}

impl CaptureContext {
    fn submission(&self, state: &AppState, source: &str) -> Result<(UriSubmissionOptions, bool), QuarkError> {
        let Some(id) = self.reopened_task else {
            ensure_private_download_enabled(state, self.private_download).map_err(QuarkError::InvalidInput)?;
            return Ok((self.configured.clone(), self.private_download));
        };
        // A window can outlive a privacy lock, task deletion, or path update.
        // Never accept task privacy or output paths from the provider page.
        let task = state.store.find(id).map_err(|_| QuarkError::Storage)?;
        ensure_private_task_access(state, &task).map_err(QuarkError::InvalidInput)?;
        self.for_task(&state.store, &task, source)
    }

    fn for_task(&self, store: &TaskStore, task: &DownloadTask, source: &str) -> Result<(UriSubmissionOptions, bool), QuarkError> {
        let mut configured = existing_capture_options(store, task, self.configured.clone())?;
        if task.source != source {
            // Another file selected from this page inherits the directory and
            // privacy, but gets its own name and output reservation.
            configured.output_name = None;
        }
        Ok((configured, self.private_download || task.is_private))
    }
}

fn existing_capture_options(
    store: &TaskStore,
    task: &DownloadTask,
    mut configured: UriSubmissionOptions,
) -> Result<UriSubmissionOptions, QuarkError> {
    let output = store.reserved_output_path(task.id).map_err(|_| QuarkError::Storage)?
        .or(store.files(task.id).map_err(|_| QuarkError::Storage)?.into_iter()
            .map(|file| PathBuf::from(file.path)).find(|path| path.is_absolute()))
        .filter(|path| path.is_absolute())
        .ok_or_else(|| QuarkError::InvalidInput("cloud.browserShare.missingDestination".into()))?;
    configured.directory = output.parent().map(Path::to_path_buf);
    configured.output_name = output.file_name().and_then(|name| name.to_str()).map(str::to_owned);
    Ok(configured)
}

/// Reserve before provider resolution, so failures and application restarts
/// cannot replace the selected destination with the global default.
pub(super) fn reserve_capture_output(
    store: &TaskStore,
    task: &DownloadTask,
    mut configured: UriSubmissionOptions,
) -> Result<UriSubmissionOptions, QuarkError> {
    let directory = configured.directory.as_deref().ok_or_else(||
        QuarkError::InvalidInput("cloud.browserShare.missingDestination".into()))?;
    let name = configured.output_name.as_deref().unwrap_or(&task.name);
    let output = crate::reserve_unique_output_path(store, task.id, directory, name)
        .map_err(|_| QuarkError::Storage)?;
    configured.directory = output.parent().map(Path::to_path_buf);
    configured.output_name = output.file_name().and_then(|name| name.to_str()).map(str::to_owned);
    Ok(configured)
}

fn window_label(page: &Url, context: &CaptureContext) -> String {
    use std::hash::{Hash, Hasher};
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    context.configured.directory.hash(&mut identity);
    context.private_download.hash(&mut identity);
    context.reopened_task.hash(&mut identity);
    let code = page.path().rsplit('/').next().unwrap_or_default();
    format!(
        "{WINDOW_LABEL_PREFIX}{}-{code}-{:016x}",
        CloudProvider::Gofile.key(), identity.finish()
    )
}

fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Navigations, new windows and downloads of one window all report here.
struct CaptureSession {
    app: AppHandle,
    label: String,
    provider: CloudProvider,
    share_page: String,
    context: CaptureContext,
    /// Localized text for "this download was not taken over", shown in the
    /// window itself where the user is looking.
    failure_notice: String,
    user_agent: Mutex<Option<String>>,
    recent: Mutex<HashMap<String, Instant>>,
}

impl CaptureSession {
    /// Takes over `url` when it is a file download of this provider. Returns
    /// whether it was a file request, so the caller cancels the browser's
    /// own handling either way.
    fn offer(self: &Arc<Self>, url: &Url, suggested_name: Option<String>) -> bool {
        if !is_file_request(url) {
            return false;
        }
        if !self.first_sighting(url) {
            return true;
        }
        let session = Arc::clone(self);
        let url = url.clone();
        tauri::async_runtime::spawn(async move {
            let event = match session.queue(url, suggested_name).await {
                Ok((name, duplicate)) => BrowserShareCaptureEvent {
                    provider: session.provider,
                    share_page: session.share_page.clone(),
                    outcome: if duplicate { "duplicate" } else { "queued" },
                    name,
                    message: None,
                },
                Err(error) => BrowserShareCaptureEvent {
                    provider: session.provider,
                    share_page: session.share_page.clone(),
                    outcome: "failed",
                    name: String::new(),
                    message: Some(failure_message(&error)),
                },
            };
            let finished = event.finishes_window();
            let _ = session.app.emit(CAPTURE_EVENT, event);
            if finished {
                session.finish();
            } else {
                session.show_failure_notice();
            }
        });
        true
    }

    /// The file is queued: close the share window and bring the download
    /// list forward, where the new task is. Another file can be started from
    /// the task's "reopen share page" action.
    fn finish(&self) {
        if let Some(window) = self.app.get_webview_window(&self.label) {
            let _ = window.close();
        }
        crate::launcher::reveal_manager(&self.app, None);
    }

    /// A download that was cancelled but not taken over; the window stays.
    fn show_failure_notice(&self) {
        let Some(script) = notice_script(&self.failure_notice) else {
            return;
        };
        if let Some(window) = self.app.get_webview_window(&self.label) {
            let _ = window.eval(script);
        }
    }

    fn first_sighting(&self, url: &Url) -> bool {
        let Ok(mut recent) = self.recent.lock() else {
            return true;
        };
        recent.retain(|_, seen| seen.elapsed() < DUPLICATE_REQUEST_WINDOW);
        recent
            .insert(url.as_str().to_owned(), Instant::now())
            .is_none()
    }

    fn remember_user_agent(&self, user_agent: String) {
        if let Ok(mut slot) = self.user_agent.lock() {
            *slot = Some(user_agent);
        }
    }

    /// Reads the visitor cookies, stores the capture in the encrypted plan
    /// store and submits it like any other cloud-share file.
    async fn queue(
        &self,
        url: Url,
        suggested_name: Option<String>,
    ) -> Result<(String, bool), QuarkError> {
        let window = self
            .app
            .get_webview_window(&self.label)
            .ok_or(QuarkError::InvalidResponse)?;
        let cookie_url = url.as_str().to_owned();
        // Cookie access must stay off the window's event thread (Windows
        // deadlocks otherwise), hence the blocking worker.
        let cookies = tauri::async_runtime::spawn_blocking(move || {
            browser_cookies(&window, &[cookie_url.as_str()])
        })
        .await
        .map_err(|_| QuarkError::Storage)??;
        let captured = CapturedDownload {
            share_page: self.share_page.clone(),
            url,
            suggested_name,
            cookie: (!cookies.is_empty()).then(|| cookies.join("; ")),
            user_agent: self.user_agent.lock().ok().and_then(|agent| agent.clone()),
        };
        submit_captured_download(&self.app, &captured, &self.context).await
    }
}

fn failure_message(error: &QuarkError) -> String {
    if let QuarkError::InvalidInput(message) = error {
        if message == "cloud.browserShare.missingDestination" { return message.clone(); }
    }
    share_resolution_failure_message(error)
}

/// Stores the capture and queues it. Returns the file name and whether the
/// file was already in the download list.
async fn submit_captured_download(
    app: &AppHandle,
    captured: &CapturedDownload,
    context: &CaptureContext,
) -> Result<(String, bool), QuarkError> {
    let plan = captured.into_plan()?;
    let entry = plan.entries[0].clone();
    let source = plan.source_for(&entry.entry_id);
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;
    let (configured, private_download) = context.submission(&state, &source)?;
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    cloud.share_plans.upsert(&plan)?;
    let selection = share::selected_share_download(
        &cloud.share_plans,
        &source,
        std::slice::from_ref(&entry.entry_fingerprint),
    )?;
    preflight_cloud_share_download(app.clone(), &selection).await?;
    let action =
        submit_cloud_share_file(app.clone(), &state, source, configured, private_download, None).await?;
    let _ = crate::commands::emit_task_update_for_task(
        app,
        &state.store,
        Some(&cloud.share_plans),
        action.payload.task.clone(),
    );
    let name = if should_emit_task_update(&state, action.payload.task.is_private) {
        entry.name
    } else {
        String::new()
    };
    Ok((name, action.duplicate.is_some()))
}

/// Opens (or brings forward) the window for a pasted share link.
fn open_browser_share_window(
    app: &AppHandle,
    share_link: &str,
    title: &str,
    failure_notice: &str,
    context: CaptureContext,
) -> Result<(), QuarkError> {
    let (provider, page) = browser_share_page(share_link)
        .ok_or_else(|| QuarkError::InvalidInput("分享链接无效".into()))?;
    let label = window_label(&page, &context);
    let title = clean_title(title);
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.set_title(&title);
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }
    let session = Arc::new(CaptureSession {
        app: app.clone(),
        label: label.clone(),
        provider,
        share_page: page.as_str().to_owned(),
        context,
        failure_notice: failure_notice.to_owned(),
        user_agent: Mutex::new(None),
        recent: Mutex::new(HashMap::new()),
    });
    let navigation = Arc::clone(&session);
    let new_window = Arc::clone(&session);
    let download = Arc::clone(&session);
    let loaded = Arc::clone(&session);
    let window = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(page))
        .title(title)
        .inner_size(1100.0, 780.0)
        // Nothing of the visit (the page's guest account, cookies) is kept
        // on disk once the window closes.
        .incognito(true)
        .on_navigation(move |url| {
            if navigation.offer(url, None) {
                return false;
            }
            is_page_navigation_allowed(url)
        })
        .on_new_window(move |url, _| {
            new_window.offer(&url, None);
            NewWindowResponse::Deny
        })
        .on_download(move |_, event| {
            if let DownloadEvent::Requested { url, destination } = event {
                let suggested = destination
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                // Whatever the page started, the browser never saves it itself.
                if !download.offer(&url, suggested) {
                    download.show_failure_notice();
                }
                return false;
            }
            true
        })
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let session = Arc::clone(&loaded);
                let _ = window.eval_with_callback("navigator.userAgent", move |value| {
                    if let Ok(agent) = serde_json::from_str::<String>(&value) {
                        session.remember_user_agent(agent);
                    }
                });
            }
        })
        .build()
        .map_err(|_| QuarkError::WebAuthorizationRequired)?;
    // A capture emits its event before the window closes, so a listener
    // sees "captured" first and "closed" only matters when nothing was.
    let app = app.clone();
    let share_page = session.share_page.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            let _ = app.emit(
                CLOSED_EVENT,
                BrowserShareClosedEvent {
                    share_page: share_page.clone(),
                },
            );
        }
    });
    Ok(())
}

/// Opens the share page of a pasted link in the capture window.
#[tauri::command]
pub async fn cloud_open_browser_share(
    app: AppHandle,
    url: String,
    title: String,
    failure_notice: String,
    destination_directory: Option<String>,
    private_download: Option<bool>,
) -> Result<(), QuarkError> {
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;
    let private_download = private_download.unwrap_or(false);
    ensure_private_download_enabled(&state, private_download).map_err(QuarkError::InvalidInput)?;
    let configured = crate::commands::download_submission_options_for_directory(
        &state, destination_directory.as_deref(),
    ).map_err(QuarkError::InvalidInput)?;
    open_browser_share_window(&app, &url, &title, &failure_notice, CaptureContext {
        configured, private_download, reopened_task: None,
    })
}

/// Opens the share page a captured task came from, so the user can start the
/// file again and get a fresh download request.
#[tauri::command]
pub async fn cloud_reopen_browser_share(
    app: AppHandle,
    task_id: String,
    title: String,
    failure_notice: String,
) -> Result<(), QuarkError> {
    let id = uuid::Uuid::parse_str(&task_id)
        .map_err(|_| QuarkError::InvalidInput("任务 ID 无效".into()))?;
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;
    let task = state.store.find(id).map_err(|_| QuarkError::Storage)?;
    ensure_private_task_access(&state, &task).map_err(QuarkError::InvalidInput)?;
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let plan = cloud.share_plans.resolve_entry(&task.source)?.plan;
    let page = plan.provider_context["share_page"]
        .as_str()
        .ok_or_else(|| QuarkError::InvalidInput("无法找到原分享页".into()))?;
    let configured = existing_capture_options(&state.store, &task, UriSubmissionOptions::default())?;
    let configured = crate::commands::download_submission_options_for_directory(
        &state, configured.directory.as_ref().and_then(|path| path.to_str()),
    ).map_err(QuarkError::InvalidInput)?;
    open_browser_share_window(&app, page, &title, &failure_notice, CaptureContext {
        configured, private_download: task.is_private, reopened_task: Some(id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_share_links_open_a_window_and_each_page_has_its_own_label() {
        let (provider, page) = browser_share_page("https://www.gofile.io/d/AbC123?x=1").unwrap();
        assert_eq!(provider, CloudProvider::Gofile);
        let context = CaptureContext {
            configured: UriSubmissionOptions::default(), private_download: false, reopened_task: None,
        };
        assert!(window_label(&page, &context).starts_with("browser-share-gofile-AbC123-"));
        let mut private = context.clone();
        private.private_download = true;
        assert_ne!(window_label(&page, &context), window_label(&page, &private));
        let mut other_directory = context.clone();
        other_directory.configured.directory = Some(PathBuf::from("different"));
        assert_ne!(window_label(&page, &context), window_label(&page, &other_directory));
        assert!(browser_share_page("https://example.com/d/AbC123").is_none());
        assert!(browser_share_page("https://store1.gofile.io/download/web/a/b.bin").is_none());
    }

    #[test]
    fn capture_reserves_selected_destination_before_engine_submission() {
        let directory = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        for private in [false, true] {
            let task = store.create(NewTask {
                source: format!("cloud-share://gofile/share/file-{private}"),
                name: format!("file-{private}.zip"), kind: "cloud".into(), is_private: private,
            }).unwrap();
            let configured = reserve_capture_output(&store, &task, UriSubmissionOptions {
                directory: Some(directory.path().to_path_buf()), ..Default::default()
            }).unwrap();
            assert_eq!(configured.directory.as_deref(), Some(directory.path()));
            assert_eq!(store.reserved_output_path(task.id).unwrap(), Some(directory.path().join(&task.name)));
            assert_eq!(store.find(task.id).unwrap().is_private, private);
        }
    }

    #[test]
    fn recapture_after_restart_keeps_private_task_destination_and_name() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let chosen = directory.path().join("chosen");
        let task = {
            let store = TaskStore::open_path(&database).unwrap();
            let task = store.create(NewTask {
                source: "cloud-share://gofile/share/file".into(), name: "file.zip".into(),
                kind: "cloud".into(), is_private: true,
            }).unwrap();
            reserve_capture_output(&store, &task, UriSubmissionOptions {
                directory: Some(chosen.clone()), ..Default::default()
            }).unwrap();
            store.fail_with_code(task.id, Some("gofile_link_expired"), "expired").unwrap()
        };
        let store = TaskStore::open_path(&database).unwrap();
        let context = CaptureContext {
            configured: UriSubmissionOptions { directory: Some(directory.path().join("new-global-default")), ..Default::default() },
            private_download: false, reopened_task: Some(task.id),
        };
        let persisted = store.find(task.id).unwrap();
        let (options, private) = context.for_task(&store, &persisted, &persisted.source).unwrap();
        assert!(private);
        assert_eq!(options.directory, Some(chosen.clone()));
        assert_eq!(options.output_name.as_deref(), Some("file.zip"));
        let retried = reserve_capture_output(&store, &persisted, options).unwrap();
        assert_eq!(retried.directory, Some(chosen.clone()));
        let (another_file, private) = context.for_task(&store, &persisted, "cloud-share://gofile/share/other").unwrap();
        assert!(private);
        assert_eq!(another_file.directory, Some(chosen));
        assert_eq!(another_file.output_name, None);
    }

    #[test]
    fn recapture_without_a_recorded_path_does_not_guess_the_global_destination() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store.create(NewTask::direct("https://example.test/file.zip")).unwrap();
        assert!(existing_capture_options(&store, &task, UriSubmissionOptions::default()).is_err());
    }

    fn event(outcome: &'static str) -> BrowserShareCaptureEvent {
        BrowserShareCaptureEvent {
            provider: CloudProvider::Gofile,
            share_page: "https://gofile.io/d/AbC123".into(),
            outcome,
            name: String::new(),
            message: None,
        }
    }

    #[test]
    fn the_window_closes_only_once_the_file_is_in_the_download_list() {
        assert!(event("queued").finishes_window());
        assert!(event("duplicate").finishes_window());
        assert!(!event("failed").finishes_window());
    }

    #[test]
    fn events_carry_the_share_page_so_a_draft_can_be_matched() {
        let json = serde_json::to_value(event("queued")).unwrap();
        assert_eq!(json["sharePage"], "https://gofile.io/d/AbC123");
        let closed = serde_json::to_value(BrowserShareClosedEvent {
            share_page: "https://gofile.io/d/AbC123".into(),
        })
        .unwrap();
        assert_eq!(closed["sharePage"], "https://gofile.io/d/AbC123");
    }

    #[test]
    fn the_window_notice_is_one_inert_string_literal() {
        let script = notice_script("下载未被接管\n\"</script>'); alert(1);//").unwrap();
        // The text only ever reaches `textContent` through a JSON literal.
        assert!(script.contains(r#"("下载未被接管\"</script>'); alert(1);//")"#));
        assert!(!script.contains('\n'));
        assert!(notice_script("  \n ").is_none());
    }

    #[test]
    fn window_titles_are_single_line_and_bounded() {
        assert_eq!(clean_title("a\nb\u{7}c"), "abc");
        assert_eq!(
            clean_title(&"x".repeat(1000)).chars().count(),
            MAX_TITLE_CHARS
        );
    }
}
