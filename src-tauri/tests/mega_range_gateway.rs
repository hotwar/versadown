//! The range gateway's decrypting transform, against a local fixture that
//! behaves like a MEGA storage node (ciphertext addressed by `/<start>-<end>`).
//! The fixture's ciphertext comes from a hand-rolled AES-CTR keystream (block
//! encryption of counter blocks), anchored to a vector produced by Python's
//! `cryptography`, so it is independent of the `ctr` crate the gateway uses.

use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Block};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use std::sync::{Arc, Mutex};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;
use versadown_lib::aria2::{Aria2Api, Aria2LaunchConfig, Aria2Process};
use versadown_lib::cloud::download_transform::DownloadTransform;
use versadown_lib::cloud::range_gateway::{QuotaWait, RangeGateway};
use versadown_lib::{DownloadService, NewTask, TaskStore, UriSubmissionOptions};

const KEY: [u8; 16] = [
    0x05, 0x12, 0x1f, 0x2c, 0x39, 0x46, 0x53, 0x60, 0x6d, 0x7a, 0x87, 0x94, 0xa1, 0xae, 0xbb, 0xc8,
];
const NONCE: [u8; 8] = [0x02, 0x15, 0x28, 0x3b, 0x4e, 0x61, 0x74, 0x87];
/// First 48 bytes of AES-128-CTR(KEY, NONCE||counter) over `pattern`, from Python.
const PYTHON_CIPHERTEXT_PREFIX: &str = "0929ad1a5f25f3af68d55efec569340082e7d4634a610dc5007e29612e216fd327418c0c64810b538830927f681fe6dc";
const CHUNK: usize = 4 * 1024 * 1024;

fn plain(length: usize) -> Vec<u8> {
    (0..length).map(|i| ((i * 31 + 7) & 0xFF) as u8).collect()
}

fn encrypt(plain: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(&KEY.into());
    let mut out = Vec::with_capacity(plain.len());
    for (counter, block) in plain.chunks(16).enumerate() {
        let mut keystream = [0u8; 16];
        keystream[..8].copy_from_slice(&NONCE);
        keystream[8..].copy_from_slice(&(counter as u64).to_be_bytes());
        let mut keystream = Block::from(keystream);
        cipher.encrypt_block(&mut keystream);
        out.extend(block.iter().zip(keystream).map(|(byte, mask)| byte ^ mask));
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn the_fixture_cipher_matches_the_independent_python_vector() {
    let ciphertext = encrypt(&plain(48));
    assert_eq!(hex(&ciphertext), PYTHON_CIPHERTEXT_PREFIX);
}

#[derive(Clone)]
struct Node {
    ciphertext: Arc<Vec<u8>>,
    requests: Arc<Mutex<Vec<(usize, usize)>>>,
    mode: Mode,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Healthy,
    QuotaExceeded,
    ShortBody,
    /// The temporary URL has expired.
    Expired,
}

async fn serve_node(
    State(node): State<Node>,
    Path((_token, range)): Path<(String, String)>,
) -> Response {
    let (start, end) = range.split_once('-').unwrap();
    let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
    node.requests.lock().unwrap().push((start, end));
    match node.mode {
        Mode::Expired => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap(),
        Mode::QuotaExceeded => Response::builder()
            .status(509)
            .header("x-mega-time-left", "1800")
            .body(Body::empty())
            .unwrap(),
        // Chunked, one byte short: only the stream's end reveals the shortfall.
        Mode::ShortBody => Response::builder()
            .body(Body::from_stream(futures_util::stream::once(async move {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(
                    node.ciphertext[start..end].to_vec(),
                ))
            })))
            .unwrap(),
        Mode::Healthy => {
            assert!(end < node.ciphertext.len() && start <= end);
            Response::builder()
                .header(header::CONTENT_LENGTH, end - start + 1)
                .body(Body::from(node.ciphertext[start..=end].to_vec()))
                .unwrap()
        }
    }
}

struct Fixture {
    gateway: RangeGateway,
    task_id: Uuid,
    local_url: String,
    plain: Vec<u8>,
    requests: Arc<Mutex<Vec<(usize, usize)>>>,
    _server: tokio::task::JoinHandle<()>,
}

/// A fixture storage node; returns its temporary download URL.
async fn spawn_node(
    plain: &[u8],
    mode: Mode,
) -> (
    String,
    Arc<Mutex<Vec<(usize, usize)>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let node = Node {
        ciphertext: Arc::new(encrypt(plain)),
        requests: Arc::clone(&requests),
        mode,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}/dl/TOKEN", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/dl/{token}/{range}", get(serve_node))
                .with_state(node),
        )
        .await
        .unwrap();
    });
    (upstream, requests, server)
}

fn register(
    gateway: &RangeGateway,
    task_id: Uuid,
    upstream: String,
    length: usize,
    refreshing: bool,
) -> String {
    let registration = gateway
        .register_transformed(
            task_id,
            upstream,
            vec![],
            None,
            DownloadTransform::aes_ctr_path_range(KEY, NONCE, length as u64),
            refreshing,
        )
        .unwrap();
    let local_url = registration.url().to_owned();
    std::mem::forget(registration);
    local_url
}

async fn fixture(length: usize, mode: Mode) -> Fixture {
    let plain = plain(length);
    let (upstream, requests, server) = spawn_node(&plain, mode).await;
    let gateway = RangeGateway::start().await.unwrap();
    let task_id = Uuid::new_v4();
    let local_url = register(&gateway, task_id, upstream, length, false);
    Fixture {
        gateway,
        task_id,
        local_url,
        plain,
        requests,
        _server: server,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn get_range(fixture: &Fixture, range: &str) -> reqwest::Response {
    client()
        .get(&fixture.local_url)
        .header(header::RANGE, range)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn unaligned_start_is_decrypted_correctly() {
    let fixture = fixture(100_003, Mode::Healthy).await;
    for (start, end) in [
        (0usize, 0usize),
        (1, 14),
        (15, 16),
        (17, 4_000),
        (99_990, 100_002),
    ] {
        let response = get_range(&fixture, &format!("bytes={start}-{end}")).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            format!("bytes {start}-{end}/100003")
        );
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            (end - start + 1).to_string()
        );
        assert_eq!(
            response.bytes().await.unwrap().as_ref(),
            &fixture.plain[start..=end],
            "{start}-{end}"
        );
    }
}

#[tokio::test]
async fn range_crossing_four_mib_chunk_boundaries_is_exact_and_bounded() {
    let length = 2 * CHUNK + 12_345;
    let fixture = fixture(length, Mode::Healthy).await;
    // Starts mid-block, ends mid-block, spans three upstream requests.
    let (start, end) = (CHUNK - 17, 2 * CHUNK + 9);
    let response = get_range(&fixture, &format!("bytes={start}-{end}")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        &fixture.plain[start..=end]
    );
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        vec![
            (CHUNK - 17, 2 * CHUNK - 18),
            (2 * CHUNK - 17, 2 * CHUNK + 9)
        ]
    );
    assert!(requests.iter().all(|(from, to)| to - from < CHUNK));
}

#[tokio::test]
async fn open_ended_and_full_responses_return_the_whole_plaintext() {
    let length = CHUNK + 777;
    let fixture = fixture(length, Mode::Healthy).await;
    let open = get_range(&fixture, "bytes=5-").await;
    assert_eq!(open.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        open.headers()[header::CONTENT_RANGE],
        format!("bytes 5-{}/{length}", length - 1)
    );
    assert_eq!(open.bytes().await.unwrap().as_ref(), &fixture.plain[5..]);
    let whole = client().get(&fixture.local_url).send().await.unwrap();
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(whole.bytes().await.unwrap().as_ref(), &fixture.plain[..]);
}

#[tokio::test]
async fn empty_file_get_and_head_succeed_without_an_upstream_range() {
    let fixture = fixture(0, Mode::Healthy).await;
    for method in [reqwest::Method::GET, reqwest::Method::HEAD] {
        let response = client()
            .request(method, &fixture.local_url)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert!(response.bytes().await.unwrap().is_empty());
    }
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_file_ranges_remain_unsatisfiable_without_touching_upstream() {
    let fixture = fixture(0, Mode::Healthy).await;
    for range in ["bytes=0-", "bytes=0-0", "bytes=1-10"] {
        let response = get_range(&fixture, range).await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */0");
        assert!(response.bytes().await.unwrap().is_empty());
    }
    for range in ["bytes=-1", "bytes=5-4", "bytes=0-0,1-2"] {
        let response = get_range(&fixture, range).await;
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(response.bytes().await.unwrap().is_empty());
    }
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn head_reports_the_plain_size_without_touching_upstream() {
    let fixture = fixture(54_321, Mode::Healthy).await;
    let response = client().head(&fixture.local_url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "54321");
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_segments_like_aria2_split_downloads_reassemble_the_file() {
    let length = 3 * CHUNK + 4_321;
    let fixture = Arc::new(fixture(length, Mode::Healthy).await);
    // Four connections, segment starts that are not multiples of 16.
    let cuts = [0usize, 1_048_581, 2_097_163, 3_145_735, length];
    let mut tasks = Vec::new();
    for window in cuts.windows(2) {
        let (start, end) = (window[0], window[1] - 1);
        let fixture = Arc::clone(&fixture);
        tasks.push(tokio::spawn(async move {
            let response = get_range(&fixture, &format!("bytes={start}-{end}")).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            (start, response.bytes().await.unwrap())
        }));
    }
    let mut assembled = vec![0u8; length];
    for task in tasks {
        let (start, bytes) = task.await.unwrap();
        assembled[start..start + bytes.len()].copy_from_slice(&bytes);
    }
    assert_eq!(assembled, fixture.plain);
}

#[tokio::test]
async fn unsatisfiable_ranges_are_rejected_and_oversized_ends_are_clamped() {
    let fixture = fixture(1_000, Mode::Healthy).await;
    let beyond = get_range(&fixture, "bytes=1000-1010").await;
    assert_eq!(beyond.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let clamped = get_range(&fixture, "bytes=990-5000").await;
    assert_eq!(clamped.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        clamped.headers()[header::CONTENT_RANGE],
        "bytes 990-999/1000"
    );
    assert_eq!(
        clamped.bytes().await.unwrap().as_ref(),
        &fixture.plain[990..]
    );
}

#[tokio::test]
async fn upstream_quota_exhaustion_is_surfaced_and_never_leaks_bytes() {
    let fixture = fixture(10_000, Mode::QuotaExceeded).await;
    let response = get_range(&fixture, "bytes=0-99").await;
    assert_eq!(response.status().as_u16(), 509);
    assert!(response.bytes().await.unwrap().is_empty());
    // The node's `x-mega-time-left` is kept for the task, once.
    assert_eq!(
        fixture.gateway.take_quota_wait(fixture.task_id),
        Some(QuotaWait {
            seconds: Some(1800),
            account: false
        })
    );
    assert_eq!(fixture.gateway.take_quota_wait(fixture.task_id), None);
}

#[tokio::test]
async fn quota_exhaustion_of_an_account_download_is_marked_as_the_accounts() {
    let plain = plain(10_000);
    let (upstream, _requests, _server) = spawn_node(&plain, Mode::QuotaExceeded).await;
    let gateway = RangeGateway::start().await.unwrap();
    let task_id = Uuid::new_v4();
    let registration = gateway
        .register_transformed(
            task_id,
            upstream,
            vec![],
            None,
            DownloadTransform::aes_ctr_path_range(KEY, NONCE, 10_000).with_account_quota(),
            false,
        )
        .unwrap();
    let response = client()
        .get(registration.url())
        .header(header::RANGE, "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 509);
    assert_eq!(
        gateway.take_quota_wait(task_id),
        Some(QuotaWait {
            seconds: Some(1800),
            account: true
        })
    );
}

#[tokio::test]
async fn an_expired_url_is_replaced_in_place_and_the_download_continues() {
    // The first temporary URL has expired (403). Re-resolving registers the
    // fresh URL for the same task: the local URL aria2 holds stays valid.
    let fixture = fixture(300_000, Mode::Expired).await;
    let expired = get_range(&fixture, "bytes=1000-1999").await;
    assert_eq!(expired.status(), StatusCode::FORBIDDEN);
    let (fresh, requests, _node) = spawn_node(&fixture.plain, Mode::Healthy).await;
    let local_url = register(&fixture.gateway, fixture.task_id, fresh, 300_000, true);
    assert_eq!(local_url, fixture.local_url);
    let resumed = get_range(&fixture, "bytes=150001-250000").await;
    assert_eq!(resumed.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resumed.bytes().await.unwrap().as_ref(),
        &fixture.plain[150_001..=250_000]
    );
    assert_eq!(requests.lock().unwrap().as_slice(), [(150_001, 250_000)]);
}

#[tokio::test]
async fn a_truncated_upstream_body_aborts_the_local_stream() {
    let fixture = fixture(10_000, Mode::ShortBody).await;
    let response = get_range(&fixture, "bytes=0-99").await;
    let outcome = response.bytes().await;
    assert!(
        outcome.is_err(),
        "a short upstream body must not look like a complete range"
    );
}

#[tokio::test]
async fn removed_tokens_are_gone() {
    let fixture = fixture(100, Mode::Healthy).await;
    let token = fixture.local_url.rsplit('/').next().unwrap().to_owned();
    fixture.gateway.remove_token(&token);
    let response = get_range(&fixture, "bytes=0-9").await;
    assert_eq!(response.status(), StatusCode::GONE);
}

/// Drive the bundled engine against the gateway; returns the final aria2 state.
async fn engine_download(fixture: &Fixture, connections: u8) -> (String, Option<String>, PathBuf) {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store.create(NewTask::direct(&fixture.local_url)).unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = reserved.local_addr().unwrap().port();
    aria2.rpc_token = format!("mega-gateway-{}", aria2.rpc_port);
    drop(reserved);
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let binary = versadown_lib::engine_binary::EngineBinary::for_current_target()
        .unwrap()
        .resolve(&manifest, &manifest)
        .unwrap();
    let root = tempfile::tempdir().unwrap();
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
            vec![fixture.local_url.clone()],
            UriSubmissionOptions {
                directory: Some(output.clone()),
                output_name: Some("decrypted.bin".into()),
                max_connections_per_server: Some(connections),
                max_tries: Some(1),
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let outcome = loop {
        let status = client
            .tell_status(submitted.aria_gid.as_deref().unwrap())
            .await
            .unwrap();
        if matches!(status.status.as_str(), "error" | "complete") {
            break (status.status, status.error_message);
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "transfer timed out"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    process.terminate_fallback().unwrap();
    let (state, message) = outcome;
    // Keep the temp dir alive for the caller to read the file.
    let kept = root.keep();
    (state, message, kept.join("downloads").join("decrypted.bin"))
}

#[tokio::test]
async fn bundled_engine_downloads_the_decrypted_file_with_split_connections() {
    let length = 3 * CHUNK + 1_001;
    let fixture = fixture(length, Mode::Healthy).await;
    let (state, message, file) = engine_download(&fixture, 4).await;
    assert_eq!(state, "complete", "{message:?}");
    assert_eq!(std::fs::read(&file).unwrap(), fixture.plain);
    // aria2 really used several segments, every upstream request stayed bounded.
    let requests = fixture.requests.lock().unwrap().clone();
    assert!(requests.len() > 3, "{requests:?}");
    assert!(requests.iter().all(|(from, to)| to - from < CHUNK));
    std::fs::remove_dir_all(file.parent().unwrap().parent().unwrap()).unwrap();
}

#[tokio::test]
async fn bundled_engine_reports_a_quota_failure_as_an_http_509() {
    let fixture = fixture(100_000, Mode::QuotaExceeded).await;
    let (state, message, file) = engine_download(&fixture, 1).await;
    assert_eq!(state, "error");
    eprintln!("aria2 quota failure message: {message:?}");
    std::fs::remove_dir_all(file.parent().unwrap().parent().unwrap()).unwrap();
}

#[test]
fn quota_failures_name_the_account_only_when_it_was_used() {
    use versadown_lib::cloud::mega::MegaFailure;
    let wait = |seconds, account| QuotaWait { seconds, account };
    assert_eq!(
        wait(Some(60), false).failure(),
        Some(MegaFailure::QuotaExceeded {
            retry_after_secs: Some(60)
        })
    );
    // Without a wait the anonymous refusal keeps the generic message.
    assert_eq!(wait(None, false).failure(), None);
    assert_eq!(
        wait(Some(60), true).failure(),
        Some(MegaFailure::AccountQuotaExceeded {
            retry_after_secs: Some(60)
        })
    );
    assert_eq!(
        wait(None, true).failure(),
        Some(MegaFailure::AccountQuotaExceeded {
            retry_after_secs: None
        })
    );
}
