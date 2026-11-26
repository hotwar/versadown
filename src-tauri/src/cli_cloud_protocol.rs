//! Cloud DTOs shared by the local host, CLI and MCP; no runtime dependencies.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use crate::cli_protocol::ApiError;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudRequest {
    Accounts { provider: Option<String> },
    AuthBegin { provider: String, account_ref: Option<String> },
    AuthStatus { authorization_id: String },
    AuthCancel { authorization_id: String },
    Prepare { input: CloudPrepareInput },
    Confirm { preparation_id: String, request_id: String },
    Status { operation_id: String },
    Cancel { operation_id: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudPrepareInput {
    Upload {
        /// Opaque folder selection returned by resource preparation.
        destination_id: String,
        #[schemars(length(min = 1, max = 20))]
        paths: Vec<String>,
    },
    CreateShare {
        /// Opaque selections of existing drive files/folders, never local paths.
        #[schemars(length(min = 1, max = 100))]
        selection_ids: Vec<String>,
        name: String,
        /// Zero explicitly requests no expiry; providers may reject it.
        #[schemars(range(min = 0, max = 365))]
        expires_in_days: u16,
        pass_code: Option<String>,
        generate_pass_code: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudAccountDto {
    pub provider: String,
    pub account_ref: Option<String>,
    pub display_name: Option<String>,
    pub authorization_state: String,
    pub capabilities: Vec<String>,
    pub permitted_operations: Vec<String>,
    pub error: Option<ApiError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudAccountsDto { pub accounts: Vec<CloudAccountDto> }

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudAuthorizationDto {
    pub authorization_id: String,
    pub provider: String,
    pub status: String,
    pub account_ref: Option<String>,
    pub expires_at: i64,
    pub next_action: Option<String>,
    pub poll_interval_ms: u64,
    pub error: Option<ApiError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudOperationItemDto {
    pub name: String,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudPreparedDto {
    pub preparation_id: String,
    pub operation: String,
    pub provider: String,
    pub account_ref: String,
    pub expires_at: i64,
    pub items: Vec<CloudOperationItemDto>,
    pub effects: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudFileResultDto {
    pub selection_id: String,
    pub name: String,
    pub size_bytes: Option<u64>,
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudShareResultDto {
    pub url: String,
    pub pass_code: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudItemErrorDto {
    pub index: usize,
    pub name: String,
    pub error: ApiError,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloudOperationDto {
    pub id: String,
    pub request_id: String,
    pub operation: String,
    pub provider: String,
    pub account_ref: String,
    pub status: String,
    pub completed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub supported_actions: Vec<String>,
    pub files: Vec<CloudFileResultDto>,
    pub share: Option<CloudShareResultDto>,
    pub failures: Vec<CloudItemErrorDto>,
    pub error: Option<ApiError>,
}

// Tool-specific inputs deliberately do not accept an action discriminator.
// The selected CLI subcommand / MCP tool owns the operation being requested.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudAccountsInput {
    #[schemars(length(min = 1, max = 32))]
    pub provider: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudAuthBeginInput {
    #[schemars(length(min = 1, max = 32))]
    pub provider: String,
    #[schemars(length(min = 1, max = 256))]
    pub account_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudAuthorizationInput {
    #[schemars(length(min = 1, max = 128))]
    pub authorization_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudPrepareParams { pub input: CloudPrepareInput }

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudConfirmInput {
    #[schemars(length(min = 1, max = 128))]
    pub preparation_id: String,
    #[schemars(length(min = 1, max = 128), regex(pattern = "^[A-Za-z0-9_.:-]+$"))]
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudOperationInput {
    #[schemars(length(min = 1, max = 128))]
    pub operation_id: String,
}

/// Shared adapter input limits. Actual provider support, account ownership and
/// permissions are always checked again by the host.
pub fn valid_request(request: &CloudRequest) -> bool {
    fn bounded(value: &str, max: usize) -> bool { !value.trim().is_empty() && value.len() <= max }
    fn unique(values: &[String]) -> bool { values.iter().collect::<std::collections::HashSet<_>>().len() == values.len() }
    match request {
        CloudRequest::Accounts { provider } => provider.as_deref().is_none_or(|value| bounded(value, 32)),
        CloudRequest::AuthBegin { provider, account_ref } => bounded(provider, 32)
            && account_ref.as_deref().is_none_or(|value| bounded(value, 256)),
        CloudRequest::AuthStatus { authorization_id } | CloudRequest::AuthCancel { authorization_id } => bounded(authorization_id, 128),
        CloudRequest::Status { operation_id } | CloudRequest::Cancel { operation_id } => bounded(operation_id, 128),
        CloudRequest::Confirm { preparation_id, request_id } => bounded(preparation_id, 128) && bounded(request_id, 128)
            && request_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte)),
        CloudRequest::Prepare { input: CloudPrepareInput::Upload { destination_id, paths } } => bounded(destination_id, 256)
            && (1..=20).contains(&paths.len()) && unique(paths) && paths.iter().all(|path| bounded(path, 4096)),
        CloudRequest::Prepare { input: CloudPrepareInput::CreateShare { selection_ids, name, expires_in_days, pass_code, generate_pass_code } } =>
            (1..=100).contains(&selection_ids.len()) && unique(selection_ids) && selection_ids.iter().all(|id| bounded(id, 256))
            && bounded(name, 256) && *expires_in_days <= 365
            && pass_code.as_deref().is_none_or(|code| bounded(code, 128)) && !(pass_code.is_some() && *generate_pass_code),
    }
}
