use crate::models::TaskStatus;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone)]
pub struct RpcSecret(Option<String>);

impl RpcSecret {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Some(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }
    pub fn from_token(token: impl AsRef<str>) -> Result<Self, Aria2Error> {
        let token = token.as_ref();
        if !token.is_empty()
            && token
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(Aria2Error::InvalidField {
                field: "rpcSecret",
                value: "[REDACTED]".into(),
            });
        }
        Ok(Self((!token.is_empty()).then(|| token.into())))
    }
    pub fn authorization_param(&self) -> Option<String> {
        self.0.as_ref().map(|token| format!("token:{token}"))
    }
    pub(super) fn token(&self) -> Option<&str> {
        self.0.as_deref()
    }
    pub(super) fn redact(&self, text: &str) -> String {
        self.token()
            .map(|token| text.replace(token, "[REDACTED]"))
            .unwrap_or_else(|| text.into())
    }
}
impl fmt::Debug for RpcSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_some() {
            f.write_str("RpcSecret([REDACTED])")
        } else {
            f.write_str("RpcSecret(None)")
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RpcFault {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Aria2Error {
    #[error("Invalid aria2 field {field}")]
    InvalidField { field: &'static str, value: String },
    #[error("Unknown aria2 status")]
    UnknownStatus(String),
    #[error("RPC endpoint must be an HTTP loopback address without credentials or query")]
    InvalidEndpoint,
    #[error("aria2 RPC transport failed")]
    Transport,
    #[error("aria2 RPC timed out")]
    Timeout,
    #[error("aria2 RPC HTTP status {0}")]
    HttpStatus(u16),
    #[error("aria2 RPC response id did not match")]
    MismatchedResponseId,
    #[error("Malformed aria2 RPC response")]
    MalformedResponse,
    #[error("aria2 RPC error {0:?}")]
    Rpc(RpcFault),
    #[error("aria2 process I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("aria2 RPC port {0} is already in use")]
    RpcPortInUse(u16),
    #[error("aria2 RPC readiness timed out")]
    ReadinessTimeout,
    #[error("aria2 process exited before becoming ready")]
    ProcessExited,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Aria2File {
    pub index: String,
    pub path: String,
    pub length: String,
    pub completed_length: String,
    pub selected: String,
    #[serde(default)]
    pub uris: Vec<Aria2Uri>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Aria2Uri {
    pub uri: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Aria2Peer {
    pub ip: String,
    pub port: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Aria2Status {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Aria2MediaProgress>,
    pub gid: String,
    pub status: String,
    pub total_length: String,
    pub completed_length: String,
    pub download_speed: String,
    pub upload_speed: String,
    pub connections: String,
    #[serde(default)]
    pub files: Vec<Aria2File>,
    #[serde(default)]
    pub bittorrent: Option<Aria2BitTorrent>,
    #[serde(default)]
    pub followed_by: Vec<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
}

/// aria2-next reports progress against presentation duration while the remuxed
/// output size is still unknown. Retained network bytes are not a final size.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Aria2MediaProgress {
    #[serde(default)]
    pub progress: Option<String>,
    #[serde(default)]
    pub downloaded_length: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Aria2BitTorrent {
    #[serde(default)]
    pub info: Option<Aria2TorrentInfo>,
    /// Aria2 Next keeps a magnet on its original GID while awaiting a
    /// file-selection decision. Legacy aria2 instead exposes a followed child
    /// GID, so this field is optional for compatibility with both engines.
    #[serde(default, rename = "fileSelectionState")]
    pub file_selection_state: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Aria2TorrentInfo {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferFile {
    pub index: u32,
    pub path: String,
    pub length: u64,
    pub completed_length: u64,
    pub selected: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransferSnapshot {
    pub gid: String,
    pub status: TaskStatus,
    pub total_length: Option<u64>,
    pub completed_length: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    pub connections: u32,
    pub progress_percent: Option<u8>,
    pub eta_seconds: Option<u64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub followed_by: Vec<String>,
    pub files: Vec<TransferFile>,
}

pub fn parse_u64(field: &'static str, value: &str) -> Result<u64, Aria2Error> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Aria2Error::InvalidField {
            field,
            value: value.into(),
        });
    }
    value.parse().map_err(|_| Aria2Error::InvalidField {
        field,
        value: value.into(),
    })
}

fn parse_u32(field: &'static str, value: &str) -> Result<u32, Aria2Error> {
    u32::try_from(parse_u64(field, value)?).map_err(|_| Aria2Error::InvalidField {
        field,
        value: value.into(),
    })
}

pub fn map_aria2_status(value: &str) -> Result<TaskStatus, Aria2Error> {
    Ok(match value {
        "waiting" => TaskStatus::Queued,
        "active" => TaskStatus::Downloading,
        "paused" => TaskStatus::Paused,
        "complete" => TaskStatus::Processing,
        "error" => TaskStatus::Failed,
        "removed" => TaskStatus::Cancelled,
        other => return Err(Aria2Error::UnknownStatus(other.into())),
    })
}

impl TryFrom<Aria2File> for TransferFile {
    type Error = Aria2Error;
    fn try_from(file: Aria2File) -> Result<Self, Self::Error> {
        let selected = match file.selected.as_str() {
            "true" => true,
            "false" => false,
            _ => {
                return Err(Aria2Error::InvalidField {
                    field: "selected",
                    value: file.selected,
                })
            }
        };
        let index = parse_u32("index", &file.index)?;
        if index == 0 {
            return Err(Aria2Error::InvalidField {
                field: "index",
                value: file.index,
            });
        }
        Ok(Self {
            index,
            path: file.path,
            length: parse_u64("length", &file.length)?,
            completed_length: parse_u64("completedLength", &file.completed_length)?,
            selected,
        })
    }
}

impl TryFrom<Aria2Status> for TransferSnapshot {
    type Error = Aria2Error;
    fn try_from(value: Aria2Status) -> Result<Self, Self::Error> {
        let total = parse_u64("totalLength", &value.total_length)?;
        let total_length = (total > 0).then_some(total);
        let completed_length = parse_u64("completedLength", &value.completed_length)?;
        let download_speed = parse_u64("downloadSpeed", &value.download_speed)?;
        let media_progress = value.media.as_ref().and_then(|media| {
            if value.status == "complete" {
                return Some(100);
            }
            let fraction = media.progress.as_deref()?.parse::<f64>().ok()?;
            fraction.is_finite().then(|| (fraction * 100.0).clamp(0.0, 99.0) as u8)
        });
        let progress_percent = media_progress.or_else(|| total_length
            .map(|total| ((u128::from(completed_length) * 100 / u128::from(total)).min(100)) as u8));
        let completed_length = if total_length.is_none() {
            value.media.as_ref().and_then(|m| m.downloaded_length.as_deref()?.parse().ok())
                .unwrap_or(completed_length)
        } else {
            completed_length
        };
        let eta_seconds = total_length
            .filter(|_| download_speed > 0)
            .map(|total| total.saturating_sub(completed_length) / download_speed);
        if let Some(code) = &value.error_code {
            parse_u64("errorCode", code)?;
        }
        let error_message =
            transfer_error_message(value.error_code.as_deref(), value.error_message);
        Ok(Self {
            gid: value.gid,
            status: map_aria2_status(&value.status)?,
            total_length,
            completed_length,
            download_speed,
            upload_speed: parse_u64("uploadSpeed", &value.upload_speed)?,
            connections: parse_u32("connections", &value.connections)?,
            progress_percent,
            eta_seconds,
            error_code: value.error_code,
            error_message,
            followed_by: value.followed_by,
            files: value
                .files
                .into_iter()
                .map(TransferFile::try_from)
                .collect::<Result<_, _>>()?,
        })
    }
}

pub(crate) const TRUNCATED_RESPONSE_MESSAGE: &str =
    "下载服务器提前中断传输，文件尚未接收完整；请重试";

pub(crate) fn cloud_share_transfer_error_message(message: String) -> String {
    if message == TRUNCATED_RESPONSE_MESSAGE {
        message
    } else {
        "网盘分享下载失败，请重试或重新解析分享链接".into()
    }
}

fn transfer_error_message(code: Option<&str>, message: Option<String>) -> Option<String> {
    // Aria2 Next reports a prematurely closed response as error 6. Require
    // its explicit diagnostic as well: other connection errors must retain
    // their meaning, and curl's error numbers are not aria2 error numbers.
    let truncated = code == Some("6")
        && message.as_deref().is_some_and(|message| {
            message.split("end of response with ").skip(1).any(|tail| {
                tail.split_once(" bytes missing")
                    .is_some_and(|(count, rest)| {
                        !count.is_empty()
                            && count.bytes().all(|byte| byte.is_ascii_digit())
                            && rest
                                .chars()
                                .next()
                                .is_none_or(|next| !next.is_alphanumeric())
                    })
            })
        });
    if truncated {
        // Never copy URLs, signatures, or headers attached to the diagnostic.
        Some(TRUNCATED_RESPONSE_MESSAGE.into())
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_transfer_preserves_safe_interruption_without_exposing_raw_errors() {
        assert_eq!(
            cloud_share_transfer_error_message(TRUNCATED_RESPONSE_MESSAGE.into()),
            TRUNCATED_RESPONSE_MESSAGE
        );
        assert_eq!(
            cloud_share_transfer_error_message("https://signed.example/file?token=secret".into()),
            "网盘分享下载失败，请重试或重新解析分享链接"
        );
    }

    #[test]
    fn media_progress_is_available_before_output_length_is_known() {
        let mut status: Aria2Status = serde_json::from_value(serde_json::json!({
            "gid":"1234","status":"active","totalLength":"0","completedLength":"0",
            "downloadSpeed":"64","uploadSpeed":"0","connections":"1",
            "media":{"progress":"0.375","downloadedLength":"128"}
        })).unwrap();
        let snapshot = TransferSnapshot::try_from(status.clone()).unwrap();
        assert_eq!(snapshot.progress_percent, Some(37));
        assert_eq!(snapshot.total_length, None);
        assert_eq!(snapshot.completed_length, 128);
        status.status = "complete".into();
        assert_eq!(TransferSnapshot::try_from(status).unwrap().progress_percent, Some(100));
    }

    fn error_snapshot(code: Option<&str>, message: Option<&str>) -> TransferSnapshot {
        TransferSnapshot::try_from(Aria2Status {
            gid: "fixture-gid".into(),
            status: "error".into(),
            total_length: "4194304".into(),
            completed_length: "1024".into(),
            download_speed: "0".into(),
            upload_speed: "0".into(),
            connections: "0".into(),
            files: vec![],
            bittorrent: None,
            media: None,
            followed_by: vec![],
            error_code: code.map(str::to_owned),
            error_message: message.map(str::to_owned),
        })
        .unwrap()
    }

    #[test]
    fn truncated_response_error_is_safe_and_preserves_engine_code_and_progress() {
        for message in [
            "end of response with 4194304 bytes missing",
            "GET https://cdn.example/file?signature=secret: end of response with 4194304 bytes missing; Authorization: Bearer secret",
        ] {
            let snapshot = error_snapshot(Some("6"), Some(message));
            assert_eq!(snapshot.error_code.as_deref(), Some("6"));
            assert_eq!(
                snapshot.error_message.as_deref(),
                Some("下载服务器提前中断传输，文件尚未接收完整；请重试")
            );
            assert_eq!(snapshot.status, TaskStatus::Failed);
            assert_eq!(snapshot.completed_length, 1024);
            assert_eq!(snapshot.total_length, Some(4194304));
        }
    }

    #[test]
    fn unrelated_transfer_errors_are_not_reclassified_as_truncated_responses() {
        for (code, message) in [
            (
                Some("18"),
                Some("end of response with 4194304 bytes missing"),
            ),
            (
                Some("8"),
                Some("end of response with 4194304 bytes missing"),
            ),
            (None, Some("end of response with 4194304 bytes missing")),
            (Some("6"), Some("connection refused")),
            (
                Some("6"),
                Some("end of response with unknown bytes missing"),
            ),
            (Some("6"), Some("end of response with  bytes missing")),
            (Some("6"), Some("end of response with -1 bytes missing")),
            (
                Some("6"),
                Some("end of response with 12 bytes missingunexpected"),
            ),
            (Some("6"), None),
        ] {
            let snapshot = error_snapshot(code, message);
            assert_eq!(snapshot.error_code.as_deref(), code);
            assert_eq!(snapshot.error_message.as_deref(), message);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "WireGlobalStat")]
pub struct Aria2GlobalStat {
    pub download_speed: u64,
    pub upload_speed: u64,
    pub num_active: u64,
    pub num_waiting: u64,
    pub num_stopped: u64,
    pub num_stopped_total: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireGlobalStat {
    download_speed: String,
    upload_speed: String,
    num_active: String,
    num_waiting: String,
    num_stopped: String,
    num_stopped_total: String,
}
impl TryFrom<WireGlobalStat> for Aria2GlobalStat {
    type Error = Aria2Error;
    fn try_from(value: WireGlobalStat) -> Result<Self, Self::Error> {
        Ok(Self {
            download_speed: parse_u64("downloadSpeed", &value.download_speed)?,
            upload_speed: parse_u64("uploadSpeed", &value.upload_speed)?,
            num_active: parse_u64("numActive", &value.num_active)?,
            num_waiting: parse_u64("numWaiting", &value.num_waiting)?,
            num_stopped: parse_u64("numStopped", &value.num_stopped)?,
            num_stopped_total: parse_u64("numStoppedTotal", &value.num_stopped_total)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aria2Version {
    pub version: String,
    pub enabled_features: Vec<String>,
}
