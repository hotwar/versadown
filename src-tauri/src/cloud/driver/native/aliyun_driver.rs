use super::*;
pub(super) struct AliyunDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}
impl ProviderBackend for AliyunDriver<'_> {}
impl DriveOps for AliyunDriver<'_> {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Aliyun;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        aliyun::AliyunClient::new(store, http).account()
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        aliyun::AliyunClient::new(&self.store, &self.http).storage_capacity()
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Aliyun;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return aliyun::AliyunClient::new(store, http).list_recycle_bin();
        }
        aliyun::AliyunClient::new(store, http).directory(parent_id)
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            aliyun::AliyunClient::new(store, h).mkdir(&parent_id, &name)
        }
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        aliyun::AliyunClient::new(store, h).rename(&file_id, &name)
    }
    fn move_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            aliyun::AliyunClient::new(store, h).move_files(&file_ids, &target_parent_id)
        }
    }
    fn copy_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            aliyun::AliyunClient::new(store, h).copy_files(&file_ids, &target_parent_id)
        }
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        aliyun::AliyunClient::new(store, h).trash(&file_ids)
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client = aliyun::AliyunClient::new(store, http);
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete_recycle_entries(file_ids)
        }
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client = aliyun::AliyunClient::new(store, http);
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        }
    }
}
impl DownloadOps for AliyunDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let (url, name) = aliyun::AliyunClient::new(store, http).download(file_id)?;
            Ok(ResolvedCloudDownload {
                transform: None,
                url,
                name,
                headers: vec![("Referer".into(), "https://www.aliyundrive.com/".into())],
            })
        }
    }
}
impl ShareOps for AliyunDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        aliyun::AliyunClient::new(&self.store, &self.http).create_share(file_ids, options)
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        aliyun::AliyunClient::new(&self.store, &self.http).list_shares()
    }
    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        aliyun::AliyunClient::new(&self.store, &self.http).cancel_shares(share_ids)
    }
}
impl ProviderAuth for AliyunDriver<'_> {
    fn refresh_session(&mut self) -> Result<bool, CloudError> {
        if self.store.load("aliyun")?.is_none() {
            return Ok(false);
        }
        aliyun::AliyunClient::new(&self.store, &self.http).refresh_session()?;
        Ok(true)
    }
    fn web_authorization_recipe(&self) -> WebAuthorizationRecipe {
        WebAuthorizationRecipe {
            token_script: Some(aliyun::ALIYUN_WEB_TOKENS_JS),
            ..Default::default()
        }
    }
    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let temporary = MemoryCredentialStore::default();
            let client = aliyun::AliyunClient::new(&temporary, http);
            let session = parse_aliyun_web_session(input.storage.clone())?;
            let account = authorization_result(client.authorize_browser_session(session))?;
            store.save(
                CloudProvider::Aliyun.key(),
                &temporary
                    .load(CloudProvider::Aliyun.key())?
                    .ok_or(QuarkError::Storage)?,
            )?;
            Ok(account)
        }
    }
}
impl UploadOps for AliyunDriver<'_> {
    fn upload(
        &mut self,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        aliyun::AliyunClient::new(&self.store, &h).upload_file(parent_id, path)
    }
}
