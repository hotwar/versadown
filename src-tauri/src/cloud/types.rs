use serde::{Deserialize, Serialize};
use thiserror::Error;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum CloudProvider {
    Quark,
    Pikpak,
    Baidu,
    Tianyi,
    Aliyun,
    Xunlei,
    Uc,
}
impl CloudProvider {
    pub const ALL: [Self; 7] = [
        Self::Quark,
        Self::Pikpak,
        Self::Baidu,
        Self::Tianyi,
        Self::Aliyun,
        Self::Xunlei,
        Self::Uc,
    ];

    /// Tianyi remains a known serialized provider so historical task records
    /// can be purged safely, but it is not an active product integration.
    pub fn is_enabled(self) -> bool {
        self != Self::Tianyi
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Quark => "quark",
            Self::Pikpak => "pikpak",
            Self::Baidu => "baidu",
            Self::Tianyi => "tianyi",
            Self::Aliyun => "aliyun",
            Self::Xunlei => "xunlei",
            Self::Uc => "uc",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudAuthorizationCapabilities {
    pub provider: CloudProvider,
    pub web: bool,
    pub password: bool,
    pub password_login_mode: PasswordLoginMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PasswordLoginMode {
    None,
    RustDirect,
    OfficialWebView,
}

pub fn provider_capabilities(provider: CloudProvider) -> CloudAuthorizationCapabilities {
    if !provider.is_enabled() {
        return CloudAuthorizationCapabilities {
            provider,
            web: false,
            password: false,
            password_login_mode: PasswordLoginMode::None,
        };
    }
    super::ProviderRegistry::global()
        .get(provider)
        .authorization
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CloudAuthMethod {
    #[default]
    Web,
    Password,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccount {
    pub provider: CloudProvider,
    pub id: String,
    pub display_name: String,
    pub auth_method: CloudAuthMethod,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CloudStorageCapacity {
    Limited {
        #[serde(rename = "totalBytes")]
        total_bytes: u64,
        #[serde(rename = "usedBytes")]
        used_bytes: u64,
    },
    Unlimited {
        #[serde(rename = "usedBytes")]
        used_bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CloudCapacityChangeReason {
    Upload,
    Copy,
    Transfer,
    Trash,
    Restore,
    Delete,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudCapacityInvalidated {
    pub provider: CloudProvider,
    pub account_id: String,
    pub reason: CloudCapacityChangeReason,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudAccountCapacity {
    pub provider: CloudProvider,
    pub account_id: String,
    pub capacity: CloudStorageCapacity,
    pub updated_at: u64,
}

impl CloudStorageCapacity {
    pub(crate) fn from_json(
        total: &serde_json::Value,
        used: &serde_json::Value,
        minus_one_is_unlimited: bool,
    ) -> Result<Self, QuarkError> {
        let used_bytes = parse_capacity_u64(used)?;
        let unlimited =
            minus_one_is_unlimited && (total.as_i64() == Some(-1) || total.as_str() == Some("-1"));
        if unlimited {
            return Ok(Self::Unlimited { used_bytes });
        }
        let total_bytes = parse_capacity_u64(total)?;
        if used_bytes > total_bytes {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(Self::Limited {
            total_bytes,
            used_bytes,
        })
    }

    pub(crate) fn from_total_and_available_json(
        total: &serde_json::Value,
        available: &serde_json::Value,
    ) -> Result<Self, QuarkError> {
        let total_bytes = parse_capacity_u64(total)?;
        let available_bytes = parse_capacity_u64(available)?;
        let used_bytes = total_bytes
            .checked_sub(available_bytes)
            .ok_or(QuarkError::InvalidResponse)?;
        Ok(Self::Limited {
            total_bytes,
            used_bytes,
        })
    }

    pub fn available_bytes(&self) -> Option<u64> {
        match *self {
            Self::Limited {
                total_bytes,
                used_bytes,
            } => total_bytes.checked_sub(used_bytes),
            Self::Unlimited { .. } => None,
        }
    }

    pub fn used_bytes(&self) -> u64 {
        match *self {
            Self::Limited { used_bytes, .. } | Self::Unlimited { used_bytes } => used_bytes,
        }
    }
}

fn parse_capacity_u64(value: &serde_json::Value) -> Result<u64, QuarkError> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_str()
                .filter(|raw| !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|raw| raw.parse().ok())
        })
        .ok_or(QuarkError::InvalidResponse)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudFile {
    pub id: String,
    pub parent_id: String,
    pub name: String,
    pub kind: CloudFileKind,
    pub size: Option<u64>,
    pub modified_at: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CloudDirectory {
    pub id: String,
    pub items: Vec<CloudFile>,
}

/// Provider-neutral sharing settings. Provider drivers translate these values
/// to their own wire protocol instead of exposing provider-specific fields to
/// the command or UI layer.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudShareOptions {
    pub name: String,
    pub pass_code: Option<String>,
    #[serde(default)]
    pub generate_pass_code: bool,
    pub expires_in_days: Option<u16>,
}

/// A provider-declared constraint for a user-entered share pass code.
///
/// The shared UI consumes this metadata while each provider plugin owns its
/// own requirements.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudSharePassCodeConstraint {
    pub required: bool,
    pub min_length: u8,
    pub max_length: u8,
    pub character_set: CloudSharePassCodeCharacterSet,
    pub hint: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CloudSharePassCodeCharacterSet {
    Any,
    AsciiAlphanumeric,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudShare {
    pub id: String,
    pub url: String,
    pub name: String,
    pub kind: CloudFileKind,
    pub source_file_ids: Vec<String>,
    pub pass_code: Option<String>,
    pub created_at: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CloudFileKind {
    File,
    Folder,
}

impl CloudFileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Folder => "folder",
        }
    }
}

impl std::str::FromStr for CloudFileKind {
    type Err = CloudError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "file" => Ok(Self::File),
            "folder" => Ok(Self::Folder),
            _ => Err(CloudError::InvalidResponse),
        }
    }
}

impl From<&str> for CloudFileKind {
    fn from(value: &str) -> Self {
        match value {
            "file" => Self::File,
            "folder" => Self::Folder,
            _ => panic!("invalid internal cloud file kind: {value}"),
        }
    }
}

impl PartialEq<&str> for CloudFileKind {
    fn eq(&self, other: &&str) -> bool {
        matches!(
            (self, *other),
            (Self::File, "file") | (Self::Folder, "folder")
        )
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAuthorization {
    pub provider: CloudProvider,
    pub window_label: String,
    pub instructions: String,
    pub completed: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedCloudCredentialInfo {
    pub username: String,
    pub has_saved_password: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordLoginRequest {
    pub provider: CloudProvider,
    pub username: String,
    pub password: Option<String>,
    #[serde(default)]
    pub use_saved_password: bool,
}

impl Drop for PasswordLoginRequest {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.username.zeroize();
        self.password.zeroize();
    }
}
#[derive(Clone, Copy, Debug)]
pub enum XunleiNativeDownloadFailure {
    #[cfg(target_os = "macos")]
    UnsupportedCheckpoint,
    ProxyCheckpoint,
    SubmitFailed,
}

impl XunleiNativeDownloadFailure {
    pub(crate) fn code(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::UnsupportedCheckpoint => "xunlei_unsupported_checkpoint",
            Self::ProxyCheckpoint => "xunlei_proxy_checkpoint",
            Self::SubmitFailed => "xunlei_submission_failed",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::UnsupportedCheckpoint => crate::xunlei_native::macos_checkpoint_notice(),
            Self::ProxyCheckpoint => "download.xunlei.proxyCheckpoint",
            Self::SubmitFailed => "download.xunlei.submitFailed",
        }
    }
}

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("尚未授权此网盘，请登录后使用")]
    NotAuthorized,
    #[error("当前登录的网盘账号与下载任务所属账号不一致，请切换账号或取消该任务")]
    AccountMismatch,
    #[error("网盘会话已过期，请重新登录授权")]
    AuthorizationExpired,
    #[error("登录已自动恢复，请重新执行本次操作")]
    SessionRenewedRetryOperation,
    #[error("此登录方式需要网页验证，请点击网页授权完成登录")]
    WebAuthorizationRequired,
    #[error("{}", xunlei_verification_text(.0))]
    XunleiVerificationUnavailable(&'static str),
    #[error("网盘要求完成官方操作验证后重试")]
    CaptchaRequired,
    #[error("PikPak {0}需要官方操作验证，并非登录失效；请完成验证后返回重试")]
    PikpakOperationVerificationRequired(&'static str),
    #[error("baidu_operation_verification_required")]
    BaiduOperationVerificationRequired,
    #[error("区域会员在当前 IP 地区下载受限；请切换到有权限 IP 后重新登录重试，或升级全球会员")]
    PikpakRegionalMembershipRestricted,
    #[error("正在等待网页登录，请在授权窗口完成登录")]
    AuthorizationPending,
    #[error("网盘中的源文件已不存在，请刷新网盘列表后重新创建下载")]
    SourceFileMissing,
    #[error("本地下载文件或续传断点已缺失或失效，无法继续下载；请重新创建下载任务从头下载（不会自动覆盖已有文件）")]
    DownloadCheckpointInvalid,
    #[error("本地下载引擎不可用，请重启应用后重试")]
    DownloadEngineUnavailable,
    #[error("本地下载引擎未能接受任务，请重试；若持续失败请检查下载目录和引擎日志")]
    DownloadSubmissionFailed,
    #[error("{}", .0.message())]
    XunleiNativeDownload(XunleiNativeDownloadFailure),
    #[error("百度转存仍在进行或结果未知，请稍后在网盘中查看，确认后再重试")]
    BaiduTransferPending,
    #[error("云盘响应无效，请刷新后重试")]
    InvalidResponse,
    #[error("安全凭据存储不可用")]
    Storage,
    #[error("网络请求失败：{0}")]
    Transport(String),
    #[error("网盘请求失败：{0}")]
    Provider(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("当前上传协议尚未接通，请使用网盘官方页面上传")]
    UploadUnavailable,
    #[error("当前网盘服务尚未接通，请稍后重试")]
    ProviderUnsupported,
}
pub use CloudError as QuarkError;

/// User-readable Chinese text for a Xunlei verification reason code. The code
/// itself stays the stable wire value; it must never be shown to the user.
pub(crate) fn xunlei_verification_text(reason: &str) -> &'static str {
    match reason {
        "metadata_missing" => "缺少迅雷操作验证信息，请重新网页授权后重试",
        "metadata_expired" => "迅雷分享信息已过期，请重新打开分享链接或重新网页授权",
        "timestamp_invalid" => "迅雷验证信息的时间戳无效，请检查系统时间后重试",
        "xunlei_share_profile_unsupported" => {
            "当前迅雷网页版本暂不支持解析此分享，请在迅雷官网打开该分享"
        }
        _ => "暂时无法准备迅雷操作所需的验证信息，请稍后重试",
    }
}

/// Outcome of cancelling several shares one by one. Already-deleted ids are
/// reported as cancelled so a retry only needs to carry the failed ones.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudShareCancelOutcome {
    pub cancelled: Vec<String>,
    pub failures: Vec<CloudShareCancelFailure>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CloudShareCancelFailure {
    pub id: String,
    pub code: String,
    pub message: String,
}

impl CloudShareCancelOutcome {
    pub fn all_cancelled(ids: &[String]) -> Self {
        Self { cancelled: ids.to_vec(), failures: Vec::new() }
    }
}

impl CloudShareCancelFailure {
    pub fn from_error(id: &str, error: &CloudError) -> Self {
        let message = match error {
            CloudError::Provider(_) => "迅雷取消分享请求失败，请稍后重试".to_owned(),
            CloudError::Transport(_) => "网络请求失败，请检查网络后重试".to_owned(),
            other => other.to_string(),
        };
        let code = serde_json::to_value(error)
            .ok()
            .and_then(|value| value["code"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "provider_error".into());
        Self { id: id.to_owned(), code, message }
    }
}

impl CloudError {
    /// Errors about the whole session rather than one item: continuing the
    /// batch would only repeat the same failure.
    pub fn is_session_level(&self) -> bool {
        matches!(
            self,
            Self::NotAuthorized
                | Self::AccountMismatch
                | Self::AuthorizationExpired
                | Self::SessionRenewedRetryOperation
                | Self::WebAuthorizationRequired
                | Self::CaptchaRequired
                | Self::XunleiVerificationUnavailable(_)
                | Self::Storage
        )
    }
}

impl Serialize for CloudError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let code = match self {
            Self::NotAuthorized => "not_authorized",
            Self::AccountMismatch => "account_mismatch",
            Self::AuthorizationExpired => "credentials_expired",
            Self::SessionRenewedRetryOperation => "session_renewed_retry_operation",
            Self::WebAuthorizationRequired => "web_auth_required",
            Self::XunleiVerificationUnavailable(_) => "xunlei_verification_unavailable",
            Self::CaptchaRequired => "web_auth_required",
            Self::PikpakOperationVerificationRequired(_) => {
                "pikpak_operation_verification_required"
            }
            Self::BaiduOperationVerificationRequired => "baidu_operation_verification_required",
            Self::PikpakRegionalMembershipRestricted => "pikpak_regional_membership_restricted",
            Self::AuthorizationPending => "authorization_pending",
            Self::SourceFileMissing => "source_file_missing",
            Self::DownloadCheckpointInvalid => "download_checkpoint_invalid",
            Self::DownloadEngineUnavailable => "download_engine_unavailable",
            Self::DownloadSubmissionFailed => "download_submission_failed",
            Self::XunleiNativeDownload(failure) => failure.code(),
            Self::BaiduTransferPending => "baidu_transfer_pending",
            Self::Storage => "credential_storage_error",
            Self::InvalidResponse => "invalid_response",
            Self::Transport(_) => "network_error",
            Self::Provider(_) => "provider_error",
            Self::InvalidInput(_) => "invalid_input",
            Self::UploadUnavailable => "provider_unsupported",
            Self::ProviderUnsupported => "provider_unsupported",
        };
        // Xunlei verification keeps its stable reason on the wire so the
        // frontend can localize it; Display stays human-readable.
        let message = match self {
            Self::XunleiVerificationUnavailable(reason) => (*reason).to_owned(),
            other => other.to_string(),
        };
        serde_json::json!({"code":code,"message":message}).serialize(s)
    }
}

/// Maps the generic HTTP 404 text produced by the shared JSON transport only
/// when a provider is resolving a concrete file download endpoint. A 404 on
/// other endpoints (such as a stale share token) has provider-specific
/// meaning and must not be treated as a deleted source file.
pub(crate) fn download_source_missing_from_http_404(error: QuarkError) -> QuarkError {
    match error {
        QuarkError::Provider(message) if message.to_ascii_lowercase().contains("http 404") => {
            QuarkError::SourceFileMissing
        }
        error => error,
    }
}
pub(crate) fn required(v: &serde_json::Value, key: &str) -> Result<String, QuarkError> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(QuarkError::InvalidResponse)
}
pub(crate) fn encoded(v: &str) -> String {
    url::form_urlencoded::byte_serialize(v.as_bytes()).collect()
}

pub struct ResolvedCloudDownload {
    pub url: String,
    pub name: String,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cloud_storage_capacity_parses_limited_numeric_and_string_values() {
        assert_eq!(
            CloudStorageCapacity::from_json(&json!(100), &json!(40), false).unwrap(),
            CloudStorageCapacity::Limited {
                total_bytes: 100,
                used_bytes: 40,
            }
        );
        assert_eq!(
            CloudStorageCapacity::from_json(&json!("100"), &json!("40"), false).unwrap(),
            CloudStorageCapacity::Limited {
                total_bytes: 100,
                used_bytes: 40,
            }
        );
    }

    #[test]
    fn cloud_storage_capacity_preserves_used_bytes_for_unlimited_storage() {
        let capacity = CloudStorageCapacity::from_json(&json!(-1), &json!(40), true).unwrap();
        assert_eq!(capacity, CloudStorageCapacity::Unlimited { used_bytes: 40 });
        assert_eq!(capacity.available_bytes(), None);
    }

    #[test]
    fn cloud_storage_capacity_rejects_invalid_values() {
        for (total, used, allow_unlimited) in [
            (json!(-1), json!(40), false),
            (json!(100), json!(-1), false),
            (json!(100), json!(101), false),
            (json!("1.5"), json!(1), false),
            (json!(null), json!(1), false),
        ] {
            assert!(CloudStorageCapacity::from_json(&total, &used, allow_unlimited).is_err());
        }
    }

    #[test]
    fn cloud_storage_capacity_converts_total_and_available_bytes() {
        assert_eq!(
            CloudStorageCapacity::from_total_and_available_json(&json!(100), &json!(35)).unwrap(),
            CloudStorageCapacity::Limited {
                total_bytes: 100,
                used_bytes: 65,
            }
        );
        assert!(
            CloudStorageCapacity::from_total_and_available_json(&json!(100), &json!(101)).is_err()
        );
    }

    #[test]
    fn cloud_capacity_invalidation_serializes_account_bound_payload() {
        let value = serde_json::to_value(CloudCapacityInvalidated {
            provider: CloudProvider::Quark,
            account_id: "account-1".into(),
            reason: CloudCapacityChangeReason::Upload,
        })
        .unwrap();
        assert_eq!(
            value,
            json!({"provider":"quark","accountId":"account-1","reason":"upload"})
        );
    }

    #[test]
    fn file_download_http_404_is_a_typed_missing_source_only_at_download_boundary() {
        assert!(matches!(
            download_source_missing_from_http_404(QuarkError::Provider(
                "HTTP 404 / code 0 / not_found".into()
            )),
            QuarkError::SourceFileMissing
        ));
        assert!(matches!(
            download_source_missing_from_http_404(QuarkError::Provider(
                "HTTP 403 / code 0 / forbidden".into()
            )),
            QuarkError::Provider(_)
        ));
        assert_eq!(
            serde_json::to_value(QuarkError::SourceFileMissing).unwrap(),
            json!({
                "code":"source_file_missing",
                "message":"网盘中的源文件已不存在，请刷新网盘列表后重新创建下载"
            })
        );
        assert_eq!(
            serde_json::to_value(QuarkError::AccountMismatch).unwrap(),
            json!({
                "code":"account_mismatch",
                "message":"当前登录的网盘账号与下载任务所属账号不一致，请切换账号或取消该任务"
            })
        );
    }
}
