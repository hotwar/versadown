use super::*;

pub(crate) struct TianyiBrowserValidationTransport<'a, T: CloudHttpTransport> {
    pub(crate) window: &'a tauri::WebviewWindow,
    pub(crate) native: &'a T,
}

impl<T: CloudHttpTransport> CloudHttpTransport for TianyiBrowserValidationTransport<'_, T> {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        if tianyi::accepts_browser_api_request(&request.url) {
            tianyi_browser_send(self.window, request)
        } else {
            self.native.send(request)
        }
    }
}

pub(crate) fn password_webview_data_directory_from_root(
    root: &Path,
    provider: CloudProvider,
) -> Result<PathBuf, QuarkError> {
    if !uses_password_browser_profile(provider) {
        return Err(QuarkError::InvalidInput(
            "该网盘不支持持久化账号密码登录".into(),
        ));
    }
    Ok(root.join("web-auth").join(provider.key()))
}

pub(crate) fn password_webview_data_directory(
    app: &AppHandle,
    provider: CloudProvider,
) -> Result<PathBuf, QuarkError> {
    let root = if std::env::var("VERSADOWN_BACKGROUND_UI_TEST")
        .ok()
        .as_deref()
        == Some("1")
    {
        std::env::var_os("VERSADOWN_TEST_DATA_DIR")
            .map(PathBuf::from)
            .ok_or(QuarkError::Storage)?
    } else {
        app.path().app_data_dir().map_err(|_| QuarkError::Storage)?
    };
    password_webview_data_directory_from_root(&root, provider)
}

pub(crate) fn password_login_url(provider: CloudProvider) -> &'static str {
    ProviderRegistry::global().get(provider).authorization_url
}

pub(crate) fn prepare_baidu_account_login(
    store: &impl CredentialStore,
    submitted_username: &str,
) -> Result<bool, QuarkError> {
    let submitted_username = submitted_username.trim();
    if let Some(owner) = baidu_browser_profile_username(store)? {
        return Ok(owner.trim() != submitted_username);
    }
    // A session captured by manual web authorization proves that a Baidu
    // profile exists, but not that it belongs to the submitted username. On
    // the first password login after upgrading, clear that unbound profile
    // once instead of accepting its already logged-in account.
    if store.load(CloudProvider::Baidu.key())?.is_some() {
        return Ok(true);
    }
    let previous_username =
        if let Some(credential) = load_password_credential(store, CloudProvider::Baidu)? {
            Some(credential.username().to_owned())
        } else {
            load_password_login_draft(store, CloudProvider::Baidu)?
                .map(|draft| draft.username().to_owned())
        };
    Ok(previous_username.is_some_and(|username| username.trim() != submitted_username))
}

pub(crate) fn clear_password_webview_profile(
    app: &AppHandle,
    provider: CloudProvider,
) -> Result<(), QuarkError> {
    for label in [
        format!("cloud-auth-{}", provider.key()),
        format!("cloud-password-{}", provider.key()),
    ] {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
    let label = format!("cloud-password-reset-{}", provider.key());
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.close();
    }
    let window = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::External(
            "about:blank"
                .parse()
                .map_err(|_| QuarkError::InvalidResponse)?,
        ),
    )
    .visible(false)
    .data_directory(password_webview_data_directory(app, provider)?)
    .build()
    .map_err(|_| QuarkError::Storage)?;
    let result = window
        .clear_all_browsing_data()
        .map_err(|_| QuarkError::Storage);
    let _ = window.close();
    result
}

pub(crate) fn tianyi_browser_request_script(
    request: &CloudHttpRequest,
) -> Result<String, QuarkError> {
    let url = validate_tianyi_browser_request(request.method, &request.url)?;
    let origin =
        serde_json::to_string(tianyi::BROWSER_ORIGIN).map_err(|_| QuarkError::InvalidResponse)?;
    let payload = serde_json::json!({
        "method": request.method,
        "url": url.as_str(),
        "headers": tianyi_browser_request_headers(request.headers.clone()),
        "body": request.body,
    });
    Ok(format!(
        r#"(() => {{
          if (location.origin !== {origin}) return {{done:false}};
          const key = '__versadown_tianyi_request';
          const current = window[key];
          if (current?.done) {{
            delete window[key];
            return {{done:true, value:current.value}};
          }}
          if (!current) {{
            const input = {payload};
            const slot = {{done:false}};
            window[key] = slot;
            fetch(input.url, {{
              method: input.method,
              headers: input.headers,
              body: input.body == null ? undefined : input.body,
              credentials: 'include',
              redirect: 'follow',
            }}).then(async response => {{
              slot.value = {{status:response.status, body:await response.text()}};
              slot.done = true;
            }}, () => {{
              slot.value = {{transportError:true}};
              slot.done = true;
            }});
          }}
          return {{done:false}};
        }})()"#
    ))
}

pub(crate) fn tianyi_browser_send(
    window: &tauri::WebviewWindow,
    request: CloudHttpRequest,
) -> Result<CloudHttpResponse, QuarkError> {
    let script = tianyi_browser_request_script(&request)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    while std::time::Instant::now() < deadline {
        if window
            .app_handle()
            .get_webview_window(window.label())
            .is_none()
        {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        match web_storage_tokens(window, &script) {
            Ok(state) if state["done"].as_bool() == Some(true) => {
                let value = state.get("value").ok_or(QuarkError::InvalidResponse)?;
                if value["transportError"].as_bool() == Some(true) {
                    return Err(QuarkError::Transport("天翼官网授权页请求失败".into()));
                }
                let status = value["status"]
                    .as_u64()
                    .and_then(|status| u16::try_from(status).ok())
                    .ok_or(QuarkError::InvalidResponse)?;
                let body = value["body"]
                    .as_str()
                    .ok_or(QuarkError::InvalidResponse)?
                    .to_owned();
                return Ok(CloudHttpResponse {
                    status,
                    body,
                    set_cookies: Vec::new(),
                });
            }
            Ok(_) | Err(QuarkError::AuthorizationPending) => {}
            Err(error) => return Err(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(QuarkError::WebAuthorizationRequired)
}

#[cfg(test)]
pub(crate) fn should_attempt_native_password_recovery(
    provider: CloudProvider,
    kind: SessionWorkKind,
    error: &QuarkError,
) -> bool {
    kind == SessionWorkKind::Read
        && browser_login::supports_browser_password(provider)
        && matches!(
            error,
            QuarkError::AuthorizationExpired
                | QuarkError::NotAuthorized
                | QuarkError::WebAuthorizationRequired
                | QuarkError::CaptchaRequired
        )
}

pub(crate) fn recover_authorization(
    app: &AppHandle,
    state: &CloudState,
    provider: CloudProvider,
    http: &impl CloudHttpTransport,
    genuine_auth_error: impl Into<auth_supervisor::RecoveryPhase>,
) -> Result<(), QuarkError> {
    auth_supervisor::recover_in_order(
        provider,
        &state.store,
        ProviderRegistry::global()
            .driver(provider, &state.store, http)
            .auth(),
        genuine_auth_error,
        || capture_persistent_authorization(app, state, provider, http),
    )
}

pub(crate) fn authorization_profile_directory(
    app: &AppHandle,
    provider: CloudProvider,
) -> Result<PathBuf, CloudError> {
    if browser_login::supports_browser_password(provider) {
        return password_webview_data_directory(app, provider);
    }
    let root = if std::env::var("VERSADOWN_BACKGROUND_UI_TEST")
        .ok()
        .as_deref()
        == Some("1")
    {
        std::env::var_os("VERSADOWN_TEST_DATA_DIR")
            .map(PathBuf::from)
            .ok_or(CloudError::Storage)?
    } else {
        app.path().app_data_dir().map_err(|_| CloudError::Storage)?
    };
    Ok(root.join("web-auth").join(provider.key()))
}

/// A single bounded capture from the stable profile, only after an auth error.
/// Candidate validation takes place in an isolated store before replacing secrets.
fn capture_persistent_authorization(
    app: &AppHandle,
    state: &CloudState,
    provider: CloudProvider,
    http: &impl CloudHttpTransport,
) -> Result<bool, CloudError> {
    if auth_supervisor::is_manually_disconnected(&state.store, provider)? {
        return Err(CloudError::NotAuthorized);
    }
    let record = state.auth_supervisor.record(&state.store, provider)?;
    let expected = record.account_id.or(
        if cloud_session_policy(provider).supports_password_fallback {
            load_password_credential(&state.store, provider)?
                .map(|credential| credential.account_id().to_owned())
        } else {
            None
        },
    );
    let Some(expected) = expected.filter(|id| !id.is_empty()) else {
        return Ok(false);
    };
    let label = format!("cloud-maintenance-{}", provider.key());
    let (loaded, ready) = std::sync::mpsc::sync_channel(1);
    let mut builder = WebviewWindowBuilder::new(
        app,
        &label,
        WebviewUrl::External(
            ProviderRegistry::global()
                .get(provider)
                .authorization_url
                .parse()
                .map_err(|_| CloudError::InvalidResponse)?,
        ),
    )
    .visible(false)
    .data_directory(authorization_profile_directory(app, provider)?)
    .on_page_load(move |_, payload| {
        if payload.event() == PageLoadEvent::Finished {
            let _ = loaded.try_send(());
        }
    });
    if ProviderRegistry::global().get(provider).browser_context {
        builder = builder.initialization_script(include_str!("../tianyi-browser-context.js"));
    }
    if let Some(script) = ProviderRegistry::global()
        .get(provider)
        .web_initialization_script
    {
        builder = builder.initialization_script(script);
    }
    let window = builder
        .build()
        .map_err(|_| CloudError::WebAuthorizationRequired)?;
    let result = (|| {
        ready
            .recv_timeout(std::time::Duration::from_secs(15))
            .map_err(|_| CloudError::WebAuthorizationRequired)?;
        let temporary = MemoryCredentialStore::default();
        // Tianyi's browser candidate and signed App credentials are separate
        // transports for the same account. Seed only the isolated validation
        // store so the driver can merge them and prove the stable account id;
        // the real store remains untouched until every check below succeeds.
        if provider == CloudProvider::Tianyi {
            if let Some(existing) = state.store.load(provider.key())? {
                temporary.save(provider.key(), &existing)?;
            }
        }
        let mut driver = ProviderRegistry::global().driver(provider, &temporary, http);
        let recipe = driver.auth().web_authorization_recipe();
        let input = crate::cloud::driver::WebAuthorizationInput {
            cookies: browser_cookies(&window, web_session_cookie_urls(provider))?,
            storage: match recipe.token_script {
                Some(script) => {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                    loop {
                        let snapshot = web_storage_tokens(&window, script)?;
                        if snapshot["pending"] != true || std::time::Instant::now() >= deadline {
                            break snapshot;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                }
                None => serde_json::Value::Null,
            },
            browser_context: if recipe.browser_context {
                Some(tianyi_browser_context(&window)?)
            } else {
                None
            },
            auth_method: CloudAuthMethod::Web,
        };
        let account = if recipe.browser_context {
            ProviderRegistry::global()
                .driver(
                    provider,
                    &temporary,
                    &TianyiBrowserValidationTransport {
                        window: &window,
                        native: http,
                    },
                )
                .auth()
                .complete_web(&input)?
        } else {
            driver.auth().complete_web(&input)?
        };
        if account.id != expected {
            return Err(CloudError::WebAuthorizationRequired);
        }
        let mut verified = temporary.load(provider.key())?.ok_or(CloudError::Storage)?;
        if provider == CloudProvider::Tianyi {
            let mut candidate: tianyi::TianyiSession =
                serde_json::from_slice(&verified).map_err(|_| CloudError::Storage)?;
            if let Some(existing) = state
                .store
                .load(provider.key())?
                .and_then(|bytes| serde_json::from_slice::<tianyi::TianyiSession>(&bytes).ok())
            {
                candidate.retain_app_transport(&existing);
                verified = serde_json::to_vec(&candidate).map_err(|_| CloudError::Storage)?;
            }
        }
        state.store.save(provider.key(), &verified)?;
        Ok(true)
    })();
    let _ = window.close();
    result
}

/// Runs on a provider's blocking worker. Waiting for the official browser must
/// never hold up the Windows message loop. The provider lock coalesces renewal
/// across concurrent downloads: later readers see the newly saved session.
pub(crate) fn browser_auth_diagnostic_script(stage: &str, reason: &str) -> String {
    let payload = serde_json::json!({ "stage": stage, "reason": reason });
    format!("window.__versadownAuthDiagnostic = {payload};")
}

pub(crate) fn validate_tianyi_browser_request(
    method: &str,
    raw_url: &str,
) -> Result<url::Url, QuarkError> {
    tianyi::validate_browser_api_request(method, raw_url)
}

pub(crate) fn tianyi_browser_request_headers(
    headers: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    headers
        .into_iter()
        .filter(|(name, value)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "accept" | "content-type" | "browser-id"
            ) && !value.chars().any(char::is_control)
                && value.len() <= 1024
        })
        .collect()
}

pub(crate) fn browser_password_login(
    app: &AppHandle,
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    username: &str,
    password: &str,
    expected_account: Option<&str>,
    challenge: bool,
) -> Result<CloudAccount, QuarkError> {
    if !browser_login::supports_browser_password(provider) {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    let label = format!("cloud-password-{}", provider.key());
    if app.get_webview_window(&label).is_some() {
        return Err(QuarkError::AuthorizationPending);
    }
    let script = Arc::new(browser_login::password_script(
        provider, username, password,
    )?);
    let background_ui_test = std::env::var("VERSADOWN_BACKGROUND_UI_TEST")
        .ok()
        .as_deref()
        == Some("1");
    // The hidden live verifier can opt into an off-screen, non-focusable
    // render target when a provider's own verification SDK rejects hidden
    // documents. This is test-only and never changes production windows.
    let background_ui_test_visible = background_ui_test
        && std::env::var("VERSADOWN_BACKGROUND_UI_TEST_VISIBLE")
            .ok()
            .as_deref()
            == Some("1");
    let url = password_login_url(provider);
    let mut builder = WebviewWindowBuilder::new(
        app,
        &label,
        WebviewUrl::External(url.parse().map_err(|_| QuarkError::InvalidResponse)?),
    )
    .title("网盘登录验证：请按官方页面提示完成")
    .inner_size(1024.0, 760.0)
    .visible((challenge && !background_ui_test) || background_ui_test_visible)
    // Keep one provider profile so Baidu/Tianyi can reuse the official page's
    // stable device context. Explicit account replacement clears this profile
    // before this window is built; validated sessions and passwords remain
    // persisted separately in the system credential store.
    .initialization_script_for_all_frames(script.as_str())
    .on_new_window(|_, _| NewWindowResponse::Deny)
    .on_page_load(move |window, payload| {
        if payload.event() == PageLoadEvent::Finished
            && browser_login::trusted_login_url(provider, payload.url())
        {
            if ProviderRegistry::global()
                .get(provider)
                .browser_profile_owner
                && is_baidu_authorization_page(payload.url())
            {
                let _ = window.eval(baidu::BAIDU_AUTO_OPEN_LOGIN_JS);
            }
        }
    });
    if background_ui_test_visible {
        builder = builder.skip_taskbar(true).position(-10_000.0, -10_000.0);
    }
    builder = builder.data_directory(password_webview_data_directory(app, provider)?);
    if ProviderRegistry::global().get(provider).browser_context {
        builder = builder.initialization_script(include_str!("../tianyi-browser-context.js"));
    }
    if let Some(script) = ProviderRegistry::global()
        .get(provider)
        .web_initialization_script
    {
        builder = builder.initialization_script(script);
    }
    let window = builder.build().map_err(|_| {
        if background_ui_test {
            QuarkError::Provider("后台验证未能创建网盘登录窗口".into())
        } else {
            QuarkError::WebAuthorizationRequired
        }
    })?;
    let started = std::time::Instant::now();
    let result = (|| {
        let mut shown = challenge || background_ui_test;
        let mut last_notice = String::new();
        while started.elapsed() < std::time::Duration::from_secs(180) {
            if app.get_webview_window(&label).is_none() {
                return Err(if background_ui_test {
                    QuarkError::Provider("后台验证中的网盘登录窗口已提前关闭".into())
                } else {
                    QuarkError::WebAuthorizationRequired
                });
            }
            if !shown && started.elapsed() >= std::time::Duration::from_secs(12) {
                window
                    .show()
                    .map_err(|_| QuarkError::WebAuthorizationRequired)?;
                let _ = window.set_focus();
                shown = true;
            }
            // A newly created WebView may reject its first cookie read before
            // navigating to the provider. Treat that as an incomplete login
            // snapshot and let the bounded loop retry after the page is ready;
            // returning here closes the official password window before its
            // injected login script can complete.
            let cookies =
                browser_cookies(&window, web_session_cookie_urls(provider)).unwrap_or_default();
            let browser_context = if ProviderRegistry::global().get(provider).browser_context {
                tianyi_browser_context(&window).ok()
            } else {
                None
            };
            let mut validation_stage = "读取登录凭据";
            let descriptor = ProviderRegistry::global().get(provider);
            let candidate = (|| {
                if descriptor.authorization.password_login_mode
                    == PasswordLoginMode::OfficialWebView
                {
                    validation_stage = "验证官网登录会话";
                    let temporary = MemoryCredentialStore::default();
                    let account = {
                        let mut driver =
                            ProviderRegistry::global().driver(provider, &temporary, http);
                        let recipe = driver.auth().web_authorization_recipe();
                        let input = crate::cloud::driver::WebAuthorizationInput {
                            cookies,
                            storage: match recipe.token_script {
                                Some(script) => web_storage_tokens(&window, script)?,
                                None => serde_json::Value::Null,
                            },
                            browser_context,
                            auth_method: CloudAuthMethod::Password,
                        };
                        driver.auth().complete_web(&input)?
                    };
                    if account.id.is_empty()
                        || expected_account.is_some_and(|expected| expected != account.id)
                    {
                        Err(QuarkError::InvalidInput(
                            "登录账号与原任务账号不一致，已保留原凭据，请使用原账号登录".into(),
                        ))
                    } else {
                        let verified =
                            temporary.load(provider.key())?.ok_or(QuarkError::Storage)?;
                        store.save(provider.key(), &verified)?;
                        Ok(account)
                    }
                } else if descriptor.browser_context {
                    let validation_http = TianyiBrowserValidationTransport {
                        window: &window,
                        native: http,
                    };
                    validate_browser_password_session(
                        provider,
                        store,
                        &validation_http,
                        &cookies,
                        browser_context.as_ref(),
                        expected_account,
                        |stage| validation_stage = stage,
                    )
                } else {
                    validate_browser_password_session(
                        provider,
                        store,
                        http,
                        &cookies,
                        browser_context.as_ref(),
                        expected_account,
                        |stage| validation_stage = stage,
                    )
                }
            })();
            match candidate {
                Ok(account) => return Ok(account),
                Err(QuarkError::Storage) => return Err(QuarkError::Storage),
                Err(QuarkError::InvalidInput(message)) => {
                    return Err(QuarkError::InvalidInput(message))
                }
                Err(QuarkError::Provider(message))
                    if descriptor.authorization.password_login_mode
                        == PasswordLoginMode::OfficialWebView =>
                {
                    return Err(QuarkError::Provider(message));
                }
                Err(error) => {
                    // Only fixed stage/error labels are exposed. Never show
                    // response bodies, account names, passwords or cookies.
                    let reason = match &error {
                        QuarkError::AuthorizationExpired | QuarkError::NotAuthorized => {
                            "凭据尚未生效"
                        }
                        QuarkError::InvalidResponse => "接口返回格式无法识别",
                        QuarkError::WebAuthorizationRequired | QuarkError::CaptchaRequired => {
                            "接口要求安全验证"
                        }
                        QuarkError::Transport(_) => "网络请求失败",
                        QuarkError::Provider(message)
                            if ProviderRegistry::global().get(provider).browser_context =>
                        {
                            message.as_str()
                        }
                        _ => "接口暂未接受凭据",
                    };
                    let notice = format!("自动登录 · {validation_stage}：{reason}");
                    if notice != last_notice {
                        let _ = window.set_title(&notice);
                        if std::env::var_os("VERSADOWN_BACKGROUND_UI_TEST").is_some() {
                            let _ = window
                                .eval(&browser_auth_diagnostic_script(validation_stage, reason));
                        }
                        last_notice = notice;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
        Err(QuarkError::WebAuthorizationRequired)
    })();
    // Retain a failed window only in the hidden diagnostic harness so it can
    // report the official page's non-sensitive progress flags before process
    // teardown. Normal interactive and production flows always close it.
    if result.is_ok() || !background_ui_test {
        let _ = window.close();
    }
    if result.is_ok()
        && ProviderRegistry::global()
            .get(provider)
            .browser_profile_owner
    {
        set_baidu_browser_profile_username(store, Some(username))?;
        set_browser_password_login_preferred(store, provider, true)?;
    }
    result
}

/// Temporarily installs a fresh cookie snapshot captured from the already
/// authorized official web tab, then keeps it only if the original read-only
/// operation succeeds.  A rejected candidate never destroys the saved
/// session that the user could still use after reopening the tab.
#[cfg(test)]
pub(crate) fn retry_read_with_refreshed_web_session<T>(
    provider: CloudProvider,
    store: &impl CredentialStore,
    session: &[u8],
    retry: impl FnOnce() -> Result<T, QuarkError>,
) -> Result<T, QuarkError> {
    if session.is_empty() {
        return Err(QuarkError::AuthorizationExpired);
    }
    let previous = store.load(provider.key())?;
    store.save(provider.key(), session)?;
    match retry() {
        Ok(value) => Ok(value),
        Err(error) => {
            match previous {
                Some(previous) => store.save(provider.key(), &previous)?,
                None => store.remove(provider.key())?,
            }
            Err(error)
        }
    }
}

#[cfg(test)]
pub(crate) fn can_retry_read_with_web_session(error: &QuarkError) -> bool {
    matches!(
        error,
        QuarkError::AuthorizationExpired | QuarkError::WebAuthorizationRequired
    )
}

/// The authorization page can visit several hosts, but a captured Cookie
/// header is replayed to one provider API host.  Do not merge same-named
/// host-only cookies from the login host into Tianyi's cloud API session.
pub(crate) fn tianyi_browser_context(
    window: &tauri::WebviewWindow,
) -> Result<serde_json::Value, QuarkError> {
    web_storage_tokens(window, tianyi::BROWSER_CONTEXT_READER_JS)
}

pub(crate) fn web_session_cookie_urls(provider: CloudProvider) -> &'static [&'static str] {
    ProviderRegistry::global()
        .get(provider)
        .web_session_cookie_urls
}

pub(crate) fn browser_cookies(
    window: &tauri::WebviewWindow,
    urls: &[&str],
) -> Result<Vec<String>, QuarkError> {
    if urls.is_empty() {
        return Ok(Vec::new());
    }
    let mut values = std::collections::BTreeMap::new();
    // Wry's WKWebView cookies_for_url compares domain strings literally,
    // excluding valid parent-domain cookies such as .baidu.com/.quark.cn.
    // Fetch native cookies once and scope them to our registered provider URLs.
    #[cfg(target_os = "macos")]
    let available = window
        .cookies()
        .map_err(|_| QuarkError::WebAuthorizationRequired)?;
    for url in urls {
        #[cfg(target_os = "macos")]
        let cookies = {
            let url = url::Url::parse(url).map_err(|_| QuarkError::InvalidResponse)?;
            available
                .iter()
                .filter(|cookie| {
                    cookie_matches_provider_url(
                        cookie.domain(),
                        cookie.path(),
                        cookie.secure().unwrap_or(false),
                        &url,
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        #[cfg(not(target_os = "macos"))]
        let cookies = window
            .cookies_for_url(url.parse().map_err(|_| QuarkError::InvalidResponse)?)
            .map_err(|_| QuarkError::WebAuthorizationRequired)?;
        for cookie in cookies {
            values.insert(cookie.name().to_owned(), cookie.value().to_owned());
        }
    }
    Ok(values
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect())
}

#[cfg(any(target_os = "macos", test))]
fn cookie_matches_provider_url(
    domain: Option<&str>,
    path: Option<&str>,
    secure: bool,
    url: &url::Url,
) -> bool {
    let Some(domain) = domain else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    if domain.is_empty() || !(host == domain || host.ends_with(&format!(".{domain}"))) {
        return false;
    }
    if secure && url.scheme() != "https" {
        return false;
    }
    let path = path.unwrap_or("/");
    url.path() == path
        || (url.path().starts_with(path)
            && (path.ends_with('/') || url.path().as_bytes().get(path.len()) == Some(&b'/')))
}

#[cfg(test)]
mod macos_cookie_scope_tests {
    use super::cookie_matches_provider_url;

    #[test]
    fn parent_domain_sessions_match_the_registered_provider_host() {
        for (domain, url) in [(".baidu.com", "https://pan.baidu.com/"), (".quark.cn", "https://pan.quark.cn/")] {
            assert!(cookie_matches_provider_url(Some(domain), Some("/"), true, &url::Url::parse(url).unwrap()));
        }
    }

    #[test]
    fn cookies_never_cross_an_unrelated_domain_path_or_insecure_origin() {
        for url in ["https://evilbaidu.com/", "https://baidu.com.evil.test/", "https://quark.cn/", "http://pan.baidu.com/"] {
            assert!(!cookie_matches_provider_url(Some(".baidu.com"), Some("/"), true, &url::Url::parse(url).unwrap()));
        }
        assert!(!cookie_matches_provider_url(None, Some("/"), false, &url::Url::parse("https://pan.baidu.com/").unwrap()));
        assert!(!cookie_matches_provider_url(Some(".baidu.com"), Some("/disk"), true, &url::Url::parse("https://pan.baidu.com/disk-other").unwrap()));
        assert!(cookie_matches_provider_url(Some(".baidu.com"), Some("/disk"), true, &url::Url::parse("https://pan.baidu.com/disk/main").unwrap()));
    }
}

pub(crate) fn web_value(raw: &str) -> Result<serde_json::Value, QuarkError> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| QuarkError::AuthorizationPending)?;
    if let Some(serialized) = value.as_str() {
        value = serde_json::from_str(serialized).map_err(|_| QuarkError::AuthorizationPending)?;
    }
    value
        .is_object()
        .then_some(value)
        .ok_or(QuarkError::AuthorizationPending)
}

pub(crate) fn web_storage_tokens(
    window: &tauri::WebviewWindow,
    script: &str,
) -> Result<serde_json::Value, QuarkError> {
    let (tx, rx) = std::sync::mpsc::channel();
    window
        .eval_with_callback(script, move |value| {
            let _ = tx.send(value);
        })
        .map_err(|_| QuarkError::WebAuthorizationRequired)?;
    let raw = rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .map_err(|_| QuarkError::WebAuthorizationRequired)?;
    web_value(&raw)
}

// Cloud vendors place their login controls at the upper right of their wide
// desktop pages.  Run after every completed navigation because login flows
// often redirect through one or more provider-owned pages before rendering.
pub(crate) const AUTHORIZATION_TOP_RIGHT_VIEWPORT_JS: &str = r#"(() => {
  const scrollToTopRight = () => {
    const root = document.scrollingElement || document.documentElement || document.body;
    const maxLeft = Math.max(0, root.scrollWidth - window.innerWidth);
    window.scrollTo({ left: maxLeft, top: 0, behavior: 'auto' });
  };
  scrollToTopRight();
  requestAnimationFrame(scrollToTopRight);
  for (const delay of [120, 480, 1000]) setTimeout(scrollToTopRight, delay);
})()"#;

// Baidu's landing page currently puts the real sign-in dialog behind its
// "去登录" action.  This is deliberately best-effort: a provider redesign or
// a slow page must leave the official page usable rather than fail the native
// authorization command.
pub(crate) fn allows_cloud_authorization_popup(url: &url::Url) -> bool {
    url.scheme() == "https" || url.as_str() == "about:blank"
}

pub(crate) fn is_baidu_authorization_page(url: &url::Url) -> bool {
    baidu::is_authorization_page(url)
}
