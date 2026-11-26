//! User-actionable MEGA failures. Their wire text is a stable localization
//! key (never provider text) so the UI can translate it. Link keys must never
//! appear in it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MegaFailure {
    /// The pasted link carries no decryption key (`#KEY`), or it is malformed.
    LinkIncomplete,
    /// A recognised but not yet supported link form.
    LinkUnsupported,
    /// The key does not decrypt the shared item.
    KeyInvalid,
    /// The link was removed, expired or never existed.
    LinkUnavailable,
    /// The IP transfer quota is used up. `retry_after_secs` is MEGA's own
    /// estimate of the remaining wait when it reported one.
    QuotaExceeded { retry_after_secs: Option<u64> },
    /// The transfer quota of the signed-in account that a public link was
    /// downloaded with is used up (`-17`/HTTP 509 on an account request).
    AccountQuotaExceeded { retry_after_secs: Option<u64> },
    /// The downloaded file does not match the MAC carried by the link key:
    /// the transfer was corrupted or tampered with.
    IntegrityFailed,
    /// The MAC check could not run (missing key, source or unreadable file).
    IntegrityUnavailable,
    /// Account login: wrong e-mail or password (the API answers `-9`).
    InvalidCredentials,
    /// Account login: the account has two-factor authentication enabled and
    /// no code was supplied.
    MfaRequired,
    /// Account login: the supplied two-factor code was refused.
    MfaInvalid,
    /// The account was suspended by MEGA.
    AccountBlocked,
    /// The API refused the change (`-11`): the node is not ours to modify.
    AccessDenied,
    /// The account's storage is full (`-17` while writing).
    StorageFull,
    /// The name is empty, too long or contains characters MEGA rejects.
    InvalidName,
    /// A folder cannot be moved into itself or one of its own subfolders.
    InvalidMove,
    /// MEGA links are created one item at a time.
    ShareSingleOnly,
    /// MEGA links carry no pass code or expiry in this app.
    ShareOptionsUnsupported,
    /// A new folder link needs a share key, and this account (one without
    /// MEGA's key manager) cannot create one through the legacy command.
    FolderShareUnsupported,
    /// The account's `^!keys` container was changed by another client again
    /// and again while a share key was being recorded.
    KeysConflict,
    /// The `^!keys` container is of a kind this client cannot safely edit:
    /// unknown format, not this account's, older than a copy seen before, or
    /// an edit that would change more than intended.
    KeysUnsupported,
    /// A copy would create more nodes than one request may carry.
    CopyTooLarge,
    /// The API or upload server is still busy after its retry budget.
    ServiceBusy,
    /// The public API returned an unsuccessful HTTP status.
    ServiceUnavailable,
    /// An unmapped API failure; retain only its numeric code, never raw text.
    ApiError { code: i64 },
    /// The upload server refused a piece with an unsuccessful HTTP status.
    UploadHttpError { status: u16 },
    /// The local file could not be read for upload.
    LocalFileUnreadable,
    /// A change selected the recycle bin as its destination.
    InvalidDestination,
}

impl MegaFailure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::LinkIncomplete => "mega_link_incomplete",
            Self::LinkUnsupported => "mega_link_unsupported",
            Self::KeyInvalid => "mega_key_invalid",
            Self::LinkUnavailable => "mega_link_unavailable",
            Self::QuotaExceeded { .. } => "mega_quota_exceeded",
            Self::AccountQuotaExceeded { .. } => "mega_account_quota_exceeded",
            Self::IntegrityFailed => "mega_integrity_failed",
            Self::IntegrityUnavailable => "mega_integrity_unavailable",
            Self::InvalidCredentials => "mega_invalid_credentials",
            Self::MfaRequired => "mega_mfa_required",
            Self::MfaInvalid => "mega_mfa_invalid",
            Self::AccountBlocked => "mega_account_blocked",
            Self::AccessDenied => "mega_access_denied",
            Self::StorageFull => "mega_storage_full",
            Self::InvalidName => "mega_invalid_name",
            Self::InvalidMove => "mega_invalid_move",
            Self::ShareSingleOnly => "mega_share_single_only",
            Self::ShareOptionsUnsupported => "mega_share_options_unsupported",
            Self::FolderShareUnsupported => "mega_folder_share_unsupported",
            Self::KeysConflict => "mega_keys_conflict",
            Self::KeysUnsupported => "mega_keys_unsupported",
            Self::CopyTooLarge => "mega_copy_too_large",
            Self::ServiceBusy => "mega_service_busy",
            Self::ServiceUnavailable => "mega_service_unavailable",
            Self::ApiError { .. } => "mega_api_error",
            Self::UploadHttpError { .. } => "mega_upload_http_error",
            Self::LocalFileUnreadable => "mega_local_file_unreadable",
            Self::InvalidDestination => "mega_invalid_destination",
        }
    }

    /// Localization key without parameters.
    pub fn message_key(&self) -> &'static str {
        match self {
            Self::LinkIncomplete => "cloud.error.mega.linkIncomplete",
            Self::LinkUnsupported => "cloud.error.mega.linkUnsupported",
            Self::KeyInvalid => "cloud.error.mega.keyInvalid",
            Self::LinkUnavailable => "cloud.error.mega.linkUnavailable",
            Self::QuotaExceeded { .. } => "cloud.error.mega.quotaExceeded",
            Self::AccountQuotaExceeded { .. } => "cloud.error.mega.accountQuotaExceeded",
            Self::IntegrityFailed => "cloud.error.mega.integrityFailed",
            Self::IntegrityUnavailable => "cloud.error.mega.integrityUnavailable",
            Self::InvalidCredentials => "cloud.error.mega.invalidCredentials",
            Self::MfaRequired => "cloud.error.mega.mfaRequired",
            Self::MfaInvalid => "cloud.error.mega.mfaInvalid",
            Self::AccountBlocked => "cloud.error.mega.accountBlocked",
            Self::AccessDenied => "cloud.error.mega.accessDenied",
            Self::StorageFull => "cloud.error.mega.storageFull",
            Self::InvalidName => "cloud.error.mega.invalidName",
            Self::InvalidMove => "cloud.error.mega.invalidMove",
            Self::ShareSingleOnly => "cloud.error.mega.shareSingleOnly",
            Self::ShareOptionsUnsupported => "cloud.error.mega.shareOptionsUnsupported",
            Self::FolderShareUnsupported => "cloud.error.mega.folderShareUnsupported",
            Self::KeysConflict => "cloud.error.mega.keysConflict",
            Self::KeysUnsupported => "cloud.error.mega.keysUnsupported",
            Self::CopyTooLarge => "cloud.error.mega.copyTooLarge",
            Self::ServiceBusy => "cloud.error.mega.serviceBusy",
            Self::ServiceUnavailable => "cloud.error.mega.serviceUnavailable",
            Self::ApiError { .. } => "cloud.error.mega.apiError",
            Self::UploadHttpError { .. } => "cloud.error.mega.uploadHttpError",
            Self::LocalFileUnreadable => "cloud.error.mega.localFileUnreadable",
            Self::InvalidDestination => "cloud.error.mega.invalidDestination",
        }
    }

    /// Localization key, with a numeric parameter appended when available.
    pub fn message(&self) -> String {
        match self {
            Self::QuotaExceeded {
                retry_after_secs: Some(seconds),
            }
            | Self::AccountQuotaExceeded {
                retry_after_secs: Some(seconds),
            } => format!("{}:{seconds}", self.message_key()),
            Self::ApiError { code } => format!("{}:{code}", self.message_key()),
            Self::UploadHttpError { status } => format!("{}:{status}", self.message_key()),
            _ => self.message_key().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_stable_localization_keys() {
        assert_eq!(
            MegaFailure::QuotaExceeded {
                retry_after_secs: Some(1800)
            }
            .message(),
            "cloud.error.mega.quotaExceeded:1800"
        );
        assert_eq!(
            MegaFailure::AccountQuotaExceeded {
                retry_after_secs: Some(60)
            }
            .message(),
            "cloud.error.mega.accountQuotaExceeded:60"
        );
        assert_eq!(
            MegaFailure::AccountQuotaExceeded {
                retry_after_secs: None
            }
            .message(),
            "cloud.error.mega.accountQuotaExceeded"
        );
        assert_eq!(
            MegaFailure::ApiError { code: -99 }.message(),
            "cloud.error.mega.apiError:-99"
        );
        assert_eq!(
            MegaFailure::UploadHttpError { status: 403 }.message(),
            "cloud.error.mega.uploadHttpError:403"
        );
        assert_eq!(
            MegaFailure::IntegrityUnavailable.message(),
            "cloud.error.mega.integrityUnavailable"
        );
        assert_eq!(
            MegaFailure::LinkIncomplete.message(),
            "cloud.error.mega.linkIncomplete"
        );
    }
}
