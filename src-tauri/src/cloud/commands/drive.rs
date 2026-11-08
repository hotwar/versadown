use super::*;

pub fn restore_account(
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
) -> Result<CloudAccount, QuarkError> {
    ProviderRegistry::global()
        .driver(provider, store, http)
        .verified_account()
}

/// Keep the persisted owner aligned with the credentials in the provider's
/// single authorization slot. Startup recovery and later downloads can then
/// compare task ownership without querying the provider again.
fn record_restored_account(
    state: &CloudState,
    provider: CloudProvider,
    account: &CloudAccount,
) -> Result<(), QuarkError> {
    state.auth_supervisor.ready(
        &state.store,
        provider,
        Some(account.id.clone()),
        Some(account.auth_method),
        auth_supervisor::session_expiry(&state.store, provider)?,
    )?;
    Ok(())
}

pub(crate) fn list_cloud_directory(
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    parent_id: &str,
) -> Result<CloudDirectory, QuarkError> {
    ProviderRegistry::global()
        .driver(provider, store, http)
        .list_directory(parent_id)
}

pub(crate) fn restore_provider_recycle_entries(
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    file_ids: &[String],
) -> Result<(), QuarkError> {
    ProviderRegistry::global()
        .driver(provider, store, http)
        .restore(file_ids)
}

pub(crate) fn delete_provider_recycle_entries(
    provider: CloudProvider,
    store: &impl CredentialStore,
    http: &impl CloudHttpTransport,
    file_ids: &[String],
) -> Result<(), QuarkError> {
    ProviderRegistry::global()
        .driver(provider, store, http)
        .delete(file_ids)
}

#[tauri::command]
pub async fn cloud_account(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAccount, QuarkError> {
    // `record_restored_account` uses the credential supervisor's synchronous
    // provider guard. Keep it in `run_session_work`'s blocking worker instead
    // of calling it after `.await` on Tauri's Tokio runtime thread.
    run_session_work(app, provider, SessionWorkKind::Read, move |s, h| {
        let account = restore_account(provider, &s.store, h)?;
        record_restored_account(s, provider, &account)?;
        Ok(account)
    })
    .await
}

#[tauri::command]
pub async fn cloud_xunlei_account_profile(
    app: AppHandle,
) -> Result<xunlei::XunleiAccountProfile, QuarkError> {
    run_session_work(
        app,
        CloudProvider::Xunlei,
        SessionWorkKind::Read,
        |state, http| {
            with_xunlei_client(&state.store, http, |client| client.account_card_profile())
        },
    )
    .await
}

#[tauri::command]
pub async fn cloud_account_capacity(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<CloudAccountCapacity, QuarkError> {
    run_session_work(app, provider, SessionWorkKind::Read, move |s, h| {
        let mut driver = ProviderRegistry::global().driver(provider, &s.store, h);
        let account = driver.account()?;
        let capacity = driver.storage_capacity()?;
        Ok(CloudAccountCapacity {
            provider,
            account_id: account.id,
            capacity,
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        })
    })
    .await
}

async fn emit_cloud_capacity_invalidated(
    app: AppHandle,
    provider: CloudProvider,
    reason: CloudCapacityChangeReason,
) {
    let account = run_session_work(app.clone(), provider, SessionWorkKind::Read, move |s, h| {
        restore_account(provider, &s.store, h)
    })
    .await;
    if let Ok(account) = account {
        let _ = app.emit(
            "cloud-capacity-invalidated",
            CloudCapacityInvalidated {
                provider,
                account_id: account.id,
                reason,
            },
        );
    }
}

pub(crate) fn schedule_cloud_capacity_invalidation(
    app: AppHandle,
    provider: CloudProvider,
    reason: CloudCapacityChangeReason,
) {
    tauri::async_runtime::spawn(async move {
        emit_cloud_capacity_invalidated(app, provider, reason).await;
    });
}

#[tauri::command]
pub async fn cloud_list_directory(
    app: AppHandle,
    provider: CloudProvider,
    parent_id: String,
) -> Result<CloudDirectory, QuarkError> {
    run_session_work(app, provider, SessionWorkKind::Read, move |s, h| {
        list_cloud_directory(provider, &s.store, h, &parent_id)
    })
    .await
}

fn validate_share_options(
    provider: CloudProvider,
    options: &CloudShareOptions,
) -> Result<(), QuarkError> {
    if options.name.trim().is_empty() {
        return Err(QuarkError::InvalidInput("分享名称不能为空".into()));
    }
    let constraint = ProviderRegistry::global().get(provider).share_pass_code_constraint;
    if constraint.is_some_and(|constraint| constraint.required)
        && options.pass_code.is_none() && !options.generate_pass_code
    {
        return Err(QuarkError::InvalidInput("提取码格式无效".into()));
    }
    if let Some(pass_code) = &options.pass_code {
        let pass_code = pass_code.trim();
        let has_valid_provider_format = if let Some(constraint) = constraint {
            let length = pass_code.chars().count();
            length >= usize::from(constraint.min_length) && length <= usize::from(constraint.max_length)
                && (constraint.character_set != CloudSharePassCodeCharacterSet::AsciiAlphanumeric
                    || pass_code.chars().all(|character| character.is_ascii_alphanumeric()))
        } else {
            !pass_code.is_empty() && pass_code.chars().count() <= 32
        };
        if !has_valid_provider_format {
            return Err(QuarkError::InvalidInput("提取码格式无效".into()));
        }
    }
    if options.generate_pass_code && options.pass_code.is_some() {
        return Err(QuarkError::InvalidInput(
            "不能同时指定提取码和自动生成提取码".into(),
        ));
    }
    if let Some(days) = options.expires_in_days {
        if days == 0 || days > 365 {
            return Err(QuarkError::InvalidInput(
                "有效期必须在 1 到 365 天之间".into(),
            ));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn cloud_create_share(
    app: AppHandle,
    provider: CloudProvider,
    file_ids: Vec<String>,
    options: CloudShareOptions,
) -> Result<CloudShare, QuarkError> {
    if file_ids.is_empty() {
        return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
    }
    validate_share_options(provider, &options)?;
    run_session_work(app, provider, SessionWorkKind::Mutation, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .create_share(&file_ids, &options)
    })
    .await
}

#[tauri::command]
pub async fn cloud_list_shares(
    app: AppHandle,
    provider: CloudProvider,
) -> Result<Vec<CloudShare>, QuarkError> {
    run_session_work(app, provider, SessionWorkKind::Read, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .list_shares()
    })
    .await
}

#[tauri::command]
pub async fn cloud_update_share_pass_code(
    app: AppHandle,
    provider: CloudProvider,
    share_id: String,
    pass_code: String,
) -> Result<(), QuarkError> {
    if share_id.trim().is_empty() || pass_code.trim().is_empty() {
        return Err(QuarkError::InvalidInput("分享 ID 和提取码不能为空".into()));
    }
    run_session_work(app, provider, SessionWorkKind::Mutation, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .update_share_pass_code(&share_id, &pass_code)
    })
    .await
}

#[tauri::command]
pub async fn cloud_cancel_shares(
    app: AppHandle,
    provider: CloudProvider,
    share_ids: Vec<String>,
) -> Result<CloudShareCancelOutcome, QuarkError> {
    if share_ids.is_empty() {
        return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
    }
    run_session_work(app, provider, SessionWorkKind::Mutation, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .cancel_shares_detailed(&share_ids)
    })
    .await
}

#[tauri::command]
pub async fn cloud_create_directory(
    app: AppHandle,
    provider: CloudProvider,
    parent_id: String,
    name: String,
) -> Result<CloudFile, QuarkError> {
    valid_name(&name)?;
    run_session_work(app, provider, SessionWorkKind::Mutation, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .create_directory(&parent_id, &name)
    })
    .await
}

#[tauri::command]
pub async fn cloud_rename(
    app: AppHandle,
    provider: CloudProvider,
    file_id: String,
    name: String,
) -> Result<(), QuarkError> {
    valid_name(&name)?;
    run_session_work(app, provider, SessionWorkKind::Mutation, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .rename(&file_id, &name)
    })
    .await
}

#[tauri::command]
pub async fn cloud_move(
    app: AppHandle,
    provider: CloudProvider,
    file_ids: Vec<String>,
    target_parent_id: String,
) -> Result<(), QuarkError> {
    run_session_work(app, provider, SessionWorkKind::Mutation, move |s, h| {
        ProviderRegistry::global()
            .driver(provider, &s.store, h)
            .move_files(&file_ids, &target_parent_id)
    })
    .await
}

#[tauri::command]
pub async fn cloud_copy(
    app: AppHandle,
    provider: CloudProvider,
    file_ids: Vec<String>,
    target_parent_id: String,
) -> Result<(), QuarkError> {
    let changed = !file_ids.is_empty();
    run_session_work(
        app.clone(),
        provider,
        SessionWorkKind::Mutation,
        move |s, h| {
            ProviderRegistry::global()
                .driver(provider, &s.store, h)
                .copy_files(&file_ids, &target_parent_id)
        },
    )
    .await?;
    if changed {
        schedule_cloud_capacity_invalidation(app, provider, CloudCapacityChangeReason::Copy);
    }
    Ok(())
}

#[tauri::command]
pub async fn cloud_trash(
    app: AppHandle,
    provider: CloudProvider,
    file_ids: Vec<String>,
) -> Result<(), QuarkError> {
    let changed = !file_ids.is_empty();
    run_session_work(
        app.clone(),
        provider,
        SessionWorkKind::Mutation,
        move |s, h| trash_provider_files(provider, &s.store, h, &file_ids),
    )
    .await?;
    if changed {
        schedule_cloud_capacity_invalidation(app, provider, CloudCapacityChangeReason::Trash);
    }
    Ok(())
}

pub(crate) fn trash_provider_files(
    provider: CloudProvider,
    store: &impl CredentialStore,
    h: &impl CloudHttpTransport,
    file_ids: &[String],
) -> Result<(), QuarkError> {
    ProviderRegistry::global()
        .driver(provider, store, h)
        .trash(file_ids)
}

#[tauri::command]
pub async fn cloud_delete(
    app: AppHandle,
    provider: CloudProvider,
    file_ids: Vec<String>,
) -> Result<(), QuarkError> {
    let changed = !file_ids.is_empty();
    run_session_work(
        app.clone(),
        provider,
        SessionWorkKind::Mutation,
        move |s, h| delete_provider_recycle_entries(provider, &s.store, h, &file_ids),
    )
    .await?;
    if changed {
        schedule_cloud_capacity_invalidation(app, provider, CloudCapacityChangeReason::Delete);
    }
    Ok(())
}

#[tauri::command]
pub async fn cloud_restore(
    app: AppHandle,
    provider: CloudProvider,
    file_ids: Vec<String>,
) -> Result<(), QuarkError> {
    let changed = !file_ids.is_empty();
    run_session_work(
        app.clone(),
        provider,
        SessionWorkKind::Mutation,
        move |s, h| restore_provider_recycle_entries(provider, &s.store, h, &file_ids),
    )
    .await?;
    if changed {
        schedule_cloud_capacity_invalidation(app, provider, CloudCapacityChangeReason::Restore);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{auth_supervisor::AuthHealth, CloudAuthMethod, CredentialStore};

    fn options(pass_code: String) -> CloudShareOptions {
        CloudShareOptions {
            name: "test".into(),
            pass_code: Some(pass_code),
            generate_pass_code: false,
            expires_in_days: None,
        }
    }

    #[test]
    fn enforces_each_provider_share_code_constraint_before_submitting() {
        assert!(validate_share_options(CloudProvider::Aliyun, &options("a".repeat(64))).is_ok());
        assert!(validate_share_options(CloudProvider::Aliyun, &options("a".repeat(65))).is_err());
        assert!(validate_share_options(CloudProvider::Quark, &options("a1B2".into())).is_ok());
        assert!(validate_share_options(CloudProvider::Quark, &options("a1!2".into())).is_err());
        assert!(validate_share_options(CloudProvider::Quark, &options("a1B".into())).is_err());
        assert!(validate_share_options(CloudProvider::Baidu, &options("a1B2".into())).is_ok());
        assert!(validate_share_options(CloudProvider::Baidu, &options("a1B".into())).is_err());
        let mut automatic = options("".into());
        automatic.pass_code = None;
        assert!(validate_share_options(CloudProvider::Baidu, &automatic).is_err());
        automatic.generate_pass_code = true;
        assert!(validate_share_options(CloudProvider::Baidu, &automatic).is_ok());
    }

    #[test]
    fn startup_reconnect_replaces_the_persisted_account_owner() {
        let root = tempfile::tempdir().unwrap();
        let state = CloudState::new(root.path().join("sessions")).unwrap();
        state.store.save("uc", b"uc-session=fixture").unwrap();
        state
            .auth_supervisor
            .ready(
                &state.store,
                CloudProvider::Uc,
                Some("previous-account".into()),
                Some(CloudAuthMethod::Web),
                None,
            )
            .unwrap();
        let account = CloudAccount {
            provider: CloudProvider::Uc,
            id: "uc-account-1".into(),
            display_name: "fixture".into(),
            auth_method: CloudAuthMethod::Web,
        };

        record_restored_account(&state, CloudProvider::Uc, &account).unwrap();

        let record = state
            .auth_supervisor
            .record(&state.store, CloudProvider::Uc)
            .unwrap();
        assert_eq!(record.health, AuthHealth::Ready);
        assert_eq!(record.account_id.as_deref(), Some("uc-account-1"));
    }
}
