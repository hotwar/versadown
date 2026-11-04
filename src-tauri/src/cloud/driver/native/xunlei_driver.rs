use super::*;
pub(super) struct XunleiDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}
impl ProviderBackend for XunleiDriver<'_> {}

fn append_native_download_markers(
    headers: &mut Vec<(String, String)>,
    download: &xunlei::XunleiDownload,
) {
    if let Some(token) = &download.acceleration_token {
        headers.push((
            crate::xunlei_native::ACCELERATION_TOKEN_MARKER_HEADER.into(),
            token.clone(),
        ));
    }
    if let Some(size) = download.size {
        headers.push((
            crate::xunlei_native::CONTENT_LENGTH_MARKER_HEADER.into(),
            size.to_string(),
        ));
    }
    if let Some(cid) = &download.cid {
        headers.push((crate::xunlei_native::CID_MARKER_HEADER.into(), cid.clone()));
    }
    if let Some(gcid) = &download.gcid {
        headers.push((
            crate::xunlei_native::GCID_MARKER_HEADER.into(),
            gcid.clone(),
        ));
    }
}

impl DriveOps for XunleiDriver<'_> {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Xunlei;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        with_xunlei_client(store, http, |client| {
            if client.session().profile.is_modern_web()
                || (client.session().auth_method == CloudAuthMethod::Web
                    && client.session().profile.client_secret.is_empty())
            {
                // Validate the root before restoring the workspace. The helper
                // also preserves modern refresh rotation when this probe fails.
                client.directory(ProviderRegistry::global().get(provider).root_directory_id)?;
                return client.web_account();
            }
            client.account()
        })
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        with_xunlei_client(&self.store, &self.http, |client| client.storage_capacity())
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Xunlei;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return with_xunlei_client(store, http, |client| client.list_recycle_bin());
        }
        {
            with_xunlei_client(store, http, |client| client.directory(parent_id))
        }
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_xunlei_client(store, h, |client| {
            let previous_ids = client
                .directory(&parent_id)?
                .items
                .into_iter()
                .map(|file| file.id)
                .collect();
            client.mkdir(&parent_id, &name)?;
            find_created_directory(client.directory(&parent_id)?, &name, &previous_ids)
        })
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            with_xunlei_client(store, h, |client| client.rename(&file_id, &name))
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
        with_xunlei_client(store, h, |client| {
            client.move_files(&file_ids, &target_parent_id)
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
        with_xunlei_client(store, h, |client| {
            client.copy_files(&file_ids, &target_parent_id)
        })
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        with_xunlei_client(store, h, |client| client.trash(&file_ids))
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        with_xunlei_client(store, http, |client| {
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete_recycle_entries(file_ids)
        })
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        with_xunlei_client(store, http, |client| {
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        })
    }
}
impl DownloadOps for XunleiDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        with_xunlei_client(store, http, |client| {
            let download = client.download_with_acceleration(file_id)?;
            // Keep the authenticated browser's public identity on the CDN
            // request, but never forward bearer/captcha/cookies off the API.
            let session = client.session();
            let mut headers = Vec::new();
            if !session.profile.user_agent.is_empty() {
                headers.push(("User-Agent".into(), session.profile.user_agent.clone()));
            }
            if session.profile.is_modern_web() {
                headers.push(("Referer".into(), "https://pan.xunlei.com/".into()));
            }
            headers.extend(xunlei::xunlei_native_identity_markers(&session));
            append_native_download_markers(&mut headers, &download);
            Ok(ResolvedCloudDownload {
                url: download.url,
                name: download.name,
                headers,
            })
        })
    }
}
impl ShareOps for XunleiDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        with_xunlei_client(&self.store, &self.http, |client| {
            client.create_share(file_ids, options)
        })
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        with_xunlei_client(&self.store, &self.http, |client| client.list_shares())
    }
    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        let outcome = self.cancel_shares_detailed(share_ids)?;
        match outcome.failures.first() {
            None => Ok(()),
            Some(failure) => Err(CloudError::Provider(failure.message.clone())),
        }
    }
    fn cancel_shares_detailed(
        &mut self,
        share_ids: &[String],
    ) -> Result<CloudShareCancelOutcome, CloudError> {
        with_xunlei_client(&self.store, &self.http, |client| {
            client.cancel_shares(share_ids)
        })
    }
}
impl ProviderAuth for XunleiDriver<'_> {
    fn login_password(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<NativeLoginOutcome, CloudError> {
        if username.trim().is_empty() || password.is_empty() {
            return Err(CloudError::InvalidInput("请输入账号和密码".into()));
        }
        // Xunlei's official page owns the password exchange and any security
        // challenge. Tell the shared login flow to open that trusted WebView
        // instead of treating this supported mode as unavailable.
        Ok(NativeLoginOutcome::NeedBrowserFallback(
            CloudError::WebAuthorizationRequired,
        ))
    }
    fn refresh_session(&mut self) -> Result<bool, CloudError> {
        if self.store.load("xunlei")?.is_none() {
            return Ok(false);
        }
        let session = load_provider_session(CloudProvider::Xunlei, &self.store)?;
        let client = xunlei::XunleiClient::new(session, &self.http);
        let session = client.refresh_session()?;
        save_provider_session(CloudProvider::Xunlei, &self.store, &session)?;
        Ok(true)
    }
    fn web_authorization_recipe(&self) -> WebAuthorizationRecipe {
        WebAuthorizationRecipe {
            token_script: Some(super::xunlei_auth::SNAPSHOT),
            ..Default::default()
        }
    }
    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        super::xunlei_auth::complete(input, &self.store, &self.http)
    }
}
impl UploadOps for XunleiDriver<'_> {
    fn upload(
        &mut self,
        s: &CloudState,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        s.with_upload_session(CloudProvider::Xunlei, &h, |store| {
            with_xunlei_client(store, &h, |client| client.upload_file(&parent_id, &path))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_markers_preserve_the_file_acceleration_token() {
        let download = xunlei::XunleiDownload {
            url: "https://cdn.example/file".into(),
            name: "file.zip".into(),
            acceleration_token: Some("file-acceleration-token".into()),
            cid: Some("cid".into()),
            gcid: Some("gcid".into()),
            size: Some(42),
        };
        let mut headers = Vec::new();

        append_native_download_markers(&mut headers, &download);

        assert!(headers.iter().any(|(name, value)| {
            name == crate::xunlei_native::ACCELERATION_TOKEN_MARKER_HEADER
                && value == "file-acceleration-token"
        }));
    }
}
