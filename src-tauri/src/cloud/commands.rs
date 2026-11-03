pub use super::ResolvedCloudDownload;
use super::*;
use crate::models::{
    CloudDirectoryGroup, NewCloudDirectoryGroup, NewCloudDirectoryGroupItem, QueueTaskPayload,
};
use crate::{
    commands::{
        cloud_directory_group_queue_task, ensure_private_download_enabled,
        ensure_private_group_access, ensure_private_task_access, should_emit_task_update,
        task_action_outcome, AppState, TaskActionOutcome,
    },
    duplicate_state, DownloadTask, NewTask, TaskStatus, TaskStore, UriSubmissionOptions,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use tauri::{
    webview::{NewWindowResponse, PageLoadEvent},
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_dialog::DialogExt;

mod auth;
mod browser;
mod common;
pub(crate) use browser::*;
mod download;
mod drive;
#[path = "commands/share.rs"]
mod share_commands;
#[path = "commands/upload.rs"]
mod upload_commands;
pub use auth::*;
pub use common::*;
pub use download::*;
pub use drive::*;
pub use share_commands::*;
pub use upload_commands::*;
#[cfg(test)]
#[path = "commands/tests.rs"]
mod tests;

pub struct CloudState {
    pub(crate) credential_restore_failed: bool,
    pub store: SystemCredentialStore,
    pub share_plans: share::CloudSharePlanStore,
    pub share_refresh_budget: share::CloudShareRefreshBudget,
    pub(crate) cloud_download_refresh_budget: browser_login::CloudDownloadRefreshBudget,
    authorization_resumption: CloudDownloadSourceGate,
    authorization_completion: CloudDownloadSourceGate,
    authorization_results: Mutex<HashMap<CloudProvider, CloudAccount>>,
    pub auth_supervisor: auth_supervisor::AuthSupervisor,
    application: OnceLock<AppHandle>,
    http: ReqwestTransport,
}

impl CloudState {
    pub(crate) fn bind_application(&self, app: AppHandle) {
        let warning_app = app.clone();
        self.auth_supervisor.on_persistence_warning(move |warning| {
            let _ = warning_app.emit("cloud-session-persistence-warning", warning);
        });
        let _ = self.application.set(app);
    }

    pub fn remember_authorization_completion(
        &self,
        account: &CloudAccount,
    ) -> Result<(), QuarkError> {
        authorization_completion_event(&self.auth_supervisor, &self.store, account)?;
        self.authorization_results
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .insert(account.provider, account.clone());
        Ok(())
    }
    pub fn completed_authorization(
        &self,
        provider: CloudProvider,
    ) -> Result<Option<CloudAccount>, QuarkError> {
        let account = self
            .authorization_results
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .get(&provider)
            .cloned();
        match account {
            Some(account)
                if authorization_completion_event(&self.auth_supervisor, &self.store, &account)
                    .is_ok() =>
            {
                Ok(Some(account))
            }
            _ => Ok(None),
        }
    }
    pub fn clear_authorization_completion(
        &self,
        provider: CloudProvider,
    ) -> Result<(), QuarkError> {
        self.authorization_results
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .remove(&provider);
        Ok(())
    }

    pub(crate) fn preflight_tianyi_persisted_state(&self) -> Result<(), QuarkError> {
        let root = self.store.root();
        if !root.is_absolute() {
            return Err(QuarkError::Storage);
        }
        let app_data = root.parent().ok_or(QuarkError::Storage)?;
        let profile = app_data.join("web-auth").join(CloudProvider::Tianyi.key());
        match std::fs::symlink_metadata(profile) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
            Ok(_) => Err(QuarkError::Storage),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(QuarkError::Storage),
        }
    }

    /// Deletes the stored and in-memory authorization material for Tianyi.
    /// This is deliberately separate from task payload removal so callers can
    /// retain credentials if path preflight rejects a destructive purge.
    pub(crate) fn purge_tianyi_persisted_state(&self) -> Result<usize, QuarkError> {
        let provider = CloudProvider::Tianyi;
        self.preflight_tianyi_persisted_state()?;
        let removed_plans = self.share_plans.remove_provider(provider)?;
        for key in [
            "tianyi",
            "cloud-password-tianyi",
            "cloud-password-draft-tianyi",
            "cloud-password-profile-reset-tianyi",
            "cloud-auth-maintenance-tianyi",
        ] {
            self.store.remove(key)?;
        }
        self.auth_supervisor.forget(provider)?;
        self.clear_authorization_completion(provider)?;

        let root = self.store.root();
        let app_data = root.parent().ok_or(QuarkError::Storage)?;
        let profile = app_data.join("web-auth").join(provider.key());
        match std::fs::symlink_metadata(&profile) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(profile).map_err(|_| QuarkError::Storage)?;
            }
            Ok(_) => return Err(QuarkError::Storage),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(QuarkError::Storage),
        }
        Ok(removed_plans)
    }
    pub fn new(root: PathBuf) -> Result<Self, QuarkError> {
        let mut state = Self {
            credential_restore_failed: false,
            share_plans: share::CloudSharePlanStore::new(SystemCredentialStore::new(root.clone())?),
            share_refresh_budget: Default::default(),
            cloud_download_refresh_budget: Default::default(),
            authorization_resumption: Default::default(),
            authorization_completion: Default::default(),
            authorization_results: Default::default(),
            auth_supervisor: Default::default(),
            application: OnceLock::new(),
            store: SystemCredentialStore::new(root)?,
            http: ReqwestTransport::new()?,
        };
        state.credential_restore_failed =
            restore_startup_authorizations(&state.auth_supervisor, &state.store);
        Ok(state)
    }
    pub fn start_authorization_maintenance(self: &Arc<Self>) {
        for descriptor in ProviderRegistry::global()
            .all()
            .iter()
            .filter(|descriptor| descriptor.provider.is_enabled())
        {
            let state = Arc::downgrade(self);
            let provider = descriptor.provider;
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    let _ =
                        tauri::async_runtime::spawn_blocking(move || -> Result<(), CloudError> {
                            if !state.auth_supervisor.is_due(&state.store, provider)? {
                                return Ok(());
                            }
                            let revision = state
                                .auth_supervisor
                                .record(&state.store, provider)?
                                .session_revision;
                            state.auth_supervisor.maintain(
                                &state.store,
                                provider,
                                Some(revision),
                                false,
                                || {
                                    auth_supervisor::recover_in_order(
                                        provider,
                                        &state.store,
                                        ProviderRegistry::global()
                                            .driver(provider, &state.store, &state.http)
                                            .auth(),
                                        false,
                                        || Ok(false),
                                    )
                                },
                            )
                        })
                        .await;
                }
            });
        }
    }
}

fn restore_startup_authorizations(
    supervisor: &auth_supervisor::AuthSupervisor,
    store: &impl CredentialStore,
) -> bool {
    let mut failed = false;
    for descriptor in ProviderRegistry::global().all() {
        if let Err(error) = supervisor.restore(store, descriptor.provider) {
            // Failed reads never become persisted disconnects. Credential
            // operations remain authoritative and report the original error.
            eprintln!(
                "Unable to restore {} credentials: {error}",
                descriptor.provider.key()
            );
            failed = true;
        }
    }
    failed
}

#[cfg(test)]
#[path = "commands_startup_tests.rs"]
mod startup_tests;
