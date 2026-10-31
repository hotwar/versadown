use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::{aria2::TransferFile, cloud::CloudProvider};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProxySettings {
    pub enabled: bool,
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            protocol: "http".into(),
            host: String::new(),
            port: 7890,
            username: String::new(),
            password: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Aria2Settings {
    pub rpc_port: u16,
    pub rpc_token: String,
    pub max_concurrent_downloads: u16,
    pub max_connections_per_server: u8,
    pub bt_seed_ratio: f64,
    /// Persist detailed aria2 and local RPC diagnostics for troubleshooting.
    pub debug_logging: bool,
    /// Bytes per second; zero leaves all Aria2 downloads unrestricted.
    pub overall_download_limit: u64,
    /// Bytes per second; shared by all P2P uploads managed by Aria2 Next.
    pub p2p_upload_limit: u64,
}

impl Default for Aria2Settings {
    fn default() -> Self {
        Self {
            rpc_port: 6800,
            rpc_token: String::new(),
            max_concurrent_downloads: 20,
            max_connections_per_server: 4,
            bt_seed_ratio: 1.5,
            debug_logging: false,
            overall_download_limit: 0,
            p2p_upload_limit: 1_048_576,
        }
    }
}

impl Aria2Settings {
    pub fn ensure_rpc_token(&mut self) {
        self.rpc_token = self.rpc_token.trim().to_string();
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1024..=65535).contains(&self.rpc_port) {
            return Err("aria2 RPC 端口必须在 1024 到 65535 之间".into());
        }
        if self.rpc_token.len() > 256
            || self.rpc_token.chars().any(|character| {
                !self.rpc_token.is_empty() && (character.is_control() || character.is_whitespace())
            })
        {
            return Err("aria2 RPC Token 最长 256 个字符，且不能包含空白或控制字符".into());
        }
        if !(1..=64).contains(&self.max_concurrent_downloads) {
            return Err("最大并发任务数必须在 1 到 64 之间".into());
        }
        if !(1..=16).contains(&self.max_connections_per_server) {
            return Err("单任务并发连接数必须在 1 到 16 之间".into());
        }
        if !self.bt_seed_ratio.is_finite() || !(0.1..=1000.0).contains(&self.bt_seed_ratio) {
            return Err("BT 分享率必须在 0.1 到 1000 之间".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiSettings {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_configured: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MagnetMetadataSettings {
    pub enabled: bool,
    pub base_url: String,
}

impl Default for MagnetMetadataSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "http://peer.aigutta.com:8080".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiSettingsInput {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherVisibility {
    #[default]
    Always,
    Downloading,
    Hidden,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppSettings {
    pub download_directory: String,
    pub launcher_visibility: LauncherVisibility,
    pub proxy: ProxySettings,
    pub aria2: Aria2Settings,
    pub ai: AiSettings,
    pub magnet_metadata: MagnetMetadataSettings,
    /// Bytes per second; zero leaves cloud uploads unrestricted.
    pub cloud_upload_limit: u64,
    #[serde(default = "default_delete_transferred_share_files_after_download")]
    pub delete_transferred_share_files_after_download: bool,
    #[serde(default = "default_download_status_notifications")]
    pub download_status_notifications: bool,
    /// Opens magnet, thunder and ed2k links clicked in browsers with VersaDown.
    #[serde(default = "default_associate_download_links")]
    pub associate_download_links: bool,
    #[serde(default)]
    pub privacy_download_enabled: bool,
    #[serde(default)]
    pub privacy_password_configured: bool,
}

const fn default_delete_transferred_share_files_after_download() -> bool {
    true
}

const fn default_download_status_notifications() -> bool {
    true
}

const fn default_associate_download_links() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            download_directory: String::new(),
            launcher_visibility: LauncherVisibility::default(),
            proxy: ProxySettings::default(),
            aria2: Aria2Settings::default(),
            ai: AiSettings::default(),
            magnet_metadata: MagnetMetadataSettings::default(),
            cloud_upload_limit: 0,
            delete_transferred_share_files_after_download: true,
            download_status_notifications: true,
            associate_download_links: true,
            privacy_download_enabled: false,
            privacy_password_configured: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Draft,
    Parsing,
    AwaitingUser,
    Queued,
    Downloading,
    Processing,
    Completed,
    Failed,
    Cancelled,
    Recycled,
    Paused,
}

/// The explicit user decision required before a task can be submitted again.
///
/// The state is derived from the durable task record and its output files; it
/// deliberately does not depend on a stale aria2 session entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateDownloadState {
    ExistingActive,
    ResumeAvailable,
    CompletedFilesPresent,
    CompletedFilesMissing,
    OverwriteRequired,
}

impl TaskStatus {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Parsing => "parsing",
            Self::AwaitingUser => "awaiting_user",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Recycled => "recycled",
            Self::Paused => "paused",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        Some(match value {
            "draft" => Self::Draft,
            "parsing" => Self::Parsing,
            "awaiting_user" => Self::AwaitingUser,
            "queued" => Self::Queued,
            "downloading" => Self::Downloading,
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "recycled" => Self::Recycled,
            "paused" => Self::Paused,
            _ => return None,
        })
    }
}

pub fn may_transition(from: TaskStatus, to: TaskStatus) -> bool {
    matches!(
        (from, to),
        (TaskStatus::Draft, TaskStatus::Parsing)
            | (
                TaskStatus::Parsing,
                TaskStatus::Queued | TaskStatus::AwaitingUser | TaskStatus::Failed
            )
            | (
                TaskStatus::AwaitingUser,
                TaskStatus::Queued | TaskStatus::Cancelled
            )
            | (
                TaskStatus::Queued,
                TaskStatus::Downloading
                    | TaskStatus::Paused
                    | TaskStatus::Cancelled
                    | TaskStatus::Failed
            )
            | (
                TaskStatus::Downloading,
                TaskStatus::Processing
                    | TaskStatus::Paused
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            )
            | (
                TaskStatus::Paused,
                TaskStatus::Queued | TaskStatus::Downloading | TaskStatus::Cancelled
            )
            | (
                TaskStatus::Processing,
                TaskStatus::Completed | TaskStatus::Failed
            )
            | (
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled,
                TaskStatus::Recycled
            )
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DownloadTask {
    pub id: Uuid,
    pub source: String,
    pub name: String,
    pub kind: String,
    pub status: TaskStatus,
    pub progress: u8,
    pub error: Option<String>,
    /// Non-terminal progress information such as authorization recovery or
    /// provider-native acceleration. This must never drive retry decisions.
    #[serde(default, rename = "statusMessage")]
    pub status_message: Option<String>,
    /// Stable, user-safe classification for a terminal cloud failure. This is
    /// intentionally separate from aria2's numeric error code so the UI can
    /// offer the correct next action without parsing localized text.
    #[serde(default)]
    pub failure_code: Option<String>,
    #[serde(default)]
    pub aria_gid: Option<String>,
    #[serde(default)]
    pub followed_by_gid: Option<String>,
    /// Stable aria2 metadata task GID for magnet downloads. The download child
    /// GID can be regenerated when an aria2 session is loaded.
    #[serde(default)]
    pub metadata_gid: Option<String>,
    #[serde(default)]
    pub total_length: Option<u64>,
    #[serde(default)]
    pub completed_length: u64,
    #[serde(default)]
    pub download_speed: u64,
    #[serde(default)]
    pub upload_speed: u64,
    #[serde(default)]
    pub connections: u32,
    #[serde(default)]
    pub discovered_nodes: u32,
    #[serde(default)]
    pub eta_seconds: Option<u64>,
    #[serde(default)]
    pub aria_error_code: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub completed_at: Option<i64>,
    /// True only while a byte-complete P2P task is still actively uploading.
    #[serde(default)]
    pub p2p_sharing: bool,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub post_processing_note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewTask {
    pub source: String,
    pub name: String,
    pub kind: String,
    pub is_private: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtpSegment {
    pub index: u32,
    pub start_offset: u64,
    pub end_offset: u64,
    pub completed_length: u64,
}

impl NewTask {
    pub fn direct(source: impl Into<String>) -> Self {
        let source = source.into();
        let name = source
            .rsplit('/')
            .next()
            .filter(|item| !item.is_empty())
            .unwrap_or("download")
            .to_string();
        Self {
            source,
            name,
            kind: "file".to_string(),
            is_private: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudDirectoryGroup {
    pub id: Uuid,
    pub provider: CloudProvider,
    pub remote_directory_id: String,
    pub relative_directory: String,
    pub output_directory: PathBuf,
    pub status: TaskStatus,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub post_processing_note: Option<String>,
}

/// A task as rendered in the queue. Cloud-directory roots and intermediate
/// folders are synthetic entries; ordinary downloads remain flat entries.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueueTaskPayload {
    pub id: String,
    pub source: String,
    pub name: String,
    pub kind: String,
    pub status: TaskStatus,
    pub progress: u8,
    pub error: Option<String>,
    #[serde(default, rename = "statusMessage")]
    pub status_message: Option<String>,
    /// Stable, user-safe classification for a terminal cloud failure. This is
    /// intentionally separate from aria2's numeric error code so the UI can
    /// offer the correct next action without parsing localized text.
    #[serde(default, rename = "failure_code")]
    pub failure_code: Option<String>,
    #[serde(rename = "reauthorization_required")]
    pub reauthorization_required: bool,
    pub aria_gid: Option<String>,
    pub followed_by_gid: Option<String>,
    pub metadata_gid: Option<String>,
    pub total_length: Option<u64>,
    pub completed_length: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    pub connections: u32,
    pub discovered_nodes: u32,
    pub eta_seconds: Option<u64>,
    pub aria_error_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub p2p_sharing: bool,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub post_processing_note: Option<String>,
    pub files: Vec<TransferFile>,
    pub cleanup_pending: bool,
    #[serde(default)]
    pub children: Vec<QueueTaskPayload>,
}

#[derive(Clone, Debug)]
pub struct NewCloudDirectoryGroup {
    pub provider: CloudProvider,
    pub remote_directory_id: String,
    pub relative_directory: String,
    pub output_directory: PathBuf,
    pub is_private: bool,
}

impl NewCloudDirectoryGroup {
    pub fn new(
        provider: CloudProvider,
        remote_directory_id: impl Into<String>,
        relative_directory: impl Into<String>,
        output_directory: impl AsRef<Path>,
    ) -> Self {
        Self {
            provider,
            remote_directory_id: remote_directory_id.into(),
            relative_directory: relative_directory.into(),
            output_directory: output_directory.as_ref().to_path_buf(),
            is_private: false,
        }
    }

    pub fn with_private_download(mut self, is_private: bool) -> Self {
        self.is_private = is_private;
        self
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudDirectoryGroupItem {
    pub id: Uuid,
    pub group_id: Uuid,
    pub item_index: u32,
    pub remote_file_id: String,
    pub relative_directory: Vec<String>,
    pub output_name: String,
    pub size: Option<u64>,
    pub task_id: Option<Uuid>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewCloudDirectoryGroupItem {
    pub remote_file_id: String,
    pub relative_directory: Vec<String>,
    pub output_name: String,
    pub size: Option<u64>,
}

impl NewCloudDirectoryGroupItem {
    pub fn new(
        remote_file_id: impl Into<String>,
        relative_directory: &[impl AsRef<str>],
        output_name: impl Into<String>,
        size: Option<u64>,
    ) -> Self {
        Self {
            remote_file_id: remote_file_id.into(),
            relative_directory: relative_directory
                .iter()
                .map(|component| component.as_ref().to_string())
                .collect(),
            output_name: output_name.into(),
            size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppSettings, Aria2Settings};

    #[test]
    fn aria2_defaults_include_twenty_concurrent_tasks_and_four_connections() {
        let defaults: Aria2Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.max_concurrent_downloads, 20);
        assert_eq!(defaults.max_connections_per_server, 4);

        let persisted: Aria2Settings =
            serde_json::from_str(r#"{"maxConnectionsPerServer":8}"#).unwrap();
        assert_eq!(persisted.max_connections_per_server, 8);
        assert_eq!(
            serde_json::to_value(persisted).unwrap()["maxConnectionsPerServer"],
            8
        );
    }

    #[test]
    fn aria2_bandwidth_limits_default_and_serialize_in_bytes_per_second() {
        let defaults: Aria2Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.overall_download_limit, 0);
        assert_eq!(defaults.p2p_upload_limit, 1_048_576);
        assert_eq!(
            serde_json::to_value(defaults).unwrap()["p2pUploadLimit"],
            1_048_576
        );
        assert_eq!(AppSettings::default().cloud_upload_limit, 0);
        assert_eq!(
            serde_json::to_value(AppSettings::default()).unwrap()["cloudUploadLimit"],
            0
        );
    }
}
