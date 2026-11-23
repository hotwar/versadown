use super::{
    tianyi_app::{self, TianyiAppSession},
    types::{download_source_missing_from_http_404, encoded, required},
    CloudAccount, CloudAuthMethod, CloudDirectory, CloudFile, CloudFileKind, CloudHttpRequest,
    CloudHttpResponse, CloudHttpTransport, CloudProvider, CloudShare, CloudShareOptions,
    CloudStorageCapacity, CloudUploadTransport, QuarkError, UploadHttpRequest, UploadHttpResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const CLOUD: &str = "https://cloud.189.cn";
const CLOUD_HOST: &str = "cloud.189.cn";
pub(crate) const BROWSER_ORIGIN: &str = CLOUD;
pub(crate) const BROWSER_CONTEXT_READER_JS: &str =
    "location.origin === 'https://cloud.189.cn' ? (window.__versadownTianyiContext || {}) : {}";

pub(crate) fn accepts_browser_api_request(raw_url: &str) -> bool {
    validate_browser_api_request("GET", raw_url).is_ok()
}

pub(crate) fn validate_browser_api_request(
    method: &str,
    raw_url: &str,
) -> Result<url::Url, QuarkError> {
    if !matches!(method, "GET" | "POST") {
        return Err(QuarkError::InvalidInput(
            "天翼授权页请求方法不受支持".into(),
        ));
    }
    let url = url::Url::parse(raw_url).map_err(|_| QuarkError::InvalidResponse)?;
    let allowed_path = url.path().starts_with("/api/")
        || url.path().starts_with("/v2/")
        || url.path() == "/main.action";
    if url.scheme() != "https"
        || url.host_str() != Some(CLOUD_HOST)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !allowed_path
    {
        return Err(QuarkError::InvalidInput(
            "天翼授权页请求地址不受支持".into(),
        ));
    }
    Ok(url)
}
pub const TIANYI_RECYCLE_BIN_ID: &str = "__versa_down_tianyi_recycle_bin__";
const TIANYI_RECYCLE_BIN_NAME: &str = "回收站";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{
        share::{test_support::*, *},
        CredentialStore, MemoryCredentialStore, ProviderRegistry,
    };

    #[test]
    fn personal_share_record_normalizes_tianyi_link_and_access_code() {
        let share = tianyi_share_from_value(
            &json!({
                "shareId": 42,
                "accessURL": "//cloud.189.cn/t/example",
                "accessCode": "a7b2",
                "fileId": 9,
                "fileName": "归档",
                "isFolder": true,
                "shareTime": 1_789_000_000_000i64,
            }),
            None,
            None,
        )
        .unwrap();

        assert_eq!(share.id, "42");
        assert_eq!(share.url, "https://cloud.189.cn/t/example");
        assert_eq!(share.name, "归档");
        assert_eq!(share.kind, CloudFileKind::Folder);
        assert_eq!(share.source_file_ids, vec!["9"]);
        assert_eq!(share.pass_code.as_deref(), Some("a7b2"));
    }

    #[test]
    fn personal_share_requests_match_the_tianyi_web_protocol() {
        let http = Http::new(vec![
            json!({
                "res_code": 0,
                "shareLinkList": [{
                    "shareId": 42,
                    "accessURL": "//cloud.189.cn/t/example",
                    "accessCode": "a7b2",
                    "fileId": "file-1",
                    "fileName": "归档",
                    "isFolder": true,
                    "shareTime": 1_789_000_000_000i64
                }]
            }),
            json!({"res_code": 0, "data": []}),
            json!({"res_code": 0, "success": true}),
        ]);
        let client = TianyiClient::new(TianyiSession::cookie("COOKIE_LOGIN_USER=user"), &http);

        let share = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "归档".into(),
                    pass_code: None,
                    generate_pass_code: true,
                    expires_in_days: Some(7),
                },
            )
            .unwrap();
        assert_eq!(share.url, "https://cloud.189.cn/t/example");
        assert!(client.list_shares().unwrap().is_empty());
        client.cancel_shares(&["42".into(), "84".into()]).unwrap();

        let requests = http.requests.lock().unwrap();
        assert!(requests[0].url.contains(
            "/api/open/share/createShareLink.action?fileId=file-1&expireTime=7&shareType=3"
        ));
        assert!(requests[1]
            .url
            .contains("/api/portal/listShares.action?shareType=1&pageNum=1&pageSize=60"));
        assert!(requests[2]
            .url
            .contains("/api/portal/cancelShare.action?shareIdList=42%2C84&ancelType=1"));
    }
    fn credentials() -> MemoryCredentialStore {
        let store = MemoryCredentialStore::default();
        store
            .save(
                "tianyi",
                &serde_json::to_vec(&TianyiSession::cookie(
                    "COOKIE_LOGIN_USER=user; SESSION=old",
                ))
                .unwrap(),
            )
            .unwrap();
        store
    }
    fn info() -> Value {
        json!({"res_code":0,"shareId":22,"fileId":"root","fileName":"root","isFolder":true,"needAccessCode":1,"shareMode":1})
    }
    fn checked() -> Value {
        json!({"res_code":0,"shareId":22})
    }

    #[test]
    fn tianyi_browser_context_survives_storage_and_is_used_for_api_and_share() {
        let mut session = TianyiSession::cookie("SSON=session");
        session.apply_browser_context(&json!({
            "browser_id":"1234567890abcdef1234567890abcdef", "user_agent":"OfficialBrowser/140"
        }));
        let store = MemoryCredentialStore::default();
        store
            .save("tianyi", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        let restored: TianyiSession =
            serde_json::from_slice(&store.load("tianyi").unwrap().unwrap()).unwrap();
        let http = Http::new(vec![]);
        let client = TianyiClient::new(restored, &http);
        let adapter = TianyiShareAdapter {
            credentials: &store,
            http: &http,
        };
        for headers in [client.headers(), adapter.session().unwrap().headers()] {
            assert_eq!(headers["Browser-Id"], "1234567890abcdef1234567890abcdef");
            assert_eq!(headers["User-Agent"], "OfficialBrowser/140");
            assert_eq!(headers["Cookie"], "SSON=session");
        }
    }

    #[test]
    fn tianyi_browser_context_accepts_legacy_sessions_and_rejects_header_injection() {
        let mut session: TianyiSession =
            serde_json::from_value(json!({"cookie":"SSON=old"})).unwrap();
        session.apply_browser_context(
            &json!({"browser_id":"bad\r\nHeader: value", "user_agent":"bad\r\nHeader: value"}),
        );
        let http = Http::new(vec![]);
        let headers = TianyiClient::new(session, &http).headers();
        assert!(!headers.contains_key("Browser-Id"));
        assert_eq!(headers["User-Agent"], TIANYI_WEB_USER_AGENT);
    }

    #[test]
    fn tianyi_storage_capacity_converts_available_space() {
        let store = credentials();
        let http = Http::new(vec![json!({
            "res_code": 0,
            "data": {"capacity": "1000", "available": "350"}
        })]);
        let capacity = ProviderRegistry::global()
            .driver(CloudProvider::Tianyi, &store, &http)
            .storage_capacity()
            .unwrap();
        assert_eq!(
            capacity,
            CloudStorageCapacity::Limited {
                total_bytes: 1000,
                used_bytes: 650,
            }
        );
        let requests = http.requests.lock().unwrap();
        assert!(requests[0]
            .url
            .contains("/api/open/user/getUserInfoForPortal.action"));
    }

    #[test]
    fn tianyi_web_session_keeps_cloud_scoped_cookie_needed_by_share_downloads() {
        // Break caught: list operations can authenticate with the SSO cookie
        // alone while the share-download endpoint also requires a cloud-host
        // session cookie issued by the official page.
        let session = parse_tianyi_web_session(&[
            "COOKIE_LOGIN_USER=authorized-user".into(),
            "JSESSIONID=cloud-download-session".into(),
        ])
        .expect("an official Tianyi web session");

        assert!(session.cookie.contains("COOKIE_LOGIN_USER=authorized-user"));
        assert!(session.cookie.contains("JSESSIONID=cloud-download-session"));
    }

    #[test]
    fn tianyi_share_prompts_for_access_code_before_requiring_hidden_share_id() {
        let store = credentials();
        // The live endpoint omits shareId until an access code has been
        // supplied. The user should still receive the access-code prompt.
        let protected = json!({
            "res_code": 0,
            "fileId": "protected-file",
            "fileName": "protected.zip",
            "isFolder": false,
            "needAccessCode": 1,
            "shareMode": 1
        });
        let http = Http::new(vec![protected]);
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;

        let result = provider_share_adapter(CloudProvider::Tianyi, &store, &http)
            .unwrap()
            .inspect(&input);

        assert!(matches!(result, Ok(ShareInspection::AccessCodeRequired)));
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn tianyi_share_recovers_its_web_session_from_a_signed_app_session() {
        // Break caught: rejecting an empty Cookie here makes a valid native
        // App session work for personal files but not for the same user's
        // Tianyi share links.
        let store = MemoryCredentialStore::default();
        store
            .save(
                "tianyi",
                br#"{"cookie":"","auth_method":"password","app":{"sessionKey":"app-key","sessionSecret":"app-secret","accessToken":"access-token","loginName":"alice@189.cn"}}"#,
            )
            .unwrap();
        let http = Http::with_cookies(
            vec![
                json!({"res_code": 0}),
                json!({
                    "res_code": 0,
                    "shareId": "share-id",
                    "fileId": "524751232948846473",
                    "fileName": "05_如何制作动感影集.mp4",
                    "isFolder": false,
                    "needAccessCode": 0,
                    "shareMode": 1
                }),
            ],
            vec![
                "COOKIE_LOGIN_USER=recovered; Path=/; Secure".into(),
                "JSESSIONID=share-session; Path=/; Secure".into(),
            ],
        );
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;

        let ShareInspection::Plan(plan) =
            provider_share_adapter(CloudProvider::Tianyi, &store, &http)
                .unwrap()
                .inspect(&input)
                .expect("the native session should recover the web share session")
        else {
            panic!("a public file share should yield a plan")
        };

        assert_eq!(plan.entries[0].name, "05_如何制作动感影集.mp4");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0]
            .url
            .contains("/ssoLogin.action?sessionKey=app-key"));
        assert!(requests[1]
            .url
            .contains("/getShareInfoByCode.action?shareCode=share123"));
        drop(requests);
        let stored: TianyiSession =
            serde_json::from_slice(&store.load("tianyi").unwrap().unwrap()).unwrap();
        assert!(stored.cookie.contains("COOKIE_LOGIN_USER=recovered"));
        assert!(stored.cookie.contains("JSESSIONID=share-session"));
    }

    #[test]
    fn tianyi_share_replaces_a_rejected_web_cookie_from_signed_app_session() {
        // A retained browser cookie may expire independently of the signed App
        // session. The latter must renew only this safe share GET once instead
        // of making a password-logged-in account appear unauthorized.
        let store = MemoryCredentialStore::default();
        store
            .save(
                "tianyi",
                br#"{"cookie":"COOKIE_LOGIN_USER=stale; JSESSIONID=stale-session","auth_method":"password","app":{"sessionKey":"app-key","sessionSecret":"app-secret","accessToken":"access-token","loginName":"alice@189.cn"}}"#,
            )
            .unwrap();
        let http = Http::with_cookies(
            vec![
                json!({"res_code": -1}),
                json!({"res_code": 0}),
                json!({
                    "res_code": 0,
                    "shareId": "share-id",
                    "fileId": "524751232948846473",
                    "fileName": "05_如何制作动感影集.mp4",
                    "isFolder": false,
                    "needAccessCode": 0,
                    "shareMode": 1
                }),
            ],
            vec![
                "COOKIE_LOGIN_USER=recovered; Path=/; Secure".into(),
                "JSESSIONID=share-session; Path=/; Secure".into(),
            ],
        );
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;

        let ShareInspection::Plan(plan) =
            provider_share_adapter(CloudProvider::Tianyi, &store, &http)
                .unwrap()
                .inspect(&input)
                .expect("the signed App session should replace the expired share cookie")
        else {
            panic!("a public file share should yield a plan")
        };

        assert_eq!(plan.entries[0].name, "05_如何制作动感影集.mp4");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0]
            .url
            .contains("/getShareInfoByCode.action?shareCode=share123"));
        assert!(requests[1]
            .url
            .contains("/ssoLogin.action?sessionKey=app-key"));
        assert!(requests[2]
            .url
            .contains("/getShareInfoByCode.action?shareCode=share123"));
        drop(requests);
        let stored: TianyiSession =
            serde_json::from_slice(&store.load("tianyi").unwrap().unwrap()).unwrap();
        assert!(stored.cookie.contains("COOKIE_LOGIN_USER=recovered"));
        assert!(stored.cookie.contains("JSESSIONID=share-session"));
        assert!(stored.app.is_some());
    }

    #[test]
    fn tianyi_share_accepts_zero_count_directory_without_list_fields() {
        let store = credentials();
        let directory = json!({
            "res_code": 0,
            "shareId": 22,
            "fileId": "empty-root",
            "fileName": "empty",
            "isFolder": true,
            "needAccessCode": 0,
            "shareMode": 1
        });
        // The live API returns exactly this compact empty-directory shape.
        let empty_page = json!({"res_code": 0, "fileListAO": {"count": 0}});
        let http = Http::new(vec![directory, empty_page]);
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;

        let result = provider_share_adapter(CloudProvider::Tianyi, &store, &http)
            .unwrap()
            .inspect(&input);

        let ShareInspection::Plan(plan) = result.unwrap() else {
            panic!("empty directory should still produce a share plan")
        };
        assert!(matches!(plan.root_kind, CloudShareRootKind::Directory));
        assert!(plan.entries.is_empty());
        assert_eq!(http.requests.lock().unwrap().len(), 2);
    }
    #[test]
    fn tianyi_share_validates_code_recurses_pages_and_carries_session_headers() {
        let store = credentials();
        let first = json!({"res_code":0,"fileListAO":{"count":2,"folderList":[{"id":"dir","name":"folder"}],"fileList":[]}});
        let last = json!({"res_code":0,"fileListAO":{"count":2,"folderList":[],"fileList":[{"id":"root-file","name":"root.txt","size":2}]}});
        let child = json!({"res_code":0,"fileListAO":{"count":1,"folderList":[],"fileList":[{"id":"child","name":"verify.mp4","size":7}]}});
        let http = Http::new(vec![
            info(),
            checked(),
            first,
            last,
            child,
            checked(),
            json!({"res_code":0,"fileDownloadUrl":"http://cdn.example/file"}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Tianyi, &store, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Tianyi)).unwrap()
        else {
            panic!()
        };
        assert!(matches!(plan.root_kind, CloudShareRootKind::Directory));
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[1].relative_directory, vec!["folder"]);
        let ShareResolution::Direct(direct) = adapter.resolve(&entry(plan)).unwrap() else {
            panic!()
        };
        assert_eq!(direct.url, "http://cdn.example/file");
        assert!(direct.headers.contains(&(
            "Cookie".into(),
            "COOKIE_LOGIN_USER=user; SESSION=issued".into()
        )));
        assert!(direct
            .headers
            .contains(&("User-Agent".into(), TIANYI_WEB_USER_AGENT.into())));
        let requests = http.requests.lock().unwrap();
        assert!(requests[1].url.contains("checkAccessCode.action?"));
        assert!(requests[1].url.contains("accessCode=a123"));
        assert!(requests[3].url.contains("pageNum=2"));
        assert!(requests[4].url.contains("fileId=dir"));
        assert!(requests[6].url.contains("shareId=22"));
        assert!(requests[6].url.contains("getFileDownloadUrl.action?"));
    }
    #[test]
    fn tianyi_share_code_expiry_risk_and_malformed_success_fail_closed() {
        let store = credentials();
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;
        let http = Http::new(vec![info()]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Tianyi, &store, &http)
                .unwrap()
                .inspect(&input),
            Ok(ShareInspection::AccessCodeRequired)
        ));
        for response in [
            json!({"res_code":"ShareNotFound"}),
            json!({"res_code":0,"result":401}),
            json!({"res_code":"AccessCodeError"}),
            json!({"res_code":0,"errorCode":"Forbidden"}),
            json!({"res_code":-1}),
            json!({"res_code":0,"errorMsg":"captcha required"}),
        ] {
            let http = Http::new(vec![response]);
            assert!(provider_share_adapter(CloudProvider::Tianyi, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Tianyi))
                .is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 1);
        }
        let http = Http::new(vec![
            json!({"res_code":0,"shareId":22,"fileId":"single","fileName":"verify.mp4","isFolder":false,"needAccessCode":0,"shareMode":1}),
            json!({"res_code":0,"fileDownloadUrl":""}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Tianyi, &store, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&input).unwrap() else {
            panic!()
        };
        assert!(adapter.resolve(&entry(plan)).is_err());
    }

    #[test]
    fn tianyi_share_rejects_forbidden_error_text_even_with_a_url() {
        let store = credentials();
        for error in [
            json!({"errorMsg":"forbidden"}),
            json!({"error":"forbidden"}),
            json!({"errorMsg":{"code":"Forbidden"}}),
        ] {
            let mut response = json!({"res_code":0,"fileDownloadUrl":"https://cdn.example/f"});
            response
                .as_object_mut()
                .unwrap()
                .extend(error.as_object().unwrap().clone());
            let http = Http::new(vec![
                json!({"res_code":0,"shareId":22,"fileId":"single","fileName":"verify.mp4","isFolder":false,"needAccessCode":0,"shareMode":1}),
                response,
            ]);
            let adapter = provider_share_adapter(CloudProvider::Tianyi, &store, &http).unwrap();
            let ShareInspection::Plan(plan) =
                adapter.inspect(&candidate(CloudProvider::Tianyi)).unwrap()
            else {
                panic!()
            };
            assert!(adapter.resolve(&entry(plan)).is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn tianyi_share_public_file_download_does_not_require_an_access_code() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"res_code":0,"shareId":22,"fileId":"single","fileName":"verify.mp4","isFolder":false,"needAccessCode":0,"shareMode":1}),
            json!({"res_code":0,"fileDownloadUrl":"https://cdn.example/f"}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Tianyi, &store, &http).unwrap();
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;
        let ShareInspection::Plan(plan) = adapter.inspect(&input).unwrap() else {
            panic!()
        };
        assert!(matches!(
            plan.root_kind,
            CloudShareRootKind::File { ref entry_id } if entry_id == "single"
        ));
        assert!(matches!(
            adapter.resolve(&entry(plan)),
            Ok(ShareResolution::Direct(_))
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn tianyi_share_invalid_session_key_requires_reauthorization() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"res_code":0,"shareId":22,"fileId":"single","fileName":"verify.mp4","isFolder":false,"needAccessCode":0,"shareMode":1}),
            json!({"errorCode":"InvalidSessionKey","errorMsg":"session invalid","success":false}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Tianyi, &store, &http).unwrap();
        let mut input = candidate(CloudProvider::Tianyi);
        input.access_code = None;
        let ShareInspection::Plan(plan) = adapter.inspect(&input).unwrap() else {
            panic!()
        };

        let result = adapter.resolve(&entry(plan));

        assert!(matches!(result, Err(QuarkError::AuthorizationExpired)));
    }

    #[test]
    fn tianyi_share_rejects_wrong_checked_code_and_repeated_directory_entries() {
        let store = credentials();
        for checked in [
            json!({"res_code":"AccessCodeError"}),
            json!({"res_code":0,"shareId":999}),
        ] {
            let http = Http::new(vec![info(), checked]);
            assert!(provider_share_adapter(CloudProvider::Tianyi, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Tianyi))
                .is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 2);
        }
        let page = json!({"res_code":0,"fileListAO":{"count":2,"folderList":[],"fileList":[{"id":"f1","name":"file.txt","size":7}]}});
        let http = Http::new(vec![info(), checked(), page.clone(), page]);
        assert!(provider_share_adapter(CloudProvider::Tianyi, &store, &http)
            .unwrap()
            .inspect(&candidate(CloudProvider::Tianyi))
            .is_err());
        assert_eq!(http.requests.lock().unwrap().len(), 4);
        let empty = MemoryCredentialStore::default();
        let http = Http::new(vec![]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Tianyi, &empty, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Tianyi)),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn tianyi_share_preserves_metadata_mode_and_directory_ids_at_every_depth() {
        let store = credentials();
        for mode in [json!(0), json!("2")] {
            let mut metadata = info();
            metadata["shareMode"] = mode.clone();
            let http = Http::new(vec![
                metadata,
                checked(),
                json!({"res_code":0,"fileListAO":{"count":1,"folderList":[{"id":"level-one","name":"one"}],"fileList":[]}}),
                json!({"res_code":0,"fileListAO":{"count":1,"folderList":[{"id":"level-two","name":"two"}],"fileList":[]}}),
                json!({"res_code":0,"fileListAO":{"count":1,"folderList":[],"fileList":[{"id":"leaf","name":"file.txt","size":7}]}}),
            ]);
            let adapter = provider_share_adapter(CloudProvider::Tianyi, &store, &http).unwrap();
            let ShareInspection::Plan(plan) =
                adapter.inspect(&candidate(CloudProvider::Tianyi)).unwrap()
            else {
                panic!()
            };
            assert_eq!(plan.entries[0].relative_directory, vec!["one", "two"]);
            let expected_mode = if mode.is_number() { "0" } else { "2" };
            let requests = http.requests.lock().unwrap();
            for (request, directory) in requests[2..].iter().zip(["root", "level-one", "level-two"])
            {
                let query: BTreeMap<String, String> = url::Url::parse(&request.url)
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
                assert_eq!(
                    query.get("shareMode").map(String::as_str),
                    Some(expected_mode)
                );
                assert_eq!(query.get("fileId").map(String::as_str), Some(directory));
                assert_eq!(
                    query.get("shareDirFileId").map(String::as_str),
                    Some(directory)
                );
                assert_eq!(query.get("shareId").map(String::as_str), Some("22"));
            }
            assert_eq!(requests.len(), 5);
            assert_eq!(
                plan.provider_context["share_mode"],
                if mode.is_number() { json!(0) } else { json!(2) }
            );
            assert_eq!(plan.provider_context["share_dir_file_id"], "root");
        }
    }
}
const TIANYI_WEB_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TianyiSession {
    pub cookie: String,
    #[serde(default)]
    pub auth_method: CloudAuthMethod,
    #[serde(default)]
    pub browser_id: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) app: Option<TianyiAppSession>,
}
impl TianyiSession {
    pub fn cookie(cookie: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            auth_method: CloudAuthMethod::Web,
            browser_id: String::new(),
            user_agent: String::new(),
            app: None,
        }
    }

    pub fn apply_browser_context(&mut self, context: &Value) {
        if let (Some(id), Some(agent)) = (
            context["browser_id"].as_str(),
            context["user_agent"].as_str(),
        ) {
            if valid_browser_id(id) && valid_user_agent(agent) {
                self.browser_id = id.into();
                self.user_agent = agent.into();
            }
        }
    }

    pub fn has_browser_context(&self) -> bool {
        valid_browser_id(&self.browser_id) && valid_user_agent(&self.user_agent)
    }

    /// A Tianyi account has two independent production transports: signed
    /// desktop-App credentials for stable reads/downloads and the official
    /// browser cookie/context for web-only operations. Completing one login
    /// must not erase the other transport from the persisted session.
    pub(crate) fn retain_app_transport(&mut self, existing: &Self) {
        if self.app.is_none() {
            self.app = existing.app.clone();
        }
        if self.app.is_some() {
            self.auth_method = CloudAuthMethod::Password;
        }
    }

    pub(crate) fn retain_web_transport(&mut self, existing: &Self) {
        if !existing.cookie.trim().is_empty() && existing.has_browser_context() {
            self.cookie = existing.cookie.clone();
            self.browser_id = existing.browser_id.clone();
            self.user_agent = existing.user_agent.clone();
        }
        if self.app.is_some() {
            self.auth_method = CloudAuthMethod::Password;
        }
    }

    fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([
            ("Accept".into(), "application/json;charset=UTF-8".into()),
            ("Cookie".into(), self.cookie.clone()),
            ("Origin".into(), CLOUD.into()),
            ("Referer".into(), format!("{CLOUD}/")),
            (
                "User-Agent".into(),
                if valid_user_agent(&self.user_agent) {
                    self.user_agent.clone()
                } else {
                    TIANYI_WEB_USER_AGENT.into()
                },
            ),
        ]);
        if valid_browser_id(&self.browser_id) {
            headers.insert("Browser-Id".into(), self.browser_id.clone());
        }
        headers
    }
}

fn valid_browser_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_user_agent(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && value.bytes().all(|byte| (32..=126).contains(&byte))
}

pub fn parse_tianyi_web_session(cookies: &[String]) -> Result<TianyiSession, QuarkError> {
    // The WebView has already scoped these cookies to cloud.189.cn.  Keep the
    // complete valid cookie jar: Tianyi's directory endpoint accepts only SSO
    // cookies in some sessions, but its share-download endpoint can require a
    // cloud-host session cookie such as JSESSIONID as well.
    let mut pairs = BTreeMap::new();
    for raw in cookies {
        let pair = raw.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if valid_cookie_name(name) && !value.is_empty() && !value.chars().any(char::is_control) {
            pairs.insert(name, value);
        }
    }
    if !pairs.contains_key("COOKIE_LOGIN_USER") && !pairs.contains_key("SSON") {
        return Err(QuarkError::AuthorizationExpired);
    }
    Ok(TianyiSession::cookie(
        pairs
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

fn valid_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[derive(Clone, Debug)]
pub struct TianyiDownload {
    pub url: String,
    pub name: String,
    pub headers: BTreeMap<String, String>,
}

struct TianyiEntryInfo {
    id: String,
    name: String,
    is_folder: bool,
}

pub struct TianyiClient<'a, T: CloudHttpTransport> {
    session: TianyiSession,
    http: &'a T,
}

impl<'a, T: CloudHttpTransport> TianyiClient<'a, T> {
    pub fn new(session: TianyiSession, http: &'a T) -> Self {
        Self { session, http }
    }
    fn headers(&self) -> BTreeMap<String, String> {
        self.session.headers()
    }
    fn require_web_session(&self) -> Result<(), QuarkError> {
        if self.session.app.is_some() && self.session.cookie.trim().is_empty() {
            Err(QuarkError::WebAuthorizationRequired)
        } else {
            Ok(())
        }
    }
    fn web_response(&self, response: &CloudHttpResponse) -> Result<Value, QuarkError> {
        match tianyi_json(response) {
            Err(QuarkError::AuthorizationExpired) if self.session.app.is_some() => {
                Err(QuarkError::WebAuthorizationRequired)
            }
            result => result,
        }
    }
    fn request(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        self.require_web_session()?;
        let response = self.http.send(CloudHttpRequest {
            method,
            url: format!("{CLOUD}{path}?{query}"),
            headers: self.headers(),
            body: body.map(|value| value.to_string()),
        })?;
        self.web_response(&response)
    }

    fn app_request(
        &self,
        method: &'static str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, QuarkError> {
        let app = self
            .session
            .app
            .as_ref()
            .ok_or(QuarkError::AuthorizationExpired)?;
        tianyi_json(&app.request(self.http, method, path, query)?)
    }

    /// The official Tianyi browser window is the authentication boundary for
    /// web authorization.  Its session cookie is sufficient to establish the
    /// workspace; probing a separate private account endpoint here can fail
    /// independently and must not strand a completed login in that window.
    pub fn web_account(&self) -> Result<CloudAccount, QuarkError> {
        if self.session.cookie.trim().is_empty() {
            return Err(QuarkError::AuthorizationPending);
        }
        Ok(CloudAccount {
            provider: CloudProvider::Tianyi,
            id: "tianyi-web".into(),
            display_name: "天翼云盘".into(),
            auth_method: self.session.auth_method,
        })
    }

    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        if let Some(app) = self.session.app.as_ref() {
            if app.login_name().is_empty() {
                return Err(QuarkError::InvalidResponse);
            }
            return Ok(CloudAccount {
                provider: CloudProvider::Tianyi,
                id: app.login_name().to_owned(),
                display_name: app.login_name().to_owned(),
                auth_method: self.session.auth_method,
            });
        }
        let value = self.request(
            "GET",
            "/api/open/user/getUserInfoForPortal.action",
            "",
            None,
        )?;
        let user = value
            .get("userInfo")
            .or_else(|| value.get("data"))
            .unwrap_or(&value);
        Ok(CloudAccount {
            provider: CloudProvider::Tianyi,
            id: user
                .get("userId")
                .or_else(|| user.get("id"))
                .or_else(|| user.get("loginName"))
                .and_then(string_or_number)
                .ok_or(QuarkError::InvalidResponse)?,
            display_name: user
                .get("nickName")
                .or_else(|| user.get("userName"))
                .or_else(|| user.get("loginName"))
                .and_then(Value::as_str)
                .unwrap_or("天翼云盘")
                .into(),
            auth_method: self.session.auth_method,
        })
    }
    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let value = if self.session.app.is_some() {
            self.app_request("GET", "/getUserInfo.action", &[])?
        } else {
            self.request(
                "GET",
                "/api/open/user/getUserInfoForPortal.action",
                "",
                None,
            )?
        };
        let quota = value
            .get("data")
            .or_else(|| value.get("userInfo"))
            .unwrap_or(&value);
        let total = quota
            .get("capacity")
            .or_else(|| quota.get("totalSize"))
            .or_else(|| quota.get("total"))
            .ok_or(QuarkError::InvalidResponse)?;
        if let Some(used) = quota.get("usedSize").or_else(|| quota.get("used")) {
            return CloudStorageCapacity::from_json(total, used, false);
        }
        let available = quota
            .get("available")
            .or_else(|| quota.get("availableSize"))
            .ok_or(QuarkError::InvalidResponse)?;
        CloudStorageCapacity::from_total_and_available_json(total, available)
    }
    pub fn directory(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        let mut seen = BTreeSet::new();
        for page_number in 1..=10_000 {
            let page_number_text = page_number.to_string();
            let value = if self.session.app.is_some() {
                self.app_request(
                    "GET",
                    "/listFiles.action",
                    &[
                        ("folderId", parent),
                        ("recursive", "0"),
                        ("fileType", "0"),
                        ("iconOption", "10"),
                        ("mediaAttr", "0"),
                        ("orderBy", "filename"),
                        ("descending", "false"),
                        ("pageNum", &page_number_text),
                        ("pageSize", "100"),
                    ],
                )?
            } else {
                self.request(
                    "GET",
                    "/api/open/file/listFiles.action",
                    &format!(
                        "folderId={}&pageNum={page_number_text}&pageSize=100&orderBy=1&order=ASC",
                        encoded(parent)
                    ),
                    None,
                )?
            };
            let files = tianyi_file_list(&value)?;
            let page_length = files.len();
            for (file, listed_as_folder) in files {
                let id = file
                    .get("id")
                    .or_else(|| file.get("fileId"))
                    .and_then(string_or_number)
                    .ok_or(QuarkError::InvalidResponse)?;
                if !seen.insert(id.clone()) {
                    return Err(QuarkError::InvalidResponse);
                }
                items.push(CloudFile {
                    id,
                    parent_id: parent.into(),
                    name: required(file, "name").or_else(|_| required(file, "fileName"))?,
                    kind: if listed_as_folder
                        || bool_value(file.get("isFolder").or_else(|| file.get("folder")))
                    {
                        "folder"
                    } else {
                        "file"
                    }
                    .into(),
                    size: u64_value(file.get("size").or_else(|| file.get("fileSize"))),
                    modified_at: file.get("lastOpTime").and_then(string_or_number),
                });
            }
            if page_length < 100 {
                break;
            }
            if page_number == 10_000 {
                return Err(QuarkError::InvalidResponse);
            }
        }
        if parent == "-11" {
            items.push(CloudFile {
                id: TIANYI_RECYCLE_BIN_ID.into(),
                parent_id: parent.into(),
                name: TIANYI_RECYCLE_BIN_NAME.into(),
                kind: "folder".into(),
                size: None,
                modified_at: None,
            });
        }
        Ok(CloudDirectory {
            id: parent.into(),
            items,
        })
    }

    pub fn list_recycle_bin(&self) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        let mut seen = BTreeSet::new();
        for page in 1..=10_000 {
            let value = self.request(
                "GET",
                "/api/open/file/listRecycleBinFiles.action",
                &format!("pageNum={page}&pageSize=100&iconOption=1&family=false"),
                None,
            )?;
            let files = value
                .get("fileList")
                .or_else(|| value.pointer("/data/fileList"))
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let length = files.len();
            for file in files {
                let id = file
                    .get("id")
                    .and_then(string_or_number)
                    .filter(|id| valid_tianyi_recycle_record_id(id))
                    .ok_or(QuarkError::InvalidResponse)?;
                if !seen.insert(id.clone()) {
                    return Err(QuarkError::InvalidResponse);
                }
                items.push(CloudFile {
                    id,
                    parent_id: TIANYI_RECYCLE_BIN_ID.into(),
                    name: required(file, "name").or_else(|_| required(file, "fileName"))?,
                    kind: if bool_value(file.get("isFolder").or_else(|| file.get("folder"))) {
                        "folder"
                    } else {
                        "file"
                    }
                    .into(),
                    size: u64_value(file.get("size").or_else(|| file.get("fileSize"))),
                    modified_at: file
                        .get("lastOpTime")
                        .or_else(|| file.get("lastOpDate"))
                        .and_then(string_or_number),
                });
            }
            if length < 100 {
                return Ok(CloudDirectory {
                    id: TIANYI_RECYCLE_BIN_ID.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn restore_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        let ids = validated_tianyi_recycle_record_ids(ids)?;
        let task_infos = serde_json::to_string(
            &ids.iter()
                .map(|id| json!({ "fileId": id }))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| QuarkError::InvalidResponse)?;
        self.form_request(
            "POST",
            "/api/open/batch/createBatchTask.action",
            &[("type", "RESTORE"), ("taskInfos", &task_infos)],
        )
        .map(|_| ())
    }

    pub fn delete_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        let ids = validated_tianyi_recycle_record_ids(ids)?;
        self.request(
            "GET",
            "/v2/deleteFile.action",
            &format!("fileIdList={}", encoded(&ids.join(","))),
            None,
        )
        .map(|_| ())
    }

    /// Tianyi's personal-share endpoint creates one link per file or folder,
    /// and generates the private access code itself. The service only offers
    /// one-day, seven-day, or permanent expiry modes, so reject unsupported
    /// generic values rather than silently changing the requested expiry.
    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.len() != 1 {
            return Err(QuarkError::InvalidInput(
                "天翼云盘一次只能为一个文件或目录创建分享".into(),
            ));
        }
        if options
            .pass_code
            .as_deref()
            .is_some_and(|code| !code.trim().is_empty())
        {
            return Err(QuarkError::InvalidInput(
                "天翼云盘会自动生成访问码，暂不支持自定义访问码".into(),
            ));
        }
        let expire_time = match options.expires_in_days {
            None => 2099,
            Some(1) => 1,
            Some(7) => 7,
            Some(_) => {
                return Err(QuarkError::InvalidInput(
                    "天翼云盘仅支持永久、1 天或 7 天有效期".into(),
                ))
            }
        };
        let value = self.request(
            "GET",
            "/api/open/share/createShareLink.action",
            &format!(
                "fileId={}&expireTime={expire_time}&shareType=3",
                encoded(&file_ids[0])
            ),
            None,
        )?;
        let record = value
            .get("shareLinkList")
            .and_then(Value::as_array)
            .and_then(|records| records.first())
            .ok_or(QuarkError::InvalidResponse)?;
        let mut share = tianyi_share_from_value(record, Some(file_ids), None)?;
        share.name = options.name.clone();
        Ok(share)
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let mut shares = Vec::new();
        for page in 1..=10_000 {
            let value = self.request(
                "GET",
                "/api/portal/listShares.action",
                &format!("shareType=1&pageNum={page}&pageSize=60"),
                None,
            )?;
            let records = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = records.len();
            shares.extend(
                records
                    .iter()
                    .map(|record| tianyi_share_from_value(record, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if count < 60 {
                return Ok(shares);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        if share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        self.request(
            "GET",
            "/api/portal/cancelShare.action",
            // The deployed personal-drive endpoint names this historical
            // parameter `ancelType` (without the leading c). Keep the wire
            // spelling here; `cancelType` is ignored by that endpoint.
            &format!("shareIdList={}&ancelType=1", encoded(&share_ids.join(","))),
            None,
        )?;
        Ok(())
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<(), QuarkError> {
        self.form_request(
            "POST",
            "/api/open/file/createFolder.action",
            &[("parentFolderId", parent), ("folderName", name)],
        )
        .map(|_| ())
    }
    pub fn rename(&self, id: &str, name: &str) -> Result<(), QuarkError> {
        let entry = self.entry_info(id)?;
        let (path, id_key, name_key) = if entry.is_folder {
            (
                "/api/open/file/renameFolder.action",
                "folderId",
                "destFolderName",
            )
        } else {
            ("/api/open/file/renameFile.action", "fileId", "destFileName")
        };
        self.form_request("POST", path, &[(id_key, id), (name_key, name)])
            .map(|_| ())
    }
    pub fn move_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.batch_operation("MOVE", ids, parent)
    }
    pub fn copy_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.batch_operation("COPY", ids, parent)
    }
    pub fn trash(&self, ids: &[String]) -> Result<(), QuarkError> {
        self.batch_operation("DELETE", ids, "")
    }
    fn batch_operation(
        &self,
        operation: &str,
        ids: &[String],
        parent: &str,
    ) -> Result<(), QuarkError> {
        if ids.is_empty() || (operation != "DELETE" && parent.is_empty()) {
            return Err(QuarkError::InvalidInput("请选择文件和目标目录".into()));
        }
        let task_infos = ids
            .iter()
            .map(|id| self.entry_info(id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| {
                json!({
                    "fileId": entry.id,
                    "fileName": entry.name,
                    "isFolder": i64::from(entry.is_folder),
                })
            })
            .collect::<Vec<_>>();
        let task_infos =
            serde_json::to_string(&task_infos).map_err(|_| QuarkError::InvalidResponse)?;
        self.form_request(
            "POST",
            "/api/open/batch/createBatchTask.action",
            &[
                ("type", operation),
                ("targetFolderId", parent),
                ("taskInfos", &task_infos),
            ],
        )
        .map(|_| ())
    }

    fn form_request(
        &self,
        method: &'static str,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<Value, QuarkError> {
        self.require_web_session()?;
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in fields {
            serializer.append_pair(name, value);
        }
        let mut headers = self.headers();
        headers.insert(
            "Content-Type".into(),
            "application/x-www-form-urlencoded; charset=UTF-8".into(),
        );
        let response = self.http.send(CloudHttpRequest {
            method,
            url: format!("{CLOUD}{path}"),
            headers,
            body: Some(serializer.finish()),
        })?;
        self.web_response(&response)
    }

    fn entry_info(&self, id: &str) -> Result<TianyiEntryInfo, QuarkError> {
        let value = self.request(
            "GET",
            "/api/portal/getFileInfo.action",
            &format!("fileId={}", encoded(id)),
            None,
        )?;
        let file = value
            .get("fileInfo")
            .or_else(|| value.get("data"))
            .unwrap_or(&value);
        Ok(TianyiEntryInfo {
            id: file
                .get("fileId")
                .or_else(|| file.get("folderId"))
                .or_else(|| file.get("id"))
                .and_then(string_or_number)
                .unwrap_or_else(|| id.into()),
            name: required(file, "fileName").or_else(|_| required(file, "name"))?,
            is_folder: bool_value(file.get("isFolder").or_else(|| file.get("folder")))
                || file.get("fileCata").and_then(i64_value) == Some(1),
        })
    }
    /// Obtains the upload host; bytes are submitted later by the command layer.
    pub fn upload(&self) -> Result<Value, QuarkError> {
        self.request("POST", "/v2/getUserUploadUrl.action", "", None)
    }

    /// Uploads a regular file with the authenticated web session. The page
    /// session key and upload URL are consumed in this stack frame only.
    pub fn upload_file<U: CloudUploadTransport>(
        &self,
        parent: &str,
        local_path: &Path,
        upload_http: &U,
    ) -> Result<CloudFile, QuarkError> {
        let snapshot = super::UploadFileSnapshot::capture(local_path)?;
        if parent.is_empty() {
            return Err(QuarkError::InvalidInput("请选择目标目录".into()));
        }
        let metadata = std::fs::symlink_metadata(local_path)
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(QuarkError::InvalidInput("请选择普通文件上传".into()));
        }
        let name = local_path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty() && !value.contains(['\r', '\n']))
            .ok_or_else(|| QuarkError::InvalidInput("文件名无效".into()))?
            .to_owned();
        let session_key = self.page_session_key()?;
        let endpoint = required(&self.upload()?, "uploadUrl")?;
        let endpoint = super::https_transfer_url(&endpoint)?;
        let (content_type, body) = super::multipart_file(
            &[
                ("sessionKey", session_key),
                ("parentId", parent.into()),
                ("albumId", "undefined".into()),
                ("opertype", "1".into()),
                ("fname", name.clone()),
            ],
            "Filedata",
            &name,
            snapshot.slice(0, snapshot.len())?,
        )?;
        let mut headers = self.headers();
        headers.insert("Content-Type".into(), content_type);
        let response = upload_http.send_upload(UploadHttpRequest {
            method: "POST",
            url: endpoint,
            headers,
            body,
        })?;
        snapshot.validate()?;
        tianyi_upload_response(&response, parent, &name, metadata.len())
    }
    pub fn download(&self, id: &str) -> Result<TianyiDownload, QuarkError> {
        let value = if self.session.app.is_some() {
            self.app_request(
                "GET",
                "/getFileDownloadUrl.action",
                &[("fileId", id), ("dt", "3"), ("flag", "1")],
            )
        } else {
            self.request(
                "GET",
                "/api/open/file/getFileDownloadUrl.action",
                &format!("fileId={}", encoded(id)),
                None,
            )
        }
        .map_err(download_source_missing_from_http_404)?;
        let mut url = required(&value, "fileDownloadUrl")?;
        if !url.starts_with("https://") {
            return Err(QuarkError::InvalidResponse);
        }
        let headers = if let Some(app) = self.session.app.as_ref() {
            let signed = app.signed_download(&url)?;
            url = signed.0;
            signed.1
        } else {
            self.headers()
        };
        Ok(TianyiDownload {
            url,
            name: value
                .get("fileName")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .into(),
            headers,
        })
    }
    pub fn login_password(
        username: &str,
        password: &str,
        http: &'a T,
    ) -> Result<TianyiSession, QuarkError> {
        let login = tianyi_app::login_password(username, password, http)?;
        let session = TianyiSession {
            cookie: login.web_cookie,
            auth_method: CloudAuthMethod::Password,
            browser_id: String::new(),
            user_agent: String::new(),
            app: Some(login.session),
        };
        let client = Self::new(session.clone(), http);
        client.account()?;
        client.directory("-11")?;
        Ok(session)
    }

    fn page_session_key(&self) -> Result<String, QuarkError> {
        self.require_web_session()?;
        let response = self.http.send(CloudHttpRequest {
            method: "GET",
            url: format!("{CLOUD}/main.action"),
            headers: self.headers(),
            body: None,
        })?;
        if !(200..300).contains(&response.status) {
            return Err(QuarkError::Provider("天翼云盘无法建立上传会话".into()));
        }
        let capture = regex::Regex::new(r#"sessionKey\s*=\s*['\"]([^'\"]+)['\"]"#)
            .map_err(|_| QuarkError::InvalidResponse)?
            .captures(&response.body)
            .and_then(|captures| captures.get(1))
            .map(|capture| capture.as_str().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        Ok(capture)
    }
}

fn tianyi_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = value
        .get("shareId")
        .or_else(|| value.get("share_id"))
        .and_then(string_or_number)
        .filter(|id| !id.is_empty())
        .ok_or(QuarkError::InvalidResponse)?;
    let url = value
        .get("accessURL")
        .or_else(|| value.get("accessUrl"))
        .or_else(|| value.get("shortShareUrl"))
        .or_else(|| value.get("url"))
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(|url| {
            if url.starts_with("http://") || url.starts_with("https://") {
                url.to_owned()
            } else if url.starts_with("//") {
                format!("https:{url}")
            } else {
                format!("https://{url}")
            }
        })
        .ok_or(QuarkError::InvalidResponse)?;
    let source_file_ids = value
        .get("fileId")
        .and_then(string_or_number)
        .map(|id| vec![id])
        .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
        .unwrap_or_default();
    Ok(CloudShare {
        id,
        url,
        name: value
            .get("fileName")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("分享文件")
            .into(),
        kind: if bool_value(value.get("isFolder").or_else(|| value.get("folder"))) {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids,
        pass_code: value
            .get("accessCode")
            .or_else(|| value.get("passCode"))
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: value
            .get("shareTime")
            .or_else(|| value.get("shareDate"))
            .and_then(string_or_number),
        expires_at: value.get("expireTime").and_then(string_or_number),
    })
}

fn tianyi_upload_response(
    response: &UploadHttpResponse,
    parent: &str,
    name: &str,
    size: u64,
) -> Result<CloudFile, QuarkError> {
    if !(200..300).contains(&response.status) {
        return Err(QuarkError::Provider("天翼云盘拒绝上传文件".into()));
    }
    let value = tianyi_response_value(&response.body)?;
    let code = value
        .get("res_code")
        .or_else(|| value.get("result"))
        .and_then(i64_value)
        .unwrap_or(0);
    if code != 0 {
        return Err(QuarkError::Provider("天翼云盘拒绝上传文件".into()));
    }
    Ok(CloudFile {
        id: value
            .get("id")
            .or_else(|| value.get("fileId"))
            .and_then(string_or_number)
            .ok_or(QuarkError::InvalidResponse)?,
        parent_id: parent.into(),
        name: value
            .get("fileName")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(name)
            .into(),
        kind: "file".into(),
        size: value
            .get("size")
            .or_else(|| value.get("fileSize"))
            .and_then(Value::as_u64)
            .or(Some(size)),
        modified_at: value.get("createDate").and_then(string_or_number),
    })
}
fn tianyi_json(response: &CloudHttpResponse) -> Result<Value, QuarkError> {
    if response.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    let value = tianyi_response_value(&response.body)?;
    if challenge(&value) {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    if let Some(code) = tianyi_protocol_error_code(&value) {
        if matches!(
            code.to_ascii_lowercase().as_str(),
            "filenotfound" | "filenotexist" | "sharenotfound"
        ) {
            return Err(QuarkError::SourceFileMissing);
        }
        return Err(
            if matches!(code.as_str(), "-1" | "401" | "InvalidSessionKey") {
                QuarkError::AuthorizationExpired
            } else {
                QuarkError::Provider(format!("天翼云盘拒绝请求（错误码 {code}）"))
            },
        );
    }
    if !(200..300).contains(&response.status) {
        return Err(QuarkError::Provider(format!(
            "天翼云盘请求失败（HTTP {}）",
            response.status
        )));
    }
    Ok(value)
}

fn tianyi_protocol_error_code(value: &Value) -> Option<String> {
    let candidates = ["res_code", "result", "errorCode", "error_code"]
        .iter()
        .filter_map(|field| value.get(*field))
        .chain(
            ["/error/code", "/error/errorCode"]
                .iter()
                .filter_map(|path| value.pointer(path)),
        );
    for raw in candidates {
        let code = match raw {
            Value::Number(number) => number.to_string(),
            Value::String(text) => text.trim().to_owned(),
            _ => continue,
        };
        if code.is_empty() || matches!(code.to_ascii_lowercase().as_str(), "0" | "success" | "ok") {
            continue;
        }
        if code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Some(code);
        }
        return Some("Unknown".into());
    }
    None
}
fn challenge(value: &Value) -> bool {
    // Account metadata, filenames and download URLs are ordinary data, not
    // authorization errors. Scanning the complete response strands a valid
    // login whenever, for example, its root directory contains verify.txt.
    [
        "msg",
        "message",
        "errorMsg",
        "res_message",
        "error",
        "errorCode",
        "error_code",
    ]
    .iter()
    .filter_map(|field| value.get(*field).and_then(Value::as_str))
    .chain(
        ["/error/code", "/error/message"]
            .iter()
            .filter_map(|path| value.pointer(path).and_then(Value::as_str)),
    )
    .any(|message| {
        let lower = message.to_ascii_lowercase();
        [
            "captcha",
            "短信",
            "sms",
            "verify",
            "risk",
            "安全验证",
            "验证码",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    })
}

use super::{
    share::{
        CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudShareResolvedEntry,
        CloudShareRootKind, ShareInspect, ShareInspection, ShareResolution, ShareResolve,
    },
    CredentialStore,
};

pub struct TianyiShareAdapter<'a> {
    pub credentials: &'a dyn CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
struct TianyiShareSession<'a> {
    cookie: String,
    browser_session: TianyiSession,
    credentials: &'a dyn CredentialStore,
    http: &'a dyn CloudHttpTransport,
    renewed_cookie: bool,
}
impl TianyiShareAdapter<'_> {
    fn session(&self) -> Result<TianyiShareSession<'_>, QuarkError> {
        let bytes = self
            .credentials
            .load("tianyi")?
            .ok_or(QuarkError::NotAuthorized)?;
        let mut session: TianyiSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.cookie.trim().is_empty() {
            let app = session.app.as_ref().ok_or(QuarkError::NotAuthorized)?;
            let cookie = tianyi_app::refresh_web_cookie(app, self.http)?;
            if cookie.is_empty() {
                return Err(QuarkError::WebAuthorizationRequired);
            }
            session.cookie = cookie;
            self.credentials.save(
                "tianyi",
                &serde_json::to_vec(&session).map_err(|_| QuarkError::Storage)?,
            )?;
        }
        Ok(TianyiShareSession {
            cookie: session.cookie.clone(),
            browser_session: session,
            credentials: self.credentials,
            http: self.http,
            renewed_cookie: false,
        })
    }
}
fn tianyi_share_json(response: &CloudHttpResponse) -> Result<Value, QuarkError> {
    if response.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
    if !value.is_object() {
        return Err(QuarkError::InvalidResponse);
    }
    // Only protocol error fields can signal a challenge, never filenames or URLs.
    for field in ["errorMsg", "res_message", "message", "error"] {
        if let Some(text) = value[field].as_str() {
            let lower = text.to_ascii_lowercase();
            if ["captcha", "risk", "安全验证", "验证码"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                return Err(QuarkError::WebAuthorizationRequired);
            }
        }
    }
    if ["errorCode", "error_code"].iter().any(|field| {
        value[*field]
            .as_str()
            .is_some_and(|code| code.eq_ignore_ascii_case("InvalidSessionKey"))
    }) {
        return Err(QuarkError::AuthorizationExpired);
    }
    for key in ["res_code", "result", "errorCode", "error_code"] {
        if let Some(code) = value.get(key) {
            match i64_value(code) {
                Some(0) => {}
                Some(-1 | 401) => return Err(QuarkError::AuthorizationExpired),
                _ => {
                    return Err(QuarkError::Provider(
                        "天翼云盘分享不可用，请检查访问码、有效期及权限".into(),
                    ))
                }
            }
        }
    }
    for key in ["errorMsg", "error"] {
        if let Some(error) = value.get(key) {
            let error = error.as_str().ok_or(QuarkError::InvalidResponse)?;
            if !matches!(
                error.to_ascii_lowercase().as_str(),
                "" | "success" | "ok" | "成功" | "操作成功"
            ) {
                return Err(QuarkError::Provider("天翼云盘分享请求失败".into()));
            }
        }
    }
    if value.get("res_code").is_none() || !(200..300).contains(&response.status) {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(value)
}
impl TianyiShareSession<'_> {
    fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = self.browser_session.headers();
        headers.insert("Cookie".into(), self.cookie.clone());
        headers
    }
    fn send_request(&mut self, path: &str, query: &str) -> Result<Value, QuarkError> {
        let response = self.http.send(CloudHttpRequest {
            method: "GET",
            url: format!("{CLOUD}{path}?{query}"),
            headers: self.headers(),
            body: None,
        })?;
        let value = tianyi_share_json(&response)?;
        self.cookie = super::share::download_headers(
            &self.cookie,
            &response.set_cookies,
            TIANYI_WEB_USER_AGENT,
            None,
        )
        .into_iter()
        .find(|(name, _)| name == "Cookie")
        .ok_or(QuarkError::InvalidResponse)?
        .1;
        self.browser_session.cookie = self.cookie.clone();
        Ok(value)
    }
    fn renew_cookie_from_app_session(&mut self) -> Result<bool, QuarkError> {
        let Some(app) = self.browser_session.app.clone() else {
            return Ok(false);
        };
        let cookie = tianyi_app::refresh_web_cookie(&app, self.http)?;
        if cookie.trim().is_empty() {
            return Ok(false);
        }
        self.cookie = cookie.clone();
        self.browser_session.cookie = cookie;
        self.credentials.save(
            "tianyi",
            &serde_json::to_vec(&self.browser_session).map_err(|_| QuarkError::Storage)?,
        )?;
        Ok(true)
    }
    fn request(&mut self, path: &str, query: &str) -> Result<Value, QuarkError> {
        match self.send_request(path, query) {
            Err(error @ QuarkError::AuthorizationExpired) if !self.renewed_cookie => {
                self.renewed_cookie = true;
                if self.renew_cookie_from_app_session()? {
                    self.send_request(path, query)
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }
    fn check(&mut self, code: &str, access: &str, expected: &str) -> Result<(), QuarkError> {
        let value = self.request(
            "/api/open/share/checkAccessCode.action",
            &format!("shareCode={}&accessCode={}", encoded(code), encoded(access)),
        )?;
        if value.get("shareId").and_then(string_or_number).as_deref() != Some(expected) {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(())
    }
}
fn tianyi_share_flag(value: &Value) -> Result<bool, QuarkError> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        _ => match i64_value(value) {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(QuarkError::InvalidResponse),
        },
    }
}
impl ShareInspect for TianyiShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let code = super::share::share_id(candidate, CloudProvider::Tianyi)?;
        let mut session = self.session()?;
        let info = session.request(
            "/api/open/share/getShareInfoByCode.action",
            &format!("shareCode={}", encoded(&code)),
        )?;
        let needs_code = match info.get("needAccessCode") {
            Some(value) => tianyi_share_flag(value)?,
            None => false,
        };
        if needs_code {
            let Some(access) = candidate.access_code.as_deref() else {
                return Ok(ShareInspection::AccessCodeRequired);
            };
            let id = info
                .get("shareId")
                .and_then(string_or_number)
                .filter(|id| !id.is_empty())
                .ok_or(QuarkError::InvalidResponse)?;
            session.check(&code, access, &id)?;
        }
        let id = info
            .get("shareId")
            .and_then(string_or_number)
            .filter(|id| !id.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        let root = info
            .get("fileId")
            .and_then(string_or_number)
            .filter(|id| !id.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        let share_mode = info
            .get("shareMode")
            .and_then(i64_value)
            .filter(|mode| *mode >= 0)
            .ok_or(QuarkError::InvalidResponse)?;
        let context = json!({"share_id":id,"needs_code":needs_code,"share_mode":share_mode,"share_dir_file_id":root});
        let mut entries = vec![];
        let folder = tianyi_share_flag(&info["isFolder"])?;
        let root_kind = if folder {
            CloudShareRootKind::Directory
        } else {
            CloudShareRootKind::File {
                entry_id: root.clone(),
            }
        };
        if !folder {
            entries.push(CloudShareEntry::new(
                &code,
                root,
                required(&info, "fileName")?,
                vec![],
                u64_value(info.get("fileSize")),
            ));
        } else {
            let mut queue = std::collections::VecDeque::from([(root.clone(), vec![])]);
            let mut seen = std::collections::BTreeSet::from([root]);
            let mut requests = 0usize;
            while let Some((parent, directory)) = queue.pop_front() {
                let mut listed = 0u64;
                for page in 1..=10_000 {
                    requests += 1;
                    if requests > 10_000 || directory.len() > 128 {
                        return Err(QuarkError::InvalidResponse);
                    }
                    // Tianyi requires both directory parameters to identify the current
                    // traversal node; neither stays pinned to the original share root.
                    let value = session.request("/api/open/share/listShareDir.action",&format!("shareId={}&fileId={}&shareDirFileId={}&isFolder=true&shareMode={share_mode}&accessCode={}&pageNum={page}&pageSize=100",encoded(&id),encoded(&parent),encoded(&parent),encoded(candidate.access_code.as_deref().unwrap_or(""))))?;
                    let files = tianyi_file_list(&value)?;
                    let length = files.len();
                    listed += length as u64;
                    for (file, as_folder) in files {
                        let fid = file
                            .get("id")
                            .or_else(|| file.get("fileId"))
                            .and_then(string_or_number)
                            .filter(|id| !id.is_empty())
                            .ok_or(QuarkError::InvalidResponse)?;
                        let name =
                            required(file, "name").or_else(|_| required(file, "fileName"))?;
                        if !seen.insert(fid.clone()) {
                            return Err(QuarkError::InvalidResponse);
                        }
                        let folder = as_folder
                            || file
                                .get("isFolder")
                                .map(tianyi_share_flag)
                                .transpose()?
                                .unwrap_or(false);
                        if folder {
                            let mut path = directory.clone();
                            path.push(name);
                            queue.push_back((fid, path));
                        } else {
                            entries.push(CloudShareEntry::new(
                                &code,
                                fid,
                                name,
                                directory.clone(),
                                u64_value(file.get("size").or_else(|| file.get("fileSize"))),
                            ));
                        }
                    }
                    let count = u64_value(value.pointer("/fileListAO/count"));
                    if count.is_some_and(|count| listed >= count)
                        || (count.is_none() && length < 100)
                    {
                        break;
                    }
                    if length == 0 || page == 10_000 {
                        return Err(QuarkError::InvalidResponse);
                    }
                }
            }
        }
        let mut plan = CloudSharePlan::new(
            CloudProvider::Tianyi,
            code,
            candidate.access_code.clone(),
            entries,
            context,
        );
        plan.root_kind = root_kind;
        Ok(ShareInspection::Plan(plan))
    }
}
impl ShareResolve for TianyiShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_tianyi_share(self.credentials, self.http, entry)
    }
}
impl CloudShareAdapter for TianyiShareAdapter<'_> {}
pub fn resolve_tianyi_share(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    if entry.provider != CloudProvider::Tianyi || entry.plan.provider != CloudProvider::Tianyi {
        return Err(QuarkError::InvalidResponse);
    }
    let adapter = TianyiShareAdapter { credentials, http };
    let mut session = adapter.session()?;
    let id = required(&entry.plan.provider_context, "share_id")?;
    // Protected shares must revalidate their access code; public shares rely on
    // the download API's shareId check without calling a password-only endpoint.
    if entry.plan.provider_context["needs_code"]
        .as_bool()
        .ok_or(QuarkError::InvalidResponse)?
    {
        session.check(
            &entry.plan.canonical_share_id,
            entry.access_code().ok_or(QuarkError::InvalidResponse)?,
            &id,
        )?;
    }
    let value = session.request(
        "/api/open/file/getFileDownloadUrl.action",
        &format!(
            "fileId={}&shareId={}&dt=1",
            encoded(&entry.entry.entry_id),
            encoded(&id)
        ),
    )?;
    let url = required(&value, "fileDownloadUrl")?;
    super::share::validate_direct_url(&url)?;
    Ok(ShareResolution::Direct(
        super::commands::ResolvedCloudDownload {
            transform: None,
            url,
            name: entry.entry.name.clone(),
            headers: session.headers().into_iter().collect(),
        },
    ))
}
fn string_or_number(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

fn valid_tianyi_recycle_record_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn validated_tianyi_recycle_record_ids(ids: &[String]) -> Result<Vec<String>, QuarkError> {
    if ids.is_empty()
        || ids.iter().any(|id| !valid_tianyi_recycle_record_id(id))
        || ids
            .iter()
            .enumerate()
            .any(|(index, id)| ids[..index].contains(id))
    {
        return Err(QuarkError::InvalidInput("回收站记录 ID 无效".into()));
    }
    Ok(ids.to_vec())
}

fn i64_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

fn u64_value(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
    })
}

fn bool_value(value: Option<&Value>) -> bool {
    value
        .and_then(|value| {
            value.as_bool().or_else(|| {
                value
                    .as_str()
                    .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" => Some(true),
                        "false" | "0" | "no" => Some(false),
                        _ => None,
                    })
            })
        })
        .unwrap_or(false)
}

fn tianyi_file_list(value: &Value) -> Result<Vec<(&Value, bool)>, QuarkError> {
    let list = value
        .pointer("/fileListAO/fileList")
        .or_else(|| value.pointer("/listFiles/fileList"))
        .or_else(|| value.get("data"));
    let container = value
        .get("fileListAO")
        .or_else(|| value.get("listFiles"))
        .or_else(|| value.get("data"));
    let mut items = Vec::new();
    if let Some(container) = container {
        let listings = [
            (container.get("fileList"), "file", false),
            (container.get("folderList"), "folder", true),
        ];
        for (listing, nested_name, folder) in listings {
            append_tianyi_entries(&mut items, listing, nested_name, folder);
        }
        // An authenticated but brand-new drive can validly contain neither
        // files nor folders.  Only an explicit empty array is trusted as an
        // empty list; null or scalar values can be malformed API responses.
        let known_empty_listing = listings
            .iter()
            .filter_map(|(listing, _, _)| *listing)
            .all(|listing| listing.as_array().is_some_and(Vec::is_empty));
        let has_listing = listings.iter().any(|(listing, _, _)| listing.is_some());
        let server_confirmed_empty =
            !has_listing && u64_value(container.get("count")).is_some_and(|count| count == 0);
        if !items.is_empty() || (has_listing && known_empty_listing) || server_confirmed_empty {
            return Ok(items);
        }
    }
    let list = list.ok_or(QuarkError::InvalidResponse)?;
    append_tianyi_entries(&mut items, Some(list), "file", false);
    (!items.is_empty())
        .then_some(items)
        .ok_or(QuarkError::InvalidResponse)
}

fn append_tianyi_entries<'a>(
    output: &mut Vec<(&'a Value, bool)>,
    candidate: Option<&'a Value>,
    nested_name: &str,
    folder: bool,
) {
    let Some(candidate) = candidate else {
        return;
    };
    let candidate = candidate.get(nested_name).unwrap_or(candidate);
    if let Some(entries) = candidate.as_array() {
        output.extend(entries.iter().map(|entry| (entry, folder)));
    } else if candidate.is_object() {
        output.push((candidate, folder));
    }
}

/// Tianyi's authenticated file endpoints can return either JSON or XML.  XML
/// is converted into the small JSON-like tree consumed by the rest of this
/// provider, without exposing a general XML execution surface.  Attributes,
/// DTDs and processing instructions are intentionally ignored.
fn tianyi_response_value(body: &str) -> Result<Value, QuarkError> {
    if let Ok(value) = serde_json::from_str(body) {
        return Ok(value);
    }
    tianyi_xml_value(body)
}

struct XmlNode {
    name: String,
    children: serde_json::Map<String, Value>,
    text: String,
}

impl XmlNode {
    fn new(name: String) -> Self {
        Self {
            name,
            children: serde_json::Map::new(),
            text: String::new(),
        }
    }

    fn into_value(mut self) -> Value {
        let text = self.text.trim();
        if self.children.is_empty() {
            // XML represents an empty collection as `<fileList/>` rather
            // than `[]`. Normalize only these known collection nodes before
            // directory validation so an arbitrary empty JSON scalar cannot
            // be mistaken for an empty drive.
            if text.is_empty() && matches!(self.name.as_str(), "fileList" | "folderList") {
                return Value::Array(Vec::new());
            }
            return Value::String(text.into());
        }
        if !text.is_empty() {
            self.children
                .insert("#text".into(), Value::String(text.into()));
        }
        Value::Object(self.children)
    }
}

fn insert_xml_child(children: &mut serde_json::Map<String, Value>, name: String, value: Value) {
    match children.entry(name) {
        serde_json::map::Entry::Vacant(entry) => {
            entry.insert(value);
        }
        serde_json::map::Entry::Occupied(mut entry) => match entry.get_mut() {
            Value::Array(values) => values.push(value),
            previous => {
                let previous = std::mem::replace(previous, Value::Null);
                *entry.get_mut() = Value::Array(vec![previous, value]);
            }
        },
    }
}

fn xml_name(name: &str) -> Result<String, QuarkError> {
    (!name.is_empty())
        .then(|| name.to_owned())
        .ok_or(QuarkError::InvalidResponse)
}

fn xml_text(raw: &str) -> Result<String, QuarkError> {
    quick_xml::escape::unescape(raw)
        .map(|value| value.into_owned())
        .map_err(|_| QuarkError::InvalidResponse)
}

fn xml_reference(reference: &quick_xml::events::BytesRef<'_>) -> Result<char, QuarkError> {
    let value = match reference.as_ref() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "apos" => '\'',
        "quot" => '"',
        _ => reference
            .resolve_char_ref()
            .map_err(|_| QuarkError::InvalidResponse)?
            .ok_or(QuarkError::InvalidResponse)?,
    };
    (!value.is_control() || matches!(value, '\t' | '\n' | '\r'))
        .then_some(value)
        .ok_or(QuarkError::InvalidResponse)
}

fn tianyi_xml_value(body: &str) -> Result<Value, QuarkError> {
    use quick_xml::{events::Event, Reader};

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<XmlNode>::new();
    let mut root: Option<(String, Value)> = None;
    loop {
        match reader
            .read_event()
            .map_err(|_| QuarkError::InvalidResponse)?
        {
            Event::Start(event) => stack.push(XmlNode::new(xml_name(event.name().as_ref())?)),
            Event::Empty(event) => {
                let node = XmlNode::new(xml_name(event.name().as_ref())?);
                let name = node.name.clone();
                let value = node.into_value();
                if let Some(parent) = stack.last_mut() {
                    insert_xml_child(&mut parent.children, name, value);
                } else if root.is_none() {
                    root = Some((name, value));
                } else {
                    return Err(QuarkError::InvalidResponse);
                }
            }
            Event::Text(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&xml_text(event.as_ref())?);
                }
            }
            Event::CData(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(event.as_ref());
                }
            }
            Event::GeneralRef(event) => {
                let node = stack.last_mut().ok_or(QuarkError::InvalidResponse)?;
                node.text.push(xml_reference(&event)?);
            }
            Event::End(_) => {
                let node = stack.pop().ok_or(QuarkError::InvalidResponse)?;
                let name = node.name.clone();
                let value = node.into_value();
                if let Some(parent) = stack.last_mut() {
                    insert_xml_child(&mut parent.children, name, value);
                } else if root.is_none() {
                    root = Some((name, value));
                } else {
                    return Err(QuarkError::InvalidResponse);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(QuarkError::InvalidResponse);
    }
    let (name, value) = root.ok_or(QuarkError::InvalidResponse)?;
    Ok(json!({ name: value }))
}
