use super::*;
pub(super) struct PikpakDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}
impl ProviderBackend for PikpakDriver<'_> {}
impl DriveOps for PikpakDriver<'_> {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Pikpak;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        PikPakClient::new(http, store).account()
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        PikPakClient::new(&self.http, &self.store).storage_capacity()
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Pikpak;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return PikPakClient::new(http, store).list(
                ProviderRegistry::global()
                    .get(provider)
                    .recycle_bin_directory_id,
            );
        }
        PikPakClient::new(http, store).list(parent_id)
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        PikPakClient::new(h, store).mkdir(&parent_id, &name)
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        PikPakClient::new(h, store).rename(&file_id, &name)
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
            PikPakClient::new(h, store).move_files(&file_ids, &target_parent_id)
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
            PikPakClient::new(h, store).copy_files(&file_ids, &target_parent_id)
        }
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        PikPakClient::new(h, store).trash(&file_ids)
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let provider = CloudProvider::Pikpak;
        let store = &self.store;
        let http = &self.http;
        {
            let client = PikPakClient::new(http, store);
            let recycle_bin = client.list(
                ProviderRegistry::global()
                    .get(provider)
                    .recycle_bin_directory_id,
            )?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete(file_ids)
        }
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let provider = CloudProvider::Pikpak;
        let store = &self.store;
        let http = &self.http;
        {
            let client = PikPakClient::new(http, store);
            let recycle_bin = client.list(
                ProviderRegistry::global()
                    .get(provider)
                    .recycle_bin_directory_id,
            )?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        }
    }
}
impl DownloadOps for PikpakDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let (url, name) = PikPakClient::new(http, store).download(file_id)?;
            Ok(ResolvedCloudDownload {
                transform: None,
                url,
                name,
                headers: Vec::new(),
            })
        }
    }
}
impl ShareOps for PikpakDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        PikPakClient::new(&self.http, &self.store).create_share(file_ids, options)
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        PikPakClient::new(&self.http, &self.store).list_shares()
    }
    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        PikPakClient::new(&self.http, &self.store).cancel_shares(share_ids)
    }
    fn update_share_pass_code(
        &mut self,
        share_id: &str,
        pass_code: &str,
    ) -> Result<(), CloudError> {
        PikPakClient::new(&self.http, &self.store).update_share_pass_code(share_id, pass_code)
    }
}
impl ProviderAuth for PikpakDriver<'_> {
    fn refresh_session(&mut self) -> Result<bool, CloudError> {
        if self.store.load("pikpak")?.is_none() {
            return Ok(false);
        }
        PikPakClient::new(&self.http, &self.store).refresh_session()?;
        Ok(true)
    }
    fn web_authorization_recipe(&self) -> WebAuthorizationRecipe {
        WebAuthorizationRecipe {
            token_script: Some(pikpak::PIKPAK_WEB_TOKENS_JS),
            ..Default::default()
        }
    }
    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Pikpak;
        let store = &self.store;
        let http = &self.http;
        {
            let session = parse_web_tokens(&input.storage.clone().to_string())?;
            // Validate in a temporary store before replacing a previously authorized account.
            let temporary = MemoryCredentialStore::default();
            let client = PikPakClient::new(http, &temporary);
            client.save_session(&session)?;
            let account = authorization_result(client.account())?;
            select_password_credential_account(store, provider, &account.id)?;
            store.save(
                "pikpak",
                &temporary.load("pikpak")?.ok_or(QuarkError::Storage)?,
            )?;
            Ok(account)
        }
    }
    fn login_password(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<NativeLoginOutcome, CloudError> {
        let provider = CloudProvider::Pikpak;
        let store = &self.store;
        let http = &self.http;
        let account = {
            let temporary = MemoryCredentialStore::default();
            let client = PikPakClient::new(http, &temporary);
            client.login(&username, &password)?;
            let account = client.account()?;
            client.list(ProviderRegistry::global().get(provider).root_directory_id)?;
            store.save(
                "pikpak",
                &temporary.load("pikpak")?.ok_or(QuarkError::Storage)?,
            )?;
            account
        };
        Ok(NativeLoginOutcome::Authenticated(account))
    }
    fn recover_password(
        &mut self,
        credential: &CloudPasswordCredential,
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        let provider = CloudProvider::Pikpak;
        let http = &self.http;
        Ok({
            let temporary = MemoryCredentialStore::default();
            if let Some(previous) = self.store.load("pikpak")? {
                temporary.save("pikpak", &previous)?;
            }
            let client = PikPakClient::new(http, &temporary);
            client.login(credential.username(), credential.password())?;
            let account = client.account()?;
            client.list(ProviderRegistry::global().get(provider).root_directory_id)?;
            let session = temporary
                .load(CloudProvider::Pikpak.key())?
                .ok_or(QuarkError::Storage)?;
            (account, session)
        })
    }
}
impl UploadOps for PikpakDriver<'_> {
    fn upload(
        &mut self,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        PikPakClient::new(&h, &self.store).upload_file(parent_id, path)
    }
}
