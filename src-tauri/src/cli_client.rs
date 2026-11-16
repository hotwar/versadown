//! Shared local transport for CLI and MCP. Never opens the task database.
use crate::cli_protocol::{self, ApiError, Discovery, HealthDto, Request, Response, TaskDto, TaskListDto, TaskResultDto, PROTOCOL_VERSION};
use std::{path::PathBuf, time::Duration};
#[path = "cli_bootstrap.rs"]
mod bootstrap;

#[derive(Clone)]
pub struct LocalClient {
    http: reqwest::Client,
    discovery: Option<PathBuf>,
    lang: String,
    auto_start: bool,
}

impl LocalClient {
    pub fn new(discovery: Option<PathBuf>, lang: &str) -> Result<Self, Response> {
        let http = reqwest::Client::builder().no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(30)).build()
            .map_err(|_| Response::failure("INTERNAL_ERROR", lang))?;
        Ok(Self { http, discovery, lang: lang.to_owned(), auto_start: true })
    }

    pub fn with_auto_start(mut self, enabled: bool) -> Self {
        self.auto_start = enabled;
        self
    }

    fn discovery_path(&self) -> Result<PathBuf, Response> {
        self.discovery.clone()
            .or_else(|| std::env::var_os("VD_CLI_DISCOVERY").map(PathBuf::from))
            .or_else(cli_protocol::default_discovery_path)
            .ok_or_else(|| Response::failure("HOST_UNAVAILABLE", &self.lang))
    }

    async fn discover(&self) -> Result<Discovery, Response> {
        let path = self.discovery_path()?;
        // Bound the read itself, not just a potentially unbounded allocation.
        use tokio::io::AsyncReadExt;
        let file = tokio::fs::File::open(path).await
            .map_err(|_| Response::failure("HOST_UNAVAILABLE", &self.lang))?;
        let mut bytes = Vec::new();
        file.take(16_385).read_to_end(&mut bytes).await
            .map_err(|_| Response::failure("HOST_UNAVAILABLE", &self.lang))?;
        if bytes.len() > 16_384 { return Err(Response::failure("PROTOCOL_ERROR", &self.lang)); }
        let discovery: Discovery = serde_json::from_slice(&bytes)
            .map_err(|_| Response::failure("PROTOCOL_ERROR", &self.lang))?;
        if discovery.version != PROTOCOL_VERSION || discovery.port == 0
            || !(32..=256).contains(&discovery.token.len())
            || !discovery.token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        { return Err(Response::failure("PROTOCOL_ERROR", &self.lang)); }
        Ok(discovery)
    }

    pub async fn call(&self, request: &Request) -> Response {
        let automatic = bootstrap::automatic_start_allowed(!self.auto_start, self.discovery.is_some(), std::env::var_os("VD_CLI_DISCOVERY").is_some());
        let discovery = if automatic {
            let path = match self.discovery_path() { Ok(path) => path, Err(error) => return error };
            // Probe only health before spawning. Once a business request has
            // been sent, a disconnect must never launch and replay that request.
            match bootstrap::ensure_host(&path, &self.lang, || self.probe()).await {
                Ok((_discovery, health)) if request.command == "health" => return health,
                Ok((discovery, _)) => discovery,
                Err(error) => return error,
            }
        } else {
            match self.discover().await { Ok(value) => value, Err(error) => return error }
        };
        self.call_discovered(discovery, request).await
    }

    async fn probe(&self) -> Result<(Discovery, Response), Response> {
        let discovery = self.discover().await?;
        let request = Request { command: "health".into(), lang: Some(self.lang.clone()), ..Request::default() };
        let health = self.call_discovered(discovery, &request).await;
        if !health.ok { return Err(health); }
        // Refresh in case the health probe observed a rotated credential.
        Ok((self.discover().await?, health))
    }

    async fn call_discovered(&self, discovery: Discovery, request: &Request) -> Response {
        let response = call(&self.http, &discovery, request, &self.lang).await;
        // Only retry a rejected request when discovery actually changed. No
        // timeout/disconnect replay: acceptance may already have happened.
        if response.error.as_ref().is_some_and(|e| e.code == "UNAUTHORIZED") {
            if let Ok(current) = self.discover().await {
                if current.port != discovery.port || current.token != discovery.token {
                    return call(&self.http, &current, request, &self.lang).await;
                }
            }
        }
        response
    }
}

pub(crate) async fn call(client: &reqwest::Client, discovery: &Discovery, request: &Request, lang: &str) -> Response {
    let result = client.post(format!("http://127.0.0.1:{}/v1/call", discovery.port))
        .bearer_auth(&discovery.token).json(request).send().await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => return Response::failure(if error.is_timeout() { "TIMEOUT" } else { "HOST_UNAVAILABLE" }, lang),
    };
    if response.status() == reqwest::StatusCode::UNAUTHORIZED || response.status() == reqwest::StatusCode::FORBIDDEN {
        // A valid API body may distinguish disabled/policy denied from bad auth.
    } else if !response.status().is_success()
        && !matches!(response.status(), reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::TOO_MANY_REQUESTS) {
        return Response::failure("PROTOCOL_ERROR", lang);
    }
    let unauthorized = response.status() == reqwest::StatusCode::UNAUTHORIZED;
    let forbidden = response.status() == reqwest::StatusCode::FORBIDDEN;
    let http_error = !response.status().is_success();
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= 8 * 1024 * 1024 => bytes.extend_from_slice(&chunk),
            Ok(None) => break,
            _ => return Response::failure("PROTOCOL_ERROR", lang),
        }
    }
    let mut result: Response = match serde_json::from_slice(&bytes) {
        Ok(result) => result,
        Err(_) => return Response::failure(if unauthorized || forbidden { "UNAUTHORIZED" } else { "PROTOCOL_ERROR" }, lang),
    };
    if result.ok != result.error.is_none() || (result.ok && result.data.is_none())
        || (http_error && result.ok)
    { return Response::failure("PROTOCOL_ERROR", lang); }
    if let Some(error) = &result.error {
        if !known_error(&error.code) {
            return Response::failure("PROTOCOL_ERROR", lang);
        }
        let mut safe = Response::failure(&error.code, lang);
        // Only bounded, typed metadata can be preserved from the host.
        if let (Some(original), Some(projected)) = (&result.error, &mut safe.error) {
            projected.retry_after_ms = original.retry_after_ms.map(|ms| ms.min(86_400_000));
        }
        safe.data = result.data.take();
        result = safe;
    }
    if let Some(data) = result.data.take() {
        let projected = match request.command.as_str() {
            "health" => project::<HealthDto>(data),
            "list" => project_tasks(data, lang),
            "result" => project::<TaskResultDto>(data),
            "add" | "status" | "pause" | "resume" | "cancel" => project_task(data, lang),
            _ => Err(()),
        };
        match projected {
            Ok(data) => result.data = Some(data),
            Err(()) => return Response::failure("PROTOCOL_ERROR", lang),
        }
    }
    result
}

fn project<T: serde::de::DeserializeOwned + serde::Serialize>(data: serde_json::Value) -> Result<serde_json::Value, ()> {
    let data = serde_json::from_value::<T>(data).map_err(|_| ())?;
    serde_json::to_value(data).map_err(|_| ())
}

fn known_error(code: &str) -> bool {
    matches!(code, "INVALID_ARGUMENT" | "HOST_UNAVAILABLE" | "UNAUTHORIZED" | "NOT_FOUND" | "REQUEST_CONFLICT" | "DUPLICATE" | "INTERNAL_ERROR" | "TASK_FAILED" | "TASK_CANCELLED" | "TIMEOUT" | "INTERRUPTED" | "PROTOCOL_ERROR" | "UNSUPPORTED" | "ENGINE_UNAVAILABLE" | "ACTION_FAILED" | "FORBIDDEN" | "HOST_DISABLED" | "TASK_NOT_READY" | "BUSY" | "AUTH_REQUIRED" | "CLI_RECOVERY_REQUIRED")
}

fn sanitize_task(mut task: TaskDto, lang: &str) -> TaskDto {
    if let Some(failure) = task.failure.take() {
        task.failure = Some(ApiError::new(if known_error(&failure.code) { &failure.code } else { "TASK_FAILED" }, lang));
    }
    // This field is a finite action identifier, never arbitrary host text.
    if task.required_action.as_deref().is_some_and(|action| !matches!(action,
        "start_desktop" | "enable_desktop_agent" | "review_directory_permissions" | "open_desktop_login"
        | "refresh_discovery" | "query_task" | "retry_same_request" | "inspect_desktop" | "resume_task")) {
        task.required_action = Some("inspect_desktop".into());
    }
    task
}

fn project_task(data: serde_json::Value, lang: &str) -> Result<serde_json::Value, ()> {
    let task = serde_json::from_value::<TaskDto>(data).map_err(|_| ())?;
    serde_json::to_value(sanitize_task(task, lang)).map_err(|_| ())
}

fn project_tasks(data: serde_json::Value, lang: &str) -> Result<serde_json::Value, ()> {
    let mut list = serde_json::from_value::<TaskListDto>(data).map_err(|_| ())?;
    list.tasks = list.tasks.into_iter().map(|task| sanitize_task(task, lang)).collect();
    serde_json::to_value(list).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn transport_authenticates_and_redacts_server_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let n = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.contains("POST /v1/call"));
            assert!(request.contains(&format!("authorization: Bearer {}", "a".repeat(64))));
            let body = json!({"ok":false,"error":{"code":"INTERNAL_ERROR","message":"https://user:password@example.com/?token=secret","next_action":"secret"}}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let response = call(&reqwest::Client::builder().no_proxy().build().unwrap(),
            &Discovery {version:1,port,token:"a".repeat(64)},
            &Request {command:"health".into(),..Request::default()},"en").await;
        assert!(!serde_json::to_string(&response).unwrap().contains("secret"));
        assert_eq!(cli_protocol::exit_code(&response),1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn accepted_business_request_is_not_replayed_after_disconnect() {
        use tokio::io::AsyncReadExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(&path, serde_json::to_vec(&Discovery { version: 1, port, token: "a".repeat(64) }).unwrap()).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                let mut received = Vec::new();
                while !String::from_utf8_lossy(&received).contains("same-intent") {
                    let mut bytes = [0; 4096];
                    let size = stream.read(&mut bytes).await.unwrap();
                    assert!(size > 0 && received.len() + size <= 16_384);
                    received.extend_from_slice(&bytes[..size]);
                }
            }).await.unwrap();
            // Simulate acceptance followed by a lost response. The caller must
            // use its original request ID to retry, never automatically replay.
            drop(stream);
            assert!(tokio::time::timeout(Duration::from_millis(150), listener.accept()).await.is_err());
        });
        let response = LocalClient::new(Some(path), "en").unwrap().call(&Request {
            command: "add".into(), request_id: Some("same-intent".into()), ..Request::default()
        }).await;
        assert_eq!(response.error.unwrap().code, "HOST_UNAVAILABLE");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn discovery_limits_and_version_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        for (lang, guidance) in [
            ("en", ["VD is running", "enable", "Allow local Agent access", "Settings > Advanced", "discovery file"]),
            ("zh-CN", ["VD 已启动", "启用", "允许本机 Agent 调用", "设置 → 高级", "发现文件"]),
        ] {
            let client = LocalClient::new(Some(path.clone()), lang).unwrap();
            let response = client.discover().await.err().unwrap();
            assert_eq!(cli_protocol::exit_code(&response), 3);
            let error = response.error.unwrap();
            assert_eq!(error.code, "HOST_UNAVAILABLE");
            assert_eq!(error.next_action.as_deref(), Some("inspect_desktop"));
            assert!(!error.retryable);
            for instruction in guidance {
                assert!(error.message.contains(instruction), "missing {lang} guidance: {instruction}");
            }

            let disabled = Response::failure("HOST_DISABLED", lang);
            assert_eq!(cli_protocol::exit_code(&disabled), 4);
            let error = disabled.error.unwrap();
            assert_eq!(error.code, "HOST_DISABLED");
            assert_eq!(error.next_action.as_deref(), Some("enable_desktop_agent"));
        }
        let client = LocalClient::new(Some(path.clone()), "en").unwrap();
        std::fs::write(&path," ".repeat(16_385)).unwrap();
        assert_eq!(client.discover().await.err().unwrap().error.unwrap().code,"PROTOCOL_ERROR");
        std::fs::write(&path,json!({"version":999,"port":1234,"token":"a".repeat(64)}).to_string()).unwrap();
        assert_eq!(client.discover().await.err().unwrap().error.unwrap().code,"PROTOCOL_ERROR");
    }

    #[tokio::test]
    async fn transport_preserves_rate_limit_and_recovery_errors() {
        for (status, code) in [(axum::http::StatusCode::TOO_MANY_REQUESTS,"BUSY"),
            (axum::http::StatusCode::BAD_REQUEST,"INVALID_ARGUMENT"),
            (axum::http::StatusCode::OK,"CLI_RECOVERY_REQUIRED")] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let router = axum::Router::new().route("/v1/call",axum::routing::post(move || async move {
                (status,axum::Json(Response::failure(code,"en")))
            }));
            let host = tokio::spawn(async move { axum::serve(listener,router).await.unwrap(); });
            let response = call(&reqwest::Client::builder().no_proxy().build().unwrap(),
                &Discovery{version:1,port,token:"a".repeat(64)},&Request{command:"health".into(),..Request::default()},"en").await;
            let error = response.error.unwrap();
            assert_eq!(error.code,code);
            if code == "BUSY" { assert!(error.retryable); assert_eq!(error.retry_after_ms,Some(1000)); }
            host.abort();
        }
    }

    #[tokio::test]
    async fn stale_token_retries_once_only_if_discovery_changes_with_same_request() {
        use std::sync::{Arc, Mutex};
        for rotate in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("connection.json");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            std::fs::write(&path,serde_json::to_vec(&Discovery{version:1,port,token:"a".repeat(64)}).unwrap()).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let update_path = path.clone();
            let router = axum::Router::new().route("/v1/call",axum::routing::post(move |headers: axum::http::HeaderMap,axum::Json(request):axum::Json<Request>| {
                let seen = seen.clone(); let update_path = update_path.clone();
                async move {
                    seen.lock().unwrap().push(request.request_id);
                    if headers.get("authorization").unwrap() == format!("Bearer {}","b".repeat(64)).as_str() {
                        (axum::http::StatusCode::OK,axum::Json(Response::failure("HOST_DISABLED","en")))
                    } else {
                        if rotate { std::fs::write(update_path,serde_json::to_vec(&Discovery{version:1,port,token:"b".repeat(64)}).unwrap()).unwrap(); }
                        (axum::http::StatusCode::UNAUTHORIZED,axum::Json(Response::failure("UNAUTHORIZED","en")))
                    }
                }
            }));
            let host = tokio::spawn(async move { axum::serve(listener,router).await.unwrap(); });
            let response = LocalClient::new(Some(path),"en").unwrap()
                .call(&Request {command:"add".into(),request_id:Some("same-intent".into()),..Request::default()}).await;
            assert_eq!(response.error.unwrap().code,if rotate {"HOST_DISABLED"} else {"UNAUTHORIZED"});
            assert_eq!(*calls.lock().unwrap(),vec![Some("same-intent".into());if rotate {2} else {1}]);
            host.abort();
        }
    }
}
