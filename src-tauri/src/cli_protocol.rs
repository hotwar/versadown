//! Versioned local CLI contract. This module must not depend on the GUI/runtime.
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

pub fn build_commit() -> Option<String> {
    option_env!("VERSADOWN_BUILD_COMMIT").filter(|value| value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_owned)
}

pub fn build_dirty() -> Option<bool> {
    option_env!("VERSADOWN_BUILD_DIRTY").and_then(|value| match value { "true" => Some(true), "false" => Some(false), _ => None })
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction: Option<crate::agent_interaction_protocol::InteractionRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<crate::cli_cloud_protocol::CloudRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<BatchItemInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepare: Option<ResourcePrepareInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<ResourceConfirmInput>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<String>,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ApiError {
    pub fn new(code: &str, lang: &str) -> Self {
        let (retryable, next_action) = match code {
            "USER_ACTION_REQUIRED" => (false, Some("complete_vd_interaction_then_query_status")),
            "HOST_UNAVAILABLE" => (false, Some("inspect_desktop")),
            "HOST_DISABLED" => (false, Some("enable_desktop_agent")),
            "FORBIDDEN" => (false, Some("review_directory_permissions")),
            "AUTH_REQUIRED" => (false, Some("begin_cloud_authorization")),
            "CLOUD_PERMISSION_REQUIRED" => (false, Some("review_cloud_permissions")),
            "ACCOUNT_MISMATCH" => (false, Some("prepare_again")),
            "CLOUD_RESULT_UNCERTAIN" => (false, Some("inspect_desktop")),
            "FILE_CONFLICT" => (false, Some("choose_another_destination")),
            "CLOUD_WRITE_REQUIRED" => (false, Some("open_desktop_share")),
            "PREPARATION_EXPIRED" => (false, Some("prepare_again")),
            "METADATA_UNAVAILABLE" => (false, Some("prepare_again")),
            "RESOURCE_LIMIT" => (false, Some("reduce_selection")),
            "ACCESS_CODE_REQUIRED" => (false, Some("provide_access_code")),
            "UNAUTHORIZED" => (false, Some("refresh_discovery")),
            "TASK_NOT_READY" => (true, Some("query_task")),
            "BUSY" => (true, Some("retry_same_request")),
            "TIMEOUT" => (true, Some("retry_same_request")),
            "TASK_FAILED" | "ENGINE_UNAVAILABLE" | "ACTION_FAILED" | "CLI_RECOVERY_REQUIRED" => (false, Some("inspect_desktop")),
            _ => (false, None),
        };
        Self { code: code.into(), interaction_id: None, message: message(code, lang).into(), retryable,
            next_action: next_action.map(str::to_owned),
            retry_after_ms: matches!(code, "BUSY" | "TASK_NOT_READY").then_some(1000), provider:None, reason:None }
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchItemInput {
    #[schemars(length(min = 1, max = 8192))]
    pub url: String,
    #[serde(default)]
    #[schemars(length(max = 4096))]
    pub directory: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_.:-]+$"))]
    pub request_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BatchItemResult {
    pub request_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<TaskDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BatchAddDto {
    pub items: Vec<BatchItemResult>,
    pub accepted: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourcePrepareInput {
    #[schemars(length(min = 1, max = 32))]
    pub kind: String,
    #[serde(default)]
    #[schemars(length(max = 8192))]
    pub input: String,
    #[schemars(length(min = 1, max = 32))]
    pub provider: Option<String>,
    #[schemars(length(min = 1, max = 256))]
    pub parent_id: Option<String>,
    #[schemars(length(min = 1, max = 128))]
    pub access_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceConfirmInput {
    #[schemars(length(min = 1, max = 128))]
    pub preparation_id: String,
    #[schemars(length(min = 1, max = 100))]
    pub selection_ids: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 4096))]
    pub directory: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_.:-]+$"))]
    pub request_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ResourceChoiceDto {
    pub selection_id: String,
    pub name: String,
    pub kind: String,
    pub size_bytes: Option<u64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PreparedResourceDto {
    pub preparation_id: String,
    pub expires_at: i64,
    pub kind: String,
    pub resources: Vec<ResourceChoiceDto>,
    pub effects: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ResourceConfirmDto {
    pub preparation_id: String,
    pub task_ids: Vec<String>,
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
    pub prepared_resource_kinds: Option<Vec<String>>,
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
    pub build_commit: Option<String>,
    pub build_dirty: Option<bool>,
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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
        ("USER_ACTION_REQUIRED", false) => "Complete the request in VD, then query interaction status. Do not create a new request or repeatedly reopen the window.",
        ("USER_ACTION_REQUIRED", true) => "请在 VD 中完成授权或登录，然后查询 interaction 状态。请勿另建请求或反复打开授权窗口。",
        ("INVALID_ARGUMENT", false) => "Invalid arguments. Use help for usage.",
        ("INVALID_ARGUMENT", true) => "参数无效，请使用 help 查看用法。",
        ("HOST_UNAVAILABLE", false) => "Unable to connect to the local VD Agent service. Confirm VD is running, enable 'Allow local Agent access' in Settings > Advanced, and check the discovery file configuration.",
        ("HOST_UNAVAILABLE", true) => "无法连接 VD 的本机 Agent 服务。请确认 VD 已启动，在设置 → 高级中启用“允许本机 Agent 调用”，并检查发现文件配置。",
        ("UNAUTHORIZED", false) => "Local interface authentication failed.",
        ("UNAUTHORIZED", true) => "本机接口鉴权失败。",
        ("HOST_DISABLED", false) => "Agent access is disabled. Enable it in VD settings.",
        ("HOST_DISABLED", true) => "Agent 调用已关闭，请在 VD 设置中启用。",
        ("FORBIDDEN", false) => "This operation is outside the authorized download directories.",
        ("FORBIDDEN", true) => "此操作超出了已授权的下载目录范围。",
        ("AUTH_REQUIRED", false) => "Complete account authorization in the VD application.",
        ("AUTH_REQUIRED", true) => "请在 VD 主程序中完成账号授权。",
        ("CLOUD_PERMISSION_REQUIRED", false) => "Approve Agent access to this cloud account in VD.",
        ("CLOUD_PERMISSION_REQUIRED", true) => "请在 VD 中确认允许 Agent 使用此网盘账号。",
        ("ACCOUNT_MISMATCH", false) => "The cloud account changed. Select the account and prepare the operation again.",
        ("ACCOUNT_MISMATCH", true) => "网盘账号已变化，请选择账号并重新准备操作。",
        ("CLOUD_RESULT_UNCERTAIN", false) => "The cloud operation outcome needs review. Do not resubmit it as a new request.",
        ("CLOUD_RESULT_UNCERTAIN", true) => "云端操作结果需要核对，请勿作为新请求重复提交。",
        ("FILE_CONFLICT", false) => "The destination already contains an item with this name. Select another destination.",
        ("FILE_CONFLICT", true) => "目标目录已有同名项目，请选择其他目标目录。",
        ("SOURCE_CHANGED", false) => "An upload source changed after preparation. Prepare it again.",
        ("SOURCE_CHANGED", true) => "上传源文件在准备后发生变化，请重新准备。",
        ("AUTH_CANCELLED", false) => "Cloud authorization was cancelled.",
        ("AUTH_CANCELLED", true) => "网盘授权已取消。",
        ("AUTH_EXPIRED", false) => "Cloud authorization timed out. Begin a new authorization when ready.",
        ("AUTH_EXPIRED", true) => "网盘授权已超时，请在准备好后重新发起。",
        ("CLOUD_WRITE_REQUIRED", false) => "This resource requires a cloud save operation. Open VD to review and perform it; Agent download access does not grant cloud write permission.",
        ("CLOUD_WRITE_REQUIRED", true) => "此资源需要云端转存，请打开 VD 审阅并操作；Agent 下载权限不包含云端写入权限。",
        ("PREPARATION_EXPIRED", false) => "The prepared resource is missing or expired. Prepare it again and review the available selections.",
        ("PREPARATION_EXPIRED", true) => "资源准备记录不存在或已过期，请重新解析并检查可选资源。",
        ("METADATA_UNAVAILABLE", false) => "Verified torrent metadata is unavailable. Prepare an authorized local torrent file or retry after metadata becomes available.",
        ("METADATA_UNAVAILABLE", true) => "暂时无法取得可校验的种子元数据。请准备已授权目录内的种子文件，或在元数据可用后重试。",
        ("RESOURCE_LIMIT", false) => "The resource or selection exceeds this operation's limits. Use a smaller folder or selection.",
        ("RESOURCE_LIMIT", true) => "资源或选择超出本操作的限制，请使用较小的目录或选择范围。",
        ("ACCESS_CODE_REQUIRED", false) => "This share requires a valid access code. Ask the user for the code and prepare it again.",
        ("ACCESS_CODE_REQUIRED", true) => "此分享需要有效提取码，请向用户获取提取码后重新解析。",
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
        ("HELP", false) => "Enable Agent access and authorize download directories once in VD settings. Commands start the default host on demand; --no-start or explicit discovery connects only. doctor is read-only, mcp-config generates configuration without writing it. add accepts HTTP/HTTPS/FTP files. batch-add --file accepts up to 20 independent requests; inspect every item. Complex resources use prepare/confirm --file. Reuse request IDs and identical arguments when retrying. wait timeout or Ctrl-C never cancels tasks.",
        ("HELP", true) => "首次在 VD 设置中启用 Agent 调用并授权下载目录。命令按需启动默认宿主；--no-start 或显式发现文件路径只连接。doctor 只读诊断，mcp-config 生成配置但不写入。add 接受 HTTP/HTTPS/FTP 文件；batch-add --file 最多提交 20 项独立请求，需检查逐项结果。复杂资源使用 prepare/confirm --file。重试复用请求 ID 和原参数；wait 超时或 Ctrl-C 不取消任务。",
        (_, false) => "The operation failed. Inspect the VD application for details.",
        (_, true) => "操作失败，请在 VD 主程序中查看详情。",
    }
}

pub fn exit_code(response: &Response) -> u8 {
    if response.ok { return 0; }
    match response.error.as_ref().map(|e| e.code.as_str()) {
        Some("INVALID_ARGUMENT" | "UNSUPPORTED" | "RESOURCE_LIMIT") => 2,
        Some("HOST_UNAVAILABLE") => 3,
        Some("ENGINE_UNAVAILABLE" | "ACTION_FAILED") => 1,
        Some("UNAUTHORIZED" | "FORBIDDEN" | "HOST_DISABLED" | "AUTH_REQUIRED" | "CLOUD_WRITE_REQUIRED" | "ACCESS_CODE_REQUIRED" | "CLOUD_PERMISSION_REQUIRED" | "ACCOUNT_MISMATCH") => 4,
        Some("PREPARATION_EXPIRED" | "SOURCE_CHANGED") => 5,
        Some("NOT_FOUND") => 5,
        Some("REQUEST_CONFLICT" | "DUPLICATE" | "FILE_CONFLICT") => 6,
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
