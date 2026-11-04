#![cfg(any(windows, target_os = "macos", target_os = "linux"))]

use versadown_lib::aria2::{
    Aria2Api, Aria2LaunchConfig, Aria2Process, Aria2RpcClient, Aria2Status,
};
use versadown_lib::engine_binary::EngineBinary;
use versadown_lib::{DownloadService, NewTask, TaskStatus, TaskStore, UriSubmissionOptions};
use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::{sleep, timeout_at, Instant};

const PAYLOAD_BYTES: usize = 1024 * 1024;
const HOLD_AFTER_BYTES: usize = 256 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(50);

#[tokio::test]
async fn transfer_gate_blocks_until_the_test_releases_it() {
    let gate = TransferGate::new();
    assert!(
        timeout_at(Instant::now() + Duration::from_millis(50), gate.wait())
            .await
            .is_err(),
        "a held transfer must not rely on timing to remain incomplete"
    );

    gate.release();
    timeout_at(Instant::now() + Duration::from_millis(250), gate.wait())
        .await
        .expect("released transfer gate did not unblock");
}

#[derive(Clone)]
struct TransferGate {
    released: Arc<AtomicBool>,
    notified: Arc<tokio::sync::Notify>,
}

impl TransferGate {
    fn new() -> Self {
        Self {
            released: Arc::new(AtomicBool::new(false)),
            notified: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.notified.notify_waiters();
    }

    async fn wait(&self) {
        while !self.released.load(Ordering::SeqCst) {
            self.notified.notified().await;
        }
    }
}

struct HeldDownload {
    gid: String,
    gate: TransferGate,
}

impl HeldDownload {
    fn gid(&self) -> &str {
        &self.gid
    }

    fn release(&self) {
        self.gate.release();
    }
}

#[derive(Clone)]
struct HttpFixture {
    payload: Arc<Vec<u8>>,
    total_requests: Arc<AtomicUsize>,
    resumed_requests: Arc<AtomicUsize>,
    gates: Arc<Mutex<BTreeMap<String, TransferGate>>>,
}

impl HttpFixture {
    fn register_gate(&self, name: &str) -> TransferGate {
        let gate = TransferGate::new();
        assert!(
            self.gates
                .lock()
                .unwrap()
                .insert(name.to_string(), gate.clone())
                .is_none(),
            "test gate {name} was registered twice"
        );
        gate
    }

    fn gate_for_uri(&self, uri: &OriginalUri) -> Option<TransferGate> {
        let name = uri
            .query()
            .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("hold=")))?;
        self.gates.lock().unwrap().get(name).cloned()
    }
}

// The only origin is loopback. Held responses make pause/resume deterministic without
// throttling every test transfer by wall-clock sleeps.
async fn serve_payload(
    State(state): State<HttpFixture>,
    headers: HeaderMap,
    uri: OriginalUri,
) -> Response {
    state.total_requests.fetch_add(1, Ordering::SeqCst);
    let length = state.payload.len();
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (start, end, partial) = match range {
        Some(range) => {
            let parsed = range
                .strip_prefix("bytes=")
                .and_then(|v| v.split_once('-'))
                .and_then(|(start, end)| {
                    Some((
                        start.parse::<usize>().ok()?,
                        if end.is_empty() {
                            length - 1
                        } else {
                            end.parse::<usize>().ok()?.min(length - 1)
                        },
                    ))
                });
            match parsed {
                Some((start, end)) if start <= end && start < length => (start, end, true),
                _ => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                        .body(Body::empty())
                        .unwrap()
                }
            }
        }
        None => (0, length - 1, false),
    };
    if start > 0 {
        state.resumed_requests.fetch_add(1, Ordering::SeqCst);
    }
    let gate = state.gate_for_uri(&uri);
    let stream = futures_util::stream::unfold(
        (state.payload, start, false, gate),
        move |(payload, offset, gate_released, gate)| async move {
            if offset > end {
                return None;
            }
            let gate_released = if !gate_released && offset >= HOLD_AFTER_BYTES {
                if let Some(gate) = &gate {
                    gate.wait().await;
                }
                true
            } else {
                gate_released
            };
            let next = (offset + STREAM_CHUNK_BYTES).min(end + 1);
            let bytes = Bytes::copy_from_slice(&payload[offset..next]);
            Some((
                Ok::<_, Infallible>(bytes),
                (payload, next, gate_released, gate),
            ))
        },
    );
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::CONNECTION, "close");
    if partial {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    response.body(Body::from_stream(stream)).unwrap()
}

fn serve_static_media(headers: HeaderMap, payload: &[u8], content_type: &str) -> Response {
    let length = payload.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let (start, end, partial) = match range {
        Some(value) => match value
            .strip_prefix("bytes=")
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, end)| {
                Some((
                    start.parse::<usize>().ok()?,
                    if end.is_empty() {
                        length - 1
                    } else {
                        end.parse::<usize>().ok()?.min(length - 1)
                    },
                ))
            }) {
            Some((start, end)) if start <= end && start < length => (start, end, true),
            _ => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                    .body(Body::empty())
                    .unwrap()
            }
        },
        None => (0, length - 1, false),
    };
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, end - start + 1);
    if partial {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    response
        .body(Body::from(Bytes::copy_from_slice(&payload[start..=end])))
        .unwrap()
}

async fn serve_dash_init(headers: HeaderMap) -> Response {
    serve_static_media(
        headers,
        include_bytes!("fixtures/silence-init.mp4"),
        "audio/mp4",
    )
}

async fn serve_dash_segment(headers: HeaderMap) -> Response {
    serve_static_media(
        headers,
        include_bytes!("fixtures/silence-segment.m4s"),
        "audio/iso.segment",
    )
}

struct LiveAria2Fixture {
    process: Option<Aria2Process>,
    client: Aria2RpcClient,
    config: Aria2LaunchConfig,
    server: tokio::task::JoinHandle<()>,
    origin: String,
    http: HttpFixture,
    _root: tempfile::TempDir,
}

impl LiveAria2Fixture {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let http = HttpFixture {
            payload: Arc::new((0..PAYLOAD_BYTES).map(|i| (i % 251) as u8).collect()),
            total_requests: Arc::new(AtomicUsize::new(0)),
            resumed_requests: Arc::new(AtomicUsize::new(0)),
            gates: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/payload.bin", get(serve_payload))
            .route(
                "/playlist.m3u8",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXTINF:1.02,\nsilence.ts\n#EXT-X-ENDLIST\n",
                    )
                }),
            )
            .route(
                "/silence.ts",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "video/mp2t")],
                        include_bytes!("fixtures/silence.ts").as_slice(),
                    )
                }),
            )
            .route("/silence-init.mp4", get(serve_dash_init))
            .route("/silence-segment.m4s", get(serve_dash_segment))
            .route(
                "/manifest.mpd",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/dash+xml")],
                        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT1S" minBufferTime="PT1S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period duration="PT1S">
    <AdaptationSet mimeType="audio/mp4" segmentAlignment="true">
      <Representation id="1" bandwidth="64000" codecs="mp4a.40.2" audioSamplingRate="44100">
        <AudioChannelConfiguration schemeIdUri="urn:mpeg:dash:23003:3:audio_channel_configuration:2011" value="2" />
        <SegmentList timescale="44100" duration="45056">
          <Initialization sourceURL="silence-init.mp4" />
          <SegmentURL media="silence-segment.m4s" />
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#,
                    )
                }),
            )
            .with_state(http.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut aria2 = versadown_lib::Aria2Settings::default();
        aria2.rpc_port = listener.local_addr().unwrap().port();
        aria2.rpc_token = format!("aria2-live-test-{}", aria2.rpc_port);
        drop(listener);
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let binary = EngineBinary::for_current_target()
            .unwrap()
            .resolve(&manifest_dir, &manifest_dir)
            .unwrap();
        let config = Aria2LaunchConfig {
            binary,
            data_dir: root.path().join("data"),
            download_dir: root.path().join("downloads"),
            parent_pid: std::process::id(),
            peer_discovery: false,
            aria2,
        };
        let (process, client) = Aria2Process::start(config.clone()).await.unwrap();
        assert_eq!(client.get_version().await.unwrap().version, "3.0.9");
        Self {
            process: Some(process),
            client,
            config,
            server,
            origin,
            http,
            _root: root,
        }
    }

    async fn add_download(&self, name: &str, uri: String) -> String {
        let options = BTreeMap::from([
            ("out".into(), name.into()),
            ("split".into(), "1".into()),
            ("max-connection-per-server".into(), "1".into()),
            ("min-split-size".into(), "1M".into()),
            ("file-allocation".into(), "none".into()),
            ("all-proxy".into(), "".into()),
            // Mirrors `DownloadService::submit_uri`: the engine starts with `--pause=true`.
            ("pause".into(), "false".into()),
        ]);
        self.client.add_uri(vec![uri], options).await.unwrap()
    }

    async fn add_download_without_hold(&self, name: &str) -> String {
        self.add_download(name, format!("{}/payload.bin", self.origin))
            .await
    }

    async fn add_slow_download(&self, name: &str) -> HeldDownload {
        let gate = self.http.register_gate(name);
        let gid = self
            .add_download(name, format!("{}/payload.bin?hold={name}", self.origin))
            .await;
        HeldDownload { gid, gate }
    }

    async fn wait_status(
        &self,
        gid: &str,
        duration: Duration,
        predicate: impl Fn(&Aria2Status) -> bool,
    ) -> Aria2Status {
        let deadline = Instant::now() + duration;
        loop {
            let status = timeout_at(deadline, self.client.tell_status(gid))
                .await
                .expect("tellStatus exceeded live-test deadline")
                .unwrap();
            assert_ne!(
                status.status, "error",
                "aria2 download failed: {:?}",
                status.error_code
            );
            if predicate(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "live-test deadline: status={}, completedLength={}",
                status.status,
                status.completed_length
            );
            tokio::time::sleep_until((Instant::now() + POLL).min(deadline)).await;
        }
    }

    async fn wait_progress(&self, gid: &str) -> Aria2Status {
        self.wait_status(gid, Duration::from_secs(5), |status| {
            status.status == "active"
                && status.completed_length.parse::<usize>().unwrap() >= HOLD_AFTER_BYTES
        })
        .await
    }

    async fn pause(&self, gid: &str) -> Aria2Status {
        assert_eq!(self.client.pause(gid).await.unwrap(), gid);
        self.wait_status(gid, Duration::from_secs(3), |status| {
            status.status == "paused"
        })
        .await
    }

    async fn wait_complete(&self, gid: &str) {
        let complete = self
            .wait_status(gid, Duration::from_secs(20), |status| {
                status.status == "complete"
            })
            .await;
        assert_eq!(
            complete.completed_length.parse::<usize>().unwrap(),
            PAYLOAD_BYTES
        );
        assert_eq!(
            complete.total_length.parse::<usize>().unwrap(),
            PAYLOAD_BYTES
        );
    }

    fn assert_download_sha256(&self, name: &str) {
        let downloaded = std::fs::read(self.config.download_dir.join(name)).unwrap();
        assert_eq!(downloaded.len(), PAYLOAD_BYTES);
        assert_eq!(
            Sha256::digest(&downloaded),
            Sha256::digest(self.http.payload.as_slice())
        );
    }

    async fn save_and_shutdown(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            timeout_at(deadline, self.client.save_session())
                .await
                .unwrap()
                .unwrap(),
            "OK"
        );
        let session =
            std::fs::read_to_string(self.config.data_dir.join("aria2-next.tasks")).unwrap();
        assert!(!session.contains("rpc-secret") && !session.contains("token:"));
        assert_eq!(
            timeout_at(deadline, self.client.shutdown())
                .await
                .unwrap()
                .unwrap(),
            "OK"
        );
        let process = self.process.as_mut().unwrap();
        assert!(
            process
                .wait_for_exit(deadline.saturating_duration_since(Instant::now()))
                .await
                .unwrap(),
            "owned aria2 child did not exit after saveSession/shutdown"
        );
        assert!(!process.is_running().unwrap());
        session
    }

    async fn restart(&mut self) {
        assert!(!self.process.as_mut().unwrap().is_running().unwrap());
        let (process, client) = Aria2Process::start(self.config.clone()).await.unwrap();
        self.process = Some(process);
        self.client = client;
    }

    fn terminate_for_cleanup(&mut self) {
        let process = self.process.as_mut().unwrap();
        process.terminate_fallback().unwrap();
        assert!(
            !process.is_running().unwrap(),
            "owned aria2 child remained alive after test cleanup"
        );
    }
}

#[tokio::test]
async fn bundled_aria2_next_accepts_ed2k_and_downloads_hls_and_dash_manifests() {
    let mut fixture = LiveAria2Fixture::start().await;

    let ed2k = "ed2k://|file|fixture.bin|1048576|0123456789ABCDEF0123456789ABCDEF|/";
    let ed2k_gid = fixture
        .client
        .add_uri(vec![ed2k.into()], BTreeMap::new())
        .await
        .expect("Aria2 Next accepts an ED2K link through JSON-RPC");
    let ed2k_status = fixture.client.tell_status(&ed2k_gid).await.unwrap();
    assert!(matches!(
        ed2k_status.status.as_str(),
        "waiting" | "active" | "paused"
    ));
    let _ = fixture.client.force_remove(&ed2k_gid).await;

    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let service = Arc::new(DownloadService::new(
        Arc::new(fixture.client.clone()),
        Arc::clone(&store),
    ));
    for (kind, name, manifest) in [
        ("hls", "fixture-hls.mp4", "playlist.m3u8"),
        ("dash", "fixture-dash.mp4", "manifest.mpd"),
    ] {
        let source = format!("{}/{}", fixture.origin, manifest);
        let task = store
            .create(NewTask {
                source: source.clone(),
                name: name.into(),
                kind: kind.into(),
                is_private: false,
            })
            .unwrap();
        service
            .submit_native_media_with_options(
                task.id,
                vec![source],
                UriSubmissionOptions {
                    directory: Some(fixture.config.download_dir.clone()),
                    output_name: Some(name.into()),
                    ..Default::default()
                },
                kind,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                service.reconcile_task(task.id).await.unwrap();
                let task = store.find(task.id).unwrap();
                assert_ne!(
                    task.status,
                    TaskStatus::Failed,
                    "{manifest}: {:?}",
                    task.error
                );
                if task.status == TaskStatus::Completed {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("media task did not complete");
        let output = fixture.config.download_dir.join(name);
        assert!(output.is_file(), "{manifest} did not produce {name}");
        assert!(
            std::fs::metadata(output).unwrap().len() > 0,
            "{manifest} produced an empty media container"
        );
    }

    fixture.save_and_shutdown().await;
}

impl Drop for LiveAria2Fixture {
    fn drop(&mut self) {
        // Failure cleanup touches only this fixture's owned child and temporary data.
        if let Some(process) = &mut self.process {
            let _ = process.terminate_fallback();
        }
        self.server.abort();
    }
}

#[tokio::test]
async fn bundled_aria2_next_changes_active_http_range_parallelism() {
    let mut fixture = LiveAria2Fixture::start().await;
    let download = fixture.add_slow_download("parallelism.bin").await;
    fixture.wait_progress(download.gid()).await;

    fixture
        .client
        .change_option(
            download.gid(),
            BTreeMap::from([
                ("split".into(), "2".into()),
                ("max-connection-per-server".into(), "2".into()),
            ]),
        )
        .await
        .expect("aria2-next must accept changing HTTP range parallelism");
    let options = fixture.client.get_option(download.gid()).await.unwrap();
    assert_eq!(options.get("split").map(String::as_str), Some("2"));
    assert_eq!(
        options.get("max-connection-per-server").map(String::as_str),
        Some("2")
    );

    fixture
        .client
        .change_option(
            download.gid(),
            BTreeMap::from([
                ("split".into(), "1".into()),
                ("max-connection-per-server".into(), "1".into()),
            ]),
        )
        .await
        .expect("aria2-next must accept reducing HTTP range parallelism");
    let options = fixture.client.get_option(download.gid()).await.unwrap();
    assert_eq!(options.get("split").map(String::as_str), Some("1"));
    assert_eq!(
        options.get("max-connection-per-server").map(String::as_str),
        Some("1")
    );

    download.release();
    fixture.wait_complete(download.gid()).await;
    fixture.save_and_shutdown().await;
}

#[tokio::test]
async fn bundled_aria2_downloads_pauses_resumes_and_stops() {
    // Catches missing RPC control, synthetic progress, corrupt range output, and child leaks.
    let mut fixture = LiveAria2Fixture::start().await;
    let download = fixture.add_slow_download("complete.bin").await;
    let gid = download.gid();
    let progress = fixture.wait_progress(gid).await;
    assert!(progress.completed_length.parse::<usize>().unwrap() < PAYLOAD_BYTES);
    let paused = fixture.pause(gid).await;
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        let status = fixture.client.tell_status(gid).await.unwrap();
        assert_eq!(status.status, "paused");
        assert_eq!(status.completed_length, paused.completed_length);
        sleep(POLL).await;
    }
    assert_eq!(fixture.client.unpause(gid).await.unwrap(), gid);
    download.release();
    fixture.wait_complete(gid).await;
    fixture.assert_download_sha256("complete.bin");
    assert!(
        fixture.http.resumed_requests.load(Ordering::SeqCst) > 0,
        "unpause must exercise an actual HTTP Range continuation"
    );
    fixture.save_and_shutdown().await;
    println!("Aria2 Next 3.0.9: real progress, pause, Range resume, SHA-256, session save and owned-child shutdown passed");
}

#[tokio::test]
async fn bundled_aria2_does_not_restore_or_redownload_completed_session_entries() {
    // Catches completed HTTP transfers being force-saved and loaded as new work on restart.
    let mut fixture = LiveAria2Fixture::start().await;
    let gid = fixture
        .add_download_without_hold("completed-once.bin")
        .await;
    fixture.wait_complete(&gid).await;
    fixture.assert_download_sha256("completed-once.bin");
    let control_path = fixture.config.download_dir.join("completed-once.bin.aria2");
    assert!(
        !control_path.exists(),
        "a completed transfer must not retain an aria2 control file"
    );
    let requests_after_completion = fixture.http.total_requests.load(Ordering::SeqCst);

    fixture.save_and_shutdown().await;
    fixture.restart().await;

    assert!(
        fixture.client.tell_status(&gid).await.is_err(),
        "a completed GID must not be restored from the saved session"
    );
    assert_eq!(
        fixture.http.total_requests.load(Ordering::SeqCst),
        requests_after_completion,
        "restarting from the saved session must not issue another HTTP request"
    );
    assert!(
        !control_path.exists(),
        "restarting must not create a new control file for a completed transfer"
    );
    fixture.assert_download_sha256("completed-once.bin");
    fixture.terminate_for_cleanup();
}

#[tokio::test]
async fn bundled_aria2_restores_session_and_cancel_preserves_partial_files() {
    // Catches session options/GID loss across restart and destructive cancellation.
    let mut fixture = LiveAria2Fixture::start().await;
    let recovered = fixture.add_slow_download("recovered.bin").await;
    let gid = recovered.gid();
    fixture.wait_progress(gid).await;
    let paused = fixture.pause(gid).await;
    // Let the fixture's response drain before stopping aria2. Its paused GID must remain
    // frozen, proving partial-state preservation without an idle HTTP connection.
    recovered.release();
    let still_paused = fixture.client.tell_status(gid).await.unwrap();
    assert_eq!(still_paused.status, "paused");
    assert_eq!(still_paused.completed_length, paused.completed_length);
    let session = fixture.save_and_shutdown().await;
    assert!(
        session.contains(&format!("gid={gid}")),
        "unfinished GID missing from saved session"
    );
    assert!(session.contains("/payload.bin"));
    fixture.restart().await;
    let restored = fixture.client.tell_status(&gid).await.unwrap();
    if restored.status == "paused" {
        fixture.client.unpause(gid).await.unwrap();
    }
    recovered.release();
    fixture.wait_complete(gid).await;
    fixture.assert_download_sha256("recovered.bin");
    assert!(fixture.http.resumed_requests.load(Ordering::SeqCst) > 0);

    let cancelled = fixture.add_slow_download("cancelled.bin").await;
    let cancelled_gid = cancelled.gid();
    fixture.wait_progress(cancelled_gid).await;
    let paused = fixture.pause(cancelled_gid).await;
    cancelled.release();
    let still_paused = fixture.client.tell_status(cancelled_gid).await.unwrap();
    assert_eq!(still_paused.status, "paused");
    assert_eq!(still_paused.completed_length, paused.completed_length);
    let path = fixture.config.download_dir.join("cancelled.bin");
    let partial = std::fs::read(&path).unwrap();
    assert!(!partial.is_empty());
    assert_eq!(
        fixture.client.remove(cancelled_gid).await.unwrap(),
        cancelled_gid
    );
    // aria2 drops paused GIDs immediately on remove; they need not appear in tellStopped.
    timeout_at(Instant::now() + Duration::from_secs(3), async {
        loop {
            let mut pending = fixture.client.tell_active().await.unwrap();
            pending.extend(fixture.client.tell_waiting(0, 100).await.unwrap());
            if pending.iter().all(|status| status.gid != cancelled_gid) {
                break;
            }
            sleep(POLL).await;
        }
        for status in fixture.client.tell_stopped(0, 100).await.unwrap() {
            if status.gid == cancelled_gid {
                assert_eq!(status.status, "removed");
            }
        }
    })
    .await
    .expect("cancelled GID remained in the engine queue");
    assert_eq!(std::fs::read(path).unwrap(), partial);
    fixture.terminate_for_cleanup();
    println!("Aria2 Next 3.0.9: saved GID recovery, Range resume, SHA-256 and cancellation preserving partial files passed");
}
