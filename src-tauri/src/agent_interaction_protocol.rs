//! Runtime-independent waiting/continuation contract. Approval is VD-only IPC.
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use crate::cli_protocol::Response;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionRequest {
    Status { interaction_id: String },
    Cancel { interaction_id: String },
    Resume { interaction_id: String },
}
impl InteractionRequest {
    pub fn id(&self) -> &str { match self { Self::Status { interaction_id } | Self::Cancel { interaction_id } | Self::Resume { interaction_id } => interaction_id } }
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractionInput {
    #[schemars(length(min = 36, max = 36), regex(pattern = "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"))]
    pub interaction_id: String
}

pub fn valid_interaction_id(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil())
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct InteractionDto {
    pub id: String,
    pub status: String,
    pub kind: String,
    pub provider: Option<String>,
    pub expires_at: i64,
    pub poll_interval_ms: u64,
    /// Original operation projection, used to sanitize the nested result.
    pub command: String,
    pub cloud_action: Option<String>,
    pub result: Option<Response>,
}
