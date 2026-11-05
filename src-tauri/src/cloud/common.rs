//! Shared protocol and credential helpers. This module has no Tauri or browser host dependency.
use super::types::required;
use super::*;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashSet;

pub(crate) fn cookie_for_download(web_cookie: &str, response_cookies: &[String]) -> String {
    let mut cookies: Vec<(String, String)> = web_cookie
        .split(';')
        .filter_map(cookie_pair)
        .map(|(name, pair)| (name.to_owned(), pair.to_owned()))
        .collect();
    for response_cookie in response_cookies {
        if let Some((name, pair)) = cookie_pair(response_cookie) {
            cookies.retain(|(current, _)| current != name);
            if !cookie_is_expired(response_cookie) {
                cookies.push((name.to_owned(), pair.to_owned()));
            }
        }
    }
    cookies
        .into_iter()
        .map(|(_, pair)| pair)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether a Set-Cookie value is a deletion instruction (`Max-Age<=0` or an
/// `Expires` date in the past). `Max-Age` takes precedence over `Expires`.
fn cookie_is_expired(set_cookie: &str) -> bool {
    let attribute = |wanted: &str| {
        set_cookie.split(';').skip(1).find_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case(wanted)
                .then(|| value.trim())
        })
    };
    if let Some(max_age) = attribute("max-age") {
        if let Ok(seconds) = max_age.parse::<i64>() {
            return seconds <= 0;
        }
    }
    attribute("expires")
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .is_some_and(|expires| expires <= std::time::SystemTime::now())
}

pub(crate) fn cookie_pair(value: &str) -> Option<(&str, &str)> {
    let pair = value.split(';').next()?.trim();
    let (name, _) = pair.split_once('=')?;
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        && !pair.contains(['\r', '\n']))
    .then_some((name, pair))
}

pub(crate) fn with_xunlei_client<T, H: CloudHttpTransport>(
    store: &impl CredentialStore,
    http: &H,
    work: impl FnOnce(&xunlei::XunleiClient<'_, H>) -> Result<T, QuarkError>,
) -> Result<T, QuarkError> {
    let session = load_provider_session(CloudProvider::Xunlei, store)?;
    let client = xunlei::XunleiClient::new(session, http);
    let client = if store.automatic_session_recovery() {
        client
    } else {
        client.supervised()
    };
    let result = work(&client);
    save_provider_session(CloudProvider::Xunlei, store, &client.session())?;
    result
}

pub(crate) fn validate_recycle_entry_ids(
    file_ids: &[String],
    recycle_bin: &CloudDirectory,
) -> Result<(), QuarkError> {
    if file_ids.is_empty() {
        return Err(QuarkError::InvalidInput("请选择至少一个回收站项目".into()));
    }
    let entries = recycle_bin
        .items
        .iter()
        .filter(|item| item.parent_id == recycle_bin.id)
        .map(|item| item.id.as_str())
        .collect::<HashSet<_>>();
    if file_ids
        .iter()
        .any(|id| id.trim().is_empty() || !entries.contains(id.as_str()))
    {
        return Err(QuarkError::InvalidInput(
            "只能操作当前网盘回收站中的项目".into(),
        ));
    }
    Ok(())
}

pub(crate) fn is_provider_recycle_bin(provider: CloudProvider, directory_id: &str) -> bool {
    directory_id
        == crate::cloud::ProviderRegistry::global()
            .get(provider)
            .recycle_bin_directory_id
}

pub(crate) fn find_created_directory(
    directory: CloudDirectory,
    name: &str,
    previous_ids: &HashSet<String>,
) -> Result<CloudFile, QuarkError> {
    let mut created = directory.items.into_iter().filter(|file| {
        file.kind == "folder" && file.name == name && !previous_ids.contains(&file.id)
    });
    let result = created.next().ok_or(QuarkError::InvalidResponse)?;
    if created.next().is_some() {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(result)
}

#[cfg(test)]
mod created_directory_tests {
    use super::*;

    fn folder(id: &str, name: &str) -> CloudFile {
        CloudFile {
            id: id.into(),
            parent_id: "parent".into(),
            name: name.into(),
            kind: "folder".into(),
            size: None,
            modified_at: None,
        }
    }

    #[test]
    fn created_directory_is_the_only_new_matching_id() {
        let before = HashSet::from(["existing".to_owned()]);
        let directory = CloudDirectory {
            id: "parent".into(),
            items: vec![folder("existing", "photos"), folder("created", "photos")],
        };

        assert_eq!(
            find_created_directory(directory, "photos", &before)
                .unwrap()
                .id,
            "created"
        );
    }

    #[test]
    fn created_directory_rejects_ambiguous_new_matching_ids() {
        let directory = CloudDirectory {
            id: "parent".into(),
            items: vec![folder("created-a", "photos"), folder("created-b", "photos")],
        };

        assert!(matches!(
            find_created_directory(directory, "photos", &HashSet::new()),
            Err(QuarkError::InvalidResponse)
        ));
    }
}

pub(crate) fn save_provider_session<T: Serialize>(
    provider: CloudProvider,
    store: &impl CredentialStore,
    session: &T,
) -> Result<(), QuarkError> {
    let bytes = serde_json::to_vec(session).map_err(|_| QuarkError::Storage)?;
    store.save(provider.key(), &bytes)
}

pub(crate) fn load_provider_session<T: DeserializeOwned>(
    provider: CloudProvider,
    store: &impl CredentialStore,
) -> Result<T, QuarkError> {
    let bytes = store
        .load(provider.key())?
        .ok_or(QuarkError::NotAuthorized)?;
    serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)
}

#[cfg(test)]
pub(crate) fn run_cloud_work<T>(
    provider: CloudProvider,
    kind: SessionWorkKind,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    work: impl FnMut() -> Result<T, QuarkError>,
) -> Result<T, QuarkError> {
    run_with_session_recovery(provider, store, kind, work, |credential| {
        recover_provider_password_session(provider, store, http, credential)
    })
}

pub(crate) fn cookie(store: &impl CredentialStore) -> Result<String, QuarkError> {
    String::from_utf8(
        store
            .load("quark")?
            .ok_or(QuarkError::AuthorizationExpired)?,
    )
    .map_err(|_| QuarkError::Storage)
}

pub(crate) fn finalize_uc_web_authorization(
    descriptor: &ProviderDescriptor,
    mut session: uc::UcSession,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
) -> Result<CloudAccount, QuarkError> {
    let client = uc::UcClient::new(session, http);
    // A cookie pair only says that the WebView completed part of its flow.
    // Verify the exact first read the workspace needs before replacing the
    // stored account and closing the official authorization window.
    authorization_result(client.directory(descriptor.root_directory_id))?;
    // The official UC page captures the nickname before this call, but the
    // directory read remains the authorization proof. A page which has not
    // populated its profile yet must still preserve a verified login.
    let account_id = client.verified_account_id();
    session = client.session();
    let account = match account_id {
        Ok(Some(account_id)) => {
            session.account_id = Some(account_id);
            uc::UcClient::new(session.clone(), http).web_account()?
        }
        // The root listing already proved the official browser session is
        // usable. Keep that login when UC temporarily withholds the member
        // profile; the next successful account read will upgrade its binding.
        Ok(None) | Err(_) => client.web_account()?,
    };
    session.display_name =
        (account.display_name != uc::UC_CARD_DISPLAY_NAME).then(|| account.display_name.clone());
    save_provider_session(CloudProvider::Uc, store, &session)?;
    Ok(account)
}

pub(crate) fn finalize_tianyi_web_authorization(
    descriptor: &ProviderDescriptor,
    session: tianyi::TianyiSession,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
) -> Result<CloudAccount, QuarkError> {
    let mut web_candidate = session.clone();
    web_candidate.app = None;
    web_candidate.auth_method = CloudAuthMethod::Web;
    let web_client = tianyi::TianyiClient::new(web_candidate, http);
    // The web window owns login, but a candidate session must prove it can
    // read the drive before replacing a previously usable stored session.
    // Validate with App credentials removed: a valid signed App read must not
    // mask an InvalidSessionKey from the new browser cookie and close the
    // authorization window with a session that cannot perform web writes.
    web_client
        .directory(descriptor.root_directory_id)
        .map_err(|error| match error {
            QuarkError::AuthorizationExpired
            | QuarkError::WebAuthorizationRequired
            | QuarkError::CaptchaRequired => QuarkError::AuthorizationPending,
            other => other,
        })?;
    let web_account = if session.app.is_some() {
        Some(web_client.account().map_err(|error| match error {
            QuarkError::AuthorizationExpired
            | QuarkError::WebAuthorizationRequired
            | QuarkError::CaptchaRequired => QuarkError::AuthorizationPending,
            other => other,
        })?)
    } else {
        None
    };
    let client = tianyi::TianyiClient::new(session.clone(), http);
    let account = if session.app.is_some() {
        client.account()?
    } else {
        client.web_account()?
    };
    if web_account
        .as_ref()
        .is_some_and(|web_account| web_account.id != account.id)
    {
        return Err(QuarkError::AuthorizationPending);
    }
    // `tianyi-web` is deliberately only a generic browser identity. It must
    // never be fed to the password-account selector because that would erase
    // a saved password simply because a Web-only session cannot expose the
    // stable account id. A merged App+Web session does have that stable id.
    if account.id != "tianyi-web" {
        select_password_credential_account(store, CloudProvider::Tianyi, &account.id)?;
    }
    save_provider_session(CloudProvider::Tianyi, store, &session)?;
    Ok(account)
}

pub(crate) fn finalize_baidu_web_authorization(
    descriptor: &ProviderDescriptor,
    session: baidu::BaiduSession,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
) -> Result<CloudAccount, QuarkError> {
    let client = baidu::BaiduClient::new(session.clone(), http);
    let account = authorization_result(client.account())?;
    // The UI loads the root directory immediately after it accepts this
    // account. Do not close the official browser window or replace a saved
    // session until that exact request accepts the candidate cookie jar.
    // Authentication and verification failures map to AuthorizationPending,
    // so frontend polling keeps the window open for the user to finish the
    // official flow instead of creating an immediately-expired tab.
    authorization_result(client.directory(descriptor.root_directory_id))?;
    select_password_credential_account(store, CloudProvider::Baidu, &account.id)?;
    save_provider_session(CloudProvider::Baidu, store, &session)?;
    Ok(account)
}

pub fn authorization_result<T>(result: Result<T, QuarkError>) -> Result<T, QuarkError> {
    result.map_err(|e| match e {
        QuarkError::AuthorizationExpired
        | QuarkError::WebAuthorizationRequired
        | QuarkError::CaptchaRequired => QuarkError::AuthorizationPending,
        other => other,
    })
}

pub fn parse_web_tokens(raw: &str) -> Result<PikPakSession, QuarkError> {
    let mut v: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| QuarkError::AuthorizationPending)?;
    if let Some(s) = v.as_str() {
        v = serde_json::from_str(s).map_err(|_| QuarkError::AuthorizationPending)?;
    }
    Ok(PikPakSession {
        access_token: required(&v, "access_token").map_err(|_| QuarkError::AuthorizationPending)?,
        refresh_token: required(&v, "refresh_token")
            .map_err(|_| QuarkError::AuthorizationPending)?,
        expires_at: v["expires_at"].as_u64().unwrap_or(u64::MAX),
        device_id: v["device_id"].as_str().unwrap_or("").into(),
        captcha_token: v["captcha_token"].as_str().unwrap_or("").into(),
        client_id: v["client_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned),
        user_id: v["user_id"].as_str().unwrap_or("").into(),
        captcha_expires_at: 0,
        captcha_action: None,
        verification_url: None,
        operation_verifications: Default::default(),
        auth_method: CloudAuthMethod::Web,
    })
}

pub(crate) fn parse_aliyun_web_session(
    value: serde_json::Value,
) -> Result<aliyun::AliyunSession, QuarkError> {
    let required_field = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| value.get(*name).and_then(serde_json::Value::as_str))
            .filter(|field| !field.is_empty())
            .map(str::to_owned)
            .ok_or(QuarkError::AuthorizationPending)
    };
    Ok(aliyun::AliyunSession {
        access_token: required_field(&["access_token", "accessToken"])?,
        refresh_token: required_field(&["refresh_token", "refreshToken"])?,
        drive_id: required_field(&["default_drive_id", "defaultDriveId", "drive_id", "driveId"])?,
        browser_access_ready: false,
        expires_at: value["expires_at"].as_u64(),
    })
}

#[cfg(test)]
pub(crate) fn recover_provider_password_session(
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    credential: &CloudPasswordCredential,
) -> Result<(), QuarkError> {
    if credential.provider() != provider {
        return Err(QuarkError::Storage);
    }
    if browser_password_login_preferred(store, provider)? {
        return Err(QuarkError::WebAuthorizationRequired);
    }

    let (account, session) = ProviderRegistry::global()
        .driver(provider, store, http)
        .auth()
        .recover_password(credential)?;

    if account.id.is_empty() || account.id != credential.account_id() {
        // Baidu intentionally has one account slot. Selecting a different
        // browser account discards the old password instead of retaining an
        // inactive multi-account credential.
        if !account.id.is_empty() {
            select_password_credential_account(store, provider, &account.id)?;
        }
        return Err(QuarkError::WebAuthorizationRequired);
    }
    store.save(provider.key(), &session)?;
    Ok(())
}
