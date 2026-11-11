//! Aliyun Drive's browser-granted refresh-token protocol.
//!
//! The credential store deliberately contains only the renewable session fields.
//! Signed download URLs are requested immediately before use and never written here.
use super::{
    http::response_json, types::required, CloudAccount, CloudAuthMethod, CloudDirectory, CloudFile,
    CloudFileKind, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider,
    CloudShare, CloudShareOptions, CloudSharePassCodeCharacterSet, CloudSharePassCodeConstraint,
    CloudStorageCapacity, CloudUploadTransport, CredentialStore, QuarkError, UploadHttpRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

// The current official web client renews the browser-granted `token` entry
// through this endpoint.  The legacy auth.aliyundrive.com endpoint rejects
// otherwise valid refresh tokens created by the contemporary web login flow.
const TOKEN_URL: &str = "https://api.aliyundrive.com/token/refresh";
const API: &str = "https://api.aliyundrive.com";
const WEB_REFERER: &str = "https://www.aliyundrive.com/";
const SHARE_REFERER: &str = "https://www.alipan.com/";
const ALIYUN_TRANSIENT_RETRY_DELAYS_MS: [u64; 5] = [300, 700, 1_500, 3_000, 5_000];
pub(crate) const ALIYUN_WEB_TOKENS_JS: &str = r#"(() => {
 if (location.protocol !== 'https:' || !(['aliyundrive.com','alipan.com'].some(h => location.hostname === h || location.hostname.endsWith('.' + h)))) return null;
 const out = {}; const seen = new Set();
 const fields = {
  access_token: ['access_token', 'accessToken'], refresh_token: ['refresh_token', 'refreshToken'],
  default_drive_id: ['default_drive_id', 'defaultDriveId', 'drive_id', 'driveId']
 };
 function inspect(v, depth) {
  if (!v || depth > 6) return;
  if (typeof v === 'string') { try { inspect(JSON.parse(v), depth + 1); } catch (_) {} return; }
  if (typeof v !== 'object' || seen.has(v)) return; seen.add(v);
  for (const [k, x] of Object.entries(v)) {
   if (['expires_at','expiresAt'].includes(k) && Number.isFinite(Number(x))) out.expires_at = Number(x);
   if (['expire_time','expireTime'].includes(k) && typeof x === 'string' && Number.isFinite(Date.parse(x))) out.expires_at = Math.floor(Date.parse(x) / 1000);
   const target = Object.entries(fields).find(([, names]) => names.includes(k));
   if (target && typeof x === 'string' && x) out[target[0]] = x;
   else if (!/password/i.test(k)) inspect(x, depth + 1);
  }
 }
 for (const storage of [localStorage, sessionStorage]) for (let i = 0; i < storage.length; i++) inspect({[storage.key(i)]: storage.getItem(storage.key(i))}, 0);
 return out.access_token && out.refresh_token && out.default_drive_id ? out : null;
})()"#;
pub(crate) const ALIYUN_RECYCLE_BIN_ID: &str = "__versa_down_aliyun_recycle_bin__";
const ALIYUN_RECYCLE_BIN_NAME: &str = "回收站";
pub const ALIYUN_SHARE_PASS_CODE_CONSTRAINT: CloudSharePassCodeConstraint =
    CloudSharePassCodeConstraint {
        required: false,
        min_length: 1,
        max_length: 64,
        character_set: CloudSharePassCodeCharacterSet::Any,
        hint: "提取码可留空；填写时最多 64 个字符",
    };

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{
        share::{test_support::*, *},
        MemoryCredentialStore,
    };

    struct StatusHttp {
        replies: std::sync::Mutex<std::collections::VecDeque<CloudHttpResponse>>,
        requests: std::sync::Mutex<Vec<CloudHttpRequest>>,
    }

    impl CloudHttpTransport for StatusHttp {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(QuarkError::InvalidResponse)
        }
    }

    fn listing() -> Value {
        json!({"items":[{"file_id":"f1","name":"verify.mp4","type":"file","size":7}],"next_marker":""})
    }
    fn aliyun_capacity_entry() -> CloudShareResolvedEntry {
        entry(CloudSharePlan::new(
            CloudProvider::Aliyun,
            "capacity-share".into(),
            None,
            vec![CloudShareEntry::new(
                "capacity-share",
                "entry-1".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            json!({"share_token":"share-token"}),
        ))
    }

    fn aliyun_capacity_credentials() -> MemoryCredentialStore {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh","access_token":"personal-secret","drive_id":"drive-secret"}"#,
            )
            .unwrap();
        credentials
    }

    #[test]
    fn aliyun_share_transfer_capacity_uses_current_drive() {
        let credentials = aliyun_capacity_credentials();
        let http = Http::new(vec![json!({
            "total_size":"1099511627776",
            "used_size":1024
        })]);
        let adapter = AliyunShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        assert_eq!(
            adapter.transfer_capacity(&aliyun_capacity_entry()).unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 1_099_511_627_776,
                used_bytes: 1024
            }
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.ends_with("/v2/drive/get"));
        assert_eq!(
            requests[0].headers["Authorization"],
            "Bearer personal-secret"
        );
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].body.as_ref().unwrap()).unwrap()["drive_id"],
            "drive-secret"
        );
        assert!(!requests[0].url.ends_with("/v2/file/copy"));
    }

    #[test]
    fn aliyun_share_transfer_capacity_accepts_unlimited_drive() {
        let credentials = aliyun_capacity_credentials();
        let http = Http::new(vec![json!({"total_size":-1,"used_size":"1024"})]);
        let adapter = AliyunShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        assert_eq!(
            adapter.transfer_capacity(&aliyun_capacity_entry()).unwrap(),
            CloudTransferCapacity::Unlimited { used_bytes: 1024 }
        );
    }

    #[test]
    fn aliyun_share_transfer_capacity_rejects_invalid_bytes() {
        for response in [
            json!({"used_size":1}),
            json!({"total_size":100,"used_size":-1}),
            json!({"total_size":100,"used_size":101}),
        ] {
            let credentials = aliyun_capacity_credentials();
            let http = Http::new(vec![response]);
            assert!(AliyunShareAdapter {
                credentials: &credentials,
                http: &http
            }
            .transfer_capacity(&aliyun_capacity_entry())
            .is_err());
        }
    }

    #[test]
    fn aliyun_share_transfer_capacity_preserves_expired_authorization() {
        let credentials = aliyun_capacity_credentials();
        let http = StatusHttp {
            replies: std::sync::Mutex::new(
                [CloudHttpResponse {
                    status: 401,
                    body: "{}".into(),
                    ..Default::default()
                }]
                .into(),
            ),
            requests: std::sync::Mutex::new(vec![]),
        };
        assert!(matches!(
            AliyunShareAdapter {
                credentials: &credentials,
                http: &http
            }
            .transfer_capacity(&aliyun_capacity_entry()),
            Err(QuarkError::AuthorizationExpired)
        ));
    }
    #[test]
    fn aliyun_personal_share_list_uses_its_pds_resource_host() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh","access_token":"access","drive_id":"drive","browser_access_ready":true}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"user_id":"u1","nick_name":"alice"}),
            json!({"items":[],"next_marker":""}),
            json!({"domain_id":"bj29"}),
            json!({"items":[],"next_marker":""}),
        ]);

        assert!(AliyunClient::new(&credentials, &http)
            .list_shares()
            .unwrap()
            .is_empty());

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[3].url,
            "https://bj29.api.aliyunpds.com/v2/share_link/list"
        );
        let body: Value = serde_json::from_str(requests[3].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["creator"], "u1");
        assert_eq!(body["limit"], 100);
        assert_eq!(body["marker"], "");
        assert_eq!(body["include_cancelled"], false);
        assert!(body.get("drive_id").is_none());
    }

    #[test]
    fn aliyun_personal_download_uses_a_browser_contract_and_short_lived_url() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh","access_token":"access","drive_id":"drive","browser_access_ready":true}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"user_id":"u1","nick_name":"alice"}),
            json!({"items":[],"next_marker":""}),
            json!({"name":"file.txt"}),
            json!({"url":"https://cdn.example/file"}),
        ]);

        assert_eq!(
            AliyunClient::new(&credentials, &http)
                .download("file-id")
                .unwrap(),
            ("https://cdn.example/file".into(), "file.txt".into())
        );

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[3].url.ends_with("/v2/file/get_download_url"));
        let body: Value = serde_json::from_str(requests[3].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["drive_id"], "drive");
        assert_eq!(body["file_id"], "file-id");
        assert_eq!(body["file_name"], "file.txt");
        assert_eq!(body["expire_sec"], 900);
    }

    #[test]
    fn aliyun_personal_download_retries_via_its_pds_resource_host_when_consumer_url_is_empty() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh","access_token":"access","drive_id":"drive","browser_access_ready":true}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"user_id":"u1","nick_name":"alice"}),
            json!({"items":[],"next_marker":""}),
            json!({"name":"file.txt","domain_id":"bj29"}),
            json!({"url":"","cdn_url":""}),
            json!({"url":"https://cdn.example/file"}),
        ]);

        assert_eq!(
            AliyunClient::new(&credentials, &http)
                .download("file-id")
                .unwrap(),
            ("https://cdn.example/file".into(), "file.txt".into())
        );

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[3]
            .url
            .ends_with("api.aliyundrive.com/v2/file/get_download_url"));
        assert_eq!(
            requests[4].url,
            "https://bj29.api.aliyunpds.com/v2/file/get_download_url"
        );
        assert_eq!(requests[3].body, requests[4].body);
    }

    #[test]
    fn aliyun_personal_share_uses_its_pds_resource_host_with_explicit_empty_options() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh","access_token":"access","drive_id":"drive","browser_access_ready":true}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"user_id":"u1","nick_name":"alice"}),
            json!({"items":[],"next_marker":""}),
            json!({"domain_id":"bj29"}),
            json!({"share_id":"share-1","share_name":"test"}),
        ]);
        let options = CloudShareOptions {
            name: "test".into(),
            pass_code: None,
            generate_pass_code: false,
            expires_in_days: None,
        };

        let share = AliyunClient::new(&credentials, &http)
            .create_share(&["file-1".into()], &options)
            .unwrap();

        assert_eq!(share.id, "share-1");
        assert_eq!(share.url, "https://www.alipan.com/s/share-1");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[3].url,
            "https://bj29.api.aliyunpds.com/v2/share_link/create"
        );
        let body: Value = serde_json::from_str(requests[3].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["drive_id"], "drive");
        assert_eq!(body["file_id_list"], json!(["file-1"]));
        assert_eq!(body["share_name"], "test");
        assert_eq!(body["share_pwd"], "");
        assert_eq!(body["expiration"], "");
    }

    #[test]
    fn aliyun_personal_share_cancel_uses_its_pds_resource_host() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh","access_token":"access","drive_id":"drive","browser_access_ready":true}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"user_id":"u1","nick_name":"alice"}),
            json!({"items":[],"next_marker":""}),
            json!({"domain_id":"bj29"}),
            json!({}),
        ]);

        AliyunClient::new(&credentials, &http)
            .cancel_shares(&["share-1".into()])
            .unwrap();

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[3].url,
            "https://bj29.api.aliyunpds.com/v2/share_link/cancel"
        );
        let body: Value = serde_json::from_str(requests[3].body.as_deref().unwrap()).unwrap();
        assert_eq!(body, json!({"share_id":"share-1"}));
    }

    #[test]
    fn aliyun_share_reuses_the_inspected_token_without_a_personal_bearer() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "aliyun",
                br#"{"refresh_token":"refresh-secret","access_token":"personal-secret","drive_id":"drive-secret"}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"share_token":"share-secret"}),
            listing(),
            json!({"url":"https://cdn.example/file"}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Aliyun, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Aliyun)).unwrap()
        else {
            panic!()
        };
        let ShareResolution::Direct(direct) = adapter.resolve(&entry(plan)).unwrap() else {
            panic!()
        };
        assert!(direct
            .headers
            .contains(&("x-share-token".into(), "share-secret".into())));
        assert!(!direct
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")));
        let requests = http.requests.lock().unwrap();
        assert!(requests[..2].iter().all(|request| !request
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))));
        assert_eq!(
            requests[2].headers.get("Authorization").map(String::as_str),
            Some("Bearer personal-secret")
        );
        assert_eq!(requests.len(), 3);
        assert!(requests[2].url.ends_with("/v2/file/get_download_url"));
        assert_eq!(
            requests[2].headers.get("Referer").map(String::as_str),
            Some(SHARE_REFERER)
        );
        let download_body: Value =
            serde_json::from_str(requests[2].body.as_ref().unwrap()).unwrap();
        assert_eq!(download_body["share_id"], "share123");
        assert_eq!(download_body["file_id"], "f1");
        assert!(download_body.get("drive_id").is_none());
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].body.as_ref().unwrap()).unwrap()["share_pwd"],
            "a123"
        );
    }

    #[test]
    fn aliyun_share_refreshes_an_expired_inspected_token_once() {
        let credentials = MemoryCredentialStore::default();
        let http = Http::new(vec![
            json!({"code":"ShareLinkTokenExpired"}),
            json!({"share_token":"renewed-token"}),
            json!({"url":"https://cdn.example/file"}),
        ]);
        let plan = CloudSharePlan::new(
            CloudProvider::Aliyun,
            "share123".into(),
            Some("a123".into()),
            vec![CloudShareEntry::new(
                "share123",
                "f1".into(),
                "file.txt".into(),
                vec![],
                Some(7),
            )],
            json!({"share_token":"expired-token"}),
        );
        let adapter = provider_share_adapter(CloudProvider::Aliyun, &credentials, &http).unwrap();

        let resolved = adapter.resolve_with_context(&entry(plan)).unwrap();

        assert!(matches!(resolved.result, Ok(ShareResolution::Direct(_))));
        assert_eq!(
            resolved.updated_context.unwrap()["share_token"],
            "renewed-token"
        );
        assert_eq!(http.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn aliyun_share_recurses_and_pages_and_rejects_cycles() {
        let credentials = MemoryCredentialStore::default();
        let http = Http::new(vec![
            json!({"share_token":"s"}),
            json!({"items":[{"file_id":"d","name":"dir","type":"folder"}],"next_marker":"page2"}),
            json!({"items":[],"next_marker":""}),
            listing(),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Aliyun, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Aliyun)).unwrap()
        else {
            panic!()
        };
        assert_eq!(plan.entries[0].relative_directory, vec!["dir"]);
        let requests = http.requests.lock().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(requests[2].body.as_ref().unwrap()).unwrap()["marker"],
            "page2"
        );
        drop(requests);
        let http = Http::new(vec![
            json!({"share_token":"s"}),
            json!({"items":[],"next_marker":"same"}),
            json!({"items":[],"next_marker":"same"}),
        ]);
        assert!(
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Aliyun))
                .is_err()
        );
    }
    #[test]
    fn aliyun_share_access_code_and_expiry_fail_closed() {
        let credentials = MemoryCredentialStore::default();
        let mut input = candidate(CloudProvider::Aliyun);
        input.access_code = None;
        let http = Http::new(vec![
            json!({"code":"ShareLink.PasswordRequired","message":"secret"}),
        ]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &http)
                .unwrap()
                .inspect(&input),
            Ok(ShareInspection::AccessCodeRequired)
        ));
        for code in [
            "ShareLink.PasswordMismatch",
            "ShareLink.Cancelled",
            "ShareLink.Expired",
            "Forbidden",
            "TooManyRequests",
            "ShareLinkTokenInvalid",
        ] {
            let http = Http::new(vec![json!({"code":code,"message":"secret"})]);
            let result = provider_share_adapter(CloudProvider::Aliyun, &credentials, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Aliyun));
            assert!(result.is_err(), "accepted {code}");
            assert_eq!(http.requests.lock().unwrap().len(), 1);
        }
        for value in [
            json!({}),
            json!({"download_url":""}),
            json!({"download_url":"https://www.alipan.com/s/other"}),
            json!({"code":"Forbidden","download_url":"https://cdn.example/f"}),
        ] {
            let http = Http::new(vec![json!({"share_token":"s"}), listing(), value]);
            let adapter =
                provider_share_adapter(CloudProvider::Aliyun, &credentials, &http).unwrap();
            let ShareInspection::Plan(plan) =
                adapter.inspect(&candidate(CloudProvider::Aliyun)).unwrap()
            else {
                panic!()
            };
            assert!(adapter.resolve(&entry(plan)).is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 3);
        }
    }

    #[test]
    fn aliyun_share_invalid_resource_password_requests_an_access_code() {
        let credentials = MemoryCredentialStore::default();
        let missing_code = Http::new(vec![json!({"code":"InvalidResource.SharePwd"})]);
        let mut without_code = candidate(CloudProvider::Aliyun);
        without_code.access_code = None;
        assert!(matches!(
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &missing_code)
                .unwrap()
                .inspect(&without_code),
            Ok(ShareInspection::AccessCodeRequired)
        ));

        let supplied_code = Http::new(vec![json!({"code":"InvalidResource.SharePwd"})]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &supplied_code)
                .unwrap()
                .inspect(&candidate(CloudProvider::Aliyun)),
            Err(QuarkError::InvalidInput(_))
        ));
    }

    #[test]
    fn aliyun_share_rejects_malformed_error_fields_even_with_a_url() {
        let credentials = MemoryCredentialStore::default();
        for response in [
            json!({"error_code":"Forbidden","url":"https://cdn.example/f"}),
            json!({"error":{"code":"Forbidden"},"url":"https://cdn.example/f"}),
            json!({"status":"403","url":"https://cdn.example/f"}),
        ] {
            let http = Http::new(vec![json!({"share_token":"s"}), listing(), response]);
            let adapter =
                provider_share_adapter(CloudProvider::Aliyun, &credentials, &http).unwrap();
            let ShareInspection::Plan(plan) =
                adapter.inspect(&candidate(CloudProvider::Aliyun)).unwrap()
            else {
                panic!()
            };
            assert!(adapter.resolve(&entry(plan)).is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 3);
        }
    }

    #[test]
    fn aliyun_share_retries_block_exception_from_initial_token_request() {
        let credentials = MemoryCredentialStore::default();
        let http = StatusHttp {
            replies: std::sync::Mutex::new(
                vec![
                    CloudHttpResponse {
                        status: 429,
                        body: json!({"code":"BlockException"}).to_string(),
                        set_cookies: vec![],
                    },
                    CloudHttpResponse {
                        status: 200,
                        body: json!({"share_token":"share-token"}).to_string(),
                        set_cookies: vec![],
                    },
                    CloudHttpResponse {
                        status: 200,
                        body: listing().to_string(),
                        set_cookies: vec![],
                    },
                ]
                .into(),
            ),
            requests: std::sync::Mutex::new(vec![]),
        };

        let ShareInspection::Plan(plan) =
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Aliyun))
                .unwrap()
        else {
            panic!();
        };
        assert_eq!(plan.entries.len(), 1);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].url.ends_with("/v2/share_link/get_share_token"));
        assert_eq!(requests[0].url, requests[1].url);
        assert_eq!(requests[0].body, requests[1].body);
    }

    #[test]
    fn aliyun_share_retries_transport_timeout_from_initial_token_request() {
        struct ResultHttp {
            replies:
                std::sync::Mutex<std::collections::VecDeque<Result<CloudHttpResponse, QuarkError>>>,
            requests: std::sync::Mutex<Vec<CloudHttpRequest>>,
        }
        impl CloudHttpTransport for ResultHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.requests.lock().unwrap().push(request);
                self.replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Err(QuarkError::InvalidResponse))
            }
        }

        let credentials = MemoryCredentialStore::default();
        let http = ResultHttp {
            replies: std::sync::Mutex::new(
                vec![
                    Err(QuarkError::Transport("response read timed out".into())),
                    Ok(CloudHttpResponse {
                        status: 200,
                        body: json!({"share_token":"share-token"}).to_string(),
                        set_cookies: vec![],
                    }),
                    Ok(CloudHttpResponse {
                        status: 200,
                        body: listing().to_string(),
                        set_cookies: vec![],
                    }),
                ]
                .into(),
            ),
            requests: std::sync::Mutex::new(vec![]),
        };

        let ShareInspection::Plan(plan) =
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Aliyun))
                .unwrap()
        else {
            panic!();
        };
        assert_eq!(plan.entries.len(), 1);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].url, requests[1].url);
        assert_eq!(requests[0].body, requests[1].body);
    }

    #[test]
    fn aliyun_copy_does_not_retry_an_ambiguous_transport_timeout() {
        struct TransportHttp(std::sync::atomic::AtomicUsize);
        impl CloudHttpTransport for TransportHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(QuarkError::Transport("response read timed out".into()))
            }
        }

        let credentials = MemoryCredentialStore::default();
        let http = TransportHttp(std::sync::atomic::AtomicUsize::new(0));
        let adapter = AliyunShareAdapter {
            credentials: &credentials,
            http: &http,
        };

        assert!(matches!(
            adapter.raw_with_account_bearer(
                "/v2/file/copy",
                Some("share-token"),
                json!({"share_id":"share","file_id":"file"}),
                true,
            ),
            Err(QuarkError::Transport(_))
        ));
        assert_eq!(http.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn aliyun_personal_download_url_transport_timeout_has_one_retry_layer() {
        struct TransportHttp(std::sync::atomic::AtomicUsize);
        impl CloudHttpTransport for TransportHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(QuarkError::Transport("response read timed out".into()))
            }
        }

        let credentials = MemoryCredentialStore::default();
        let http = TransportHttp(std::sync::atomic::AtomicUsize::new(0));
        let adapter = AliyunShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        let session = AliyunSession {
            refresh_token: "refresh".into(),
            access_token: "access".into(),
            drive_id: "drive".into(),
            browser_access_ready: true,
            expires_at: None,
        };

        assert!(matches!(
            adapter.personal_download(&session, "file", "file.bin"),
            Err(QuarkError::Transport(_))
        ));
        assert_eq!(
            http.0.load(std::sync::atomic::Ordering::SeqCst),
            ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len() + 1
        );
    }

    #[test]
    fn aliyun_personal_download_url_block_exception_has_one_retry_layer() {
        struct BlockHttp(std::sync::atomic::AtomicUsize);
        impl CloudHttpTransport for BlockHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CloudHttpResponse {
                    status: 429,
                    body: json!({"code":"BlockException"}).to_string(),
                    set_cookies: vec![],
                })
            }
        }

        let credentials = MemoryCredentialStore::default();
        let http = BlockHttp(std::sync::atomic::AtomicUsize::new(0));
        let adapter = AliyunShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        let session = AliyunSession {
            refresh_token: "refresh".into(),
            access_token: "access".into(),
            drive_id: "drive".into(),
            browser_access_ready: true,
            expires_at: None,
        };

        assert!(matches!(
            adapter.personal_download(&session, "file", "file.bin"),
            Err(QuarkError::Provider(_))
        ));
        assert_eq!(
            http.0.load(std::sync::atomic::Ordering::SeqCst),
            ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len() + 1
        );
    }

    #[test]
    fn aliyun_distinguishes_missing_recorded_files_from_new_copies() {
        struct RecycleBinHttp(std::sync::atomic::AtomicUsize);
        impl CloudHttpTransport for RecycleBinHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CloudHttpResponse {
                    status: 403,
                    body: json!({"code":"ForbiddenFileInTheRecycleBin"}).to_string(),
                    set_cookies: vec![],
                })
            }
        }

        let credentials = MemoryCredentialStore::default();
        let http = RecycleBinHttp(std::sync::atomic::AtomicUsize::new(0));
        let adapter = AliyunShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        let session = AliyunSession {
            refresh_token: "refresh".into(),
            access_token: "access".into(),
            drive_id: "drive".into(),
            browser_access_ready: true,
            expires_at: None,
        };

        assert!(matches!(
            adapter.personal_download(&session, "file", "file.bin"),
            Err(QuarkError::SourceFileMissing)
        ));
        assert_eq!(http.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            adapter.personal_download_after_copy(&session, "file", "file.bin"),
            Err(QuarkError::ShareTransferDownloadUnavailable)
        ));
    }

    #[test]
    fn aliyun_share_retries_consecutive_block_exceptions_while_listing() {
        let credentials = MemoryCredentialStore::default();
        let http = StatusHttp {
            replies: std::sync::Mutex::new(
                vec![
                    CloudHttpResponse {
                        status: 200,
                        body: json!({"share_token":"share-token"}).to_string(),
                        set_cookies: vec![],
                    },
                    CloudHttpResponse {
                        status: 200,
                        body: json!({"items":[{"file_id":"dir","name":"folder","type":"folder"}],"next_marker":""}).to_string(),
                        set_cookies: vec![],
                    },
                    CloudHttpResponse {
                        status: 429,
                        body: json!({"code":"BlockException"}).to_string(),
                        set_cookies: vec![],
                    },
                    CloudHttpResponse {
                        status: 429,
                        body: json!({"code":"BlockException"}).to_string(),
                        set_cookies: vec![],
                    },
                    CloudHttpResponse {
                        status: 200,
                        body: listing().to_string(),
                        set_cookies: vec![],
                    },
                ]
                .into(),
            ),
            requests: std::sync::Mutex::new(vec![]),
        };

        let ShareInspection::Plan(plan) =
            provider_share_adapter(CloudProvider::Aliyun, &credentials, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Aliyun))
                .unwrap()
        else {
            panic!();
        };
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].relative_directory, vec!["folder"]);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[2].url.ends_with("/adrive/v3/file/list"));
        assert_eq!(requests[2].url, requests[3].url);
        assert_eq!(requests[3].url, requests[4].url);
        assert_eq!(requests[2].body, requests[3].body);
        assert_eq!(requests[3].body, requests[4].body);
    }
}

/// The minimum durable state obtained after the user finishes the official web login.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AliyunSession {
    pub refresh_token: String,
    pub access_token: String,
    pub drive_id: String,
    /// The current bearer was captured immediately after the official web login.
    #[serde(default)]
    pub browser_access_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

pub struct AliyunClient<'a, S: CredentialStore + ?Sized, T: CloudHttpTransport + ?Sized> {
    store: &'a S,
    http: &'a T,
}

impl<'a, S: CredentialStore + ?Sized, T: CloudHttpTransport + ?Sized> AliyunClient<'a, S, T> {
    pub fn new(store: &'a S, http: &'a T) -> Self {
        Self { store, http }
    }

    fn stored(&self) -> Result<AliyunSession, QuarkError> {
        let bytes = self
            .store
            .load("aliyun")?
            .ok_or(QuarkError::AuthorizationExpired)?;
        let session: AliyunSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.refresh_token.is_empty()
            || session.access_token.is_empty()
            || session.drive_id.is_empty()
        {
            return Err(QuarkError::AuthorizationExpired);
        }
        Ok(session)
    }

    fn save(&self, session: &AliyunSession) -> Result<(), QuarkError> {
        if session.refresh_token.is_empty()
            || session.access_token.is_empty()
            || session.drive_id.is_empty()
        {
            return Err(QuarkError::AuthorizationExpired);
        }
        self.store.save(
            "aliyun",
            &serde_json::to_vec(session).map_err(|_| QuarkError::Storage)?,
        )
    }

    fn response(&self, response: &CloudHttpResponse) -> Result<Value, QuarkError> {
        // Token refresh failures use a 400 body with string error codes. Parse it
        // before the shared numeric-code mapper turns it into a generic provider
        // error, so callers can return to the official web authorization flow.
        if response.status == 400 {
            if let Ok(value) = serde_json::from_str::<Value>(&response.body) {
                if matches!(
                    value.get("code").and_then(Value::as_str),
                    Some("RefreshTokenExpired" | "InvalidParameter.RefreshToken")
                ) {
                    return Err(QuarkError::AuthorizationExpired);
                }
            }
        }
        if aliyun_source_file_missing(response) {
            return Err(QuarkError::SourceFileMissing);
        }
        let value = response_json(response)?;
        let code = value.get("code").and_then(Value::as_str).unwrap_or("");
        if matches!(
            code,
            "AccessTokenInvalid"
                | "AccessTokenExpired"
                | "InvalidParameter.AccessToken"
                | "InvalidToken"
        ) {
            return Err(QuarkError::AuthorizationExpired);
        }
        if !code.is_empty() {
            return Err(QuarkError::Provider("阿里云盘拒绝请求".into()));
        }
        Ok(value)
    }

    fn send(
        &self,
        method: &'static str,
        url: String,
        headers: BTreeMap<String, String>,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers,
            body: body.map(|body| body.to_string()),
        })?;
        self.response(&response)
    }

    fn refresh(&self, previous: &AliyunSession) -> Result<AliyunSession, QuarkError> {
        let value = self.send(
            "POST",
            TOKEN_URL.into(),
            BTreeMap::from([
                ("Referer".into(), WEB_REFERER.into()),
                ("X-Requested-With".into(), "XMLHttpRequest".into()),
            ]),
            Some(json!({
                "grant_type": "refresh_token",
                "refresh_token": previous.refresh_token,
            })),
        )?;
        Ok(AliyunSession {
            refresh_token: required(&value, "refresh_token")?,
            access_token: required(&value, "access_token")?,
            drive_id: value
                .get("default_drive_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .unwrap_or(&previous.drive_id)
                .to_owned(),
            browser_access_ready: false,
            expires_at: value["expires_in"]
                .as_u64()
                .filter(|ttl| *ttl > 0)
                .map(|ttl| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .saturating_add(ttl)
                }),
        })
    }

    fn refresh_and_persist(&self, previous: &AliyunSession) -> Result<AliyunSession, QuarkError> {
        let refreshed = self.refresh(previous)?;
        self.save(&refreshed)?;
        Ok(refreshed)
    }
    pub(crate) fn refresh_session(&self) -> Result<(), QuarkError> {
        self.refresh_and_persist(&self.stored()?).map(|_| ())
    }

    fn bearer_headers(session: &AliyunSession) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "Authorization".into(),
                format!("Bearer {}", session.access_token),
            ),
            ("Referer".into(), WEB_REFERER.into()),
        ])
    }

    fn api(&self, session: &AliyunSession, path: &str, body: Value) -> Result<Value, QuarkError> {
        self.send(
            "POST",
            format!("{API}{path}"),
            Self::bearer_headers(session),
            Some(body),
        )
    }

    fn directory_with(
        &self,
        session: &AliyunSession,
        parent: &str,
    ) -> Result<CloudDirectory, QuarkError> {
        let mut marker = String::new();
        let mut items = Vec::new();
        for _ in 0..10_000 {
            let value = self.api(
                session,
                "/adrive/v3/file/list",
                json!({
                    "drive_id": session.drive_id,
                    "parent_file_id": parent,
                    "limit": 200,
                    "marker": marker,
                }),
            )?;
            let page = value
                .get("items")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            for file in page {
                items.push(parse_file(file, parent)?);
            }
            marker = value
                .get("next_marker")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if marker.is_empty() {
                if parent == "root" {
                    items.push(CloudFile {
                        id: ALIYUN_RECYCLE_BIN_ID.into(),
                        parent_id: "root".into(),
                        name: ALIYUN_RECYCLE_BIN_NAME.into(),
                        kind: "folder".into(),
                        size: None,
                        modified_at: None,
                    });
                }
                return Ok(CloudDirectory {
                    id: parent.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    /// Accept an official browser session before rotating its refresh token.
    /// The access token was just issued by the authenticated web page, and
    /// validating it directly prevents a needless first refresh from trapping a
    /// successful login in its authorization window.
    pub fn authorize_browser_session(
        &self,
        mut session: AliyunSession,
    ) -> Result<CloudAccount, QuarkError> {
        if session.refresh_token.trim().is_empty()
            || session.access_token.trim().is_empty()
            || session.drive_id.trim().is_empty()
        {
            return Err(QuarkError::InvalidInput("网页登录会话不完整".into()));
        }
        let account = self.account_with(&session)?;
        session.browser_access_ready = true;
        self.save(&session)?;
        Ok(account)
    }

    /// A refresh token may be single-use. Validate the refreshed replacement
    /// completely and persist it exactly once before any caller-visible operation.
    fn refreshed_validated_session(
        &self,
    ) -> Result<(AliyunSession, CloudAccount, CloudDirectory), QuarkError> {
        let previous = self.stored()?;
        if !self.store.automatic_session_recovery() {
            let (account, root) = self.validate(&previous)?;
            return Ok((previous, account, root));
        }
        if previous.browser_access_ready {
            match self.validate(&previous) {
                Ok((account, root)) => return Ok((previous, account, root)),
                Err(QuarkError::AuthorizationExpired) => {}
                Err(error) => return Err(error),
            }
        }
        let session = self.refresh(&previous)?;
        let (account, root) = self.validate(&session)?;
        self.save(&session)?;
        Ok((session, account, root))
    }

    fn validate(
        &self,
        session: &AliyunSession,
    ) -> Result<(CloudAccount, CloudDirectory), QuarkError> {
        let account = self.account_with(session)?;
        let root = self.directory_with(session, "root")?;
        Ok((account, root))
    }

    fn account_with(&self, session: &AliyunSession) -> Result<CloudAccount, QuarkError> {
        let value = self.api(session, "/adrive/v2/user/get", json!({}))?;
        let non_empty = |keys: &[&str]| {
            keys.iter().find_map(|key| {
                value
                    .get(*key)
                    .and_then(Value::as_str)
                    .filter(|field| !field.is_empty())
            })
        };
        let id = non_empty(&["user_id", "id"]).ok_or(QuarkError::InvalidResponse)?;
        let display_name = non_empty(&["nick_name", "name", "user_name", "phone"])
            .ok_or(QuarkError::InvalidResponse)?;
        Ok(CloudAccount {
            provider: CloudProvider::Aliyun,
            id: id.into(),
            display_name: display_name.into(),
            auth_method: CloudAuthMethod::Web,
        })
    }

    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let (_, account, _) = self.refreshed_validated_session()?;
        Ok(account)
    }

    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let session = self.stored()?;
        let value = self.api(
            &session,
            "/v2/drive/get",
            json!({"drive_id": session.drive_id}),
        )?;
        CloudStorageCapacity::from_json(&value["total_size"], &value["used_size"], true)
    }

    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
        }
        if file_ids.len() > 100 {
            return Err(QuarkError::InvalidInput(
                "阿里云盘单个分享最多支持 100 个文件或目录".into(),
            ));
        }
        if options.generate_pass_code {
            return Err(QuarkError::InvalidInput(
                "阿里云盘不支持自动生成提取码，请手动填写或留空".into(),
            ));
        }
        let (session, _, _) = self.refreshed_validated_session()?;
        let mut body = json!({
            "drive_id": session.drive_id,
            "file_id_list": file_ids,
            "share_name": options.name,
            "share_pwd": options.pass_code.as_deref().unwrap_or(""),
            "expiration": "",
        });
        if let Some(days) = options.expires_in_days {
            let expiration =
                time::OffsetDateTime::now_utc() + time::Duration::days(i64::from(days));
            body["expiration"] = json!(expiration
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| QuarkError::InvalidResponse)?);
        }
        let metadata = self.api(
            &session,
            "/v2/file/get",
            json!({"drive_id":session.drive_id,"file_id":file_ids[0]}),
        )?;
        let resource_api = pds_resource_api(&metadata)?;
        let value = self.send(
            "POST",
            format!("{resource_api}/v2/share_link/create"),
            Self::bearer_headers(&session),
            Some(body),
        )?;
        aliyun_share_from_value(&value, Some(file_ids), options.pass_code.as_deref())
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let (session, account, _) = self.refreshed_validated_session()?;
        let profile = self.api(&session, "/v2/user/get", json!({}))?;
        let resource_api = pds_resource_api(&profile)?;
        let mut marker = String::new();
        let mut shares = Vec::new();
        for _ in 0..10_000 {
            let value = self.send(
                "POST",
                format!("{resource_api}/v2/share_link/list"),
                Self::bearer_headers(&session),
                Some(json!({
                    "creator": account.id,
                    "limit": 100,
                    "marker": marker,
                    "include_cancelled": false,
                })),
            )?;
            let page = value
                .get("items")
                .or_else(|| value.get("share_links"))
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            shares.extend(
                page.iter()
                    .map(|share| aliyun_share_from_value(share, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            marker = value
                .get("next_marker")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if marker.is_empty() {
                return Ok(shares);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        if share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        let (session, _, _) = self.refreshed_validated_session()?;
        let profile = self.api(&session, "/v2/user/get", json!({}))?;
        let resource_api = pds_resource_api(&profile)?;
        for share_id in share_ids {
            self.send(
                "POST",
                format!("{resource_api}/v2/share_link/cancel"),
                Self::bearer_headers(&session),
                Some(json!({"share_id": share_id})),
            )?;
        }
        Ok(())
    }

    pub fn directory(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        if parent == ALIYUN_RECYCLE_BIN_ID {
            return self.list_recycle_bin();
        }
        let session = self.stored()?;
        let first = self.directory_with(&session, parent);
        match first {
            Err(QuarkError::AuthorizationExpired) if self.store.automatic_session_recovery() => {
                let refreshed = self.refresh_and_persist(&session)?;
                self.directory_with(&refreshed, parent)
            }
            result => result,
        }
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<CloudFile, QuarkError> {
        let (session, _, _) = self.refreshed_validated_session()?;
        let value = self.api(
            &session,
            "/v2/file/create",
            json!({
                "drive_id": session.drive_id,
                "parent_file_id": parent,
                "name": name,
                "type": "folder",
                "check_name_mode": "refuse",
            }),
        )?;
        parse_file(&value, parent)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), QuarkError> {
        let (session, _, _) = self.refreshed_validated_session()?;
        self.api(
            &session,
            "/v3/file/update",
            json!({"drive_id":session.drive_id,"file_id":id,"name":name,"check_name_mode":"refuse"}),
        )?;
        Ok(())
    }

    fn files_action(
        &self,
        path: &str,
        ids: &[String],
        parent: Option<&str>,
    ) -> Result<(), QuarkError> {
        if ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件".into()));
        }
        let (session, _, _) = self.refreshed_validated_session()?;
        for id in ids {
            let mut body = json!({"drive_id":session.drive_id,"file_id":id});
            if let Some(parent) = parent {
                body["to_parent_file_id"] = Value::String(parent.into());
            }
            let response = self.http.send(CloudHttpRequest {
                method: "POST",
                url: format!("{API}{path}"),
                headers: Self::bearer_headers(&session),
                body: Some(body.to_string()),
            })?;
            let already_absent = path == "/v2/recyclebin/trash"
                && response.status == 404
                && serde_json::from_str::<Value>(&response.body)
                    .ok()
                    .is_some_and(|value| {
                        value.get("code").and_then(Value::as_str) == Some("NotFound.FileId")
                    });
            if !already_absent {
                self.response(&response)?;
            }
        }
        Ok(())
    }

    pub fn move_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.files_action("/v2/file/move", ids, Some(parent))
    }

    pub fn copy_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.files_action("/v2/file/copy", ids, Some(parent))
    }

    pub fn trash(&self, ids: &[String]) -> Result<(), QuarkError> {
        self.files_action("/v2/recyclebin/trash", ids, None)
    }

    pub fn list_recycle_bin(&self) -> Result<CloudDirectory, QuarkError> {
        let (session, _, _) = self.refreshed_validated_session()?;
        let mut marker = String::new();
        let mut items = Vec::new();
        for _ in 0..10_000 {
            let value = self.api(
                &session,
                "/v2/recyclebin/list",
                json!({
                    "drive_id": session.drive_id,
                    "limit": 200,
                    "marker": marker,
                }),
            )?;
            let page = value
                .get("items")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            for file in page {
                let mut file = parse_file(file, ALIYUN_RECYCLE_BIN_ID)?;
                file.parent_id = ALIYUN_RECYCLE_BIN_ID.into();
                items.push(file);
            }
            marker = value
                .get("next_marker")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if marker.is_empty() {
                return Ok(CloudDirectory {
                    id: ALIYUN_RECYCLE_BIN_ID.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn restore_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        self.files_action("/v2/recyclebin/restore", ids, None)
    }

    pub fn delete_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        self.files_action("/v2/file/delete", ids, None)
    }

    /// Initialize a one-part upload; the caller transfers returned upload URLs and
    /// completes the upload separately, keeping those transient URLs out of storage.
    pub fn upload_init(&self, parent: &str, name: &str, size: u64) -> Result<Value, QuarkError> {
        let (session, _, _) = self.refreshed_validated_session()?;
        let value = self.api(
            &session,
            "/v2/file/create",
            json!({
                "drive_id": session.drive_id,
                "parent_file_id": parent,
                "name": name,
                "type": "file",
                "size": size,
                "check_name_mode": "refuse",
                "part_info_list": [{"part_number": 1}],
            }),
        )?;
        validate_upload_urls(&value)?;
        Ok(value)
    }

    pub fn upload(&self, parent: &str, name: &str, size: u64) -> Result<Value, QuarkError> {
        self.upload_init(parent, name, size)
    }

    /// Always asks the service for a fresh short-lived download URL.
    pub fn download(&self, id: &str) -> Result<(String, String), QuarkError> {
        let (session, _, _) = self.refreshed_validated_session()?;
        self.download_with_session(&session, id)
    }

    fn download_with_session(
        &self,
        session: &AliyunSession,
        id: &str,
    ) -> Result<(String, String), QuarkError> {
        let metadata = self.api(
            session,
            "/v2/file/get",
            json!({"drive_id":session.drive_id,"file_id":id}),
        )?;
        let name = required(&metadata, "name")?;
        let body =
            json!({"drive_id":session.drive_id,"file_id":id,"file_name":name,"expire_sec":900});
        let value = self.api(session, "/v2/file/get_download_url", body.clone())?;
        if let Some(url) = direct_download_url(&value) {
            return Ok((url, name));
        }

        // The consumer gateway can acknowledge the request while returning an
        // empty URL. Retry only against the file's own official PDS resource
        // host, which is bounded to the server-provided domain label.
        let resource_api = pds_resource_api(&metadata)?;
        let fallback = self.send(
            "POST",
            format!("{resource_api}/v2/file/get_download_url"),
            Self::bearer_headers(session),
            Some(body),
        )?;
        direct_download_url(&fallback)
            .map(|url| (url, name))
            .ok_or(QuarkError::InvalidResponse)
    }
}

fn direct_download_url(value: &Value) -> Option<String> {
    ["url", "download_url"]
        .iter()
        .filter_map(|key| value[*key].as_str())
        .find(|url| url.starts_with("https://"))
        .map(str::to_owned)
}

fn pds_resource_api(metadata: &Value) -> Result<String, QuarkError> {
    let domain = required(metadata, "domain_id")?;
    let valid_label = !domain.is_empty()
        && domain.len() <= 63
        && !domain.starts_with('-')
        && !domain.ends_with('-')
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid_label {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(format!("https://{domain}.api.aliyunpds.com"))
}

fn aliyun_source_file_missing(response: &CloudHttpResponse) -> bool {
    serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| value.get("code").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|code| {
            matches!(
                (response.status, code.as_str()),
                (404, "NotFound.File" | "NotFound.FileId") | (_, "ForbiddenFileInTheRecycleBin")
            )
        })
}

fn aliyun_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = required(value, "share_id")?;
    let source_file_ids = value
        .get("file_id_list")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
        .unwrap_or_default();
    Ok(CloudShare {
        id: id.clone(),
        url: value
            .get("share_url")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://www.alipan.com/s/{id}")),
        name: value
            .get("share_name")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("分享文件")
            .into(),
        kind: if value.get("type").and_then(Value::as_str) == Some("folder") {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids,
        pass_code: value
            .get("share_pwd")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at: value
            .get("expiration")
            .and_then(Value::as_str)
            .filter(|time| !time.is_empty())
            .map(str::to_owned),
    })
}

impl<'a, S: CredentialStore, T: CloudHttpTransport + CloudUploadTransport> AliyunClient<'a, S, T> {
    /// Upload a regular local file through the short-lived HTTPS part URLs, then
    /// tell Aliyun Drive to publish the completed object.  The temporary URLs are
    /// intentionally scoped to this call and never enter the credential store.
    pub fn upload_file(&self, parent: &str, path: &Path) -> Result<CloudFile, QuarkError> {
        let snapshot = super::UploadFileSnapshot::capture(path)?;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(QuarkError::InvalidInput("请选择普通文件上传".into()));
        }
        let name = path
            .file_name()
            .ok_or_else(|| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?
            .to_string_lossy()
            .into_owned();
        let size = metadata.len();
        let parts = upload_part_count(size)?;
        let (session, _, _) = self.refreshed_validated_session()?;
        let init = self.api(
            &session,
            "/v2/file/create",
            json!({
                "drive_id": session.drive_id,
                "parent_file_id": parent,
                "name": name,
                "type": "file",
                "size": size,
                "check_name_mode": "refuse",
                "part_info_list": (1..=parts).map(|part_number| json!({"part_number": part_number})).collect::<Vec<_>>(),
            }),
        )?;
        validate_upload_urls(&init)?;
        let file_id = required(&init, "file_id")?;
        let upload_id = required(&init, "upload_id")?;
        let upload_parts = upload_parts(&init, parts)?;

        snapshot.validate()?;
        let mut remaining = size;
        for part in upload_parts {
            let length = remaining.min(ALIYUN_PART_SIZE);
            let body = snapshot.slice(size - remaining, length)?;
            remaining = remaining.saturating_sub(length);
            let mut headers = BTreeMap::new();
            if let Some(content_type) = part.content_type {
                headers.insert("Content-Type".into(), content_type);
            }
            let response = self.http.send_upload(UploadHttpRequest {
                method: "PUT",
                url: part.url,
                headers,
                body,
            })?;
            if !(200..300).contains(&response.status) {
                return Err(QuarkError::Provider(format!(
                    "阿里云盘上传分片失败（HTTP {}），请重试",
                    response.status
                )));
            }
        }
        if remaining != 0 {
            return Err(QuarkError::InvalidInput("无法读取选中的本地文件".into()));
        }
        snapshot.validate()?;
        self.api(
            &session,
            "/v2/file/complete",
            json!({"drive_id": session.drive_id, "file_id": file_id, "upload_id": upload_id}),
        )?;
        parse_file(&init, parent)
    }
}

const ALIYUN_PART_SIZE: u64 = 10 * 1024 * 1024;

fn upload_part_count(size: u64) -> Result<u64, QuarkError> {
    let count = size.saturating_add(ALIYUN_PART_SIZE - 1) / ALIYUN_PART_SIZE;
    let count = count.max(1);
    if count > 10_000 {
        return Err(QuarkError::InvalidInput(
            "文件过大，无法安全分片上传".into(),
        ));
    }
    Ok(count)
}

struct UploadPart {
    url: String,
    content_type: Option<String>,
}

fn upload_parts(value: &Value, expected_parts: u64) -> Result<Vec<UploadPart>, QuarkError> {
    let parts = value
        .get("part_info_list")
        .and_then(Value::as_array)
        .filter(|parts| parts.len() == expected_parts as usize)
        .ok_or(QuarkError::InvalidResponse)?;
    let mut upload_parts = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        if part.get("part_number").and_then(Value::as_u64) != Some(index as u64 + 1) {
            return Err(QuarkError::InvalidResponse);
        }
        let url = part
            .get("upload_url")
            .and_then(Value::as_str)
            .filter(|url| secure_temporary_url(url))
            .ok_or(QuarkError::InvalidResponse)?;
        let content_type = match part.get("content_type") {
            None => None,
            Some(Value::String(value)) if value.is_empty() => None,
            Some(Value::String(value)) if !value.contains(['\r', '\n']) => Some(value.to_owned()),
            _ => return Err(QuarkError::InvalidResponse),
        };
        upload_parts.push(UploadPart {
            url: url.into(),
            content_type,
        });
    }
    Ok(upload_parts)
}

fn secure_temporary_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn validate_upload_urls(value: &Value) -> Result<(), QuarkError> {
    match value {
        Value::Array(values) => values.iter().try_for_each(validate_upload_urls),
        Value::Object(values) => {
            for (key, value) in values {
                // Only the public upload URL is ever transferred by this
                // client.  Aliyun also returns an HTTP-only intranet endpoint
                // (`internal_upload_url`) for its own infrastructure; treating
                // that as a browser transfer target rejects a valid public URL.
                if key == "upload_url" {
                    match value.as_str() {
                        Some(url) if secure_temporary_url(url) => {}
                        _ => return Err(QuarkError::InvalidResponse),
                    }
                }
                validate_upload_urls(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn parse_file(value: &Value, parent: &str) -> Result<CloudFile, QuarkError> {
    Ok(CloudFile {
        id: required(value, "file_id")?,
        parent_id: value
            .get("parent_file_id")
            .and_then(Value::as_str)
            .unwrap_or(parent)
            .into(),
        name: value
            .get("name")
            .or_else(|| value.get("file_name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .ok_or(QuarkError::InvalidResponse)?,
        kind: required(value, "type")?.parse()?,
        size: value
            .get("size")
            .and_then(|size| size.as_u64().or_else(|| size.as_str()?.parse().ok())),
        modified_at: value
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

use super::share::{
    CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudSharePlanStore,
    CloudShareResolvedEntry, CloudTransferCapacity, RecordedTransferDownload,
    RecordedTransferResolver, ShareInspect, ShareInspection, ShareOperationResult, ShareResolution,
    ShareResolve, ShareTransfer, TransferRequest,
};

/// Share-token authorization is deliberately separate from personal-drive sessions.
pub struct AliyunShareAdapter<'a> {
    pub credentials: &'a dyn CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
impl AliyunShareAdapter<'_> {
    fn headers(
        &self,
        token: Option<&str>,
        include_account_bearer: bool,
    ) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([
            ("Referer".into(), SHARE_REFERER.into()),
            ("User-Agent".into(), "Mozilla/5.0".into()),
        ]);
        if let Some(token) = token {
            headers.insert("x-share-token".into(), token.into());
        }
        if include_account_bearer {
            if let Some(bearer) = self.account_bearer() {
                headers.insert("Authorization".into(), bearer);
            }
        }
        headers
    }
    fn account_bearer(&self) -> Option<String> {
        self.personal_session()
            .ok()
            .map(|session| format!("Bearer {}", session.access_token))
    }
    fn personal_session(&self) -> Result<AliyunSession, QuarkError> {
        let bytes = self
            .credentials
            .load("aliyun")?
            .ok_or(QuarkError::AuthorizationExpired)?;
        let session =
            serde_json::from_slice::<AliyunSession>(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.access_token.is_empty() || session.drive_id.is_empty() {
            return Err(QuarkError::AuthorizationExpired);
        }
        Ok(session)
    }
    fn personal_download(
        &self,
        session: &AliyunSession,
        file_id: &str,
        name: &str,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        let client = AliyunClient::new(self.credentials, self.http);
        for attempt in 0..=ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len() {
            let result = (|| {
                let (url, _) = client.download_with_session(session, file_id)?;
                super::share::validate_direct_url(&url)?;
                Ok(super::commands::ResolvedCloudDownload {
                    url,
                    name: name.into(),
                    headers: vec![("Referer".into(), WEB_REFERER.into())],
                })
            })();
            match result {
                Ok(download) => return Ok(download),
                Err(error)
                    if attempt < ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len()
                        && matches!(
                            error,
                            QuarkError::Transport(_)
                                | QuarkError::Provider(_)
                                | QuarkError::InvalidResponse
                        ) =>
                {
                    aliyun_transient_retry_pause(attempt);
                }
                Err(error) => return Err(error),
            }
        }
        Err(QuarkError::InvalidResponse)
    }
    fn personal_download_after_copy(
        &self,
        session: &AliyunSession,
        file_id: &str,
        name: &str,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        // A newly copied file may briefly be invisible to the download URL
        // endpoint. Only retry this case immediately after a successful copy;
        // a missing previously recorded file should be invalidated promptly.
        for attempt in 0..=ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len() {
            match self.personal_download(session, file_id, name) {
                Err(QuarkError::SourceFileMissing)
                    if attempt < ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len() =>
                {
                    aliyun_transient_retry_pause(attempt);
                }
                Err(QuarkError::SourceFileMissing | QuarkError::InvalidResponse) => break,
                result => return result,
            }
        }
        Err(QuarkError::ShareTransferDownloadUnavailable)
    }
    fn raw(
        &self,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.send_raw_with_account_bearer(path, token, body, false, true)
    }
    fn raw_with_account_bearer(
        &self,
        path: &str,
        token: Option<&str>,
        body: Value,
        include_account_bearer: bool,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.send_raw_with_account_bearer(path, token, body, include_account_bearer, false)
    }
    fn read_raw_with_account_bearer(
        &self,
        path: &str,
        token: Option<&str>,
        body: Value,
        include_account_bearer: bool,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.send_raw_with_account_bearer(path, token, body, include_account_bearer, true)
    }
    fn send_raw_with_account_bearer(
        &self,
        path: &str,
        token: Option<&str>,
        body: Value,
        include_account_bearer: bool,
        retry_transport: bool,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.send_with_rate_limit_retry(
            CloudHttpRequest {
                method: "POST",
                url: format!("{API}{path}"),
                headers: self.headers(token, include_account_bearer),
                body: Some(body.to_string()),
            },
            retry_transport,
        )
    }
    fn send_with_rate_limit_retry(
        &self,
        request: CloudHttpRequest,
        retry_transport: bool,
    ) -> Result<CloudHttpResponse, QuarkError> {
        for attempt in 0..=ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len() {
            let response = match self.http.send(request.clone()) {
                Ok(response) => response,
                Err(error)
                    if retry_transport
                        && attempt < ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len()
                        && matches!(error, QuarkError::Transport(_)) =>
                {
                    aliyun_transient_retry_pause(attempt);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !aliyun_share_rate_limited(&response)
                || attempt == ALIYUN_TRANSIENT_RETRY_DELAYS_MS.len()
            {
                return Ok(response);
            }
            aliyun_transient_retry_pause(attempt);
        }
        Err(QuarkError::InvalidResponse)
    }
    fn request(&self, path: &str, token: Option<&str>, body: Value) -> Result<Value, QuarkError> {
        aliyun_share_json(&self.raw(path, token, body)?)
    }
    fn token(&self, id: &str, code: Option<&str>) -> Result<CloudHttpResponse, QuarkError> {
        self.raw(
            "/v2/share_link/get_share_token",
            None,
            json!({"share_id":id,"share_pwd":code.unwrap_or("")}),
        )
    }
}
fn aliyun_share_json(response: &CloudHttpResponse) -> Result<Value, QuarkError> {
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
    if !value.is_object() {
        return Err(QuarkError::InvalidResponse);
    }
    for key in ["error_code", "status"] {
        if value.get(key).is_some_and(|value| value.as_i64().is_none()) {
            return Err(QuarkError::InvalidResponse);
        }
    }
    if value
        .get("error")
        .is_some_and(|value| value.as_str().is_none())
    {
        return Err(QuarkError::InvalidResponse);
    }
    if response.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    if let Some(code) = value.get("code") {
        let code = code.as_str().ok_or(QuarkError::InvalidResponse)?;
        if !code.is_empty() {
            return Err(match code {
                "ShareLink.PasswordMismatch"
                | "ShareLink.PasswordRequired"
                | "InvalidResource.SharePwd" => {
                    QuarkError::InvalidInput("阿里云盘分享访问码错误或缺失".into())
                }
                "ShareLinkTokenInvalid"
                | "ShareLinkTokenExpired"
                | "AccessTokenInvalid"
                | "AccessTokenExpired" => QuarkError::AuthorizationExpired,
                "TooManyRequests" | "Forbidden.RiskControl" | "CaptchaRequired" => {
                    QuarkError::WebAuthorizationRequired
                }
                "BlockException" => QuarkError::Provider("阿里云盘分享请求频繁，请稍后重试".into()),
                _ => QuarkError::Provider("阿里云盘分享不可用，请检查分享有效期及访问权限".into()),
            });
        }
    }
    // The common decoder catches numeric error_code and HTTP errors; redact its text.
    response_json(response).map_err(|error| match error {
        QuarkError::Provider(_) => QuarkError::Provider("阿里云盘分享请求失败".into()),
        error => error,
    })?;
    Ok(value)
}

fn aliyun_share_rate_limited(response: &CloudHttpResponse) -> bool {
    serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|value| value.get("code").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("BlockException")
}

fn aliyun_transient_retry_pause(attempt: usize) {
    #[cfg(not(test))]
    if let Some(delay_ms) = ALIYUN_TRANSIENT_RETRY_DELAYS_MS.get(attempt) {
        std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
    }
    #[cfg(test)]
    let _ = attempt;
}

fn stored_aliyun_share_token(context: &Value) -> Option<String> {
    context
        .get("share_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty() && !token.chars().any(char::is_control))
        .map(str::to_owned)
}

fn aliyun_share_token_needs_refresh(response: &CloudHttpResponse) -> bool {
    response.status == 401
        || aliyun_share_rate_limited(response)
        || serde_json::from_str::<Value>(&response.body)
            .ok()
            .and_then(|value| value.get("code").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|code| {
                matches!(
                    code.as_str(),
                    "ShareLinkTokenInvalid"
                        | "ShareLinkTokenExpired"
                        | "AccessTokenInvalid"
                        | "AccessTokenExpired"
                )
            })
}

fn aliyun_share_requires_personal_transfer(response: &CloudHttpResponse) -> bool {
    response.status == 403
        && serde_json::from_str::<Value>(&response.body)
            .ok()
            .and_then(|value| value.get("code").and_then(Value::as_str).map(str::to_owned))
            .as_deref()
            == Some("Forbidden")
}

fn with_aliyun_share_token(mut context: Value, token: &str) -> Value {
    if let Some(values) = context.as_object_mut() {
        values.insert("share_token".into(), token.into());
        context
    } else {
        json!({"share_token":token})
    }
}

impl ShareInspect for AliyunShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let id = super::share::share_id(candidate, CloudProvider::Aliyun)?;
        let response = self.token(&id, candidate.access_code.as_deref())?;
        let value: Value =
            serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
        if (200..500).contains(&response.status)
            && candidate.access_code.is_none()
            && matches!(
                value["code"].as_str(),
                Some(
                    "ShareLink.PasswordRequired"
                        | "ShareLink.PasswordMismatch"
                        | "InvalidResource.SharePwd"
                )
            )
        {
            return Ok(ShareInspection::AccessCodeRequired);
        }
        let mut token = required(&aliyun_share_json(&response)?, "share_token")?;
        let mut queue = std::collections::VecDeque::from([("root".to_string(), vec![])]);
        let mut seen = std::collections::BTreeSet::from(["root".to_string()]);
        let mut entries = vec![];
        let mut requests = 0usize;
        while let Some((parent, directory)) = queue.pop_front() {
            let mut marker = String::new();
            let mut markers = std::collections::BTreeSet::new();
            loop {
                requests += 1;
                if requests > 10_000 || directory.len() > 128 {
                    return Err(QuarkError::InvalidResponse);
                }
                let body =
                    json!({"share_id":id,"parent_file_id":parent,"limit":200,"marker":marker});
                let response = self.raw("/adrive/v3/file/list", Some(&token), body.clone())?;
                let value = if aliyun_share_rate_limited(&response) {
                    let refreshed = self.token(&id, candidate.access_code.as_deref())?;
                    token = required(&aliyun_share_json(&refreshed)?, "share_token")?;
                    self.request("/adrive/v3/file/list", Some(&token), body)?
                } else {
                    aliyun_share_json(&response)?
                };
                for raw in value["items"]
                    .as_array()
                    .ok_or(QuarkError::InvalidResponse)?
                {
                    let file = parse_file(raw, &parent)?;
                    if !seen.insert(file.id.clone()) {
                        return Err(QuarkError::InvalidResponse);
                    }
                    match file.kind.as_str() {
                        "folder" => {
                            let mut path = directory.clone();
                            path.push(file.name);
                            queue.push_back((file.id, path));
                        }
                        "file" => entries.push(CloudShareEntry::new(
                            &id,
                            file.id,
                            file.name,
                            directory.clone(),
                            file.size,
                        )),
                        _ => return Err(QuarkError::InvalidResponse),
                    }
                }
                marker = match value.get("next_marker") {
                    None => String::new(),
                    Some(value) => value.as_str().ok_or(QuarkError::InvalidResponse)?.into(),
                };
                if marker.is_empty() {
                    break;
                }
                if !markers.insert(marker.clone()) {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        Ok(ShareInspection::Plan(CloudSharePlan::new(
            CloudProvider::Aliyun,
            id,
            candidate.access_code.clone(),
            entries,
            json!({"share_token":token}),
        )))
    }
}
impl ShareResolve for AliyunShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_aliyun_share(self.credentials, self.http, entry)
    }
    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        resolve_aliyun_share_with_context(self.credentials, self.http, entry)
    }
}
impl ShareTransfer for AliyunShareAdapter<'_> {
    fn transfer_capacity(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        AliyunClient::new(self.credentials, self.http).storage_capacity()
    }

    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        let session = self.personal_session()?;
        // The default drive belongs to exactly one currently-authorized account
        // and is durable across access-token rotation, so it safely gates reuse
        // and cleanup without retaining account identifiers in the journal.
        super::share::stable_owner_fingerprint(CloudProvider::Aliyun, &session.drive_id).map(Some)
    }

    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        self.transfer_with_context(entry, store)?.result
    }

    fn transfer_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<ShareOperationResult<super::commands::ResolvedCloudDownload>, QuarkError> {
        if entry.provider != CloudProvider::Aliyun || entry.plan.provider != CloudProvider::Aliyun {
            return Err(QuarkError::InvalidResponse);
        }
        let session = self.personal_session()?;
        let mut context = entry.plan.provider_context.clone();
        let mut rotated_context = false;
        let mut token = match stored_aliyun_share_token(&context) {
            Some(token) => token,
            None => {
                let response = self.token(&entry.plan.canonical_share_id, entry.access_code())?;
                let token = required(&aliyun_share_json(&response)?, "share_token")?;
                context = with_aliyun_share_token(context, &token);
                rotated_context = true;
                token
            }
        };
        let copy = json!({
            "share_id":entry.plan.canonical_share_id,
            "file_id":entry.entry.entry_id,
            "to_drive_id":session.drive_id,
            "to_parent_file_id":"root",
            "auto_rename":true,
        });
        let mut response =
            self.raw_with_account_bearer("/v2/file/copy", Some(&token), copy.clone(), true)?;
        if aliyun_share_token_needs_refresh(&response) {
            let refreshed = self.token(&entry.plan.canonical_share_id, entry.access_code())?;
            token = required(&aliyun_share_json(&refreshed)?, "share_token")?;
            context = with_aliyun_share_token(context, &token);
            rotated_context = true;
            response = self.raw_with_account_bearer("/v2/file/copy", Some(&token), copy, true)?;
        }
        let result = (|| {
            let copied = aliyun_share_json(&response)?;
            if copied
                .get("drive_id")
                .and_then(Value::as_str)
                .is_some_and(|drive_id| drive_id != session.drive_id)
            {
                return Err(QuarkError::InvalidResponse);
            }
            let file_id = required(&copied, "file_id")?;
            let owner =
                super::share::stable_owner_fingerprint(CloudProvider::Aliyun, &session.drive_id)?;
            store.record_transfer_with_owner(
                &entry.plan.source_for(&entry.entry.entry_id),
                &file_id,
                &owner,
            )?;
            self.personal_download_after_copy(&session, &file_id, &entry.entry.name)
        })();
        Ok(ShareOperationResult {
            result,
            updated_context: rotated_context.then_some(context),
        })
    }
}

impl RecordedTransferResolver for AliyunShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        if entry.provider != CloudProvider::Aliyun
            || entry.plan.provider != CloudProvider::Aliyun
            || created_file_id.is_empty()
        {
            return Err(QuarkError::InvalidResponse);
        }
        let session = self.personal_session()?;
        Ok(ShareOperationResult {
            result: self
                .personal_download(&session, created_file_id, &entry.entry.name)
                .map(RecordedTransferDownload::Available),
            updated_context: None,
        })
    }
}

impl CloudShareAdapter for AliyunShareAdapter<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}
pub fn resolve_aliyun_share(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    resolve_aliyun_share_with_context(credentials, http, entry)?.result
}

fn resolve_aliyun_share_with_context(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
    if entry.provider != CloudProvider::Aliyun || entry.plan.provider != CloudProvider::Aliyun {
        return Err(QuarkError::InvalidResponse);
    }
    let adapter = AliyunShareAdapter { credentials, http };
    let mut context = entry.plan.provider_context.clone();
    let mut rotated_context = false;
    let mut token = match stored_aliyun_share_token(&context) {
        Some(token) => token,
        None => {
            let response = adapter.token(&entry.plan.canonical_share_id, entry.access_code())?;
            let token = required(&aliyun_share_json(&response)?, "share_token")?;
            context = with_aliyun_share_token(context, &token);
            rotated_context = true;
            token
        }
    };
    // A public share resolves through GetDownloadUrl with an account bearer
    // and short-lived x-share-token; drive_id stays absent.
    let body = json!({"share_id":entry.plan.canonical_share_id,"file_id":entry.entry.entry_id,"expire_sec":900});
    let mut response = adapter.read_raw_with_account_bearer(
        "/v2/file/get_download_url",
        Some(&token),
        body.clone(),
        true,
    )?;
    if aliyun_share_token_needs_refresh(&response) {
        let refreshed = adapter.token(&entry.plan.canonical_share_id, entry.access_code())?;
        token = required(&aliyun_share_json(&refreshed)?, "share_token")?;
        context = with_aliyun_share_token(context, &token);
        rotated_context = true;
        response = adapter.read_raw_with_account_bearer(
            "/v2/file/get_download_url",
            Some(&token),
            body,
            true,
        )?;
    }
    if aliyun_share_requires_personal_transfer(&response) {
        return Ok(ShareOperationResult {
            result: Ok(ShareResolution::TransferRequired(TransferRequest {
                source: entry.plan.source_for(&entry.entry.entry_id),
            })),
            updated_context: rotated_context.then_some(context),
        });
    }
    let result = (|| {
        let value = aliyun_share_json(&response)?;
        let url = required(&value, "url").or_else(|_| required(&value, "download_url"))?;
        super::share::validate_direct_url(&url)?;
        Ok(ShareResolution::Direct(
            super::commands::ResolvedCloudDownload {
                url,
                name: entry.entry.name.clone(),
                // The account bearer authorizes only the API request that
                // created this temporary URL. Never forward it to the
                // untrusted CDN host consumed by aria2.
                headers: adapter.headers(Some(&token), false).into_iter().collect(),
            },
        ))
    })();
    Ok(ShareOperationResult {
        result,
        updated_context: rotated_context.then_some(context),
    })
}
