use crate::cloud::CredentialStore;
use tauri::{Emitter, Manager};

mod ai;
pub mod aria2;
mod bandwidth;
pub mod cloud;
mod cloud_directory_events;
mod commands;
mod connectors;
mod downloader;
mod duplicates;
pub mod engine_binary;
mod engine_runtime;
mod external_links;
mod ftp;
mod launcher;
mod launcher_drag;
mod launcher_input;
mod launcher_window;
mod link_association;
mod local_upload_picker;
mod magnet_metadata;
mod models;
#[cfg(all(target_os = "macos", feature = "native-smoke"))]
mod native_smoke;
mod output_path;
mod parser;
mod policy;
mod privacy;
mod processing;
mod store;
mod startup;
mod update;
mod webview_memory;
mod window_icon;
#[cfg(windows)]
mod window_interaction;
mod window_lifecycle;
mod xunlei_native;

pub use aria2::*;
pub use commands::list_queue_tasks_for_scope;
pub use connectors::{
    CloudConnector, CloudDirectory, CloudItem, ConnectorError, MockConnector, Provider,
};
pub use downloader::{
    confirm_download, prepare_download, DownloadError, ParseOutcome, ParseResource,
};
pub use duplicates::{duplicate_state, retry_duplicate_state};
pub use engine_runtime::{
    EngineAvailability, EngineRuntime, EngineStateSink, EngineUnavailableError,
};
pub use ftp::{parse_ftp_url, FtpDownloadEngine, FtpDownloadError, FtpDownloadLimits, FtpTarget};
pub use launcher::{
    launcher_menu_entries, should_apply_launcher_window_visibility, tray_menu_entries,
    LauncherInteractionState,
};
pub use magnet_metadata::{
    resolve_magnet_metadata, resolve_magnet_torrent, MagnetMetadataError, ResolvedMagnetFile,
    ResolvedMagnetMetadata, ResolvedMagnetTorrent,
};
pub use models::{
    may_transition, AiSettings, AiSettingsInput, AppSettings, Aria2Settings, DownloadTask,
    DuplicateDownloadState, FtpSegment, LauncherVisibility, MagnetMetadataSettings, NewTask,
    ProxySettings, TaskStatus,
};
pub(crate) use output_path::reserve_unique_output_path;
pub use parser::{
    classify_input, extract_candidates, extract_cloud_share_candidate, scan_download_inputs,
    validate_download_input, validate_task_source, Candidate, CandidateKind, CloudShareCandidate,
    InputValidationError, ScannedInput, ValidatedCandidate,
};
pub use policy::{validate_processing, PolicyError, ProcessAction, ProcessStep};
pub use processing::{plan_processing, ProcessingHint, Resource};
pub use store::{StoreError, TaskStore};
pub use window_lifecycle::{
    manager_close_disposition, should_prevent_implicit_exit, ApplicationLifecycle,
    ManagerCloseDisposition,
};

pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn should_show_windows(background_ui_test: Option<&str>) -> bool {
    background_ui_test != Some("1")
}

pub fn should_enable_peer_discovery(background_ui_test: Option<&str>) -> bool {
    background_ui_test != Some("1")
}

struct TauriDownloadEvents {
    app: tauri::AppHandle,
    store: std::sync::Arc<TaskStore>,
    cloud: std::sync::Arc<cloud::commands::CloudState>,
    directory_refreshes: std::sync::Arc<cloud_directory_events::CloudDirectoryRefreshes>,
    cloud_download_dispatch: cloud::download_dispatch::CloudDownloadDispatch,
    baidu_range_gateway: std::sync::Arc<
        tokio::sync::OnceCell<std::sync::Arc<cloud::baidu_range_gateway::BaiduRangeGateway>>,
    >,
}

impl TauriDownloadEvents {
    fn schedule_directory_refresh(&self, root: uuid::Uuid) {
        if !self.directory_refreshes.queue(root) {
            return;
        }
        let refreshes = std::sync::Arc::clone(&self.directory_refreshes);
        let app = self.app.clone();
        let store = std::sync::Arc::clone(&self.store);
        tauri::async_runtime::spawn(async move {
            refreshes
                .flush_pending(|roots| {
                    let app = app.clone();
                    let store = std::sync::Arc::clone(&store);
                    async move {
                        let _ = tauri::async_runtime::spawn_blocking(move || {
                            for root in roots {
                                let _ = commands::emit_task_update_for_directory_root(
                                    &app, &store, root,
                                );
                            }
                        })
                        .await;
                    }
                })
                .await;
        });
    }
}

impl EventSink for TauriDownloadEvents {
    fn task_updated(&self, task: &DownloadTask) {
        let terminal = matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Recycled
        );
        let current_task =
            self.store.find(task.id).ok().filter(|current| {
                current.status == task.status && current.aria_gid == task.aria_gid
            });
        let group = self
            .store
            .cloud_directory_group_for_task(task.id)
            .ok()
            .flatten();
        if terminal && current_task.is_some() {
            let directory_finished = group.as_ref().is_some_and(|group| {
                commands::cloud_directory_group_queue_task(&self.store, group.id).is_ok_and(
                    |root| {
                        matches!(
                            root.status,
                            TaskStatus::Completed
                                | TaskStatus::Failed
                                | TaskStatus::Cancelled
                                | TaskStatus::Recycled
                        )
                    },
                )
            });
            self.cloud_download_dispatch
                .release_terminal(task.id, directory_finished);
            if let Some(gateway) = self.baidu_range_gateway.get() {
                gateway.remove_task(task.id);
            }
        }
        if let Some(group) = group {
            self.schedule_directory_refresh(group.id);
        } else {
            let _ = commands::emit_task_update_for_task(
                &self.app,
                &self.store,
                Some(&self.cloud.share_plans),
                task.clone(),
            );
        }
        commands::schedule_cloud_download_refresh(self.app.clone(), task);
        cloud::commands::schedule_cloud_share_cleanup(self.app.clone(), task);
    }

    fn global_stat_updated(&self, stat: &Aria2GlobalStat) {
        let _ = self.app.emit("download-global-stat-updated", stat);
    }
}

impl EngineStateSink for TauriDownloadEvents {
    fn engine_state_updated(&self, state: &EngineAvailability) {
        let _ = self.app.emit("download-engine-state-updated", state);
    }
}

#[tauri::command]
async fn check_for_app_update() -> Result<Option<update::AppUpdate>, String> {
    let current_version = app_version();
    tauri::async_runtime::spawn_blocking(move || update::fetch_app_update(&current_version))
        .await
        .map_err(|error| format!("版本检测任务失败：{error}"))?
}

#[tauri::command]
async fn main_window_interaction_state(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    #[cfg(windows)]
    {
        window_interaction::snapshot(app).await
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Ok(serde_json::json!({ "active": false, "nativeDrag": false }))
    }
}

pub fn run() {
    webview_memory::apply_process_defaults();
    // Managed before any plugin: the single-instance callback can queue links
    // while setup is still running.
    let builder =
        tauri::Builder::default().manage(external_links::PendingExternalLinks::default());
    // A second launch, such as a link clicked in a browser, hands its arguments
    // to the running app and exits. Background UI tests run their own instance.
    let builder = if std::env::var("VERSADOWN_BACKGROUND_UI_TEST").as_deref() == Ok("1") {
        builder
    } else {
        builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let links = external_links::links_from_args(args.iter().skip(1));
            if links.is_empty() {
                launcher::reveal_manager(app, None);
            } else {
                external_links::deliver(app, links);
            }
        }))
    };
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .on_menu_event(|app, event| {
            // Keep the Windows/macOS/Linux native menu pump immediately
            // available even if an action needs to touch a window or runtime
            // state while aria2 is recovering.
            let app = app.clone();
            let id = event.id().as_ref().to_owned();
            tauri::async_runtime::spawn(async move {
                launcher::handle_launcher_menu_action(&app, &id);
            });
        })
        .on_window_event(|window, event| {
            if let Some(lifecycle) = window
                .app_handle()
                .try_state::<std::sync::Arc<ApplicationLifecycle>>()
            {
                window_lifecycle::handle_manager_window_event(
                    window,
                    event,
                    lifecycle.inner().as_ref(),
                );
            }
        })
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.manage(std::sync::Arc::new(launcher_input::macos::InputMonitor::default()));
            app.manage(window_icon::TaskbarIconState::default());
            let lifecycle = std::sync::Arc::new(ApplicationLifecycle::new());
            app.manage(std::sync::Arc::clone(&lifecycle));
            let background_ui_test = std::env::var("VERSADOWN_BACKGROUND_UI_TEST").ok();
            let data_dir = if background_ui_test.as_deref() == Some("1") {
                std::env::var_os("VERSADOWN_TEST_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .ok_or("后台界面测试必须提供隔离的数据目录")?
            } else {
                app.path().app_data_dir()?
            };
            std::fs::create_dir_all(&data_dir)?;
            if let Err(error) = aria2::migrate_legacy_ed2k_discovery_state(&data_dir) {
                eprintln!("Unable to migrate legacy eD2K discovery state: {error}");
            }
            let store = std::sync::Arc::new(TaskStore::open_path(&data_dir.join("tasks.sqlite"))?);
            let cloud = std::sync::Arc::new(cloud::commands::CloudState::new(
                data_dir.join("cloud-sessions"),
            )?);
            let mut startup_health = startup::StartupHealth {
                credentials_unavailable: cloud.credential_restore_failed,
            };
            if !startup_health.credentials_unavailable {
                commands::purge_tianyi_local_state(&store, &cloud).map_err(std::io::Error::other)?;
            }
            store.recover_interrupted_cloud_directories()?;
            // aria2 persists the short-lived direct URL it received from a
            // cloud provider. Do this before the native session is loaded so
            // those URLs cannot run ahead of the provider session check.
            let mut startup_cloud_providers = Vec::new();
            for task in store.defer_startup_cloud_downloads()? {
                if let Ok(provider) =
                    cloud::commands::cloud_provider_from_download_source(&task.source)
                {
                    if !startup_cloud_providers.contains(&provider) {
                        startup_cloud_providers.push(provider);
                    }
                }
            }
            let startup_store = std::sync::Arc::clone(&store);
            let ai_secrets = cloud::SystemCredentialStore::new(data_dir.join("ai-secrets"))?;
            // Webviews can invoke cloud commands while setup is still creating the
            // download runtime. Register this state immediately so those early
            // calls cannot race a later `manage` call.
            app.manage(std::sync::Arc::clone(&cloud));
            cloud.start_authorization_maintenance();
            let sidecar = engine_binary::EngineBinary::for_current_target()
                .and_then(|binary| {
                    binary.resolve(
                        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
                        &app.path().resource_dir().map_err(|_| {
                            engine_binary::EngineBinaryError::Missing(std::path::PathBuf::from(
                                "resource directory",
                            ))
                        })?,
                    )
                })
                .map_err(std::io::Error::other)?;
            let default_destination = if background_ui_test.as_deref() == Some("1") {
                std::env::var_os("VERSADOWN_TEST_DOWNLOAD_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| data_dir.join("downloads"))
            } else {
                #[cfg(target_os = "macos")]
                {
                    app.path()
                        .download_dir()
                        .unwrap_or_else(|_| data_dir.join("downloads"))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    std::env::var_os("USERPROFILE")
                        .map(std::path::PathBuf::from)
                        .map(|path| path.join("Downloads"))
                        .unwrap_or_else(|| data_dir.join("downloads"))
                }
            };
            let settings_path = data_dir.join("settings.json");
            let mut settings: AppSettings = std::fs::read(&settings_path)
                .ok()
                .and_then(|json| serde_json::from_slice(&json).ok())
                .unwrap_or_default();
            #[cfg(target_os = "macos")]
            {
                match ai_secrets.contains_local(ai::API_KEY_SECRET_NAME) {
                    Ok(configured) => settings.ai.api_key_configured = configured,
                    Err(_) => startup_health.credentials_unavailable = true,
                }
                match ai_secrets.contains_local(privacy::PASSWORD_SECRET_NAME) {
                    Ok(local) => {
                        ai_secrets.preserve_legacy_privacy(settings.privacy_password_configured && !local);
                        settings.privacy_password_configured |= local;
                    }
                    Err(_) => startup_health.credentials_unavailable = true,
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                match ai_secrets.load(ai::API_KEY_SECRET_NAME) {
                    Ok(key) => settings.ai.api_key_configured = key.is_some(),
                    Err(_) => startup_health.credentials_unavailable = true,
                }
                match privacy::password_is_configured(&ai_secrets) {
                    Ok(configured) => settings.privacy_password_configured = configured,
                    Err(_) => startup_health.credentials_unavailable = true,
                }
            }
            // A failed read must not reset saved configuration or unlock the
            // privacy session. Credential-dependent commands still fail closed.
            app.manage(startup_health);
            if settings.download_directory.trim().is_empty() {
                settings.download_directory = default_destination.to_string_lossy().into_owned();
            }
            settings.aria2.ensure_rpc_token();
            settings.aria2.validate().map_err(std::io::Error::other)?;
            std::fs::write(&settings_path, serde_json::to_vec_pretty(&settings)?)?;
            let launcher_visibility = settings.launcher_visibility;
            if background_ui_test.as_deref() != Some("1") {
                let enabled = settings.associate_download_links;
                let data_dir = data_dir.clone();
                // xdg-mime can take a moment; startup does not wait for it.
                std::thread::spawn(move || {
                    if let Err(error) = link_association::sync(enabled, &data_dir) {
                        eprintln!("Unable to update link association: {error}");
                    }
                });
            }
            let cloud_upload_limit = settings.cloud_upload_limit;
            let destination = std::path::PathBuf::from(&settings.download_directory);
            std::fs::create_dir_all(&destination)?;
            let launch_config = Aria2LaunchConfig {
                binary: sidecar,
                data_dir: data_dir.clone(),
                download_dir: destination,
                parent_pid: std::process::id(),
                peer_discovery: should_enable_peer_discovery(background_ui_test.as_deref()),
                aria2: settings.aria2.clone(),
            };
            let cloud_download_dispatch =
                cloud::download_dispatch::CloudDownloadDispatch::default();
            let baidu_range_gateway = std::sync::Arc::new(tokio::sync::OnceCell::new());
            let xunlei_native_backend =
                std::sync::Arc::new(xunlei_native::XunleiNativeBackend::with_plugin_config(
                    data_dir.join("plugins").join("xunlei-native.json"),
                ));
            xunlei_native_backend.initialize_plugin_config();
            let events = std::sync::Arc::new(TauriDownloadEvents {
                app: app.handle().clone(),
                store: std::sync::Arc::clone(&store),
                cloud,
                directory_refreshes: Default::default(),
                cloud_download_dispatch: cloud_download_dispatch.clone(),
                baidu_range_gateway: std::sync::Arc::clone(&baidu_range_gateway),
            });
            let runtime = std::sync::Arc::new(EngineRuntime::new(
                launch_config,
                std::sync::Arc::clone(&store),
                events.clone(),
                events,
            ));
            runtime.set_cloud_upload_limit(cloud_upload_limit);
            app.manage(commands::AppState {
                store,
                runtime: std::sync::Arc::clone(&runtime),
                settings: std::sync::Mutex::new(settings),
                settings_path,
                ai_secrets,
                privacy_session: Default::default(),
                cloud_download_sources: Default::default(),
                cloud_download_dispatch,
                baidu_range_gateway,
                xunlei_native_backend,
            });
            #[cfg(target_os = "macos")]
            app.set_menu(launcher::macos_application_menu(app.handle())?)?;
            app.manage(launcher::LauncherInteractionState::new(
                background_ui_test.as_deref() == Some("1"),
            ));
            // Created only after `AppState` is managed: its page reads settings
            // as soon as it loads.
            let launcher =
                launcher_window::ensure_launcher_window(app.handle(), launcher_visibility)
                    .map_err(std::io::Error::other)?;
            if let Some(main) = app.get_webview_window("main") {
                webview_memory::configure_webview(&main);
            }
            #[cfg(windows)]
            if background_ui_test.as_deref() != Some("1") {
                if let Some(main) = app.get_webview_window("main") {
                    window_interaction::install(&main).map_err(std::io::Error::other)?;
                }
            }
            if should_show_windows(background_ui_test.as_deref()) {
                launcher::setup_manager_tray(app.handle()).map_err(std::io::Error::other)?;
                if let Some(main) = app.get_webview_window("main") {
                    main.set_min_size(Some(tauri::LogicalSize::new(960.0, 640.0)))?;
                    main.show()?;
                }
                if launcher_visibility == LauncherVisibility::Always {
                    if let Some(launcher) = launcher {
                        launcher_window::present_launcher(launcher)?;
                    }
                }
            }
            let startup_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _availability = runtime.initialize().await;
                runtime.start_monitor(std::time::Duration::from_secs(2));
                let emit_updates = |tasks: Vec<DownloadTask>| {
                    for task in tasks {
                        let _ = commands::emit_task_update_for_task(
                            &startup_app,
                            &startup_store,
                            None,
                            task,
                        );
                    }
                };
                for provider in startup_cloud_providers {
                    let account = match cloud::commands::validate_current_provider_session(
                        startup_app.clone(),
                        provider,
                    )
                    .await
                    {
                        Ok(account) => Ok(account),
                        Err(error)
                            if cloud::commands::should_attempt_silent_provider_recovery(&error) =>
                        {
                            if let Ok(tasks) =
                                startup_store.set_startup_cloud_authorization_recovering(provider)
                            {
                                emit_updates(tasks);
                            }
                            cloud::commands::silently_recover_provider_session(
                                startup_app.clone(),
                                provider,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    if account.is_ok() {
                        let _ = cloud::commands::resume_reauthorized_provider_tasks(
                            startup_app.clone(),
                            provider,
                        )
                        .await;
                        // The provider has one active authorization slot. Any
                        // task bound to another account must ask for that
                        // account instead of staying in an indefinite check.
                        if let Ok(tasks) =
                            startup_store.set_startup_cloud_authorization_required(provider)
                        {
                            emit_updates(tasks);
                        }
                    } else if let Err(error) = account {
                        let tasks =
                            if cloud::commands::should_attempt_silent_provider_recovery(&error) {
                                startup_store.set_startup_cloud_authorization_required(provider)
                            } else {
                                startup_store.set_startup_cloud_authorization_check_failed(
                                    provider,
                                    cloud::commands::cloud_failure_code(&error),
                                    &cloud::commands::share_resolution_failure_message(&error),
                                )
                            };
                        if let Ok(tasks) = tasks {
                            emit_updates(tasks);
                        }
                    }
                }
            });
            // A link that started the app waits until the manager takes it.
            external_links::deliver(
                app.handle(),
                external_links::links_from_args(std::env::args_os().skip(1)),
            );
            #[cfg(all(target_os = "macos", feature = "native-smoke"))]
            native_smoke::schedule(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            startup::startup_health,
            external_links::take_external_links,
            launcher_drag::drag_launcher_window,
            launcher_input::set_launcher_input_regions,
            launcher_input::launcher_input_hit_test_for_test,
            window_icon::set_main_window_icon,
            check_for_app_update,
            local_upload_picker::local_upload_entries,
            main_window_interaction_state,
            commands::scan_download_inputs,
            commands::stage_dropped_torrent,
            commands::parse_download,
            commands::discard_prepared_download,
            commands::confirm_cloud_share_download,
            commands::confirm_download_command,
            commands::set_task_private,
            commands::list_tasks,
            commands::task_action,
            commands::resolve_duplicate_download,
            commands::open_task_directory,
            commands::open_task_file,
            commands::play_task_videos,
            commands::recycle_task,
            commands::purge_recycled_tasks,
            commands::delete_tasks,
            commands::select_files,
            commands::cancel_download_file,
            commands::get_download_engine_state,
            commands::retry_download_engine,
            commands::get_download_global_stat,
            commands::get_settings,
            commands::get_privacy_task_visibility,
            commands::configure_privacy_download,
            commands::unlock_privacy_tasks,
            commands::lock_privacy_tasks,
            commands::update_settings,
            commands::update_ai_settings,
            commands::list_ai_models,
            cloud::commands::cloud_provider_capabilities,
            cloud::commands::cloud_provider_catalog,
            cloud::commands::cloud_begin_authorization,
            cloud::commands::cloud_complete_authorization,
            cloud::commands::cloud_open_operation_verification,
            cloud::commands::cloud_complete_operation_verification,
            cloud::commands::cloud_completed_authorization,
            cloud::commands::cloud_reauthorize_failed_task,
            cloud::commands::cloud_login_password,
            cloud::commands::cloud_saved_password_credentials,
            cloud::commands::cloud_tianyi_session_shape_for_test,
            cloud::commands::cloud_tianyi_resolve_download_for_test,
            cloud::commands::cloud_account,
            cloud::commands::cloud_account_capacity,
            cloud::commands::cloud_list_directory,
            cloud::commands::cloud_create_share,
            cloud::commands::cloud_list_shares,
            cloud::commands::cloud_cancel_shares,
            cloud::commands::cloud_update_share_pass_code,
            cloud::commands::cloud_create_directory,
            cloud::commands::cloud_rename,
            cloud::commands::cloud_move,
            cloud::commands::cloud_copy,
            cloud::commands::cloud_trash,
            cloud::commands::cloud_delete,
            cloud::commands::cloud_restore,
            cloud::commands::cloud_download_url,
            cloud::commands::cloud_download_directory,
            cloud::commands::cloud_upload_files,
            cloud::commands::cloud_upload_directory,
            cloud::commands::cloud_control_upload,
            cloud::commands::cloud_logout,
            launcher::set_launcher_window_visible,
            launcher::show_launcher_menu,
            launcher::run_launcher_menu_action,
            launcher::launcher_manager_visible,
            launcher::launcher_interaction_snapshot,
            launcher::manager_window_snapshot,
            launcher::activate_launcher_menu_for_test,
            window_lifecycle::request_native_manager_close_for_test,
            window_lifecycle::request_native_launcher_close_for_test,
            window_lifecycle::launcher_visible_for_test,
            window_lifecycle::foreground_window_handle_for_test,
            window_lifecycle::manager_close_request_count_for_test
        ])
        .build(tauri::generate_context!())
        .expect("error while building VersaDown")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            match &event {
                tauri::RunEvent::Opened { urls } => {
                    let links = external_links::links_from_args(urls.iter().map(|url| url.as_str()));
                    external_links::deliver(app, links);
                }
                tauri::RunEvent::Reopen { .. } => launcher::reveal_manager(app, None),
                _ => {}
            }
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                let application_exiting = app
                    .try_state::<std::sync::Arc<ApplicationLifecycle>>()
                    .is_some_and(|lifecycle| lifecycle.is_exiting());
                if should_prevent_implicit_exit(application_exiting, code) {
                    api.prevent_exit();
                    // Cmd-Q and the native application menu are explicit quit
                    // requests on macOS; use the same bounded engine shutdown
                    // as the tray instead of trapping the user in the app.
                    #[cfg(target_os = "macos")]
                    if code == Some(0) {
                        launcher::handle_launcher_menu_action(app, launcher::QUIT);
                    }
                }
            }
        });
}

/// Runs the private no-UI Linux Xunlei worker before Tauri is initialized.
/// It is selected only by the executable's internal multicall argument.
#[cfg(target_os = "linux")]
pub fn run_xunlei_native_worker() -> Result<(), ()> {
    xunlei_native::run_linux_worker_from_args()
}

#[cfg(not(target_os = "linux"))]
pub fn run_xunlei_native_worker() -> Result<(), ()> {
    Err(())
}

#[cfg(test)]
mod tests {
    use super::{should_enable_peer_discovery, should_show_windows};

    #[test]
    fn background_ui_test_is_hidden_and_network_isolated() {
        assert!(!should_show_windows(Some("1")));
        assert!(!should_enable_peer_discovery(Some("1")));
    }

    #[test]
    fn production_keeps_windows_and_peer_discovery_enabled() {
        assert!(should_show_windows(None));
        assert!(should_enable_peer_discovery(None));
    }
}
