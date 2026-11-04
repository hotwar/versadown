use super::{
    types::{encoded, required},
    CloudAccount, CloudAuthMethod, CloudDirectory, CloudFile, CloudFileKind, CloudHttpRequest,
    CloudHttpResponse, CloudHttpTransport, CloudProvider, CloudShare, CloudShareOptions,
    CloudStorageCapacity, CloudUploadTransport, QuarkError, UploadHttpRequest, UploadHttpResponse,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use rsa::{pkcs8::DecodePublicKey, Pkcs1v15Encrypt, RsaPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};
use zeroize::{Zeroize, Zeroizing};

pub const BAIDU_SHARE_PASS_CODE_CONSTRAINT: super::CloudSharePassCodeConstraint =
    super::CloudSharePassCodeConstraint {
        required: true,
        min_length: 4,
        max_length: 4,
        character_set: super::CloudSharePassCodeCharacterSet::AsciiAlphanumeric,
        hint: "提取码必须为 4 位字母或数字",
    };

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{share::*, CredentialStore, MemoryCredentialStore};
    use std::sync::Mutex;
    struct Http {
        replies: Mutex<std::collections::VecDeque<Value>>,
        requests: Mutex<Vec<CloudHttpRequest>>,
    }
    impl Http {
        fn new(replies: Vec<Value>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                requests: Mutex::new(vec![]),
            }
        }
    }
    impl CloudHttpTransport for Http {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.requests.lock().unwrap().push(request);
            Ok(CloudHttpResponse {
                status: 200,
                body: self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request")
                    .to_string(),
                set_cookies: vec![],
            })
        }
    }
    fn candidate() -> crate::parser::CloudShareCandidate {
        crate::parser::CloudShareCandidate {
            provider: CloudProvider::Baidu,
            url: "https://pan.baidu.com/s/1share123".into(),
            // Existing listing fixtures represent public/already accessible shares.
            access_code: None,
        }
    }

    #[test]
    fn personal_share_record_keeps_baidu_link_and_expiry_fields() {
        let share = baidu_share_from_value(
            &json!({
                "shareid": 42,
                "shorturl": "share-code",
                "filename": "归档",
                "isdir": 1,
                "fs_id": 9,
                "pwd": "a7b2",
                "ctime": 1_789_000_000,
                "expire_time": 1_790_000_000,
            }),
            None,
            None,
        )
        .unwrap();

        assert_eq!(share.id, "42");
        assert_eq!(share.url, "https://pan.baidu.com/s/1share-code");
        assert_eq!(share.name, "归档");
        assert_eq!(share.kind, CloudFileKind::Folder);
        assert_eq!(share.source_file_ids, vec!["9"]);
        assert_eq!(share.pass_code.as_deref(), Some("a7b2"));
    }

    #[test]
    fn personal_share_record_uses_shorturl_when_link_is_empty() {
        let http = Http::new(vec![json!({
            "errno": 0,
            "list": [{
                "shareid": 42,
                "link": "",
                "shorturl": "share-code",
                "filename": "归档",
                "isdir": 1,
                "fs_id": 9,
            }],
        })]);

        let shares = BaiduClient::new(BaiduSession::cookie("BDUSS=current"), &http)
            .list_shares()
            .unwrap();

        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].url, "https://pan.baidu.com/s/1share-code");
    }

    #[test]
    fn personal_share_list_accepts_camel_case_record_fields() {
        let http = Http::new(vec![json!({
            "errno": 0,
            "list": [{
                "shareId": 42,
                "shortlink": "1share-code",
                "typicalPath": "/归档",
                "fsIds": [9],
                "passwd": "a7b2",
            }],
        })]);

        let shares = BaiduClient::new(BaiduSession::cookie("BDUSS=current"), &http)
            .list_shares()
            .unwrap();

        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].id, "42");
        assert_eq!(shares[0].url, "https://pan.baidu.com/s/1share-code");
        assert_eq!(shares[0].name, "归档");
        assert_eq!(shares[0].source_file_ids, vec!["9"]);
        assert_eq!(shares[0].pass_code.as_deref(), Some("a7b2"));
    }

    #[test]
    fn personal_share_list_uses_typical_path_when_filename_is_placeholder() {
        let http = Http::new(vec![json!({
            "errno": 0,
            "list": [{
                "shareId": 42,
                "shortlink": "1share-code",
                "filename": "0",
                "typicalPath": "/资料/下载说明.pdf",
                "fsIds": [9],
            }],
        })]);

        let shares = BaiduClient::new(BaiduSession::cookie("BDUSS=current"), &http)
            .list_shares()
            .unwrap();

        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].name, "下载说明.pdf");
    }

    fn baidu_capacity_entry() -> CloudShareResolvedEntry {
        let plan = CloudSharePlan::new(
            CloudProvider::Baidu,
            "capacity-share".into(),
            None,
            vec![CloudShareEntry::new(
                "capacity-share",
                "1".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            json!({}),
        );
        CloudShareResolvedEntry {
            provider: CloudProvider::Baidu,
            entry: plan.entries[0].clone(),
            plan,
        }
    }

    #[test]
    fn baidu_share_transfer_capacity_uses_quota_bytes() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "baidu",
                &serde_json::to_vec(&BaiduSession::cookie("BDUSS=current")).unwrap(),
            )
            .unwrap();
        let http = Http::new(vec![json!({"errno": 0, "total": "2048", "used": 512})]);
        let adapter = BaiduShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        assert_eq!(
            adapter.transfer_capacity(&baidu_capacity_entry()).unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 2048,
                used_bytes: 512
            }
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.contains("/api/quota?"));
        assert!(!requests[0].url.contains("/share/transfer"));
    }

    struct FixedStatusHttp(u16);
    impl CloudHttpTransport for FixedStatusHttp {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            Ok(CloudHttpResponse {
                status: self.0,
                body: "{}".into(),
                ..Default::default()
            })
        }
    }

    #[test]
    fn baidu_share_transfer_capacity_preserves_expired_authorization() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "baidu",
                &serde_json::to_vec(&BaiduSession::cookie("BDUSS=current")).unwrap(),
            )
            .unwrap();
        assert!(matches!(
            BaiduShareAdapter {
                credentials: &credentials,
                http: &FixedStatusHttp(401)
            }
            .transfer_capacity(&baidu_capacity_entry()),
            Err(QuarkError::AuthorizationExpired)
        ));
    }

    #[test]
    fn locatedownload_signature_matches_the_known_baidupcs_go_fixture() {
        assert_eq!(
            baidu_locatedownload_signature(
                10086,
                "test_bduss",
                1_571_140_066,
                "O|1E67351CCE80B2CF48DB511CD77ACD9F",
            ),
            "b6bb7a6f46899e181baea58798d4fdb889775c2c"
        );
    }

    #[test]
    #[ignore = "read-only: saved Baidu session and 1 KiB native download probe"]
    fn live_baidu_native_download_probe() {
        assert_eq!(
            std::env::var("VERSADOWN_LIVE_CLOUD").as_deref(),
            Ok("1")
        );
        let store = crate::cloud::SystemCredentialStore::new(
            std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
                .join("com.versadown.desktop/cloud-sessions"),
        )
        .unwrap();
        let session: BaiduSession =
            serde_json::from_slice(&store.load("baidu").unwrap().unwrap()).unwrap();
        let http = crate::cloud::ReqwestTransport::new().unwrap();
        let client = BaiduClient::new(session, &http);
        let root = client.directory("/").unwrap();
        let file = root
            .items
            .iter()
            .find(|f| f.kind == "file" && f.size.unwrap_or(0) > 0)
            .unwrap();
        let download = client
            .locatedownload(&parse_file_reference(&file.id).unwrap())
            .unwrap();
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let mut request = http.get(&download.url).header("Range", "bytes=0-1023");
        for (key, value) in download.headers {
            request = request.header(key, value);
        }
        let response = request
            .send()
            .map_err(|_| "native transfer network failure")
            .unwrap();
        let status = response.status();
        assert!(status.is_success(), "native transfer HTTP {status}");
        let mut bytes = Vec::new();
        response.take(1024).read_to_end(&mut bytes).unwrap();
        assert!(!bytes.is_empty());
        eprintln!(
            "Native Baidu download: HTTP {status}, {} bytes",
            bytes.len()
        );
    }
    #[test]
    #[ignore = "requires explicit share URL, code, saved Baidu session; transfers one file, probes 1 KiB, then trashes the exact created copy"]
    fn live_baidu_share_transfer_download_probe() {
        assert_eq!(
            std::env::var("VERSADOWN_LIVE_BAIDU_SHARE_TRANSFER").as_deref(),
            Ok("1")
        );
        let input = std::env::var("BAIDU_SHARE_PROBE_URL").expect("share URL required");
        let code = std::env::var("BAIDU_SHARE_PROBE_CODE").expect("access code required");
        let candidate = crate::parser::extract_cloud_share_candidate(&input, Some(&code))
            .expect("valid share URL required")
            .expect("Baidu share URL required");
        assert_eq!(candidate.provider, CloudProvider::Baidu);
        let credentials = crate::cloud::SystemCredentialStore::new(
            std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
                .join("com.versadown.desktop/cloud-sessions"),
        )
        .unwrap();
        let session: BaiduSession =
            serde_json::from_slice(&credentials.load("baidu").unwrap().unwrap()).unwrap();
        let http = crate::cloud::ReqwestTransport::new().unwrap();
        let adapter = BaiduShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate).unwrap() else {
            panic!("share did not yield a file plan")
        };
        let entry = plan.entries.first().expect("share has no files");
        let source = plan.source_for(&entry.entry_id);
        let dir = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(dir.path().into()).unwrap(),
        );
        store.upsert(&plan).unwrap();
        let selection =
            selected_share_download(&store, &source, &[entry.entry_fingerprint.clone()]).unwrap();
        let result = (|| -> Result<(), QuarkError> {
            preflight_selected_share_download(&store, &adapter, &selection)?;
            let download = resolve_current_share_download(&store, &adapter, &source)?;
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|_| QuarkError::Transport("下载探针初始化失败".into()))?;
            let mut request = client.get(&download.url).header("Range", "bytes=0-1023");
            for (key, value) in download.headers {
                request = request.header(key, value);
            }
            let response = request
                .send()
                .map_err(|_| QuarkError::Transport("下载探针请求失败".into()))?;
            let status = response.status();
            if !status.is_success() {
                return Err(QuarkError::Provider(format!("下载探针 HTTP {status}")));
            }
            let mut bytes = Vec::new();
            response
                .take(1024)
                .read_to_end(&mut bytes)
                .map_err(|_| QuarkError::Transport("下载探针读取失败".into()))?;
            if bytes.is_empty() {
                return Err(QuarkError::InvalidResponse);
            }
            eprintln!(
                "Baidu share transfer probe: HTTP {status}, {} bytes",
                bytes.len()
            );
            Ok(())
        })();
        let created = store
            .resolve_entry(&source)
            .unwrap()
            .plan
            .transfers
            .iter()
            .map(|record| record.created_file_id.clone())
            .collect::<Vec<_>>();
        if !created.is_empty() {
            let client = BaiduClient::new(session, &http);
            client
                .trash_transferred_files(&created)
                .unwrap_or_else(|error| {
                    panic!("test copy cleanup failed for fs_id {created:?}: {error}")
                });
        }
        result.unwrap();
    }
    fn listing() -> Value {
        json!({"errno":0,"data":{"shareid":22,"uk":33,"seckey":"secret-key","has_more":false,"list":[{"fs_id":11,"server_filename":"movie.mp4","path":"/movie.mp4","isdir":0,"size":123}]}})
    }
    #[test]
    fn share_cleanup_resolves_only_exact_recorded_ids_before_deleting() {
        let http = Http::new(vec![
            json!({"errno":0,"list":[{"fs_id":99,"path":"/created.txt","isdir":0},{"fs_id":100,"path":"/unrelated.txt","isdir":0}]}),
            json!({"errno":0,"result":{"bdstoken":"token"}}),
            json!({"errno":0}),
        ]);
        BaiduClient::new(BaiduSession::cookie("BDUSS=current"), &http)
            .trash_transferred_files(&["99".into()])
            .unwrap();
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let url = url::Url::parse(&requests[0].url).unwrap();
        assert_eq!(url.path(), "/api/list");
        assert!(requests[2].url.contains("opera=delete"));
        assert!(
            url::form_urlencoded::parse(requests[2].body.as_ref().unwrap().as_bytes())
                .any(|(k, v)| k == "filelist" && v == "[\"/created.txt\"]")
        );
    }

    #[test]
    fn share_cleanup_rejects_missing_extra_directory_and_invalid_path_metadata() {
        for files in [
            json!([]),
            json!([{"fs_id":100,"path":"/other","isdir":0}]),
            json!([{"fs_id":99,"path":"/folder","isdir":1}]),
            json!([{"fs_id":99,"path":"/","isdir":0}]),
            json!([{"fs_id":99,"path":"/../other","isdir":0}]),
        ] {
            let http = Http::new(vec![json!({"errno":0,"list":files})]);
            assert!(
                BaiduClient::new(BaiduSession::cookie("BDUSS=current"), &http)
                    .trash_transferred_files(&["99".into()])
                    .is_err()
            );
            assert_eq!(http.requests.lock().unwrap().len(), 1);
        }
    }
    #[test]
    fn baidu_transfer_fallback_requires_current_login() {
        let credentials = MemoryCredentialStore::default();
        let http = Http::new(vec![listing()]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        assert!(matches!(
            adapter.inspect(&candidate()),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
        credentials
            .save(
                "baidu",
                &serde_json::to_vec(&BaiduSession::cookie("BDUSS=current")).unwrap(),
            )
            .unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("plan required")
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };
        assert!(matches!(
            adapter.resolve(&entry).unwrap(),
            ShareResolution::TransferRequired(_)
        ));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].headers["User-Agent"], BAIDU_PAN_USER_AGENT);
        assert!(requests[0].headers["Cookie"].contains("BDUSS=current"));
    }
    fn credentials() -> MemoryCredentialStore {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "baidu",
                &serde_json::to_vec(&BaiduSession::cookie("BDUSS_BFESS=current")).unwrap(),
            )
            .unwrap();
        credentials
    }
    struct ShareHttp {
        replies: Mutex<std::collections::VecDeque<CloudHttpResponse>>,
        requests: Mutex<Vec<CloudHttpRequest>>,
    }
    impl CloudHttpTransport for ShareHttp {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(QuarkError::InvalidResponse)
        }
    }
    fn share_response(body: &str, cookies: &[&str]) -> CloudHttpResponse {
        CloudHttpResponse {
            status: 200,
            body: body.into(),
            set_cookies: cookies.iter().map(|cookie| (*cookie).into()).collect(),
        }
    }
    #[test]
    fn protected_share_verifies_code_and_keeps_session_for_children_and_download() {
        for cookie in [vec![], vec!["BDCLND=verified%2Bkey%2F%3D; Path=/; Secure"]] {
            let credentials = credentials();
            credentials
                .save(
                    "baidu",
                    &serde_json::to_vec(&BaiduSession::cookie("BDUSS=current; BDCLND=old-share"))
                        .unwrap(),
                )
                .unwrap();
            let saved = credentials.load("baidu").unwrap();
            let http = ShareHttp {
                replies: Mutex::new(vec![
                    share_response("<html>share</html>", &["csrfToken=page-token; Path=/"]),
                    share_response(r#"{"errno":0,"randsk":"verified%2Bkey%2F%3D"}"#, &cookie),
                    share_response(r#"{"errno":0,"shareid":22,"uk":33,"list":[{"fs_id":12,"server_filename":"folder","path":"/folder","isdir":1}]}"#, &[]),
                    share_response(r#"{"errno":0,"list":[{"fs_id":11,"server_filename":"file.txt","path":"/folder/file.txt","isdir":0,"size":123}]}"#, &[]),
                ].into()),
                requests: Mutex::new(vec![]),
            };
            let adapter =
                provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
            let input = crate::parser::CloudShareCandidate {
                provider: CloudProvider::Baidu,
                url: "https://pan.baidu.com/s/1_49elQi_54BPJASvAxAB6w".into(),
                access_code: Some("88in".into()),
            };
            let result = adapter.inspect(&input);
            assert!(
                matches!(result, Ok(ShareInspection::Plan(_))),
                "correct code must unlock the file list: {:?}",
                result.as_ref().err()
            );
            let ShareInspection::Plan(plan) = result.unwrap() else {
                unreachable!()
            };
            assert_eq!(plan.entries[0].relative_directory, vec!["folder"]);
            assert_eq!(plan.provider_context["bdclnd"], "verified%2Bkey%2F%3D");
            assert_eq!(plan.provider_context["seckey"], "verified+key/=");
            let entry = CloudShareResolvedEntry {
                provider: plan.provider,
                entry: plan.entries[0].clone(),
                plan,
            };
            assert!(matches!(
                adapter.resolve(&entry).unwrap(),
                ShareResolution::TransferRequired(_)
            ));
            assert_eq!(
                credentials.load("baidu").unwrap(),
                saved,
                "share cookies must not replace account credentials"
            );
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 4);
            assert_eq!(requests[0].url, input.url);
            assert!(
                !requests[0].headers.contains_key("Cookie"),
                "account cookies on the share page trigger an endless passport SSO redirect"
            );
            assert!(requests[1].headers["Cookie"].contains("BDUSS=current"));
            assert_eq!(requests[1].method, "POST");
            let verify_url = url::Url::parse(&requests[1].url).unwrap();
            assert_eq!(verify_url.path(), "/share/verify");
            assert!(verify_url
                .query_pairs()
                .any(|(k, v)| k == "surl" && v == "_49elQi_54BPJASvAxAB6w"));
            let body: BTreeMap<_, _> =
                url::form_urlencoded::parse(requests[1].body.as_ref().unwrap().as_bytes())
                    .into_owned()
                    .collect();
            assert_eq!(body["pwd"], "88in");
            assert!(requests[1].headers["Cookie"].contains("csrfToken=page-token"));
            for request in &requests[2..] {
                assert!(request.headers["Cookie"].contains("BDCLND=verified%2Bkey%2F%3D"));
                assert_eq!(request.headers["Cookie"].matches("BDCLND=").count(), 1);
                assert!(
                    !request.url.contains("pwd="),
                    "password belongs only in verification POST body"
                );
            }
        }
    }
    #[test]
    fn protected_share_verification_errors_never_list_or_transfer() {
        for (response, expected) in [
            (json!({"errno":-9}), "code"),
            (json!({"errno":-12}), "code"),
            (json!({"errno":-6}), "auth"),
            (json!({"errno":-20}), "web"),
            (json!({"errno":-9,"vcode_str":"challenge"}), "web"),
            (json!({"errno":9019}), "captcha"),
            (json!({"errno":0}), "invalid"),
        ] {
            let credentials = credentials();
            let http = ShareHttp {
                replies: Mutex::new(
                    vec![
                        share_response("<html/>", &[]),
                        share_response(&response.to_string(), &[]),
                    ]
                    .into(),
                ),
                requests: Mutex::new(vec![]),
            };
            let adapter =
                provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
            let mut input = candidate();
            input.access_code = Some("a123".into());
            let result = adapter.inspect(&input);
            assert!(
                match expected {
                    "code" => matches!(result, Err(QuarkError::InvalidInput(_))),
                    "auth" => matches!(result, Err(QuarkError::AuthorizationExpired)),
                    "web" => matches!(result, Err(QuarkError::WebAuthorizationRequired)),
                    "captcha" => matches!(result, Err(QuarkError::CaptchaRequired)),
                    _ => matches!(result, Err(QuarkError::InvalidResponse)),
                },
                "unexpected verification outcome: {:?}",
                result.as_ref().err()
            );
            let requests = http.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[1].url.contains("/share/verify?"));
        }
    }
    #[test]
    fn protected_share_list_rejection_is_not_reported_as_wrong_code() {
        let credentials = credentials();
        let http = ShareHttp {
            replies: Mutex::new(
                vec![
                    share_response("<html/>", &[]),
                    share_response(r#"{"errno":0,"randsk":"verified"}"#, &[]),
                    share_response(r#"{"errno":-9}"#, &[]),
                ]
                .into(),
            ),
            requests: Mutex::new(vec![]),
        };
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let mut input = candidate();
        input.access_code = Some("a123".into());
        assert!(matches!(
            adapter.inspect(&input),
            Err(QuarkError::Provider(_))
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 3);
    }

    #[test]
    #[ignore = "read-only: requires BAIDU_SHARE_PROBE_URL and BAIDU_SHARE_PROBE_CODE; no account credentials or downloads"]
    fn live_protected_share_verification() {
        let input = std::env::var("BAIDU_SHARE_PROBE_URL").expect("explicit share URL required");
        let code = std::env::var("BAIDU_SHARE_PROBE_CODE").expect("explicit access code required");
        let candidate = crate::parser::extract_cloud_share_candidate(&input, Some(&code))
            .unwrap()
            .unwrap();
        let id = super::super::share::share_id(&candidate, CloudProvider::Baidu).unwrap();
        let short = id.strip_prefix('1').unwrap();
        let http = crate::cloud::ReqwestTransport::new().unwrap();
        let mut client = BaiduClient::new(BaiduSession::cookie(""), &http);
        let (bdclnd, _) = client.verify_share_access(short, &code).unwrap();
        let response = client.share_http("GET", &format!("/share/list?channel=chunlei&clienttype=0&app_id=250528&web=1&shorturl={}&root=1&dir=%2F&num=1&page=1", encoded(short)), None, &bdclnd, None).unwrap();
        let value = share_json(&response).unwrap();
        assert!(
            value["list"].is_array(),
            "verification must unlock the root listing"
        );
    }
    #[test]
    fn baidu_share_uses_the_current_share_list_endpoint() {
        let credentials = credentials();
        let http = Http::new(vec![listing()]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();

        let result = adapter.inspect(&candidate());

        assert!(matches!(result, Ok(ShareInspection::Plan(_))));
        let request = &http.requests.lock().unwrap()[0];
        assert_eq!(request.method, "GET");
        assert!(request.body.is_none());
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.path(), "/share/list");
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("channel").map(String::as_str), Some("chunlei"));
        assert_eq!(query.get("clienttype").map(String::as_str), Some("0"));
        assert_eq!(query.get("app_id").map(String::as_str), Some("250528"));
        assert_eq!(query.get("web").map(String::as_str), Some("1"));
        assert_eq!(query.get("shorturl").map(String::as_str), Some("share123"));
        assert_eq!(query.get("root").map(String::as_str), Some("1"));
        assert_eq!(query.get("dir").map(String::as_str), Some("/"));
        assert_eq!(query.get("num").map(String::as_str), Some("100"));
        assert_eq!(query.get("page").map(String::as_str), Some("1"));
        assert_eq!(query.get("order").map(String::as_str), Some("time"));
    }
    #[test]
    fn baidu_share_current_list_accepts_top_level_payload() {
        let credentials = credentials();
        let response = json!({
            "errno": 0,
            "share_id": 22,
            "uk": 33,
            "list": [{
                "fs_id": 11,
                "server_filename": "movie.mp4",
                "path": "/movie.mp4",
                "isdir": 0,
                "size": 123
            }]
        });
        let http = Http::new(vec![response]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();

        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.provider_context["shareid"], "22");
        assert_eq!(plan.provider_context["uk"], "33");
        assert_eq!(plan.provider_context["seckey"], "");
    }
    #[test]
    fn baidu_share_resolve_requires_personal_transfer_without_requesting_web_dlink() {
        let credentials = credentials();
        let http = Http::new(vec![listing()]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("share plan required")
        };
        let source = plan.source_for(&plan.entries[0].entry_id);
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };

        assert!(matches!(
            adapter.resolve(&entry).unwrap(),
            ShareResolution::TransferRequired(request) if request.source == source
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn baidu_share_inspection_accepts_decimal_string_file_size() {
        let credentials = credentials();
        let mut response = listing();
        response["data"]["list"][0]["size"] = json!("123");
        let http = Http::new(vec![response]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();

        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("share plan required")
        };
        assert_eq!(plan.entries[0].size, Some(123));
    }
    #[test]
    fn baidu_share_preflight_rejects_logged_out_account_without_creating_transfer() {
        let credentials = credentials();
        let http = Http::new(vec![listing()]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("share plan required")
        };
        let dir = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(dir.path().into()).unwrap(),
        );
        store.upsert(&plan).unwrap();
        let source = plan.source_for("11");
        let selection = selected_share_download(
            &store,
            &source,
            &[plan.entries[0].entry_fingerprint.clone()],
        )
        .unwrap();
        credentials.remove("baidu").unwrap();

        assert!(matches!(
            preflight_selected_share_download(&store, adapter.as_ref(), &selection),
            Err(QuarkError::NotAuthorized)
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 1);
        assert!(store
            .resolve_entry(&source)
            .unwrap()
            .plan
            .transfers
            .is_empty());
    }
    #[test]
    fn baidu_share_preflight_checks_capacity_before_creating_transfer() {
        let credentials = credentials();
        let http = Http::new(vec![
            listing(),
            json!({"user":{"uk":"account-7"}}),
            json!({"errno":0,"total":100,"used":10}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("share plan required")
        };
        let dir = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(dir.path().into()).unwrap(),
        );
        store.upsert(&plan).unwrap();
        let source = plan.source_for("11");
        let selection = selected_share_download(
            &store,
            &source,
            &[plan.entries[0].entry_fingerprint.clone()],
        )
        .unwrap();

        let error =
            preflight_selected_share_download(&store, adapter.as_ref(), &selection).unwrap_err();
        assert!(error.to_string().contains("百度网盘转存空间不足"));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].url.contains("/api/quota?"));
        assert!(store
            .resolve_entry(&source)
            .unwrap()
            .plan
            .transfers
            .is_empty());
    }
    #[test]
    fn baidu_share_access_code_and_expiry_never_transfer() {
        let credentials = credentials();
        let http = Http::new(vec![
            json!({"errno":-9}),
            json!({"errno":-6}),
            json!({"errno":-20}),
            json!({"errno":9019}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let mut missing = candidate();
        missing.access_code = None;
        assert!(matches!(
            adapter.inspect(&missing),
            Ok(ShareInspection::AccessCodeRequired)
        ));
        assert!(matches!(
            adapter.inspect(&candidate()),
            Err(QuarkError::AuthorizationExpired)
        ));
        assert!(matches!(
            adapter.inspect(&candidate()),
            Err(QuarkError::WebAuthorizationRequired)
        ));
        assert!(matches!(
            adapter.inspect(&candidate()),
            Err(QuarkError::CaptchaRequired)
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 4);
    }
    #[test]
    fn baidu_share_pages_nested_paths_and_empty_folder_stays_empty() {
        let credentials = credentials();
        let mut first = listing();
        first["data"]["has_more"] = json!(true);
        first["data"]["list"] =
            json!([{"fs_id":12,"server_filename":"season","path":"/season","isdir":1}]);
        let mut nested = listing();
        nested["data"]["list"][0]["fs_id"] = json!(13);
        nested["data"]["list"][0]["path"] = json!("/season/verify.mp4");
        nested["data"]["list"][0]["server_filename"] = json!("verify.mp4");
        let http = Http::new(vec![first, listing(), nested]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[1].relative_directory, vec!["season"]);
        let requests = http.requests.lock().unwrap();
        let second_page = url::Url::parse(&requests[1].url).unwrap();
        assert!(second_page
            .query_pairs()
            .any(|(name, value)| name == "page" && value == "2"));
        let mut empty = listing();
        empty["data"]["list"] = json!([]);
        let http = Http::new(vec![empty]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        assert!(plan.entries.is_empty());
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn baidu_share_expands_a_folder_with_string_isdir_instead_of_listing_it_as_a_file() {
        let credentials = credentials();
        let root = json!({
            "errno": 0, "shareid": 22, "uk": 33, "has_more": false,
            "list": [{"fs_id": 12, "server_filename": "BiliDownloader", "path": "/BiliDownloader", "isdir": "1", "size": 0}]
        });
        let children = json!({
            "errno": 0, "has_more": false,
            "list": [
                {"fs_id": 13, "server_filename": "video.mp4", "path": "/BiliDownloader/video.mp4", "isdir": 0, "size": 1024},
                {"fs_id": 14, "server_filename": "cover.jpg", "path": "/BiliDownloader/cover.jpg", "isdir": 0, "size": 256}
            ]
        });
        let http = Http::new(vec![root, children]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();

        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("expected share plan")
        };
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].name, "video.mp4");
        assert_eq!(plan.entries[0].relative_directory, vec!["BiliDownloader"]);
        assert_eq!(plan.entries[1].name, "cover.jpg");
        assert_eq!(plan.entries[1].relative_directory, vec!["BiliDownloader"]);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let nested = url::Url::parse(&requests[1].url).unwrap();
        assert!(nested
            .query_pairs()
            .any(|(name, value)| name == "dir" && value == "/BiliDownloader"));
    }
    #[test]
    fn baidu_share_transfer_records_exact_id_and_downloads_personal_file() {
        let credentials = credentials();
        let http = Http::new(vec![
            listing(),
            json!({"user":{"uk":"account-7"}}),
            json!({"errno":0,"result":{"bdstoken":"write-token"}}),
            json!({"errno":0,"extra":{"list":[{"from_fs_id":11,"to_fs_id":99,"from":"/movie.mp4","to":"/movie (1).mp4"}]}}),
            json!({"errno":0,"uk":7}),
            json!({"errno":0,"list":[{"fs_id":99,"path":"/movie (1).mp4","isdir":0}]}),
            json!({"urls":[{"url":"https://d.pcs.baidu.com/file?sign=personal","encrypt":0}]}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let dir = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(dir.path().into()).unwrap(),
        );
        store.upsert(&plan).unwrap();
        let source = plan.source_for("11");
        let download = resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();
        assert_eq!(download.name, "movie.mp4");
        assert_eq!(
            store.resolve_entry(&source).unwrap().plan.transfers[0].created_file_id,
            "99"
        );
        let requests = http.requests.lock().unwrap();
        assert!(requests[3].url.contains("ondup=newcopy"));
        assert_eq!(
            requests[3].headers["Referer"],
            "https://pan.baidu.com/s/1share123"
        );
        assert!(requests[6].url.contains("method=locatedownload"));
        assert!(requests[6].url.contains("path=%2Fmovie+%281%29.mp4"));
    }
    #[test]
    fn baidu_recorded_transfer_refreshes_personal_file_without_share_transfer() {
        let credentials = credentials();
        let http = Http::new(vec![
            listing(),
            json!({"errno":0,"list":[{"fs_id":99,"path":"/movie (1).mp4","isdir":0}]}),
            json!({"errno":0,"uk":7}),
            json!({"errno":0,"list":[{"fs_id":99,"path":"/movie (1).mp4","isdir":0}]}),
            json!({"urls":[{"url":"https://d.pcs.baidu.com/file?sign=personal","encrypt":0}]}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };

        let refreshed = adapter.refresh_recorded_transfer(&entry, "99").unwrap();

        let Ok(RecordedTransferDownload::Available(download)) = refreshed.result else {
            panic!("a personal direct link is required")
        };
        assert_eq!(download.url, "https://d.pcs.baidu.com/file?sign=personal");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[1].url.contains("/api/list"));
        assert!(requests[4].url.contains("method=locatedownload"));
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("/share/transfer")));
    }
    #[test]
    fn baidu_transfer_owner_survives_cookie_rotation_for_the_same_account() {
        let credentials = credentials();
        let http = Http::new(vec![
            listing(),
            json!({"user":{"uk":"account-7"}}),
            json!({"user":{"uk":"account-7"}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };

        let before = adapter.transfer_owner_fingerprint(&entry).unwrap();
        credentials
            .save(
                "baidu",
                &serde_json::to_vec(&BaiduSession::cookie("BDUSS_BFESS=renewed")).unwrap(),
            )
            .unwrap();
        let after = adapter.transfer_owner_fingerprint(&entry).unwrap();

        assert_eq!(before, after);
    }
    #[test]
    fn baidu_share_transfer_accepts_verify_filename_and_http_download() {
        let credentials = credentials();
        let mut listing = listing();
        listing["data"]["list"][0]["server_filename"] = json!("verify.mp4");
        listing["data"]["list"][0]["path"] = json!("/verify.mp4");
        let http = Http::new(vec![
            listing,
            json!({"user":{"uk":"account-7"}}),
            json!({"errno":0,"result":{"bdstoken":"write-token"}}),
            json!({"errno":0,"extra":{"list":[{"from_fs_id":11,"to_fs_id":99,"from":"/verify.mp4","to":"/verify.mp4"}]}}),
            json!({"errno":0,"uk":7}),
            json!({"errno":0,"list":[{"fs_id":99,"path":"/verify.mp4","isdir":0}]}),
            json!({"urls":[{"url":"http://d.pcs.baidu.com/file?sign=personal","encrypt":0}]}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let dir = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(dir.path().into()).unwrap(),
        );
        store.upsert(&plan).unwrap();
        let source = plan.source_for("11");
        let download = resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();
        assert_eq!(download.name, "verify.mp4");
        assert_eq!(download.url, "https://d.pcs.baidu.com/file?sign=personal");
        let transfers = store.resolve_entry(&source).unwrap().plan.transfers;
        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].created_file_id, "99");
        assert_eq!(transfers[0].cleanup_state, CloudShareCleanupState::Pending);
        assert_eq!(http.requests.lock().unwrap().len(), 7);
    }
    #[test]
    fn baidu_share_transfer_errors_do_not_record_personal_files() {
        for error in [
            json!({"errno":-20}),
            json!({"errno":-6}),
            json!({"errno":112}),
            json!({"errno":0,"list":{}}),
        ] {
            let credentials = credentials();
            let http = Http::new(vec![
                listing(),
                json!({"user":{"uk":"account-7"}}),
                json!({"errno":0,"result":{"bdstoken":"write-token"}}),
                error,
            ]);
            let adapter =
                provider_share_adapter(CloudProvider::Baidu, &credentials, &http).unwrap();
            let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
                panic!()
            };
            let dir = tempfile::tempdir().unwrap();
            let store = CloudSharePlanStore::new(
                crate::cloud::SystemCredentialStore::new(dir.path().into()).unwrap(),
            );
            store.upsert(&plan).unwrap();
            let source = plan.source_for("11");
            assert!(resolve_current_share_download(&store, adapter.as_ref(), &source).is_err());
            assert!(store
                .resolve_entry(&source)
                .unwrap()
                .plan
                .transfers
                .is_empty());
            assert_eq!(http.requests.lock().unwrap().len(), 4);
        }
    }
}

const PAN: &str = "https://pan.baidu.com";
const PASSPORT: &str = "https://passport.baidu.com";
const BAIDU_HOME: &str = "https://www.baidu.com/";
const BAIDU_WEB_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";
const BAIDU_PAN_USER_AGENT: &str =
    "netdisk;P2SP;3.0.0.8;netdisk;11.12.3;ANG-AN00;android-android;10.0;JSbridge4.4.0;jointBridge;1.1.0;";
const REFERENCE_PREFIX: &str = "bdr_";
pub const BAIDU_RECYCLE_BIN_ID: &str = "__versa_down_baidu_recycle_bin__";
const BAIDU_RECYCLE_BIN_NAME: &str = "回收站";
const BAIDU_UPLOAD_HOST: &str = "https://d.pcs.baidu.com/rest/2.0/pcs/superfile2";
pub(crate) const BAIDU_AUTO_OPEN_LOGIN_JS: &str = include_str!("baidu-auto-open-login.js");

pub(crate) fn is_authorization_page(url: &url::Url) -> bool {
    matches!(url.host_str(), Some("pan.baidu.com"))
}
const BAIDU_UPLOAD_BLOCK_SIZE: usize = 4 * 1024 * 1024;
const BAIDU_LOCATEDOWNLOAD_SIGNING_SALT: &str = "ebrcUYiuxaZv2XGu7KIYKxUrqfnOfpDF";

fn baidu_locatedownload_signature(uid: u64, bduss: &str, timestamp: u64, devuid: &str) -> String {
    let bduss_sha1 = format!("{:x}", Sha1::digest(bduss.as_bytes()));
    let mut digest = Sha1::new();
    digest.update(bduss_sha1.as_bytes());
    digest.update(uid.to_string().as_bytes());
    digest.update(BAIDU_LOCATEDOWNLOAD_SIGNING_SALT.as_bytes());
    digest.update(timestamp.to_string().as_bytes());
    digest.update(devuid.as_bytes());
    format!("{:x}", digest.finalize())
}

fn baidu_locatedownload_https_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let normalized = trimmed
        .strip_prefix("//")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| trimmed.to_owned());
    let mut url = url::Url::parse(&normalized).ok()?;
    if url.scheme() == "http" {
        let host = url.host_str()?.to_ascii_lowercase();
        if !(host.ends_with(".baidu.com") || host.ends_with(".baidupcs.com")) {
            return None;
        }
        url.set_scheme("https").ok()?;
    }
    super::https_transfer_url(url.as_str()).ok()
}

#[derive(Clone, Deserialize, Serialize)]
struct BaiduFileReference {
    fsid: String,
    path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BaiduSession {
    pub cookie: String,
    #[serde(default)]
    pub auth_method: CloudAuthMethod,
}

impl BaiduSession {
    pub fn cookie(cookie: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            auth_method: CloudAuthMethod::Web,
        }
    }
}

fn native_api_cookie(cookie: String) -> String {
    let mut has_legacy_bduss = false;
    let mut secure_webview_bduss = None;
    for pair in cookie.split(';').map(str::trim) {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        match name {
            "BDUSS" if !value.is_empty() => has_legacy_bduss = true,
            "BDUSS_BFESS" if !value.is_empty() => secure_webview_bduss = Some(value),
            _ => {}
        }
    }
    if has_legacy_bduss {
        return cookie;
    }
    let Some(value) = secure_webview_bduss else {
        return cookie;
    };
    format!("{}; BDUSS={value}", cookie.trim_end_matches(';').trim())
}

/// Normalize the complete official Pan web session into a Cookie header.
/// Cookie attributes (including expiry and domain) never enter the saved
/// session.  The browser has already scoped this list to Baidu's domains; do
/// not reduce it to a hand-maintained allowlist because Pan also relies on
/// session-bound cookies such as `PANPSC` and `SCRC` for file operations.
/// Chromium/WebView2 keeps the secure Baidu login credential under
/// `BDUSS_BFESS`, so it must remain intact instead of being rewritten here.
pub fn parse_baidu_web_session(cookies: &[String]) -> Result<BaiduSession, QuarkError> {
    let mut pairs = BTreeMap::new();
    for raw in cookies {
        let pair = raw.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if valid_cookie_name(name) && !value.is_empty() && !value.chars().any(char::is_control) {
            // Cookie names are unique in a Cookie header.  Later values are
            // the current values when multiple browser URLs expose a pair.
            pairs.insert(name, value);
        }
    }
    if !pairs.contains_key("BDUSS") && !pairs.contains_key("BDUSS_BFESS") {
        return Err(QuarkError::AuthorizationExpired);
    }
    Ok(BaiduSession::cookie(
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

#[derive(Default)]
struct BaiduLoginCookies(BTreeMap<String, String>);

impl BaiduLoginCookies {
    fn absorb_set_cookies(&mut self, cookies: &[String]) {
        for raw in cookies {
            self.absorb_pair(raw.split(';').next().unwrap_or_default());
        }
    }

    fn absorb_cookie_header(&mut self, cookie: &str) {
        for pair in cookie.split(';') {
            self.absorb_pair(pair);
        }
    }

    fn absorb_pair(&mut self, pair: &str) {
        let Some((name, value)) = pair.trim().split_once('=') else {
            return;
        };
        if valid_cookie_name(name) && !value.is_empty() && !value.chars().any(char::is_control) {
            self.0.insert(name.to_owned(), value.to_owned());
        }
    }

    fn insert(&mut self, name: &str, value: &str) {
        if valid_cookie_name(name) && !value.is_empty() && !value.chars().any(char::is_control) {
            self.0.insert(name.to_owned(), value.to_owned());
        }
    }

    fn header(&self) -> String {
        self.0
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn pairs(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }

    fn request_headers(&self, form: bool, include_cookie: bool) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([
            ("Accept".into(), "application/json, text/plain, */*".into()),
            ("Referer".into(), format!("{PAN}/")),
            ("User-Agent".into(), BAIDU_WEB_USER_AGENT.into()),
        ]);
        let cookie = self.header();
        if include_cookie && !cookie.is_empty() {
            headers.insert("Cookie".into(), cookie);
        }
        if form {
            headers.insert(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            );
            headers.insert("X-Requested-With".into(), "XMLHttpRequest".into());
        }
        headers
    }
}

fn baidu_login_get<T: CloudHttpTransport + ?Sized>(
    http: &T,
    cookies: &mut BaiduLoginCookies,
    url: String,
) -> Result<CloudHttpResponse, QuarkError> {
    let response = http.send(CloudHttpRequest {
        method: "GET",
        url,
        headers: cookies.request_headers(false, !http.manages_cookie_session()),
        body: None,
    })?;
    cookies.absorb_set_cookies(&response.set_cookies);
    if !(200..300).contains(&response.status) {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    Ok(response)
}

fn baidu_login_json(body: &str) -> Result<Value, QuarkError> {
    let body = body.trim();
    let payload = if body.starts_with('{') {
        body
    } else {
        let start = body.find('(').map(|index| index + 1);
        let end = body.rfind(')');
        match (start, end) {
            (Some(start), Some(end)) if start < end => &body[start..end],
            _ => return Err(QuarkError::WebAuthorizationRequired),
        }
    };
    serde_json::from_str(payload)
        .or_else(|_| {
            // Passport still returns JavaScript object literals with single
            // quoted scalar fields when no JSONP callback is requested.
            serde_json::from_str(&payload.replace('\'', "\""))
        })
        .map_err(|_| QuarkError::WebAuthorizationRequired)
}

fn baidu_password_v4(public_key: &str, password: &str) -> Result<Zeroizing<String>, QuarkError> {
    let public_key = RsaPublicKey::from_public_key_pem(public_key)
        .map_err(|_| QuarkError::WebAuthorizationRequired)?;
    let mut encrypted = public_key
        .encrypt(&mut rand::rngs::OsRng, Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|_| QuarkError::WebAuthorizationRequired)?;
    let result = Zeroizing::new(STANDARD.encode(&encrypted));
    encrypted.zeroize();
    Ok(result)
}

fn unix_millis() -> Result<u128, QuarkError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| QuarkError::InvalidResponse)?
        .as_millis())
}

fn baidu_login_code(value: &Value) -> Option<String> {
    value
        .pointer("/errInfo/no")
        .or_else(|| value.get("err_no"))
        .and_then(string_or_number)
}

fn baidu_html_login_code(body: &str) -> Option<String> {
    let matcher = regex::Regex::new(r#"(?:err_no|errno)=([0-9-]+)"#).ok()?;
    matcher
        .captures(body)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_owned())
}

fn baidu_login_value(value: &Value, name: &str) -> Option<String> {
    value
        .get("data")
        .and_then(|data| data.get(name))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn absorb_baidu_login_result(cookies: &mut BaiduLoginCookies, value: &Value, raw_body: &str) {
    if let Some(cookie) = baidu_login_value(value, "cookieString") {
        cookies.absorb_cookie_header(&cookie);
    }
    for (json_name, cookie_name) in [
        ("bduss", "BDUSS"),
        ("ptoken", "PTOKEN"),
        ("stoken", "STOKEN"),
    ] {
        if let Some(token) = baidu_login_value(value, json_name) {
            cookies.insert(cookie_name, &token);
        }
    }
    if let Ok(matcher) = regex::Regex::new(r"<stoken>netdisk#([A-Za-z0-9_-]+)<") {
        if let Some(token) = matcher
            .captures(raw_body)
            .and_then(|captures| captures.get(1))
        {
            cookies.insert("STOKEN", token.as_str());
        }
    }
}

#[derive(Clone, Debug)]
pub struct BaiduDownload {
    pub url: String,
    pub name: String,
    pub headers: BTreeMap<String, String>,
}

pub struct BaiduClient<'a, T: CloudHttpTransport + ?Sized> {
    session: BaiduSession,
    http: &'a T,
    use_transport_cookie_session: bool,
}

impl<'a, T: CloudHttpTransport + ?Sized> BaiduClient<'a, T> {
    pub fn new(mut session: BaiduSession, http: &'a T) -> Self {
        // WebView2 stores the current QR-login credential as BDUSS_BFESS,
        // while Baidu's native xpan file API still expects the legacy BDUSS
        // key. Keep the secure cookie and add a request-only compatibility
        // alias so sessions saved by earlier releases keep working.
        session.cookie = native_api_cookie(session.cookie);
        Self {
            session,
            http,
            use_transport_cookie_session: false,
        }
    }

    fn from_managed_cookie_session(mut session: BaiduSession, http: &'a T) -> Self {
        session.cookie = native_api_cookie(session.cookie);
        Self {
            session,
            http,
            use_transport_cookie_session: true,
        }
    }

    /// Native Pan endpoints reject a browser origin paired with the Netdisk
    /// client signature. Keep this profile aligned with BaiduPCS-Go: its Pan
    /// requests provide the Netdisk UA and authenticated cookie jar only.
    fn pan_headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([("User-Agent".into(), BAIDU_PAN_USER_AGENT.into())]);
        if !self.use_transport_cookie_session {
            headers.insert("Cookie".into(), self.session.cookie.clone());
        }
        headers
    }

    fn web_headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([
            ("Origin".into(), PAN.into()),
            ("Referer".into(), format!("{PAN}/")),
            ("User-Agent".into(), BAIDU_WEB_USER_AGENT.into()),
        ]);
        if !self.use_transport_cookie_session {
            headers.insert("Cookie".into(), self.session.cookie.clone());
        }
        headers
    }

    fn request(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers: self.pan_headers(),
            body: body.map(|value| value.to_string()),
        })?;
        baidu_json(&response)
    }

    fn web_request(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers: self.web_headers(),
            body: body.map(|value| value.to_string()),
        })?;
        baidu_json(&response)
    }

    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let value = self.account_value()?;
        let user = value.get("user").unwrap_or(&value);
        let id = user
            .get("uk")
            .or_else(|| user.get("user_id"))
            .and_then(string_or_number)
            .filter(|id| !id.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        Ok(CloudAccount {
            provider: CloudProvider::Baidu,
            id,
            display_name: ["netdisk_name", "baidu_name", "nickname"]
                .into_iter()
                .filter_map(|name| user.get(name).and_then(Value::as_str))
                .find(|name| !name.trim().is_empty())
                .unwrap_or("百度网盘")
                .into(),
            auth_method: self.session.auth_method,
        })
    }

    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let value = self.request(
            "GET",
            format!(
                "{PAN}/api/quota?checkexpire=1&checkfree=1&channel=chunlei&web=1&app_id=250528&clienttype=0"
            ),
            None,
        )?;
        CloudStorageCapacity::from_json(&value["total"], &value["used"], false)
    }

    fn stable_account_id(&self) -> Result<Option<String>, QuarkError> {
        let value = self.account_value()?;
        let user = value.get("user").unwrap_or(&value);
        Ok(user
            .get("uk")
            .or_else(|| user.get("user_id"))
            .and_then(string_or_number)
            .filter(|id| !id.is_empty()))
    }

    fn account_value(&self) -> Result<Value, QuarkError> {
        self.request(
            "GET",
            format!("{PAN}/rest/2.0/xpan/nas?method=uinfo&web=1"),
            None,
        )
    }

    pub fn directory(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        let parent = parse_parent_reference(parent)?;
        // BaiduPCS-Go's current directory implementation uses the Pan web
        // listing route (not xpan/file) with this small, fixed query shape.
        // Keeping the native Netdisk header profile is required for the
        // browser-issued cookie jar to be accepted by that route.
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for page in 1..=10_000 {
            let value = self.request(
                "GET",
                format!(
                    "{PAN}/api/list?dir={}&order=name&desc=0&clienttype=0&num=100&page={page}",
                    encoded(&parent.path)
                ),
                None,
            )?;
            let batch = value
                .get("list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            for file in batch {
                let id = file
                    .get("fs_id")
                    .or_else(|| file.get("fsid"))
                    .and_then(string_or_number)
                    .ok_or(QuarkError::InvalidResponse)?;
                if !seen.insert(id) {
                    return Err(QuarkError::InvalidResponse);
                }
                files.push(file.clone());
            }
            if batch.len() < 100 {
                break;
            }
            if page == 10_000 {
                return Err(QuarkError::Provider("百度目录超过分页上限".into()));
            }
        }
        let directory_id = if parent.path == "/" {
            "/".into()
        } else {
            encode_reference(parent.clone())?
        };
        let mut items = files
            .iter()
            .map(|file| {
                Ok(CloudFile {
                    id: encode_reference(BaiduFileReference {
                        fsid: file
                            .get("fs_id")
                            .or_else(|| file.get("fsid"))
                            .and_then(string_or_number)
                            .ok_or(QuarkError::InvalidResponse)?,
                        path: required(file, "path")?,
                    })?,
                    parent_id: directory_id.clone(),
                    name: required(file, "server_filename")
                        .or_else(|_| required(file, "name"))
                        .or_else(|_| {
                            file.get("path")
                                .and_then(Value::as_str)
                                .and_then(|path| path.rsplit('/').next())
                                .filter(|name| !name.is_empty())
                                .map(str::to_owned)
                                .ok_or(QuarkError::InvalidResponse)
                        })?,
                    kind: if baidu_is_directory(file) {
                        "folder"
                    } else {
                        "file"
                    }
                    .into(),
                    size: file.get("size").and_then(Value::as_u64),
                    modified_at: file.get("server_mtime").and_then(string_or_number),
                })
            })
            .collect::<Result<Vec<_>, QuarkError>>()?;
        if parent.path == "/" {
            items.push(CloudFile {
                id: BAIDU_RECYCLE_BIN_ID.into(),
                parent_id: directory_id.clone(),
                name: BAIDU_RECYCLE_BIN_NAME.into(),
                kind: "folder".into(),
                size: None,
                modified_at: None,
            });
        }
        Ok(CloudDirectory {
            id: directory_id,
            items,
        })
    }

    pub fn list_recycle_bin(&self) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        let mut seen = BTreeSet::new();
        for page in 1..=10_000 {
            let value = self.web_request(
                "GET",
                format!("{PAN}/api/recycle/list/?num=100&page={page}&web=1"),
                None,
            )?;
            let files = value
                .get("list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let length = files.len();
            for file in files {
                let id = file
                    .get("fs_id")
                    .or_else(|| file.get("fsid"))
                    .and_then(string_or_number)
                    .filter(|id| valid_baidu_recycle_record_id(id))
                    .ok_or(QuarkError::InvalidResponse)?;
                if !seen.insert(id.clone()) {
                    return Err(QuarkError::InvalidResponse);
                }
                items.push(CloudFile {
                    id,
                    parent_id: BAIDU_RECYCLE_BIN_ID.into(),
                    name: required(file, "server_filename").or_else(|_| required(file, "name"))?,
                    kind: if baidu_is_directory(file) {
                        "folder"
                    } else {
                        "file"
                    }
                    .into(),
                    size: file.get("size").and_then(Value::as_u64),
                    modified_at: file.get("server_mtime").and_then(string_or_number),
                });
            }
            if length < 100 {
                return Ok(CloudDirectory {
                    id: BAIDU_RECYCLE_BIN_ID.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn restore_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        self.recycle_operation("restore", ids)
    }

    pub fn delete_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        self.recycle_operation("delete", ids)
    }

    /// Creates a personal Baidu Netdisk share through the same web endpoint
    /// used by the official share dialog. File IDs in the drive layer are
    /// opaque references, so unwrap their numeric `fsid` only at this adapter
    /// boundary.
    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
        }
        use rand::distributions::{Alphanumeric, DistString};
        let pass_code = if options.generate_pass_code {
            Alphanumeric.sample_string(&mut rand::thread_rng(), 4)
        } else {
            options.pass_code.as_deref().unwrap_or_default().trim().to_owned()
        };
        if pass_code.len() != 4 || !pass_code.bytes().all(|ch| ch.is_ascii_alphanumeric()) {
            return Err(QuarkError::InvalidInput(BAIDU_SHARE_PASS_CODE_CONSTRAINT.hint.into()));
        }
        let fsids = file_ids
            .iter()
            .map(|id| {
                parse_file_reference(id)?
                    .fsid
                    .parse::<u64>()
                    .map_err(|_| QuarkError::InvalidInput("百度网盘文件标识无效".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let token = self.web_token()?;
        let value = self.web_form(
            "/share/set",
            &write_query(&token, "channel=chunlei"),
            form(&[
                ("fid_list", json!(fsids).to_string()),
                ("schannel", "4".into()),
                ("channel_list", "[]".into()),
                ("period", options.expires_in_days.unwrap_or(0).to_string()),
                ("pwd", pass_code.clone()),
                ("eflag_disable", "true".into()),
            ]),
        )?;
        baidu_share_from_value(&value, Some(file_ids), Some(&pass_code))
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let mut shares = Vec::new();
        for page in 1..=10_000 {
            let value = self.web_request(
                "GET",
                format!(
                    "{PAN}/share/record?channel=chunlei&clienttype=0&app_id=250528&web=1&page={page}&num=100&order=time&desc=1"
                ),
                None,
            )?;
            let records = value
                .get("list")
                .or_else(|| value.pointer("/data/list"))
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = records.len();
            shares.extend(
                records
                    .iter()
                    .map(|record| baidu_share_from_value(record, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if count < 100 {
                return Ok(shares);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        if share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        self.web_form(
            "/share/cancel",
            "channel=chunlei&clienttype=0&app_id=250528&web=1",
            form(&[("shareid_list", json!(share_ids).to_string())]),
        )?;
        Ok(())
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<(), QuarkError> {
        let parent = parse_parent_reference(parent)?;
        let path = join_path(&parent.path, name)?;
        let token = self.web_token()?;
        self.web_form(
            "/api/create",
            &write_query(&token, "a=commit"),
            form(&[
                ("path", path),
                ("isdir", "1".into()),
                ("block_list", "[]".into()),
            ]),
        )
        .map(|_| ())
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<(), QuarkError> {
        let file = parse_file_reference(id)?;
        validate_name(name)?;
        let token = self.web_token()?;
        self.web_file_manager(
            "rename",
            &token,
            json!([{ "path": file.path, "newname": name }]),
        )
    }

    pub fn move_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.file_manager_operation("move", ids, parent)
    }
    pub fn copy_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        self.file_manager_operation("copy", ids, parent)
    }
    pub fn trash(&self, ids: &[String]) -> Result<(), QuarkError> {
        let paths = ids
            .iter()
            .map(|id| parse_file_reference(id).map(|reference| reference.path))
            .collect::<Result<Vec<_>, _>>()?;
        if paths.is_empty() {
            return Err(QuarkError::InvalidInput("请选择文件".into()));
        }
        let token = self.web_token()?;
        self.web_file_manager("delete", &token, json!(paths))
    }

    fn recycle_operation(&self, action: &str, ids: &[String]) -> Result<(), QuarkError> {
        let fidlist = baidu_recycle_fidlist(ids)?;
        let token = self.web_token()?;
        let response = self.web_form_response(
            &format!("/api/recycle/{action}"),
            &format!("channel=chunlei&async=1&bdstoken={}", encoded(&token)),
            form(&[("fidlist", fidlist)]),
        )?;
        if action == "delete"
            && serde_json::from_str::<Value>(&response.body)
                .ok()
                .and_then(|value| value.get("errno").and_then(baidu_error_number))
                == Some(132)
        {
            return Err(QuarkError::BaiduOperationVerificationRequired);
        }
        baidu_json(&response).map(|_| ())
    }

    pub(crate) fn trash_transferred_files(&self, ids: &[String]) -> Result<(), QuarkError> {
        let expected: std::collections::BTreeSet<_> = ids.iter().cloned().collect();
        if expected.is_empty()
            || expected.len() != ids.len()
            || ids
                .iter()
                .any(|id| id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(QuarkError::InvalidResponse);
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut paths = Vec::new();
        for file in self.directory("/")?.items {
            if file.kind != "file" {
                continue;
            }
            let reference = parse_file_reference(&file.id)?;
            if expected.contains(&reference.fsid) && seen.insert(reference.fsid) {
                paths.push(reference.path);
            }
        }
        if seen != expected {
            return Err(QuarkError::InvalidResponse);
        }
        let token = self.web_token()?;
        self.web_file_manager("delete", &token, json!(paths))
    }

    fn transferred_file_reference(&self, fsid: &str) -> Result<String, QuarkError> {
        for file in self.directory("/")?.items {
            if file.kind == "file" && parse_file_reference(&file.id)?.fsid == fsid {
                return Ok(file.id);
            }
        }
        Err(QuarkError::SourceFileMissing)
    }

    fn file_manager_operation(
        &self,
        operation: &str,
        ids: &[String],
        parent: &str,
    ) -> Result<(), QuarkError> {
        let parent = parse_parent_reference(parent)?;
        let files = ids
            .iter()
            .map(|id| parse_file_reference(id))
            .collect::<Result<Vec<_>, _>>()?;
        if files.is_empty() {
            return Err(QuarkError::InvalidInput("请选择文件".into()));
        }
        let filelist = files
            .iter()
            .map(|file| {
                Ok(json!({
                    "path": file.path,
                    "dest": parent.path,
                    "newname": basename(&file.path)?,
                }))
            })
            .collect::<Result<Vec<_>, QuarkError>>()?;
        let token = self.web_token()?;
        self.web_file_manager(operation, &token, json!(filelist))
    }

    /// Starts the official web precreate flow after obtaining a fresh token.
    pub fn upload(
        &self,
        parent: &str,
        name: &str,
        size: u64,
        content_md5: &str,
    ) -> Result<Value, QuarkError> {
        let parent = parse_parent_reference(parent)?;
        let path = join_path(&parent.path, name)?;
        if content_md5.is_empty() {
            return Err(QuarkError::InvalidInput("文件校验值无效".into()));
        }
        let token = self.web_token()?;
        self.web_form(
            "/api/precreate",
            &write_query(&token, ""),
            form(&[
                ("path", path),
                ("size", size.to_string()),
                ("isdir", "0".into()),
                ("autoinit", "1".into()),
                ("block_list", json!([content_md5]).to_string()),
            ]),
        )
    }

    /// Uploads a regular local file through the official precreate, slice and
    /// commit sequence. Upload IDs and transfer requests stay entirely local
    /// to this call and are never stored with the session.
    pub fn upload_file<U: CloudUploadTransport>(
        &self,
        parent: &str,
        local_path: &Path,
        upload_http: &U,
    ) -> Result<CloudFile, QuarkError> {
        let snapshot = super::UploadFileSnapshot::capture(local_path)?;
        let parent = parse_parent_reference(parent)?;
        let (name, size) = local_file_metadata(local_path)?;
        let path = join_path(&parent.path, &name)?;
        let blocks = baidu_block_hashes(local_path, size)?;
        snapshot.validate()?;
        let token = self.web_token()?;
        let precreate = self.web_form(
            "/api/precreate",
            &write_query(&token, ""),
            form(&[
                ("path", path.clone()),
                ("size", size.to_string()),
                ("isdir", "0".into()),
                ("autoinit", "1".into()),
                ("rtype", "3".into()),
                ("block_list", json!(blocks).to_string()),
            ]),
        )?;
        // A zero-slice/rapid-upload response still needs the normal create
        // commit unless the service has already returned a concrete file.
        // Treating precreate itself as a completed file loses valid rapid
        // uploads because that response normally contains only an upload id.
        snapshot.validate()?;
        if precreate.get("return_type").and_then(Value::as_i64) == Some(2)
            && precreate.get("info").is_some()
        {
            return upload_file_result(&precreate, &parent, &name, size);
        }
        let upload_id = required(&precreate, "uploadid")?;
        let requested_blocks = precreate
            .get("block_list")
            .and_then(Value::as_array)
            .ok_or(QuarkError::InvalidResponse)?
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|index| *index < blocks.len())
                    .ok_or(QuarkError::InvalidResponse)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for part in requested_blocks {
            let offset = part as u64 * BAIDU_UPLOAD_BLOCK_SIZE as u64;
            let bytes =
                snapshot.slice(offset, (size - offset).min(BAIDU_UPLOAD_BLOCK_SIZE as u64))?;
            let response = upload_http
                .send_upload(self.slice_request(&path, &upload_id, part, &token, &name, bytes)?)?;
            validate_slice_response(&response, &blocks[part])?;
        }
        snapshot.validate()?;
        let committed = self.web_form(
            "/api/create",
            &write_query(&token, "a=commit"),
            form(&[
                ("path", path),
                ("size", size.to_string()),
                ("isdir", "0".into()),
                ("uploadid", upload_id),
                ("block_list", json!(blocks).to_string()),
            ]),
        )?;
        upload_file_result(&committed, &parent, &name, size)
    }

    pub fn download(&self, id: &str) -> Result<BaiduDownload, QuarkError> {
        let file = parse_file_reference(id)?;
        // This driver stores BDUSS cookies, not OAuth access tokens. The
        // OAuth filemetas dlink flow returns 9019 for otherwise valid web
        // sessions. Use the authenticated PCS protocol (BaiduPCS-Go).
        self.locatedownload(&file)
    }

    fn locatedownload(&self, file: &BaiduFileReference) -> Result<BaiduDownload, QuarkError> {
        let bduss = self
            .session
            .cookie
            .split(';')
            .map(str::trim)
            .filter_map(|pair| pair.split_once('='))
            .find_map(|(name, value)| (name == "BDUSS" && !value.is_empty()).then_some(value))
            .ok_or(QuarkError::AuthorizationExpired)?;
        let uid = self
            .stable_account_id()?
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(QuarkError::InvalidResponse)?;
        // PCS resolves by path, while queued downloads are bound to fs_id.
        // Re-read the original parent before handing it a persisted path:
        // never download a different file that has since reused that name.
        let parent_path = file
            .path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let parent = if parent_path.is_empty() {
            "/".to_owned()
        } else {
            // Directory listing is path-addressed; this reference stays local
            // and is used only to obtain the current child identities.
            encode_reference(BaiduFileReference {
                fsid: "0".into(),
                path: parent_path.into(),
            })?
        };
        let current = self
            .directory(&parent)?
            .items
            .into_iter()
            .filter(|item| item.kind == "file")
            .map(|item| parse_file_reference(&item.id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|current| current.fsid == file.fsid)
            .ok_or_else(|| QuarkError::SourceFileMissing)?;
        let file = &current;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| QuarkError::InvalidResponse)?
            .as_secs();
        let devuid = format!("{:X}|0", md5::compute(bduss.as_bytes()));
        let signature = baidu_locatedownload_signature(uid, bduss, timestamp, &devuid);
        let timestamp = timestamp.to_string();
        let mut endpoint = url::Url::parse("https://pcs.baidu.com/rest/2.0/pcs/file")
            .map_err(|_| QuarkError::InvalidResponse)?;
        endpoint.query_pairs_mut().extend_pairs([
            ("ant", "1"),
            ("check_blue", "1"),
            ("es", "1"),
            ("esl", "1"),
            ("app_id", "250528"),
            ("method", "locatedownload"),
            ("path", file.path.as_str()),
            ("ver", "4.0"),
            ("clienttype", "17"),
            ("channel", "0"),
            ("apn_id", "1_0"),
            ("freeisp", "0"),
            ("queryfree", "0"),
            ("use", "0"),
            ("time", timestamp.as_str()),
            ("rand", signature.as_str()),
            ("devuid", devuid.as_str()),
            ("cuid", devuid.as_str()),
        ]);
        let value = self.request("POST", endpoint.into(), None)?;
        let urls = value
            .get("urls")
            .and_then(Value::as_array)
            .ok_or(QuarkError::InvalidResponse)?;
        let candidates = urls
            .iter()
            .filter(|candidate| candidate.get("encrypt").and_then(Value::as_i64) == Some(0))
            .filter_map(|candidate| candidate.get("url").and_then(Value::as_str))
            .filter_map(baidu_locatedownload_https_url)
            .collect::<Vec<_>>();
        let url = candidates
            .iter()
            .find(|candidate| {
                url::Url::parse(candidate)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
                    .is_some_and(|host| !host.starts_with("nb.cache"))
            })
            .or_else(|| candidates.first())
            .cloned()
            .ok_or(QuarkError::InvalidResponse)?;
        let name = file
            .path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        Ok(BaiduDownload {
            url,
            name: name.into(),
            headers: self.pan_headers(),
        })
    }

    fn web_token(&self) -> Result<String, QuarkError> {
        let value = self.web_request(
            "GET",
            format!(
                "{PAN}/api/gettemplatevariable?clienttype=0&app_id=250528&web=1&fields=%5B%22bdstoken%22%5D"
            ),
            None,
        )?;
        required(
            value.get("result").ok_or(QuarkError::InvalidResponse)?,
            "bdstoken",
        )
    }

    fn slice_request(
        &self,
        path: &str,
        upload_id: &str,
        part: usize,
        token: &str,
        name: &str,
        bytes: super::UploadBody,
    ) -> Result<UploadHttpRequest, QuarkError> {
        let mut url =
            url::Url::parse(BAIDU_UPLOAD_HOST).map_err(|_| QuarkError::InvalidResponse)?;
        url.query_pairs_mut()
            .append_pair("method", "upload")
            .append_pair("type", "tmpfile")
            .append_pair("app_id", "250528")
            .append_pair("bdstoken", token)
            .append_pair("path", path)
            .append_pair("uploadid", upload_id)
            .append_pair("partseq", &part.to_string());
        let url = super::https_transfer_url(url.as_str())?;
        let (content_type, body) = super::multipart_file(&[], "file", name, bytes)?;
        let mut headers = self.web_headers();
        headers.insert("Content-Type".into(), content_type);
        Ok(UploadHttpRequest {
            method: "POST",
            url,
            headers,
            body,
        })
    }

    fn web_form(&self, path: &str, query: &str, body: String) -> Result<Value, QuarkError> {
        baidu_json(&self.web_form_response(path, query, body)?)
    }

    fn web_form_response(
        &self,
        path: &str,
        query: &str,
        body: String,
    ) -> Result<CloudHttpResponse, QuarkError> {
        let mut headers = self.web_headers();
        headers.insert(
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        );
        self.http.send(CloudHttpRequest {
            method: "POST",
            url: format!("{PAN}{path}?{query}"),
            headers,
            body: Some(body),
        })
    }

    fn web_file_manager(
        &self,
        operation: &str,
        token: &str,
        filelist: Value,
    ) -> Result<(), QuarkError> {
        self.web_form(
            "/api/filemanager",
            &write_query(token, &format!("opera={operation}&async=2&onnest=fail")),
            form(&[("filelist", filelist.to_string())]),
        )
        .map(|_| ())
    }

    /// Follow the same v4 Passport handshake used by Baidu's current login
    /// page: initialize a token, fetch the per-attempt RSA key, preflight the
    /// username and only then submit an encrypted password. CAPTCHA, SMS and
    /// risk challenges are handed to the existing official browser flow.
    pub fn login_password(
        username: &str,
        password: &str,
        http: &'a T,
    ) -> Result<BaiduSession, QuarkError> {
        if let Some(isolated) = http.isolated_session()? {
            return Self::login_password_in_session(username, password, isolated.as_ref());
        }
        Self::login_password_in_session(username, password, http)
    }

    fn login_password_in_session<U: CloudHttpTransport + ?Sized>(
        username: &str,
        password: &str,
        http: &U,
    ) -> Result<BaiduSession, QuarkError> {
        if username.trim().is_empty() || password.is_empty() {
            return Err(QuarkError::InvalidInput("请输入账号和密码".into()));
        }
        let password = Zeroizing::new(password.to_owned());
        let mut cookies = BaiduLoginCookies::default();
        baidu_login_get(http, &mut cookies, BAIDU_HOME.into())?;
        let gid = uuid::Uuid::new_v4().to_string().to_ascii_uppercase();
        let timestamp = unix_millis()?;
        let api = baidu_login_json(
            &baidu_login_get(
                http,
                &mut cookies,
                format!(
                    "{PASSPORT}/v2/api/?getapi&tpl=netdisk&apiver=v3&tt={timestamp}&class=login&gid={gid}&loginVersion=v4&logintype=basicLogin"
                ),
            )?
            .body,
        )?;
        if baidu_login_code(&api).as_deref() != Some("0") {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let token = baidu_login_value(&api, "token").ok_or(QuarkError::WebAuthorizationRequired)?;
        let key = baidu_login_json(
            &baidu_login_get(
                http,
                &mut cookies,
                format!(
                    "{PASSPORT}/v2/getpublickey?token={}&tpl=netdisk&subpro=&apiver=v3&tt={timestamp}&gid={gid}&loginVersion=v4",
                    encoded(&token)
                ),
            )?
            .body,
        )?;
        let key_code = key
            .get("errno")
            .and_then(string_or_number)
            .or_else(|| baidu_login_code(&key));
        if key_code.as_deref() != Some("0") {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let public_key = key
            .get("pubkey")
            .and_then(Value::as_str)
            .filter(|value| value.contains("BEGIN PUBLIC KEY"))
            .ok_or(QuarkError::WebAuthorizationRequired)?;
        let rsa_key = key
            .get("key")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(QuarkError::WebAuthorizationRequired)?;
        let check = baidu_login_json(
            &baidu_login_get(
                http,
                &mut cookies,
                format!(
                    "{PASSPORT}/v2/api/?logincheck&token={}&tpl=netdisk&apiver=v3&tt={}&sub_source=leadsetpwd&username={}&isphone=false&loginVersion=v4&gid={gid}&dv=",
                    encoded(&token),
                    unix_millis()?,
                    encoded(username.trim())
                ),
            )?
            .body,
        )?;
        if baidu_login_code(&check).as_deref() != Some("0") {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let code_string = baidu_login_value(&check, "codeString").unwrap_or_default();
        if !code_string.is_empty() {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let encrypted = baidu_password_v4(public_key, &password)?;
        let callback = format!("parent.bd__pcbs__{}", uuid::Uuid::new_v4().simple());
        let body = form(&[
            (
                "staticpage",
                format!("{PASSPORT}/static/passpc-account/html/v3Jump.html"),
            ),
            ("charset", "UTF-8".into()),
            ("token", token),
            ("tpl", "netdisk".into()),
            ("subpro", String::new()),
            ("apiver", "v3".into()),
            ("tt", unix_millis()?.to_string()),
            ("codestring", code_string),
            ("safeflg", "0".into()),
            ("u", format!("{PAN}/")),
            ("isPhone", "false".into()),
            ("quick_user", "0".into()),
            ("logintype", "basicLogin".into()),
            ("logLoginType", "pc_loginBasic".into()),
            ("idc", String::new()),
            ("loginmerge", "true".into()),
            ("username", username.trim().to_owned()),
            ("password", encrypted.to_string()),
            ("verifycode", String::new()),
            ("mem_pass", "on".into()),
            ("rsakey", rsa_key.to_owned()),
            ("crypttype", "12".into()),
            ("ppui_logintime", "1000".into()),
            ("loginVersion", "v4".into()),
            ("supportdv", "1".into()),
            ("gid", gid),
            ("countrycode", String::new()),
            ("FP_UID", String::new()),
            ("FP_INFO", String::new()),
            ("dv", String::new()),
            ("traceid", String::new()),
            ("callback", callback),
        ]);
        let response = http.send(CloudHttpRequest {
            method: "POST",
            url: format!("{PASSPORT}/v2/api/?login"),
            headers: cookies.request_headers(true, !http.manages_cookie_session()),
            body: Some(body),
        })?;
        cookies.absorb_set_cookies(&response.set_cookies);
        if !(200..300).contains(&response.status) {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let value = baidu_login_json(&response.body).ok();
        let code = value
            .as_ref()
            .and_then(baidu_login_code)
            .or_else(|| baidu_html_login_code(&response.body));
        match code.as_deref() {
            Some("0") => {}
            Some("3" | "6" | "257" | "500001" | "500002") => {
                return Err(QuarkError::CaptchaRequired)
            }
            Some(
                "5" | "16" | "120016" | "120019" | "120021" | "400023" | "400031" | "400101"
                | "50000" | "50010" | "50043" | "50045" | "50046" | "50052",
            ) => return Err(QuarkError::WebAuthorizationRequired),
            _ if value.as_ref().is_some_and(challenge) => {
                return Err(QuarkError::WebAuthorizationRequired)
            }
            _ => {
                let message =
                    if std::env::var("VERSADOWN_BAIDU_DIAGNOSTIC").as_deref() == Ok("1") {
                        format!(
                            "登录被服务拒绝（code {}）",
                            code.as_deref().unwrap_or("unparseable")
                        )
                    } else {
                        "登录被服务拒绝".into()
                    };
                return Err(QuarkError::Provider(message));
            }
        }
        if let Some(value) = value.as_ref() {
            absorb_baidu_login_result(&mut cookies, value, &response.body);
        }
        let authenticated_cookies = http.cookies_for_url(PAN)?;
        let mut session = if authenticated_cookies.is_empty() {
            parse_baidu_web_session(&cookies.pairs())?
        } else {
            parse_baidu_web_session(&authenticated_cookies)?
        };
        session.auth_method = CloudAuthMethod::Password;
        let client = if http.manages_cookie_session() {
            BaiduClient::from_managed_cookie_session(session.clone(), http)
        } else {
            BaiduClient::new(session.clone(), http)
        };
        let validated_account = client.account()?;
        client.directory("/")?;
        let final_cookies = http.cookies_for_url(PAN)?;
        if !final_cookies.is_empty() {
            session = parse_baidu_web_session(&final_cookies)?;
            session.auth_method = CloudAuthMethod::Password;
        }
        // Validate the exact normalized Cookie header that will be encrypted
        // and replayed after restart. The managed login jar can contain a
        // different host/domain duplicate than the final serialized snapshot;
        // accepting only the jar-backed requests can persist a session that
        // immediately fails with errno -6 in a fresh production transport.
        let persisted = BaiduClient::new(session.clone(), http);
        if persisted.account()?.id != validated_account.id {
            return Err(QuarkError::AuthorizationExpired);
        }
        persisted.directory("/")?;
        Ok(session)
    }
}

fn local_file_metadata(path: &Path) -> Result<(String, u64), QuarkError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(QuarkError::InvalidInput("请选择普通文件上传".into()));
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| QuarkError::InvalidInput("文件名无效".into()))?
        .to_owned();
    validate_name(&name)?;
    Ok((name, metadata.len()))
}

fn baidu_block_hashes(path: &Path, size: u64) -> Result<Vec<String>, QuarkError> {
    let mut file = std::fs::File::open(path)
        .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
    let block_count = usize::try_from(
        (size + BAIDU_UPLOAD_BLOCK_SIZE as u64 - 1) / BAIDU_UPLOAD_BLOCK_SIZE as u64,
    )
    .map_err(|_| QuarkError::InvalidInput("文件过大，无法上传".into()))?;
    if block_count == 0 {
        return Err(QuarkError::InvalidInput("不能上传空文件".into()));
    }
    let mut hashes = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let mut bytes = vec![0; BAIDU_UPLOAD_BLOCK_SIZE];
        let read = file
            .read(&mut bytes)
            .map_err(|_| QuarkError::InvalidInput("无法读取选中的本地文件".into()))?;
        if read == 0 {
            return Err(QuarkError::InvalidInput("本地文件在上传前发生变化".into()));
        }
        bytes.truncate(read);
        hashes.push(format!("{:x}", md5::compute(bytes)));
    }
    Ok(hashes)
}

fn validate_slice_response(
    response: &UploadHttpResponse,
    expected_md5: &str,
) -> Result<(), QuarkError> {
    if !(200..300).contains(&response.status) {
        return Err(QuarkError::Provider("百度网盘拒绝上传分片".into()));
    }
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
    if required(&value, "md5")? != expected_md5 {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(())
}

fn upload_file_result(
    value: &Value,
    parent: &BaiduFileReference,
    name: &str,
    size: u64,
) -> Result<CloudFile, QuarkError> {
    let value = value.get("info").unwrap_or(value);
    let fsid = value
        .get("fs_id")
        .or_else(|| value.get("fsid"))
        .and_then(string_or_number)
        .ok_or(QuarkError::InvalidResponse)?;
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(join_path(&parent.path, name)?);
    let id = encode_reference(BaiduFileReference { fsid, path })?;
    Ok(CloudFile {
        id,
        parent_id: if parent.path == "/" {
            "/".into()
        } else {
            encode_reference(parent.clone())?
        },
        name: value
            .get("server_filename")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .into(),
        kind: "file".into(),
        size: value.get("size").and_then(Value::as_u64).or(Some(size)),
        modified_at: value.get("server_mtime").and_then(string_or_number),
    })
}

fn baidu_json(response: &CloudHttpResponse) -> Result<Value, QuarkError> {
    if response.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
    if challenge(&value) {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    let errno = ["errno", "error_code", "err_no"]
        .iter()
        .filter_map(|key| value.get(key).and_then(baidu_error_number))
        .find(|code| *code != 0)
        .unwrap_or(0);
    if errno != 0 {
        if matches!(errno, -6 | -7 | 110 | 111 | 31045) {
            if std::env::var("VERSADOWN_BAIDU_DIAGNOSTIC").as_deref() == Ok("1") {
                return Err(QuarkError::Provider(format!(
                    "百度授权拒绝（errno {errno}，HTTP {}）",
                    response.status
                )));
            }
            return Err(QuarkError::AuthorizationExpired);
        }
        return Err(QuarkError::Provider(format!(
            "百度网盘拒绝请求（错误码 {errno}，HTTP {}）",
            response.status
        )));
    }
    if !(200..300).contains(&response.status) {
        return Err(QuarkError::Provider("百度网盘请求失败".into()));
    }
    Ok(value)
}

fn challenge(value: &Value) -> bool {
    ["errno", "err_no", "error_code"].iter().any(|key| {
        matches!(
            value.get(key).and_then(baidu_error_number),
            Some(-20 | 6 | 16 | 257 | 9019 | 120021 | 400031 | 400023 | 400101 | 500001 | 500002)
        )
    }) || [
        "captcha",
        "vcode",
        "vcode_str",
        "codeString",
        "verify_url",
        "captcha_url",
    ]
    .iter()
    .any(|key| {
        value
            .get(key)
            .is_some_and(|field| field.as_str().is_some_and(|s| !s.is_empty()))
    })
}
fn baidu_error_number(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}
fn baidu_is_directory(value: &Value) -> bool {
    value.get("isdir").and_then(baidu_error_number) == Some(1)
}
fn string_or_number(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn valid_baidu_recycle_record_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn baidu_recycle_fidlist(ids: &[String]) -> Result<String, QuarkError> {
    if ids.is_empty()
        || ids.iter().any(|id| !valid_baidu_recycle_record_id(id))
        || ids
            .iter()
            .enumerate()
            .any(|(index, id)| ids[..index].contains(id))
    {
        return Err(QuarkError::InvalidInput("回收站记录 ID 无效".into()));
    }
    let records = ids
        .iter()
        .map(|id| {
            id.parse::<u64>()
                .map(Value::from)
                .map_err(|_| QuarkError::InvalidInput("回收站记录 ID 无效".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    serde_json::to_string(&records).map_err(|_| QuarkError::InvalidResponse)
}

fn encode_reference(reference: BaiduFileReference) -> Result<String, QuarkError> {
    if reference.fsid.is_empty()
        || !reference.fsid.bytes().all(|byte| byte.is_ascii_digit())
        || reference.path == "/"
    {
        return Err(QuarkError::InvalidResponse);
    }
    validate_path(&reference.path)?;
    let bytes = serde_json::to_vec(&reference).map_err(|_| QuarkError::InvalidResponse)?;
    Ok(format!(
        "{REFERENCE_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn parse_parent_reference(value: &str) -> Result<BaiduFileReference, QuarkError> {
    if value == "/" {
        return Ok(BaiduFileReference {
            fsid: String::new(),
            path: "/".into(),
        });
    }
    parse_file_reference(value)
}

fn parse_file_reference(value: &str) -> Result<BaiduFileReference, QuarkError> {
    let encoded = value
        .strip_prefix(REFERENCE_PREFIX)
        .ok_or(QuarkError::InvalidResponse)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| QuarkError::InvalidResponse)?;
    let reference: BaiduFileReference =
        serde_json::from_slice(&bytes).map_err(|_| QuarkError::InvalidResponse)?;
    if reference.fsid.is_empty() || !reference.fsid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(QuarkError::InvalidResponse);
    }
    validate_path(&reference.path)?;
    if reference.path == "/" {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(reference)
}

fn validate_path(path: &str) -> Result<(), QuarkError> {
    if !path.starts_with('/') || path.contains(['?', '#']) || path.chars().any(char::is_control) {
        return Err(QuarkError::InvalidResponse);
    }
    if path != "/" {
        if path.ends_with('/') {
            return Err(QuarkError::InvalidResponse);
        }
        for segment in path.split('/').skip(1) {
            if segment.is_empty() || matches!(segment, "." | "..") {
                return Err(QuarkError::InvalidResponse);
            }
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), QuarkError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(QuarkError::InvalidInput("文件名无效".into()));
    }
    Ok(())
}

fn basename(path: &str) -> Result<&str, QuarkError> {
    validate_path(path)?;
    path.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or(QuarkError::InvalidResponse)
}

fn join_path(parent: &str, name: &str) -> Result<String, QuarkError> {
    validate_name(name)?;
    Ok(if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    })
}

fn form(fields: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

fn baidu_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = value
        .get("shareid")
        .or_else(|| value.get("share_id"))
        .or_else(|| value.get("shareId"))
        .and_then(string_or_number)
        .filter(|id| !id.is_empty())
        .ok_or(QuarkError::InvalidResponse)?;
    let short_url = ["link", "shorturl", "short_url", "shortlink"]
        .into_iter()
        .filter_map(|name| value.get(name).and_then(Value::as_str))
        .map(str::trim)
        .find(|url| !url.is_empty())
        .map(|url| {
            if url.starts_with("http://") || url.starts_with("https://") {
                url.to_owned()
            } else {
                format!("{PAN}/s/1{}", url.strip_prefix('1').unwrap_or(url))
            }
        })
        .ok_or(QuarkError::InvalidResponse)?;
    let source_file_ids = value
        .get("fid_list")
        .or_else(|| value.get("fsIds"))
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(string_or_number).collect::<Vec<_>>())
        .or_else(|| {
            value
                .get("fs_id")
                .or_else(|| value.get("fsid"))
                .and_then(string_or_number)
                .map(|id| vec![id])
        })
        .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
        .unwrap_or_default();
    let record_name = ["filename", "title", "name"]
        .into_iter()
        .filter_map(|name| value.get(name).and_then(Value::as_str))
        .map(str::trim)
        .find(|name| !name.is_empty());
    let name = value
        .get("typicalPath")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty() && *path != "0")
        .and_then(|path| path.rsplit('/').find(|part| !part.is_empty()))
        .or_else(|| record_name.filter(|name| *name != "0"))
        .or(record_name)
        .unwrap_or("分享文件");
    Ok(CloudShare {
        id,
        url: short_url,
        name: name.into(),
        kind: if baidu_is_directory(value) {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids,
        pass_code: value
            .get("pwd")
            .or_else(|| value.get("share_pwd"))
            .or_else(|| value.get("passwd"))
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: value
            .get("ctime")
            .or_else(|| value.get("create_time"))
            .and_then(string_or_number),
        expires_at: value
            .get("expire_time")
            .or_else(|| value.get("expired_time"))
            .and_then(string_or_number),
    })
}

fn write_query(token: &str, operation: &str) -> String {
    let mut query = format!(
        "bdstoken={}&clienttype=0&app_id=250528&web=1",
        encoded(token)
    );
    if !operation.is_empty() {
        query.push('&');
        query.push_str(operation);
    }
    query
}

use super::share::{
    CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudSharePlanStore,
    CloudShareResolvedEntry, CloudTransferCapacity, RecordedTransferDownload,
    RecordedTransferResolver, ShareInspect, ShareInspection, ShareOperationResult, ShareResolution,
    ShareResolve, ShareTransfer, TransferRequest,
};

pub struct BaiduShareAdapter<'a> {
    pub credentials: &'a dyn super::CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
impl BaiduShareAdapter<'_> {
    fn client(&self) -> Result<BaiduClient<'_, dyn CloudHttpTransport + '_>, QuarkError> {
        let bytes = self
            .credentials
            .load("baidu")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: BaiduSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.cookie.trim().is_empty() {
            return Err(QuarkError::NotAuthorized);
        }
        Ok(BaiduClient::new(session, self.http))
    }
}
impl<T: CloudHttpTransport + ?Sized> BaiduClient<'_, T> {
    /// Verification is a web session handshake, not a `pwd` option on share/list.
    /// Keep these cookies local to this inspection; they are not account credentials.
    fn verify_share_access(
        &mut self,
        short: &str,
        code: &str,
    ) -> Result<(String, String), QuarkError> {
        let mut cookies = BaiduLoginCookies::default();
        cookies.absorb_cookie_header(&self.session.cookie);
        cookies.0.remove("BDCLND");
        self.session.cookie = cookies.header();
        let page_url = format!("{PAN}/s/1{}", encoded(short));
        // The page only seeds anonymous web cookies. Account cookies start a
        // passport SSO hop that never sees pan-scoped cookies and redirects forever.
        let mut page_headers = self.web_headers();
        page_headers.remove("Cookie");
        let page = self.http.send(CloudHttpRequest {
            method: "GET",
            url: page_url.clone(),
            headers: page_headers,
            body: None,
        })?;
        if page.status == 401 {
            return Err(QuarkError::AuthorizationExpired);
        }
        if !(200..300).contains(&page.status) {
            return Err(QuarkError::Provider(
                "百度分享验证页面暂时无法访问，请稍后重试".into(),
            ));
        }
        cookies.absorb_set_cookies(&page.set_cookies);
        self.session.cookie = cookies.header();
        let mut headers = self.web_headers();
        headers.insert("Referer".into(), page_url);
        headers.insert("X-Requested-With".into(), "XMLHttpRequest".into());
        headers.insert(
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        );
        let response = self.http.send(CloudHttpRequest {
            method: "POST",
            url: format!(
                "{PAN}/share/verify?channel=chunlei&clienttype=0&web=1&app_id=250528&surl={}",
                encoded(short)
            ),
            headers,
            body: Some(form(&[
                ("pwd", code.into()),
                ("vcode", "".into()),
                ("vcode_str", "".into()),
            ])),
        })?;
        let value = share_json(&response).map_err(|error| {
            // Do not turn login expiry or a captcha challenge into a password error.
            let errno = serde_json::from_str::<Value>(&response.body)
                .ok()
                .and_then(|value| value["errno"].as_i64());
            match error {
                QuarkError::Provider(_)
                    if response.status == 200 && matches!(errno, Some(-9 | -12)) =>
                {
                    QuarkError::InvalidInput("百度分享提取码校验失败，请检查提取码后重试".into())
                }
                error => error,
            }
        })?;
        // Only accept a token issued by this verification, never a stale account cookie.
        let mut verified = BaiduLoginCookies::default();
        verified.absorb_set_cookies(&response.set_cookies);
        let bdclnd = verified
            .0
            .get("BDCLND")
            .cloned()
            .or_else(|| {
                value["randsk"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
            .ok_or(QuarkError::InvalidResponse)?;
        if bdclnd.chars().any(|c| c.is_control() || c == ';') {
            return Err(QuarkError::InvalidResponse);
        }
        let seckey = percent_encoding::percent_decode_str(&bdclnd)
            .decode_utf8()
            .map_err(|_| QuarkError::InvalidResponse)?
            .into_owned();
        cookies.absorb_set_cookies(&response.set_cookies);
        cookies.insert("BDCLND", &bdclnd);
        self.session.cookie = cookies.header();
        Ok((bdclnd, seckey))
    }

    fn share_http(
        &self,
        method: &'static str,
        path: &str,
        body: Option<String>,
        bdclnd: &str,
        referer: Option<&str>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        let mut headers = self.pan_headers();
        if let Some(referer) = referer {
            headers.insert("Referer".into(), referer.into());
        }
        if !bdclnd.is_empty() {
            let mut cookies = BaiduLoginCookies::default();
            cookies.absorb_cookie_header(&self.session.cookie);
            cookies.insert("BDCLND", bdclnd);
            headers.insert("Cookie".into(), cookies.header());
        }
        if body.is_some() {
            headers.insert(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            );
        }
        self.http.send(CloudHttpRequest {
            method,
            url: format!("{PAN}{path}"),
            headers,
            body,
        })
    }
}
fn share_json(response: &CloudHttpResponse) -> Result<Value, QuarkError> {
    if response.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
    let code = value["errno"].as_i64().ok_or(QuarkError::InvalidResponse)?;
    if code == -20
        || matches!(
            value["err_no"].as_i64(),
            Some(6 | 16 | 257 | 120021 | 400031)
        )
        || ["captcha", "vcode", "vcode_str", "codeString"]
            .iter()
            .any(|key| {
                value
                    .get(key)
                    .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            })
    {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    if matches!(code, -6 | -7 | 111) {
        return Err(QuarkError::AuthorizationExpired);
    }
    if code == 9019 {
        return Err(QuarkError::CaptchaRequired);
    }
    if code != 0 || !(200..300).contains(&response.status) {
        return Err(QuarkError::Provider("百度分享无效或请求被拒绝".into()));
    }
    Ok(value)
}
fn numeric_field(value: &Value, field: &str) -> Result<String, QuarkError> {
    value
        .get(field)
        .and_then(string_or_number)
        .filter(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
        .ok_or(QuarkError::InvalidResponse)
}
impl ShareInspect for BaiduShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let mut client = self.client()?;
        let id = super::share::share_id(candidate, CloudProvider::Baidu)?;
        let short = id.strip_prefix('1').ok_or(QuarkError::InvalidResponse)?;
        let mut entries = vec![];
        let mut context = json!({"shorturl":short,"bdclnd":"","paths":{}});
        if let Some(code) = candidate
            .access_code
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let (bdclnd, seckey) = client.verify_share_access(short, code)?;
            context["bdclnd"] = json!(bdclnd);
            context["seckey"] = json!(seckey);
        }
        let mut queue =
            std::collections::VecDeque::from([("/".to_owned(), Vec::<String>::new(), true)]);
        let mut visited = std::collections::BTreeSet::new();
        while let Some((parent, directory, root)) = queue.pop_front() {
            if directory.len() > 128 || !visited.insert(parent.clone()) || visited.len() > 10000 {
                return Err(QuarkError::InvalidResponse);
            }
            for page in 1..=10000 {
                let query = form(&[
                    ("shorturl", short.into()),
                    ("root", if root { "1" } else { "0" }.into()),
                    ("dir", parent.clone()),
                    ("num", "100".into()),
                    ("page", page.to_string()),
                    ("order", "time".into()),
                ]);
                let response = client.share_http(
                    "GET",
                    &format!(
                        "/share/list?channel=chunlei&clienttype=0&app_id=250528&web=1&{query}"
                    ),
                    None,
                    context["bdclnd"].as_str().unwrap_or(""),
                    None,
                )?;
                let raw: Value = serde_json::from_str(&response.body)
                    .map_err(|_| QuarkError::InvalidResponse)?;
                if response.status == 200 && raw["errno"].as_i64() == Some(-9) {
                    return if context["bdclnd"].as_str().unwrap_or("").is_empty() {
                        Ok(ShareInspection::AccessCodeRequired)
                    } else {
                        Err(QuarkError::Provider(
                            "百度分享验证已通过，但文件列表访问失败，请重试或重新网页授权".into(),
                        ))
                    };
                }
                let value = share_json(&response)?;
                // `/share/list` is the currently supported endpoint.  Unlike
                // the retired `/share/wxlist`, it returns the file list and
                // share metadata at the top level rather than in `data`.
                // Retain the nested form for recorded responses and older
                // service versions.
                let data = value
                    .get("data")
                    .filter(|data| data.is_object())
                    .unwrap_or(&value);
                if root && page == 1 {
                    context["shareid"] = json!(numeric_field(data, "shareid")
                        .or_else(|_| numeric_field(data, "share_id"))?);
                    context["uk"] = json!(numeric_field(data, "uk")?);
                    if context["seckey"].as_str().unwrap_or("").is_empty() {
                        context["seckey"] = json!(data["seckey"].as_str().unwrap_or(""));
                    }
                }
                for cookie in &response.set_cookies {
                    if let Some(value) = cookie
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .strip_prefix("BDCLND=")
                    {
                        context["bdclnd"] = json!(value);
                    }
                }
                let files = data["list"].as_array().ok_or(QuarkError::InvalidResponse)?;
                for file in files {
                    let fid = numeric_field(file, "fs_id")?;
                    let name = required(file, "server_filename")?;
                    let path = required(file, "path")?;
                    validate_path(&path)?;
                    if baidu_is_directory(file) {
                        let mut relative = directory.clone();
                        relative.push(name);
                        queue.push_back((path, relative, false));
                    } else {
                        context["paths"][&fid] = json!(path);
                        entries.push(CloudShareEntry::new(
                            &id,
                            fid,
                            name,
                            directory.clone(),
                            file["size"]
                                .as_u64()
                                .or_else(|| file["size"].as_str()?.parse::<u64>().ok()),
                        ));
                    }
                }
                let more = data["has_more"]
                    .as_bool()
                    .or_else(|| data["has_more"].as_u64().map(|v| v != 0))
                    .unwrap_or(files.len() == 100);
                if !more {
                    break;
                }
                if files.is_empty() || page == 10000 {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        Ok(ShareInspection::Plan(CloudSharePlan::new(
            CloudProvider::Baidu,
            id,
            candidate.access_code.clone(),
            entries,
            context,
        )))
    }
}
impl ShareResolve for BaiduShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_baidu_share(self.credentials, self.http, entry)
    }
}
impl ShareTransfer for BaiduShareAdapter<'_> {
    fn transfer_capacity(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        self.client()?.storage_capacity()
    }

    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        self.client()?
            .stable_account_id()?
            .map(|id| super::share::stable_owner_fingerprint(CloudProvider::Baidu, &id))
            .transpose()
    }
    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        let owner = self.transfer_owner_fingerprint(entry)?;
        let client = self.client()?;
        let context = &entry.plan.provider_context;
        let token = client.web_token()?;
        let referer = format!("{PAN}/s/1{}", encoded(&required(context, "shorturl")?));
        let response = client.share_http(
            "POST",
            &format!(
                "/share/transfer?{}&shareid={}&from={}&ondup=newcopy&async=0",
                write_query(&token, ""),
                numeric_field(context, "shareid")?,
                numeric_field(context, "uk")?
            ),
            Some(form(&[
                (
                    "fsidlist",
                    format!(
                        "[{}]",
                        numeric_field(&json!({"id":entry.entry.entry_id}), "id")?
                    ),
                ),
                ("path", "/".into()),
            ])),
            context["bdclnd"].as_str().unwrap_or(""),
            Some(&referer),
        )?;
        let value = share_json(&response)?;
        let files = value
            .pointer("/extra/list")
            .and_then(Value::as_array)
            .filter(|v| v.len() == 1)
            .ok_or(QuarkError::InvalidResponse)?;
        let file = &files[0];
        if numeric_field(file, "from_fs_id")? != entry.entry.entry_id {
            return Err(QuarkError::InvalidResponse);
        }
        let id = numeric_field(file, "to_fs_id")?;
        if let Some(owner) = owner {
            store.record_transfer_with_owner(
                &entry.plan.source_for(&entry.entry.entry_id),
                &id,
                &owner,
            )?;
        } else {
            store.record_transfer(&entry.plan.source_for(&entry.entry.entry_id), &id)?;
        }
        let reference = encode_reference(BaiduFileReference {
            fsid: id,
            path: required(file, "to")?,
        })?;
        let download = client.download(&reference)?;
        Ok(super::commands::ResolvedCloudDownload {
            url: download.url,
            name: entry.entry.name.clone(),
            headers: download.headers.into_iter().collect(),
        })
    }
}
impl RecordedTransferResolver for BaiduShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        if entry.provider != CloudProvider::Baidu
            || entry.plan.provider != CloudProvider::Baidu
            || created_file_id.is_empty()
            || !created_file_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(QuarkError::InvalidResponse);
        }
        let client = self.client()?;
        // Transfers are created in root. Resolve the journalled fs_id from
        // that directory, never by the share's original name (ondup=newcopy
        // may have renamed it), and never perform another transfer here.
        let reference = client.transferred_file_reference(created_file_id)?;
        let download = client.download(&reference)?;
        Ok(ShareOperationResult {
            result: Ok(RecordedTransferDownload::Available(
                super::commands::ResolvedCloudDownload {
                    url: download.url,
                    name: entry.entry.name.clone(),
                    headers: download.headers.into_iter().collect(),
                },
            )),
            updated_context: None,
        })
    }
}
impl CloudShareAdapter for BaiduShareAdapter<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}
pub fn resolve_baidu_share(
    credentials: &dyn super::CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    let adapter = BaiduShareAdapter { credentials, http };
    if entry.provider != CloudProvider::Baidu {
        return Err(QuarkError::InvalidResponse);
    }
    // A share dlink can be returned even when the web download cannot serve
    // the selected file. Use the account-bound transfer + PCS path for every
    // Baidu share, and require a current login before a task is submitted.
    adapter.client()?;
    Ok(ShareResolution::TransferRequired(TransferRequest {
        source: entry.plan.source_for(&entry.entry.entry_id),
    }))
}
