//! Versioned local CLI contract. This module must not depend on the GUI/runtime.
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Discovery {
    pub version: u32,
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ApiError {
    pub fn new(code: &str, lang: &str) -> Self {
        let (retryable, next_action) = match code {
            "HOST_UNAVAILABLE" => (false, Some("start_desktop")),
            "HOST_DISABLED" => (false, Some("enable_desktop_agent")),
            "FORBIDDEN" => (false, Some("review_directory_permissions")),
            "AUTH_REQUIRED" => (false, Some("open_desktop_login")),
            "UNAUTHORIZED" => (false, Some("refresh_discovery")),
            "TASK_NOT_READY" => (true, Some("query_task")),
            "BUSY" => (true, Some("retry_same_request")),
            "TIMEOUT" => (true, Some("retry_same_request")),
            "TASK_FAILED" | "ENGINE_UNAVAILABLE" | "ACTION_FAILED" | "CLI_RECOVERY_REQUIRED" => (false, Some("inspect_desktop")),
            _ => (false, None),
        };
        Self { code: code.into(), message: message(code, lang).into(), retryable,
            next_action: next_action.map(str::to_owned),
            retry_after_ms: matches!(code, "BUSY" | "TASK_NOT_READY").then_some(1000) }
    }
}

/// Typed wire DTOs are shared with MCP's tool-specific output schemas. Never
/// substitute the internal queue/engine objects for these public projections.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskDto {
    pub id: String,
    pub name: Option<String>,
    pub kind: String,
    pub status: String,
    pub progress: u8,
    pub total_length: Option<u64>,
    pub completed_length: u64,
    pub download_speed: u64,
    /// Unix epoch seconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub cleanup_pending: bool,
    pub child_count: u32,
    pub failure: Option<ApiError>,
    pub required_action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskListDto {
    pub tasks: Vec<TaskDto>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactDto {
    pub path: String,
    pub size_bytes: u64,
    pub exists: bool,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskResultDto {
    pub task_id: String,
    pub status: String,
    pub files: Vec<ArtifactDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CapabilitiesDto {
    pub new_download_protocols: Vec<String>,
    pub task_types: Vec<String>,
    pub operations: Vec<String>,
    pub private_tasks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LimitsDto {
    pub list_default_limit: u32,
    pub list_max_limit: u32,
    pub poll_interval_ms: u32,
    pub max_response_bytes: u32,
    pub max_pending_tasks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HealthDto {
    pub version: String,
    pub protocol_version: u32,
    pub instance_id: String,
    pub engine_available: bool,
    pub capabilities: CapabilitiesDto,
    pub limits: LimitsDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TypedResponse<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl Response {
    pub fn success(data: Value) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }

    pub fn failure(code: &str, lang: &str) -> Self {
        Self { ok: false, data: None, error: Some(ApiError::new(code, lang)) }
    }
}

/// Translation keys double as stable error codes; arbitrary engine diagnostics
/// must never enter the public envelope because they may contain credentials.
pub fn message(key: &str, lang: &str) -> &'static str {
    let zh = lang == "zh-CN";
    match (key, zh) {
        ("INVALID_ARGUMENT", false) => "Invalid arguments. Use help for usage.",
        ("INVALID_ARGUMENT", true) => "参数无效，请使用 help 查看用法。",
        ("HOST_UNAVAILABLE", false) => "VD is unavailable. Start the VD application first.",
        ("HOST_UNAVAILABLE", true) => "VD 不可用，请先启动 VD 主程序。",
        ("UNAUTHORIZED", false) => "Local interface authentication failed.",
        ("UNAUTHORIZED", true) => "本机接口鉴权失败。",
        ("HOST_DISABLED", false) => "Agent access is disabled. Enable it in VD settings.",
        ("HOST_DISABLED", true) => "Agent 调用已关闭，请在 VD 设置中启用。",
        ("FORBIDDEN", false) => "This operation is outside the authorized download directories.",
        ("FORBIDDEN", true) => "此操作超出了已授权的下载目录范围。",
        ("AUTH_REQUIRED", false) => "Complete account authorization in the VD application.",
        ("AUTH_REQUIRED", true) => "请在 VD 主程序中完成账号授权。",
        ("TASK_NOT_READY", false) => "Final files are not ready. Query the task status first.",
        ("TASK_NOT_READY", true) => "最终文件尚未就绪，请先查询任务状态。",
        ("CLI_RECOVERY_REQUIRED", false) => "Recovery requires review in VD. Existing files were preserved.",
        ("CLI_RECOVERY_REQUIRED", true) => "请在 VD 中检查恢复状态，已有文件已保留。",
        ("BUSY", false) => "VD is busy. Retry the same request after a short delay.",
        ("BUSY", true) => "VD 正忙，请稍后使用相同请求重试。",
        ("NOT_FOUND", false) => "Task not found or unavailable to CLI.",
        ("NOT_FOUND", true) => "任务不存在或无法通过 CLI 访问。",
        ("REQUEST_CONFLICT", false) => "Request ID was already used with different parameters.",
        ("REQUEST_CONFLICT", true) => "请求 ID 已被其他参数使用。",
        ("DUPLICATE", false) => "A task with the same source already exists.",
        ("DUPLICATE", true) => "相同来源的任务已存在。",
        ("TASK_FAILED", false) => "The task failed.",
        ("TASK_FAILED", true) => "任务失败。",
        ("TASK_CANCELLED", false) => "The task was cancelled.",
        ("TASK_CANCELLED", true) => "任务已取消。",
        ("TIMEOUT", false) => "The operation timed out. The task was not cancelled.",
        ("TIMEOUT", true) => "操作超时，任务未被取消。",
        ("INTERRUPTED", false) => "The operation was interrupted. The task was not cancelled.",
        ("INTERRUPTED", true) => "操作已中断，任务未被取消。",
        ("PROTOCOL_ERROR", false) => "Invalid local interface response or discovery file.",
        ("PROTOCOL_ERROR", true) => "本机接口响应或发现文件无效。",
        ("UNSUPPORTED", false) => "This operation or resource is not supported by this CLI version.",
        ("UNSUPPORTED", true) => "当前 CLI 版本不支持此操作或资源。",
        ("ENGINE_UNAVAILABLE", false) => "The download engine is unavailable.",
        ("ENGINE_UNAVAILABLE", true) => "下载引擎不可用。",
        ("ACTION_FAILED", false) => "The task action could not be completed.",
        ("ACTION_FAILED", true) => "无法完成此任务操作。",
        ("HELP", false) => "VD must be running. All commands emit one JSON result. add accepts an HTTP/HTTPS file URL, an absolute directory and a request ID. Reuse the same request ID and arguments when retrying. wait never cancels the task on timeout or Ctrl-C.",
        ("HELP", true) => "请先启动 VD。所有命令输出一条 JSON 结果。add 接受 HTTP/HTTPS 文件地址、绝对保存目录和请求 ID；重试时复用相同请求 ID 与参数。wait 超时或 Ctrl-C 不会取消任务。",
        (_, false) => "The operation failed. Inspect the VD application for details.",
        (_, true) => "操作失败，请在 VD 主程序中查看详情。",
    }
}

pub fn exit_code(response: &Response) -> u8 {
    if response.ok { return 0; }
    match response.error.as_ref().map(|e| e.code.as_str()) {
        Some("INVALID_ARGUMENT" | "UNSUPPORTED") => 2,
        Some("HOST_UNAVAILABLE") => 3,
        Some("ENGINE_UNAVAILABLE" | "ACTION_FAILED") => 1,
        Some("UNAUTHORIZED" | "FORBIDDEN" | "HOST_DISABLED" | "AUTH_REQUIRED") => 4,
        Some("NOT_FOUND") => 5,
        Some("REQUEST_CONFLICT" | "DUPLICATE") => 6,
        Some("TIMEOUT") => 7,
        Some("TASK_FAILED" | "TASK_CANCELLED") => 8,
        Some("INTERRUPTED") => 130,
        _ => 1,
    }
}

pub fn default_discovery_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let root = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")));
    root.map(|root| root.join("com.versadown.desktop/cli/connection.json"))
}
