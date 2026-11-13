use super::*;
use axum::{
    body::Body,
    http::{header, HeaderMap, Method, Response, StatusCode},
    routing::get,
    Router,
};
use futures_util::FutureExt;
use std::{
    panic::AssertUnwindSafe,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

// Deliberately opaque bytes: this test verifies credentials and exact transfer,
// not media decoding, and does not need an FFmpeg encoder.
const PAYLOAD: &[u8] = b"VersaDown cookie transfer fixture; not encoded audio or video.\n";
const PAGE: &str = "<html><title>Cookie fixture</title><video controls><source src='/fixture.mp4' type='video/mp4'></video></html>";
const PAGE_URL: &str = "http://gated.example.test/article";

fn has_cookie(headers: &HeaderMap, name: &str, value: &str) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .any(|cookie| cookie.trim().split_once('=') == Some((name, value)))
}

fn source_cookies(generation: usize) -> String {
    format!(
        "# Netscape HTTP Cookie File\n\
         gated.example.test\tFALSE\t/\tFALSE\t0\tauth\tcredential-{generation}\n"
    )
}

async fn terminal_task(store: &TaskStore, id: Uuid) -> DownloadTask {
    loop {
        let task = store.find(id).unwrap();
        if matches!(task.status, TaskStatus::Completed | TaskStatus::Failed) {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
#[ignore = "requires installed yt-dlp and local TCP"]
async fn cookie_file_authenticates_preview_and_aria2_retry_without_modifying_the_source() {
    extractor::set_resource_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("source-cookies.txt");
    let original = source_cookies(1);
    std::fs::write(&cookie_path, &original).unwrap();

    let generation = Arc::new(AtomicUsize::new(1));
    let denied_pages = Arc::new(AtomicUsize::new(0));
    let retry_media_requests = Arc::new(AtomicUsize::new(0));
    let missing_session_requests = Arc::new(AtomicUsize::new(0));
    let (page_generation, page_denials) = (generation.clone(), denied_pages.clone());
    let (media_generation, media_requests, missing_sessions) = (
        generation.clone(),
        retry_media_requests.clone(),
        missing_session_requests.clone(),
    );
    let site = Router::new()
        .route(
            "/article",
            get(move |headers: HeaderMap| {
                let (generation, denials) = (page_generation.clone(), page_denials.clone());
                async move {
                    let generation = generation.load(Ordering::SeqCst);
                    if !has_cookie(&headers, "auth", &format!("credential-{generation}")) {
                        denials.fetch_add(1, Ordering::SeqCst);
                        return Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .body(Body::empty())
                            .unwrap();
                    }
                    Response::builder()
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(
                            header::SET_COOKIE,
                            format!("page-session=issued-{generation}; Path=/; HttpOnly"),
                        )
                        .body(Body::from(PAGE))
                        .unwrap()
                }
            }),
        )
        .route(
            "/fixture.mp4",
            get(move |method: Method, headers: HeaderMap| {
                let (generation, requests, missing_sessions) = (
                    media_generation.clone(),
                    media_requests.clone(),
                    missing_sessions.clone(),
                );
                async move {
                    let generation = generation.load(Ordering::SeqCst);
                    let authenticated =
                        has_cookie(&headers, "auth", &format!("credential-{generation}"));
                    let session =
                        has_cookie(&headers, "page-session", &format!("issued-{generation}"));
                    if !authenticated || !session {
                        if authenticated && !session {
                            missing_sessions.fetch_add(1, Ordering::SeqCst);
                        }
                        return Response::builder()
                            .status(StatusCode::FORBIDDEN)
                            .body(Body::empty())
                            .unwrap();
                    }
                    if generation == 2 && method == Method::GET {
                        requests.fetch_add(1, Ordering::SeqCst);
                    }
                    Response::builder()
                        .header(header::CONTENT_TYPE, "video/mp4")
                        .header(header::CONTENT_LENGTH, PAYLOAD.len())
                        .body(Body::from(PAYLOAD))
                        .unwrap()
                }
            }),
        );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", socket.local_addr().unwrap());
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = VideoDownloads::new(store.clone(), 1, |_| {});
    let server = tokio::spawn(async move { axum::serve(socket, site).await.unwrap() });
    let options = || VideoOptions {
        proxy: Some(proxy.clone()),
        cookie_file: Some(cookie_path.clone()),
        ..Default::default()
    };

    // Capture assertion failures as well as timeouts so every worker is stopped
    // before this test drops its server, files, or in-memory task store.
    let result = tokio::time::timeout(
        Duration::from_secs(90),
        AssertUnwindSafe(async {
            let page = || sites::manual_page(&PAGE_URL.parse().unwrap()).unwrap();
            let anonymous = VideoOptions {
                proxy: Some(proxy.clone()),
                ..Default::default()
            };
            assert_eq!(
                engine.prepare(page(), &anonymous).await.unwrap_err(),
                "video.error.accessDenied"
            );
            let preview = engine.prepare(page(), &options()).await.unwrap();
            assert_eq!(preview.formats.len(), 1);
            assert_eq!(std::fs::read(&cookie_path).unwrap(), original.as_bytes());

            generation.store(2, Ordering::SeqCst);
            let task = engine
                .submit(
                    preview.preview_id,
                    &preview.formats[0].id,
                    directory.path(),
                    false,
                    None,
                    options(),
                )
                .await
                .unwrap();
            assert_eq!(
                store.video_engine(task.id).unwrap(),
                VideoTransferEngine::Aria2
            );
            let failed = terminal_task(&store, task.id).await;
            assert_eq!(failed.status, TaskStatus::Failed);
            assert_eq!(failed.error.as_deref(), Some("video.error.accessDenied"));
            assert!(denied_pages.load(Ordering::SeqCst) >= 2);
            let output = store.reserved_output_path(task.id).unwrap().unwrap();
            assert!(
                !output.exists(),
                "failed authentication must not publish a file"
            );
            assert_eq!(std::fs::read(&cookie_path).unwrap(), original.as_bytes());

            // A retry must read the user's updated file, not reuse the preview's
            // or the failed worker's private cookie snapshot.
            let updated = source_cookies(2);
            std::fs::write(&cookie_path, &updated).unwrap();
            engine.start(failed, options()).await.unwrap();
            let finished = terminal_task(&store, task.id).await;
            assert_eq!(
                finished.status,
                TaskStatus::Completed,
                "{:?}",
                finished.error
            );
            assert_eq!(finished.completed_length, PAYLOAD.len() as u64);
            assert_eq!(std::fs::read(&output).unwrap(), PAYLOAD);
            assert!(retry_media_requests.load(Ordering::SeqCst) > 0);
            assert_eq!(missing_session_requests.load(Ordering::SeqCst), 0);
            assert_eq!(std::fs::read(&cookie_path).unwrap(), updated.as_bytes());
        })
        .catch_unwind(),
    )
    .await;
    let shutdown = tokio::time::timeout(Duration::from_secs(15), engine.shutdown()).await;
    server.abort();
    let _ = server.await;
    shutdown.expect("video workers did not stop during cleanup");
    match result {
        Ok(Ok(())) => (),
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("cookie preview/download/retry lifecycle timed out"),
    }
}
