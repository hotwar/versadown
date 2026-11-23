use super::{
    http::response_json,
    types::{download_source_missing_from_http_404, encoded, required},
    *,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};
const AUTH: &str = "https://user.mypikpak.com";
const DRIVE: &str = "https://api-drive.mypikpak.com";

/// Only official provider pages may receive an operation challenge token.
pub fn allows_pikpak_operation_verification_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.host_str(),
                Some(
                    "mypikpak.com"
                        | "www.mypikpak.com"
                        | "user.mypikpak.com"
                        | "captcha.mypikpak.com"
                        | "xluser-ssl.xunlei.com"
                        | "captcha.xunlei.com"
                )
            )
    })
}

fn pikpak_operation_verification_error(method: &str, raw_url: &str) -> QuarkError {
    let parsed = url::Url::parse(raw_url).ok();
    let path = parsed.as_ref().map(|url| url.path()).unwrap_or_default();
    let stage = match (method, path) {
        ("GET", "/drive/v1/files") => "读取文件列表",
        ("GET", path) if path.starts_with("/drive/v1/files/") => "读取文件信息",
        ("POST", "/drive/v1/files:batchDelete") => "永久删除",
        ("POST", "/drive/v1/files:batchUntrash") => "恢复回收站项目",
        (_, path) if path.starts_with("/drive/v1/share") => "解析分享",
        _ => "当前操作",
    };
    QuarkError::PikpakOperationVerificationRequired(stage)
}

fn pikpak_captcha_action(method: &str, raw_url: &str) -> Result<String, QuarkError> {
    let url = url::Url::parse(raw_url).map_err(|_| QuarkError::InvalidResponse)?;
    let path = if matches!(method, "GET" | "PATCH")
        && url
            .path()
            .strip_prefix("/drive/v1/files/")
            .is_some_and(|id| !id.is_empty() && !id.contains(['/', ':']))
    {
        "/drive/v1/files/"
    } else {
        url.path()
    };
    Ok(format!("{method}:{path}"))
}
pub(crate) const PIKPAK_WEB_TOKENS_JS: &str = r#"(() => {
 if (location.protocol !== 'https:' || !(location.hostname === 'mypikpak.com' || location.hostname.endsWith('.mypikpak.com'))) return null;
 const out = {}; const seen = new Set();
 function inspect(v, depth) {
  if (!v || depth > 6) return;
  if (typeof v === 'string') { try { inspect(JSON.parse(v),depth+1); } catch (_) {} return; }
  if (typeof v !== 'object' || seen.has(v)) return; seen.add(v);
  for (const [k,x] of Object.entries(v)) {
   if (['expires_at','expiresAt'].includes(k) && Number.isFinite(Number(x))) out.expires_at = Number(x);
   if (['access_token','accessToken'].includes(k) && typeof x === 'string') out.access_token=x;
   else if (['refresh_token','refreshToken'].includes(k) && typeof x === 'string') out.refresh_token=x;
   else if (['device_id','deviceId','deviceid'].includes(k) && typeof x === 'string') out.device_id=x;
   else if (['captcha_token','captchaToken'].includes(k) && typeof x === 'string') out.captcha_token=x;
   else if (!/password/i.test(k)) inspect(x,depth+1);
  }
 }
 for (let i=0;i<localStorage.length;i++) {const k=localStorage.key(i); inspect({[k]:localStorage.getItem(k)},0);}
 for (let i=0;i<localStorage.length;i++) {const k=localStorage.key(i); if(k.startsWith('credentials_')) {try {const v=JSON.parse(localStorage.getItem(k)); if(v.access_token && v.refresh_token) {inspect(v,0);out.client_id=k.slice(12);break;}} catch (_) {}}}
 return out.access_token && out.refresh_token ? out : null;
})()"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::share::{test_support::*, *};
    fn credentials() -> MemoryCredentialStore {
        let store = MemoryCredentialStore::default();
        store
            .save(
                "pikpak",
                &serde_json::to_vec(&PikPakSession {
                    access_token: "current-user".into(),
                    refresh_token: "refresh".into(),
                    expires_at: u64::MAX,
                    device_id: "0123456789abcdef0123456789abcdef".into(),
                    captcha_token: "personal-captcha".into(),
                    client_id: Some("YUMx5nI8ZU8Ap8pm".into()),
                    user_id: "u1".into(),
                    captcha_expires_at: u64::MAX,
                    captcha_action: None,
                    verification_url: None,
                    operation_verifications: BTreeMap::new(),
                    auth_method: CloudAuthMethod::Web,
                })
                .unwrap(),
            )
            .unwrap();
        store
    }
    fn cap() -> Value {
        json!({"captcha_token":"share-captcha","expires_in":300,"url":""})
    }
    #[test]
    fn pikpak_password_captcha_uses_origin_device_without_changing_session_identity() {
        for device in [
            "wdi10.0123456789abcdef0123456789abcdefzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "wdi10.0123456789abcdef0123456789abcdefaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "div101.0123456789abcdef0123456789abcdefaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0123456789abcdef0123456789abcdef",
        ] {
            let store = credentials();
            let mut session: PikPakSession =
                serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
            session.client_id = Some("YNxT9w7GMdWvEOKa".into());
            session.device_id = device.into();
            session.auth_method = CloudAuthMethod::Password;
            store
                .save("pikpak", &serde_json::to_vec(&session).unwrap())
                .unwrap();
            let http = Http::new(vec![cap(), json!({"task_id":"deleted"})]);
            PikPakClient::new(&http, &store)
                .request(
                    "POST",
                    format!("{DRIVE}/drive/v1/files:batchDelete"),
                    Some(json!({"ids":["fixture"]})),
                )
                .unwrap();
            let requests = http.requests.lock().unwrap();
            let body: Value = serde_json::from_str(requests[0].body.as_ref().unwrap()).unwrap();
            assert_eq!(body["device_id"], "0123456789abcdef0123456789abcdef");
            assert_eq!(body["client_id"], "YNxT9w7GMdWvEOKa");
            assert_eq!(requests[0].headers["X-Device-Id"], device);
            assert_eq!(requests[1].headers["X-Device-Id"], device);
            let saved: PikPakSession =
                serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
            assert_eq!(saved.device_id, device);
            assert_eq!(saved.access_token, session.access_token);
        }
    }
    #[test]
    fn pikpak_account_restore_does_not_request_drive_captcha_for_auth_endpoint() {
        let store = credentials();
        let mut session: PikPakSession =
            serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
        session.captcha_action = Some("GET:/drive/v1/files".into());
        store
            .save("pikpak", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        let http = Http::new(vec![json!({"sub":"u1","name":"fixture"})]);
        let account = ProviderRegistry::global()
            .driver(CloudProvider::Pikpak, &store, &http)
            .account()
            .unwrap();
        assert_eq!(account.id, "u1");
        assert_eq!(http.requests.lock().unwrap().len(), 1);
        assert!(http.requests.lock().unwrap()[0]
            .url
            .ends_with("/v1/user/me"));
    }
    #[test]
    fn pikpak_storage_capacity_uses_drive_about_quota() {
        let store = credentials();
        let http = Http::new(vec![json!({
            "quota": {"limit": "1099511627776", "usage": "274877906944"}
        })]);
        let capacity = ProviderRegistry::global()
            .driver(CloudProvider::Pikpak, &store, &http)
            .storage_capacity()
            .unwrap();
        assert_eq!(
            capacity,
            CloudStorageCapacity::Limited {
                total_bytes: 1_099_511_627_776,
                used_bytes: 274_877_906_944,
            }
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.ends_with("/drive/v1/about"));
    }
    #[test]
    fn pikpak_share_pass_code_update_patches_existing_share() {
        let store = credentials();
        let http = Http::new(vec![
            json!({}),
            json!({"data":[{"share_id":"existing-share", "pass_code":"a7B2"}]}),
        ]);
        ProviderRegistry::global()
            .driver(CloudProvider::Pikpak, &store, &http)
            .update_share_pass_code("existing-share", "a7B2")
            .unwrap();
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "PATCH");
        assert!(requests[0].url.ends_with("/drive/v1/share"));
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].body.as_deref().unwrap()).unwrap(),
            json!({"share_id":"existing-share", "share_to":"encryptedlink", "pass_code_option":"REQUIRED", "custom_pass_code":"a7B2"})
        );
    }
    #[test]
    fn pikpak_share_pass_code_rejects_invalid_input_without_http() {
        let store = credentials();
        let http = Http::new(vec![]);
        let client = PikPakClient::new(&http, &store);
        for code in ["", "123", "12345", "a 12", "测试代码", "a!23"] {
            assert!(matches!(
                client.update_share_pass_code("s", code),
                Err(QuarkError::InvalidInput(_))
            ));
        }
        assert!(client.update_share_pass_code("", "a7B2").is_err());
        assert!(http.requests.lock().unwrap().is_empty());
    }
    #[test]
    fn pikpak_share_update_does_not_report_success_when_provider_ignores_code() {
        let store = credentials();
        let http = Http::new(vec![
            json!({}),
            json!({"data":[{"share_id":"s", "pass_code":"old1"}]}),
        ]);
        assert!(PikPakClient::new(&http, &store)
            .update_share_pass_code("s", "a7B2")
            .is_err());
    }
    #[test]
    fn pikpak_creation_does_not_silently_replace_a_custom_code() {
        let store = credentials();
        let http = Http::new(vec![]);
        let options = CloudShareOptions {
            name: "test".into(),
            pass_code: Some("a7B2".into()),
            generate_pass_code: false,
            expires_in_days: Some(1),
        };
        assert!(PikPakClient::new(&http, &store)
            .create_share(&["f".into()], &options)
            .is_err());
        assert!(http.requests.lock().unwrap().is_empty());
    }
    #[test]
    fn pikpak_share_list_reads_actual_provider_metadata() {
        let value = json!({"share_id":"share-1", "title":"照片合集", "params":{"name":"照片合集"},
            "file_id":"folder-1", "file_kind":"drive#folder", "file_num":"1", "file_size":"123",
            "create_time":"2026-09-18T10:00:00Z", "expiration_at":"2026-09-19T10:00:00Z"});
        let share = pikpak_share_from_value(&value, None, None).unwrap();
        assert_eq!(share.name, "照片合集");
        assert_eq!(share.kind, CloudFileKind::Folder);
        assert_eq!(share.source_file_ids, vec!["folder-1"]);
        assert_eq!(share.created_at.as_deref(), Some("2026-09-18T10:00:00Z"));
        assert_eq!(share.expires_at.as_deref(), Some("2026-09-19T10:00:00Z"));
    }
    #[test]
    fn pikpak_share_name_falls_back_past_empty_fields() {
        let share = pikpak_share_from_value(&json!({"share_id":"s", "share_name":"", "name":"", "title":"", "params":{"name":"报告.pdf"}}), None, None).unwrap();
        assert_eq!(share.name, "报告.pdf");
        assert_eq!(share.kind, CloudFileKind::File);
    }
    #[test]
    fn pikpak_personal_shares_use_the_dedicated_share_management_protocol() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"share_id":"created","share_url":"https://mypikpak.com/s/created","pass_code":"p4ss"}),
            json!({"data":[{"share_id":"created","share_url":"https://mypikpak.com/s/created"}],"next_page_token":""}),
            json!({}),
        ]);
        let mut driver = ProviderRegistry::global().driver(CloudProvider::Pikpak, &store, &http);
        let options = CloudShareOptions {
            name: "报告.pdf".into(),
            pass_code: None,
            generate_pass_code: true,
            expires_in_days: Some(7),
        };
        let share = driver.create_share(&["file-1".into()], &options).unwrap();
        assert_eq!(share.pass_code.as_deref(), Some("p4ss"));
        assert_eq!(driver.list_shares().unwrap().len(), 1);
        driver.cancel_shares(&["created".into()]).unwrap();

        let requests = http.requests.lock().unwrap();
        assert!(requests[0].url.ends_with("/drive/v1/share"));
        let created: Value = serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(created["share_to"], "encryptedlink");
        assert_eq!(created["pass_code_option"], "REQUIRED");
        assert_eq!(created["expiration_days"], 7);
        assert!(requests[1].url.contains("/drive/v1/share/list?"));
        assert!(requests[2].url.ends_with("/drive/v1/share:batchDelete"));
        assert_eq!(
            serde_json::from_str::<Value>(requests[2].body.as_deref().unwrap()).unwrap()["ids"],
            json!(["created"])
        );
    }
    #[test]
    fn pikpak_share_rotation_does_not_relabel_share_ticket_as_recycle_permission() {
        struct ScopedTickets;
        impl CloudHttpTransport for ScopedTickets {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                let url = url::Url::parse(&request.url).unwrap();
                let body: Value = request
                    .body
                    .as_deref()
                    .map(|body| serde_json::from_str(body).unwrap())
                    .unwrap_or(Value::Null);
                let token = request.headers.get("X-Captcha-Token").map(String::as_str);
                let response = match (request.method, url.path()) {
                    ("POST", "/v1/shield/captcha/init") => {
                        let ticket = match body["action"].as_str().unwrap() {
                            "GET:/drive/v1/share" => "share-only",
                            "GET:/drive/v1/files" => "list-only",
                            "POST:/drive/v1/files:batchDelete" => "delete-only",
                            _ => panic!("unexpected verification action"),
                        };
                        json!({"captcha_token":ticket,"expires_in":300})
                    }
                    ("GET", "/drive/v1/files") if token == Some("list-only") => {
                        json!({"files":[{"id":"fixture","name":"test","kind":"drive#folder"}],"next_page_token":""})
                    }
                    ("POST", "/drive/v1/files:batchDelete") if token == Some("delete-only") => {
                        assert_eq!(body["ids"], json!(["fixture"]));
                        json!({"task_id":"deletion"})
                    }
                    _ => {
                        return Ok(CloudHttpResponse {
                            status: 400,
                            body: json!({"error_code":3,"error":"invalid_argument"}).to_string(),
                            set_cookies: vec![],
                        })
                    }
                };
                Ok(CloudHttpResponse {
                    status: 200,
                    body: response.to_string(),
                    set_cookies: vec![],
                })
            }
        }
        let store = credentials();
        let mut saved: PikPakSession =
            serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
        saved.captcha_action = Some("GET:/drive/v1/files".into());
        store
            .save("pikpak", &serde_json::to_vec(&saved).unwrap())
            .unwrap();
        let http = ScopedTickets;
        let adapter = PikPakShareAdapter {
            credentials: &store,
            http: &http,
        };
        adapter
            .session()
            .unwrap()
            .refresh_action_captcha("GET", &format!("{SHARE_DRIVE}/drive/v1/share"))
            .unwrap();
        ProviderRegistry::global()
            .driver(CloudProvider::Pikpak, &store, &http)
            .delete(&["fixture".into()])
            .unwrap();
    }
    fn pass() -> Value {
        json!({"share_status":"OK","pass_code_token":"pass-secret"})
    }
    fn listing() -> Value {
        json!({"share_status":"OK","files":[{"id":"f1","kind":"drive#file","name":"verify.mp4","size":"7"}],"next_page_token":""})
    }
    #[test]
    fn pikpak_share_reuses_the_inspected_pass_token_with_the_current_identity() {
        let store = credentials();
        let http = Http::new(vec![
            pass(),
            listing(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":"http://cdn.example/file"}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Pikpak)).unwrap()
        else {
            panic!()
        };
        assert!(plan.provider_context.to_string().contains("pass-secret"));
        let ShareResolution::Direct(direct) = adapter.resolve(&entry(plan)).unwrap() else {
            panic!()
        };
        assert_eq!(direct.url, "http://cdn.example/file");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0]
            .url
            .starts_with("https://api-drive.mypikpak.com/"));
        assert!(requests[0].url.contains("/drive/v1/share?"));
        assert_eq!(requests[0].headers["X-Captcha-Token"], "personal-captcha");
        assert!(requests[2].url.contains("/drive/v1/share/file_info?"));
        assert!(requests[2].url.contains("pass_code_token=pass-secret"));
        assert_eq!(requests[2].headers["Authorization"], "Bearer current-user");
        assert_eq!(requests[2].headers["X-Captcha-Token"], "personal-captcha");
    }

    #[test]
    fn pikpak_share_cdn_link_does_not_leak_api_authorization() {
        let store = credentials();
        let http = Http::new(vec![
            pass(),
            listing(),
            json!({
                "share_status":"OK",
                "file_info":{
                    "id":"f1",
                    "kind":"drive#file",
                    "web_content_link":"https://cdn.example/file"
                }
            }),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Pikpak)).unwrap()
        else {
            panic!()
        };

        let ShareResolution::Direct(direct) = adapter.resolve(&entry(plan)).unwrap() else {
            panic!()
        };

        assert!(direct
            .headers
            .contains(&("User-Agent".into(), SHARE_WEB_UA.into())));
        assert!(direct
            .headers
            .contains(&("Referer".into(), "https://mypikpak.com/".into())));
        for secret in [
            "Authorization",
            "X-Client-Id",
            "X-Device-Id",
            "X-Captcha-Token",
        ] {
            assert!(
                !direct
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(secret)),
                "CDN request must not receive {secret}"
            );
        }
    }

    #[test]
    fn pikpak_share_refreshes_an_expired_inspected_pass_token_once() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"share_status":"PASS_CODE_ERROR"}),
            pass(),
            json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":"https://cdn.example/file"}}),
        ]);
        let plan = CloudSharePlan::new(
            CloudProvider::Pikpak,
            "share123".into(),
            Some("a123".into()),
            vec![CloudShareEntry::new(
                "share123",
                "f1".into(),
                "file.txt".into(),
                vec![],
                Some(7),
            )],
            json!({"pass_code_token":"expired-pass"}),
        );
        let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();

        let resolved = adapter.resolve_with_context(&entry(plan)).unwrap();

        assert!(matches!(resolved.result, Ok(ShareResolution::Direct(_))));
        assert_eq!(
            resolved.updated_context.unwrap()["pass_code_token"],
            "pass-secret"
        );
        assert_eq!(http.requests.lock().unwrap().len(), 3);
    }
    #[test]
    fn pikpak_share_accepts_password_and_web_token_sessions() {
        for auth_method in [CloudAuthMethod::Password, CloudAuthMethod::Web] {
            let store = credentials();
            let mut session: PikPakSession =
                serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
            session.auth_method = auth_method;
            store
                .save("pikpak", &serde_json::to_vec(&session).unwrap())
                .unwrap();
            let http = Http::new(vec![pass(), listing()]);

            let ShareInspection::Plan(plan) =
                provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                    .unwrap()
                    .inspect(&candidate(CloudProvider::Pikpak))
                    .unwrap()
            else {
                panic!("a valid token session should inspect a public share")
            };

            assert_eq!(plan.entries[0].entry_id, "f1");
            assert_eq!(
                http.requests.lock().unwrap()[0].headers["Authorization"],
                "Bearer current-user"
            );
        }
    }

    #[test]
    fn pikpak_share_refreshes_action_captcha_and_retries_once() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"error_code":9,"error":"captcha_required"}),
            json!({"captcha_token":"share-captcha","expires_in":300,"url":""}),
            pass(),
            listing(),
        ]);

        let ShareInspection::Plan(plan) =
            provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Pikpak))
                .unwrap()
        else {
            panic!()
        };

        assert_eq!(plan.entries.len(), 1);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].url.contains("/drive/v1/share?"));
        assert!(requests[1].url.contains("/v1/shield/captcha/init"));
        let payload: Value = serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(payload["action"], "GET:/drive/v1/share");
        assert_eq!(payload["captcha_token"], "personal-captcha");
        assert_eq!(payload["client_id"], SHARE_WEB_CLIENT);
        assert_eq!(payload["device_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(payload["meta"]["user_id"], "u1");
        assert_eq!(payload["redirect_uri"], "https://mypikpak.com/loading");
        assert_eq!(payload["meta"]["client_version"], "undefined");
        assert_eq!(payload["meta"]["package_name"], "drive.mypikpak.com");
        assert_eq!(payload["meta"]["timestamp"], "1789110095225");
        assert_eq!(
            payload["meta"]["captcha_sign"],
            "1.a7fcbb4b535e3c24e3e1b3020ec33bcf"
        );
        assert_eq!(requests[2].headers["X-Captcha-Token"], "share-captcha");
    }
    #[test]
    fn pikpak_share_media_only_link_requires_personal_transfer() {
        let store = credentials();
        let http = Http::new(vec![
            pass(),
            listing(),
            json!({
                "share_status":"OK",
                "file_info":{
                    "id":"f1",
                    "kind":"drive#file",
                    "medias":[{"link":{"url":"https://cdn.example/source-file"}}]
                }
            }),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Pikpak)).unwrap()
        else {
            panic!()
        };

        let entry = entry(plan);
        let ShareResolution::TransferRequired(request) = adapter.resolve(&entry).unwrap() else {
            panic!("media playback URLs cannot download complete shared files")
        };
        assert_eq!(request.source, entry.plan.source_for(&entry.entry.entry_id));
    }

    #[test]
    fn pikpak_share_transfer_records_only_new_file_with_matching_origin_metadata() {
        let credentials = credentials();
        let (plans, _) = counted_plan_store();
        let plan = CloudSharePlan::new(
            CloudProvider::Pikpak,
            "share123".into(),
            Some("a123".into()),
            vec![CloudShareEntry::new(
                "share123",
                "f1".into(),
                "verify.mp4".into(),
                vec![],
                Some(7),
            )],
            json!({"pass_code_token":"pass-secret"}),
        );
        plans.upsert(&plan).unwrap();
        let entry = entry(plan);
        let http = Http::new(vec![
            json!({"files":[{"id":"shared-folder","kind":"drive#folder","name":"Pack From Shared"}]}),
            json!({"files":[{"id":"old","kind":"drive#file","name":"verify.mp4","size":"7"}]}),
            json!({"id":"old","params":{"original_share_id":"share123","original_file_id":"f1"}}),
            json!({"restore_status":"RESTORE_START","restore_task_id":"task-1"}),
            json!({"phase":"PHASE_TYPE_COMPLETE","params":{"share_id":"share123"}}),
            json!({"files":[{"id":"shared-folder","kind":"drive#folder","name":"Pack From Shared"}]}),
            json!({"files":[
                {"id":"old","kind":"drive#file","name":"verify.mp4","size":"7"},
                {"id":"new","kind":"drive#file","name":"verify.mp4","size":"7"},
                {"id":"other","kind":"drive#file","name":"verify.mp4","size":"7"}
            ]}),
            json!({"id":"old","params":{"original_share_id":"share123","original_file_id":"f1"}}),
            json!({"id":"new","params":{"original_share_id":"share123","original_file_id":"f1"}}),
            json!({"id":"other","params":{"original_share_id":"different-share","original_file_id":"f1"}}),
            json!({"id":"new","name":"verify.mp4","web_content_link":"https://cdn.example/full-file"}),
        ]);
        let adapter = PikPakShareAdapter { credentials: &credentials, http: &http };

        let download = adapter.transfer(&entry, &plans).unwrap();

        assert_eq!(download.url, "https://cdn.example/full-file");
        let owner = adapter.transfer_owner_fingerprint(&entry).unwrap().unwrap();
        let recorded = plans.reusable_transfer(&entry.plan.source_for("f1"), &owner).unwrap().unwrap();
        assert_eq!(recorded.created_file_id, "new");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.iter().filter(|request| request.method == "POST").count(), 1);
        assert!(requests.iter().any(|request| request.url.ends_with("/drive/v1/share/restore")));
    }
    #[test]
    fn pikpak_share_verified_file_token_survives_pass_captcha_rotation() {
        let store = credentials();
        let http = Http::new(vec![
            json!({"error_code":9,"error":"captcha_required"}),
            json!({"captcha_token":"verified-file","expires_in":300,"url":"https://captcha.mypikpak.com/challenge"}),
            json!({"error_code":9,"error":"captcha_required"}),
            json!({"captcha_token":"pass-captcha","expires_in":300,"url":""}),
            pass(),
            json!({"share_status":"OK","file_info":{"id":"f1"}}),
        ]);
        let adapter = PikPakShareAdapter {
            credentials: &store,
            http: &http,
        };
        let file_url =
            format!("{SHARE_DRIVE}/drive/v1/share/file_info?share_id=fixture&file_id=f1");
        assert!(matches!(
            adapter
                .session()
                .unwrap()
                .request("GET", file_url.clone(), None),
            Err(QuarkError::PikpakOperationVerificationRequired(_))
        ));
        let mut saved: PikPakSession =
            serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
        saved.operation_verifications.insert(
            "POST:/drive/v1/files:batchDelete".into(),
            PikPakActionVerification {
                token: "other-action".into(),
                expires_at: u64::MAX,
                url: Some("https://captcha.mypikpak.com/other".into()),
            },
        );
        store
            .save("pikpak", &serde_json::to_vec(&saved).unwrap())
            .unwrap();

        // Explicit retry starts a fresh adapter session, then renews the pass ticket.
        let mut retry = adapter.session().unwrap();
        retry.pass("fixture", None).unwrap();
        retry.request("GET", file_url, None).unwrap();
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert_eq!(requests[4].headers["X-Captcha-Token"], "pass-captcha");
        assert_eq!(requests[5].headers["X-Captcha-Token"], "verified-file");
        let saved: PikPakSession =
            serde_json::from_slice(&store.load("pikpak").unwrap().unwrap()).unwrap();
        assert!(!saved
            .operation_verifications
            .contains_key("GET:/drive/v1/share/file_info"));
        assert!(saved
            .operation_verifications
            .contains_key("POST:/drive/v1/files:batchDelete"));
        assert_eq!(
            saved.verification_url.as_deref(),
            Some("https://captcha.mypikpak.com/other")
        );
    }
    #[test]
    fn pikpak_share_marks_not_found_and_deleted_as_missing_sources() {
        for share_status in ["NOT_FOUND", "DELETED"] {
            assert!(matches!(
                pikpak_share_status(&json!({"share_status":share_status})),
                Err(QuarkError::SourceFileMissing)
            ));
        }
    }
    #[test]
    fn pikpak_share_pages_recursively_and_never_retries_challenges() {
        let store = credentials();
        let http = Http::new(vec![
            pass(),
            json!({"share_status":"OK","files":[{"id":"d","kind":"drive#folder","name":"dir"}],"next_page_token":"p2"}),
            json!({"share_status":"OK","files":[],"next_page_token":""}),
            listing(),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();
        let ShareInspection::Plan(plan) =
            adapter.inspect(&candidate(CloudProvider::Pikpak)).unwrap()
        else {
            panic!()
        };
        assert_eq!(plan.entries[0].relative_directory, vec!["dir"]);
        assert!(http.requests.lock().unwrap()[2]
            .url
            .contains("page_token=p2"));
        let challenge = json!({"error_code":9,"error":"captcha_required"});
        let http = Http::new(vec![challenge.clone(), cap(), challenge]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Pikpak)),
            Err(QuarkError::CaptchaRequired)
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 3);
        for reply in [
            json!({"error_code":4031}),
            json!({"error":"forbidden"}),
            json!({"share_status":"EXPIRED"}),
            json!({"share_status":"PASS_CODE_ERROR"}),
        ] {
            let http = Http::new(vec![reply]);
            assert!(provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Pikpak))
                .is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 1);
        }
        let http = Http::new(vec![
            json!({"error_code":10,"error_description":"Regional Premium restricted, upgrade to Global Premium"}),
        ]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Pikpak)),
            Err(QuarkError::PikpakRegionalMembershipRestricted)
        ));
        let http = Http::new(vec![json!({"share_status":"PASS_CODE_EMPTY"})]);
        let mut input = candidate(CloudProvider::Pikpak);
        input.access_code = None;
        assert!(matches!(
            provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                .unwrap()
                .inspect(&input),
            Ok(ShareInspection::AccessCodeRequired)
        ));
    }

    #[test]
    fn pikpak_share_rejects_malformed_error_fields_even_with_a_url() {
        let store = credentials();
        for error in [
            json!({"error":{"code":"forbidden"}}),
            json!({"status":"403"}),
        ] {
            let mut response = json!({"share_status":"OK","file_info":{"id":"f1","kind":"drive#file","web_content_link":"https://cdn.example/f"}});
            response
                .as_object_mut()
                .unwrap()
                .extend(error.as_object().unwrap().clone());
            let http = Http::new(vec![pass(), listing(), response]);
            let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();
            let ShareInspection::Plan(plan) =
                adapter.inspect(&candidate(CloudProvider::Pikpak)).unwrap()
            else {
                panic!()
            };
            assert!(adapter.resolve(&entry(plan)).is_err());
            assert_eq!(http.requests.lock().unwrap().len(), 3);
        }
    }

    #[test]
    fn pikpak_share_missing_download_url_requires_transfer_but_bad_response_does_not() {
        let store = credentials();
        let original = store.load("pikpak").unwrap().unwrap();
        for (file, transfer) in [
            (json!({"id":"f1","kind":"drive#file"}), true),
            (json!({"id":"other","kind":"drive#file","web_content_link":"https://cdn.example/f"}), false),
            (json!({"id":"f1","kind":"drive#file","web_content_link":"https://mypikpak.com/s/other"}), false),
            (json!({"id":"f1","kind":"drive#file","web_content_link":42}), false),
        ] {
            let http = Http::new(vec![
                pass(),
                listing(),
                json!({"share_status":"OK","file_info":file}),
            ]);
            let adapter = provider_share_adapter(CloudProvider::Pikpak, &store, &http).unwrap();
            let ShareInspection::Plan(plan) =
                adapter.inspect(&candidate(CloudProvider::Pikpak)).unwrap()
            else {
                panic!()
            };
            if transfer {
                assert!(matches!(adapter.resolve(&entry(plan)), Ok(ShareResolution::TransferRequired(_))));
            } else {
                assert!(adapter.resolve(&entry(plan)).is_err());
            }
            assert_eq!(http.requests.lock().unwrap().len(), 3);
            assert_eq!(store.load("pikpak").unwrap().unwrap(), original);
        }
        let http = Http::new(vec![
            json!({"error_code":9,"error":"captcha_required"}),
            json!({"captcha_token":"token","url":"https://captcha.mypikpak.com/challenge"}),
        ]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Pikpak, &store, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Pikpak)),
            Err(QuarkError::PikpakOperationVerificationRequired("解析分享"))
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 2);
        let empty = MemoryCredentialStore::default();
        let http = Http::new(vec![]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Pikpak, &empty, &http)
                .unwrap()
                .inspect(&candidate(CloudProvider::Pikpak)),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn pikpak_restore_only_mutates_entries_confirmed_as_trashed() {
        let store = MemoryCredentialStore::default();
        store
            .save(
                "pikpak",
                br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device"}"#,
            )
            .unwrap();
        let http = Http::new(vec![
            json!({"files":[{"id":"trashed-id","name":"old.txt","kind":"drive#file"}],"next_page_token":""}),
            json!({"captcha_token":"restore-cap","expires_in":300}),
            json!({}),
        ]);

        PikPakClient::new(&http, &store)
            .restore_recycle_entries(&["trashed-id".into()])
            .unwrap();

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].url.contains("/drive/v1/files?"));
        assert!(requests[0]
            .url
            .contains("%22trashed%22%3A%7B%22eq%22%3Atrue%7D"));
        assert!(requests[1].url.contains("/v1/shield/captcha/init"));
        assert_eq!(requests[2].method, "POST");
        assert!(requests[2].url.ends_with("/drive/v1/files:batchUntrash"));
        assert_eq!(
            serde_json::from_str::<Value>(requests[2].body.as_deref().unwrap()).unwrap()["ids"],
            json!(["trashed-id"])
        );
    }

    #[test]
    fn pikpak_restore_rejects_live_entries_without_a_mutation() {
        let store = MemoryCredentialStore::default();
        store
            .save(
                "pikpak",
                br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device"}"#,
            )
            .unwrap();
        let http = Http::new(vec![json!({"files":[],"next_page_token":""})]);

        let error = PikPakClient::new(&http, &store)
            .restore_recycle_entries(&["live-id".into()])
            .unwrap_err();

        assert!(matches!(error, QuarkError::InvalidInput(_)));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("batchUntrash")));
    }

    #[test]
    fn supervised_snapshot_rotates_action_captcha_without_refreshing_access_session() {
        let durable = MemoryCredentialStore::default();
        durable
            .save(
                "pikpak",
                &serde_json::to_vec(&PikPakSession {
                    access_token: "current-access".into(),
                    refresh_token: "current-refresh".into(),
                    expires_at: u64::MAX,
                    device_id: "device".into(),
                    captcha_token: "stale-captcha".into(),
                    client_id: Some("YNxT9w7GMdWvEOKa".into()),
                    user_id: "user".into(),
                    captcha_expires_at: u64::MAX,
                    captcha_action: None,
                    verification_url: None,
                    operation_verifications: BTreeMap::new(),
                    auth_method: CloudAuthMethod::Password,
                })
                .unwrap(),
            )
            .unwrap();
        let snapshot =
            crate::cloud::auth_supervisor::SessionSnapshot::new(&durable, CloudProvider::Pikpak)
                .unwrap();
        let http = Http::new(vec![
            json!({"error":"riskLimited","error_code":9}),
            json!({"captcha_token":"rotated-captcha","expires_in":300,"url":""}),
            json!({}),
        ]);

        PikPakClient::new(&http, &snapshot)
            .rename("fixture", "renamed")
            .unwrap();

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].url.ends_with("/drive/v1/files/fixture"));
        assert!(requests[1].url.contains("/v1/shield/captcha/init"));
        assert!(requests[2].url.ends_with("/drive/v1/files/fixture"));
        let saved: PikPakSession =
            serde_json::from_slice(&snapshot.load("pikpak").unwrap().unwrap()).unwrap();
        assert_eq!(saved.access_token, "current-access");
        assert_eq!(saved.refresh_token, "current-refresh");
        assert_eq!(saved.captcha_token, "rotated-captcha");
    }
}
pub(crate) const PIKPAK_RECYCLE_BIN_ID: &str = "__versa_down_pikpak_recycle_bin__";
const PIKPAK_RECYCLE_BIN_NAME: &str = "回收站";
const CAPTCHA_SALTS: &[&str] = &[
    "SOP04dGzk0TNO7t7t9ekDbAmx+eq0OI1ovEx",
    "nVBjhYiND4hZ2NCGyV5beamIr7k6ifAsAbl",
    "Ddjpt5B/Cit6EDq2a6cXgxY9lkEIOw4yC1GDF28KrA",
    "VVCogcmSNIVvgV6U+AochorydiSymi68YVNGiz",
    "u5ujk5sM62gpJOsB/1Gu/zsfgfZO",
    "dXYIiBOAHZgzSruaQ2Nhrqc2im",
    "z5jUTBSIpBN9g4qSJGlidNAutX6",
    "KJE2oveZ34du/g1tiimm",
];
/// Public Android 1.53.2 protocol signature; timestamp is supplied so test vectors are stable.
pub fn pikpak_captcha_sign(client_id: &str, device_id: &str, timestamp: &str) -> String {
    let mut value = format!("{client_id}1.53.2com.pikcloud.pikpak{device_id}{timestamp}");
    for salt in CAPTCHA_SALTS {
        value = format!("{:x}", md5::compute(format!("{value}{salt}")));
    }
    format!("1.{value}")
}
#[derive(Clone, Serialize, Deserialize)]
pub struct PikPakActionVerification {
    token: String,
    expires_at: u64,
    url: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PikPakSession {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub captcha_token: String,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub captcha_expires_at: u64,
    /// Drive action for which `captcha_token` was issued. Legacy login tokens
    /// remain unscoped until PikPak rotates them for a concrete operation.
    #[serde(default)]
    pub captcha_action: Option<String>,
    /// Kept only in the encrypted provider session, never returned in an error/event.
    #[serde(default)]
    pub verification_url: Option<String>,
    #[serde(default)]
    pub operation_verifications: BTreeMap<String, PikPakActionVerification>,
    #[serde(default)]
    pub auth_method: CloudAuthMethod,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub struct PikPakClient<'a, T: CloudHttpTransport + ?Sized, S: CredentialStore + ?Sized> {
    pub(super) http: &'a T,
    store: &'a S,
    client_id: String,
    client_secret: String,
}
impl<'a, T: CloudHttpTransport + ?Sized, S: CredentialStore + ?Sized> PikPakClient<'a, T, S> {
    pub fn new(http: &'a T, store: &'a S) -> Self {
        Self {
            http,
            store,
            client_id: std::env::var("VERSADOWN_PIKPAK_CLIENT_ID")
                .unwrap_or_else(|_| "YNxT9w7GMdWvEOKa".into()),
            client_secret: std::env::var("VERSADOWN_PIKPAK_CLIENT_SECRET")
                .unwrap_or_else(|_| "dbw2OtmVEeuUvIptb1Coyg".into()),
        }
    }
    fn raw(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        session: Option<&PikPakSession>,
    ) -> Result<Value, QuarkError> {
        let mut headers = BTreeMap::from([(
            "User-Agent".into(),
            "ANDROID-com.pikcloud.pikpak/1.53.2".into(),
        )]);
        if session.is_some_and(|s| s.client_id.as_deref() == Some(SHARE_WEB_CLIENT)) {
            headers.insert("User-Agent".into(), SHARE_WEB_UA.into());
            headers.insert("Origin".into(), "https://mypikpak.com".into());
            headers.insert("Referer".into(), "https://mypikpak.com/".into());
        }
        if let Some(s) = session {
            if !s.access_token.is_empty() {
                headers.insert("Authorization".into(), format!("Bearer {}", s.access_token));
            }
            headers.insert("X-Device-Id".into(), s.device_id.clone());
            if let Some(id) = &s.client_id {
                headers.insert("X-Client-Id".into(), id.clone());
            }
            if !s.captcha_token.is_empty() {
                headers.insert("X-Captcha-Token".into(), s.captcha_token.clone());
            }
        }
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers,
            body: body.map(|b| b.to_string()),
        })?;
        if pikpak_regional_membership_restriction(&response) {
            return Err(QuarkError::PikpakRegionalMembershipRestricted);
        }
        response_json(&response)
    }
    pub fn save_session(&self, s: &PikPakSession) -> Result<(), QuarkError> {
        if s.access_token.is_empty() || s.refresh_token.is_empty() {
            return Err(QuarkError::AuthorizationExpired);
        }
        self.store.save(
            "pikpak",
            &serde_json::to_vec(s).map_err(|_| QuarkError::Storage)?,
        )
    }
    pub fn login(&self, username: &str, password: &str) -> Result<(), QuarkError> {
        if username.trim().is_empty() || password.is_empty() {
            return Err(QuarkError::InvalidInput("请输入账号和密码".into()));
        }
        let device_id = self
            .store
            .load("pikpak")?
            .map(|bytes| {
                serde_json::from_slice::<PikPakSession>(&bytes).map_err(|_| QuarkError::Storage)
            })
            .transpose()?
            .map(|session| session.device_id)
            .filter(|device| !device.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let pending = PikPakSession {
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: 0,
            device_id: device_id.clone(),
            captcha_token: String::new(),
            client_id: Some(self.client_id.clone()),
            user_id: String::new(),
            captcha_expires_at: 0,
            captcha_action: None,
            verification_url: None,
            operation_verifications: BTreeMap::new(),
            auth_method: CloudAuthMethod::Password,
        };
        let cap=self.raw("POST",format!("{AUTH}/v1/shield/captcha/init"),Some(json!({"client_id":self.client_id,"device_id":device_id,"action":format!("POST:{AUTH}/v1/auth/signin"),"meta":{"username":username}})),Some(&pending))?;
        if cap["url"].as_str().is_some_and(|u| !u.is_empty()) {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let captcha_token = required(&cap, "captcha_token")?;
        let v=self.raw("POST",format!("{AUTH}/v1/auth/signin"),Some(json!({"client_id":self.client_id,"client_secret":self.client_secret,"grant_type":"password","username":username,"password":password,"captcha_token":captcha_token})),Some(&pending))?;
        self.save_session(&PikPakSession {
            access_token: required(&v, "access_token")?,
            refresh_token: required(&v, "refresh_token")?,
            expires_at: now() + v["expires_in"].as_u64().unwrap_or(0),
            device_id,
            captcha_token,
            client_id: Some(self.client_id.clone()),
            user_id: v["sub"].as_str().unwrap_or("").into(),
            captcha_expires_at: now().saturating_add(cap["expires_in"].as_u64().unwrap_or(0)),
            captcha_action: None,
            verification_url: None,
            operation_verifications: BTreeMap::new(),
            auth_method: CloudAuthMethod::Password,
        })
    }
    fn stored(&self) -> Result<PikPakSession, QuarkError> {
        let bytes = self
            .store
            .load("pikpak")?
            .ok_or(QuarkError::AuthorizationExpired)?;
        let legacy = serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| QuarkError::Storage)?
            .get("auth_method")
            .is_none();
        let mut session: PikPakSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if legacy
            && session.client_id.as_deref() == Some(self.client_id.as_str())
            && !session.device_id.is_empty()
        {
            session.auth_method = CloudAuthMethod::Password;
        }
        Ok(session)
    }
    pub(crate) fn refresh_session(&self) -> Result<(), QuarkError> {
        self.refresh(self.stored()?).map(|_| ())
    }
    fn refresh(&self, mut s: PikPakSession) -> Result<PikPakSession, QuarkError> {
        let client_id = s.client_id.as_deref().unwrap_or(&self.client_id);
        let mut payload = json!({"client_id":client_id,"grant_type":"refresh_token","refresh_token":s.refresh_token});
        // Browser OAuth refresh tokens belong to their public browser client, not the Android client.
        if client_id == self.client_id {
            payload["client_secret"] = json!(self.client_secret);
        }
        let v = self.raw(
            "POST",
            format!("{AUTH}/v1/auth/token"),
            Some(payload),
            Some(&s),
        )?;
        s.access_token = required(&v, "access_token")?;
        if let Some(user_id) = v["sub"].as_str() {
            s.user_id = user_id.into();
        }
        if let Ok(r) = required(&v, "refresh_token") {
            s.refresh_token = r
        }
        s.expires_at = now() + v["expires_in"].as_u64().unwrap_or(0);
        self.save_session(&s)?;
        Ok(s)
    }
    pub fn request(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let s = self.stored()?;
        let automatic_session_recovery = self.store.automatic_session_recovery();
        // AuthSupervisor snapshots must not rotate access/refresh credentials on
        // their own. PikPak's per-action captcha, however, is part of an ordinary
        // drive request: it is scoped to the exact method/path and must be rotated
        // inside the private snapshot so the supervisor can publish it atomically.
        let mut access_refreshed = automatic_session_recovery && s.expires_at <= now() + 60;
        let mut s = if access_refreshed {
            self.refresh(s)?
        } else {
            s
        };
        let mut captcha_refreshed = false;
        let action = pikpak_captcha_action(method, &url)?;
        let mut has_action_token = false;
        if let Some(verification) = s
            .operation_verifications
            .get(&action)
            .filter(|v| v.expires_at > now())
        {
            s.captcha_token = verification.token.clone();
            s.captcha_expires_at = verification.expires_at;
            s.captcha_action = Some(action.clone());
            has_action_token = true;
        }
        // After PikPak rotates the generic login captcha into an
        // action-scoped ticket, that ticket must never be reused for another
        // endpoint. Recycle operations validate each item with GET and then
        // mutate it with POST; reusing the GET ticket for batchDelete is
        // rejected as `code 3 / invalid_argument` instead of captcha_required.
        has_action_token |=
            s.captcha_action.as_deref() == Some(action.as_str()) && s.captcha_expires_at > now();
        let requires_explicit_action_token = matches!(
            action.as_str(),
            "POST:/drive/v1/files:batchDelete" | "POST:/drive/v1/files:batchUntrash"
        );
        // Drive tickets do not scope the separate authentication service.
        // Account restoration must remain usable after a drive/share operation.
        if url.starts_with(&format!("{DRIVE}/"))
            && (requires_explicit_action_token || s.captcha_action.is_some())
            && !has_action_token
        {
            s = self.refresh_action_captcha(method, &url, s)?;
            captcha_refreshed = true;
        }
        loop {
            match self.raw(method, url.clone(), body.clone(), Some(&s)) {
                Err(QuarkError::AuthorizationExpired)
                    if automatic_session_recovery && !access_refreshed =>
                {
                    s = self.refresh(s)?;
                    access_refreshed = true;
                }
                Err(QuarkError::CaptchaRequired) if !captcha_refreshed => {
                    s = self.refresh_action_captcha(method, &url, s)?;
                    captcha_refreshed = true;
                }
                Err(QuarkError::CaptchaRequired | QuarkError::WebAuthorizationRequired)
                    if url.starts_with(&format!("{DRIVE}/")) =>
                {
                    return Err(pikpak_operation_verification_error(method, &url))
                }
                r => {
                    if r.is_ok() && s.operation_verifications.remove(&action).is_some() {
                        s.verification_url = s
                            .operation_verifications
                            .values()
                            .filter(|v| v.expires_at > now())
                            .find_map(|v| v.url.clone());
                        self.save_session(&s)?;
                    }
                    return r;
                }
            }
        }
    }
    fn refresh_action_captcha(
        &self,
        method: &str,
        url: &str,
        mut s: PikPakSession,
    ) -> Result<PikPakSession, QuarkError> {
        let action_url = url::Url::parse(url).map_err(|_| QuarkError::InvalidResponse)?;
        // Use the signature belonging to the saved OAuth client. A web session
        // must never be silently replaced by the Android login/client identity.
        let client_id = s.client_id.as_deref().unwrap_or(&self.client_id).to_owned();
        let web = client_id == SHARE_WEB_CLIENT;
        if action_url.host_str() != Some("api-drive.mypikpak.com")
            || s.device_id.is_empty()
            || (!web && client_id != self.client_id)
        {
            return Err(pikpak_operation_verification_error(method, url));
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        let captcha_device_id = if web {
            pikpak_web_origin_device_id(&s.device_id)?
        } else {
            pikpak_android_origin_device_id(&s.device_id).to_owned()
        };
        let mut meta = if web {
            json!({
                "client_version": SHARE_WEB_VERSION,
                "package_name": SHARE_WEB_PACKAGE,
                "timestamp": SHARE_WEB_SIGNATURE_TIMESTAMP,
                "captcha_sign": pikpak_current_share_captcha_sign(&captcha_device_id),
            })
        } else {
            json!({"client_version":"1.53.2","package_name":"com.pikcloud.pikpak","timestamp":timestamp,"captcha_sign":pikpak_captcha_sign(&client_id,&captcha_device_id,&timestamp)})
        };
        if s.user_id.is_empty() {
            // This claim is only metadata for the provider, never used as a local authorization decision.
            if let Some(payload) = s
                .access_token
                .split('.')
                .nth(1)
                .and_then(|p| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(p)
                        .ok()
                })
                .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
            {
                s.user_id = payload["sub"].as_str().unwrap_or("").into();
            }
        }
        if !s.user_id.is_empty() {
            meta["user_id"] = json!(s.user_id);
        }
        // PikPak scopes a GET for an individual drive file to the collection
        // captcha action. Sending the concrete id here yields an interactive
        // challenge even with a freshly completed browser authorization.
        let action = pikpak_captcha_action(method, url)?;
        let redirect = if web {
            "https://mypikpak.com/loading"
        } else {
            "xlaccsdk01://xbase.cloud/callback?state=harbor"
        };
        let payload = json!({"action":action,"captcha_token":s.captcha_token,"client_id":client_id,"device_id":captcha_device_id,"meta":meta,"redirect_uri":redirect});
        let v = self
            .raw(
                "POST",
                format!(
                    "{AUTH}/v1/shield/captcha/init?client_id={}",
                    encoded(&client_id)
                ),
                Some(payload),
                Some(&s),
            )
            .map_err(|e| {
                if matches!(
                    e,
                    QuarkError::CaptchaRequired | QuarkError::WebAuthorizationRequired
                ) {
                    pikpak_operation_verification_error(method, url)
                } else {
                    e
                }
            })?;
        let challenge = v["url"].as_str().filter(|u| !u.is_empty());
        s.captcha_token = required(&v, "captcha_token")
            .map_err(|_| pikpak_operation_verification_error(method, url))?;
        s.captcha_expires_at = now().saturating_add(v["expires_in"].as_u64().unwrap_or(300));
        s.captcha_action = Some(action.clone());
        if let Some(challenge) = challenge {
            s.verification_url =
                allows_pikpak_operation_verification_url(challenge).then(|| challenge.to_owned());
            s.operation_verifications
                .retain(|_, v| v.expires_at > now());
            if s.operation_verifications.len() >= 16 {
                s.operation_verifications.pop_first();
            }
            s.operation_verifications.insert(
                action,
                PikPakActionVerification {
                    token: s.captcha_token.clone(),
                    expires_at: s.captcha_expires_at,
                    url: s.verification_url.clone(),
                },
            );
        }
        // A challenge token becomes usable only after the server accepts the
        // user's verification. Preserve it so a subsequent explicit retry uses
        // that exact token instead of starting another challenge.
        self.save_session(&s)?;
        if challenge.is_some() {
            return Err(pikpak_operation_verification_error(method, url));
        }
        Ok(s)
    }

    pub fn operation_verification_url(
        &self,
        expected_account_id: &str,
    ) -> Result<String, QuarkError> {
        if expected_account_id.is_empty() || self.account()?.id != expected_account_id {
            return Err(QuarkError::InvalidInput(
                "授权账号已变化，已停止打开原操作的验证页面".into(),
            ));
        }
        let session = self.stored()?;
        Ok(session
            .verification_url
            .filter(|url| {
                let expiry = session
                    .operation_verifications
                    .values()
                    .find(|v| v.url.as_ref() == Some(url))
                    .map(|v| v.expires_at)
                    .unwrap_or(session.captcha_expires_at);
                expiry > now() && allows_pikpak_operation_verification_url(url)
            })
            .unwrap_or_else(|| "https://mypikpak.com/drive".into()))
    }
    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let v = self.request("GET", format!("{AUTH}/v1/user/me"), None)?;
        let auth_method = self.stored()?.auth_method;
        let id = required(&v, "sub").or_else(|_| required(&v, "id"))?;
        let display_name = required(&v, "name")
            .or_else(|_| required(&v, "username"))
            .or_else(|_| required(&v, "email"))?;
        Ok(CloudAccount {
            provider: CloudProvider::Pikpak,
            id,
            display_name,
            auth_method,
        })
    }
    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let value = self.request("GET", format!("{DRIVE}/drive/v1/about"), None)?;
        CloudStorageCapacity::from_json(&value["quota"]["limit"], &value["quota"]["usage"], false)
    }
    pub fn list(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        let trashed = parent == PIKPAK_RECYCLE_BIN_ID;
        let p = if trashed { "" } else { root(parent) };
        let mut items = Vec::new();
        let mut token = String::new();
        let mut seen = HashSet::new();
        loop {
            let v = self.request(
                "GET",
                format!(
                    "{DRIVE}/drive/v1/files?parent_id={}&limit=500&filters={}&page_token={}",
                    encoded(p),
                    encoded(if trashed {
                        r#"{"trashed":{"eq":true}}"#
                    } else {
                        r#"{"trashed":{"eq":false}}"#
                    }),
                    encoded(&token)
                ),
                None,
            )?;
            for f in v["files"].as_array().ok_or(QuarkError::InvalidResponse)? {
                items.push(parse_pikpak_file(f, parent)?)
            }
            token = v["next_page_token"].as_str().unwrap_or("").into();
            if token.is_empty() {
                break;
            }
            if !seen.insert(token.clone()) || seen.len() > 10000 {
                return Err(QuarkError::InvalidResponse);
            }
        }
        if parent == "0" {
            items.push(CloudFile {
                id: PIKPAK_RECYCLE_BIN_ID.into(),
                name: PIKPAK_RECYCLE_BIN_NAME.into(),
                parent_id: "0".into(),
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

    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
        }
        // Creation generates the code; editing is a separate provider operation.
        // Never silently discard a caller's explicit custom code.
        if options
            .pass_code
            .as_deref()
            .is_some_and(|code| !code.is_empty())
        {
            return Err(QuarkError::InvalidInput(
                "PikPak 请先生成分享，再修改提取码".into(),
            ));
        }
        let needs_pass_code = options.generate_pass_code;
        let value = self.request(
            "POST",
            format!("{DRIVE}/drive/v1/share"),
            Some(json!({
                "file_ids": file_ids,
                "share_to": if needs_pass_code { "encryptedlink" } else { "publiclink" },
                "expiration_days": options.expires_in_days.map(i64::from).unwrap_or(-1),
                "pass_code_option": if needs_pass_code { "REQUIRED" } else { "NOT_REQUIRED" },
            })),
        )?;
        let mut share = pikpak_share_from_value(&value, Some(file_ids), None)?;
        share.name = options.name.clone();
        Ok(share)
    }

    pub fn update_share_pass_code(
        &self,
        share_id: &str,
        pass_code: &str,
    ) -> Result<(), QuarkError> {
        if share_id.trim().is_empty() || share_id.chars().any(char::is_control) {
            return Err(QuarkError::InvalidInput("分享 ID 无效".into()));
        }
        if pass_code.len() != 4 || !pass_code.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(QuarkError::InvalidInput(
                "提取码必须为 4 位字母或数字".into(),
            ));
        }
        self.request(
            "PATCH",
            format!("{DRIVE}/drive/v1/share"),
            Some(json!({"share_id":share_id,"share_to":"encryptedlink",
                "pass_code_option":"REQUIRED","custom_pass_code":pass_code})),
        )?;
        // The provider may acknowledge a PATCH while ignoring unsupported
        // fields. Do not let the UI claim the code changed without readback.
        if self
            .list_shares()?
            .iter()
            .any(|share| share.id == share_id && share.pass_code.as_deref() == Some(pass_code))
        {
            Ok(())
        } else {
            Err(QuarkError::Provider(
                "网盘暂未确认新提取码，请刷新分享列表核对后重试".into(),
            ))
        }
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let mut shares = Vec::new();
        let mut page_token = String::new();
        let mut seen_tokens = HashSet::new();
        for _ in 0..10_000 {
            let value = self.request(
                "GET",
                format!(
                    "{DRIVE}/drive/v1/share/list?limit=100&thumbnail_size=SIZE_SMALL&page_token={}",
                    encoded(&page_token)
                ),
                None,
            )?;
            let list = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            shares.extend(
                list.iter()
                    .map(|share| pikpak_share_from_value(share, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            page_token = value
                .get("next_page_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if page_token.is_empty() {
                return Ok(shares);
            }
            if !seen_tokens.insert(page_token.clone()) {
                return Err(QuarkError::InvalidResponse);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        if share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        self.request(
            "POST",
            format!("{DRIVE}/drive/v1/share:batchDelete"),
            Some(json!({"ids": share_ids})),
        )?;
        Ok(())
    }
    pub fn mkdir(&self, parent: &str, name: &str) -> Result<CloudFile, QuarkError> {
        reject_pikpak_recycle_bin_target(parent)?;
        let v = self.request(
            "POST",
            format!("{DRIVE}/drive/v1/files"),
            Some(json!({"kind":"drive#folder","parent_id":root(parent),"name":name})),
        )?;
        parse_pikpak_file(v.get("file").unwrap_or(&v), parent)
    }
    pub fn rename(&self, id: &str, name: &str) -> Result<(), QuarkError> {
        reject_pikpak_recycle_bin_source(id)?;
        self.request(
            "PATCH",
            format!("{DRIVE}/drive/v1/files/{}", encoded(id)),
            Some(json!({"name":name})),
        )?;
        Ok(())
    }
    pub fn move_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        reject_pikpak_recycle_bin_sources(ids)?;
        reject_pikpak_recycle_bin_target(parent)?;
        self.request(
            "POST",
            format!("{DRIVE}/drive/v1/files:batchMove"),
            Some(json!({"ids":ids,"to":{"parent_id":root(parent)}})),
        )?;
        Ok(())
    }
    pub fn copy_files(&self, ids: &[String], parent: &str) -> Result<(), QuarkError> {
        reject_pikpak_recycle_bin_sources(ids)?;
        reject_pikpak_recycle_bin_target(parent)?;
        self.request(
            "POST",
            format!("{DRIVE}/drive/v1/files:batchCopy"),
            Some(json!({"ids":ids,"to":{"parent_id":root(parent)}})),
        )?;
        Ok(())
    }
    pub fn trash(&self, ids: &[String]) -> Result<(), QuarkError> {
        reject_pikpak_recycle_bin_sources(ids)?;
        self.request(
            "POST",
            format!("{DRIVE}/drive/v1/files:batchTrash"),
            Some(json!({"ids":ids})),
        )?;
        Ok(())
    }
    /// Permanently deletes items already present in PikPak's recycle bin.
    pub fn delete(&self, ids: &[String]) -> Result<(), QuarkError> {
        reject_pikpak_recycle_bin_sources(ids)?;
        self.validate_recycle_entries(ids, "只能永久删除 PikPak 回收站中的项目")?;
        self.request(
            "POST",
            format!("{DRIVE}/drive/v1/files:batchDelete"),
            Some(json!({"ids":ids})),
        )?;
        Ok(())
    }
    /// Restores items only after the provider confirms that they remain trashed.
    pub fn restore_recycle_entries(&self, ids: &[String]) -> Result<(), QuarkError> {
        reject_pikpak_recycle_bin_sources(ids)?;
        self.validate_recycle_entries(ids, "只能恢复 PikPak 回收站中的项目")?;
        self.request(
            "POST",
            format!("{DRIVE}/drive/v1/files:batchUntrash"),
            Some(json!({"ids":ids})),
        )?;
        Ok(())
    }
    fn validate_recycle_entries(
        &self,
        ids: &[String],
        message: &'static str,
    ) -> Result<(), QuarkError> {
        let recycle = self.list(PIKPAK_RECYCLE_BIN_ID)?;
        let current = recycle
            .items
            .into_iter()
            .map(|item| item.id)
            .collect::<HashSet<_>>();
        if ids.iter().all(|id| current.contains(id)) {
            Ok(())
        } else {
            Err(QuarkError::InvalidInput(message.into()))
        }
    }
    pub fn download(&self, id: &str) -> Result<(String, String), QuarkError> {
        reject_pikpak_recycle_bin_source(id)?;
        let cached = self
            .request(
                "GET",
                format!("{DRIVE}/drive/v1/files/{}?usage=CACHE", encoded(id)),
                None,
            )
            .map_err(download_source_missing_from_http_404)?;
        if let Some(url) = cached["web_content_link"]
            .as_str()
            .filter(|url| url.starts_with("https://"))
        {
            return Ok((url.into(), required(&cached, "name")?));
        }
        let fetched = self
            .request(
                "GET",
                format!("{DRIVE}/drive/v1/files/{}?usage=FETCH", encoded(id)),
                None,
            )
            .map_err(download_source_missing_from_http_404)?;
        let url = fetched["web_content_link"]
            .as_str()
            .filter(|url| url.starts_with("https://"))
            .ok_or(QuarkError::InvalidResponse)?;
        Ok((url.into(), required(&fetched, "name")?))
    }
}

fn pikpak_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = required(value, "share_id")?;
    // The management list uses title/params.name, not the creation response's
    // share_name. Skip empty fields so they do not mask a valid fallback.
    let text = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
        })
    };
    Ok(CloudShare {
        id: id.clone(),
        url: value
            .get("share_url")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://mypikpak.com/s/{id}")),
        name: text(&["title", "share_name", "name"])
            .or_else(|| {
                value
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "分享文件".into()),
        kind: if matches!(
            text(&["file_kind", "file_type"]).as_deref(),
            Some("drive#folder" | "folder")
        ) {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids: value
            .get("file_ids")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .filter(|ids: &Vec<String>| !ids.is_empty())
            .or_else(|| text(&["file_id"]).map(|id| vec![id]))
            .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
            .unwrap_or_default(),
        pass_code: value
            .get("pass_code")
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: text(&["create_time", "created_at"]),
        expires_at: text(&["expiration_at", "expiration", "expire_at"]),
    })
}

pub(crate) fn reject_pikpak_recycle_bin_target(parent: &str) -> Result<(), QuarkError> {
    if parent == PIKPAK_RECYCLE_BIN_ID {
        return Err(QuarkError::InvalidInput(
            "回收站不能作为创建、移动、复制或上传的目标目录".into(),
        ));
    }
    Ok(())
}

fn reject_pikpak_recycle_bin_source(id: &str) -> Result<(), QuarkError> {
    if id == PIKPAK_RECYCLE_BIN_ID {
        return Err(QuarkError::InvalidInput(
            "回收站仅可打开，不能作为文件操作对象".into(),
        ));
    }
    Ok(())
}

fn reject_pikpak_recycle_bin_sources(ids: &[String]) -> Result<(), QuarkError> {
    if ids.iter().any(|id| id == PIKPAK_RECYCLE_BIN_ID) {
        return Err(QuarkError::InvalidInput(
            "回收站仅可打开，不能作为文件操作对象".into(),
        ));
    }
    Ok(())
}

/// Maps only the provider's explicit Regional Premium → Global Premium
/// restriction wording from fixed JSON error fields. It never infers this
/// from HTTP status, numeric codes, generic region wording, CAPTCHA, or
/// authorization errors.
fn pikpak_regional_membership_restriction(response: &CloudHttpResponse) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(&response.body) else {
        return false;
    };
    let error = value["error"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let error_code = value["error_code"]
        .as_i64()
        .or_else(|| value["code"].as_i64());
    // Preserve the provider's structured risk/CAPTCHA handling in response_json.
    // Its text can include plan wording, but it is not proof of a membership-only
    // restriction and must continue to trigger web authorization.
    if matches!(error_code, Some(9 | 4031)) || error == "risklimited" {
        return false;
    }
    let mut texts = Vec::new();
    for key in ["error", "error_description", "message"] {
        if let Some(text) = value[key].as_str() {
            texts.push(text);
        }
    }
    for key in ["details", "error_details"] {
        if let Some(details) = value[key].as_array() {
            for detail in details {
                for key in ["reason", "message"] {
                    if let Some(text) = detail[key].as_str() {
                        texts.push(text);
                    }
                }
            }
        }
    }
    let normalized = texts
        .iter()
        .map(|text| text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let is_captcha_or_authorization = [
        "captcha",
        "unauthenticated",
        "invalid_grant",
        "invalid_token",
        "验证码",
        "授权失效",
    ]
    .iter()
    .any(|marker| normalized.contains(marker));
    let is_regional_membership =
        normalized.contains("regional premium") || normalized.contains("区域会员");
    let is_global_membership =
        normalized.contains("global premium") || normalized.contains("全球会员");
    let is_restricted_or_upgrade = ["restrict", "upgrade", "受限", "升级"]
        .iter()
        .any(|marker| normalized.contains(marker));
    !is_captcha_or_authorization
        && is_regional_membership
        && is_global_membership
        && is_restricted_or_upgrade
}

fn root(p: &str) -> &str {
    if p == "0" {
        ""
    } else {
        p
    }
}

use super::share::{
    CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudSharePlanStore,
    CloudShareResolvedEntry, CloudTransferCapacity, RecordedTransferDownload,
    RecordedTransferResolver, ShareInspect, ShareInspection, ShareOperationResult,
    ShareResolution, ShareResolve, ShareTransfer, TransferRequest,
};
impl<'a, T: CloudHttpTransport + CloudUploadTransport, S: CredentialStore + ?Sized>
    PikPakClient<'a, T, S>
{
    pub fn upload_file(&self, parent: &str, path: &Path) -> Result<CloudFile, QuarkError> {
        reject_pikpak_recycle_bin_target(parent)?;
        super::upload::upload_pikpak_file(self.http, parent, path, |payload| {
            self.request("POST", format!("{DRIVE}/drive/v1/files"), Some(payload))
        })
    }
}

const SHARE_DRIVE: &str = "https://api-drive.mypikpak.com";
const SHARE_AUTH: &str = "https://user.mypikpak.com";
const SHARE_WEB_CLIENT: &str = "YUMx5nI8ZU8Ap8pm";
const SHARE_WEB_VERSION: &str = "undefined";
const SHARE_WEB_PACKAGE: &str = "drive.mypikpak.com";
const SHARE_WEB_SIGNATURE_TIMESTAMP: &str = "1789110095225";
const SHARE_WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";
pub struct PikPakShareAdapter<'a> {
    pub credentials: &'a dyn CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
struct PikPakShareSession<'a> {
    credentials: &'a dyn CredentialStore,
    http: &'a dyn CloudHttpTransport,
    session: PikPakSession,
    device_id: String,
    headers: BTreeMap<String, String>,
}
impl PikPakShareAdapter<'_> {
    fn session(&self) -> Result<PikPakShareSession<'_>, QuarkError> {
        let bytes = self
            .credentials
            .load("pikpak")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: PikPakSession =
            serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.access_token.is_empty() {
            return Err(QuarkError::NotAuthorized);
        }
        if session.expires_at <= now() {
            return Err(QuarkError::AuthorizationExpired);
        }
        if session.device_id.is_empty() {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        if session.captcha_token.is_empty() {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let device_id = pikpak_web_origin_device_id(&session.device_id)?;
        Ok(PikPakShareSession {
            credentials: self.credentials,
            http: self.http,
            session: session.clone(),
            device_id: device_id.clone(),
            headers: BTreeMap::from([
                (
                    "Authorization".into(),
                    format!("Bearer {}", session.access_token),
                ),
                ("X-Client-Id".into(), SHARE_WEB_CLIENT.into()),
                ("X-Device-Id".into(), device_id),
                ("X-Captcha-Token".into(), session.captcha_token),
                ("User-Agent".into(), SHARE_WEB_UA.into()),
                ("Referer".into(), "https://mypikpak.com/".into()),
            ]),
        })
    }
}
fn pikpak_current_share_captcha_sign(device: &str) -> String {
    let mut value = format!(
        "{SHARE_WEB_CLIENT}{SHARE_WEB_VERSION}{SHARE_WEB_PACKAGE}{device}{SHARE_WEB_SIGNATURE_TIMESTAMP}"
    );
    for salt in [
        "fyZ4+p77W1U4zcWBUwefAIFhFxvADWtT1wzolCxhg9q7etmGUjXr",
        "uSUX02HYJ1IkyLdhINEFcCf7l2",
        "iWt97bqD/qvjIaPXB2Ja5rsBWtQtBZZmaHH2rMR41",
        "3binT1s/5a1pu3fGsN",
        "8YCCU+AIr7pg+yd7CkQEY16lDMwi8Rh4WNp5",
        "DYS3StqnAEKdGddRP8CJrxUSFh",
        "crquW+4",
        "ryKqvW9B9hly+JAymXCIfag5Z",
        "Hr08T/NDTX1oSJfHk90c",
        "i",
    ] {
        value = format!("{:x}", md5::compute(format!("{value}{salt}")));
    }
    format!("1.{value}")
}
fn pikpak_web_origin_device_id(raw: &str) -> Result<String, QuarkError> {
    let raw = raw.rsplit('.').next().unwrap_or(raw);
    let device = raw.get(..32).ok_or(QuarkError::WebAuthorizationRequired)?;
    if !device.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    Ok(device.to_ascii_lowercase())
}
// A persisted login may carry the SDK's device signature, not just its device
// ID. Keep that session/header identity intact, but sign captcha requests with
// the original ID, as the provider does. Do not truncate arbitrary opaque IDs.
fn pikpak_android_origin_device_id(raw: &str) -> &str {
    match raw
        .strip_prefix("wdi10.")
        .or_else(|| raw.strip_prefix("div101."))
    {
        Some(signed)
            if signed.len() == 64
                && signed
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                && signed[..32].bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            &signed[..32]
        }
        _ => raw,
    }
}
impl PikPakShareSession<'_> {
    fn request(
        &mut self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let action = pikpak_captcha_action(method, &url)?;
        let token = self
            .session
            .operation_verifications
            .get(&action)
            .filter(|verification| verification.expires_at > now())
            .map(|verification| verification.token.clone())
            .unwrap_or_else(|| self.session.captcha_token.clone());
        self.headers.insert("X-Captcha-Token".into(), token);
        let result = match self.request_once(method, url.clone(), body.clone()) {
            Err(QuarkError::CaptchaRequired) => {
                self.refresh_action_captcha(method, &url)?;
                self.request_once(method, url, body)
            }
            result => result,
        };
        if result.is_ok()
            && self
                .session
                .operation_verifications
                .remove(&action)
                .is_some()
        {
            self.session.verification_url = self
                .session
                .operation_verifications
                .values()
                .filter(|verification| verification.expires_at > now())
                .find_map(|verification| verification.url.clone());
            self.credentials.save(
                "pikpak",
                &serde_json::to_vec(&self.session).map_err(|_| QuarkError::Storage)?,
            )?;
        }
        result
    }
    fn request_once(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let response = self.http.send(CloudHttpRequest {
            method,
            url,
            headers: self.headers.clone(),
            body: body.map(|v| v.to_string()),
        })?;
        let value: Value =
            serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
        if !value.is_object() {
            return Err(QuarkError::InvalidResponse);
        }
        // Numeric fields must not hide one another or default malformed errors to success.
        for key in ["code", "error_code", "status"] {
            if let Some(code) = value.get(key) {
                if code.as_i64().is_none() {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        if value
            .get("error")
            .is_some_and(|value| value.as_str().is_none())
        {
            return Err(QuarkError::InvalidResponse);
        }
        if let (Some(a), Some(b)) = (value.get("code"), value.get("error_code")) {
            if a != b {
                return Err(QuarkError::InvalidResponse);
            }
        }
        if pikpak_regional_membership_restriction(&response) {
            return Err(QuarkError::PikpakRegionalMembershipRestricted);
        }
        response_json(&response).map_err(|error| match error {
            QuarkError::Provider(_) => {
                QuarkError::Provider("PikPak 分享请求失败，请检查分享有效期及权限".into())
            }
            error => error,
        })
    }
    fn refresh_action_captcha(&mut self, method: &str, url: &str) -> Result<(), QuarkError> {
        let action = pikpak_captcha_action(method, url)?;
        let meta = json!({
            "client_version":SHARE_WEB_VERSION,
            "package_name":SHARE_WEB_PACKAGE,
            "user_id":self.session.user_id,
            "timestamp":SHARE_WEB_SIGNATURE_TIMESTAMP,
            "captcha_sign":pikpak_current_share_captcha_sign(&self.device_id),
        });
        let payload = json!({
            "action":action,
            "captcha_token":self.session.captcha_token,
            "client_id":SHARE_WEB_CLIENT,
            "device_id":self.device_id,
            "meta":meta,
            "redirect_uri":"https://mypikpak.com/loading",
        });
        let response = self.http.send(CloudHttpRequest {
            method: "POST",
            url: format!("{SHARE_AUTH}/v1/shield/captcha/init?client_id={SHARE_WEB_CLIENT}"),
            headers: self.headers.clone(),
            body: Some(payload.to_string()),
        })?;
        let value = response_json(&response).map_err(|error| match error {
            QuarkError::CaptchaRequired | QuarkError::WebAuthorizationRequired => {
                pikpak_operation_verification_error(method, url)
            }
            error => error,
        })?;
        let token = required(&value, "captcha_token")?;
        let challenge = value["url"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        self.session.captcha_token = token.clone();
        self.session.captcha_expires_at =
            now().saturating_add(value["expires_in"].as_u64().unwrap_or(300));
        // Persist the ticket and its scope together. A share ticket must not
        // inherit a previous drive/list/delete action when reusing the session.
        self.session.captcha_action = Some(action.clone());
        self.headers.insert("X-Captcha-Token".into(), token);
        if let Some(challenge) = challenge.as_deref() {
            self.session.verification_url =
                allows_pikpak_operation_verification_url(challenge).then(|| challenge.to_owned());
            self.session
                .operation_verifications
                .retain(|_, verification| verification.expires_at > now());
            if self.session.operation_verifications.len() >= 16 {
                self.session.operation_verifications.pop_first();
            }
            self.session.operation_verifications.insert(
                action,
                PikPakActionVerification {
                    token: self.session.captcha_token.clone(),
                    expires_at: self.session.captcha_expires_at,
                    url: self.session.verification_url.clone(),
                },
            );
        }
        self.credentials.save(
            "pikpak",
            &serde_json::to_vec(&self.session).map_err(|_| QuarkError::Storage)?,
        )?;
        if challenge.is_some() {
            return Err(pikpak_operation_verification_error(method, url));
        }
        Ok(())
    }
    fn pass(&mut self, id: &str, code: Option<&str>) -> Result<Value, QuarkError> {
        self.request("GET",format!("{SHARE_DRIVE}/drive/v1/share?share_id={}&pass_code={}&thumbnail_size=SIZE_LARGE&limit=100",encoded(id),encoded(code.unwrap_or(""))),None)
    }
}
fn pikpak_share_status(value: &Value) -> Result<(), QuarkError> {
    match value["share_status"].as_str() {
        Some("OK") => Ok(()),
        Some("PASS_CODE_EMPTY" | "PASS_CODE_ERROR") => Err(QuarkError::InvalidInput(
            "PikPak 分享访问码错误或缺失".into(),
        )),
        Some("NOT_FOUND" | "DELETED") => Err(QuarkError::SourceFileMissing),
        Some(_) => Err(QuarkError::Provider("PikPak 分享已失效或无权访问".into())),
        None => Err(QuarkError::InvalidResponse),
    }
}

fn pikpak_share_pass_token(value: &Value, code: Option<&str>) -> Result<String, QuarkError> {
    pikpak_share_status(value)?;
    let token = value
        .get("pass_code_token")
        .map(|value| value.as_str().ok_or(QuarkError::InvalidResponse))
        .transpose()?
        .unwrap_or("");
    if code.is_some() && token.is_empty() {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(token.into())
}

fn stored_pikpak_share_pass_token(
    context: &Value,
    code: Option<&str>,
) -> Result<Option<String>, QuarkError> {
    let Some(value) = context.get("pass_code_token") else {
        return Ok(None);
    };
    let token = value.as_str().ok_or(QuarkError::InvalidResponse)?;
    if token.chars().any(char::is_control) {
        return Err(QuarkError::InvalidResponse);
    }
    if code.is_some() && token.is_empty() {
        return Ok(None);
    }
    Ok(Some(token.into()))
}

fn pikpak_share_token_needs_refresh(value: &Value) -> bool {
    value["share_status"].as_str() == Some("PASS_CODE_ERROR")
}

fn with_pikpak_share_pass_token(mut context: Value, token: &str) -> Value {
    if let Some(values) = context.as_object_mut() {
        values.insert("pass_code_token".into(), token.into());
        context
    } else {
        json!({"pass_code_token":token})
    }
}
impl ShareInspect for PikPakShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let id = super::share::share_id(candidate, CloudProvider::Pikpak)?;
        let mut session = self.session()?;
        let pass = session.pass(&id, candidate.access_code.as_deref())?;
        if candidate.access_code.is_none()
            && matches!(
                pass["share_status"].as_str(),
                Some("PASS_CODE_EMPTY" | "PASS_CODE_ERROR")
            )
        {
            return Ok(ShareInspection::AccessCodeRequired);
        }
        pikpak_share_status(&pass)?;
        let token = pass
            .get("pass_code_token")
            .map(|value| value.as_str().ok_or(QuarkError::InvalidResponse))
            .transpose()?
            .unwrap_or("");
        if candidate.access_code.is_some() && token.is_empty() {
            return Err(QuarkError::InvalidResponse);
        }
        let mut queue = std::collections::VecDeque::from([(String::new(), vec![])]);
        let mut seen = HashSet::new();
        let mut entries = vec![];
        let mut requests = 0usize;
        while let Some((parent, directory)) = queue.pop_front() {
            let mut page = String::new();
            let mut pages = HashSet::new();
            loop {
                requests += 1;
                if requests > 10_000 || directory.len() > 128 {
                    return Err(QuarkError::InvalidResponse);
                }
                let value = session.request("GET",format!("{SHARE_DRIVE}/drive/v1/share/detail?share_id={}&parent_id={}&pass_code_token={}&page_token={}&limit=100&thumbnail_size=SIZE_LARGE&with_audit=true",encoded(&id),encoded(&parent),encoded(token),encoded(&page)),None)?;
                pikpak_share_status(&value)?;
                for raw in value["files"]
                    .as_array()
                    .ok_or(QuarkError::InvalidResponse)?
                {
                    let file = parse_pikpak_file(raw, &parent)?;
                    if !seen.insert(file.id.clone()) {
                        return Err(QuarkError::InvalidResponse);
                    }
                    match raw["kind"].as_str() {
                        Some("drive#folder") => {
                            let mut path = directory.clone();
                            path.push(file.name);
                            queue.push_back((file.id, path));
                        }
                        Some("drive#file") => entries.push(CloudShareEntry::new(
                            &id,
                            file.id,
                            file.name,
                            directory.clone(),
                            file.size,
                        )),
                        _ => return Err(QuarkError::InvalidResponse),
                    }
                }
                page = value
                    .get("next_page_token")
                    .map(|v| v.as_str().ok_or(QuarkError::InvalidResponse))
                    .transpose()?
                    .unwrap_or("")
                    .into();
                if page.is_empty() {
                    break;
                }
                if !pages.insert(page.clone()) {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        Ok(ShareInspection::Plan(CloudSharePlan::new(
            CloudProvider::Pikpak,
            id,
            candidate.access_code.clone(),
            entries,
            json!({"pass_code_token":token}),
        )))
    }
}
impl ShareResolve for PikPakShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_pikpak_share(self.credentials, self.http, entry)
    }
    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        resolve_pikpak_share_with_context(self.credentials, self.http, entry)
    }
}
impl PikPakShareAdapter<'_> {
    fn saved_share_file_ids(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<HashSet<String>, QuarkError> {
        let client = PikPakClient::new(self.http, self.credentials);
        let root = client.list("0")?;
        let Some(folder) = root
            .items
            .into_iter()
            .find(|item| item.kind == CloudFileKind::Folder && item.name == "Pack From Shared")
        else {
            return Ok(HashSet::new());
        };
        let mut found = HashSet::new();
        for file in client.list(&folder.id)?.items {
            if file.kind != CloudFileKind::File
                || file.name != entry.entry.name
                || entry.entry.size.is_some_and(|size| file.size != Some(size))
            {
                continue;
            }
            let detail = client.request(
                "GET",
                format!("{SHARE_DRIVE}/drive/v1/files/{}?usage=CACHE", encoded(&file.id)),
                None,
            )?;
            if detail["id"].as_str() == Some(&file.id)
                && detail["params"]["original_share_id"].as_str()
                    == Some(&entry.plan.canonical_share_id)
                && detail["params"]["original_file_id"].as_str()
                    == Some(&entry.entry.entry_id)
            {
                found.insert(file.id);
            }
        }
        Ok(found)
    }

    fn personal_download(
        &self,
        file_id: &str,
        name: String,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        let (url, _) = PikPakClient::new(self.http, self.credentials).download(file_id)?;
        super::share::validate_direct_url(&url)?;
        Ok(super::commands::ResolvedCloudDownload {
            transform: None,
            url,
            name,
            headers: pikpak_share_cdn_headers(&self.session()?.headers),
        })
    }
}

impl ShareTransfer for PikPakShareAdapter<'_> {
    fn transfer_capacity(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        let mut session = self.session()?;
        let value = session.request("GET", format!("{SHARE_DRIVE}/drive/v1/about"), None)?;
        CloudTransferCapacity::from_json(&value["quota"]["limit"], &value["quota"]["usage"], false)
    }

    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        let session = self.session()?;
        if session.session.user_id.trim().is_empty() {
            Ok(None)
        } else {
            super::share::stable_owner_fingerprint(
                CloudProvider::Pikpak,
                &session.session.user_id,
            )
            .map(Some)
        }
    }

    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        if entry.provider != CloudProvider::Pikpak || entry.plan.provider != CloudProvider::Pikpak {
            return Err(QuarkError::InvalidResponse);
        }
        let owner = self.transfer_owner_fingerprint(entry)?;
        let before = self.saved_share_file_ids(entry)?;
        let mut session = self.session()?;
        let token = match stored_pikpak_share_pass_token(
            &entry.plan.provider_context,
            entry.access_code(),
        )? {
            Some(token) => token,
            None => pikpak_share_pass_token(
                &session.pass(&entry.plan.canonical_share_id, entry.access_code())?,
                entry.access_code(),
            )?,
        };
        let value = session.request(
            "POST",
            format!("{SHARE_DRIVE}/drive/v1/share/restore"),
            Some(json!({
                "kind": "drive#file",
                "share_id": entry.plan.canonical_share_id,
                "pass_code_token": token,
                "file_ids": [entry.entry.entry_id],
            })),
        )?;
        let task_id = value["restore_task_id"]
            .as_str()
            .or_else(|| value["task_id"].as_str())
            .filter(|id| !id.is_empty())
            .ok_or(QuarkError::InvalidResponse)?;
        for _ in 0..40 {
            let task = session.request(
                "GET",
                format!("{SHARE_DRIVE}/drive/v1/tasks/{}", encoded(task_id)),
                None,
            )?;
            match task["phase"].as_str() {
                Some("PHASE_TYPE_COMPLETE") => {
                    if task["params"]["share_id"].as_str()
                        != Some(&entry.plan.canonical_share_id)
                    {
                        return Err(QuarkError::InvalidResponse);
                    }
                    let mut id = None;
                    for _ in 0..20 {
                        let mut created = self.saved_share_file_ids(entry)?;
                        created.retain(|id| !before.contains(id));
                        match created.len() {
                            1 => {
                                id = created.into_iter().next();
                                break;
                            }
                            0 => std::thread::sleep(std::time::Duration::from_millis(250)),
                            _ => return Err(QuarkError::InvalidResponse),
                        }
                    }
                    let id = id.ok_or(QuarkError::InvalidResponse)?;
                    let source = entry.plan.source_for(&entry.entry.entry_id);
                    if let Some(owner) = &owner {
                        store.record_transfer_with_owner(&source, &id, owner)?;
                    } else {
                        store.record_transfer(&source, &id)?;
                    }
                    return self.personal_download(&id, entry.entry.name.clone());
                }
                Some("PHASE_TYPE_PENDING" | "PHASE_TYPE_RUNNING") => {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                _ => return Err(QuarkError::InvalidResponse),
            }
        }
        Err(QuarkError::InvalidResponse)
    }
}

impl RecordedTransferResolver for PikPakShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        if entry.provider != CloudProvider::Pikpak
            || entry.plan.provider != CloudProvider::Pikpak
            || created_file_id.is_empty()
        {
            return Err(QuarkError::InvalidResponse);
        }
        Ok(ShareOperationResult {
            result: self
                .personal_download(created_file_id, entry.entry.name.clone())
                .map(RecordedTransferDownload::Available),
            updated_context: None,
        })
    }
}

impl CloudShareAdapter for PikPakShareAdapter<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}
pub fn resolve_pikpak_share(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    resolve_pikpak_share_with_context(credentials, http, entry)?.result
}

fn resolve_pikpak_share_with_context(
    credentials: &dyn CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
    if entry.provider != CloudProvider::Pikpak || entry.plan.provider != CloudProvider::Pikpak {
        return Err(QuarkError::InvalidResponse);
    }
    let adapter = PikPakShareAdapter { credentials, http };
    let mut session = adapter.session()?;
    let mut context = entry.plan.provider_context.clone();
    let mut rotated_context = false;
    let mut token = match stored_pikpak_share_pass_token(&context, entry.access_code())? {
        Some(token) => token,
        None => {
            let pass = session.pass(&entry.plan.canonical_share_id, entry.access_code())?;
            let token = pikpak_share_pass_token(&pass, entry.access_code())?;
            context = with_pikpak_share_pass_token(context, &token);
            rotated_context = true;
            token
        }
    };
    let mut value = session.request(
        "GET",
        format!(
            "{SHARE_DRIVE}/drive/v1/share/file_info?share_id={}&file_id={}&pass_code_token={}",
            encoded(&entry.plan.canonical_share_id),
            encoded(&entry.entry.entry_id),
            encoded(&token)
        ),
        None,
    )?;
    if !rotated_context && pikpak_share_token_needs_refresh(&value) {
        let pass = session.pass(&entry.plan.canonical_share_id, entry.access_code())?;
        token = pikpak_share_pass_token(&pass, entry.access_code())?;
        context = with_pikpak_share_pass_token(context, &token);
        rotated_context = true;
        value = session.request(
            "GET",
            format!(
                "{SHARE_DRIVE}/drive/v1/share/file_info?share_id={}&file_id={}&pass_code_token={}",
                encoded(&entry.plan.canonical_share_id),
                encoded(&entry.entry.entry_id),
                encoded(&token)
            ),
            None,
        )?;
    }
    let result = (|| {
        pikpak_share_status(&value)?;
        let file = &value["file_info"];
        if file["id"].as_str() != Some(entry.entry.entry_id.as_str())
            || file["kind"] != "drive#file"
        {
            return Err(QuarkError::InvalidResponse);
        }
        // Media links are playback previews, not full-file downloads: PikPak
        // can return 416 partway through the original media despite a valid
        // full-file Content-Range total. Use a personal copy when the complete
        // download link is absent.
        let url = match file.get("web_content_link") {
            Some(Value::String(url)) if !url.is_empty() => Some(url.as_str()),
            None | Some(Value::Null | Value::String(_)) => None,
            Some(_) => return Err(QuarkError::InvalidResponse),
        };
        let Some(url) = url else {
            return Ok(ShareResolution::TransferRequired(TransferRequest {
                source: entry.plan.source_for(&entry.entry.entry_id),
            }));
        };
        super::share::validate_direct_url(&url)?;
        Ok(ShareResolution::Direct(
            super::commands::ResolvedCloudDownload {
                transform: None,
                url: url.to_owned(),
                name: entry.entry.name.clone(),
                headers: pikpak_share_cdn_headers(&session.headers),
            },
        ))
    })();
    Ok(ShareOperationResult {
        result,
        updated_context: rotated_context.then_some(context),
    })
}
fn pikpak_share_cdn_headers(headers: &BTreeMap<String, String>) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            ![
                "Authorization",
                "X-Client-Id",
                "X-Device-Id",
                "X-Captcha-Token",
            ]
            .iter()
            .any(|private| name.eq_ignore_ascii_case(private))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}
pub fn parse_pikpak_file(v: &Value, parent: &str) -> Result<CloudFile, QuarkError> {
    Ok(CloudFile {
        id: required(v, "id")?,
        name: required(v, "name")?,
        parent_id: parent.into(),
        kind: if v["kind"] == "drive#folder" {
            "folder"
        } else {
            "file"
        }
        .into(),
        size: v["size"]
            .as_u64()
            .or_else(|| v["size"].as_str().and_then(|s| s.parse().ok())),
        modified_at: v["modified_time"].as_str().map(str::to_owned),
    })
}
