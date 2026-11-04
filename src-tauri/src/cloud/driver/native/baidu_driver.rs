use super::*;
pub(super) struct BaiduDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}
impl ProviderBackend for BaiduDriver<'_> {}
impl DriveOps for BaiduDriver<'_> {
    fn trash_transferred_files(&mut self, files: &[String]) -> Result<(), CloudError> {
        baidu::BaiduClient::new(
            load_provider_session(CloudProvider::Baidu, &self.store)?,
            &self.http,
        )
        .trash_transferred_files(files)
    }
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Baidu;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        {
            let session = load_provider_session(CloudProvider::Baidu, store)?;
            baidu::BaiduClient::new(session, http).account()
        }
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        baidu::BaiduClient::new(
            load_provider_session(CloudProvider::Baidu, &self.store)?,
            &self.http,
        )
        .storage_capacity()
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Baidu;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return {
                baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, http)
                    .list_recycle_bin()
            };
        }
        {
            baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, http)
                .directory(parent_id)
        }
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            let client =
                baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, h);
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
        {
            baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, h)
                .rename(&file_id, &name)
        }
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
            baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, h)
                .move_files(&file_ids, &target_parent_id)
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
            baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, h)
                .copy_files(&file_ids, &target_parent_id)
        }
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, h)
                .trash(&file_ids)
        }
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client =
                baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, http);
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete_recycle_entries(file_ids)
        }
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client =
                baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, http);
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        }
    }
}
impl DownloadOps for BaiduDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let download =
                baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, http)
                    .download(file_id)?;
            Ok(ResolvedCloudDownload {
                url: download.url,
                name: download.name,
                headers: download.headers.into_iter().collect(),
            })
        }
    }
}
impl ShareOps for BaiduDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        baidu::BaiduClient::new(
            load_provider_session(CloudProvider::Baidu, &self.store)?,
            &self.http,
        )
        .create_share(file_ids, options)
    }

    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        baidu::BaiduClient::new(
            load_provider_session(CloudProvider::Baidu, &self.store)?,
            &self.http,
        )
        .list_shares()
    }

    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        baidu::BaiduClient::new(
            load_provider_session(CloudProvider::Baidu, &self.store)?,
            &self.http,
        )
        .cancel_shares(share_ids)
    }
}
impl ProviderAuth for BaiduDriver<'_> {
    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            // While the official login page is open, an absent login cookie
            // means pending authorization, not an expired saved account.
            let session = authorization_result(baidu::parse_baidu_web_session(&input.cookies))?;
            let account = finalize_baidu_web_authorization(
                crate::cloud::ProviderRegistry::global().get(CloudProvider::Baidu),
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
        let provider = CloudProvider::Baidu;
        let store = &self.store;
        let http = &self.http;
        let native_result = if browser_password_login_preferred(store, provider)? {
            Err(QuarkError::WebAuthorizationRequired)
        } else {
            baidu::BaiduClient::login_password(username, password, http)
        };
        complete_native_password_login(native_result, |session| {
            let client = baidu::BaiduClient::new(session.clone(), http);
            let account = client.account()?;
            client.directory(ProviderRegistry::global().get(provider).root_directory_id)?;
            save_provider_session(CloudProvider::Baidu, store, &session)?;
            set_browser_password_login_preferred(store, provider, false)?;
            Ok(account)
        })
    }
    fn recover_password(
        &mut self,
        credential: &CloudPasswordCredential,
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        let provider = CloudProvider::Baidu;
        let store = &self.store;
        let http = &self.http;
        Ok({
            let session = match baidu::BaiduClient::login_password(
                credential.username(),
                credential.password(),
                http,
            ) {
                Ok(session) => session,
                Err(
                    error @ (QuarkError::WebAuthorizationRequired | QuarkError::CaptchaRequired),
                ) => {
                    set_browser_password_login_preferred(store, provider, true)?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            set_browser_password_login_preferred(store, provider, false)?;
            let client = baidu::BaiduClient::new(session.clone(), http);
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
        _browser_context: Option<&serde_json::Value>,
        report_stage: &mut dyn FnMut(&'static str),
    ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
        let provider = CloudProvider::Baidu;
        let http = &self.http;
        Ok({
            let mut session = baidu::parse_baidu_web_session(cookies)?;
            session.auth_method = CloudAuthMethod::Password;
            let client = baidu::BaiduClient::new(session.clone(), http);
            report_stage("账号信息校验");
            let account = client.account()?;
            report_stage("目录访问校验");
            client.directory(ProviderRegistry::global().get(provider).root_directory_id)?;
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
        Ok(Some(
            serde_json::to_vec(&baidu::parse_baidu_web_session(&cookies)?)
                .map_err(|_| QuarkError::Storage)?,
        ))
    }
}
impl UploadOps for BaiduDriver<'_> {
    fn upload(
        &mut self,
        s: &CloudState,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        s.with_upload_session(CloudProvider::Baidu, &h, |store| {
            baidu::BaiduClient::new(load_provider_session(CloudProvider::Baidu, store)?, &h)
                .upload_file(&parent_id, &path, &h)
        })
    }
}
