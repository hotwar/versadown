//! Shared local transport for CLI and MCP. Never opens the task database.
use crate::cli_protocol::{self, ApiError, BatchAddDto, Discovery, HealthDto, PreparedResourceDto, ResourceConfirmDto, Request, Response, TaskDto, TaskListDto, TaskResultDto, PROTOCOL_VERSION};
use crate::cli_cloud_protocol::{CloudRequest, CloudAccountsDto, CloudAuthorizationDto, CloudPreparedDto, CloudOperationDto};
use crate::agent_interaction_protocol::{InteractionDto, valid_interaction_id};
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
        safe.error = Some(sanitize_error(error, lang));
        safe.data = result.data.take();
        result = safe;
    }
    if let Some(data) = result.data.take() {
        let projected = match request.command.as_str() {
            "health" => project::<HealthDto>(data),
            "list" => project_tasks(data, lang),
            "result" => project::<TaskResultDto>(data),
            "batch_add" => project_batch(data, lang),
            "prepare" => project::<PreparedResourceDto>(data),
            "confirm" => project::<ResourceConfirmDto>(data),
            "cloud" => project_cloud(data, request.cloud.as_ref().ok_or(()), lang),
            "interaction" => project_interaction(data, request.interaction.as_ref().map(|r|r.id()).ok_or(()), lang),
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
    matches!(code, "INVALID_ARGUMENT" | "HOST_UNAVAILABLE" | "UNAUTHORIZED" | "NOT_FOUND" | "REQUEST_CONFLICT" | "DUPLICATE" | "INTERNAL_ERROR" | "TASK_FAILED" | "TASK_CANCELLED" | "TIMEOUT" | "INTERRUPTED" | "PROTOCOL_ERROR" | "UNSUPPORTED" | "ENGINE_UNAVAILABLE" | "ACTION_FAILED" | "FORBIDDEN" | "HOST_DISABLED" | "TASK_NOT_READY" | "BUSY" | "AUTH_REQUIRED" | "CLI_RECOVERY_REQUIRED" | "PREPARATION_EXPIRED" | "CLOUD_WRITE_REQUIRED" | "METADATA_UNAVAILABLE" | "RESOURCE_LIMIT" | "ACCESS_CODE_REQUIRED"
        | "CLOUD_UPLOAD_AUTHORIZATION_REQUIRED" | "CLOUD_UPLOAD_PREPARATION_REJECTED" | "CLOUD_UPLOAD_SESSION_REQUIRED" | "CLOUD_UPLOAD_VERIFICATION_REQUIRED" | "CLOUD_OPERATION_VERIFICATION_REQUIRED"
        | "CLOUD_AUTHORIZATION_PENDING" | "INTERACTION_EXPIRED" | "CLOUD_PERMISSION_REQUIRED" | "ACCOUNT_MISMATCH" | "CLOUD_RESULT_UNCERTAIN" | "FILE_CONFLICT" | "SOURCE_CHANGED" | "USER_ACTION_REQUIRED")
}

fn known_provider(value: &str) -> bool { matches!(value, "quark" | "pikpak" | "baidu" | "aliyun" | "xunlei" | "uc") }

fn cloud_text(value: &str, max: usize) -> bool { !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control) }

fn sanitize_error(error: &ApiError, lang: &str) -> ApiError {
    let mut safe = ApiError::new(if known_error(&error.code) { &error.code } else { "ACTION_FAILED" }, lang);
    safe.interaction_id = error.interaction_id.as_ref().filter(|value| valid_interaction_id(value)).cloned();
    safe.retry_after_ms = error.retry_after_ms.map(|ms| ms.min(86_400_000));
    safe.provider = error.provider.as_ref().filter(|value| known_provider(value)).cloned();
    safe.reason = error.reason.as_ref().filter(|value| matches!(value.as_str(),
        "not_authorized" | "expired" | "verification_required" | "cancelled" | "window_closed"
        | "session_expired" | "window_unavailable" | "unavailable" | "account_changed" | "account_mismatch"
        | "authorization_pending" | "operation_verification_required")).cloned();
    safe
}

fn project_cloud(data: serde_json::Value, request: Result<&CloudRequest, ()>, lang: &str) -> Result<serde_json::Value, ()> {
    let action = match request? {
        CloudRequest::Accounts { .. } => "accounts",
        CloudRequest::AuthBegin { .. } => "auth_begin",
        CloudRequest::AuthStatus { .. } => "auth_status",
        CloudRequest::AuthCancel { .. } => "auth_cancel",
        CloudRequest::Prepare { .. } => "prepare",
        CloudRequest::Confirm { .. } => "confirm",
        CloudRequest::Status { .. } => "status",
        CloudRequest::Cancel { .. } => "cancel",
    };
    project_cloud_action(data, action, lang)
}

fn project_cloud_action(data: serde_json::Value, action: &str, lang: &str) -> Result<serde_json::Value, ()> {
    match action {
        "accounts" => {
            let mut data: CloudAccountsDto = serde_json::from_value(data).map_err(|_| ())?;
            if data.accounts.len() > 6 || data.accounts.iter().any(|account| !known_provider(&account.provider)) { return Err(()); }
            for account in &mut data.accounts {
                if !matches!(account.authorization_state.as_str(), "ready" | "disconnected" | "expired" | "verification_required" | "account_mismatch" | "interactive_required" | "pending" | "unavailable") { return Err(()); }
                if account.account_ref.as_deref().is_some_and(|value| !cloud_text(value, 256))
                    || account.display_name.as_deref().is_some_and(|value| value.len() > 1024) { return Err(()); }
                account.capabilities.retain(|capability| matches!(capability.as_str(), "list" | "download" | "upload" | "create_share" | "inspect_share" | "transfer_share"));
                account.permitted_operations.retain(|operation| matches!(operation.as_str(), "browse" | "download" | "upload" | "create_share" | "transfer"));
                account.error = account.error.as_ref().map(|error| sanitize_error(error, lang));
            }
            serde_json::to_value(data).map_err(|_| ())
        }
        "auth_begin" | "auth_status" | "auth_cancel" => {
            let mut data: CloudAuthorizationDto = serde_json::from_value(data).map_err(|_| ())?;
            if !known_provider(&data.provider) || !matches!(data.status.as_str(), "pending" | "succeeded" | "account_mismatch" | "cancelled" | "closed" | "expired" | "failed") { return Err(()); }
            if !cloud_text(&data.authorization_id, 128) || data.account_ref.as_deref().is_some_and(|value| !cloud_text(value, 256)) { return Err(()); }
            data.error = data.error.as_ref().map(|error| sanitize_error(error, lang));
            data.poll_interval_ms = if data.status == "pending" { data.poll_interval_ms.clamp(1000, 30_000) } else { 0 };
            data.next_action = match data.status.as_str() {
                "pending" => Some("complete_desktop_authorization"),
                "account_mismatch" => Some("review_account"),
                "failed" => Some("inspect_desktop"),
                "closed" | "cancelled" | "expired" => Some("await_user_request"),
                _ => None,
            }.map(str::to_owned);
            if let Some(error) = &mut data.error {
                error.next_action.clone_from(&data.next_action);
            }
            serde_json::to_value(data).map_err(|_| ())
        }
        "prepare" => {
            let data: CloudPreparedDto = serde_json::from_value(data).map_err(|_| ())?;
            if !known_provider(&data.provider) || !matches!(data.operation.as_str(), "upload" | "create_share")
                || !cloud_text(&data.preparation_id, 128) || !cloud_text(&data.account_ref, 256)
                || !(1..=100).contains(&data.items.len()) || data.items.iter().any(|item| !cloud_text(&item.name, 1024))
                || data.effects.len() > 8 || data.effects.iter().any(|effect| !cloud_text(effect, 128)) { return Err(()); }
            serde_json::to_value(data).map_err(|_| ())
        }
        "confirm" | "status" | "cancel" => {
            let mut data: CloudOperationDto = serde_json::from_value(data).map_err(|_| ())?;
            if !known_provider(&data.provider) || data.failures.len() > 100 || data.files.len() > 100 { return Err(()); }
            if !cloud_text(&data.id, 128) || !cloud_text(&data.request_id, 128) || !cloud_text(&data.account_ref, 256)
                || !matches!(data.operation.as_str(), "upload" | "create_share")
                || !matches!(data.status.as_str(), "queued" | "running" | "cancelling" | "succeeded" | "partial" | "failed" | "cancelled" | "needs_review")
                || data.files.iter().any(|file| !cloud_text(&file.selection_id, 256) || !cloud_text(&file.name, 1024) || !matches!(file.kind.as_str(), "file" | "folder"))
                || data.failures.iter().any(|failure| !cloud_text(&failure.name, 1024)) { return Err(()); }
            if let Some(share) = &data.share {
                if !cloud_text(&share.url, 8192) || share.pass_code.as_ref().is_some_and(|code| code.len() > 128)
                    || share.expires_at.as_deref().is_some_and(|value| !cloud_text(value, 256)) { return Err(()); }
                let url = url::Url::parse(&share.url).map_err(|_| ())?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
                    || !url.username().is_empty() || url.password().is_some() { return Err(()); }
            }
            data.error = data.error.as_ref().map(|error| sanitize_error(error, lang));
            for failure in &mut data.failures { failure.error = sanitize_error(&failure.error, lang); }
            data.supported_actions.retain(|action| matches!(action.as_str(), "cancel" | "pause" | "resume"));
            serde_json::to_value(data).map_err(|_| ())
        }
        _ => Err(()),
    }
}

fn project_interaction(data: serde_json::Value, expected_id: Result<&str, ()>, lang: &str) -> Result<serde_json::Value, ()> {
    let mut interaction: InteractionDto = serde_json::from_value(data).map_err(|_| ())?;
    if !valid_interaction_id(&interaction.id)
        || uuid::Uuid::parse_str(&interaction.id).ok() != uuid::Uuid::parse_str(expected_id?).ok()
        || !matches!(interaction.status.as_str(), "awaiting_user" | "awaiting_login" | "resuming" | "succeeded" | "failed" | "cancelled" | "expired" | "interrupted")
        || !matches!(interaction.kind.as_str(), "local_access" | "cloud_account" | "cloud_login")
        || interaction.provider.as_deref().is_some_and(|provider| !known_provider(provider))
        || !matches!(interaction.command.as_str(), "add" | "batch_add" | "prepare" | "confirm" | "cloud")
        || (interaction.command == "cloud" && !matches!(interaction.cloud_action.as_deref(), Some("prepare" | "confirm")))
        || (interaction.command != "cloud" && interaction.cloud_action.is_some()) {
        return Err(());
    }
    let pending = matches!(interaction.status.as_str(), "awaiting_user" | "awaiting_login" | "resuming");
    interaction.poll_interval_ms = if pending { interaction.poll_interval_ms.clamp(1000, 30_000) } else { 0 };
    if pending && interaction.result.is_some() { return Err(()); }
    if let Some(mut result) = interaction.result.take() {
        if result.ok != result.error.is_none() || (result.ok && result.data.is_none())
            || (result.ok != (interaction.status == "succeeded")) { return Err(()); }
        if let Some(error) = &result.error {
            if !known_error(&error.code) || error.code == "USER_ACTION_REQUIRED" || error.interaction_id.is_some() { return Err(()); }
            result.error = Some(sanitize_error(error, lang));
        }
        if let Some(data) = result.data.take() {
            result.data = Some(match interaction.command.as_str() {
                "add" => project_task(data, lang),
                "batch_add" => project_batch(data, lang),
                "prepare" => project::<PreparedResourceDto>(data),
                "confirm" => project::<ResourceConfirmDto>(data),
                "cloud" => project_cloud_action(data, interaction.cloud_action.as_deref().ok_or(())?, lang),
                _ => Err(()),
            }?);
        }
        // Typed projections remove host-only fields first; no nested result may
        // start another authorization loop, including batch/task failure DTOs.
        if contains_interaction_reference(&serde_json::to_value(&result).map_err(|_| ())?) { return Err(()); }
        interaction.result = Some(result);
    } else if matches!(interaction.status.as_str(), "succeeded" | "failed") { return Err(()); }
    serde_json::to_value(interaction).map_err(|_| ())
}

fn contains_interaction_reference(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.get("code").and_then(|v| v.as_str()) == Some("USER_ACTION_REQUIRED")
            || object.get("interaction_id").is_some_and(|v| !v.is_null())
            || object.values().any(contains_interaction_reference),
        serde_json::Value::Array(values) => values.iter().any(contains_interaction_reference),
        _ => false,
    }
}

fn sanitize_task(mut task: TaskDto, lang: &str) -> TaskDto {
    if let Some(failure) = task.failure.take() {
        task.failure = Some(sanitize_error(&failure, lang));
    }
    // This field is a finite action identifier, never arbitrary host text.
    if task.required_action.as_deref().is_some_and(|action| !matches!(action,
        "start_desktop" | "enable_desktop_agent" | "review_directory_permissions" | "open_desktop_login"
        | "refresh_discovery" | "query_task" | "retry_same_request" | "inspect_desktop" | "resume_task" | "prepare_again" | "open_desktop_share" | "reduce_selection" | "provide_access_code"
        | "begin_cloud_authorization" | "review_cloud_permissions" | "review_account" | "inspect_cloud_result")) {
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

fn project_batch(data: serde_json::Value, lang: &str) -> Result<serde_json::Value, ()> {
    let mut batch: BatchAddDto = serde_json::from_value(data).map_err(|_| ())?;
    if batch.items.is_empty() || batch.items.len() > 20 { return Err(()); }
    let mut accepted = 0;
    for item in &mut batch.items {
        if item.ok {
            if item.error.is_some() { return Err(()); }
            item.data = Some(sanitize_task(item.data.take().ok_or(())?, lang));
            accepted += 1;
        } else {
            if item.data.is_some() { return Err(()); }
            let error = item.error.take().ok_or(())?;
            if !known_error(&error.code) { return Err(()); }
            item.error = Some(sanitize_error(&error, lang));
        }
    }
    if batch.accepted != accepted || batch.failed != batch.items.len() as u32 - accepted { return Err(()); }
    serde_json::to_value(batch).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;


    const INTERACTION_ID: &str = "11111111-1111-4111-8111-111111111111";

    fn completed_interaction() -> serde_json::Value {
        json!({"id":INTERACTION_ID,"status":"succeeded","kind":"local_access","provider":null,
            "expires_at":600,"poll_interval_ms":999999,"command":"confirm","cloud_action":null,
            "result":{"ok":true,"data":{"preparation_id":"prepared","task_ids":["task-id"],
                "token":"secret"},"cookie":"secret"},"token":"secret"})
    }

    #[test]
    fn interaction_projection_keeps_typed_original_result_and_drops_internal_fields() {
        let result = project_interaction(completed_interaction(), Ok(INTERACTION_ID), "en").unwrap();
        assert_eq!(result["result"]["data"]["task_ids"], json!(["task-id"]));
        assert_eq!(result["poll_interval_ms"], 0);
        assert!(!result.to_string().contains("secret"));
        let mut data = completed_interaction();
        data["status"] = json!("awaiting_user"); data["result"] = serde_json::Value::Null;
        assert_eq!(project_interaction(data.clone(), Ok(INTERACTION_ID), "en").unwrap()["poll_interval_ms"], 30000);
        data["poll_interval_ms"] = json!(1); data["status"] = json!("awaiting_login");
        assert_eq!(project_interaction(data.clone(), Ok(INTERACTION_ID), "en").unwrap()["poll_interval_ms"], 1000);
        data["status"] = json!("expired");
        assert_eq!(project_interaction(data, Ok(INTERACTION_ID), "en").unwrap()["poll_interval_ms"], 0);
    }

    #[test]
    fn interaction_projection_rejects_wrong_identity_action_state_and_untyped_results() {
        for (key, value) in [
            ("id", json!("22222222-2222-4222-8222-222222222222")),
            ("status", json!("approved")),
            ("status", json!("awaiting_user")),
            ("status", json!("failed")),
            ("status", json!("cancelled")),
            ("kind", json!("unrestricted")),
            ("provider", json!("secret")),
            ("command", json!("interaction")),
            ("command", json!("health")),
            ("cloud_action", json!("confirm")),
            ("result", serde_json::Value::Null),
            ("result", json!({"ok":true,"data":{"secret":"opaque result"}})),
            ("result", json!({"ok":true,"data":{"preparation_id":"p","task_ids":[]},
                "error":{"code":"INTERNAL_ERROR","message":"secret"}})),
        ] {
            let mut data = completed_interaction(); data[key] = value;
            assert!(project_interaction(data, Ok(INTERACTION_ID), "en").is_err(), "{key}");
        }
        assert!(project_interaction(completed_interaction(), Err(()), "en").is_err());
    }

    #[test]
    fn interaction_projection_cleans_failure_and_rejects_recursive_approval() {
        let mut data = completed_interaction();
        data["status"] = json!("failed");
        data["result"] = json!({"ok":false,"error":{"code":"AUTH_REQUIRED","message":"secret",
            "provider":"quark","reason":"expired","next_action":"secret"}});
        let result = project_interaction(data.clone(), Ok(INTERACTION_ID), "en").unwrap();
        assert_eq!(result["result"]["error"]["code"], "AUTH_REQUIRED");
        assert_eq!(result["result"]["error"]["provider"], "quark");
        assert!(!result.to_string().contains("secret"));
        for code in ["USER_ACTION_REQUIRED", "UNRECOGNIZED"] {
            let mut invalid = data.clone(); invalid["result"]["error"]["code"] = json!(code);
            assert!(project_interaction(invalid, Ok(INTERACTION_ID), "en").is_err());
        }
        data["result"]["error"]["interaction_id"] = json!(INTERACTION_ID);
        assert!(project_interaction(data, Ok(INTERACTION_ID), "en").is_err());

        let mut batch = completed_interaction();
        batch["command"] = json!("batch_add");
        batch["result"] = json!({"ok":true,"data":{"items":[{"request_id":"request","ok":false,
            "error":{"code":"USER_ACTION_REQUIRED","message":"waiting","interaction_id":INTERACTION_ID}}],
            "accepted":0,"failed":1}});
        assert!(project_interaction(batch, Ok(INTERACTION_ID), "en").is_err());
    }

    #[test]
    fn interaction_projection_uses_cloud_action_to_sanitize_original_result() {
        let mut data = completed_interaction();
        data["command"] = json!("cloud"); data["cloud_action"] = json!("confirm");
        data["kind"] = json!("cloud_account"); data["provider"] = json!("quark");
        data["result"] = json!({"ok":true,"data":{
            "id":"operation","request_id":"stable","operation":"create_share","provider":"quark",
            "account_ref":"opaque","status":"succeeded","completed_bytes":0,"total_bytes":null,
            "supported_actions":["secret"],"files":[],"failures":[],"error":null,
            "share":{"url":"https://example.test/share/demo","pass_code":"demo","token":"secret"},
            "cookie":"secret"}});
        let result = project_interaction(data.clone(), Ok(INTERACTION_ID), "en").unwrap();
        assert_eq!(result["result"]["data"]["share"]["pass_code"], "demo");
        assert!(!result.to_string().contains("secret"));
        for action in ["prepare", "status", "auth_begin", "unknown"] {
            data["cloud_action"] = json!(action);
            assert!(project_interaction(data.clone(), Ok(INTERACTION_ID), "en").is_err(), "{action}");
        }
    }

    #[test]
    fn user_action_error_preserves_only_valid_interaction_identifiers() {
        let mut error = ApiError::new("USER_ACTION_REQUIRED", "en");
        error.message = "secret".into(); error.next_action = Some("approve-yourself".into());
        error.interaction_id = Some(INTERACTION_ID.into());
        let safe = sanitize_error(&error, "en");
        assert_eq!(safe.interaction_id.as_deref(), Some(INTERACTION_ID));
        assert_eq!(safe.next_action.as_deref(), Some("complete_vd_interaction_then_query_status"));
        assert!(!safe.retryable);
        assert!(!serde_json::to_string(&safe).unwrap().contains("secret"));
        for id in ["secret", "00000000-0000-0000-0000-000000000000", "11111111111141118111111111111111"] {
            error.interaction_id = Some(id.into());
            assert!(sanitize_error(&error, "en").interaction_id.is_none(), "{id}");
        }
    }

    #[test]
    fn cloud_projection_keeps_safe_authorization_guidance_and_strips_credentials() {
        let request = CloudRequest::AuthStatus { authorization_id: "auth-id".into() };
        let result = project_cloud(serde_json::json!({
            "authorization_id":"auth-id","provider":"quark","status":"failed","account_ref":null,
            "expires_at":300,"next_action":"inspect_desktop","poll_interval_ms":0,"cookie":"secret",
            "error":{"code":"AUTH_REQUIRED","message":"secret","retryable":true,"next_action":"secret",
                "provider":"quark","reason":"verification_required","token":"secret"}
        }), Ok(&request), "en").unwrap();
        assert_eq!(result["error"]["provider"], "quark");
        assert_eq!(result["error"]["reason"], "verification_required");
        assert_eq!(result["error"]["next_action"], "inspect_desktop");
        assert_eq!(result["poll_interval_ms"], 0);
        assert!(!result.to_string().contains("secret"));
        let malicious: ApiError = serde_json::from_value(serde_json::json!({"code":"ACCOUNT_MISMATCH",
            "message":"secret","provider":"secret","reason":"secret","next_action":"secret"})).unwrap();
        let error = sanitize_error(&malicious, "en");
        assert_eq!(error.code, "ACCOUNT_MISMATCH");
        assert!(error.provider.is_none() && error.reason.is_none());
    }

    #[test]
    fn cloud_projection_preserves_baidu_challenges_and_current_authorization() {
        for (code, action) in [
            ("CLOUD_UPLOAD_AUTHORIZATION_REQUIRED", "renew_upload_authorization_in_vd"),
            ("CLOUD_UPLOAD_PREPARATION_REJECTED", "inspect_desktop"),
            ("CLOUD_UPLOAD_SESSION_REQUIRED", "verify_cloud_upload_in_browser"),
            ("CLOUD_UPLOAD_VERIFICATION_REQUIRED", "verify_cloud_upload_in_browser"),
            ("CLOUD_OPERATION_VERIFICATION_REQUIRED", "complete_cloud_operation_verification"),
            ("CLOUD_AUTHORIZATION_PENDING", "wait_for_current_cloud_authorization"),
            ("INTERACTION_EXPIRED", "await_user_request"),
        ] {
            let error = sanitize_error(&ApiError::new(code, "zh-CN"), "zh-CN");
            assert_eq!(error.code, code);
            assert_eq!(error.next_action.as_deref(), Some(action));
        }
        let result = project_cloud_action(json!({"accounts":[{
            "provider":"baidu","authorization_state":"pending","capabilities":[],"permitted_operations":[],
            "error":{"code":"CLOUD_AUTHORIZATION_PENDING","message":"secret","reason":"authorization_pending"}
        }]}), "accounts", "en").unwrap();
        assert_eq!(result["accounts"][0]["error"]["reason"], "authorization_pending");
        assert!(!result.to_string().contains("secret"));
    }

    #[test]
    fn cloud_operation_projection_keeps_business_results_but_cleans_nested_errors() {
        let request = CloudRequest::Status { operation_id: "operation-id".into() };
        let result = project_cloud(serde_json::json!({
            "id":"operation-id","request_id":"request-id","operation":"create_share","provider":"quark",
            "account_ref":"opaque","status":"succeeded","completed_bytes":0,"total_bytes":null,
            "supported_actions":["cancel","secret"],"files":[],"failures":[],"error":null,
            "share":{"url":"https://example.test/share/demo","pass_code":"demo","expires_at":"2026-11-24T12:00:00Z","token":"secret"},
            "credentials":"secret"
        }), Ok(&request), "en").unwrap();
        assert_eq!(result["share"]["pass_code"], "demo", "explicit share result is business data");
        assert_eq!(result["share"]["expires_at"], "2026-11-24T12:00:00Z", "preserve the provider's expiry representation");
        assert_eq!(result["supported_actions"], serde_json::json!(["cancel"]));
        assert!(!result.to_string().contains("secret"));
    }
    use serde_json::json;

    #[test]
    fn batch_projection_rejects_inconsistent_totals_and_redacts_nested_errors() {
        let mut data = json!({"items":[{"request_id":"stable-1","ok":false,
            "error":{"code":"FORBIDDEN","message":"secret cookie","next_action":"secret action"},"token":"secret"}],
            "accepted":0,"failed":1});
        let safe = project_batch(data.clone(), "zh-CN").unwrap();
        assert!(!safe.to_string().contains("secret"));
        assert_eq!(safe["items"][0]["error"]["next_action"], "review_directory_permissions");
        data["accepted"] = json!(1);
        assert!(project_batch(data.clone(), "en").is_err());
        data["accepted"] = json!(0);
        data["items"][0]["ok"] = json!(true);
        assert!(project_batch(data, "en").is_err());
    }

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
