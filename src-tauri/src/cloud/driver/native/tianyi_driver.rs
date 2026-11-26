use super::*;
pub(super) struct TianyiDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}
impl ProviderBackend for TianyiDriver<'_> {}
impl DriveOps for TianyiDriver<'_> {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Tianyi;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        {
            let session: tianyi::TianyiSession =
                load_provider_session(CloudProvider::Tianyi, store)?;
            let client = tianyi::TianyiClient::new(session.clone(), http);
            if session.auth_method == CloudAuthMethod::Web {
                client.web_account()
            } else {
                client.account()
            }
        }
    }
    fn verified_account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Tianyi;
        if self.store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        let session: tianyi::TianyiSession = load_provider_session(provider, &self.store)?;
        // The card may deliberately use the generic `tianyi-web` identity,
        // but a download owner must come from Tianyi's authenticated profile.
        tianyi::TianyiClient::new(session, &self.http).account()
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        tianyi::TianyiClient::new(
            load_provider_session(CloudProvider::Tianyi, &self.store)?,
            &self.http,
        )
        .storage_capacity()
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Tianyi;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return {
                tianyi::TianyiClient::new(
                    load_provider_session(CloudProvider::Tianyi, store)?,
                    http,
                )
                .list_recycle_bin()
            };
        }
        {
            tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, store)?, http)
                .directory(parent_id)
        }
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            let client =
                tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, store)?, h);
            let previous_ids = client
                .directory(&parent_id)?
                .items
                .into_iter()
                .map(|file| file.id)
                .collect();
            client.mkdir(&parent_id, &name)?;
            find_created_directory(client.directory(&parent_id)?, &name, &previous_ids)
        }
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, store)?, h)
            .rename(&file_id, &name)
    }
    fn move_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, store)?, h)
            .move_files(&file_ids, &target_parent_id)
    }
    fn copy_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, store)?, h)
            .copy_files(&file_ids, &target_parent_id)
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, store)?, h)
                .trash(&file_ids)
        }
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client = tianyi::TianyiClient::new(
                load_provider_session(CloudProvider::Tianyi, store)?,
                http,
            );
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete_recycle_entries(file_ids)
        }
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client = tianyi::TianyiClient::new(
                load_provider_session(CloudProvider::Tianyi, store)?,
                http,
            );
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        }
    }
}
impl DownloadOps for TianyiDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let download = tianyi::TianyiClient::new(
                load_provider_session(CloudProvider::Tianyi, store)?,
                http,
            )
            .download(file_id)?;
            Ok(ResolvedCloudDownload {
                transform: None,
                url: download.url,
                name: download.name,
                headers: download.headers.into_iter().collect(),
            })
        }
    }
}
impl ShareOps for TianyiDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        tianyi::TianyiClient::new(
            load_provider_session(CloudProvider::Tianyi, &self.store)?,
            &self.http,
        )
        .create_share(file_ids, options)
    }

    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        tianyi::TianyiClient::new(
            load_provider_session(CloudProvider::Tianyi, &self.store)?,
            &self.http,
        )
        .list_shares()
    }

    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        tianyi::TianyiClient::new(
            load_provider_session(CloudProvider::Tianyi, &self.store)?,
            &self.http,
        )
        .cancel_shares(share_ids)
    }
}
impl ProviderAuth for TianyiDriver<'_> {
    fn refresh_session(&mut self) -> Result<bool, CloudError> {
        if self.store.load("tianyi")?.is_none() {
            return Ok(false);
        }
        let mut session: tianyi::TianyiSession =
            load_provider_session(CloudProvider::Tianyi, &self.store)?;
        let Some(app) = session.app.as_ref() else {
            return Ok(false);
        };
        session.app = Some(app.refresh(&self.http)?);
        save_provider_session(CloudProvider::Tianyi, &self.store, &session)?;
        Ok(true)
    }
    fn web_authorization_recipe(&self) -> WebAuthorizationRecipe {
        WebAuthorizationRecipe {
            browser_context: true,
            ..Default::default()
        }
    }
    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let mut session = tianyi::parse_tianyi_web_session(&input.cookies)?;
            session.apply_browser_context(&input.browser_context.clone().unwrap_or_default());
            if !session.has_browser_context() {
                return Err(QuarkError::AuthorizationPending);
            }
            if let Some(existing) = store
                .load(CloudProvider::Tianyi.key())?
                .and_then(|bytes| serde_json::from_slice::<tianyi::TianyiSession>(&bytes).ok())
            {
                session.retain_app_transport(&existing);
            }
            let account = finalize_tianyi_web_authorization(
                crate::cloud::ProviderRegistry::global().get(CloudProvider::Tianyi),
                session,
                store,
                http,
            )?;
            Ok(account)
        }
    }
    fn login_password(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<NativeLoginOutcome, CloudError> {
        let provider = CloudProvider::Tianyi;
        let store = &self.store;
        let http = &self.http;
        let native_result = tianyi::TianyiClient::login_password(username, password, http);
        complete_native_password_login(native_result, |mut session| {
            if let Some(existing) = store
                .load(CloudProvider::Tianyi.key())?
                .and_then(|bytes| serde_json::from_slice::<tianyi::TianyiSession>(&bytes).ok())
            {
                session.retain_web_transport(&existing);
            }
            let client = tianyi::TianyiClient::new(session.clone(), http);
            let account = client.account()?;
            client.directory(ProviderRegistry::global().get(provider).root_directory_id)?;
            save_provider_session(CloudProvider::Tianyi, store, &session)?;
            Ok(account)
        })
    }
    fn recover_password(
        &mut self,
        credential: &CloudPasswordCredential,
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        let store = &self.store;
        let http = &self.http;
        Ok({
            let mut session = tianyi::TianyiClient::login_password(
                credential.username(),
                credential.password(),
                http,
            )?;
            if let Some(existing) = store
                .load(CloudProvider::Tianyi.key())?
                .and_then(|bytes| serde_json::from_slice::<tianyi::TianyiSession>(&bytes).ok())
            {
                session.retain_web_transport(&existing);
            }
            let client = tianyi::TianyiClient::new(session.clone(), http);
            let account = client.account()?;
            (
                account,
                serde_json::to_vec(&session).map_err(|_| CloudError::Storage)?,
            )
        })
    }
    fn validate_browser_password(
        &mut self,
        cookies: &[String],
        browser_context: Option<&serde_json::Value>,
        report_stage: &mut dyn FnMut(&'static str),
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        let provider = CloudProvider::Tianyi;
        let store = &self.store;
        let http = &self.http;
        Ok({
            let mut session = tianyi::parse_tianyi_web_session(cookies)?;
            if let Some(context) = browser_context {
                session.apply_browser_context(context);
            }
            if !session.has_browser_context() {
                report_stage("等待官网浏览器标识");
                return Err(QuarkError::AuthorizationPending);
            }
            session.auth_method = CloudAuthMethod::Password;
            let web_client = tianyi::TianyiClient::new(session.clone(), http);
            report_stage("账号信息校验");
            let web_account = web_client.account()?;
            report_stage("目录访问校验");
            web_client.directory(ProviderRegistry::global().get(provider).root_directory_id)?;
            if let Some(existing) = store
                .load(CloudProvider::Tianyi.key())?
                .and_then(|bytes| serde_json::from_slice::<tianyi::TianyiSession>(&bytes).ok())
            {
                session.retain_app_transport(&existing);
            }
            let client = tianyi::TianyiClient::new(session.clone(), http);
            let account = if session.app.is_some() {
                let stable = client.account()?;
                if stable.id != web_account.id {
                    return Err(QuarkError::InvalidInput(
                        "登录账号与原任务账号不一致，已保留原凭据，请使用原账号登录".into(),
                    ));
                }
                stable
            } else {
                web_account
            };
            (
                account,
                serde_json::to_vec(&session).map_err(|_| QuarkError::Storage)?,
            )
        })
    }
    fn web_session_snapshot(
        &mut self,
        input: &WebAuthorizationInput,
    ) -> Result<Option<Vec<u8>>, CloudError> {
        let cookies = &input.cookies;
        Ok(Some({
            let mut session = tianyi::parse_tianyi_web_session(&cookies)?;
            session.apply_browser_context(&input.browser_context.clone().unwrap_or_default());
            if let Some(existing) = self
                .store
                .load(CloudProvider::Tianyi.key())?
                .and_then(|bytes| serde_json::from_slice::<tianyi::TianyiSession>(&bytes).ok())
            {
                session.retain_app_transport(&existing);
            }
            serde_json::to_vec(&session).map_err(|_| QuarkError::Storage)?
        }))
    }
}
impl UploadOps for TianyiDriver<'_> {
    fn upload_no_replace(&mut self, _: &dyn UploadHttp, _: &str, _: &Path) -> Result<CloudFile, CloudError> {
        // The disabled connector has no verified no-replacement contract.
        Err(CloudError::ProviderUnsupported)
    }

    fn upload(
        &mut self,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        tianyi::TianyiClient::new(load_provider_session(CloudProvider::Tianyi, &self.store)?, &h)
            .upload_file(parent_id, path, &h)
    }
}
