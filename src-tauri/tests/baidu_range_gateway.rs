use versadown_lib::aria2::{Aria2Api, Aria2LaunchConfig, Aria2Process};
use versadown_lib::cloud::baidu_range_gateway::BaiduRangeGateway;
use versadown_lib::cloud::{
    baidu::{BaiduClient, BaiduDownload, BaiduSession},
    CredentialStore, ReqwestTransport, SystemCredentialStore,
};
use versadown_lib::{
    DownloadService, DownloadTask, NewTask, TaskStatus, TaskStore, UriSubmissionOptions,
};
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

const CHUNK: usize = 4 * 1024 * 1024;

// The bundled engine declares a connected peer stalled after eight seconds.
// A healthy 320 KiB/s origin takes over twelve seconds to fill a 4 MiB range.
async fn serve_slow_origin(State(origin): State<Origin>, headers: HeaderMap) -> Response {
    let range = headers[header::RANGE]
        .to_str()
        .unwrap()
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let start = range.0.parse::<usize>().unwrap();
    let end = range
        .1
        .parse::<usize>()
        .unwrap()
        .min(origin.payload.len() - 1);
    assert!(end - start < CHUNK);
    origin.ranges.lock().unwrap().push((start, end));
    let total = origin.payload.len();
    let chunks = futures_util::stream::unfold(
        (origin.payload, start),
        move |(payload, offset)| async move {
            if offset > end {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let next = (offset + 32 * 1024).min(end + 1);
            Some((
                Ok::<_, std::io::Error>(Bytes::copy_from_slice(&payload[offset..next])),
                (payload, next),
            ))
        },
    );
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        )
        .header(header::CONTENT_LENGTH, end - start + 1)
        .body(Body::from_stream(chunks))
        .unwrap()
}

#[tokio::test]
async fn bundled_engine_downloads_slow_continuous_ranges_without_false_stalls() {
    let payload = Arc::new(
        (0..2 * CHUNK + 37)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let origin = Origin {
        payload: Arc::clone(&payload),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    };
    let ranges = Arc::clone(&origin.ranges);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/slow", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/slow", get(serve_slow_origin))
                .with_state(origin),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store.create(NewTask::direct(&upstream)).unwrap();
    let registration = gateway.register(task.id, upstream, vec![], None).unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("slow-range-{}", aria2.rpc_port);
    drop(reserved);
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = versadown_lib::engine_binary::EngineBinary::for_current_target()
        .unwrap()
        .resolve(&manifest, &manifest)
        .unwrap();
    let output = root.path().join("downloads");
    let (mut process, client) = Aria2Process::start(Aria2LaunchConfig {
        binary,
        data_dir: root.path().join("engine"),
        download_dir: output.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    })
    .await
    .unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), store);
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![registration.url().into()],
            UriSubmissionOptions {
                directory: Some(output.clone()),
                output_name: Some("slow.bin".into()),
                max_connections_per_server: Some(1),
                max_tries: Some(1),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let mut observed_progress = false;
    let result = loop {
        let status = client
            .tell_status(submitted.aria_gid.as_deref().unwrap())
            .await
            .unwrap();
        observed_progress |= status.completed_length.parse::<u64>().unwrap_or_default() > 0;
        if status.status == "error" {
            break Err(format!(
                "{:?}: {:?}",
                status.error_code, status.error_message
            ));
        }
        if status.status == "complete" {
            break Ok(());
        }
        if started.elapsed() > Duration::from_secs(45) {
            break Err("slow transfer exceeded deadline".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    process.terminate_fallback().unwrap();
    server.abort();
    assert!(
        result.is_ok(),
        "healthy continuous source failed: {result:?}"
    );
    assert!(observed_progress);
    assert_eq!(std::fs::read(output.join("slow.bin")).unwrap(), *payload);
    assert!(ranges
        .lock()
        .unwrap()
        .iter()
        .all(|(start, end)| end - start < CHUNK));
}

#[derive(Clone)]
struct Origin {
    payload: Arc<Vec<u8>>,
    ranges: Arc<Mutex<Vec<(usize, usize)>>>,
    delay: Duration,
}

#[derive(Clone)]
struct TrackingOrigin {
    origin: Origin,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

async fn serve_tracking_origin(
    State(tracking): State<TrackingOrigin>,
    headers: HeaderMap,
) -> Response {
    if headers.get(header::RANGE).is_none() {
        return serve_origin(State(tracking.origin), headers).await;
    }
    let active = tracking.active.fetch_add(1, Ordering::SeqCst) + 1;
    tracking.peak.fetch_max(active, Ordering::SeqCst);
    let response = serve_origin(State(tracking.origin), headers).await;
    tracking.active.fetch_sub(1, Ordering::SeqCst);
    response
}

async fn serve_origin(State(origin): State<Origin>, headers: HeaderMap) -> Response {
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)));
    let Some((start, end)) = range else {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap();
    };
    if end < start || end - start >= CHUNK || start >= origin.payload.len() {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap();
    }
    let end = end.min(origin.payload.len() - 1);
    origin.ranges.lock().unwrap().push((start, end));
    if !origin.delay.is_zero() {
        tokio::time::sleep(origin.delay).await;
    }
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", origin.payload.len()),
        )
        .header(header::CONTENT_LENGTH, end - start + 1)
        .body(Body::from(Bytes::copy_from_slice(
            &origin.payload[start..=end],
        )))
        .unwrap()
}

async fn serve_expiring_origin(
    State((origin, expired)): State<(Origin, Arc<AtomicBool>)>,
    headers: HeaderMap,
) -> Response {
    if expired.load(Ordering::SeqCst) {
        return Response::builder()
            .status(StatusCode::GONE)
            .body(Body::empty())
            .unwrap();
    }
    serve_origin(State(origin), headers).await
}

async fn origin_ignores_range() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("not a range response"))
        .unwrap()
}

async fn origin_forbids_range() -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Body::empty())
        .unwrap()
}

async fn origin_returns_short_body() -> Response {
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_RANGE, "bytes 0-3/8")
        .body(Body::from("abc"))
        .unwrap()
}

fn chunked_range_with_wrong_length(expected: usize, error_at_eof: bool) -> Response {
    let chunks = futures_util::stream::unfold(0, move |index| async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        match index {
            0 => Some((Ok::<_, std::io::Error>(Bytes::from_static(b"abc")), 1)),
            1 => Some((Ok(Bytes::from_static(b"def")), 2)),
            2 if error_at_eof => Some((Err(std::io::Error::other("truncated origin")), 3)),
            _ => None,
        }
    });
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_RANGE, format!("bytes 0-{}/8", expected - 1))
        .body(Body::from_stream(chunks))
        .unwrap()
}

#[tokio::test]
async fn invalid_chunked_body_never_completes_the_advertised_range() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/truncated",
                    get(|| async { chunked_range_with_wrong_length(8, false) }),
                )
                .route(
                    "/overlong",
                    get(|| async { chunked_range_with_wrong_length(5, false) }),
                )
                .route(
                    "/broken-eof",
                    get(|| async { chunked_range_with_wrong_length(6, true) }),
                ),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for path in ["truncated", "overlong", "broken-eof"] {
        let registration = gateway
            .register(Uuid::new_v4(), format!("{upstream}/{path}"), vec![], None)
            .unwrap();
        let response = client
            .get(registration.url())
            .header(header::RANGE, "bytes=0-7")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{path}");
        assert!(response.bytes().await.is_err(), "{path} must not complete");
    }
    server.abort();
}

async fn origin_returns_valid_short_ranges(
    State(origin): State<Origin>,
    mut headers: HeaderMap,
) -> Response {
    let start = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, _)| start.parse::<usize>().ok())
        .unwrap();
    headers.insert(
        header::RANGE,
        format!("bytes={start}-{}", start + 1024 * 1024 - 1)
            .parse()
            .unwrap(),
    );
    serve_origin(State(origin), headers).await
}

#[tokio::test]
async fn valid_short_upstream_ranges_are_streamed_without_gaps() {
    let payload = Arc::new((0..(2 * CHUNK + 17)).map(|index| (index % 251) as u8).collect());
    let origin = Origin {
        payload: Arc::clone(&payload),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/short", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/short", get(origin_returns_valid_short_ranges))
                .with_state(origin),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let registration = gateway.register(Uuid::new_v4(), upstream, vec![], None).unwrap();
    let response = reqwest::Client::builder().no_proxy().build().unwrap()
        .get(registration.url())
        .header(header::RANGE, format!("bytes=0-{}", payload.len() - 1))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().as_ref(), payload.as_slice());
    server.abort();
}

async fn origin_changes_total(State(origin): State<Origin>, headers: HeaderMap) -> Response {
    let requested = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)));
    let mut response = serve_origin(State(origin.clone()), headers).await;
    if let Some((start, end)) = requested.filter(|(start, _)| *start >= CHUNK) {
        let changed = format!(
            "bytes {start}-{}/{}",
            end.min(origin.payload.len() - 1),
            origin.payload.len() + 1
        );
        response
            .headers_mut()
            .insert(header::CONTENT_RANGE, changed.parse().unwrap());
    }
    response
}

#[tokio::test]
async fn changing_upstream_total_aborts_the_local_response_stream() {
    let origin = Origin {
        payload: Arc::new(
            (0..(2 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(origin_changes_total))
                .with_state(origin),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let registration = gateway
        .register(Uuid::new_v4(), upstream, vec![], None)
        .unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(registration.url())
        .header(header::RANGE, format!("bytes=0-{}", 2 * CHUNK + 36))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(response.bytes().await.is_err());
    server.abort();
}

#[tokio::test]
async fn invalid_upstream_range_responses_are_rejected_before_exposing_file_bytes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/ignored", get(origin_ignores_range))
                .route("/forbidden", get(origin_forbids_range))
                .route("/short", get(origin_returns_short_body)),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (path, expected) in [
        ("ignored", StatusCode::BAD_GATEWAY),
        ("forbidden", StatusCode::FORBIDDEN),
        ("short", StatusCode::BAD_GATEWAY),
    ] {
        let registration = gateway
            .register(Uuid::new_v4(), format!("{upstream}/{path}"), vec![], None)
            .unwrap();
        let response = client
            .get(registration.url())
            .header(header::RANGE, "bytes=0-3")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
        assert!(response.bytes().await.unwrap().is_empty(), "{path}");
    }
    server.abort();
}

#[tokio::test]
async fn no_range_and_head_expose_the_whole_file_without_unbounded_upstream_requests() {
    let origin = Origin {
        payload: Arc::new(
            (0..(2 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let served = origin.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(serve_origin))
                .with_state(served),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let registration = gateway
        .register(Uuid::new_v4(), upstream, vec![], None)
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let head = client.head(registration.url()).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        origin.payload.len().to_string()
    );
    let response = client.get(registration.url()).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        origin.payload.as_slice()
    );
    assert!(origin
        .ranges
        .lock()
        .unwrap()
        .iter()
        .all(|(start, end)| end - start < CHUNK));
    server.abort();
}

#[tokio::test]
async fn large_local_range_is_fetched_in_exact_four_mib_baidu_ranges() {
    let origin = Origin {
        payload: Arc::new(
            (0..(3 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::ZERO,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let served = origin.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(serve_origin))
                .with_state(served),
        )
        .await
        .unwrap();
    });

    let gateway = BaiduRangeGateway::start().await.unwrap();
    let registration = gateway
        .register(
            Uuid::new_v4(),
            upstream,
            vec![("User-Agent".into(), "netdisk-test".into())],
            None,
        )
        .unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(registration.url())
        .header(
            header::RANGE,
            format!("bytes=0-{}", origin.payload.len() - 1),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let bytes = response.bytes().await.unwrap();
    assert_eq!(bytes.as_ref(), origin.payload.as_slice());
    assert_eq!(
        *origin.ranges.lock().unwrap(),
        vec![
            (0, CHUNK - 1),
            (CHUNK, 2 * CHUNK - 1),
            (2 * CHUNK, 3 * CHUNK - 1),
            (3 * CHUNK, 3 * CHUNK + 36)
        ]
    );
    server.abort();
}

#[cfg(windows)]
#[tokio::test]
async fn bundled_engine_completes_large_baidu_file_through_bounded_gateway() {
    let origin = Origin {
        payload: Arc::new(
            (0..(8 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::from_millis(200),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let served = TrackingOrigin {
        origin: origin.clone(),
        active: Arc::new(AtomicUsize::new(0)),
        peak: Arc::new(AtomicUsize::new(0)),
    };
    let peak = Arc::clone(&served.peak);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(serve_tracking_origin))
                .with_state(served),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store.create(NewTask::direct(&upstream)).unwrap();
    let registration = gateway.register(task.id, upstream, vec![], None).unwrap();

    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("baidu-range-gateway-{}", aria2.rpc_port);
    drop(reserved);
    let download_dir = root.path().join("downloads");
    let config = Aria2LaunchConfig {
        binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
        data_dir: root.path().join("engine"),
        download_dir: download_dir.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    };
    let (mut process, client) = Aria2Process::start(config).await.unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), Arc::clone(&store));
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![registration.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("baidu.bin".into()),
                max_connections_per_server: Some(4),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let gid = submitted.aria_gid.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = client.tell_status(&gid).await.unwrap();
        assert_ne!(
            status.status, "error",
            "engine failed: {:?}",
            status.error_code
        );
        if status.status == "complete" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "engine did not finish: {}",
            status.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read(download_dir.join("baidu.bin")).unwrap(),
        *origin.payload
    );
    let ranges = origin.ranges.lock().unwrap();
    assert!(ranges.len() >= 9);
    assert!(ranges.iter().all(|(start, end)| end - start < CHUNK));
    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "one file never fetched multiple 4 MiB ranges concurrently"
    );
    drop(ranges);
    process.terminate_fallback().unwrap();
    server.abort();
}

#[cfg(windows)]
#[tokio::test]
async fn bundled_engine_resumes_after_pausing_with_a_rebound_gateway_token() {
    exercise_paused_gateway_resume(true).await;
}

#[cfg(windows)]
#[tokio::test]
async fn bundled_engine_resumes_after_pausing_when_the_old_gateway_token_is_gone() {
    exercise_paused_gateway_resume(false).await;
}

#[cfg(windows)]
async fn exercise_paused_gateway_resume(rebind_existing_token: bool) {
    let origin = Origin {
        payload: Arc::new(
            (0..(8 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::from_millis(120),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let fresh_upstream = format!("http://{}/fresh", listener.local_addr().unwrap());
    let expired = Arc::new(AtomicBool::new(false));
    let served = origin.clone();
    let expires = Arc::clone(&expired);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(serve_expiring_origin))
                .with_state((served.clone(), expires))
                .merge(
                    Router::new()
                        .route("/fresh", get(serve_origin))
                        .with_state(served),
                ),
        )
        .await
        .unwrap();
    });
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap());
    let task = store
        .create(NewTask {
            source: "cloud://baidu/paused-gateway".into(),
            name: "rotated.bin".into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap();
    let mut original = gateway
        .register(task.id, upstream.clone(), vec![], None)
        .unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("baidu-rotate-gateway-{}", aria2.rpc_port);
    drop(reserved);
    let download_dir = root.path().join("downloads");
    let config = Aria2LaunchConfig {
        binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
        data_dir: root.path().to_path_buf(),
        download_dir: download_dir.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    };
    let (mut process, client) = Aria2Process::start(config).await.unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), Arc::clone(&store));
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![original.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("rotated.bin".into()),
                max_connections_per_server: Some(4),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    original.commit();
    let gid = submitted.aria_gid.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = client.tell_status(&gid).await.unwrap();
        assert_ne!(
            status.status, "error",
            "engine failed: {:?}",
            status.error_code
        );
        if status.completed_length.parse::<usize>().unwrap() >= CHUNK {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "engine never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client.pause(&gid).await.unwrap();
    let paused = service.reconcile_task(task.id).await.unwrap();
    assert_eq!(paused.status, TaskStatus::Paused);
    let checkpoint =
        rusqlite::Connection::open(root.path().join("aria2-next-state/stream/state.db")).unwrap();
    let (persisted_bytes, completed_ranges): (u64, String) = checkpoint
        .query_row(
            "SELECT completed_length, completed_ranges FROM downloads WHERE gid=?1",
            [&gid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(persisted_bytes >= CHUNK as u64);
    let paused_payload = std::fs::read(download_dir.join("rotated.bin")).unwrap();
    for range in completed_ranges.split(',') {
        let (start, end) = range.split_once('-').unwrap();
        let start = start.parse::<usize>().unwrap();
        let end = end.parse::<usize>().unwrap();
        let mismatch = paused_payload[start..end]
            .iter()
            .zip(origin.payload[start..end].iter())
            .position(|(actual, expected)| actual != expected);
        assert_eq!(
            mismatch, None,
            "checkpoint included corrupt bytes at {start}"
        );
    }
    let paused = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Queued,
            failure_code: Some("cloud_url_refreshing".into()),
            ..paused
        })
        .unwrap();
    expired.store(true, Ordering::SeqCst);
    let mut replacement = if rebind_existing_token {
        gateway.register_refreshing(task.id, fresh_upstream, vec![], None)
    } else {
        gateway.register(task.id, fresh_upstream, vec![], None)
    }
    .unwrap();
    let new_url = replacement.url().to_owned();
    assert_eq!(new_url == original.url(), rebind_existing_token);
    let resumed = service
        .replace_cloud_uri_with_options(
            paused.id,
            &gid,
            vec![new_url],
            UriSubmissionOptions {
                continue_download: true,
                max_connections_per_server: Some(4),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(resumed.aria_gid.as_deref(), Some(gid.as_str()));
    gateway.retire(task.id, replacement.token());
    replacement.commit();
    loop {
        let status = client.tell_status(&gid).await.unwrap();
        assert_ne!(
            status.status, "error",
            "engine failed: {:?}",
            status.error_code
        );
        if status.status == "complete" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "rotated engine did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let actual = std::fs::read(download_dir.join("rotated.bin")).unwrap();
    assert_eq!(actual.len(), origin.payload.len());
    let first_mismatch = actual
        .iter()
        .zip(origin.payload.iter())
        .position(|(actual, expected)| actual != expected);
    assert_eq!(first_mismatch, None, "resumed payload was corrupted");
    process.terminate_fallback().unwrap();
    server.abort();
}

#[cfg(windows)]
#[tokio::test]
async fn bundled_engine_restores_a_native_checkpoint_through_fresh_gateway_with_dead_old_mirror() {
    let origin = Origin {
        payload: Arc::new(
            (0..(8 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::from_millis(150),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let served = origin.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(serve_origin))
                .with_state(served),
        )
        .await
        .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap());
    let task = store.create(NewTask::direct(&upstream)).unwrap();
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let mut original = gateway
        .register(task.id, upstream.clone(), vec![], None)
        .unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("baidu-checkpoint-gateway-{}", aria2.rpc_port);
    drop(reserved);
    let download_dir = root.path().join("downloads");
    let config = Aria2LaunchConfig {
        binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
        data_dir: root.path().to_path_buf(),
        download_dir: download_dir.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    };
    let (mut process, client) = Aria2Process::start(config.clone()).await.unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), Arc::clone(&store));
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![original.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("checkpoint.bin".into()),
                max_connections_per_server: Some(1),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    original.commit();
    let gid = submitted.aria_gid.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = client.tell_status(&gid).await.unwrap();
        assert_ne!(status.status, "error");
        if status.completed_length.parse::<usize>().unwrap() >= CHUNK {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "engine never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client.pause(&gid).await.unwrap();
    let paused = client.tell_status(&gid).await.unwrap();
    assert_eq!(paused.status, "paused");
    let completed = paused.completed_length.parse::<u64>().unwrap();
    let total = origin.payload.len() as u64;
    assert!(completed > 0 && completed < total);
    client.save_session().await.unwrap();
    client.shutdown().await.unwrap();
    assert!(process.wait_for_exit(Duration::from_secs(5)).await.unwrap());
    drop(process);
    drop(gateway);

    let path = download_dir.join("checkpoint.bin");
    let snapshot = store.find(task.id).unwrap();
    store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Failed,
            total_length: Some(total),
            completed_length: completed,
            ..snapshot
        })
        .unwrap();
    let existing_files = store.files(task.id).unwrap();
    assert_eq!(existing_files.len(), 1);
    store
        .upsert_files(
            task.id,
            &[(
                existing_files[0].index,
                existing_files[0].path.clone(),
                total,
                completed,
                true,
            )],
        )
        .unwrap();

    let gateway = BaiduRangeGateway::start().await.unwrap();
    let mut replacement = gateway.register(task.id, upstream, vec![], None).unwrap();
    let (mut process, client) = Aria2Process::start(config).await.unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), Arc::clone(&store));
    let resumed = service
        .resubmit_uri_with_options(
            task.id,
            vec![replacement.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("checkpoint.bin".into()),
                continue_download: true,
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    replacement.commit();
    let resumed_gid = resumed.aria_gid.unwrap();
    loop {
        let status = client.tell_status(&resumed_gid).await.unwrap();
        assert_ne!(
            status.status, "error",
            "resume failed: {:?} {:?}",
            status.error_code, status.error_message
        );
        if status.status == "complete" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "resumed download timed out"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(path).unwrap(), *origin.payload);
    process.terminate_fallback().unwrap();
    server.abort();
}

#[cfg(windows)]
#[tokio::test]
async fn failed_old_gateway_gid_refreshes_without_losing_the_partial_file() {
    let origin = Origin {
        payload: Arc::new(
            (0..(8 * CHUNK + 37))
                .map(|index| (index % 251) as u8)
                .collect(),
        ),
        ranges: Arc::new(Mutex::new(Vec::new())),
        delay: Duration::from_millis(120),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/file", listener.local_addr().unwrap());
    let served = origin.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/file", get(serve_origin))
                .with_state(served),
        )
        .await
        .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap());
    let task = store
        .create(NewTask {
            source: "cloud://baidu/file-id".into(),
            name: "refresh.bin".into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap();
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let mut original = gateway
        .register(task.id, upstream.clone(), vec![], None)
        .unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("baidu-terminal-refresh-{}", aria2.rpc_port);
    drop(reserved);
    let download_dir = root.path().join("downloads");
    let config = Aria2LaunchConfig {
        binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
        data_dir: root.path().to_path_buf(),
        download_dir: download_dir.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    };
    let (mut process, client) = Aria2Process::start(config).await.unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), Arc::clone(&store));
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![original.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("refresh.bin".into()),
                max_connections_per_server: Some(1),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    original.commit();
    let old_gid = submitted.aria_gid.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = client.tell_status(&old_gid).await.unwrap();
        assert_ne!(status.status, "error", "initial transfer failed");
        if status.completed_length.parse::<usize>().unwrap() >= CHUNK {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "initial transfer timed out"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client.pause(&old_gid).await.unwrap();
    let paused = service.reconcile_task(task.id).await.unwrap();
    assert!(paused.completed_length > 0);
    gateway.remove_token(original.token());
    client.unpause(&old_gid).await.unwrap();
    loop {
        let status = client.tell_status(&old_gid).await.unwrap();
        if status.status == "error" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "old gateway did not fail"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let queued = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Queued,
            failure_code: Some("cloud_url_refreshing".into()),
            ..paused
        })
        .unwrap();
    let mut replacement = gateway.register(task.id, upstream, vec![], None).unwrap();
    let refreshed = service
        .replace_cloud_uri_with_options(
            queued.id,
            &old_gid,
            vec![replacement.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("refresh.bin".into()),
                continue_download: true,
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    replacement.commit();
    let new_gid = refreshed.aria_gid.unwrap();
    loop {
        let status = client.tell_status(&new_gid).await.unwrap();
        assert_ne!(
            status.status, "error",
            "refresh failed: {:?} {:?}",
            status.error_code, status.error_message
        );
        if status.status == "complete" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "refreshed transfer timed out"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        std::fs::read(download_dir.join("refresh.bin")).unwrap(),
        *origin.payload
    );
    process.terminate_fallback().unwrap();
    server.abort();
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "read-only Baidu network probe; requires VERSADOWN_LIVE_CLOUD=1 and BAIDU_LIVE_FILE_NAME"]
async fn live_baidu_gateway_reads_three_consecutive_four_mib_chunks() {
    assert_eq!(
        std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(),
        Ok("1")
    );
    let filename = std::env::var("BAIDU_LIVE_FILE_NAME").expect("file name required");
    let (signed, size) = live_baidu_download_metadata(filename).await;
    assert!(size >= 3 * CHUNK as u64);
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let registration = gateway
        .register(
            Uuid::new_v4(),
            signed.url,
            signed.headers.into_iter().collect(),
            None,
        )
        .unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
        .get(registration.url())
        .header(header::RANGE, format!("bytes=0-{}", 3 * CHUNK - 1))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().len(), 3 * CHUNK);
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "read-only four-way Baidu CDN probe; requires VERSADOWN_LIVE_CLOUD=1 and BAIDU_LIVE_FILE_NAME"]
async fn live_baidu_cdn_accepts_four_simultaneous_four_mib_ranges() {
    assert_eq!(
        std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(),
        Ok("1")
    );
    let filename = std::env::var("BAIDU_LIVE_FILE_NAME").expect("file name required");
    let (signed, total) = live_baidu_download_metadata(filename).await;
    assert!(total >= 4 * CHUNK as u64);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let started = tokio::time::Instant::now();
    let ranges = tokio::join!(
        fetch_live_baidu_range(&client, &signed, 0, total),
        fetch_live_baidu_range(&client, &signed, CHUNK, total),
        fetch_live_baidu_range(&client, &signed, 2 * CHUNK, total),
        fetch_live_baidu_range(&client, &signed, 3 * CHUNK, total),
    );
    for result in [ranges.0, ranges.1, ranges.2, ranges.3] {
        assert_eq!(result, (StatusCode::PARTIAL_CONTENT, CHUNK));
    }
    let parallel_seconds = started.elapsed().as_secs_f64();
    let sequential_started = tokio::time::Instant::now();
    for start in [0, CHUNK, 2 * CHUNK, 3 * CHUNK] {
        assert_eq!(
            fetch_live_baidu_range(&client, &signed, start, total).await,
            (StatusCode::PARTIAL_CONTENT, CHUNK)
        );
    }
    eprintln!(
        "Baidu four-range probe: {} bytes parallel in {:.2}s, sequential in {:.2}s",
        4 * CHUNK,
        parallel_seconds,
        sequential_started.elapsed().as_secs_f64()
    );
}

#[cfg(windows)]
async fn fetch_live_baidu_range(
    client: &reqwest::Client,
    signed: &BaiduDownload,
    start: usize,
    total: u64,
) -> (StatusCode, usize) {
    let end = start + CHUNK - 1;
    let mut request = client
        .get(&signed.url)
        .header(header::RANGE, format!("bytes={start}-{end}"));
    for (name, value) in &signed.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|_| panic!("Baidu range request failed"));
    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
            Some(format!("bytes {start}-{end}/{total}").as_str())
        );
    }
    let length = response
        .bytes()
        .await
        .unwrap_or_else(|_| panic!("Baidu range body failed"))
        .len();
    (status, length)
}

#[cfg(windows)]
async fn live_baidu_download_metadata(filename: String) -> (BaiduDownload, u64) {
    tokio::task::spawn_blocking(move || {
        let store = SystemCredentialStore::new(
            PathBuf::from(std::env::var_os("APPDATA").unwrap())
                .join("com.versadown.desktop/cloud-sessions"),
        )
        .unwrap();
        let session: BaiduSession =
            serde_json::from_slice(&store.load("baidu").unwrap().unwrap()).unwrap();
        let transport = ReqwestTransport::new().unwrap();
        let client = BaiduClient::new(session, &transport);
        let root = client.directory("/").unwrap();
        let file = root
            .items
            .iter()
            .find(|item| item.kind == "file" && item.name == filename)
            .expect("named file missing from root");
        (client.download(&file.id).unwrap(), file.size.unwrap())
    })
    .await
    .unwrap()
}

#[cfg(windows)]
async fn live_baidu_medium_download_metadata() -> (BaiduDownload, u64, String) {
    tokio::task::spawn_blocking(|| {
        let store = SystemCredentialStore::new(
            PathBuf::from(std::env::var_os("APPDATA").unwrap())
                .join("com.versadown.desktop/cloud-sessions"),
        )
        .unwrap();
        let session: BaiduSession =
            serde_json::from_slice(&store.load("baidu").unwrap().unwrap()).unwrap();
        let transport = ReqwestTransport::new().unwrap();
        let client = BaiduClient::new(session, &transport);
        let mut directories = VecDeque::from(["/".to_owned()]);
        for index in 0..100 {
            let Some(directory) = directories.pop_front() else {
                break;
            };
            let listing = match client.directory(&directory) {
                Ok(listing) => listing,
                Err(error) if index == 0 => panic!("Baidu root listing failed: {error:?}"),
                Err(_) => continue,
            };
            if let Some(file) = listing.items.iter().find(|item| {
                item.kind == "file"
                    && item
                        .size
                        .is_some_and(|size| (20 * 1024 * 1024..=100 * 1024 * 1024).contains(&size))
            }) {
                return (
                    client.download(&file.id).unwrap(),
                    file.size.unwrap(),
                    file.name.clone(),
                );
            }
            directories.extend(
                listing
                    .items
                    .iter()
                    .filter(|item| {
                        item.kind == "folder"
                            && item.id != versadown_lib::cloud::baidu::BAIDU_RECYCLE_BIN_ID
                    })
                    .map(|item| item.id.clone()),
            );
        }
        panic!("no 20–100 MiB Baidu file found in the first 100 directories");
    })
    .await
    .unwrap()
}

#[cfg(windows)]
async fn independent_baidu_sha256(signed: &BaiduDownload, total: u64) -> Vec<u8> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let mut digest = Sha256::new();
    for start in (0..total).step_by(CHUNK) {
        let end = (start + CHUNK as u64 - 1).min(total - 1);
        let mut request = client
            .get(&signed.url)
            .header(header::RANGE, format!("bytes={start}-{end}"));
        for (name, value) in &signed.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|_| panic!("independent Baidu range request failed"));
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok()),
            Some(format!("bytes {start}-{end}/{total}").as_str())
        );
        let bytes = response
            .bytes()
            .await
            .unwrap_or_else(|_| panic!("independent Baidu range body failed"));
        assert_eq!(bytes.len() as u64, end - start + 1);
        digest.update(bytes);
    }
    digest.finalize().to_vec()
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "downloads a full 20-100 MiB Baidu file to a temporary directory and verifies SHA-256; requires VERSADOWN_LIVE_CLOUD=1 and BAIDU_LIVE_FULL_DOWNLOAD=1"]
async fn live_baidu_file_completes_through_bundled_engine_and_gateway() {
    assert_eq!(
        std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(),
        Ok("1")
    );
    assert_eq!(
        std::env::var("BAIDU_LIVE_FULL_DOWNLOAD").as_deref(),
        Ok("1")
    );
    let (signed, expected_size, filename) =
        if let Ok(filename) = std::env::var("BAIDU_LIVE_FILE_NAME") {
            let (signed, size) = live_baidu_download_metadata(filename.clone()).await;
            (signed, size, filename)
        } else {
            live_baidu_medium_download_metadata().await
        };
    let reference = signed.clone();
    let gateway = BaiduRangeGateway::start().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct(
            "https://example.test/verified-personal-file",
        ))
        .unwrap();
    let registration = gateway
        .register(
            task.id,
            signed.url,
            signed.headers.into_iter().collect(),
            None,
        )
        .unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("baidu-live-gateway-{}", aria2.rpc_port);
    drop(reserved);
    let download_dir = root.path().join("downloads");
    let config = Aria2LaunchConfig {
        binary: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-pc-windows-msvc.exe"),
        data_dir: root.path().join("engine"),
        download_dir: download_dir.clone(),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    };
    let (mut process, client) = Aria2Process::start(config).await.unwrap();
    let service = DownloadService::new(Arc::new(client.clone()), Arc::clone(&store));
    let submitted = service
        .submit_uri_with_options(
            task.id,
            vec![registration.url().into()],
            UriSubmissionOptions {
                directory: Some(download_dir.clone()),
                output_name: Some("baidu-live.bin".into()),
                max_connections_per_server: Some(4),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let gid = submitted.aria_gid.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
    let mut last_reported = 0;
    loop {
        let status = client.tell_status(&gid).await.unwrap();
        assert_ne!(
            status.status, "error",
            "engine error code: {:?}",
            status.error_code
        );
        let completed = status.completed_length.parse::<u64>().unwrap();
        if completed.saturating_sub(last_reported) >= 64 * 1024 * 1024 {
            eprintln!("Baidu live bytes: {completed}/{expected_size}");
            last_reported = completed;
        }
        if status.status == "complete" {
            assert_eq!(completed, expected_size);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Baidu live download timed out at {completed}/{expected_size}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let file = download_dir.join("baidu-live.bin");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), expected_size);
    let downloaded_sha256 = Sha256::digest(std::fs::read(&file).unwrap()).to_vec();
    let reference_sha256 = independent_baidu_sha256(&reference, expected_size).await;
    assert_eq!(downloaded_sha256, reference_sha256);
    eprintln!("Baidu live download complete: {filename}, {expected_size} bytes, SHA-256 matched");
    process.terminate_fallback().unwrap();
}
