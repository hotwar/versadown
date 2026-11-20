//! Agent account projections and explicit, bounded desktop authorization sessions.
//! Credentials remain owned by the cloud connector; a window completion is the
//! only event that can complete an Agent authorization session.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::cli_cloud_protocol::{CloudAccountDto, CloudAccountsDto, CloudAuthorizationDto, CloudRequest};
use crate::cli_protocol::{ApiError, Response};
use crate::cloud::{commands::CloudState, CloudAccount, CloudProvider, ProviderRegistry, QuarkError};

const SESSION_SECONDS: i64 = 300;
const MAX_SESSIONS: usize = 128;

pub(crate) fn account_ref(provider: CloudProvider, account_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"versadown.agent.account.v1\0");
    digest.update(provider.key().as_bytes());
    digest.update(b"\0");
    digest.update(account_id.as_bytes());
    format!("acct_{:x}", digest.finalize())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    Succeeded,
    AccountMismatch,
    Cancelled,
    Closed,
    Expired,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
struct Session {
    session_id: String,
    provider: CloudProvider,
    status: Status,
    account_ref: Option<String>,
    expires_at: i64,
    #[serde(skip)]
    expected_account: Option<String>,
    #[serde(skip)]
    window_generation: Option<String>,
    #[serde(skip)]
    failure_reason: Option<String>,
    #[serde(skip)]
    opening: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorizationWindowSource { Desktop, Agent }

#[derive(Clone)]
struct AuthorizationWindow {
    generation: String,
    used_by_desktop: bool,
}

#[derive(Default)]
struct Sessions {
    sessions: HashMap<String, Session>,
    windows: HashMap<CloudProvider, AuthorizationWindow>,
}

impl Sessions {
    fn expire(&mut self, timestamp: i64) -> Vec<(CloudProvider, String)> {
        let mut windows = Vec::new();
        for session in self.sessions.values_mut() {
            if session.status == Status::Pending && timestamp >= session.expires_at {
                session.status = Status::Expired;
                if let Some(generation) = &session.window_generation {
                    windows.push((session.provider, generation.clone()));
                }
            }
        }
        windows
    }

    fn begin(
        &mut self,
        provider: CloudProvider,
        expected: Option<String>,
        timestamp: i64,
    ) -> Result<(Session, bool), &'static str> {
        if let Some(session) = self.sessions.values().find(|session| {
            session.provider == provider && session.status == Status::Pending
        }) {
            return if session.expected_account == expected {
                Ok((session.clone(), false))
            } else {
                Err("BUSY")
            };
        }
        // A cancelled request may still be constructing its window. Wait for
        // that construction to finish so it cannot attach to a newer request.
        if self.sessions.values().any(|session| session.provider == provider && session.opening) {
            return Err("BUSY");
        }
        if self.sessions.len() >= MAX_SESSIONS {
            let oldest = self.sessions.values()
                .filter(|session| session.status != Status::Pending && !session.opening)
                .min_by_key(|session| session.expires_at)
                .map(|session| session.session_id.clone());
            if let Some(id) = oldest { self.sessions.remove(&id); }
            else { return Err("BUSY"); }
        }
        let session = Session {
            session_id: Uuid::new_v4().to_string(),
            provider,
            status: Status::Pending,
            account_ref: None,
            expires_at: timestamp.saturating_add(SESSION_SECONDS),
            expected_account: expected,
            window_generation: None,
            failure_reason: None,
            opening: true,
        };
        self.sessions.insert(session.session_id.clone(), session.clone());
        Ok((session, true))
    }

    fn opened(&mut self, provider: CloudProvider, new_window: bool, source: AuthorizationWindowSource) -> String {
        if new_window {
            if let Some(previous) = self.windows.remove(&provider) {
                self.finish_window(provider, &previous.generation, Status::Closed, None);
            }
        }
        let window = self.windows.entry(provider).or_insert_with(|| AuthorizationWindow {
            generation: Uuid::new_v4().to_string(),
            // A native window created before Agent tracking may belong to a desktop login.
            used_by_desktop: !new_window,
        });
        if source == AuthorizationWindowSource::Desktop {
            window.used_by_desktop = true;
        }
        let generation = window.generation.clone();
        for session in self.sessions.values_mut().filter(|session| {
            session.provider == provider && (session.status == Status::Pending || session.opening)
                && session.window_generation.is_none()
        }) {
            session.window_generation = Some(generation.clone());
        }
        generation
    }

    fn finish_window(
        &mut self,
        provider: CloudProvider,
        generation: &str,
        status: Status,
        account: Option<&str>,
    ) {
        for session in self.sessions.values_mut().filter(|session| {
            session.provider == provider && session.status == Status::Pending
                && session.window_generation.as_deref() == Some(generation)
        }) {
            session.status = if status == Status::Succeeded
                && session.expected_account.as_deref().is_some_and(|expected| Some(expected) != account)
            {
                Status::AccountMismatch
            } else { status };
            if session.status == Status::Succeeded {
                session.account_ref = account.map(str::to_owned);
            }
        }
    }

    fn closed(&mut self, provider: CloudProvider, generation: &str) {
        self.finish_window(provider, generation, Status::Closed, None);
        if self.windows.get(&provider).is_some_and(|window| window.generation == generation) {
            self.windows.remove(&provider);
        }
    }

    fn agent_can_close(&self, provider: CloudProvider, generation: &str) -> bool {
        self.windows.get(&provider).is_some_and(|window| window.generation == generation && !window.used_by_desktop)
            && !self.sessions.values().any(|session| session.provider == provider
                && session.status == Status::Pending && session.window_generation.as_deref() == Some(generation))
    }

    fn finished_opening(&mut self, id: &str) -> Option<(CloudProvider, String)> {
        let session = self.sessions.get_mut(id)?;
        session.opening = false;
        if matches!(session.status, Status::Cancelled | Status::Expired | Status::Failed) {
            session.window_generation.clone().map(|generation| (session.provider, generation))
        } else { None }
    }
}

#[derive(Default)]
struct AuthorizationSessions(Mutex<Sessions>);

pub(crate) fn register(app: &AppHandle) {
    app.manage(AuthorizationSessions::default());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            expire_sessions(&app).await;
        }
    });
}

async fn expire_sessions(app: &AppHandle) {
    let (expired, windows) = app.try_state::<AuthorizationSessions>()
        .and_then(|state| {
            let mut sessions = state.0.lock().ok()?;
            Some((sessions.expire(now()), sessions.windows.clone()))
        })
        .unwrap_or_default();
    // Also observe windows that existed before Agent state was registered, or
    // closed between native window construction and event-handler attachment.
    for (provider, window) in windows {
        if app.get_webview_window(&format!("cloud-auth-{}", provider.key())).is_none() {
            authorization_window_closed(app, provider, &window.generation);
        }
    }
    for (provider, generation) in expired {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = crate::cloud::commands::cloud_cancel_agent_authorization(
                app, provider, &generation,
            ).await;
        });
    }
}

/// Called under the connector's authorization gate, for a newly created or
/// reused window. The generation identifies the actual window, not its label.
pub(crate) fn authorization_window_opened(
    app: &AppHandle, provider: CloudProvider, new_window: bool, source: AuthorizationWindowSource,
) -> Option<String> {
    app.try_state::<AuthorizationSessions>()
        .and_then(|state| {
            let mut sessions = state.0.lock().ok()?;
            Some(sessions.opened(provider, new_window, source))
        })
}

pub(crate) fn authorization_window_generation(app: &AppHandle, provider: CloudProvider) -> Option<String> {
    app.try_state::<AuthorizationSessions>()
        .and_then(|state| {
            let sessions = state.0.lock().ok()?;
            sessions.windows.get(&provider).map(|window| window.generation.clone())
        })
}

pub(crate) fn authorization_window_can_close(app: &AppHandle, provider: CloudProvider, generation: &str) -> bool {
    app.try_state::<AuthorizationSessions>()
        .and_then(|state| {
            let sessions = state.0.lock().ok()?;
            Some(sessions.agent_can_close(provider, generation))
        }).unwrap_or(false)
}

pub(crate) fn authorization_window_closed(app: &AppHandle, provider: CloudProvider, generation: &str) {
    if let Some(state) = app.try_state::<AuthorizationSessions>() {
        if let Ok(mut sessions) = state.0.lock() {
            sessions.expire(now());
            sessions.closed(provider, generation);
        };
    }
}

/// Only call after the connector has verified and persisted this window's
/// account. A cached account or an unrelated background refresh is insufficient.
pub(crate) fn authorization_window_completed(app: &AppHandle, account: &CloudAccount, generation: &str) {
    if let Some(state) = app.try_state::<AuthorizationSessions>() {
        if let Ok(mut sessions) = state.0.lock() {
            sessions.expire(now());
            let reference = account_ref(account.provider, &account.id);
            sessions.finish_window(account.provider, generation, Status::Succeeded, Some(&reference));
        };
    }
}

fn provider(key: &str) -> Result<CloudProvider, &'static str> {
    ProviderRegistry::global().by_key(key)
        .filter(|descriptor| descriptor.provider.is_enabled())
        .map(|descriptor| descriptor.provider).ok_or("UNSUPPORTED")
}

fn error(code: &str, lang: &str, provider: CloudProvider, reason: &str) -> ApiError {
    let mut error = ApiError::new(code, lang);
    error.provider = Some(provider.key().into());
    error.reason = Some(reason.into());
    error
}

fn session_dto(session: &Session, lang: &str) -> CloudAuthorizationDto {
    let (status, next_action, failure) = match session.status {
        Status::Pending => ("pending", Some("complete_desktop_authorization"), None),
        Status::Succeeded => ("succeeded", None, None),
        Status::AccountMismatch => ("account_mismatch", Some("review_account"), Some(("ACCOUNT_MISMATCH", "account_mismatch"))),
        Status::Cancelled => ("cancelled", Some("await_user_request"), Some(("TASK_CANCELLED", "cancelled"))),
        Status::Closed => ("closed", Some("await_user_request"), Some(("TASK_CANCELLED", "window_closed"))),
        Status::Expired => ("expired", Some("await_user_request"), Some(("INTERACTION_EXPIRED", "session_expired"))),
        Status::Failed => ("failed", Some("inspect_desktop"), Some(("ACTION_FAILED", session.failure_reason.as_deref().unwrap_or("window_unavailable")))),
    };
    let error = failure.map(|(code, reason)| {
        let mut error = error(code, lang, session.provider, reason);
        error.next_action = next_action.map(str::to_owned);
        error
    });
    CloudAuthorizationDto {
        authorization_id: session.session_id.clone(),
        provider: session.provider.key().into(),
        status: status.into(),
        account_ref: session.account_ref.clone(),
        expires_at: session.expires_at,
        next_action: next_action.map(str::to_owned),
        poll_interval_ms: if session.status == Status::Pending { 1000 } else { 0 },
        error,
    }
}

fn success(value: impl Serialize, lang: &str) -> Response {
    match serde_json::to_value(value) {
        Ok(value) => Response::success(value),
        Err(_) => Response::failure("INTERNAL_ERROR", lang),
    }
}

fn current_session(app: &AppHandle, id: &str) -> Result<Session, &'static str> {
    if Uuid::parse_str(id).is_err() { return Err("INVALID_ARGUMENT"); }
    let state = app.try_state::<AuthorizationSessions>().ok_or("INTERNAL_ERROR")?;
    let sessions = state.0.lock().map_err(|_| "INTERNAL_ERROR")?;
    sessions.sessions.get(id).cloned().ok_or("NOT_FOUND")
}

fn fail_session(app: &AppHandle, id: &str) {
    if let Some(state) = app.try_state::<AuthorizationSessions>() {
        if let Ok(mut sessions) = state.0.lock() {
            if let Some(session) = sessions.sessions.get_mut(id) {
                if session.status == Status::Pending {
                    session.status = Status::Failed;
                    session.failure_reason = Some("window_unavailable".into());
                }
            }
        };
    }
}

async fn begin(app: AppHandle, key: &str, expected: Option<String>, lang: &str) -> Response {
    let provider = match provider(key) { Ok(provider) => provider, Err(code) => return Response::failure(code, lang) };
    if expected.as_ref().is_some_and(|value| {
        !value.strip_prefix("acct_").is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
    }) {
        return Response::failure("INVALID_ARGUMENT", lang);
    }
    expire_sessions(&app).await;
    let started = (|| {
        let state = app.try_state::<AuthorizationSessions>().ok_or("INTERNAL_ERROR")?;
        let mut sessions = state.0.lock().map_err(|_| "INTERNAL_ERROR")?;
        sessions.begin(provider, expected, now())
    })();
    let (session, new) = match started { Ok(value) => value, Err(code) => return Response::failure(code, lang) };
    if !new { return success(session_dto(&session, lang), lang); }
    let authorization_id = session.session_id.clone();
    tauri::async_runtime::spawn(async move {
        open_authorization(app, provider, authorization_id).await;
    });
    success(session_dto(&session, lang), lang)
}

async fn open_authorization(app: AppHandle, provider: CloudProvider, authorization_id: String) {
    // This is explicit user-facing authorization. Account/status queries never
    // enter the UI initialization path. WebView creation runs off the UI thread.
    // The host owns this worker even if the CLI/MCP request disconnects.
    if current_session(&app, &authorization_id).is_ok_and(|session| session.status == Status::Pending) {
        if crate::cloud::commands::begin_authorization_for(app.clone(), provider, AuthorizationWindowSource::Agent).await.is_err()
        {
            fail_session(&app, &authorization_id);
        }
    }
    let close = app.try_state::<AuthorizationSessions>().and_then(|state| {
        let mut sessions = state.0.lock().ok()?;
        sessions.finished_opening(&authorization_id)
    });
    if let Some((provider, generation)) = close {
        let _ = crate::cloud::commands::cloud_cancel_agent_authorization(app.clone(), provider, &generation).await;
    }
}

async fn cancel(app: &AppHandle, id: &str, lang: &str) -> Response {
    if Uuid::parse_str(id).is_err() { return Response::failure("INVALID_ARGUMENT", lang); }
    expire_sessions(app).await;
    let cancelled = (|| {
        let state = app.try_state::<AuthorizationSessions>().ok_or("INTERNAL_ERROR")?;
        let mut sessions = state.0.lock().map_err(|_| "INTERNAL_ERROR")?;
        let session = sessions.sessions.get_mut(id).ok_or("NOT_FOUND")?;
        let close = if session.status == Status::Pending {
            session.status = Status::Cancelled;
            session.window_generation.clone().map(|generation| (session.provider, generation))
        } else { None };
        Ok::<_, &'static str>((session.clone(), close))
    })();
    match cancelled {
        Ok((session, close)) => {
            if let Some((provider, generation)) = close {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = crate::cloud::commands::cloud_cancel_agent_authorization(app, provider, &generation).await;
                });
            }
            success(session_dto(&session, lang), lang)
        }
        Err(code) => Response::failure(code, lang),
    }
}

fn account_failure(error: &QuarkError) -> (&'static str, &'static str, &'static str) {
    match error {
        QuarkError::NotAuthorized => ("disconnected", "AUTH_REQUIRED", "not_authorized"),
        QuarkError::AuthorizationExpired => ("expired", "AUTH_REQUIRED", "expired"),
        QuarkError::CaptchaRequired => ("verification_required", "AUTH_REQUIRED", "verification_required"),
        QuarkError::PikpakOperationVerificationRequired(_) | QuarkError::BaiduOperationVerificationRequired
            | QuarkError::BaiduDriveVerificationRequired =>
            ("verification_required", "CLOUD_OPERATION_VERIFICATION_REQUIRED", "operation_verification_required"),
        QuarkError::AuthorizationPending => ("pending", "CLOUD_AUTHORIZATION_PENDING", "authorization_pending"),
        QuarkError::AccountMismatch => ("account_mismatch", "ACCOUNT_MISMATCH", "account_mismatch"),
        QuarkError::WebAuthorizationRequired => ("interactive_required", "AUTH_REQUIRED", "not_authorized"),
        _ => ("unavailable", "ACTION_FAILED", "unavailable"),
    }
}

async fn read_account(app: AppHandle, provider: CloudProvider, lang: String) -> CloudAccountDto {
    let caps = ProviderRegistry::global().get(provider).capabilities;
    let capabilities = [
        ("list", caps.list), ("download", caps.download), ("upload", caps.upload),
        ("create_share", caps.share_create), ("inspect_share", caps.share_inspect),
        ("transfer_share", caps.share_transfer),
    ].into_iter().filter(|(_, enabled)| *enabled).map(|(name, _)| name.to_owned()).collect();
    let result = crate::cloud::commands::cloud_account(app.clone(), provider).await;
    let mut dto = CloudAccountDto {
        provider: provider.key().into(), account_ref: None, display_name: None,
        authorization_state: "unavailable".into(), capabilities,
        permitted_operations: Vec::new(), error: None,
    };
    match result {
        Ok(account) => {
            dto.account_ref = Some(account_ref(provider, &account.id));
            dto.display_name = (!account.display_name.contains(&account.id)
                && !account.display_name.contains("://"))
                .then(|| account.display_name.chars().filter(|c| !c.is_control()).take(128).collect());
            dto.authorization_state = "ready".into();
            if let Some(state) = app.try_state::<crate::commands::AppState>() {
                if let Ok(settings) = state.settings.lock() {
                    if settings.agent_access.enabled {
                        if settings.agent_access.require_cloud_account(provider.key(), &account.id).is_ok() {
                            dto.permitted_operations = [("browse", caps.list), ("download", caps.download),
                                ("upload", caps.upload), ("create_share", caps.share_create)]
                                .into_iter().filter(|(_, enabled)| *enabled).map(|(name, _)|name.to_owned()).collect();
                        }
                    }
                };
            }
        }
        Err(failure) => {
            let (status, code, reason) = account_failure(&failure);
            dto.authorization_state = status.into();
            dto.error = Some(error(code, &lang, provider, reason));
            if let Some(state) = app.try_state::<Arc<CloudState>>() {
                if let Ok(record) = state.auth_supervisor.record(&state.store, provider) {
                    dto.account_ref = record.account_id.as_deref().filter(|id| !id.is_empty()).map(|id| account_ref(provider, id));
                }
            }
        }
    }
    dto
}

async fn accounts(app: AppHandle, filter: Option<&str>, lang: &str) -> Response {
    let providers = match filter {
        Some(key) => match provider(key) { Ok(provider) => vec![provider], Err(code) => return Response::failure(code, lang) },
        None => ProviderRegistry::global().catalog().iter().map(|descriptor| descriptor.provider).collect(),
    };
    let mut jobs = tokio::task::JoinSet::new();
    for provider in providers {
        jobs.spawn(read_account(app.clone(), provider, lang.to_owned()));
    }
    let mut accounts = Vec::new();
    while let Some(result) = jobs.join_next().await {
        match result { Ok(account) => accounts.push(account), Err(_) => return Response::failure("INTERNAL_ERROR", lang) }
    }
    accounts.sort_by(|a, b| a.provider.cmp(&b.provider));
    success(CloudAccountsDto { accounts }, lang)
}

pub(crate) async fn execute(app: AppHandle, request: &CloudRequest, lang: &str) -> Response {
    match request {
        CloudRequest::Accounts { provider } => accounts(app, provider.as_deref(), lang).await,
        CloudRequest::AuthBegin { provider, account_ref } => begin(app, provider, account_ref.clone(), lang).await,
        CloudRequest::AuthStatus { authorization_id } => {
            expire_sessions(&app).await;
            match current_session(&app, authorization_id) {
                Ok(session) => success(session_dto(&session, lang), lang),
                Err(code) => Response::failure(code, lang),
            }
        }
        CloudRequest::AuthCancel { authorization_id } => cancel(&app, authorization_id, lang).await,
        _ => Response::failure("INVALID_ARGUMENT", lang),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(sessions: &mut Sessions, expected: Option<String>, timestamp: i64) -> (String, String) {
        let (session, created) = sessions.begin(CloudProvider::Quark, expected, timestamp).unwrap();
        assert!(created);
        let generation = sessions.opened(CloudProvider::Quark, true, AuthorizationWindowSource::Agent);
        assert!(sessions.finished_opening(&session.session_id).is_none());
        (session.session_id, generation)
    }

    #[test]
    fn cli_cloud_auth_reuses_pending_request_but_rejects_another_expected_account() {
        let mut sessions = Sessions::default();
        let expected = account_ref(CloudProvider::Quark, "account-a");
        let (first, _) = sessions.begin(CloudProvider::Quark, Some(expected.clone()), 10).unwrap();
        let (retry, created) = sessions.begin(CloudProvider::Quark, Some(expected), 11).unwrap();
        assert!(!created);
        assert_eq!(first.session_id, retry.session_id);
        assert_eq!(retry.expires_at, 310);
        assert_eq!(sessions.begin(CloudProvider::Quark, Some(account_ref(CloudProvider::Quark, "account-b")), 12).unwrap_err(), "BUSY");
    }

    #[test]
    fn cli_cloud_auth_window_completion_checks_expected_account_without_disclosing_actual_id() {
        let mut sessions = Sessions::default();
        let expected = account_ref(CloudProvider::Quark, "expected-private-account");
        let actual = account_ref(CloudProvider::Quark, "other-private-account");
        let (id, generation) = opened(&mut sessions, Some(expected.clone()), 0);
        sessions.finish_window(CloudProvider::Quark, &generation, Status::Succeeded, Some(&actual));
        let session = sessions.sessions.get(&id).unwrap();
        assert_eq!(session.status, Status::AccountMismatch);
        let serialized = serde_json::to_string(&session_dto(session, "en")).unwrap();
        for hidden in ["expected-private-account", "other-private-account", expected.as_str(), actual.as_str(), generation.as_str()] {
            assert!(!serialized.contains(hidden));
        }
        assert!(session.account_ref.is_none());
    }

    #[test]
    fn cli_cloud_auth_old_window_events_cannot_complete_a_new_session_or_resurrect_closed_one() {
        let mut sessions = Sessions::default();
        let (old_id, old_generation) = opened(&mut sessions, None, 0);
        sessions.closed(CloudProvider::Quark, &old_generation);
        let (new_id, new_generation) = opened(&mut sessions, None, 1);
        let account = account_ref(CloudProvider::Quark, "private-account");
        sessions.finish_window(CloudProvider::Quark, &old_generation, Status::Succeeded, Some(&account));
        sessions.closed(CloudProvider::Quark, &old_generation);
        assert_eq!(sessions.sessions[&old_id].status, Status::Closed);
        assert_eq!(sessions.sessions[&new_id].status, Status::Pending);
        assert_eq!(sessions.windows[&CloudProvider::Quark].generation, new_generation);
        sessions.finish_window(CloudProvider::Quark, &new_generation, Status::Succeeded, Some(&account));
        sessions.closed(CloudProvider::Quark, &new_generation);
        assert_eq!(sessions.sessions[&new_id].status, Status::Succeeded);
    }

    #[test]
    fn cli_cloud_auth_expiry_closes_only_exclusively_agent_opened_windows() {
        use AuthorizationWindowSource::{Agent, Desktop};
        let provider = CloudProvider::Baidu;
        for (initial, reused, may_close) in [(Agent, Agent, true), (Desktop, Agent, false), (Agent, Desktop, false)] {
            let mut sessions = Sessions::default();
            let (session, _) = sessions.begin(provider, None, 0).unwrap();
            let generation = sessions.opened(provider, true, initial);
            sessions.finished_opening(&session.session_id);
            assert_eq!(sessions.opened(provider, false, reused), generation);
            assert!(!sessions.agent_can_close(provider, &generation));
            sessions.expire(SESSION_SECONDS);
            assert_eq!(sessions.agent_can_close(provider, &generation), may_close);
            let next = sessions.opened(provider, true, Agent);
            assert!(!sessions.agent_can_close(provider, &generation));
            assert!(sessions.agent_can_close(provider, &next));
        }
    }

    #[test]
    fn cli_cloud_auth_expiry_is_terminal_even_when_the_login_finishes_later() {
        let mut sessions = Sessions::default();
        let (id, generation) = opened(&mut sessions, None, 100);
        assert!(sessions.expire(399).is_empty());
        assert_eq!(sessions.expire(400), vec![(CloudProvider::Quark, generation.clone())]);
        sessions.finish_window(CloudProvider::Quark, &generation, Status::Succeeded, Some("late-account-ref"));
        assert_eq!(sessions.sessions[&id].status, Status::Expired);
        let dto = session_dto(&sessions.sessions[&id], "zh-CN");
        assert_eq!(dto.next_action.as_deref(), Some("await_user_request"));
        assert_eq!(dto.error.unwrap().reason.as_deref(), Some("session_expired"));
    }

    #[test]
    fn cli_cloud_auth_cancelling_during_window_creation_waits_for_its_own_window() {
        let mut sessions = Sessions::default();
        let (session, _) = sessions.begin(CloudProvider::Quark, None, 0).unwrap();
        sessions.sessions.get_mut(&session.session_id).unwrap().status = Status::Cancelled;
        assert_eq!(sessions.begin(CloudProvider::Quark, None, 1).unwrap_err(), "BUSY");
        let generation = sessions.opened(CloudProvider::Quark, true, AuthorizationWindowSource::Agent);
        assert_eq!(sessions.finished_opening(&session.session_id), Some((CloudProvider::Quark, generation.clone())));
        sessions.finish_window(CloudProvider::Quark, &generation, Status::Succeeded, Some("already-logged-in"));
        assert_eq!(sessions.sessions[&session.session_id].status, Status::Cancelled);
        let (new_id, _) = opened(&mut sessions, None, 2);
        assert_eq!(sessions.sessions[&new_id].status, Status::Pending);
    }

    #[test]
    fn cli_cloud_auth_history_is_bounded_without_evicting_pending_authorization() {
        let mut sessions = Sessions::default();
        let (pending, _) = sessions.begin(CloudProvider::Baidu, None, 0).unwrap();
        sessions.finished_opening(&pending.session_id);
        for timestamp in 0..(MAX_SESSIONS + 20) {
            let (id, generation) = opened(&mut sessions, None, timestamp as i64);
            sessions.closed(CloudProvider::Quark, &generation);
            assert_eq!(sessions.sessions[&id].status, Status::Closed);
        }
        assert_eq!(sessions.sessions.len(), MAX_SESSIONS);
        assert_eq!(sessions.sessions[&pending.session_id].status, Status::Pending);
    }

    #[test]
    fn cli_cloud_auth_account_refs_are_provider_scoped_and_failures_keep_distinct_guidance() {
        let quark = account_ref(CloudProvider::Quark, "private-account-id");
        assert!(quark.starts_with("acct_"));
        assert_eq!(quark.len(), 69);
        assert_ne!(quark, account_ref(CloudProvider::Baidu, "private-account-id"));
        assert!(!quark.contains("private-account-id"));
        assert_eq!(account_failure(&QuarkError::NotAuthorized), ("disconnected", "AUTH_REQUIRED", "not_authorized"));
        assert_eq!(account_failure(&QuarkError::AuthorizationExpired), ("expired", "AUTH_REQUIRED", "expired"));
        assert_eq!(account_failure(&QuarkError::CaptchaRequired), ("verification_required", "AUTH_REQUIRED", "verification_required"));
        assert_eq!(account_failure(&QuarkError::BaiduOperationVerificationRequired), ("verification_required", "CLOUD_OPERATION_VERIFICATION_REQUIRED", "operation_verification_required"));
        assert_eq!(account_failure(&QuarkError::AuthorizationPending), ("pending", "CLOUD_AUTHORIZATION_PENDING", "authorization_pending"));
        assert_eq!(account_failure(&QuarkError::Storage), ("unavailable", "ACTION_FAILED", "unavailable"));
    }
}
