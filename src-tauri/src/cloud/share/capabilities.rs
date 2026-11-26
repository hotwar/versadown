use super::*;
/// Adapters own provider protocol details. Neither inspection nor resolution
/// performs implicit session recovery: challenges must return to the caller.
pub trait ShareInspect: Send + Sync {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError>;
}

pub trait ShareResolve: Send + Sync {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError>;
    /// Internal-only context rotation. The dispatcher commits this to encrypted local storage
    /// before releasing a URL or starting the transfer phase.
    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        Ok(ShareOperationResult {
            result: self.resolve(entry),
            updated_context: None,
        })
    }
}

pub trait ShareTransfer: Send + Sync {
    /// Called only for the matching durable source after a typed transfer
    /// requirement. Implementations recheck login and journal returned personal
    /// IDs before requesting the personal download URL.
    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<crate::cloud::commands::ResolvedCloudDownload, QuarkError>;
    fn transfer_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<ShareOperationResult<crate::cloud::commands::ResolvedCloudDownload>, QuarkError>
    {
        Ok(ShareOperationResult {
            result: self.transfer(entry, store),
            updated_context: None,
        })
    }
    /// A non-reversible fingerprint for the currently authorized personal
    /// drive. It gates reuse and cleanup of files created by a share transfer.
    fn transfer_owner_fingerprint(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError>;
    fn transfer_capacity(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        Err(QuarkError::ProviderUnsupported)
    }
}

pub trait RecordedTransferResolver: Send + Sync {
    /// Refresh a direct link for one exact personal-drive file previously
    /// journalled by this share. This must never perform another transfer.
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError>;
}

/// Compatibility composition for service consumers. Optional protocols are
/// represented as capabilities, never as unsupported protocol implementations.
pub trait CloudShareAdapter: ShareInspect + ShareResolve {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        None
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        None
    }
}

#[derive(Default)]
pub struct ShareCapabilities<'a> {
    pub inspect: Option<Arc<dyn ShareInspect + 'a>>,
    pub resolve: Option<Arc<dyn ShareResolve + 'a>>,
    pub transfer: Option<Arc<dyn ShareTransfer + 'a>>,
    pub recorded: Option<Arc<dyn RecordedTransferResolver + 'a>>,
}

impl ShareInspect for ShareCapabilities<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        self.inspect
            .as_ref()
            .ok_or(QuarkError::ProviderUnsupported)?
            .inspect(candidate)
    }
}
impl ShareResolve for ShareCapabilities<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        self.resolve
            .as_ref()
            .ok_or(QuarkError::ProviderUnsupported)?
            .resolve(entry)
    }
    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        self.resolve
            .as_ref()
            .ok_or(QuarkError::ProviderUnsupported)?
            .resolve_with_context(entry)
    }
}
impl CloudShareAdapter for ShareCapabilities<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        self.transfer.as_deref()
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        self.recorded.as_deref()
    }
}
impl ShareCapabilities<'_> {
    pub fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<crate::cloud::commands::ResolvedCloudDownload, QuarkError> {
        self.transfer
            .as_ref()
            .ok_or(QuarkError::ProviderUnsupported)?
            .transfer(entry, store)
    }
    pub fn transfer_owner_fingerprint(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        self.transfer
            .as_ref()
            .ok_or(QuarkError::ProviderUnsupported)?
            .transfer_owner_fingerprint(entry)
    }
    pub fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        file: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        self.recorded
            .as_ref()
            .ok_or(QuarkError::ProviderUnsupported)?
            .refresh_recorded_transfer(entry, file)
    }
}

/// Never serialize this result into IPC or task records: context is encrypted-storage-only.
pub struct ShareOperationResult<T> {
    /// A successful response may revoke context even when the operation fails.
    /// Persist updated_context before propagating this error.
    pub result: Result<T, QuarkError>,
    pub updated_context: Option<serde_json::Value>,
}

pub enum ShareInspection {
    Plan(CloudSharePlan),
    AccessCodeRequired,
}

pub enum ShareConfirmationOutcome {
    Confirmation(CloudShareConfirmation),
    AutoSubmit {
        source: String,
        confirmation: CloudShareConfirmation,
    },
    AccessCodeRequired,
}

/// A transfer request carries only a durable reference. Cleanup remains
/// separate; the dispatcher may materialize this request through the adapter.
pub struct TransferRequest {
    pub source: String,
}

pub enum ShareResolution {
    Direct(crate::cloud::commands::ResolvedCloudDownload),
    TransferRequired(TransferRequest),
}

/// A fresh direct link for one exact personal-drive file previously journalled
/// by this share. It is never a transfer request.
pub enum RecordedTransferDownload {
    Available(crate::cloud::commands::ResolvedCloudDownload),
}

/// Per-use choices a share adapter cannot derive from its credentials.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShareAdapterOptions {
    /// MEGA: request public-link downloads with the signed-in account so the
    /// account's transfer quota is used. The caller sets it only when the
    /// user allows it and the account session is ready.
    pub mega_account_quota: bool,
}

/// A recognized share page must never fall back to ordinary URI downloading.
pub fn provider_share_adapter<'a>(
    provider: CloudProvider,
    credentials: &'a dyn CredentialStore,
    http: &'a dyn crate::cloud::CloudHttpTransport,
) -> Result<Box<ShareCapabilities<'a>>, QuarkError> {
    provider_share_adapter_with(provider, credentials, http, ShareAdapterOptions::default())
}

pub fn provider_share_adapter_with<'a>(
    provider: CloudProvider,
    credentials: &'a dyn CredentialStore,
    http: &'a dyn crate::cloud::CloudHttpTransport,
    options: ShareAdapterOptions,
) -> Result<Box<ShareCapabilities<'a>>, QuarkError> {
    Ok(Box::new(
        crate::cloud::ProviderRegistry::global().share_capabilities_with(
            provider,
            credentials,
            http,
            options,
        ),
    ))
}
