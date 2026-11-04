//! Xunlei authentication plugin. Observe the official browser protocol,
//! validate the candidate with the drive client, then persist atomically.
use super::*;

pub(crate) const INITIALIZE: &str = include_str!("xunlei_auth/observer.js");
pub(crate) const PASSWORD: &str = include_str!("xunlei_auth/password.js");
pub(crate) const SNAPSHOT: &str = r#"(() => {
  if (location.origin !== 'https://pan.xunlei.com') return {pending:true};
  return window.__versadownXunleiAuth?.snapshot() || {authorization_error:'plugin_update_required'};
})()"#;

pub(super) fn complete(
    input: &WebAuthorizationInput,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
) -> Result<CloudAccount, CloudError> {
    let value = &input.storage;
    if value["authorization_error"].as_str() == Some("plugin_update_required") {
        return Err(CloudError::Provider("迅雷云盘插件需要更新".into()));
    }
    if value["pending"] == true || value.is_null() {
        return Err(CloudError::AuthorizationPending);
    }
    if value["schema"] != 1 {
        return Err(CloudError::Provider("迅雷云盘插件需要更新".into()));
    }
    let field = |name: &str| required(value, name).map_err(|_| CloudError::InvalidResponse);
    let profile = xunlei::XunleiClientProfile {
        client_id: field("client_id")?,
        device_id: field("device_id")?,
        client_version: field("client_version")?,
        package_name: field("package_name")?,
        user_agent: field("user_agent")?,
        client_secret: String::new(),
    };
    if !profile.is_modern_web() || !profile.is_drive_complete() {
        return Err(CloudError::Provider("迅雷云盘插件需要更新".into()));
    }
    let mut session = xunlei::XunleiSession::from_web_authorization(
        field("access_token")?,
        value["refresh_token"].as_str().unwrap_or_default(),
        field("user_id")?,
        "",
        profile,
        value["expires_at"]
            .as_u64()
            .ok_or(CloudError::InvalidResponse)?,
    )?;
    session.web_security = serde_json::from_value(value["web_security"].clone())
        .map_err(|_| CloudError::InvalidResponse)?;
    session.auth_method = input.auth_method;
    let client = xunlei::XunleiClient::new(session, http);
    client.directory("0")?;
    let account = client.web_account()?;
    save_provider_session(CloudProvider::Xunlei, store, &client.session())?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Root {
        reject: bool,
    }
    impl CloudHttpTransport for Root {
        fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
            assert_eq!(r.method, "GET");
            let shares = r.url.starts_with("https://api-pan.xunlei.com/drive/v1/share/list?");
            assert!(shares || r.url.starts_with("https://api-pan.xunlei.com/drive/v1/files?"));
            assert_eq!(r.headers["Authorization"], "Bearer official-access");
            assert_eq!(r.headers["X-Captcha-Token"], "official-ticket");
            Ok(CloudHttpResponse {
                status: if self.reject { 401 } else { 200 },
                body: if self.reject {
                    r#"{"error":"unauthenticated"}"#
                } else if shares {
                    r#"{"data":[]}"#
                } else {
                    r#"{"files":[]}"#
                }
                .into(),
                ..Default::default()
            })
        }
    }
    fn candidate(method: CloudAuthMethod) -> WebAuthorizationInput {
        WebAuthorizationInput {
            auth_method: method,
            storage: serde_json::json!({
                "schema":1,"user_id":"user","access_token":"official-access","expires_at":4_000_000_000u64,
                "client_id":"Xqp0kJBXWhwaTpB6","device_id":"official-device","client_version":"future-version",
                "package_name":"pan.xunlei.com","user_agent":"Official browser",
                "web_security":{"captcha_token":"official-ticket","expires_at":4_000_000_000u64,"action":"GET:/drive/v1/files",
                    "captcha_sign":"synthetic-signature","timestamp":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis().to_string(),
                    "client_version":"future-version","package_name":"pan.xunlei.com"}
            }),
            ..Default::default()
        }
    }
    #[test]
    fn both_login_methods_validate_root_before_saving() {
        for method in [CloudAuthMethod::Web, CloudAuthMethod::Password] {
            let store = MemoryCredentialStore::default();
            let account = complete(&candidate(method), &store, &Root { reject: false }).unwrap();
            assert_eq!(account.auth_method, method);
            assert_eq!(account.id, "user");
            assert!(store.load("xunlei").unwrap().is_some());
            let restored = ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &store, &Root { reject: false })
                .account()
                .unwrap();
            assert_eq!(restored.auth_method, method);
        }
    }
    #[test]
    fn valid_root_ticket_completes_both_login_methods_without_signing_metadata() {
        for method in [CloudAuthMethod::Web, CloudAuthMethod::Password] {
            let store = MemoryCredentialStore::default();
            let mut input = candidate(method);
            input.storage["web_security"]["captcha_sign"] = "".into();
            let http = Root { reject: false };

            let account = complete(&input, &store, &http).unwrap();
            assert_eq!(account.auth_method, method);
            let mut driver = ProviderRegistry::global().driver(CloudProvider::Xunlei, &store, &http);
            assert!(driver.list_shares().unwrap().is_empty());
        }
    }
    #[test]
    fn rejected_root_preserves_previous_account() {
        let store = MemoryCredentialStore::default();
        store.save("xunlei", b"previous").unwrap();
        assert!(complete(
            &candidate(CloudAuthMethod::Web),
            &store,
            &Root { reject: true }
        )
        .is_err());
        assert_eq!(store.load("xunlei").unwrap().unwrap(), b"previous");
    }
    #[test]
    fn incompatible_capture_has_the_requested_message() {
        let input = WebAuthorizationInput {
            storage: serde_json::json!({"authorization_error":"plugin_update_required"}),
            ..Default::default()
        };
        assert!(
            matches!(complete(&input,&MemoryCredentialStore::default(),&Root{reject:false}),Err(CloudError::Provider(m)) if m=="迅雷云盘插件需要更新")
        );
    }
}
