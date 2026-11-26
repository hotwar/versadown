use super::*;

/// Backend of providers without an account. Every personal-drive, upload and
/// authorization operation keeps its `ProviderUnsupported` default; public
/// share links are handled by the provider's share adapter instead.
pub(super) struct AccountlessDriver;

impl ProviderBackend for AccountlessDriver {}
impl DriveOps for AccountlessDriver {}
impl DownloadOps for AccountlessDriver {}
impl ShareOps for AccountlessDriver {}
impl ProviderAuth for AccountlessDriver {}
impl UploadOps for AccountlessDriver {}
