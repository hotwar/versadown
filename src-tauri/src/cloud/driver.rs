//! Object-safe capabilities; protocol selection happens once in the native factory.
use super::commands::CloudState;
use super::*;
use std::path::Path;

pub(crate) mod native;
pub use native::NativeDriverFactory;

pub trait DriveOps {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    /// Resolve the account that owns new or resumed personal-drive work.
    /// Connectors whose card identity is intentionally generic can override
    /// this with a stronger live identity probe.
    fn verified_account(&mut self) -> Result<CloudAccount, CloudError> {
        self.account()
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn list_directory(&mut self, _parent: &str) -> Result<CloudDirectory, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn create_directory(&mut self, _parent: &str, _name: &str) -> Result<CloudFile, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn rename(&mut self, _file: &str, _name: &str) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn move_files(&mut self, _files: &[String], _parent: &str) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn copy_files(&mut self, _files: &[String], _parent: &str) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn trash(&mut self, _files: &[String]) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn trash_transferred_files(&mut self, files: &[String]) -> Result<(), CloudError> {
        self.trash(files)
    }
    fn delete(&mut self, _files: &[String]) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn restore(&mut self, _files: &[String]) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
}

pub trait DownloadOps {
    fn resolve_download(&mut self, _file: &str) -> Result<ResolvedCloudDownload, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
}

/// Managing links created by the current account. This intentionally stays
/// separate from the existing share adapters, which consume links owned by
/// somebody else.
pub trait ShareOps {
    fn create_share(
        &mut self,
        _file_ids: &[String],
        _options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn cancel_shares(&mut self, _share_ids: &[String]) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    /// Providers that can report per-item results override this; the default
    /// is all-or-nothing on top of `cancel_shares`.
    fn cancel_shares_detailed(
        &mut self,
        share_ids: &[String],
    ) -> Result<CloudShareCancelOutcome, CloudError> {
        self.cancel_shares(share_ids)?;
        Ok(CloudShareCancelOutcome::all_cancelled(share_ids))
    }
    fn update_share_pass_code(
        &mut self,
        _share_id: &str,
        _pass_code: &str,
    ) -> Result<(), CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
}

#[derive(Default)]
pub struct WebAuthorizationInput {
    pub cookies: Vec<String>,
    pub storage: serde_json::Value,
    pub browser_context: Option<serde_json::Value>,
    pub auth_method: CloudAuthMethod,
}

#[derive(Clone, Copy, Default)]
pub struct WebAuthorizationRecipe {
    pub token_script: Option<&'static str>,
    pub browser_context: bool,
}

/// Only the native login phase can request browser fallback. Validation and
/// persistence errors remain ordinary errors, even when they have the same code.
pub enum NativeLoginOutcome {
    Authenticated(CloudAccount),
    NeedBrowserFallback(CloudError),
}

pub(crate) fn complete_native_password_login<S>(
    login: Result<S, CloudError>,
    validate_and_save: impl FnOnce(S) -> Result<CloudAccount, CloudError>,
) -> Result<NativeLoginOutcome, CloudError> {
    let session = match login {
        Ok(session) => session,
        Err(error)
            if browser_login::can_fallback_to_browser(&error)
                || matches!(error, CloudError::CaptchaRequired) =>
        {
            return Ok(NativeLoginOutcome::NeedBrowserFallback(error));
        }
        Err(error) => return Err(error),
    };
    validate_and_save(session).map(NativeLoginOutcome::Authenticated)
}

/// Protocol inputs only: the command host owns windows, scripts and browser fallback.
pub trait ProviderAuth {
    /// Returns false when no renewable credential is available. Never opens UI.
    fn refresh_session(&mut self) -> Result<bool, CloudError> {
        Ok(false)
    }
    fn web_authorization_recipe(&self) -> WebAuthorizationRecipe {
        WebAuthorizationRecipe::default()
    }
    fn complete_web(&mut self, _input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
    fn login_password(
        &mut self,
        _username: &str,
        _password: &str,
    ) -> Result<NativeLoginOutcome, CloudError> {
        Err(CloudError::WebAuthorizationRequired)
    }
    fn recover_password(
        &mut self,
        _credential: &CloudPasswordCredential,
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        Err(CloudError::WebAuthorizationRequired)
    }
    fn validate_browser_password(
        &mut self,
        _cookies: &[String],
        _context: Option<&serde_json::Value>,
        _report: &mut dyn FnMut(&'static str),
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        Err(CloudError::WebAuthorizationRequired)
    }
    fn web_session_snapshot(
        &mut self,
        _input: &WebAuthorizationInput,
    ) -> Result<Option<Vec<u8>>, CloudError> {
        Ok(None)
    }
}

pub trait UploadHttp: CloudHttpTransport + CloudUploadTransport {}
impl<T: CloudHttpTransport + CloudUploadTransport> UploadHttp for T {}

pub trait UploadOps {
    fn upload(
        &mut self,
        _state: &CloudState,
        _http: &dyn UploadHttp,
        _parent: &str,
        _path: &Path,
    ) -> Result<CloudFile, CloudError> {
        Err(CloudError::ProviderUnsupported)
    }
}

pub trait ProviderBackend: DriveOps + DownloadOps + ShareOps + ProviderAuth + UploadOps {}

pub trait ProviderDriverFactory: Send + Sync {
    fn create<'a>(
        &self,
        provider: CloudProvider,
        store: &'a dyn CredentialStore,
        http: &'a dyn CloudHttpTransport,
    ) -> Box<dyn ProviderBackend + 'a>;
}

pub struct ProviderDriver<'a> {
    capabilities: ProviderCapabilities,
    backend: Box<dyn ProviderBackend + 'a>,
}

impl<'a> ProviderDriver<'a> {
    pub fn new(capabilities: ProviderCapabilities, backend: Box<dyn ProviderBackend + 'a>) -> Self {
        Self {
            capabilities,
            backend,
        }
    }
    pub fn auth(&mut self) -> &mut dyn ProviderAuth {
        &mut *self.backend
    }
    pub fn account(&mut self) -> Result<CloudAccount, CloudError> {
        self.backend.account()
    }
    pub fn verified_account(&mut self) -> Result<CloudAccount, CloudError> {
        self.backend.verified_account()
    }
    pub fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        self.backend.storage_capacity()
    }
    pub fn list_directory(&mut self, parent: &str) -> Result<CloudDirectory, CloudError> {
        require(self.capabilities.list)?;
        self.backend.list_directory(parent)
    }
    pub fn create_directory(&mut self, parent: &str, name: &str) -> Result<CloudFile, CloudError> {
        require(self.capabilities.create_directory)?;
        self.backend.create_directory(parent, name)
    }
    pub fn rename(&mut self, file: &str, name: &str) -> Result<(), CloudError> {
        require(self.capabilities.rename)?;
        self.backend.rename(file, name)
    }
    pub fn move_files(&mut self, files: &[String], parent: &str) -> Result<(), CloudError> {
        require(self.capabilities.r#move)?;
        self.backend.move_files(files, parent)
    }
    pub fn copy_files(&mut self, files: &[String], parent: &str) -> Result<(), CloudError> {
        require(self.capabilities.copy)?;
        self.backend.copy_files(files, parent)
    }
    pub fn trash(&mut self, files: &[String]) -> Result<(), CloudError> {
        require(self.capabilities.trash)?;
        self.backend.trash(files)
    }
    pub fn trash_transferred_files(&mut self, files: &[String]) -> Result<(), CloudError> {
        require(self.capabilities.trash)?;
        self.backend.trash_transferred_files(files)
    }
    pub fn delete(&mut self, files: &[String]) -> Result<(), CloudError> {
        require(self.capabilities.delete)?;
        self.backend.delete(files)
    }
    pub fn restore(&mut self, files: &[String]) -> Result<(), CloudError> {
        require(self.capabilities.restore)?;
        self.backend.restore(files)
    }
    pub fn resolve_download(&mut self, file: &str) -> Result<ResolvedCloudDownload, CloudError> {
        require(self.capabilities.download)?;
        self.backend.resolve_download(file)
    }
    pub fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        require(self.capabilities.share_create)?;
        self.backend.create_share(file_ids, options)
    }
    pub fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        require(self.capabilities.share_list)?;
        self.backend.list_shares()
    }
    pub fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        require(self.capabilities.share_cancel)?;
        self.backend.cancel_shares(share_ids)
    }
    pub fn cancel_shares_detailed(
        &mut self,
        share_ids: &[String],
    ) -> Result<CloudShareCancelOutcome, CloudError> {
        require(self.capabilities.share_cancel)?;
        self.backend.cancel_shares_detailed(share_ids)
    }
    pub fn update_share_pass_code(
        &mut self,
        share_id: &str,
        pass_code: &str,
    ) -> Result<(), CloudError> {
        require(self.capabilities.share_update_pass_code)?;
        self.backend.update_share_pass_code(share_id, pass_code)
    }
    pub fn upload(
        &mut self,
        state: &CloudState,
        http: &dyn UploadHttp,
        parent: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        require(self.capabilities.upload)?;
        self.backend.upload(state, http, parent, path)
    }
}

fn require(enabled: bool) -> Result<(), CloudError> {
    if enabled {
        Ok(())
    } else {
        Err(CloudError::ProviderUnsupported)
    }
}

pub(crate) struct StoreRef<'a>(pub(crate) &'a dyn CredentialStore);
impl CredentialStore for StoreRef<'_> {
    fn automatic_session_recovery(&self) -> bool {
        self.0.automatic_session_recovery()
    }
    fn save(&self, key: &str, bytes: &[u8]) -> Result<(), CloudError> {
        self.0.save(key, bytes)
    }
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, CloudError> {
        self.0.load(key)
    }
    fn remove(&self, key: &str) -> Result<(), CloudError> {
        self.0.remove(key)
    }
}

struct HttpRef<'a>(&'a dyn CloudHttpTransport);
impl CloudHttpTransport for HttpRef<'_> {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
        self.0.send(request)
    }
    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), CloudError> {
        self.0.send_with_cookie_snapshot(request)
    }
    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, CloudError> {
        self.0.isolated_session()
    }
    fn manages_cookie_session(&self) -> bool {
        self.0.manages_cookie_session()
    }
    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, CloudError> {
        self.0.cookies_for_url(url)
    }
}

struct UploadHttpRef<'a>(&'a dyn UploadHttp);
impl CloudHttpTransport for UploadHttpRef<'_> {
    fn check_transfer(&self) -> Result<(), CloudError> {
        self.0.check_transfer()
    }
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
        self.0.send(request)
    }
    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), CloudError> {
        self.0.send_with_cookie_snapshot(request)
    }
    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, CloudError> {
        self.0.isolated_session()
    }
    fn manages_cookie_session(&self) -> bool {
        self.0.manages_cookie_session()
    }
    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, CloudError> {
        self.0.cookies_for_url(url)
    }
}
impl CloudUploadTransport for UploadHttpRef<'_> {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, CloudError> {
        self.0.send_upload(request)
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    struct CapacityBackend;

    impl DriveOps for CapacityBackend {
        fn account(&mut self) -> Result<CloudAccount, CloudError> {
            Ok(CloudAccount {
                provider: CloudProvider::Quark,
                id: "live-owner".into(),
                display_name: "Live Owner".into(),
                auth_method: CloudAuthMethod::Web,
            })
        }
        fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
            Ok(CloudStorageCapacity::Limited {
                total_bytes: 100,
                used_bytes: 25,
            })
        }
    }
    impl DownloadOps for CapacityBackend {}
    impl ShareOps for CapacityBackend {}
    impl ProviderAuth for CapacityBackend {}
    impl UploadOps for CapacityBackend {}
    impl ProviderBackend for CapacityBackend {}

    #[test]
    fn provider_driver_storage_capacity_delegates_without_share_context() {
        let capabilities = ProviderRegistry::global()
            .get(CloudProvider::Quark)
            .capabilities;
        let mut driver = ProviderDriver::new(capabilities, Box::new(CapacityBackend));
        assert_eq!(
            driver.storage_capacity().unwrap(),
            CloudStorageCapacity::Limited {
                total_bytes: 100,
                used_bytes: 25,
            }
        );
    }

    #[test]
    fn provider_driver_verified_account_defaults_to_the_connectors_live_account() {
        let capabilities = ProviderRegistry::global()
            .get(CloudProvider::Quark)
            .capabilities;
        let mut driver = ProviderDriver::new(capabilities, Box::new(CapacityBackend));

        assert_eq!(driver.verified_account().unwrap().id, "live-owner");
    }
}
