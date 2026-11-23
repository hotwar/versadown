use super::*;
use crate::cloud::mega::account::MegaAccountClient;

/// MEGA account backend: browse, download and manage the drive (folders,
/// rename, move, copy, rubbish bin, upload, public links). Signing in has its
/// own command (the form needs a second-factor code), so the generic
/// `ProviderAuth` login entry points stay unsupported.
pub(super) struct MegaDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}

impl MegaDriver<'_> {
    fn client(&self) -> MegaAccountClient<'_> {
        MegaAccountClient::new(&self.http, &self.store)
    }
}

impl ProviderBackend for MegaDriver<'_> {}
impl DriveOps for MegaDriver<'_> {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        self.client().account()
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        self.client().storage_capacity()
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        self.client().list(parent_id)
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        self.client().create_directory(parent_id, name)
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        self.client().rename(file_id, name)
    }
    fn move_files(&mut self, file_ids: &[String], parent_id: &str) -> Result<(), CloudError> {
        self.client().move_files(file_ids, parent_id)
    }
    fn copy_files(&mut self, file_ids: &[String], parent_id: &str) -> Result<(), CloudError> {
        self.client().copy_files(file_ids, parent_id)
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        self.client().trash(file_ids)
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        self.client().delete(file_ids)
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        self.client().restore(file_ids)
    }
}
impl DownloadOps for MegaDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        self.client().resolve_download(file_id)
    }
}
impl ShareOps for MegaDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        self.client().create_share(file_ids, options)
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        self.client().list_shares()
    }
    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        self.client().cancel_shares(share_ids)
    }
}
impl UploadOps for MegaDriver<'_> {
    fn upload(
        &mut self,
        state: &CloudState,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let http = UploadHttpRef(upload_http);
        state.with_upload_session(CloudProvider::Mega, &http, |store| {
            MegaAccountClient::new(&http, store).upload_file(&http, parent_id, path)
        })
    }
}
// A MEGA session cannot be renewed without the password, which is never
// stored: the defaults report "nothing to refresh" and the user signs in again.
impl ProviderAuth for MegaDriver<'_> {}
