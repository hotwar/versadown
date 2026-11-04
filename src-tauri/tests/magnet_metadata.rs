use axum::{
    extract::Json,
    http::{header::USER_AGENT, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use versadown_lib::{resolve_magnet_metadata, resolve_magnet_torrent, MagnetMetadataSettings};

const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

#[tokio::test]
async fn keeps_the_preview_when_the_cached_torrent_is_missing_empty_or_oversized() {
    for (status, length) in [
        (StatusCode::NOT_FOUND, "0"),
        (StatusCode::OK, "0"),
        (StatusCode::OK, "16777217"),
    ] {
        let app = Router::new()
            .route(
                "/api/v1/metadata",
                post(|| async {
                    Json(json!({"name": "fixture", "download_url": "/download/fixture"}))
                }),
            )
            .route(
                "/download/fixture",
                get(move || async move { (status, [("content-length", length)], "") }),
            );
        let base_url = serve(app).await;
        let resolved = resolve_magnet_metadata(
            &MagnetMetadataSettings {
                enabled: true,
                base_url,
            },
            MAGNET,
        )
        .await
        .unwrap()
        .expect("cache failures must preserve the preview");
        assert_eq!(resolved.title.as_deref(), Some("fixture"));
        assert_eq!(resolved.torrent, None);
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

#[tokio::test]
async fn resolves_metadata_service_result_to_a_torrent_for_local_aria2() {
    let app = Router::new()
        .route(
            "/api/v1/metadata",
            post(|Json(request): Json<Value>| async move {
                assert_eq!(request["magnet_uri"], MAGNET);
                Json(json!({
                    "info_hash": "0123456789abcdef0123456789abcdef01234567",
                    "name": "fixture",
                    "size": 13,
                    "files": [],
                    "download_url": "/download/0123456789abcdef0123456789abcdef01234567"
                }))
            }),
        )
        .route(
            "/download/0123456789abcdef0123456789abcdef01234567",
            get(|| async { (StatusCode::OK, b"torrent-bytes".to_vec()) }),
        );
    let base_url = serve(app).await;

    let result = resolve_magnet_torrent(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await
    .unwrap();

    let resolved = result.expect("metadata service should return its cached torrent");
    assert_eq!(resolved.torrent, "dG9ycmVudC1ieXRlcw==");
    assert_eq!(resolved.title.as_deref(), Some("fixture"));
}

#[tokio::test]
async fn identifies_as_versadown_when_requesting_metadata_service() {
    let observed_user_agent = Arc::new(Mutex::new(None));
    let captured_user_agent = Arc::clone(&observed_user_agent);
    let app = Router::new()
        .route(
            "/api/v1/metadata",
            post(move |headers: HeaderMap| {
                let captured_user_agent = Arc::clone(&captured_user_agent);
                async move {
                    *captured_user_agent.lock().unwrap() = headers
                        .get(USER_AGENT)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    Json(json!({
                        "download_url": "/download/0123456789abcdef0123456789abcdef01234567"
                    }))
                }
            }),
        )
        .route(
            "/download/0123456789abcdef0123456789abcdef01234567",
            get(|| async { (StatusCode::OK, b"torrent-bytes".to_vec()) }),
        );
    let base_url = serve(app).await;

    let result = resolve_magnet_torrent(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(
        observed_user_agent.lock().unwrap().as_deref(),
        Some("VersaDown/0.1")
    );
}

#[tokio::test]
async fn returns_no_torrent_when_the_metadata_service_has_no_cached_download() {
    let app = Router::new().route(
        "/api/v1/metadata",
        post(|| async {
            Json(json!({
                "info_hash": "0123456789abcdef0123456789abcdef01234567",
                "name": "fixture",
                "size": 13,
                "files": []
            }))
        }),
    );
    let base_url = serve(app).await;

    let result = resolve_magnet_torrent(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await
    .unwrap();

    assert_eq!(result, None);
}

#[tokio::test]
async fn keeps_a_metadata_only_result_for_the_confirmation_preview() {
    let app = Router::new().route(
        "/api/v1/metadata",
        post(|| async {
            Json(json!({
                "info_hash": "0123456789abcdef0123456789abcdef01234567",
                "name": "fixture-directory",
                "size": 21,
                "files": [
                    { "index": 1, "path": "fixture-directory/first.bin", "size": 13 },
                    { "index": 2, "path": "fixture-directory/second.bin", "size": 8 }
                ]
            }))
        }),
    );
    let base_url = serve(app).await;

    let result = resolve_magnet_metadata(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await
    .unwrap()
    .expect("a metadata-only response should remain available to the UI");

    assert_eq!(result.torrent, None);
    assert_eq!(result.title.as_deref(), Some("fixture-directory"));
    assert_eq!(
        result
            .files
            .into_iter()
            .map(|file| (file.index, file.path, file.length))
            .collect::<Vec<_>>(),
        vec![
            (1, "fixture-directory/first.bin".to_string(), 13),
            (2, "fixture-directory/second.bin".to_string(), 8),
        ]
    );
}

#[tokio::test]
async fn rewrites_a_service_default_localhost_download_url_to_the_configured_server() {
    let app = Router::new()
        .route(
            "/api/v1/metadata",
            post(|| async {
                Json(json!({
                    "info_hash": "0123456789abcdef0123456789abcdef01234567",
                    "name": "fixture",
                    "size": 13,
                    "files": [],
                    "download_url": "http://localhost:8080/download/0123456789abcdef0123456789abcdef01234567"
                }))
            }),
        )
        .route(
            "/download/0123456789abcdef0123456789abcdef01234567",
            get(|| async { (StatusCode::OK, b"torrent-bytes".to_vec()) }),
        );
    let base_url = serve(app).await;

    let result = resolve_magnet_torrent(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await
    .unwrap();

    let resolved = result.expect("rewritten download URL should return its torrent");
    assert_eq!(resolved.torrent, "dG9ycmVudC1ieXRlcw==");
    assert_eq!(resolved.title.as_deref(), Some("fixture"));
}

#[tokio::test]
async fn retries_service_unavailable_until_metadata_is_available() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = Arc::clone(&attempts);
    let app = Router::new()
        .route(
            "/api/v1/metadata",
            post(move || {
                let attempts = Arc::clone(&observed_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        (StatusCode::SERVICE_UNAVAILABLE, "busy").into_response()
                    } else {
                        Json(json!({
                            "download_url": "/download/0123456789abcdef0123456789abcdef01234567"
                        }))
                        .into_response()
                    }
                }
            }),
        )
        .route(
            "/download/0123456789abcdef0123456789abcdef01234567",
            get(|| async { (StatusCode::OK, b"torrent-bytes".to_vec()) }),
        );
    let base_url = serve(app).await;

    let result = resolve_magnet_torrent(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await;

    assert!(result.unwrap().is_some());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn does_not_retry_invalid_metadata_request() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = Arc::clone(&attempts);
    let app = Router::new().route(
        "/api/v1/metadata",
        post(move || {
            let attempts = Arc::clone(&observed_attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                (StatusCode::BAD_REQUEST, "invalid magnet")
            }
        }),
    );
    let base_url = serve(app).await;

    let error = resolve_magnet_torrent(
        &MagnetMetadataSettings {
            enabled: true,
            base_url,
        },
        MAGNET,
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "磁链解析服务返回 HTTP 400");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
