//! Opt-in browser acceptance bridge. Only window-shell state is replaced by
//! the browser harness; preparation and video transfers use production code.
use super::*;
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State as HttpState},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Clone)]
struct ProbeState {
    video: Arc<VideoDownloads>,
    store: Arc<TaskStore>,
    output: PathBuf,
    proxy: Option<String>,
    tools: Value,
    began: Instant,
    calls: Arc<Mutex<Vec<Value>>>,
    shutdown: Arc<Notify>,
    legacy_scan: bool,
    matcher_failure: bool,
}

#[derive(Deserialize)]
struct InvokeRequest {
    command: String,
    #[serde(default)]
    args: Value,
}

async fn invoke(
    HttpState(state): HttpState<ProbeState>,
    Json(request): Json<InvokeRequest>,
) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let started_ms = state.began.elapsed().as_secs_f64() * 1000.0;
        let result = handle(&state, &request, &sender).await;
        let finished_ms = state.began.elapsed().as_secs_f64() * 1000.0;
        let record = json!({
            "command": request.command,
            "input": request.args.get("input"),
            "probeVideo": request.args.get("probeVideo"),
            "startedMs": started_ms,
            "finishedMs": finished_ms,
            "elapsedMs": finished_ms - started_ms,
            "ok": result.is_ok(),
            "error": result.as_ref().err(),
            "result": result.as_ref().ok().map(|value| result_summary(&request.command, value)),
            "tools": state.tools,
            "matcherFailureInjected": state.matcher_failure,
        });
        println!("VD-UI-CALL {record}");
        state.calls.lock().await.push(record);
        let _ = sender.send(match result {
            Ok(value) => json!({"type": "result", "ok": true, "value": value}),
            Err(error) => json!({"type": "result", "ok": false, "error": error}),
        });
    });
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|value| {
            (Ok::<_, std::convert::Infallible>(Bytes::from(format!("{value}\n"))), receiver)
        })
    });
    Response::builder().header("Content-Type", "application/x-ndjson")
        .header("Cache-Control", "no-store")
        .body(Body::from_stream(stream)).unwrap()
}

fn result_summary(command: &str, value: &Value) -> Value {
    match command {
        "scan_download_inputs" => json!({"entries": value}),
        "parse_download" => json!({
            "kind": value.get("kind"),
            "formatCount": value.get("formats").and_then(Value::as_array).map(Vec::len),
            "formats": value.get("formats"),
            "taskId": value.pointer("/task/id"),
            "taskKind": value.pointer("/task/kind"),
            "requiresConfirmation": value.get("requires_confirmation"),
        }),
        "submit_video_download" => json!({"taskId": value.get("id"), "kind": value.get("kind")}),
        "list_tasks" => json!({"tasks": value}),
        _ => Value::Null,
    }
}

fn submission_directory(state: &ProbeState, requested: Option<&str>) -> Result<PathBuf, String> {
    if let Some(requested) = requested {
        let requested = Path::new(requested);
        let expected = state.output.canonicalize().map_err(|_| "invalid VD_UI_OUTPUT")?;
        if !requested.is_absolute() || requested.canonicalize().map_err(|_| "invalid requested directory")? != expected {
            return Err("UI probe downloads are restricted to VD_UI_OUTPUT".into());
        }
    }
    Ok(state.output.clone())
}

async fn handle(
    state: &ProbeState,
    request: &InvokeRequest,
    sender: &tokio::sync::mpsc::UnboundedSender<Value>,
) -> Result<Value, String> {
    let input = || {
        request.args.get("input").and_then(Value::as_str)
            .ok_or_else(|| "missing input".to_owned())
    };
    match request.command.as_str() {
        "scan_download_inputs" => {
            let progress = |entries: &[crate::ScannedInput], pending: &[String]| {
                println!("VD-UI-SCAN-PROGRESS {}", json!({"entries": entries, "pending": pending, "matcherFailureInjected": state.matcher_failure}));
                if !state.legacy_scan {
                    let _ = sender.send(json!({"type": "progress", "value": {"entries": entries, "pending": pending}}));
                }
            };
            let entries = if state.matcher_failure {
                routing::scan_with_matcher_result_for_test(input()?, progress, Err("video.error.extract".to_owned())).await
            } else {
                routing::scan_with_progress(input()?, progress).await
            };
            serde_json::to_value(entries).map_err(|e| e.to_string())
        }
        "parse_download" => {
            let probe = request.args.get("probeVideo").and_then(Value::as_bool).unwrap_or(false);
            let require_confirmation = request.args.get("requireConfirmation").and_then(Value::as_bool).unwrap_or(false);
            let outcome = commands::prepare_non_share_download(
                input()?, &state.store, &state.video,
                || Ok(VideoOptions { proxy: state.proxy.clone(), ..Default::default() }),
                probe, require_confirmation,
            ).await?;
            serde_json::to_value(outcome).map_err(|e| e.to_string())
        }
        "discard_prepared_download" => {
            let id = request.args.get("taskId").and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| "invalid taskId".to_owned())?;
            commands::discard_prepared_download_without_engine(&state.store, id)?;
            Ok(Value::Null)
        }
        "submit_video_download" => {
            if request.args.get("privateDownload").and_then(Value::as_bool).unwrap_or(false) {
                return Err("private downloads are not enabled in the UI probe".into());
            }
            let preview_id = request.args.get("previewId").and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| "invalid previewId".to_owned())?;
            let format_id = request.args.get("formatId").and_then(Value::as_str)
                .ok_or_else(|| "missing formatId".to_owned())?;
            let directory = submission_directory(state, request.args.get("destinationDirectory").and_then(Value::as_str))?;
            let task = state.video.submit(
                preview_id, format_id, &directory, false,
                request.args.get("postProcessingNote").and_then(Value::as_str),
                VideoOptions { proxy: state.proxy.clone(), ..Default::default() },
            ).await?;
            serde_json::to_value(commands::task_payload(&state.store, task)?).map_err(|e| e.to_string())
        }
        "list_tasks" => {
            let tasks = commands::list_queue_tasks_for_scope_with_privacy(
                &state.store, request.args.get("scope").and_then(Value::as_str), false,
            )?;
            serde_json::to_value(tasks).map_err(|e| e.to_string())
        }
        "discard_video_preview" => {
            let id = request.args.get("previewId").and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| "invalid previewId".to_owned())?;
            state.video.previews.discard(id)?;
            Ok(Value::Null)
        }
        "get_settings" => serde_json::to_value(crate::AppSettings {
            download_directory: state.output.to_string_lossy().into_owned(),
            ..Default::default()
        }).map_err(|e| e.to_string()),
        _ => Err(format!("UI probe does not implement {}", request.command)),
    }
}

async fn calls(HttpState(state): HttpState<ProbeState>) -> Json<Value> {
    Json(json!({"calls": state.calls.lock().await.clone()}))
}

async fn shutdown(HttpState(state): HttpState<ProbeState>) -> Json<Value> {
    state.shutdown.notify_one();
    Json(json!({"ok": true}))
}

async fn tool_identity(name: &str, version_arg: &str) -> Value {
    let Some(path) = extractor::tool(name) else { return json!({"available": false}) };
    let mut command = tokio::process::Command::new(&path);
    command.arg(version_arg).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    match tokio::time::timeout(Duration::from_secs(15), command.output()).await {
        Ok(Ok(output)) => json!({
            "path": path, "exitCode": output.status.code(),
            "version": String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("").trim(),
        }),
        Ok(Err(error)) => json!({"path": path, "error": error.to_string()}),
        Err(_) => json!({"path": path, "error": "version probe timed out"}),
    }
}

async fn health(HttpState(state): HttpState<ProbeState>) -> Json<Value> {
    Json(json!({
        "ready": true, "mode": "production-non-share-preparation-and-video-transfer",
        "manifestDirectory": env!("CARGO_MANIFEST_DIR"),
        "declaredSourceSha": std::env::var("VD_UI_SOURCE_SHA").ok(),
        "cookiesLoaded": false, "legacyScan": state.legacy_scan,
        "matcherFailureInjected": state.matcher_failure,
        "tools": state.tools, "outputDirectory": state.output,
    }))
}

fn test_state(output: PathBuf) -> ProbeState {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    ProbeState {
        video: Arc::new(VideoDownloads::new(store.clone(), 1, |_| {})),
        store, output, proxy: None, tools: Value::Null,
        began: Instant::now(), calls: Default::default(),
        shutdown: Arc::new(Notify::new()), legacy_scan: false, matcher_failure: false,
    }
}

#[tokio::test]
async fn ui_bridge_uses_real_plain_preparation_task_listing_and_discard() {
    let directory = tempfile::tempdir().unwrap();
    let state = test_state(directory.path().to_path_buf());
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let parsed = handle(&state, &InvokeRequest {
        command: "parse_download".into(),
        args: json!({"input": "http://127.0.0.1/page.shtml", "requireConfirmation": true}),
    }, &sender).await.unwrap();
    assert_eq!(parsed["kind"], "task");
    assert_eq!(parsed["task"]["name"], "page.shtml");
    assert_eq!(parsed["requires_confirmation"], true);
    assert_eq!(result_summary("parse_download", &parsed)["taskKind"], "file");
    let id = parsed["task"]["id"].clone();
    let tasks = handle(&state, &InvokeRequest {
        command: "list_tasks".into(), args: json!({}),
    }, &sender).await.unwrap();
    assert!(tasks.as_array().unwrap().iter().any(|task| task["id"] == id));
    let settings = handle(&state, &InvokeRequest {
        command: "get_settings".into(), args: json!({}),
    }, &sender).await.unwrap();
    assert_eq!(settings["downloadDirectory"], directory.path().to_string_lossy().as_ref());
    handle(&state, &InvokeRequest {
        command: "discard_prepared_download".into(), args: json!({"taskId": id}),
    }, &sender).await.unwrap();
    assert!(state.store.list().unwrap().is_empty());
}

#[tokio::test]
async fn ui_bridge_rejects_private_and_outside_directory_video_submissions() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let state = test_state(directory.path().to_path_buf());
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    for args in [
        json!({"privateDownload": true}),
        json!({"previewId": Uuid::new_v4(), "formatId": "360p", "destinationDirectory": outside.path()}),
    ] {
        assert!(handle(&state, &InvokeRequest { command: "submit_video_download".into(), args }, &sender).await.is_err());
    }
    assert!(state.store.list().unwrap().is_empty());
    assert_eq!(submission_directory(&state, None).unwrap(), directory.path());
}

#[tokio::test]
async fn ui_bridge_matcher_failure_mode_emits_progress_and_real_video_fallback_routes() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = test_state(directory.path().to_path_buf());
    state.matcher_failure = true;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let entries = handle(&state, &InvokeRequest {
        command: "scan_download_inputs".into(),
        args: json!({"input": "https://www.acfun.cn/v/ac48980313\nhttps://tv.cctv.com/2026/10/01/VIDEgXcuU2L3NYhZgJDaWj8A261001.shtml"}),
    }, &sender).await.unwrap();
    let initial = receiver.try_recv().unwrap();
    assert_eq!(initial["type"], "progress");
    assert_eq!(initial["value"]["pending"].as_array().unwrap().len(), 2);
    assert!(entries.as_array().unwrap().iter().all(|entry| entry["kind"] == "web_video"));
    assert!(state.store.list().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "long-running real video UI acceptance bridge; requires installed yt-dlp and explicit VD_UI_PORT"]
async fn serve_real_video_ui_probe() {
    extractor::set_resource_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    let port: u16 = std::env::var("VD_UI_PORT").expect("set VD_UI_PORT")
        .parse().expect("VD_UI_PORT must be a port number");
    assert!(port > 0, "VD_UI_PORT must select a concrete loopback port");
    let timeout = std::env::var("VD_UI_TIMEOUT_SECS").ok()
        .and_then(|value| value.parse::<u64>().ok()).unwrap_or(900).clamp(1, 900);
    let output = PathBuf::from(std::env::var_os("VD_UI_OUTPUT").expect("set VD_UI_OUTPUT"));
    assert!(output.is_absolute(), "VD_UI_OUTPUT must be an absolute path");
    std::fs::create_dir_all(&output).expect("create VD_UI_OUTPUT");
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let tools = json!({
        "ytDlp": tool_identity("yt-dlp", "--version").await,
        "ffmpeg": tool_identity("ffmpeg", "-version").await,
    });
    println!("VD-UI-TOOLS {tools}");
    let state = ProbeState {
        video: Arc::new(VideoDownloads::new(store.clone(), 3, |_| {})),
        store, output, tools,
        proxy: std::env::var("VD_UI_PROXY").ok().map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        began: Instant::now(),
        calls: Arc::new(Mutex::new(Vec::new())),
        shutdown: Arc::new(Notify::new()),
        legacy_scan: std::env::var("VD_UI_LEGACY_SCAN").as_deref() == Ok("1"),
        matcher_failure: std::env::var("VD_UI_MATCHER_FAILURE").as_deref() == Ok("1"),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/invoke", post(invoke))
        .route("/calls", get(calls))
        .route("/shutdown", post(shutdown))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state.clone());
    let socket = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
    println!("VD-UI-BRIDGE {}", json!({"port": port, "timeoutSeconds": timeout}));
    let shutdown_signal = state.shutdown.clone();
    let server = axum::serve(socket, app).with_graceful_shutdown(async move {
        shutdown_signal.notified().await;
    });
    match tokio::time::timeout(Duration::from_secs(timeout), server).await {
        Ok(result) => result.unwrap(),
        Err(_) => println!("VD-UI-BRIDGE auto-exit after {timeout} seconds"),
    }
    state.video.shutdown().await;
}
