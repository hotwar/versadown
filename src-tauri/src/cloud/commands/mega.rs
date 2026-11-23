//! MEGA sign-in. MEGA has its own form (e-mail, password and an optional
//! two-factor code) and never stores the password, so it does not use the
//! generic password-login command, which persists reusable credentials.

use super::*;
use crate::cloud::mega::account::{login, LoginRequest};
use zeroize::Zeroizing;

#[tauri::command]
pub async fn cloud_login_mega(
    app: AppHandle,
    email: String,
    password: String,
    second_factor: Option<String>,
) -> Result<CloudAccount, QuarkError> {
    let provider = CloudProvider::Mega;
    require_connected_provider(provider)?;
    let password = Zeroizing::new(password);
    let publish_app = app.clone();
    run(app, provider, move |state, http| {
        let session = login(
            http,
            &LoginRequest {
                email: &email,
                password: &password,
                second_factor: second_factor.as_deref(),
            },
        )?;
        // Replace a previously stored login only after this one is complete.
        session.save(&state.store)?;
        let account = restore_account(provider, &state.store, http)?;
        state.auth_supervisor.ready_locked(
            &state.store,
            provider,
            Some(account.id.clone()),
            Some(account.auth_method),
            None,
        )?;
        publish_authorization_completion(&publish_app, state, &account)?;
        Ok(account)
    })
    .await
}
