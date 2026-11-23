use super::*;
use crate::cloud::driver::{NativeLoginOutcome, ProviderAuth};

#[derive(Clone, Serialize)]
pub struct CloudAuthorizationStateEvent {
    pub provider: CloudProvider,
    pub state: auth_supervisor::AuthHealth,
    pub account: Option<CloudAccount>,
}

pub fn authorization_completion_event(
    supervisor: &auth_supervisor::AuthSupervisor,
    store: &impl CredentialStore,
    account: &CloudAccount,
) -> Result<CloudAuthorizationStateEvent, QuarkError> {
    let record = supervisor.record(store, account.provider)?;
    if store.load(account.provider.key())?.is_none()
        || record.health != auth_supervisor::AuthHealth::Ready
        || record.account_id.as_deref() != Some(&account.id)
    {
        return Err(QuarkError::NotAuthorized);
    }
    Ok(CloudAuthorizationStateEvent {
        provider: account.provider,
        state: auth_supervisor::AuthHealth::Ready,
        account: Some(account.clone()),
    })
}

pub fn reauthorization_provider(
    store: &TaskStore,
    id: uuid::Uuid,
) -> Result<CloudProvider, QuarkError> {
    match store.find(id) {
        Ok(task)
            if matches!(task.status, TaskStatus::Failed | TaskStatus::Paused)
                && requires_cloud_reauthorization_failure(task.failure_code.as_deref()) =>
        {
            return cloud_provider_from_download_source(&task.source)
        }
        Ok(_) => return Err(QuarkError::InvalidInput("当前任务不需要重新授权".into())),
        Err(crate::StoreError::NotFound(_)) => {}
        Err(_) => return Err(QuarkError::Storage),
    }
    for group in store
        .cloud_directory_groups()
        .map_err(|_| QuarkError::Storage)?
    {
        if matches!(
            group.status,
            TaskStatus::Cancelled | TaskStatus::Recycled | TaskStatus::Completed
        ) {
            continue;
        }
        for item in store
            .cloud_directory_group_items(group.id)
            .map_err(|_| QuarkError::Storage)?
        {
            if group.id != id && item.id != id {
                continue;
            }
            let eligible = if let Some(task_id) = item.task_id {
                store.find(task_id).is_ok_and(|task| {
                    matches!(task.status, TaskStatus::Failed | TaskStatus::Paused)
                        && requires_cloud_reauthorization_failure(task.failure_code.as_deref())
                })
            } else {
                requires_cloud_reauthorization_failure(item.failure_code.as_deref())
            };
            if eligible {
                return Ok(group.provider);
            }
        }
    }
    Err(QuarkError::InvalidInput("当前任务不需要重新授权".into()))
}

fn reauthorization_owner_ids(
    store: &TaskStore,
    id: uuid::Uuid,
    provider: CloudProvider,
) -> Result<Vec<uuid::Uuid>, QuarkError> {
    let mut ids = Vec::new();
    let task = match store.find(id) {
        Ok(task) => Some(task),
        Err(crate::StoreError::NotFound(_)) => None,
        Err(_) => return Err(QuarkError::Storage),
    };
    if let Some(task) = &task {
        if cloud_provider_from_download_source(&task.source).ok() != Some(provider) {
            return Err(QuarkError::InvalidInput("网盘任务来源无效".into()));
        }
        ids.push(task.id);
    }
    let group = if task.is_some() {
        store
            .cloud_directory_group_for_task(id)
            .map_err(|_| QuarkError::Storage)?
    } else {
        store
            .cloud_directory_group(id)
            .map_err(|_| QuarkError::Storage)?
    };
    if let Some(group) = group {
        if group.provider != provider {
            return Err(QuarkError::InvalidInput("网盘任务来源无效".into()));
        }
        ids.push(group.id);
        for item in store
            .cloud_directory_group_items(group.id)
            .map_err(|_| QuarkError::Storage)?
        {
            let Some(task_id) = item.task_id else {
                continue;
            };
            let task = store.find(task_id).map_err(|_| QuarkError::Storage)?;
            if cloud_provider_from_download_source(&task.source).ok() != Some(provider) {
                return Err(QuarkError::Storage);
            }
            ids.push(task_id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Err(QuarkError::InvalidInput("当前任务不需要重新授权".into()));
    }
    Ok(ids)
}

fn reauthorization_owner(
    store: &TaskStore,
    id: uuid::Uuid,
    provider: CloudProvider,
) -> Result<Option<String>, QuarkError> {
    let mut owner = None;
    for candidate in reauthorization_owner_ids(store, id, provider)? {
        let Some(candidate_owner) = store
            .cloud_download_account(candidate)
            .map_err(|_| QuarkError::Storage)?
        else {
            continue;
        };
        if let Some(existing) = owner.as_deref() {
            if existing != candidate_owner {
                return Err(QuarkError::Storage);
            }
        } else {
            owner = Some(candidate_owner);
        }
    }
    Ok(owner)
}

/// Legacy share tasks predate durable account ownership. An explicit
/// reauthorization is the only safe moment to adopt them into the current
/// account: future automatic recovery can then remain account-scoped.
pub(crate) fn bind_reauthorized_cloud_task_owner(
    store: &TaskStore,
    id: uuid::Uuid,
    provider: CloudProvider,
    account: &str,
) -> Result<(), QuarkError> {
    if account.trim().is_empty() {
        return Err(QuarkError::NotAuthorized);
    }
    let ids = reauthorization_owner_ids(store, id, provider)?;
    for candidate in &ids {
        if let Some(existing) = store
            .cloud_download_account(*candidate)
            .map_err(|_| QuarkError::Storage)?
        {
            if existing != account {
                return Err(QuarkError::AccountMismatch);
            }
        }
    }
    for candidate in ids {
        store
            .bind_cloud_download_account(candidate, provider, account)
            .map_err(|_| QuarkError::Storage)?;
    }
    Ok(())
}

/// Explicit authorization only: background refresh must never close interactive UI.
pub(crate) fn publish_authorization_completion(
    app: &AppHandle,
    state: &CloudState,
    account: &CloudAccount,
) -> Result<(), QuarkError> {
    use tauri::Emitter;
    let event = authorization_completion_event(&state.auth_supervisor, &state.store, account)?;
    state.remember_authorization_completion(account)?;
    let _ = app.emit("cloud-authorization-state", event);
    if let Some(window) = app.get_webview_window(&format!("cloud-auth-{}", account.provider.key()))
    {
        let _ = window.close();
    }
    let app = app.clone();
    let provider = account.provider;
    tauri::async_runtime::spawn(async move {
        let _ = resume_reauthorized_provider_tasks(app, provider).await;
    });
    Ok(())
}

/// The host owns browser fallback; the callback keeps this boundary testable without a WebView.
pub(crate) fn password_login_with_browser(
    provider: CloudProvider,
    store: &impl CredentialStore,
    auth: &mut dyn ProviderAuth,
    username: &str,
    password: &str,
    mut browser: impl FnMut(bool) -> Result<CloudAccount, QuarkError>,
) -> Result<CloudAccount, QuarkError> {
    match auth.login_password(username, password) {
        Ok(NativeLoginOutcome::Authenticated(account)) => Ok(account),
        Ok(NativeLoginOutcome::NeedBrowserFallback(error))
            if browser_login::supports_browser_password(provider) =>
        {
            // Cookies may change before validation finishes. Keep this marker
            // through failure/restart until the matching password is committed.
            if let Some(key) = password_profile_reset_key(provider) {
                store.save(key, b"1")?;
            }
            if ProviderRegistry::global()
                .get(provider)
                .browser_profile_owner
            {
                set_browser_password_login_preferred(store, provider, true)?;
            }
            browser(matches!(error, QuarkError::CaptchaRequired))
        }
        Ok(NativeLoginOutcome::NeedBrowserFallback(error)) | Err(error) => Err(error),
    }
}

pub(crate) fn validate_browser_password_session(
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    cookies: &[String],
    browser_context: Option<&serde_json::Value>,
    expected_account: Option<&str>,
    mut report_stage: impl FnMut(&'static str),
) -> Result<CloudAccount, QuarkError> {
    report_stage("读取登录凭据");
    let (account, snapshot) = ProviderRegistry::global()
        .driver(provider, store, http)
        .auth()
        .validate_browser_password(cookies, browser_context, &mut report_stage)?;
    if account.id.is_empty() || expected_account.is_some_and(|expected| expected != account.id) {
        return Err(QuarkError::InvalidInput(
            "登录账号与原任务账号不一致，已保留原凭据，请使用原账号登录".into(),
        ));
    }
    report_stage("保存登录凭据");
    store.save(provider.key(), &snapshot)?;
    Ok(account)
}

#[tauri::command]
pub fn cloud_provider_capabilities(provider: CloudProvider) -> CloudAuthorizationCapabilities {
    provider_capabilities(provider)
}

#[tauri::command]
pub fn cloud_provider_catalog() -> Vec<crate::cloud::ProviderDescriptorDto> {
    crate::cloud::ProviderRegistry::global().catalog()
}

#[tauri::command]
pub async fn cloud_saved_password_credentials(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<Option<SavedCloudCredentialInfo>, QuarkError> {
    if !provider_capabilities(provider).password {
        return Ok(None);
    }
    run(app, provider, move |state, _| {
        saved_cloud_credential_info(&state.store, provider)
    })
    .await
}

pub(crate) fn tianyi_session_safe_shape(session: &tianyi::TianyiSession) -> serde_json::Value {
    let mut cookie_names = session
        .cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('=').map(|(name, _)| name.trim()))
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    cookie_names.sort();
    cookie_names.dedup();
    serde_json::json!({
        "authMethod": session.auth_method,
        "transport": if session.app.is_some() { "nativeApp" } else { "web" },
        "cookieNames": cookie_names,
        "browserContext": session.has_browser_context(),
    })
}

#[tauri::command]
pub async fn cloud_tianyi_session_shape_for_test(
    app: AppHandle,
) -> Result<serde_json::Value, QuarkError> {
    if std::env::var("VERSADOWN_BACKGROUND_UI_TEST")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Err(QuarkError::InvalidInput("仅后台诊断模式可用".into()));
    }
    run(app, CloudProvider::Tianyi, move |state, _| {
        let session: tianyi::TianyiSession =
            load_provider_session(CloudProvider::Tianyi, &state.store)?;
        Ok(tianyi_session_safe_shape(&session))
    })
    .await
}

#[tauri::command]
pub async fn cloud_tianyi_resolve_download_for_test(
    app: AppHandle,
    file_id: String,
) -> Result<bool, QuarkError> {
    if std::env::var("VERSADOWN_BACKGROUND_UI_TEST")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Err(QuarkError::InvalidInput("仅后台诊断模式可用".into()));
    }
    run(app, CloudProvider::Tianyi, move |state, http| {
        let session: tianyi::TianyiSession =
            load_provider_session(CloudProvider::Tianyi, &state.store)?;
        if session.app.is_none() {
            return Err(QuarkError::InvalidInput("当前不是天翼原生 App 会话".into()));
        }
        let download = tianyi::TianyiClient::new(session, http).download(&file_id)?;
        Ok(download.url.starts_with("https://")
            && ["Date", "SessionKey", "Signature", "X-Request-ID"]
                .iter()
                .all(|name| download.headers.contains_key(*name)))
    })
    .await
}

/// Guards account operations (authorization, logout, ...). A provider without
/// an account has nothing to authorize; its public share flow never calls this.
pub(crate) fn require_connected_provider(provider: CloudProvider) -> Result<(), QuarkError> {
    if !provider.is_enabled() || !ProviderRegistry::global().get(provider).account {
        return Err(QuarkError::ProviderUnsupported);
    }
    Ok(())
}

pub fn logout_provider(
    provider: CloudProvider,
    store: &impl CredentialStore,
) -> Result<(), QuarkError> {
    require_connected_provider(provider)?;
    // Explicit logout clears authorization, not the encrypted login remembered
    // for the next deliberate sign-in. AuthSupervisor persists Disconnected.
    store.remove(provider.key())
}

#[tauri::command]
pub async fn cloud_begin_authorization(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAuthorization, QuarkError> {
    require_connected_provider(provider)?;
    // Providers without an official web sign-in use their own login form.
    if !provider_capabilities(provider).web {
        return Err(QuarkError::NotAuthorized);
    }
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let _completion = cloud
        .authorization_completion
        .acquire(provider.key())
        .await?;
    let label = format!("cloud-auth-{}", provider.key());
    if let Some(window) = app.get_webview_window(&label) {
        window
            .show()
            .map_err(|_| QuarkError::WebAuthorizationRequired)?;
        let _ = window.set_focus();
    } else {
        cloud.clear_authorization_completion(provider)?;
        let url = crate::cloud::ProviderRegistry::global()
            .get(provider)
            .authorization_url;
        let background_ui_test = std::env::var("VERSADOWN_BACKGROUND_UI_TEST")
            .ok()
            .as_deref()
            == Some("1");
        let mut builder = WebviewWindowBuilder::new(
            &app,
            &label,
            WebviewUrl::External(url.parse().map_err(|_| QuarkError::InvalidResponse)?),
        )
        .title(format!("{} 网页授权", provider.key()))
        .inner_size(1024.0, 760.0)
        .visible(!background_ui_test)
        .incognito(false)
        .data_directory(authorization_profile_directory(&app, provider)?)
        .on_new_window(|url, _features| {
            if allows_cloud_authorization_popup(&url) {
                NewWindowResponse::Allow
            } else {
                NewWindowResponse::Deny
            }
        })
        .on_page_load(move |window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = window.eval(AUTHORIZATION_TOP_RIGHT_VIEWPORT_JS);
                if ProviderRegistry::global()
                    .get(provider)
                    .browser_profile_owner
                    && is_baidu_authorization_page(payload.url())
                {
                    let _ = window.eval(baidu::BAIDU_AUTO_OPEN_LOGIN_JS);
                }
            }
        });
        if browser_login::supports_browser_password(provider) {
            builder = builder.data_directory(password_webview_data_directory(&app, provider)?);
        }
        if ProviderRegistry::global().get(provider).browser_context {
            builder = builder.initialization_script(include_str!("../tianyi-browser-context.js"));
        }
        if let Some(script) = ProviderRegistry::global()
            .get(provider)
            .web_initialization_script
        {
            builder = builder.initialization_script(script);
        }
        builder
            .build()
            .map_err(|_| QuarkError::WebAuthorizationRequired)?;
        // Xunlei's current SSO can establish a logged-in SPA state without a
        // top-level navigation after this first load. A page-load-only attempt
        // then misses the new credentials and leaves an already logged-in
        // authorization window open. Poll the existing window for this short,
        // user-initiated authorization only; success still requires the normal
        // root-directory validation before the window is closed.
        let completion_app = app.clone();
        tauri::async_runtime::spawn(async move {
            for _ in 0..180 {
                if completion_app
                    .get_webview_window(&format!("cloud-auth-{}", provider.key()))
                    .is_none()
                {
                    return;
                }
                if cloud_complete_authorization(completion_app.clone(), provider)
                    .await
                    .is_ok()
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
    Ok(CloudAuthorization {
        provider,
        window_label: label,
        instructions: "请在弹出的官方网盘窗口完成登录；验证成功后授权窗口会自动收起。".into(),
        completed: false,
    })
}

pub(crate) fn cloud_provider_from_download_source(
    source: &str,
) -> Result<CloudProvider, QuarkError> {
    if source.starts_with("cloud-share://") {
        return share::parse_stable_cloud_share_source(source).map(|(provider, _, _)| provider);
    }
    parse_stable_cloud_source(source).map(|(provider, _)| provider)
}

fn record_provider_account_locked(
    state: &CloudState,
    provider: CloudProvider,
    account: &CloudAccount,
) -> Result<(), QuarkError> {
    state.auth_supervisor.ready_locked(
        &state.store,
        provider,
        Some(account.id.clone()),
        Some(account.auth_method),
        auth_supervisor::session_expiry(&state.store, provider)?,
    )
}

pub(crate) async fn validate_current_provider_session(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAccount, QuarkError> {
    run(app, provider, move |state, http| {
        if auth_supervisor::is_manually_disconnected(&state.store, provider)? {
            return Err(QuarkError::NotAuthorized);
        }
        let account = restore_account(provider, &state.store, http)?;
        record_provider_account_locked(state, provider, &account)?;
        Ok(account)
    })
    .await
}

pub(crate) async fn silently_recover_provider_session(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAccount, QuarkError> {
    let recovery_app = app.clone();
    run(app, provider, move |state, http| {
        if auth_supervisor::is_manually_disconnected(&state.store, provider)? {
            return Err(QuarkError::NotAuthorized);
        }
        let recovered = recover_authorization(&recovery_app, state, provider, http, true);
        if let Err(error) = recovered {
            let can_try_password = browser_login::supports_browser_password(provider)
                && !matches!(error, QuarkError::CaptchaRequired)
                && browser_login::can_fallback_to_browser(&error);
            let Some(credential) = (if can_try_password {
                load_password_credential(&state.store, provider)?
            } else {
                None
            }) else {
                return Err(error);
            };
            let account = browser_password_login(
                &recovery_app,
                provider,
                &state.store,
                http,
                credential.username(),
                credential.password(),
                Some(credential.account_id()),
                false,
            )?;
            record_provider_account_locked(state, provider, &account)?;
            return Ok(account);
        }
        let account = restore_account(provider, &state.store, http)?;
        record_provider_account_locked(state, provider, &account)?;
        Ok(account)
    })
    .await
}

/// Startup and explicit recovery share one policy: validate the current
/// provider session first, then silently refresh credentials/profile state.
/// This function never opens a user-visible authorization window.
pub(crate) async fn restore_provider_session(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAccount, QuarkError> {
    match validate_current_provider_session(app.clone(), provider).await {
        Ok(account) => Ok(account),
        Err(error) if should_attempt_silent_provider_recovery(&error) => {
            silently_recover_provider_session(app, provider).await
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn should_attempt_silent_provider_recovery(error: &QuarkError) -> bool {
    matches!(
        error,
        QuarkError::NotAuthorized
            | QuarkError::AuthorizationExpired
            | QuarkError::WebAuthorizationRequired
            | QuarkError::AuthorizationPending
            | QuarkError::CaptchaRequired
    )
}

pub(crate) fn should_release_startup_directory_pause(
    group_status: TaskStatus,
    waiting_for_startup_authorization: bool,
) -> bool {
    group_status == TaskStatus::Paused && waiting_for_startup_authorization
}

async fn publish_silent_recovery(
    app: AppHandle,
    provider: CloudProvider,
    account: CloudAccount,
) -> Result<(), QuarkError> {
    let publish_app = app.clone();
    run(app, provider, move |state, _| {
        publish_authorization_completion(&publish_app, state, &account)
    })
    .await
}

/// Explicitly recover the provider; durable account-bound failures are discovered after success.
#[tauri::command]
pub async fn cloud_reauthorize_failed_task(
    app: AppHandle,
    task_id: String,
) -> Result<CloudAuthorization, QuarkError> {
    let id = uuid::Uuid::parse_str(&task_id)
        .map_err(|_| QuarkError::InvalidInput("任务 ID 无效".into()))?;
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;

    match state.store.find(id) {
        Ok(task) => {
            ensure_private_task_access(&state, &task).map_err(QuarkError::InvalidInput)?;
        }
        Err(crate::StoreError::NotFound(_)) => {
            for group in state
                .store
                .cloud_directory_groups()
                .map_err(|_| QuarkError::Storage)?
            {
                let matches_requested = group.id == id
                    || state
                        .store
                        .cloud_directory_group_items(group.id)
                        .map_err(|_| QuarkError::Storage)?
                        .iter()
                        .any(|item| item.id == id);
                if matches_requested {
                    ensure_private_group_access(&state, &group)
                        .map_err(QuarkError::InvalidInput)?;
                    break;
                }
            }
        }
        Err(_) => return Err(QuarkError::Storage),
    }

    let requested_provider = reauthorization_provider(&state.store, id)?;
    let expected_account = reauthorization_owner(&state.store, id, requested_provider)?;

    match restore_provider_session(app.clone(), requested_provider).await {
        Ok(account)
            if expected_account
                .as_deref()
                .is_none_or(|expected| expected == account.id) =>
        {
            bind_reauthorized_cloud_task_owner(&state.store, id, requested_provider, &account.id)?;
            publish_silent_recovery(app.clone(), requested_provider, account).await?;
            return Ok(CloudAuthorization {
                provider: requested_provider,
                window_label: String::new(),
                instructions: "登录已自动恢复，正在继续下载".into(),
                completed: true,
            });
        }
        Ok(_) => {}
        Err(error) if should_attempt_silent_provider_recovery(&error) => {}
        Err(error) => return Err(error),
    }
    cloud_begin_authorization(app, requested_provider).await
}

pub async fn resume_reauthorized_provider_tasks(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<(), QuarkError> {
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let _resumption = cloud
        .authorization_resumption
        .acquire(provider.key())
        .await?;
    let record = cloud.auth_supervisor.record(&cloud.store, provider)?;
    if record.health != auth_supervisor::AuthHealth::Ready {
        return Ok(());
    }
    let Some(account_id) = record.account_id else {
        return Ok(());
    };
    let state = app.try_state::<AppState>().ok_or(QuarkError::Storage)?;
    let (tasks, directory_items) = state
        .store
        .reauthorized_cloud_downloads(provider, &account_id)
        .map_err(|_| QuarkError::Storage)?;
    for candidate in tasks {
        let id = candidate.id;
        if cloud
            .auth_supervisor
            .record(&cloud.store, provider)?
            .account_id
            .as_deref()
            != Some(&account_id)
        {
            break;
        }
        let Ok(task) = state.store.find(id) else {
            continue;
        };
        if !should_emit_task_update(&state, task.is_private) {
            continue;
        }
        let waiting_for_startup_authorization = task.status == TaskStatus::Paused
            && matches!(
                task.failure_code.as_deref(),
                Some(
                    "authorization_checking" | "authorization_recovering" | "authorization_pending"
                )
            );
        if let Some(group) = state
            .store
            .cloud_directory_group_for_task(id)
            .map_err(|_| QuarkError::Storage)?
        {
            if should_release_startup_directory_pause(
                group.status,
                waiting_for_startup_authorization,
            ) {
                // Startup recovery pauses interrupted directories before any
                // provider work. Once that provider has recovered, release
                // only this protection pause. A manually paused directory has
                // no startup authorization marker and remains paused.
                state
                    .store
                    .set_cloud_directory_paused(group.id, false)
                    .map_err(|_| QuarkError::Storage)?;
            } else if matches!(
                group.status,
                TaskStatus::Paused
                    | TaskStatus::Cancelled
                    | TaskStatus::Recycled
                    | TaskStatus::Completed
            ) {
                continue;
            }
        }
        if !(waiting_for_startup_authorization
            || (task.status == TaskStatus::Failed
                && requires_cloud_reauthorization_failure(task.failure_code.as_deref())))
            || cloud_provider_from_download_source(&task.source).ok() != Some(provider)
            || (!waiting_for_startup_authorization
                && !matches!(crate::retry_duplicate_state(&state.store, &task), Ok(None)))
        {
            continue;
        }
        let resumed = if waiting_for_startup_authorization {
            crate::commands::resubmit_task(
                app.clone(),
                &state,
                task,
                crate::UriSubmissionOptions {
                    continue_download: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(QuarkError::Transport)
        } else {
            crate::commands::resubmit_task(app.clone(), &state, task, Default::default())
                .await
                .map_err(QuarkError::Transport)
        };
        match resumed {
            Ok(task) => {
                if let Ok(outcome) = task_action_outcome(&state.store, task, None) {
                    let payload = outcome
                        .queue_task
                        .map(crate::commands::TaskUpdatePayload::Queue)
                        .unwrap_or(crate::commands::TaskUpdatePayload::Task(outcome.payload));
                    crate::commands::emit_task_update(&app, payload);
                }
            }
            Err(_) => {
                if let Ok(task) = state.store.find(id) {
                    let _ = crate::commands::emit_task_update_for_task(
                        &app,
                        &state.store,
                        Some(&cloud.share_plans),
                        task,
                    );
                }
            }
        }
    }
    for (group_id, item_id) in directory_items {
        if cloud
            .auth_supervisor
            .record(&cloud.store, provider)?
            .account_id
            .as_deref()
            != Some(&account_id)
        {
            break;
        }
        let Some(group) = state
            .store
            .cloud_directory_group(group_id)
            .map_err(|_| QuarkError::Storage)?
        else {
            continue;
        };
        if !should_emit_task_update(&state, group.is_private) {
            continue;
        }
        if matches!(
            group.status,
            TaskStatus::Cancelled | TaskStatus::Recycled | TaskStatus::Completed
        ) {
            continue;
        }
        if !state
            .store
            .cloud_directory_group_items(group_id)
            .map_err(|_| QuarkError::Storage)?
            .iter()
            .any(|item| {
                item.id == item_id
                    && item.task_id.is_none()
                    && requires_cloud_reauthorization_failure(item.failure_code.as_deref())
            })
        {
            continue;
        }
        if let Ok(outcome) = retry_cloud_directory_group_item(app.clone(), group_id, item_id).await
        {
            let payload = outcome
                .queue_task
                .map(crate::commands::TaskUpdatePayload::Queue)
                .unwrap_or(crate::commands::TaskUpdatePayload::Task(outcome.payload));
            crate::commands::emit_task_update(&app, payload);
        }
    }
    Ok(())
}

// Only selected token fields cross the native boundary. No password, full storage dump, or DOM is returned.

// The browser page is the only place where these renewable credentials are
// acquired.  The scripts return a narrowly selected object; they never return
// a storage dump, DOM content or passwords to Rust.

#[tauri::command]
pub fn cloud_completed_authorization(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<Option<CloudAccount>, QuarkError> {
    require_connected_provider(provider)?;
    app.try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?
        .completed_authorization(provider)
}

#[tauri::command]
pub async fn cloud_complete_authorization(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAccount, QuarkError> {
    require_connected_provider(provider)?;
    let cloud = app
        .try_state::<Arc<CloudState>>()
        .ok_or(QuarkError::Storage)?;
    let _completion = cloud
        .authorization_completion
        .acquire(provider.key())
        .await?;
    if let Some(account) = cloud.completed_authorization(provider)? {
        return Ok(account);
    }
    let window = app
        .get_webview_window(&format!("cloud-auth-{}", provider.key()))
        .ok_or(QuarkError::WebAuthorizationRequired)?;
    let publish_app = app.clone();
    let account = run(app.clone(), provider, move |state, http| {
        let mut driver = ProviderRegistry::global().driver(provider, &state.store, http);
        let recipe = driver.auth().web_authorization_recipe();
        let input = crate::cloud::driver::WebAuthorizationInput {
            cookies: browser_cookies(&window, web_session_cookie_urls(provider))?,
            storage: match recipe.token_script {
                Some(script) => web_storage_tokens(&window, script)?,
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
            let validation_http = TianyiBrowserValidationTransport {
                window: &window,
                native: http,
            };
            let account = ProviderRegistry::global()
                .driver(provider, &state.store, &validation_http)
                .auth()
                .complete_web(&input)?;
            account
        } else {
            driver.auth().complete_web(&input)?
        };
        state.auth_supervisor.ready_locked(
            &state.store,
            provider,
            Some(account.id.clone()),
            Some(account.auth_method),
            auth_supervisor::session_expiry(&state.store, provider)?,
        )?;
        if ProviderRegistry::global()
            .get(provider)
            .browser_profile_owner
        {
            let profile_username = load_password_credential(&state.store, provider)?
                .filter(|credential| credential.account_id() == account.id)
                .map(|credential| credential.username().to_owned());
            set_baidu_browser_profile_username(&state.store, profile_username.as_deref())?;
            set_browser_password_login_preferred(&state.store, provider, true)?;
        }
        publish_authorization_completion(&publish_app, state, &account)?;
        Ok(account)
    })
    .await?;
    // The session is persisted only after the official page has proved it can
    // access the provider. Every queued failed task therefore resolves with
    // the new shared credentials, not a stale per-task snapshot.
    Ok(account)
}

pub(crate) fn refresh_baidu_operation_verification_session(
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    cookies: &[String],
    expected_account_id: &str,
) -> Result<CloudAccount, QuarkError> {
    if expected_account_id.is_empty() {
        return Err(QuarkError::AccountMismatch);
    }
    let saved: baidu::BaiduSession = load_provider_session(CloudProvider::Baidu, store)?;
    if baidu::BaiduClient::new(saved, http).account()?.id != expected_account_id {
        return Err(QuarkError::AccountMismatch);
    }
    let candidate = baidu::parse_baidu_web_session(cookies)?;
    let client = baidu::BaiduClient::new(candidate.clone(), http);
    let account = client.account()?;
    if account.id != expected_account_id {
        return Err(QuarkError::AccountMismatch);
    }
    client.directory(
        ProviderRegistry::global()
            .get(CloudProvider::Baidu)
            .root_directory_id,
    )?;
    save_provider_session(CloudProvider::Baidu, store, &candidate)?;
    Ok(account)
}

/// Opens the provider's official operation-verification page in its existing
/// browser profile. This window never submits or replays a file operation.
#[tauri::command]
pub async fn cloud_open_operation_verification(
    app: AppHandle,
    provider: CloudProvider,
    expected_account_id: String,
    locale: Option<String>,
) -> Result<(), QuarkError> {
    if !matches!(provider, CloudProvider::Pikpak | CloudProvider::Baidu) {
        return Err(QuarkError::InvalidInput(
            "此网盘没有待处理的操作验证".into(),
        ));
    }
    let window_app = app.clone();
    run(app, provider, move |state, http| {
        if auth_supervisor::is_manually_disconnected(&state.store, provider)? {
            return Err(QuarkError::NotAuthorized);
        }
        let (url, label, title) = if provider == CloudProvider::Baidu {
            let saved: baidu::BaiduSession = load_provider_session(provider, &state.store)?;
            if baidu::BaiduClient::new(saved, http).account()?.id != expected_account_id {
                return Err(QuarkError::AccountMismatch);
            }
            (
                "https://pan.baidu.com/disk/main#/recyclebin".to_owned(),
                "cloud-operation-baidu",
                if locale
                    .as_deref()
                    .is_some_and(|value| value.to_ascii_lowercase().starts_with("en"))
                {
                    "Baidu Netdisk verification: return to the app when finished"
                } else {
                    "百度网盘安全验证：完成后请返回应用重试"
                },
            )
        } else {
            (
                PikPakClient::new(http, &state.store)
                    .operation_verification_url(&expected_account_id)?,
                "cloud-operation-pikpak",
                "PikPak 操作验证：完成后请返回应用重试",
            )
        };
        if let Some(window) = window_app.get_webview_window(label) {
            // Never leave a previous account's/challenge's page in this window.
            window
                .navigate(url.parse().map_err(|_| QuarkError::InvalidResponse)?)
                .map_err(|_| QuarkError::Storage)?;
            window.show().map_err(|_| QuarkError::Storage)?;
            let _ = window.set_focus();
            return Ok(());
        }
        WebviewWindowBuilder::new(
            &window_app,
            label,
            WebviewUrl::External(url.parse().map_err(|_| QuarkError::InvalidResponse)?),
        )
        .title(title)
        .inner_size(1024.0, 760.0)
        .data_directory(authorization_profile_directory(&window_app, provider)?)
        .on_navigation(move |url| {
            if provider == CloudProvider::Baidu {
                url.scheme() == "https"
                    && matches!(url.host_str(), Some("pan.baidu.com" | "passport.baidu.com"))
            } else {
                allows_pikpak_operation_verification_url(url.as_str())
            }
        })
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()
        .map_err(|_| QuarkError::Storage)?;
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn cloud_complete_operation_verification(
    app: AppHandle,
    provider: CloudProvider,
    expected_account_id: String,
) -> Result<CloudAccount, QuarkError> {
    if provider != CloudProvider::Baidu {
        return Err(QuarkError::InvalidInput(
            "此网盘没有待处理的操作验证".into(),
        ));
    }
    let window = app
        .get_webview_window("cloud-operation-baidu")
        .ok_or(QuarkError::WebAuthorizationRequired)?;
    let capture_window = window.clone();
    let account = run(app, provider, move |state, http| {
        if auth_supervisor::is_manually_disconnected(&state.store, provider)? {
            return Err(QuarkError::NotAuthorized);
        }
        let cookies = browser_cookies(&capture_window, web_session_cookie_urls(provider))?;
        let account = refresh_baidu_operation_verification_session(
            &state.store,
            http,
            &cookies,
            &expected_account_id,
        )?;
        state.auth_supervisor.ready_locked(
            &state.store,
            provider,
            Some(account.id.clone()),
            Some(account.auth_method),
            auth_supervisor::session_expiry(&state.store, provider)?,
        )?;
        Ok(account)
    })
    .await?;
    let _ = window.close();
    Ok(account)
}

#[tauri::command]
pub async fn cloud_login_password(
    app: AppHandle,
    provider: CloudProvider,
    username: String,
    password: Option<String>,
    use_saved_password: Option<bool>,
) -> Result<CloudAccount, QuarkError> {
    require_connected_provider(provider)?;
    let request = PasswordLoginRequest {
        provider,
        username,
        password,
        use_saved_password: use_saved_password.unwrap_or(false),
    };
    if !provider_capabilities(provider).password {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    let browser_app = app.clone();
    let account = run(app.clone(), provider, move |state, http| {
        let (username, password) = resolve_password_login(&state.store, request)?;
        save_password_login_draft(&state.store, provider, &username, &password)?;
        let reset_baidu_profile = ProviderRegistry::global()
            .get(provider)
            .browser_profile_owner
            && prepare_baidu_account_login(&state.store, &username)?;
        let reset_password_profile =
            prepare_password_account_login(&state.store, provider, &username)?;
        reset_password_profile_if_required(
            &state.store,
            provider,
            reset_baidu_profile || reset_password_profile,
            || clear_password_webview_profile(&browser_app, provider),
        )?;
        if reset_baidu_profile || reset_password_profile {
            if ProviderRegistry::global()
                .get(provider)
                .browser_profile_owner
            {
                set_baidu_browser_profile_username(&state.store, None)?;
            }
            set_browser_password_login_preferred(&state.store, provider, false)?;
        }
        // Do not persist submitted input until the provider validates it.
        let account = password_login_with_browser(
            provider,
            &state.store,
            ProviderRegistry::global()
                .driver(provider, &state.store, http)
                .auth(),
            &username,
            &password,
            |captcha| {
                browser_password_login(
                    &browser_app,
                    provider,
                    &state.store,
                    http,
                    &username,
                    &password,
                    None,
                    captcha,
                )
            },
        )?;
        save_password_credential(
            &state.store,
            CloudPasswordCredential::new(
                provider,
                account.id.clone(),
                username.as_str(),
                password.as_str(),
            ),
        )?;
        if let Some(key) = password_profile_reset_key(provider) {
            state.store.remove(key)?;
        }
        state.auth_supervisor.ready_locked(
            &state.store,
            provider,
            Some(account.id.clone()),
            Some(account.auth_method),
            auth_supervisor::session_expiry(&state.store, provider)?,
        )?;
        publish_authorization_completion(&browser_app, state, &account)?;
        Ok(account)
    })
    .await?;
    Ok(account)
}

#[tauri::command]
pub async fn cloud_logout(app: AppHandle, provider: CloudProvider) -> Result<(), QuarkError> {
    require_connected_provider(provider)?;
    let copy = app.clone();
    run(app, provider, move |s, http| {
        // MEGA sessions live on the server: end this one while its id is
        // still stored. Best effort, so signing out never needs the network.
        if provider == CloudProvider::Mega {
            crate::cloud::mega::account::revoke_session(http, &s.store);
        }
        if provider_capabilities(provider).web {
            let profile_window = WebviewWindowBuilder::new(
                &copy,
                format!("cloud-logout-{}", provider.key()),
                WebviewUrl::External(
                    "about:blank"
                        .parse()
                        .map_err(|_| QuarkError::InvalidResponse)?,
                ),
            )
            .visible(false)
            .data_directory(authorization_profile_directory(&copy, provider)?)
            .build()
            .map_err(|_| QuarkError::Storage)?;
            let cleared = profile_window
                .clear_all_browsing_data()
                .map_err(|_| QuarkError::Storage);
            let _ = profile_window.close();
            cleared?;
        }
        logout_provider(provider, &s.store)?;
        s.auth_supervisor.disconnect_locked(&s.store, provider)?;
        s.clear_authorization_completion(provider)?;
        if let Some(w) = copy.get_webview_window(&format!("cloud-auth-{}", provider.key())) {
            let _ = w.close();
        }
        if let Some(w) = copy.get_webview_window(&format!("cloud-operation-{}", provider.key())) {
            let _ = w.close();
        }
        Ok(())
    })
    .await
}
