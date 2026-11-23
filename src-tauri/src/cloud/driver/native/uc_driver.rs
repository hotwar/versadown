use super::*;
pub(super) struct UcDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}

fn with_uc_client<S, T, R>(
    store: &S,
    http: &T,
    action: impl FnOnce(&uc::UcClient<'_, T>) -> Result<R, CloudError>,
) -> Result<R, CloudError>
where
    S: CredentialStore,
    T: CloudHttpTransport + ?Sized,
{
    let client = uc::UcClient::new(load_provider_session(CloudProvider::Uc, store)?, http);
    let result = action(&client);
    // UC rotates `__puus` on ordinary API responses. Persist it even when the
    // operation itself failed so the next request does not replay a stale
    // session and misclassify an existing file as missing.
    save_provider_session(CloudProvider::Uc, store, &client.session())?;
    result
}

impl ProviderBackend for UcDriver<'_> {}
impl DriveOps for UcDriver<'_> {
    fn verified_account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Uc;
        if self.store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        let session: uc::UcSession = load_provider_session(provider, &self.store)?;
        let client = uc::UcClient::new(session, &self.http);
        let previous_account_id = client.session().account_id;
        let account_id = client
            .verified_account_id()?
            .ok_or(CloudError::InvalidResponse)?;
        let mut session = client.session();
        if previous_account_id
            .as_deref()
            .is_some_and(|previous| previous != account_id)
        {
            session.display_name = None;
        }
        session.account_id = Some(account_id);
        let account = uc::UcClient::new(session.clone(), &self.http).web_account()?;
        session.display_name = (account.display_name != uc::UC_CARD_DISPLAY_NAME)
            .then(|| account.display_name.clone());
        save_provider_session(provider, &self.store, &session)?;
        Ok(account)
    }
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Uc;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        {
            let session: uc::UcSession = load_provider_session(CloudProvider::Uc, store)?;
            let client = uc::UcClient::new(session, http);
            // Existing sessions used the rotating browser `__uid` as their
            // account binding. Ask UC for its durable user id once, then keep
            // it in the encrypted session so startup recovery survives a
            // later cookie rotation. A member-profile failure must not make a
            // previously usable card disappear.
            let previous_account_id = client.session().account_id;
            let account_id = client.verified_account_id();
            let mut session = client.session();
            let account = match account_id {
                Ok(Some(account_id)) => {
                    if previous_account_id
                        .as_deref()
                        .is_some_and(|previous| previous != account_id)
                    {
                        // The official browser switched UC users. A nickname
                        // captured for the old account must not leak onto the
                        // new account card.
                        session.display_name = None;
                    }
                    session.account_id = Some(account_id);
                    uc::UcClient::new(session.clone(), http).web_account()?
                }
                Ok(None) | Err(_) => client.web_account()?,
            };
            session.display_name = (account.display_name != uc::UC_CARD_DISPLAY_NAME)
                .then(|| account.display_name.clone());
            save_provider_session(provider, store, &session)?;
            Ok(account)
        }
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        with_uc_client(&self.store, &self.http, |client| client.storage_capacity())
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Uc;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return with_uc_client(store, http, |client| client.list_recycle_bin());
        }
        with_uc_client(store, http, |client| client.directory(parent_id))
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_uc_client(store, h, |client| {
            let created = client.mkdir(&parent_id, &name)?;
            for attempt in 0..20 {
                if let Some(directory) = client
                    .directory(&parent_id)?
                    .items
                    .into_iter()
                    .find(|file| file.kind == "folder" && file.id == created.id)
                {
                    return Ok(directory);
                }
                if attempt < 19 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
            Err(CloudError::InvalidResponse)
        })
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_uc_client(store, h, |client| client.rename(file_id, name).map(|_| ()))
    }
    fn move_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_uc_client(store, h, |client| {
            client.move_files(file_ids, target_parent_id).map(|_| ())
        })
    }
    fn copy_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_uc_client(store, h, |client| {
            client.copy_files(file_ids, target_parent_id).map(|_| ())
        })
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_uc_client(store, h, |client| client.trash(file_ids).map(|_| ()))
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        with_uc_client(store, http, |client| {
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete_recycle_entries(file_ids)
        })
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        with_uc_client(store, http, |client| {
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        })
    }
}
impl DownloadOps for UcDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        with_uc_client(store, http, |client| {
            let download = client.download(file_id)?;
            Ok(ResolvedCloudDownload {
                transform: None,
                url: download.url,
                name: download.name,
                headers: download.headers.into_iter().collect(),
            })
        })
    }
}
impl ShareOps for UcDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        with_uc_client(&self.store, &self.http, |client| {
            client.create_share(file_ids, options)
        })
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        with_uc_client(&self.store, &self.http, |client| client.list_shares())
    }
    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        with_uc_client(&self.store, &self.http, |client| {
            client.cancel_shares(share_ids)
        })
    }
}
impl ProviderAuth for UcDriver<'_> {
    fn web_authorization_recipe(&self) -> WebAuthorizationRecipe {
        WebAuthorizationRecipe {
            token_script: Some(uc::UC_WEB_PROFILE_JS),
            ..Default::default()
        }
    }

    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let session = uc::parse_uc_web_authorization(&input.cookies, &input.storage)?;
            let account = finalize_uc_web_authorization(
                crate::cloud::ProviderRegistry::global().get(CloudProvider::Uc),
                session,
                store,
                http,
            )?;
            Ok(account)
        }
    }
    fn web_session_snapshot(
        &mut self,
        input: &WebAuthorizationInput,
    ) -> Result<Option<Vec<u8>>, CloudError> {
        Ok(Some(
            serde_json::to_vec(&uc::parse_uc_web_authorization(
                &input.cookies,
                &input.storage,
            )?)
            .map_err(|_| QuarkError::Storage)?,
        ))
    }
}
impl UploadOps for UcDriver<'_> {
    fn upload(
        &mut self,
        s: &CloudState,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        s.with_upload_session(CloudProvider::Uc, &h, |store| {
            with_uc_client(store, &h, |client| client.upload_file(parent_id, path, &h))
        })
    }
}
