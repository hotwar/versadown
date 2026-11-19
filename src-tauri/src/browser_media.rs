//! Browser requests either select a media URL (v2) or ask the shared desktop
//! extractor to preview and then queue one exact format (v3).
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Manager};
use url::Url;
use uuid::Uuid;

use crate::{
    aria2::Aria2Options,
    commands::{download_submission_options, AppState},
    DownloadService, NewTask, TaskStatus, UriSubmissionOptions,
};

const INVALID: &str = "browserMedia.invalid";
const UNAVAILABLE: &str = "browser media queue is unavailable";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserMediaRequest {
    version: u8,
    pub(crate) request_id: Uuid,
    page_url: String,
    title: String,
    formats: Vec<Format>,
    // Display hints from the player never identify transferable tracks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reported_heights: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Format {
    id: String,
    label: String,
    kind: Kind,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
    evidence: Evidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Direct,
    Hls,
    Dash,
    Pair,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Evidence {
    Player,
    Manifest,
    Observed,
}

pub(crate) fn http_url(value: &str) -> Option<Url> {
    if value.is_empty()
        || value.len() > 16_384
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn text_field(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

impl Format {
    fn valid(&self) -> bool {
        text_field(&self.id, 160)
            && text_field(&self.label, 384)
            && http_url(&self.url).is_some()
            && self.height.is_none_or(|h| (1..=32768).contains(&h))
            && self.mime_type.as_deref().is_none_or(|m| text_field(m, 256))
            && match (self.kind, self.audio_url.as_deref()) {
                (Kind::Pair, Some(audio)) => audio != self.url && http_url(audio).is_some(),
                (Kind::Pair, None) | (_, Some(_)) => false,
                (_, None) => true,
            }
    }

    fn extension(&self) -> String {
        if self.kind != Kind::Direct {
            return "mp4".to_owned();
        }
        let path = http_url(&self.url);
        let ext = path.as_ref().and_then(|u| {
            u.path_segments()?
                .next_back()?
                .rsplit_once('.')
                .map(|s| s.1.to_ascii_lowercase())
        });
        if let Some(ext) = ext.filter(|e| {
            matches!(
                e.as_str(),
                "mp4" | "webm" | "mkv" | "ogv" | "ogg" | "mov" | "m4v" | "ts"
            )
        }) {
            return ext;
        }
        let mime = self
            .mime_type
            .as_deref()
            .and_then(|s| s.split(';').next())
            .unwrap_or("");
        match mime.trim().to_ascii_lowercase().as_str() {
            "video/webm" => "webm",
            "video/ogg" => "ogv",
            "video/mp4" => "mp4",
            "video/quicktime" => "mov",
            _ => "video",
        }
        .to_owned()
    }
}

impl BrowserMediaRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != 2
            || self.request_id.is_nil()
            || http_url(&self.page_url).is_none()
            || !text_field(&self.title, 2048)
            || self.reported_heights.len() > 32
            || !self
                .reported_heights
                .iter()
                .all(|h| (1..=16384).contains(h))
            || self.reported_heights.windows(2).any(|h| h[0] >= h[1])
            || self.formats.len() != 1
            || !self.formats[0].valid()
        {
            return Err(INVALID.into());
        }
        Ok(())
    }

    fn task(&self, format: &Format) -> NewTask {
        let mut stem = String::new();
        for ch in self.title.trim().chars() {
            if stem.len() + ch.len_utf8() > 180 {
                break;
            }
            stem.push(ch);
        }
        let name = crate::cloud::commands::safe_filename(&format!("{stem}.{}", format.extension()));
        NewTask {
            source: self.page_url.clone(),
            name,
            kind: "browser_media".into(),
            is_private: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum BrowserVideoRequest {
    Resolve {
        version: u8,
        request_id: Uuid,
        page_url: String,
    },
    Select {
        version: u8,
        request_id: Uuid,
        preview_id: Uuid,
        format_id: String,
    },
}

impl BrowserVideoRequest {
    pub(crate) fn validate(&self) -> bool {
        match self {
            Self::Resolve {
                version,
                request_id,
                page_url,
            } => *version == 3 && !request_id.is_nil() && http_url(page_url).is_some(),
            Self::Select {
                version,
                request_id,
                preview_id,
                format_id,
            } => {
                *version == 3
                    && !request_id.is_nil()
                    && !preview_id.is_nil()
                    && text_field(format_id, 257)
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct BrowserVideoSubmissions(tokio::sync::Mutex<VecDeque<BrowserVideoReceipt>>);

#[derive(PartialEq, Eq)]
struct BrowserVideoReceipt {
    request_id: Uuid,
    preview_id: Uuid,
    format_id: String,
}

pub(crate) async fn receive_video(
    app: &AppHandle,
    request: BrowserVideoRequest,
) -> Result<serde_json::Value, String> {
    if !request.validate() {
        return Err(INVALID.into());
    }
    let state = wait_for_app_state(app, false).await?;
    match request {
        BrowserVideoRequest::Resolve { page_url, .. } => {
            let url = http_url(&page_url).ok_or(INVALID)?;
            let options = crate::web_video::network_options(&state)?;
            let preview = state.web_video.prepare_browser(url, &options).await?;
            Ok(json!({ "ok": true, "previewId": preview.preview_id, "choices": preview.formats }))
        }
        BrowserVideoRequest::Select {
            request_id,
            preview_id,
            format_id,
            ..
        } => {
            let submissions = app.state::<BrowserVideoSubmissions>();
            let mut receipts = submissions.0.lock().await;
            let receipt = BrowserVideoReceipt {
                request_id,
                preview_id,
                format_id,
            };
            if let Some(existing) = receipts.iter().find(|entry| entry.request_id == request_id) {
                if existing != &receipt {
                    return Err(INVALID.into());
                }
            } else {
                let options = download_submission_options(&state)?;
                let directory = options.directory.ok_or("video.error.output")?;
                let mut network = crate::web_video::network_options(&state)?;
                network.proxy = options.proxy;
                state
                    .web_video
                    .submit_browser(preview_id, &receipt.format_id, &directory, network)
                    .await?;
                if receipts.len() >= 1024 {
                    receipts.pop_front();
                }
                receipts.push_back(receipt);
            }
            show_downloads(app);
            Ok(json!({ "ok": true }))
        }
    }
}

// Concurrent native hosts may redeliver the same request while addUri is in flight.
#[derive(Default)]
pub(crate) struct BrowserMediaSubmissions(tokio::sync::Mutex<()>);

async fn submit(
    service: &DownloadService,
    request: &BrowserMediaRequest,
    options: UriSubmissionOptions,
) -> Result<(), String> {
    let store = service.store();
    if store
        .browser_media_admitted(request.request_id)
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    let format = &request.formats[0];
    let selection = serde_json::to_string(format).map_err(|_| INVALID)?;
    let (mut task, created) = store
        .create_browser_media(request.task(format), request.request_id, &selection)
        .map_err(|e| e.to_string())?;
    // Recover a handoff interrupted before RPC, without reviving a cancelled task.
    if !created && !matches!(task.status, TaskStatus::Paused | TaskStatus::Failed) {
        return Err(UNAVAILABLE.into());
    }
    let result = service.submit_browser_media(&mut task, options).await;
    // addUri can succeed even when the following tellStatus fails. The receipt
    // is committed in the same SQLite statement as the first successful GID bind.
    if store
        .browser_media_admitted(request.request_id)
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    let error = result
        .err()
        .map_or_else(|| UNAVAILABLE.to_owned(), |e| e.to_string());
    store.fail(task.id, &error).map_err(|e| e.to_string())?;
    Err(error)
}

pub(crate) async fn receive(app: &AppHandle, request: BrowserMediaRequest) -> Result<(), String> {
    request.validate()?;
    let state = wait_for_app_state(app, true).await?;
    let gate = app.state::<BrowserMediaSubmissions>();
    let _guard = gate.0.lock().await;
    if !state
        .store
        .browser_media_admitted(request.request_id)
        .map_err(|e| e.to_string())?
    {
        let options = download_submission_options(&state)?;
        let service = state.runtime.service().await.map_err(|e| e.to_string())?;
        submit(&service, &request, options).await?;
    }
    show_downloads(app);
    Ok(())
}

async fn wait_for_app_state(
    app: &AppHandle,
    wait_for_engine: bool,
) -> Result<tauri::State<'_, AppState>, String> {
    // Forwarded warm-launch arguments can arrive while setup is still installing
    // AppState. Engine initialization is also asynchronous on a cold launch.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(state) = app.try_state::<AppState>() {
                if !wait_for_engine || !state.runtime.status().retrying {
                    return state;
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    })
    .await
    .map_err(|_| UNAVAILABLE.to_owned())
}

fn show_downloads(app: &AppHandle) {
    let gui = app.clone();
    let _ =
        app.run_on_main_thread(move || crate::launcher::reveal_manager(&gui, Some("downloads")));
}

pub(crate) fn apply_selection(
    selection: &str,
    page: &str,
    options: &mut Aria2Options,
) -> Option<String> {
    let f: Format = serde_json::from_str(selection).ok()?;
    if !f.valid() {
        return None;
    }
    let referer = format!("{}/", http_url(page)?.origin().ascii_serialization());
    let mode = match f.kind {
        Kind::Direct => "file",
        Kind::Hls => "hls",
        Kind::Dash => "dash",
        Kind::Pair => "collection",
    };
    options.insert("media".into(), mode.into());
    if f.kind == Kind::Direct {
        options.insert("referer".into(), referer);
    } else {
        let mut origins = HashSet::new();
        let contexts: Vec<_> = std::iter::once(&f.url)
            .chain(f.audio_url.iter())
            .filter_map(|u| http_url(u))
            .filter(|u| origins.insert(u.origin().ascii_serialization()))
            .map(|u| json!({"url":u.as_str(), "headers":[{"name":"referer","value":referer}]}))
            .collect();
        options.insert("media-request-contexts".into(), json!(contexts).to_string());
    }
    if let Some(audio) = &f.audio_url {
        let plan = json!({"manifests":[],"keys":[],"tracks":[
            {"id":"v","type":"video","urls":[f.url]},
            {"id":"a","type":"audio","urls":[audio]}
        ]});
        options.insert("media-input".into(), plan.to_string());
        // The collection anchor is not requested by aria2; the input lists both tracks.
        return Some(page.to_owned());
    }
    Some(f.url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn example() -> BrowserMediaRequest {
        serde_json::from_value(json!({"version":2,"requestId":Uuid::new_v4(),"pageUrl":"https://player.example/watch/abc",
            "title":"Two players","formats":[{"id":"second-1080","label":"1080p","height":1080,
            "kind":"pair","url":"https://cdn.example/second/video","audioUrl":"https://audio.example/second/sound",
            "evidence":"player"}]})).unwrap()
    }

    #[test]
    fn requests_reject_non_http_targets_and_incomplete_pairs() {
        for url in [
            "file:///etc/hosts",
            "blob:https://page.example/u",
            "https://user:pass@example.test/s",
            "https://example.test/\r\nTest",
        ] {
            let mut draft = example();
            draft.formats[0].url = url.into();
            assert!(draft.validate().is_err(), "{url}");
        }
        let mut draft = example();
        draft.formats[0].audio_url = None;
        assert!(draft.validate().is_err());
        let mut draft = example();
        draft.formats.push(draft.formats[0].clone());
        assert!(draft.validate().is_err());
    }

    #[test]
    fn only_one_explicitly_selected_source_can_be_submitted() {
        let mut request = example();
        request.reported_heights = vec![720, 1080, 2160];
        assert!(request.validate().is_ok());
        request.version = 1;
        assert!(request.validate().is_err());
        request.version = 2;
        request.formats.clear();
        assert!(request.validate().is_err());
        request.formats = vec![example().formats.remove(0), example().formats.remove(0)];
        request.formats[1].id = "other".into();
        assert!(request.validate().is_err());
    }

    #[test]
    fn persisted_selection_survives_reopen_and_captures_only_selected_tracks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.db");
        let draft = example();
        let format = &draft.formats[0];
        let selection = serde_json::to_string(format).unwrap();
        let store = crate::TaskStore::open_path(&path).unwrap();
        let (task, first) = store
            .create_browser_media(draft.task(format), draft.request_id, &selection)
            .unwrap();
        assert!(first);
        drop(store);
        let store = crate::TaskStore::open_path(&path).unwrap();
        let (again, first) = store
            .create_browser_media(draft.task(format), draft.request_id, &selection)
            .unwrap();
        assert!(!first);
        assert_eq!(task.id, again.id);
        let mut options = Aria2Options::new();
        apply_selection(
            &store.browser_media_plan(task.id).unwrap(),
            &task.source,
            &mut options,
        )
        .unwrap();
        assert_eq!(options["media"], "collection");
        let plan: Value = serde_json::from_str(&options["media-input"]).unwrap();
        assert_eq!(plan["tracks"][0]["urls"][0], format.url);
        assert_eq!(
            plan["tracks"][1]["urls"][0],
            format.audio_url.as_ref().unwrap().as_str()
        );
        let contexts: Value = serde_json::from_str(&options["media-request-contexts"]).unwrap();
        assert_eq!(contexts.as_array().unwrap().len(), 2);
        assert_eq!(
            contexts[0]["headers"][0]["value"],
            "https://player.example/"
        );
    }

    #[tokio::test]
    #[ignore = "requires local TCP"]
    async fn rpc_admission_acknowledges_only_the_selected_source_and_survives_replay() {
        use axum::{extract::State, routing::post, Json, Router};
        use std::sync::{Arc, Mutex};
        type Calls = Arc<Mutex<Vec<Value>>>;
        async fn rpc(State(calls): State<Calls>, Json(body): Json<Value>) -> Json<Value> {
            let mut response = json!({"jsonrpc":"2.0", "id":body["id"]});
            if body["method"] == "aria2.addUri" {
                let mut calls = calls.lock().unwrap();
                calls.push(body["params"].clone());
                if calls.len() != 2 {
                    response["error"] = json!({"code":1,"message":"admission failed"});
                } else {
                    response["result"] = body["params"][2]["gid"].clone();
                }
            } else if calls.lock().unwrap().len() == 3 && body["method"] == "aria2.tellStatus" {
                // A simulated lost addUri response for the next task; aria2 has
                // actually accepted it and can prove ownership of its reserved GID.
                response["result"] = json!({"gid":body["params"][1], "status":"waiting", "totalLength":"0", "completedLength":"0", "downloadSpeed":"0", "uploadSpeed":"0", "connections":"0"});
            } else {
                // A post-admission status error cannot undo the successful addUri.
                response["error"] = json!({"code":1,"message":"status unavailable"});
            }
            Json(response)
        }
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let endpoint = format!("http://{}/jsonrpc", listener.local_addr().unwrap());
        let calls = Calls::default();
        let router = Router::new()
            .route("/jsonrpc", post(rpc))
            .with_state(calls.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let api =
            crate::Aria2RpcClient::new(endpoint, crate::RpcSecret::from_bytes([0; 32])).unwrap();
        let store = Arc::new(crate::TaskStore::open_in_memory().unwrap());
        let service = DownloadService::new(Arc::new(api), store.clone());
        let request = example();
        let dir = tempfile::tempdir().unwrap();
        let options = || UriSubmissionOptions {
            directory: Some(dir.path().to_owned()),
            ..Default::default()
        };
        assert!(submit(&service, &request, options()).await.is_err());
        assert!(!store.browser_media_admitted(request.request_id).unwrap());
        submit(&service, &request, options()).await.unwrap();
        assert!(store.browser_media_admitted(request.request_id).unwrap());
        let (task, created) = store
            .create_browser_media(
                request.task(&request.formats[0]),
                request.request_id,
                "unused",
            )
            .unwrap();
        assert!(!created);
        store.detach_missing_engine_task(task.id).unwrap();
        submit(&service, &request, options()).await.unwrap();
        let next = example();
        submit(&service, &next, options()).await.unwrap();
        assert!(store.browser_media_admitted(next.request_id).unwrap());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][2]["gid"], calls[1][2]["gid"]);
        assert_ne!(calls[1][2]["gid"], calls[2][2]["gid"]);
        let plan: Value =
            serde_json::from_str(calls[1][2]["media-input"].as_str().unwrap()).unwrap();
        assert_eq!(plan["tracks"][0]["urls"][0], request.formats[0].url);
        assert_eq!(
            plan["tracks"][1]["urls"][0],
            request.formats[0].audio_url.as_deref().unwrap()
        );
        server.abort();
    }

    #[test]
    fn explicit_media_kind_accepts_extensionless_manifest() {
        let mut d = example();
        let f = &mut d.formats[0];
        f.url = "https://cdn.example/hls?token=private".into();
        f.audio_url = None;
        f.kind = Kind::Hls;
        let mut options = Aria2Options::new();
        let uri = apply_selection(
            &serde_json::to_string(f).unwrap(),
            &d.page_url,
            &mut options,
        )
        .unwrap();
        assert_eq!(uri, f.url);
        assert_eq!(options["media"], "hls");
    }
}
