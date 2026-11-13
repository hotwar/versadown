//! Opt-in browser acceptance bridge. Only window-shell state is replaced by
//! the browser harness; all video routing and previews use production code.
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
    proxy: Option<String>,
    began: Instant,
    calls: Arc<Mutex<Vec<Value>>>,
    shutdown: Arc<Notify>,
    legacy_scan: bool,
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
            let entries = routing::scan_with_progress(input()?, |entries, pending| {
                if !state.legacy_scan {
                    let _ = sender.send(json!({"type": "progress", "value": {"entries": entries, "pending": pending}}));
                }
            }).await;
            serde_json::to_value(entries).map_err(|e| e.to_string())
        }
        "parse_download" => {
            let probe = request.args.get("probeVideo").and_then(Value::as_bool).unwrap_or(false);
            let page = routing::resolve(input()?.trim(), probe).await?
                .ok_or_else(|| "video.error.unsupported".to_owned())?;
            let options = VideoOptions { proxy: state.proxy.clone(), ..Default::default() };
            let preview = state.video.prepare(page, &options).await?;
            let mut value = serde_json::to_value(preview).map_err(|e| e.to_string())?;
            value.as_object_mut().expect("VideoPreview is an object")
                .insert("kind".into(), json!("web_video"));
            Ok(value)
        }
        "discard_video_preview" => {
            let id = request.args.get("previewId").and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| "invalid previewId".to_owned())?;
            state.video.previews.discard(id)?;
            Ok(Value::Null)
        }
        "get_settings" => serde_json::to_value(crate::AppSettings::default()).map_err(|e| e.to_string()),
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

#[tokio::test]
#[ignore = "long-running real video UI acceptance bridge; requires installed yt-dlp and explicit VD_UI_PORT"]
async fn serve_real_video_ui_probe() {
    extractor::set_resource_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    let port: u16 = std::env::var("VD_UI_PORT").expect("set VD_UI_PORT")
        .parse().expect("VD_UI_PORT must be a port number");
    assert!(port > 0, "VD_UI_PORT must select a concrete loopback port");
    let timeout = std::env::var("VD_UI_TIMEOUT_SECS").ok()
        .and_then(|value| value.parse::<u64>().ok()).unwrap_or(900).clamp(1, 900);
    let state = ProbeState {
        video: Arc::new(VideoDownloads::new(Arc::new(TaskStore::open_in_memory().unwrap()), 3, |_| {})),
        proxy: std::env::var("VD_UI_PROXY").ok().map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        began: Instant::now(),
        calls: Arc::new(Mutex::new(Vec::new())),
        shutdown: Arc::new(Notify::new()),
        legacy_scan: std::env::var("VD_UI_LEGACY_SCAN").as_deref() == Ok("1"),
    };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({
            "ready": true, "mode": "production-video-parser",
            "manifestDirectory": env!("CARGO_MANIFEST_DIR"),
            "declaredSourceSha": std::env::var("VD_UI_SOURCE_SHA").ok(),
            "cookiesLoaded": false,
            "legacyScan": std::env::var("VD_UI_LEGACY_SCAN").as_deref() == Ok("1"),
        })) }))
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
