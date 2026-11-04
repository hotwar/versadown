use super::{
    http::response_json,
    types::{download_source_missing_from_http_404, encoded, required},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};
use tauri::AppHandle;

use crate::{StoreError, TaskStore};

#[path = "quark/download_scheduler.rs"]
mod download_scheduler;

pub(crate) fn quark_initial_connection_count(
    store: &TaskStore,
    task_id: uuid::Uuid,
    budget: u8,
) -> Result<u8, StoreError> {
    download_scheduler::initial_connections_for_task(store, task_id, budget)
}

pub(crate) fn activate_quark_download_scheduler(app: AppHandle) {
    download_scheduler::coordinator().activate(app);
}

pub const QUARK_SHARE_PASS_CODE_CONSTRAINT: CloudSharePassCodeConstraint =
    CloudSharePassCodeConstraint {
        required: false,
        min_length: 4,
        max_length: 4,
        character_set: CloudSharePassCodeCharacterSet::AsciiAlphanumeric,
        hint: "提取码必须为 4 位字母或数字",
    };

const QUARK_SHARE_PROVIDER_CODE_PREFIX: &str = "__quark_share_provider_code__:";
const QUARK_SHARE_FAILURE_PREFIX: &str = "__quark_share_failure__:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuarkShareFailureStage {
    Transfer,
    TransferTask,
    PersonalDownload,
}

impl QuarkShareFailureStage {
    fn key(self) -> &'static str {
        match self {
            Self::Transfer => "transfer",
            Self::TransferTask => "transfer_task",
            Self::PersonalDownload => "personal_download",
        }
    }
}

/// Decode only our fixed, internal marker. Provider response text is never
/// copied into a durable task record or sent to the UI because it can contain
/// URLs or temporary credentials.
pub(crate) fn quark_share_failure_details(
    error: &QuarkError,
) -> Option<(QuarkShareFailureStage, Option<i64>)> {
    let QuarkError::Provider(message) = error else {
        return None;
    };
    let (stage, code) = message
        .strip_prefix(QUARK_SHARE_FAILURE_PREFIX)?
        .split_once(':')?;
    let stage = match stage {
        "transfer" => QuarkShareFailureStage::Transfer,
        "transfer_task" => QuarkShareFailureStage::TransferTask,
        "personal_download" => QuarkShareFailureStage::PersonalDownload,
        _ => return None,
    };
    let code = if code == "unknown" {
        None
    } else {
        let code = code.parse::<i64>().ok()?;
        (code >= 0).then_some(code)
    };
    Some((stage, code))
}

fn quark_share_provider_code(error: &QuarkError) -> Option<i64> {
    let QuarkError::Provider(message) = error else {
        return None;
    };
    message
        .strip_prefix(QUARK_SHARE_PROVIDER_CODE_PREFIX)?
        .parse::<i64>()
        .ok()
        .filter(|code| *code >= 0)
}

fn quark_share_failure(stage: QuarkShareFailureStage, error: QuarkError) -> QuarkError {
    let Some(code) = quark_share_provider_code(&error) else {
        return error;
    };
    QuarkError::Provider(format!(
        "{QUARK_SHARE_FAILURE_PREFIX}{}:{code}",
        stage.key()
    ))
}

fn is_quark_direct_download_unavailable(error: &QuarkError) -> bool {
    matches!(
        quark_share_provider_code(error),
        Some(41016) | Some(50001..)
    )
}

fn is_quark_direct_download_refresh_required(error: &QuarkError) -> bool {
    quark_share_provider_code(error) == Some(41016)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::share::*;
    use std::sync::Mutex;

    struct Http {
        replies: Mutex<std::collections::VecDeque<Value>>,
        requests: Mutex<Vec<CloudHttpRequest>>,
    }
    impl Http {
        fn new(replies: Vec<Value>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                requests: Mutex::new(Vec::new()),
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
            provider: CloudProvider::Quark,
            url: "https://pan.quark.cn/s/share123".into(),
            access_code: Some("a123".into()),
        }
    }
    fn listing() -> Vec<Value> {
        vec![
            json!({"status":200,"code":0,"data":{"stoken":"secret-token"}}),
            json!({"status":200,"code":0,"data":{"list":[{"fid":"11","file_name":"movie.mp4","dir":false,"size":123,"share_fid_token":"file-secret"}]},"metadata":{"_total":1}}),
        ]
    }
    fn capacity_entry(provider: CloudProvider) -> CloudShareResolvedEntry {
        let plan = CloudSharePlan::new(
            provider,
            "capacity-share".into(),
            None,
            vec![CloudShareEntry::new(
                "capacity-share",
                "entry-1".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            json!({}),
        );
        CloudShareResolvedEntry {
            provider,
            entry: plan.entries[0].clone(),
            plan,
        }
    }
    #[test]
    fn quark_share_transfer_capacity_uses_member_bytes() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save("quark", b"__uid=account-7; sid=current")
            .unwrap();
        let http = Http::new(vec![json!({
            "status": 200, "code": 0,
            "data": {"total_capacity": "1000", "use_capacity": 250}
        })]);
        let adapter = QuarkShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        assert_eq!(
            adapter
                .transfer_capacity(&capacity_entry(CloudProvider::Quark))
                .unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 1000,
                used_bytes: 250
            }
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].url.contains("/1/clouddrive/member?"));
        assert!(!requests[0].url.contains("sharepage/save"));
        drop(requests);

        let invalid_quark = Http::new(vec![json!({
            "status": 200, "code": 0,
            "data": {"total_capacity": 100, "use_capacity": 101}
        })]);
        assert!(QuarkShareAdapter {
            credentials: &credentials,
            http: &invalid_quark
        }
        .transfer_capacity(&capacity_entry(CloudProvider::Quark))
        .is_err());
    }
    #[test]
    fn quark_share_direct_requests_file_download_with_share_context() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let mut replies = listing();
        replies.push(json!({"status":200,"code":0,"data":[{"fid":"11","download_url":"https://download.quark.cn/movie?sign=secret"}]}));
        let http = Http::new(replies);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("plan required")
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };
        let ShareResolution::Direct(download) = adapter.resolve(&entry).unwrap() else {
            panic!("direct required")
        };
        assert_eq!(download.url, "https://download.quark.cn/movie?sign=secret");
        assert!(download
            .headers
            .contains(&("Cookie".into(), "sid=current".into())));
        assert!(download
            .headers
            .contains(&("Origin".into(), "https://pan.quark.cn".into())));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].method, "POST");
        let url = url::Url::parse(&requests[2].url).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(query.get("entry"), Some(&"ft".into()));
        assert_eq!(query.get("sys"), Some(&"win32".into()));
        assert_eq!(query.get("ve"), Some(&"3.20.0".into()));
        assert!(query.contains_key("__t"));
        assert_eq!(requests[2].headers["User-Agent"], QUARK_DESKTOP_USER_AGENT);
        assert_eq!(
            requests[2].headers["Accept"],
            "application/json, text/plain, */*"
        );
        assert_eq!(requests[2].headers["Content-Type"], "application/json");
        let token_body: Value = serde_json::from_str(requests[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(token_body["passcode"], "a123");
        let download_body: Value =
            serde_json::from_str(requests[2].body.as_ref().unwrap()).unwrap();
        assert_eq!(download_body["fids"], json!(["11"]));
        assert_eq!(download_body["pwd_id"], "share123");
        assert_eq!(download_body["stoken"], "secret-token");
        assert_eq!(download_body["fids_token"], json!(["file-secret"]));
    }

    #[test]
    fn quark_personal_download_marks_a_missing_file_response_explicitly() {
        struct StatusHttp;
        impl CloudHttpTransport for StatusHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Ok(CloudHttpResponse {
                    status: 404,
                    body: json!({}).to_string(),
                    set_cookies: vec![],
                })
            }
        }

        assert!(matches!(
            QuarkClient::new("sid=current".into(), &StatusHttp).download_url("missing-file"),
            Err(QuarkError::SourceFileMissing)
        ));
    }

    #[test]
    fn quark_share_download_rejection_falls_back_to_authorized_drive_transfer() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let mut replies = listing();
        replies.push(json!({
            "status": 200,
            "code": 50001,
            "message": "temporary share download rejection"
        }));
        let http = Http::new(replies);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!("plan required")
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };

        assert!(matches!(
            adapter.resolve(&entry),
            Ok(ShareResolution::TransferRequired(request)) if request.source == entry.plan.source_for("11")
        ));
    }

    #[test]
    fn quark_share_41016_refreshes_context_before_transferring() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save("quark", b"__uid=account-7; sid=current")
            .unwrap();
        let http = Http::new(vec![
            json!({"status":404,"code":41016}),
            json!({"code":0,"data":{"stoken":"fresh-share-token"}}),
            json!({"code":0,"data":{"list":[{"fid":"11","file_name":"movie.mp4","dir":false,"size":123,"share_fid_token":"fresh-file-token"}]},"metadata":{"_total":1}}),
            json!({"status":404,"code":41016}),
            json!({"code":0,"data":{"task_id":"save-task"}}),
            json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal-11"]}}}),
            json!({"code":0,"data":[{"fid":"personal-11","download_url":"https://download.quark.cn/personal?sign=secret"}]}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let plan = CloudSharePlan::new(
            CloudProvider::Quark,
            "share123".into(),
            Some("a123".into()),
            vec![CloudShareEntry::new(
                "share123",
                "11".into(),
                "movie.mp4".into(),
                vec![],
                Some(123),
            )],
            json!({"stoken":"stale-share-token","files":{"11":{"token":"stale-file-token","parent":"0"}}}),
        );
        let dir = tempfile::tempdir().unwrap();
        let store =
            CloudSharePlanStore::new(SystemCredentialStore::new(dir.path().into()).unwrap());
        store.upsert(&plan).unwrap();
        let source = plan.source_for("11");

        let resolved = resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();

        assert_eq!(
            resolved.url,
            "https://download.quark.cn/personal?sign=secret"
        );
        let refreshed = store.resolve_entry(&source).unwrap();
        assert_eq!(
            refreshed.plan.provider_context["stoken"],
            "fresh-share-token"
        );
        assert_eq!(
            refreshed.plan.provider_context["files"]["11"]["token"],
            "fresh-file-token"
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 7);
        let save_body: Value = serde_json::from_str(requests[4].body.as_ref().unwrap()).unwrap();
        assert_eq!(save_body["stoken"], "fresh-share-token");
        assert_eq!(save_body["fid_token_list"], json!(["fresh-file-token"]));
    }

    #[test]
    fn quark_share_download_cookie_rotation_persists_for_following_directory_entry() {
        struct CookieHttp(std::sync::Mutex<std::collections::VecDeque<CloudHttpResponse>>);
        impl CloudHttpTransport for CookieHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.0
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or(QuarkError::InvalidResponse)
            }
        }
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let http = CookieHttp(std::sync::Mutex::new(
            [
                CloudHttpResponse {
                    status: 200,
                    body: json!({"code":0,"data":[{"fid":"f1","download_url":"https://download.quark.cn/first"}]}).to_string(),
                    set_cookies: vec!["__puus=rotated; Path=/; Secure".into()],
                },
                CloudHttpResponse {
                    status: 200,
                    body: json!({"code":0,"data":[{"fid":"f2","download_url":"https://download.quark.cn/second"}]}).to_string(),
                    set_cookies: vec![],
                },
            ]
            .into(),
        ));
        let entries = vec![
            CloudShareEntry::new("share123", "f1".into(), "first.bin".into(), vec![], Some(1)),
            CloudShareEntry::new(
                "share123",
                "f2".into(),
                "second.bin".into(),
                vec![],
                Some(1),
            ),
        ];
        let plan = CloudSharePlan::new(
            CloudProvider::Quark,
            "share123".into(),
            None,
            entries.clone(),
            json!({"stoken":"share-token","files":{"f1":{"token":"first-token"},"f2":{"token":"second-token"}}}),
        );
        let first = CloudShareResolvedEntry {
            provider: CloudProvider::Quark,
            plan: plan.clone(),
            entry: entries[0].clone(),
        };
        let second = CloudShareResolvedEntry {
            provider: CloudProvider::Quark,
            plan,
            entry: entries[1].clone(),
        };

        assert!(matches!(
            resolve_quark_share(&credentials, &http, &first),
            Ok(ShareResolution::Direct(_))
        ));
        let Ok(ShareResolution::Direct(second_download)) =
            resolve_quark_share(&credentials, &http, &second)
        else {
            panic!("the second directory entry must resolve directly")
        };
        assert!(second_download
            .headers
            .iter()
            .any(|(name, value)| name == "Cookie" && value.contains("__puus=rotated")));
    }

    #[test]
    fn quark_transfer_download_uses_and_persists_the_sent_cookie() {
        struct SnapshotHttp(Http);
        impl CloudHttpTransport for SnapshotHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.0.send(request)
            }

            fn send_with_cookie_snapshot(
                &self,
                request: CloudHttpRequest,
            ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
                assert!(request.url.contains("/file/download?"));
                Ok((
                    self.send(request)?,
                    Some("__uid=account-7; sid=sent".into()),
                ))
            }
        }
        let credentials = MemoryCredentialStore::default();
        credentials
            .save("quark", b"__uid=account-7; sid=stored")
            .unwrap();
        let http = SnapshotHttp(Http::new(vec![
            json!({"code":0,"data":{"task_id":"fixture-save"}}),
            json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal-11"]}}}),
            json!({"code":0,"data":[{"fid":"personal-11","download_url":"https://download.quark.cn/personal?sign=fixture"}]}),
        ]));
        let plan = CloudSharePlan::new(
            CloudProvider::Quark,
            "share-fixture".into(),
            None,
            vec![CloudShareEntry::new(
                "share-fixture",
                "11".into(),
                "file.bin".into(),
                vec![],
                Some(1),
            )],
            json!({"stoken":"fixture-share-token","files":{"11":{"token":"fixture-file-token","parent":"0"}}}),
        );
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan: plan.clone(),
        };
        let dir = tempfile::tempdir().unwrap();
        let store =
            CloudSharePlanStore::new(SystemCredentialStore::new(dir.path().into()).unwrap());
        store.upsert(&plan).unwrap();
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();

        let download = adapter.transfer(&entry, &store).unwrap();

        let expected = BTreeMap::from([("__uid", "account-7"), ("sid", "sent")]);
        let sent_cookie = download
            .headers
            .iter()
            .find_map(|(name, value)| name.eq_ignore_ascii_case("Cookie").then_some(value))
            .unwrap();
        let stored_cookie = String::from_utf8(credentials.load("quark").unwrap().unwrap()).unwrap();
        for cookie in [sent_cookie.as_str(), stored_cookie.as_str()] {
            let pairs = cookie
                .split(';')
                .map(str::trim)
                .map(|pair| pair.split_once('=').unwrap())
                .collect::<BTreeMap<_, _>>();
            assert_eq!(pairs, expected);
        }
        assert_eq!(
            store
                .resolve_entry(&plan.source_for("11"))
                .unwrap()
                .plan
                .transfers[0]
                .created_file_id,
            "personal-11"
        );
        assert_eq!(http.0.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn quark_share_without_login_does_not_request_http() {
        let credentials = MemoryCredentialStore::default();
        let http = Http::new(vec![]);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        assert!(matches!(
            adapter.inspect(&candidate()),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
    }
    #[test]
    fn quark_share_access_code_required_and_wrong_code_are_distinct() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let http = Http::new(vec![
            json!({"status":400,"code":41003,"message":"提取码错误"}),
            json!({"status":400,"code":41003,"message":"提取码错误"}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let mut missing = candidate();
        missing.access_code = None;
        assert!(matches!(
            adapter.inspect(&missing),
            Ok(ShareInspection::AccessCodeRequired)
        ));
        assert!(matches!(
            adapter.inspect(&candidate()),
            Err(QuarkError::InvalidInput(_))
        ));
    }
    #[test]
    fn quark_share_requires_an_access_code_even_when_the_server_uses_http_400() {
        struct Http400;
        impl CloudHttpTransport for Http400 {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Ok(CloudHttpResponse {
                    status: 400,
                    body: json!({"code":41008,"message":"请输入提取码"}).to_string(),
                    set_cookies: vec![],
                })
            }
        }
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let http = Http400;
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let mut missing_code = candidate();
        missing_code.access_code = None;

        assert!(matches!(
            adapter.inspect(&missing_code),
            Ok(ShareInspection::AccessCodeRequired)
        ));
    }
    #[test]
    fn quark_share_preserves_cancelled_and_blocked_failures_for_the_parse_ui() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        for (reply, expected) in [
            (
                json!({"code":41012,"message":"好友已取消了分享"}),
                "夸克分享已被取消",
            ),
            (
                json!({"code":41031,"message":"分享者用户封禁链接查看受限"}),
                "夸克分享因账号封禁而无法访问",
            ),
        ] {
            let http = Http::new(vec![reply]);
            let adapter =
                provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();

            assert!(matches!(
                adapter.inspect(&candidate()),
                Err(QuarkError::Provider(message)) if message == expected
            ));
        }
    }
    #[test]
    fn quark_share_pages_nested_paths_and_empty_folder_stays_empty() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let http = Http::new(vec![
            listing()[0].clone(),
            json!({"code":0,"data":{"list":[{"fid":"folder","file_name":"season","dir":true}]},"metadata":{"_total":2}}),
            listing()[1].clone(),
            json!({"code":0,"data":{"list":[{"fid":"12","file_name":"episode.mp4","dir":false,"share_fid_token":"nested"}]},"metadata":{"_total":1}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[1].relative_directory, vec!["season"]);
        assert!(http.requests.lock().unwrap()[2].url.contains("_page=2"));
        let empty = Http::new(vec![
            listing()[0].clone(),
            json!({"code":0,"data":{"list":[]},"metadata":{"_total":0}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &empty).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        assert!(plan.entries.is_empty());
        assert_eq!(empty.requests.lock().unwrap().len(), 2);
    }
    #[test]
    fn quark_share_transfer_records_returned_personal_id_before_download_failure() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let mut replies = listing();
        replies.extend([
            json!({"code":0,"data":[]}),
            json!({"code":0,"data":{"uid":"account-7","nickname":"owner"}}),
            json!({"code":0,"data":{"task_id":"task1"}}),
            json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal99"]}}}),
            json!({"code":9,"message":"secret-server-detail"}),
        ]);
        let http = Http::new(replies);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let dir = tempfile::tempdir().unwrap();
        let store =
            CloudSharePlanStore::new(SystemCredentialStore::new(dir.path().into()).unwrap());
        store.upsert(&plan).unwrap();
        let source = plan.source_for("11");
        assert!(matches!(
            resolve_current_share_download(&store, adapter.as_ref(), &source),
            Err(QuarkError::CaptchaRequired)
        ));
        assert_eq!(
            store.resolve_entry(&source).unwrap().plan.transfers[0].created_file_id,
            "personal99"
        );
        let requests = http.requests.lock().unwrap();
        let body: Value = serde_json::from_str(requests[6].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["fids"], json!(["personal99"]));
        assert!(requests[4].url.contains("share/sharepage/save"));
    }
    #[test]
    fn quark_recorded_transfer_refreshes_personal_file_without_share_save() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let mut replies = listing();
        replies.push(json!({"code":0,"data":[{"fid":"personal99","download_url":"https://download.quark.cn/personal"}]}));
        let http = Http::new(replies);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };

        let refreshed = adapter
            .refresh_recorded_transfer(&entry, "personal99")
            .unwrap();

        let Ok(RecordedTransferDownload::Available(download)) = refreshed.result else {
            panic!("a personal direct link is required")
        };
        assert_eq!(download.url, "https://download.quark.cn/personal");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].url.contains("file/download"));
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("sharepage/save")));
    }
    #[test]
    fn quark_transfer_owner_uses_uid_not_rotating_session_cookie() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save("quark", b"__uid=account-7; sid=before")
            .unwrap();
        let http = Http::new(listing());
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
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
            .save("quark", b"__uid=account-7; sid=renewed")
            .unwrap();
        let after = adapter.transfer_owner_fingerprint(&entry).unwrap();

        assert_eq!(before, after);
    }
    #[test]
    fn quark_share_errors_never_become_transfer_requests() {
        for error in [
            json!({"code":9}),
            json!({"code":401}),
            json!({"code":500,"error":"https://secret.invalid/?token=secret"}),
            json!({"code":0,"data":{}}),
        ] {
            let credentials = MemoryCredentialStore::default();
            credentials.save("quark", b"sid=current").unwrap();
            let mut replies = listing();
            replies.push(error);
            let http = Http::new(replies);
            let adapter =
                provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
            let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
                panic!()
            };
            let entry = CloudShareResolvedEntry {
                provider: plan.provider,
                entry: plan.entries[0].clone(),
                plan,
            };
            assert!(adapter.resolve(&entry).is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 3);
        }
    }
    #[test]
    fn quark_share_conflicting_error_codes_never_trigger_save() {
        for error in [
            json!({"error_code":0,"code":401,"data":[]}),
            json!({"error_code":0,"code":500,"data":[]}),
            json!({"error_code":"invalid","code":0,"data":[]}),
            json!({"error_code":0,"code":"invalid","data":[]}),
        ] {
            let credentials = MemoryCredentialStore::default();
            credentials.save("quark", b"sid=current").unwrap();
            let mut replies = listing();
            replies.push(error);
            let http = Http::new(replies);
            let adapter =
                provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
            let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
                panic!()
            };
            let dir = tempfile::tempdir().unwrap();
            let store =
                CloudSharePlanStore::new(SystemCredentialStore::new(dir.path().into()).unwrap());
            store.upsert(&plan).unwrap();
            let source = plan.source_for("11");
            assert!(matches!(
                resolve_current_share_download(&store, adapter.as_ref(), &source),
                Err(QuarkError::InvalidResponse | QuarkError::Provider(_))
            ));
            assert!(store
                .resolve_entry(&source)
                .unwrap()
                .plan
                .transfers
                .is_empty());
            assert_eq!(http.requests.lock().unwrap().len(), 3);
        }
    }
    #[test]
    fn quark_share_direct_accepts_valid_http_url() {
        let credentials = MemoryCredentialStore::default();
        credentials.save("quark", b"sid=current").unwrap();
        let mut replies = listing();
        replies.push(json!({"code":0,"data":[{"fid":"11","download_url":"http://download.quark.cn/movie?sign=secret"}]}));
        let http = Http::new(replies);
        let adapter = provider_share_adapter(CloudProvider::Quark, &credentials, &http).unwrap();
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate()).unwrap() else {
            panic!()
        };
        let entry = CloudShareResolvedEntry {
            provider: plan.provider,
            entry: plan.entries[0].clone(),
            plan,
        };
        let ShareResolution::Direct(download) = adapter.resolve(&entry).unwrap() else {
            panic!()
        };
        assert_eq!(download.url, "http://download.quark.cn/movie?sign=secret");
    }
    #[test]
    fn quark_download_parser_accepts_http_and_rejects_unsafe_urls() {
        for url in [
            "http://download.quark.cn/file?sign=secret",
            "https://download.quark.cn/file",
            "https://download.quark.cn/path@part?email=user@example.com",
        ] {
            let body = json!({"data":[{"download_url":url}]}).to_string();
            assert_eq!(parse_quark_download_url(&body).unwrap(), url);
        }
        for url in [
            "ftp://download.quark.cn/file",
            "file:///tmp/file",
            "https://user:secret@download.quark.cn/file",
            "https://@download.quark.cn/file",
            "https:\n//@download.quark.cn/file",
            "http://user@download.quark.cn/file",
            "not a URL",
            "https://pan.quark.cn/s/share123",
        ] {
            let body = json!({"data":[{"download_url":url}]}).to_string();
            assert!(
                matches!(
                    parse_quark_download_url(&body),
                    Err(QuarkError::InvalidResponse)
                ),
                "unsafe URL accepted"
            );
        }
    }
}

/// Current public Quark Drive desktop transfer profile. Keep the API version
/// and User-Agent in lockstep so the server sees one coherent client profile.
pub const QUARK_DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) quark-cloud-drive/3.20.0 Chrome/112.0.5615.165 Electron/24.1.3.8 Safari/537.36 Channel/pckk_other_ch";
const QUARK_DESKTOP_VERSION: &str = "3.20.0";
const QUARK_DESKTOP_ORIGIN: &str = "https://pan.quark.cn";
const QUARK_DESKTOP_REFERER: &str = "https://pan.quark.cn/";
const QUARK_DESKTOP_ACCEPT: &str = "application/json, text/plain, */*";
const QUARK_DESKTOP_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9";
const QUARK_UPLOAD_PROFILE: CloudDriveUploadProfile = CloudDriveUploadProfile {
    object_host: "pds.quark.cn",
    object_store_user_agent: "aliyun-sdk-js/6.6.1",
};
pub(crate) const QUARK_RECYCLE_BIN_ID: &str = "__versa_down_quark_recycle_bin__";
const QUARK_RECYCLE_BIN_NAME: &str = "回收站";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct QuarkFile {
    pub id: String,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<u64>,
    pub updated_at: Option<String>,
}
pub fn parse_quark_files(body: &str) -> Result<Vec<QuarkFile>, QuarkError> {
    let v: Value = serde_json::from_str(body).map_err(|_| QuarkError::InvalidResponse)?;
    v.pointer("/data/list")
        .and_then(Value::as_array)
        .ok_or(QuarkError::InvalidResponse)?
        .iter()
        .map(|x| {
            Ok(QuarkFile {
                id: required(x, "fid")?,
                name: required(x, "file_name")?,
                is_folder: x["dir"]
                    .as_bool()
                    .unwrap_or(x["file_type"].as_i64() == Some(0)),
                size: x["size"].as_u64(),
                updated_at: x.get("updated_at").and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| value.is_number().then(|| value.to_string()))
                }),
            })
        })
        .collect()
}
pub fn parse_quark_download_url(body: &str) -> Result<String, QuarkError> {
    let v: Value = serde_json::from_str(body).map_err(|_| QuarkError::InvalidResponse)?;
    let url = v
        .pointer("/data/0/download_url")
        .and_then(Value::as_str)
        .ok_or(QuarkError::InvalidResponse)?;
    super::share::validate_direct_url(url)?;
    Ok(url.to_owned())
}

fn quark_api_url(path: &str) -> String {
    format!(
        "https://drive-pc.quark.cn/1/clouddrive/{path}{}pr=ucpro&fr=pc",
        if path.contains('?') { "&" } else { "?" }
    )
}

/// Builds the PC file-transfer request Quark uses when it issues a temporary
/// download URL. `entry=ft` is deliberately only sent for this operation: it
/// selects the desktop file-transfer flow rather than changing unrelated drive
/// requests. The timestamp prevents an intermediary from replaying a cached
/// temporary-link response.
fn quark_desktop_download_url() -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "https://drive-pc.quark.cn/1/clouddrive/file/download?entry=ft&fr=pc&pr=ucpro&uc_param_str=&sys=win32&ve={QUARK_DESKTOP_VERSION}&ut=&guid=&__dt=1000&__t={timestamp}"
    )
}

fn quark_request_headers(cookie: &str, desktop_file_transfer: bool) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([
        ("Cookie".into(), cookie.into()),
        ("Origin".into(), QUARK_DESKTOP_ORIGIN.into()),
        ("Referer".into(), QUARK_DESKTOP_REFERER.into()),
        ("User-Agent".into(), QUARK_DESKTOP_USER_AGENT.into()),
    ]);
    if desktop_file_transfer {
        headers.insert("Accept".into(), QUARK_DESKTOP_ACCEPT.into());
        headers.insert(
            "Accept-Language".into(),
            QUARK_DESKTOP_ACCEPT_LANGUAGE.into(),
        );
        headers.insert("Content-Type".into(), "application/json".into());
    }
    headers
}

/// Headers must be retained when aria2 follows Quark's signed CDN link. In
/// particular, Origin was previously sent to the API but silently dropped for
/// both personal-drive and share downloads.
pub(crate) fn quark_download_headers(
    cookie: &str,
    response_cookies: &[String],
) -> Vec<(String, String)> {
    let mut headers = super::share::download_headers(
        cookie,
        response_cookies,
        QUARK_DESKTOP_USER_AGENT,
        Some(QUARK_DESKTOP_REFERER),
    );
    headers.extend([
        ("Origin".into(), QUARK_DESKTOP_ORIGIN.into()),
        ("Accept".into(), QUARK_DESKTOP_ACCEPT.into()),
        (
            "Accept-Language".into(),
            QUARK_DESKTOP_ACCEPT_LANGUAGE.into(),
        ),
    ]);
    headers
}

pub struct QuarkClient<'a, T: CloudHttpTransport + ?Sized> {
    cookie: String,
    http: &'a T,
}

pub(crate) struct QuarkDownloadResponse {
    pub(crate) response: CloudHttpResponse,
    pub(crate) cookie: String,
}

impl<'a, T: CloudHttpTransport + ?Sized> QuarkClient<'a, T> {
    pub fn new(cookie: String, http: &'a T) -> Self {
        Self { cookie, http }
    }
    fn request_url(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.request_url_with_profile(method, url, body, false)
    }
    fn request_url_with_profile(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        desktop_file_transfer: bool,
    ) -> Result<CloudHttpResponse, QuarkError> {
        let r = self.http.send(CloudHttpRequest {
            method,
            url,
            headers: quark_request_headers(&self.cookie, desktop_file_transfer),
            body: body.map(|v| v.to_string()),
        })?;
        response_json(&r)?;
        Ok(r)
    }
    fn request(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.request_url(method, quark_api_url(path), body)
    }
    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let r = self.request_url(
            "GET",
            "https://pan.quark.cn/account/info?fr=pc&platform=pc".into(),
            None,
        )?;
        let v = response_json(&r)?;
        let d = &v["data"];
        let id = d
            .get("uid")
            .or_else(|| d.get("user_id"))
            .and_then(|x| {
                x.as_str()
                    .map(str::to_owned)
                    .or_else(|| x.as_u64().map(|n| n.to_string()))
            })
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.cookie.split(';').find_map(|part| {
                    let (name, value) = part.trim().split_once('=')?;
                    (name == "__uid" && !value.is_empty()).then(|| value.to_owned())
                })
            })
            .unwrap_or_else(|| format!("quark-{:x}", md5::compute(self.cookie.as_bytes())));
        Ok(CloudAccount {
            provider: CloudProvider::Quark,
            id,
            display_name: required(d, "nickname")?,
            auth_method: CloudAuthMethod::Web,
        })
    }
    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let value = response_json(&self.request(
            "GET",
            "member?fetch_subscribe=true&_ch=home&fetch_identity=true",
            None,
        )?)?;
        CloudStorageCapacity::from_json(
            &value["data"]["total_capacity"],
            &value["data"]["use_capacity"],
            false,
        )
    }
    pub fn list(&self, parent: &str) -> Result<CloudHttpResponse, QuarkError> {
        self.list_page(parent, 1)
    }
    fn list_page(&self, parent: &str, page: usize) -> Result<CloudHttpResponse, QuarkError> {
        self.request(
            "GET",
            &format!(
                "file/sort?pdir_fid={}&_page={page}&_size=200&_sort=file_name:asc",
                encoded(parent)
            ),
            None,
        )
    }
    pub fn directory(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        for page in 1..=10000 {
            let r = self.list_page(parent, page)?;
            let v = response_json(&r)?;
            let files = parse_quark_files(&r.body)?;
            let count = files.len();
            for f in files {
                items.push(CloudFile {
                    id: f.id,
                    parent_id: parent.into(),
                    name: f.name,
                    kind: if f.is_folder { "folder" } else { "file" }.into(),
                    size: f.size,
                    modified_at: f.updated_at,
                })
            }
            let total = v.pointer("/metadata/_total").and_then(Value::as_u64);
            if count < 200 || total.is_some_and(|t| items.len() as u64 >= t) {
                if parent == "0" {
                    items.push(CloudFile {
                        id: QUARK_RECYCLE_BIN_ID.into(),
                        parent_id: "0".into(),
                        name: QUARK_RECYCLE_BIN_NAME.into(),
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
    pub fn mkdir(&self, parent: &str, name: &str) -> Result<CloudHttpResponse, QuarkError> {
        self.request(
            "POST",
            "file",
            Some(json!({"pdir_fid":parent,"file_name":name,"dir_path":"","dir_init_lock":false})),
        )
    }
    pub fn rename(&self, id: &str, name: &str) -> Result<CloudHttpResponse, QuarkError> {
        self.request(
            "POST",
            "file/rename",
            Some(json!({"fid":id,"file_name":name})),
        )
    }
    pub fn move_files(
        &self,
        ids: &[String],
        parent: &str,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.request(
            "POST",
            "file/move",
            Some(json!({"filelist":ids,"to_pdir_fid":parent,"action_type":1})),
        )
    }
    pub fn copy_files(
        &self,
        ids: &[String],
        parent: &str,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.request(
            "POST",
            "file/copy",
            Some(json!({"filelist":ids,"to_pdir_fid":parent,"action_type":1})),
        )
    }
    pub fn trash(&self, ids: &[String]) -> Result<CloudHttpResponse, QuarkError> {
        self.request(
            "POST",
            "file/delete",
            Some(json!({"action_type":2,"filelist":ids,"exclude_fids":[]})),
        )
    }
    pub fn download_url(&self, id: &str) -> Result<CloudHttpResponse, QuarkError> {
        self.download_url_with_cookie(id)
            .map(|result| result.response)
    }

    pub(crate) fn download_url_with_cookie(
        &self,
        id: &str,
    ) -> Result<QuarkDownloadResponse, QuarkError> {
        let result = self
            .download_request(json!({"fids":[id]}))
            .map_err(download_source_missing_from_http_404)?;
        response_json(&result.response).map_err(download_source_missing_from_http_404)?;
        Ok(result)
    }

    fn download_request(&self, body: Value) -> Result<QuarkDownloadResponse, QuarkError> {
        let (response, sent_cookie) = self.http.send_with_cookie_snapshot(CloudHttpRequest {
            method: "POST",
            url: quark_desktop_download_url(),
            headers: quark_request_headers(&self.cookie, true),
            body: Some(body.to_string()),
        })?;
        // The HTTP jar may have rotated cookies during an earlier directory
        // request. Bind the CDN request to the cookie used to obtain this URL,
        // then incorporate any rotation returned by this download response.
        let cookie = super::common::cookie_for_download(
            sent_cookie.as_deref().unwrap_or(&self.cookie),
            &response.set_cookies,
        );
        Ok(QuarkDownloadResponse { response, cookie })
    }

    /// Uploads one regular local file using Quark's authenticated CloudDrive
    /// protocol. Object-store authorizations are generated for each transfer
    /// request and remain local to the call.
    pub fn upload_file<U: CloudUploadTransport>(
        &self,
        parent: &str,
        path: &Path,
        upload_http: &U,
    ) -> Result<CloudFile, QuarkError> {
        if parent == QUARK_RECYCLE_BIN_ID {
            return Err(QuarkError::InvalidInput(
                "回收站不能作为上传目标目录".into(),
            ));
        }
        super::upload::upload_cloud_drive_file(
            upload_http,
            parent,
            path,
            QUARK_UPLOAD_PROFILE,
            |payload| {
                self.request("POST", "file/upload/pre", Some(payload))
                    .and_then(|response| response_json(&response))
            },
            |payload| {
                self.request("POST", "file/update/hash", Some(payload))
                    .and_then(|response| response_json(&response))
            },
            |payload| {
                self.request("POST", "file/upload/auth", Some(payload))
                    .and_then(|response| response_json(&response))
            },
            |payload| {
                self.request("POST", "file/upload/finish", Some(payload))
                    .and_then(|response| response_json(&response))
            },
        )
    }

    pub fn list_recycle_bin(&self) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        for page in 1..=10_000 {
            let response = self.request(
                "GET",
                &format!("file/recycle/list?_page={page}&_size=200"),
                None,
            )?;
            let value = response_json(&response)?;
            let files = value
                .pointer("/data/list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = files.len();
            items.extend(
                files
                    .iter()
                    .map(quark_recycle_file)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let total = value.pointer("/metadata/_total").and_then(Value::as_u64);
            if count < 200 || total.is_some_and(|total| items.len() as u64 >= total) {
                return Ok(CloudDirectory {
                    id: QUARK_RECYCLE_BIN_ID.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn restore_recycle_entries(&self, record_ids: &[String]) -> Result<(), QuarkError> {
        validate_recycle_record_ids(record_ids)?;
        self.request(
            "POST",
            "file/recycle/restore",
            Some(json!({"select_mode":2,"record_list":record_ids})),
        )?;
        Ok(())
    }

    pub fn delete_recycle_entries(&self, record_ids: &[String]) -> Result<(), QuarkError> {
        validate_recycle_record_ids(record_ids)?;
        self.request(
            "POST",
            "file/recycle/remove",
            Some(json!({"select_mode":2,"record_list":record_ids})),
        )?;
        Ok(())
    }

    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
        }
        if options.generate_pass_code {
            return Err(QuarkError::InvalidInput(
                "夸克网盘需要设置自定义提取码".into(),
            ));
        }
        let pass_code = options
            .pass_code
            .as_deref()
            .filter(|code| !code.trim().is_empty());
        let created = response_json(&self.request(
            "POST",
            "share",
            Some(json!({
                "fid_list": file_ids,
                "title": options.name,
                "url_type": if pass_code.is_some() { 2 } else { 1 },
                "expired_type": if options.expires_in_days.is_some() { 2 } else { 1 },
                "expired_days": options.expires_in_days,
                "passcode": pass_code,
            })),
        )?)?;
        let task_id = required(
            created.get("data").ok_or(QuarkError::InvalidResponse)?,
            "task_id",
        )?;
        let mut share_id = None;
        for retry in 0..20 {
            let completed = response_json(&self.request(
                "GET",
                &format!("task?task_id={}&retry_index={retry}", encoded(&task_id)),
                None,
            )?)?;
            let data = completed.get("data").ok_or(QuarkError::InvalidResponse)?;
            if let Some(id) = data
                .get("share_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                share_id = Some(id.to_owned());
                break;
            }
            match data.get("status").and_then(Value::as_i64) {
                Some(0 | 1) | None => std::thread::sleep(std::time::Duration::from_millis(250)),
                _ => return Err(QuarkError::Provider("夸克分享创建任务失败".into())),
            }
        }
        let share_id = share_id
            .ok_or_else(|| QuarkError::Provider("夸克分享链接仍在生成，请稍后重试".into()))?;
        let details = response_json(&self.request(
            "POST",
            "share/password",
            Some(json!({"share_id": share_id})),
        )?)?;
        quark_share_from_value(
            details.get("data").unwrap_or(&details),
            Some(file_ids),
            pass_code,
        )
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let mut shares = Vec::new();
        for page in 1..=10_000 {
            let value = response_json(&self.request(
                "GET",
                &format!("share/mypage/detail?_page={page}&_size=200&_order_field=created_at&_order_type=desc&_fetch_total=1"),
                None,
            )?)?;
            let list = value
                .pointer("/data/list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = list.len();
            shares.extend(
                list.iter()
                    .map(|share| quark_share_from_value(share, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if count < 200 {
                return Ok(shares);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        if share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        for share_id in share_ids {
            self.request("POST", "share/delete", Some(json!({"share_id": share_id})))?;
        }
        Ok(())
    }
}

fn quark_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = value
        .get("share_id")
        .or_else(|| value.get("pwd_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(QuarkError::InvalidResponse)?;
    let source_file_ids = value
        .get("fid_list")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
        .unwrap_or_default();
    let folder = value.get("file_type").and_then(Value::as_i64) == Some(0)
        || value.get("dir").and_then(Value::as_bool) == Some(true);
    Ok(CloudShare {
        id: id.into(),
        url: value
            .get("share_url")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://pan.quark.cn/s/{id}")),
        name: value
            .get("title")
            .or_else(|| value.get("file_name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("分享文件")
            .into(),
        kind: if folder {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids,
        pass_code: value
            .get("passcode")
            .or_else(|| value.get("share_pwd"))
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at: value
            .get("expired_at")
            .or_else(|| value.get("expire_at"))
            .and_then(Value::as_str)
            .filter(|time| !time.is_empty())
            .map(str::to_owned),
    })
}

fn quark_recycle_file(file: &Value) -> Result<CloudFile, QuarkError> {
    Ok(CloudFile {
        id: required(file, "record_id")?,
        parent_id: QUARK_RECYCLE_BIN_ID.into(),
        name: required(file, "file_name")?,
        kind: if file
            .get("dir")
            .and_then(Value::as_bool)
            .unwrap_or(file.get("file_type").and_then(Value::as_i64) == Some(0))
        {
            "folder"
        } else {
            "file"
        }
        .into(),
        size: file.get("size").and_then(Value::as_u64),
        modified_at: file.get("updated_at").and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_u64().map(|value| value.to_string()))
        }),
    })
}

fn validate_recycle_record_ids(record_ids: &[String]) -> Result<(), QuarkError> {
    if record_ids.is_empty()
        || record_ids.iter().any(|record_id| {
            record_id.is_empty()
                || record_id.len() > 512
                || record_id.contains(['?', '#', '\r', '\n'])
                || record_id.chars().any(char::is_control)
        })
    {
        return Err(QuarkError::InvalidInput("回收站记录 ID 无效".into()));
    }
    Ok(())
}

use super::share::{
    CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudSharePlanStore,
    CloudShareResolvedEntry, CloudTransferCapacity, RecordedTransferDownload,
    RecordedTransferResolver, ShareInspect, ShareInspection, ShareOperationResult, ShareResolution,
    ShareResolve, ShareTransfer, TransferRequest,
};

pub struct QuarkShareAdapter<'a> {
    pub credentials: &'a dyn CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
impl QuarkShareAdapter<'_> {
    fn client(&self) -> Result<QuarkClient<'_, dyn CloudHttpTransport + '_>, QuarkError> {
        let bytes = self
            .credentials
            .load("quark")?
            .ok_or(QuarkError::NotAuthorized)?;
        let cookie = String::from_utf8(bytes).map_err(|_| QuarkError::Storage)?;
        if cookie.trim().is_empty() {
            return Err(QuarkError::NotAuthorized);
        }
        Ok(QuarkClient::new(cookie, self.http))
    }
}

impl<T: CloudHttpTransport + ?Sized> QuarkClient<'_, T> {
    fn stable_account_id(&self) -> Result<Option<String>, QuarkError> {
        if let Some(id) = self.cookie.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "__uid" && !value.is_empty()).then(|| value.to_owned())
        }) {
            return Ok(Some(id));
        }
        let response = self.request_url(
            "GET",
            "https://pan.quark.cn/account/info?fr=pc&platform=pc".into(),
            None,
        )?;
        let value = response_json(&response)?;
        Ok(value["data"]
            .get("uid")
            .or_else(|| value["data"].get("user_id"))
            .and_then(|id| {
                id.as_str()
                    .map(str::to_owned)
                    .or_else(|| id.as_u64().map(|id| id.to_string()))
            })
            .filter(|id| !id.is_empty()))
    }
    fn share_raw(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.http.send(CloudHttpRequest {
            method,
            url: quark_api_url(path),
            headers: quark_request_headers(&self.cookie, false),
            body: body.map(|value| value.to_string()),
        })
    }
    fn share_request(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        let response = self.share_raw(method, path, body)?;
        Self::validate_share_response(&response)?;
        Ok(response)
    }
    fn share_download_request(&self, body: Value) -> Result<QuarkDownloadResponse, QuarkError> {
        let result = self.download_request(body)?;
        Self::validate_share_response(&result.response)?;
        Ok(result)
    }
    fn validate_share_response(response: &CloudHttpResponse) -> Result<(), QuarkError> {
        let value: Value =
            serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
        let code = validate_share_codes(&value)?;
        response_json(response).map_err(|error| match error {
            QuarkError::Provider(_) => {
                QuarkError::Provider(format!("{QUARK_SHARE_PROVIDER_CODE_PREFIX}{code}"))
            }
            error => error,
        })?;
        Ok(())
    }
    fn share_download(
        &self,
        response: &QuarkDownloadResponse,
        name: &str,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        let url = parse_quark_download_url(&response.response.body)?;
        Ok(super::commands::ResolvedCloudDownload {
            url,
            name: name.into(),
            headers: quark_download_headers(&response.cookie, &[]),
        })
    }
}

pub(crate) fn persist_quark_download_cookies(
    credentials: &dyn CredentialStore,
    current_cookie: &str,
    response: &QuarkDownloadResponse,
) -> Result<(), QuarkError> {
    if response.cookie == current_cookie {
        return Ok(());
    }
    credentials.save("quark", response.cookie.as_bytes())
}

fn validate_share_codes(value: &Value) -> Result<i64, QuarkError> {
    let code = value["code"].as_i64().ok_or(QuarkError::InvalidResponse)?;
    // The generic decoder gives error_code precedence. Sharing must reject
    // malformed or contradictory codes before that precedence can hide errors.
    if let Some(other) = value.get("error_code") {
        if other.as_i64() != Some(code) {
            return Err(QuarkError::InvalidResponse);
        }
    }
    Ok(code)
}

impl ShareInspect for QuarkShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let client = self.client()?;
        let id = super::share::share_id(candidate, CloudProvider::Quark)?;
        let response = client.share_raw("POST", "share/sharepage/token", Some(json!({"pwd_id":id,"passcode":candidate.access_code.as_deref().unwrap_or(""),"support_visit_limit_private_share":true})))?;
        let raw: Value =
            serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
        let code = validate_share_codes(&raw)?;
        if code != 0
            && (code == 41008
                || matches!(
                    raw["message"].as_str(),
                    Some("提取码错误" | "提取码不正确" | "请输入提取码" | "提取码不能为空")
                ))
        {
            return if candidate.access_code.is_none() {
                Ok(ShareInspection::AccessCodeRequired)
            } else {
                Err(QuarkError::InvalidInput("夸克分享访问码错误".into()))
            };
        }
        let value = response_json(&response).map_err(|error| match error {
            QuarkError::Provider(_) if code == 41012 => {
                QuarkError::Provider("夸克分享已被取消".into())
            }
            QuarkError::Provider(_) if code == 41031 => {
                QuarkError::Provider("夸克分享因账号封禁而无法访问".into())
            }
            QuarkError::Provider(_) => QuarkError::Provider("夸克分享无效或已过期".into()),
            error => error,
        })?;
        let stoken = required(&value["data"], "stoken")?;
        let mut entries = vec![];
        let mut context = json!({"stoken":stoken,"files":{}});
        let mut queue = std::collections::VecDeque::from([("0".to_owned(), Vec::<String>::new())]);
        let mut visited = std::collections::BTreeSet::new();
        while let Some((parent, directory)) = queue.pop_front() {
            if directory.len() > 128 || !visited.insert(parent.clone()) || visited.len() > 10000 {
                return Err(QuarkError::InvalidResponse);
            }
            let mut count = 0;
            for page in 1..=10000 {
                let response = client.share_request("GET", &format!("share/sharepage/detail?pwd_id={}&stoken={}&pdir_fid={}&_page={page}&_size=200&_fetch_total=1&force=0", encoded(&id), encoded(&stoken), encoded(&parent)), None)?;
                let value = response_json(&response)?;
                let files = parse_quark_files(&response.body)?;
                let size = files.len();
                count += size;
                for (file, raw) in files.into_iter().zip(
                    value["data"]["list"]
                        .as_array()
                        .ok_or(QuarkError::InvalidResponse)?,
                ) {
                    if file.is_folder {
                        let mut relative = directory.clone();
                        relative.push(file.name);
                        queue.push_back((file.id, relative));
                    } else {
                        context["files"][&file.id] =
                            json!({"token":required(raw,"share_fid_token")?,"parent":parent});
                        entries.push(CloudShareEntry::new(
                            &id,
                            file.id,
                            file.name,
                            directory.clone(),
                            file.size,
                        ));
                    }
                }
                let more = value
                    .pointer("/metadata/_total")
                    .and_then(Value::as_u64)
                    .map(|total| count < total as usize)
                    .unwrap_or(size == 200);
                if !more {
                    break;
                }
                if size == 0 || page == 10000 {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        Ok(ShareInspection::Plan(CloudSharePlan::new(
            CloudProvider::Quark,
            id,
            candidate.access_code.clone(),
            entries,
            context,
        )))
    }
}
impl ShareResolve for QuarkShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_quark_share(self.credentials, self.http, entry)
    }

    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        if entry.plan.provider_context["quark_direct_download_unavailable"] == true {
            return Ok(ShareOperationResult {
                result: Ok(ShareResolution::TransferRequired(TransferRequest {
                    source: entry.plan.source_for(&entry.entry.entry_id),
                })),
                updated_context: None,
            });
        }
        match resolve_quark_share_direct(self.credentials, self.http, entry) {
            Ok(download) => Ok(ShareOperationResult {
                result: Ok(ShareResolution::Direct(download)),
                updated_context: None,
            }),
            Err(error) if is_quark_direct_download_refresh_required(&error) => {
                let refreshed = self.refresh_entry(entry)?;
                let mut context = refreshed.plan.provider_context.clone();
                match resolve_quark_share_direct(self.credentials, self.http, &refreshed) {
                    Ok(download) => Ok(ShareOperationResult {
                        result: Ok(ShareResolution::Direct(download)),
                        updated_context: Some(context),
                    }),
                    Err(error) if is_quark_direct_download_unavailable(&error) => {
                        context["quark_direct_download_unavailable"] = json!(true);
                        Ok(ShareOperationResult {
                            result: Ok(ShareResolution::TransferRequired(TransferRequest {
                                source: entry.plan.source_for(&entry.entry.entry_id),
                            })),
                            updated_context: Some(context),
                        })
                    }
                    Err(error) => Ok(ShareOperationResult {
                        result: Err(error),
                        updated_context: Some(context),
                    }),
                }
            }
            Err(error) if is_quark_direct_download_unavailable(&error) => {
                Ok(ShareOperationResult {
                    result: Ok(ShareResolution::TransferRequired(TransferRequest {
                        source: entry.plan.source_for(&entry.entry.entry_id),
                    })),
                    updated_context: None,
                })
            }
            Err(error) => Ok(ShareOperationResult {
                result: Err(error),
                updated_context: None,
            }),
        }
    }
}
impl ShareTransfer for QuarkShareAdapter<'_> {
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
            .map(|id| super::share::stable_owner_fingerprint(CloudProvider::Quark, &id))
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
        let file = &context["files"][&entry.entry.entry_id];
        let response = client
            .share_request("POST", "share/sharepage/save", Some(json!({"fid_list":[entry.entry.entry_id],"fid_token_list":[required(file,"token")?],"to_pdir_fid":"0","pwd_id":entry.plan.canonical_share_id,"stoken":required(context,"stoken")?,"pdir_fid":required(file,"parent")?,"scene":"link","pdir_save_all":false,"exclude_fids":[]})))
            .map_err(|error| quark_share_failure(QuarkShareFailureStage::Transfer, error))?;
        let value = response_json(&response)?;
        let task = required(&value["data"], "task_id")?;
        for retry in 0..20 {
            let response = client
                .share_request(
                    "GET",
                    &format!("task?task_id={}&retry_index={retry}", encoded(&task)),
                    None,
                )
                .map_err(|error| {
                    quark_share_failure(QuarkShareFailureStage::TransferTask, error)
                })?;
            let value = response_json(&response)?;
            match value["data"]["status"].as_i64() {
                Some(2) => {
                    let ids = value
                        .pointer("/data/save_as/save_as_top_fids")
                        .and_then(Value::as_array)
                        .filter(|ids| ids.len() == 1)
                        .ok_or(QuarkError::InvalidResponse)?;
                    let id = ids[0]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or(QuarkError::InvalidResponse)?;
                    if let Some(owner) = &owner {
                        store.record_transfer_with_owner(
                            &entry.plan.source_for(&entry.entry.entry_id),
                            id,
                            owner,
                        )?;
                    } else {
                        store.record_transfer(&entry.plan.source_for(&entry.entry.entry_id), id)?;
                    }
                    let response = client
                        .share_download_request(json!({"fids":[id]}))
                        .map_err(|error| {
                            quark_share_failure(QuarkShareFailureStage::PersonalDownload, error)
                        })?;
                    persist_quark_download_cookies(self.credentials, &client.cookie, &response)?;
                    return client.share_download(&response, &entry.entry.name);
                }
                Some(0 | 1) => std::thread::sleep(std::time::Duration::from_millis(250)),
                _ => {
                    return Err(QuarkError::Provider(format!(
                        "{QUARK_SHARE_FAILURE_PREFIX}{}:unknown",
                        QuarkShareFailureStage::TransferTask.key()
                    )))
                }
            }
        }
        Err(QuarkError::Provider(format!(
            "{QUARK_SHARE_FAILURE_PREFIX}{}:unknown",
            QuarkShareFailureStage::TransferTask.key()
        )))
    }

    fn transfer_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<ShareOperationResult<super::commands::ResolvedCloudDownload>, QuarkError> {
        let result = self.transfer(entry, store);
        let should_refresh = matches!(
            result.as_ref().err().and_then(quark_share_failure_details),
            Some((QuarkShareFailureStage::Transfer, Some(41016)))
        );
        if !should_refresh {
            return Ok(ShareOperationResult {
                result,
                updated_context: None,
            });
        }
        let refreshed = match self.refresh_entry(entry) {
            Ok(refreshed) => refreshed,
            Err(_) => {
                return Ok(ShareOperationResult {
                    result,
                    updated_context: None,
                })
            }
        };
        let mut context = refreshed.plan.provider_context.clone();
        context["quark_direct_download_unavailable"] = json!(true);
        Ok(ShareOperationResult {
            result: self.transfer(&refreshed, store),
            updated_context: Some(context),
        })
    }
}
impl RecordedTransferResolver for QuarkShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        if entry.provider != CloudProvider::Quark
            || entry.plan.provider != CloudProvider::Quark
            || created_file_id.is_empty()
        {
            return Err(QuarkError::InvalidResponse);
        }
        let client = self.client()?;
        let response = client
            .share_download_request(json!({"fids":[created_file_id]}))
            .map_err(|error| {
                quark_share_failure(QuarkShareFailureStage::PersonalDownload, error)
            })?;
        Ok(ShareOperationResult {
            result: persist_quark_download_cookies(self.credentials, &client.cookie, &response)
                .and_then(|_| client.share_download(&response, &entry.entry.name))
                .map(RecordedTransferDownload::Available),
            updated_context: None,
        })
    }
}
impl CloudShareAdapter for QuarkShareAdapter<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}

impl QuarkShareAdapter<'_> {
    fn refresh_entry(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<CloudShareResolvedEntry, QuarkError> {
        let candidate = crate::parser::CloudShareCandidate {
            provider: CloudProvider::Quark,
            url: format!("https://pan.quark.cn/s/{}", entry.plan.canonical_share_id),
            access_code: entry.plan.access_code.clone(),
        };
        let ShareInspection::Plan(plan) = self.inspect(&candidate)? else {
            return Err(QuarkError::InvalidInput(
                "夸克分享现在需要提取码，请重新解析分享链接".into(),
            ));
        };
        if plan.canonical_share_id != entry.plan.canonical_share_id {
            return Err(QuarkError::InvalidResponse);
        }
        let refreshed_entry = plan
            .entries
            .iter()
            .find(|candidate| candidate.entry_id == entry.entry.entry_id)
            .cloned()
            .ok_or_else(|| {
                QuarkError::InvalidInput(
                    "夸克分享中的文件已变更或不可下载，请重新解析分享链接".into(),
                )
            })?;
        let mut refreshed = entry.clone();
        refreshed.plan.provider_context = plan.provider_context;
        refreshed.entry = refreshed_entry;
        Ok(refreshed)
    }
}

pub fn resolve_quark_share(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    match resolve_quark_share_direct(credentials, http, entry) {
        Ok(download) => Ok(ShareResolution::Direct(download)),
        Err(error) if is_quark_direct_download_unavailable(&error) => {
            Ok(ShareResolution::TransferRequired(TransferRequest {
                source: entry.plan.source_for(&entry.entry.entry_id),
            }))
        }
        Err(error) => Err(error),
    }
}

fn resolve_quark_share_direct(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
    let adapter = QuarkShareAdapter { credentials, http };
    let client = adapter.client()?;
    if entry.provider != CloudProvider::Quark {
        return Err(QuarkError::InvalidResponse);
    }
    let context = &entry.plan.provider_context;
    let response = client.share_download_request(
        json!({"pwd_id":entry.plan.canonical_share_id,"stoken":required(context,"stoken")?,"fids":[entry.entry.entry_id],"fids_token":[required(&context["files"][&entry.entry.entry_id],"token")?]}),
    )?;
    persist_quark_download_cookies(credentials, &client.cookie, &response)?;
    let value = response_json(&response.response)?;
    let files = value["data"]
        .as_array()
        .ok_or(QuarkError::InvalidResponse)?;
    // This is the successful download response for one already-selected file,
    // never a directory listing or an authorization/provider error.
    if files.is_empty() {
        return Err(QuarkError::Provider(format!(
            "{QUARK_SHARE_PROVIDER_CODE_PREFIX}50001"
        )));
    }
    client.share_download(&response, &entry.entry.name)
}
