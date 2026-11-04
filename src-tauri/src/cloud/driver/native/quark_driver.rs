use super::*;
pub(super) struct QuarkDriver<'a> {
    pub(super) store: StoreRef<'a>,
    pub(super) http: HttpRef<'a>,
}
impl ProviderBackend for QuarkDriver<'_> {}
impl DriveOps for QuarkDriver<'_> {
    fn account(&mut self) -> Result<CloudAccount, CloudError> {
        let provider = CloudProvider::Quark;
        let store = &self.store;
        let http = &self.http;
        if store.load(provider.key())?.is_none() {
            return Err(CloudError::NotAuthorized);
        }
        QuarkClient::new(cookie(store)?, http).account()
    }
    fn storage_capacity(&mut self) -> Result<CloudStorageCapacity, CloudError> {
        QuarkClient::new(cookie(&self.store)?, &self.http).storage_capacity()
    }
    fn list_directory(&mut self, parent_id: &str) -> Result<CloudDirectory, CloudError> {
        let provider = CloudProvider::Quark;
        let store = &self.store;
        let http = &self.http;
        if is_provider_recycle_bin(provider, parent_id) {
            return QuarkClient::new(cookie(store)?, http).list_recycle_bin();
        }
        QuarkClient::new(cookie(store)?, http).directory(parent_id)
    }
    fn create_directory(&mut self, parent_id: &str, name: &str) -> Result<CloudFile, CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        {
            let v = response_json(&QuarkClient::new(cookie(store)?, h).mkdir(&parent_id, &name)?)?;
            Ok(CloudFile {
                id: required(&v["data"], "fid")?,
                parent_id: parent_id.to_owned(),
                name: name.to_owned(),
                kind: "folder".into(),
                size: None,
                modified_at: None,
            })
        }
    }
    fn rename(&mut self, file_id: &str, name: &str) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        QuarkClient::new(cookie(store)?, h)
            .rename(&file_id, &name)
            .map(|_| ())
    }
    fn move_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        QuarkClient::new(cookie(store)?, h)
            .move_files(&file_ids, &target_parent_id)
            .map(|_| ())
    }
    fn copy_files(
        &mut self,
        file_ids: &[String],
        target_parent_id: &str,
    ) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        QuarkClient::new(cookie(store)?, h)
            .copy_files(&file_ids, &target_parent_id)
            .map(|_| ())
    }
    fn trash(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        let h = http;
        QuarkClient::new(cookie(store)?, h)
            .trash(&file_ids)
            .map(|_| ())
    }
    fn delete(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client = QuarkClient::new(cookie(store)?, http);
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.delete_recycle_entries(file_ids)
        }
    }
    fn restore(&mut self, file_ids: &[String]) -> Result<(), CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let client = QuarkClient::new(cookie(store)?, http);
            let recycle_bin = client.list_recycle_bin()?;
            validate_recycle_entry_ids(file_ids, &recycle_bin)?;
            client.restore_recycle_entries(file_ids)
        }
    }
}
impl DownloadOps for QuarkDriver<'_> {
    fn resolve_download(&mut self, file_id: &str) -> Result<ResolvedCloudDownload, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            let cookie = cookie(store)?;
            let result =
                QuarkClient::new(cookie.clone(), http).download_url_with_cookie(file_id)?;
            let v = response_json(&result.response)?;
            let url = parse_quark_download_url(&result.response.body)?;
            persist_quark_download_cookies(store, &cookie, &result)?;
            Ok(ResolvedCloudDownload {
                url,
                name: v
                    .pointer("/data/0/file_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(file_id)
                    .into(),
                // The file-transfer profile is selected when resolving the
                // signed URL, and these same headers must accompany aria2's
                // CDN request for the server to retain it.
                headers: quark_download_headers(&result.cookie, &[]),
            })
        }
    }
}
impl ShareOps for QuarkDriver<'_> {
    fn create_share(
        &mut self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, CloudError> {
        QuarkClient::new(cookie(&self.store)?, &self.http).create_share(file_ids, options)
    }
    fn list_shares(&mut self) -> Result<Vec<CloudShare>, CloudError> {
        QuarkClient::new(cookie(&self.store)?, &self.http).list_shares()
    }
    fn cancel_shares(&mut self, share_ids: &[String]) -> Result<(), CloudError> {
        QuarkClient::new(cookie(&self.store)?, &self.http).cancel_shares(share_ids)
    }
}
impl ProviderAuth for QuarkDriver<'_> {
    fn complete_web(&mut self, input: &WebAuthorizationInput) -> Result<CloudAccount, CloudError> {
        let store = &self.store;
        let http = &self.http;
        {
            // Must stay on the blocking worker, never the Windows UI thread (WebView2 deadlock).
            let cookie = input.cookies.join("; ");
            if cookie.is_empty() {
                return Err(QuarkError::AuthorizationPending);
            }
            let account = authorization_result(QuarkClient::new(cookie.clone(), http).account())?;
            store.save("quark", cookie.as_bytes())?;
            Ok(account)
        }
    }
    fn web_session_snapshot(
        &mut self,
        input: &WebAuthorizationInput,
    ) -> Result<Option<Vec<u8>>, CloudError> {
        let cookies = &input.cookies;
        Ok(Some({
            let cookie = cookies.join("; ");
            if cookie.is_empty() {
                return Err(QuarkError::WebAuthorizationRequired);
            }
            cookie.into_bytes()
        }))
    }
}
impl UploadOps for QuarkDriver<'_> {
    fn upload(
        &mut self,
        s: &CloudState,
        upload_http: &dyn UploadHttp,
        parent_id: &str,
        path: &Path,
    ) -> Result<CloudFile, CloudError> {
        let h = UploadHttpRef(upload_http);
        s.with_upload_session(CloudProvider::Quark, &h, |store| {
            QuarkClient::new(cookie(store)?, &h).upload_file(&parent_id, &path, &h)
        })
    }
}
