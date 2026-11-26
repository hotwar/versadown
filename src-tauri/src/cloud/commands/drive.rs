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
    context: &SessionWorkContext<'_>,
    provider: CloudProvider,
    account: &CloudAccount,
) -> Result<(), QuarkError> {
    if account.provider != provider {
        return Err(CloudError::AccountMismatch);
    }
    context.auth_supervisor.observe_account(
        &context.state.store,
        context.snapshot.ok_or(CloudError::Storage)?,
        account,
    )
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

/// Resolve account ownership and read the directory in one session operation.
/// The caller exposes only opaque local selection handles to the Agent.
pub(crate) async fn prepare_agent_cloud_directory(
    app: AppHandle,
    provider: CloudProvider,
    expected: Option<(String, String)>,
) -> Result<(String, CloudDirectory), QuarkError> {
    run_session_work(app, provider, SessionWorkKind::Read, move |state, http| {
        let (account,directory)=read_agent_cloud_directory(provider,&state.store,http,expected.as_ref())?;
        record_restored_account(state, provider, &account)?;
        Ok((account.id, directory))
    }).await
}

fn read_agent_cloud_directory(
    provider:CloudProvider,
    credentials:&impl CredentialStore,
    http:&impl CloudHttpTransport,
    expected:Option<&(String,String)>,
)->Result<(CloudAccount,CloudDirectory),QuarkError> {
    let account=restore_account(provider,credentials,http)?;
    if let Some((owner,_))=expected {validate_download_account(Some(&account.id),owner)?;}
    let parent=expected.map(|(_,id)|id.as_str()).unwrap_or(ProviderRegistry::global().get(provider).root_directory_id);
    Ok((account,list_cloud_directory(provider,credentials,http,parent)?))
}

pub(crate) fn validate_share_options(
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
    // Reject known provider policy mismatches before accepting an Agent write
    // or entering a provider session. These reflect the existing drivers;
    // never silently replace a custom/public policy with a generated code.
    if !provider.is_enabled() {
        return Err(QuarkError::ProviderUnsupported);
    }
    if !ProviderRegistry::global().get(provider).capabilities.share_configurable
        && (options.pass_code.is_some() || options.generate_pass_code || options.expires_in_days.is_some())
    {
        return Err(QuarkError::ProviderUnsupported);
    }
    match provider {
        CloudProvider::Quark | CloudProvider::Uc | CloudProvider::Aliyun
            if options.generate_pass_code => return Err(QuarkError::ProviderUnsupported),
        CloudProvider::Pikpak if options.pass_code.is_some() => return Err(QuarkError::ProviderUnsupported),
        CloudProvider::Xunlei if options.pass_code.is_some() || !options.generate_pass_code => {
            return Err(QuarkError::ProviderUnsupported);
        }
        _ => {}
    }
    if provider == CloudProvider::Uc && !matches!(options.expires_in_days, None | Some(1 | 7 | 30 | 60 | 180)) {
        return Err(QuarkError::ProviderUnsupported);
    }
    Ok(())
}

/// Recheck the host-created ancestry with the provider. A preview cannot grant
/// access to a file/folder that has since moved outside its authorized root.
pub(crate) fn validate_agent_cloud_path(
    provider: CloudProvider, credentials: &impl CredentialStore, http: &impl CloudHttpTransport,
    account_id: &str, chain: &[String], file_id: &str, folder: bool,
) -> Result<(), QuarkError> {
    validate_agent_cloud_chain_shape(chain)?;
    let account=restore_account(provider,credentials,http)?;
    validate_download_account(Some(&account.id),account_id)?;
    validate_agent_cloud_path_with(chain, file_id, folder, |parent| {
        list_cloud_directory(provider, credentials, http, parent)
    })
}

fn validate_agent_cloud_chain_shape(chain: &[String]) -> Result<(), QuarkError> {
    if chain.is_empty() || chain.len() > 64 {
        return Err(QuarkError::InvalidInput("agent.error.forbidden".into()));
    }
    Ok(())
}

/// The ancestry is host-created, but must still match current provider listings.
/// Names are presentation only: stable IDs and file kinds establish membership.
fn validate_agent_cloud_path_with(
    chain: &[String],
    file_id: &str,
    folder: bool,
    mut list: impl FnMut(&str) -> Result<CloudDirectory, QuarkError>,
) -> Result<(), QuarkError> {
    validate_agent_cloud_chain_shape(chain)?;
    for pair in chain.windows(2) {
        let listing = list(&pair[0])?;
        if !listing.items.iter().any(|file| file.id == pair[1] && file.kind == CloudFileKind::Folder) {
            return Err(QuarkError::InvalidInput("agent.error.forbidden".into()));
        }
    }
    let parent = chain.last().ok_or(QuarkError::Storage)?;
    // Listing the root still validates that it is an accessible directory.
    let listing = list(parent)?;
    if parent == file_id && folder {
        return Ok(());
    }
    if listing.items.iter().any(|file| file.id == file_id && (file.kind == CloudFileKind::Folder) == folder) {
        Ok(())
    } else {
        Err(QuarkError::SourceFileMissing)
    }
}

#[cfg(test)]
#[path = "agent_path_tests.rs"]
mod agent_path_tests;

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

    fn share_options(code: Option<&str>, generated: bool, days: Option<u16>) -> CloudShareOptions {
        CloudShareOptions { name: "fixture-share".into(), pass_code: code.map(str::to_owned),
            generate_pass_code: generated, expires_in_days: days }
    }

    #[test]
    fn agent_share_options_reject_known_unsupported_provider_modes_before_network() {
        for provider in [CloudProvider::Quark, CloudProvider::Uc, CloudProvider::Aliyun] {
            assert!(matches!(validate_share_options(provider, &share_options(None, true, Some(7))),
                Err(QuarkError::ProviderUnsupported)), "{provider:?}");
            assert!(validate_share_options(provider, &share_options(Some("aB12"), false, Some(7))).is_ok(), "{provider:?}");
        }
        assert!(matches!(validate_share_options(CloudProvider::Pikpak, &share_options(Some("aB12"), false, Some(7))),
            Err(QuarkError::ProviderUnsupported)));
        for generated in [false, true] {
            assert!(validate_share_options(CloudProvider::Pikpak, &share_options(None, generated, Some(7))).is_ok());
        }
        for options in [share_options(None, false, None), share_options(Some("aB12"), false, Some(7))] {
            assert!(matches!(validate_share_options(CloudProvider::Xunlei, &options), Err(QuarkError::ProviderUnsupported)));
        }
        assert!(validate_share_options(CloudProvider::Xunlei, &share_options(None, true, Some(7))).is_ok());
        assert!(validate_share_options(CloudProvider::Baidu, &share_options(Some("aB12"), false, Some(7))).is_ok());
        assert!(validate_share_options(CloudProvider::Baidu, &share_options(None, true, Some(7))).is_ok());
        assert!(matches!(validate_share_options(CloudProvider::Baidu, &share_options(None, false, None)), Err(QuarkError::InvalidInput(_))));
    }

    #[test]
    fn agent_share_options_keep_uc_expiry_choices_and_reject_conflicting_pass_codes() {
        for days in [None, Some(1), Some(7), Some(30), Some(60), Some(180)] {
            assert!(validate_share_options(CloudProvider::Uc, &share_options(Some("aB12"), false, days)).is_ok());
        }
        for days in [2, 14, 90, 365] {
            assert!(matches!(validate_share_options(CloudProvider::Uc, &share_options(Some("aB12"), false, Some(days))),
                Err(QuarkError::ProviderUnsupported)));
        }
        assert!(matches!(validate_share_options(CloudProvider::Baidu, &share_options(Some("aB12"), true, Some(7))),
            Err(QuarkError::InvalidInput(_))));
        assert!(matches!(validate_share_options(CloudProvider::Quark, &share_options(Some("aB12"), false, Some(0))),
            Err(QuarkError::InvalidInput(_))));
        // Quark currently forwards expired_days together with its expiry type;
        // do not infer a different undocumented provider mapping here.
        assert!(validate_share_options(CloudProvider::Quark, &share_options(Some("aB12"), false, Some(30))).is_ok());
    }

    #[test]
    fn agent_resource_cloud_prepare_is_read_only_and_keeps_owner_bound() {
        use std::collections::VecDeque;
        struct MetadataHttp {requests:Mutex<Vec<CloudHttpRequest>>,replies:Mutex<VecDeque<&'static str>>}
        impl CloudHttpTransport for MetadataHttp {
            fn send(&self,request:CloudHttpRequest)->Result<CloudHttpResponse,QuarkError> {
                assert_eq!(request.method,"GET");self.requests.lock().unwrap().push(request);
                Ok(CloudHttpResponse{status:200,body:self.replies.lock().unwrap().pop_front().expect("unexpected provider request").into(),..Default::default()})
            }
        }
        let credentials=crate::cloud::MemoryCredentialStore::default();
        let http=MetadataHttp{requests:Mutex::new(vec![]),replies:Mutex::new(VecDeque::from([
            r#"{"status":200,"data":{"uid":"owner-1","nickname":"fixture"}}"#,
            r#"{"status":200,"data":{"list":[{"fid":"file-1","file_name":"one.txt","file":true,"size":7}]}}"#,
            r#"{"status":200,"data":{"uid":"other-owner","nickname":"fixture"}}"#,
        ]))};
        assert!(matches!(read_agent_cloud_directory(CloudProvider::Quark,&credentials,&http,None),Err(QuarkError::NotAuthorized)));
        assert!(http.requests.lock().unwrap().is_empty());
        credentials.save("quark",b"cookie=fixture").unwrap();
        let expected=("owner-1".into(),"folder-1".into());
        let(account,listing)=read_agent_cloud_directory(CloudProvider::Quark,&credentials,&http,Some(&expected)).unwrap();
        assert_eq!(account.id,"owner-1");assert_eq!(listing.items[0].name,"one.txt");
        assert!(matches!(read_agent_cloud_directory(CloudProvider::Quark,&credentials,&http,Some(&expected)),Err(QuarkError::AccountMismatch)));
        let requests=http.requests.lock().unwrap();assert_eq!(requests.len(),3);assert!(requests[1].url.contains("pdir_fid=folder-1"));
    }

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
    fn delayed_account_read_cannot_publish_after_logout_or_switch() {
        let store = MemoryCredentialStore::default();
        let supervisor = auth_supervisor::AuthSupervisor::default();
        let provider = CloudProvider::Baidu;
        let account = CloudAccount {
            provider, id: "first".into(), display_name: "fixture".into(), auth_method: CloudAuthMethod::Web,
        };
        for switch in [false, true] {
            store.save(provider.key(), b"first").unwrap();
            supervisor.ready(&store, provider, Some(account.id.clone()), Some(account.auth_method), None).unwrap();
            let result = supervisor.run_with_snapshot(&store, provider, SessionWorkKind::Read, |_, observed| {
                store.remove(provider.key())?;
                supervisor.disconnect(&store, provider)?;
                if switch {
                    store.save(provider.key(), b"second")?;
                    supervisor.ready(&store, provider, Some("second".into()), Some(CloudAuthMethod::Web), None)?;
                }
                supervisor.observe_account(&store, observed.unwrap(), &account)
            }, |_| panic!("cannot recover an obsolete read"), || Ok(()));
            assert!(matches!(result, Err(CloudError::AccountMismatch | CloudError::NotAuthorized)));
            assert_eq!(supervisor.record(&store, provider).unwrap().account_id.as_deref(), switch.then_some("second"));
        }
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

        state.auth_supervisor.run_with_snapshot(
            &state.store, CloudProvider::Uc, SessionWorkKind::Read,
            |_, snapshot| state.auth_supervisor.observe_account(&state.store, snapshot.unwrap(), &account),
            |_| panic!("unexpected recovery"), || Ok(()),
        ).unwrap();

        let record = state
            .auth_supervisor
            .record(&state.store, CloudProvider::Uc)
            .unwrap();
        assert_eq!(record.health, AuthHealth::Ready);
        assert_eq!(record.account_id.as_deref(), Some("uc-account-1"));
    }
}
