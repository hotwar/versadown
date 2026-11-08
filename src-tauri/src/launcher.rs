use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WebviewWindow,
};

pub const HIDE_MANAGER: &str = "launcher.hide_manager";
pub const NEW_DOWNLOAD: &str = "launcher.new_download";
pub const START_ALL: &str = "launcher.start_all";
pub const PAUSE_ALL: &str = "launcher.pause_all";
pub const OPEN_SETTINGS: &str = "launcher.settings";
pub const QUIT: &str = "launcher.quit";
pub const TRAY_LAUNCHER_ALWAYS: &str = "tray.launcher.always";
pub const TRAY_LAUNCHER_DOWNLOADING: &str = "tray.launcher.downloading";
pub const TRAY_LAUNCHER_HIDDEN: &str = "tray.launcher.hidden";
pub(crate) const TRAY_ID: &str = "manager-tray";
const EXIT_TRAY_REMOVE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
const EXIT_ARIA2_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(7);

#[cfg(target_os = "macos")]
pub(crate) fn macos_application_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::default(app)?;
    if let Some(tauri::menu::MenuItemKind::Submenu(application)) = menu.items()?.first() {
        let items = application.items()?;
        if let Some(tauri::menu::MenuItemKind::Predefined(quit)) = items.last() {
            // The default AppKit Quit selector terminates immediately. Keep
            // its system-localized label but route Cmd-Q through our engine
            // shutdown instead, so active downloads save their session first.
            let label = quit.text()?;
            application.remove(quit)?;
            application.append(&MenuItem::with_id(app, QUIT, label, true, Some("CmdOrCtrl+Q"))?)?;
        }
    }
    Ok(menu)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherInteractionSnapshot {
    pub menu_requests: usize,
    pub bounds_update_requests: usize,
    pub input_region_update_requests: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerWindowSnapshot {
    pub exists: bool,
    pub visible: bool,
    pub minimized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitWindowSnapshot {
    pub manager_visible: bool,
    pub launcher_visible: bool,
}

pub struct LauncherInteractionState {
    background_test: bool,
    menu_requests: AtomicUsize,
    // Regression sentinel: popup transitions must never update native bounds.
    bounds_update_requests: AtomicUsize,
    input_region_update_requests: AtomicUsize,
}

pub struct TrayMenuState {
    manager_item: MenuItem<tauri::Wry>,
}

impl LauncherInteractionState {
    pub fn new(background_test: bool) -> Self {
        Self {
            background_test,
            menu_requests: AtomicUsize::new(0),
            bounds_update_requests: AtomicUsize::new(0),
            input_region_update_requests: AtomicUsize::new(0),
        }
    }

    pub fn is_background_test(&self) -> bool {
        self.background_test
    }

    pub fn record_menu(&self) {
        self.menu_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_input_region_update(&self) {
        self.input_region_update_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> LauncherInteractionSnapshot {
        LauncherInteractionSnapshot {
            menu_requests: self.menu_requests.load(Ordering::Relaxed),
            bounds_update_requests: self.bounds_update_requests.load(Ordering::Relaxed),
            input_region_update_requests: self.input_region_update_requests.load(Ordering::Relaxed),
        }
    }
}

pub fn launcher_menu_entries() -> [(&'static str, &'static str); 6] {
    [
        (HIDE_MANAGER, "隐藏主界面"),
        (NEW_DOWNLOAD, "新建下载"),
        (START_ALL, "开始全部任务"),
        (PAUSE_ALL, "暂停全部任务"),
        (OPEN_SETTINGS, "悬浮球显示"),
        (QUIT, "退出万能下载"),
    ]
}

pub fn tray_menu_entries(manager_visible: bool) -> [(&'static str, &'static str); 8] {
    [
        (HIDE_MANAGER, tray_manager_item_label(manager_visible)),
        (NEW_DOWNLOAD, "新建下载"),
        (START_ALL, "开始全部任务"),
        (PAUSE_ALL, "暂停全部任务"),
        (TRAY_LAUNCHER_ALWAYS, "总是显示"),
        (TRAY_LAUNCHER_DOWNLOADING, "仅下载时显示"),
        (TRAY_LAUNCHER_HIDDEN, "关闭显示"),
        (QUIT, "退出万能下载"),
    ]
}

fn tray_manager_item_label(manager_visible: bool) -> &'static str {
    if manager_visible {
        "隐藏主界面"
    } else {
        "显示主界面"
    }
}

pub fn setup_manager_tray(app: &AppHandle) -> Result<(), String> {
    let manager_visible = app
        .get_webview_window("main")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false);
    let entries = tray_menu_entries(manager_visible);
    let manager_item = MenuItem::with_id(app, entries[0].0, entries[0].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let new_download = MenuItem::with_id(app, entries[1].0, entries[1].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let start_all = MenuItem::with_id(app, entries[2].0, entries[2].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let pause_all = MenuItem::with_id(app, entries[3].0, entries[3].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let always = MenuItem::with_id(app, entries[4].0, entries[4].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let downloading = MenuItem::with_id(app, entries[5].0, entries[5].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let hidden = MenuItem::with_id(app, entries[6].0, entries[6].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let quit = MenuItem::with_id(app, entries[7].0, entries[7].1, true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let first_separator = PredefinedMenuItem::separator(app).map_err(|error| error.to_string())?;
    let second_separator = PredefinedMenuItem::separator(app).map_err(|error| error.to_string())?;
    let visibility_menu = Submenu::with_id_and_items(
        app,
        "tray.launcher.visibility",
        "悬浮球显示",
        true,
        &[&always, &downloading, &hidden],
    )
    .map_err(|error| error.to_string())?;
    let menu = Menu::with_items(
        app,
        &[
            &manager_item,
            &new_download,
            &start_all,
            &pause_all,
            &first_separator,
            &visibility_menu,
            &second_separator,
            &quit,
        ],
    )
    .map_err(|error| error.to_string())?;
    TrayIconBuilder::with_id(TRAY_ID)
        // The packaged icon includes intentional export padding for window
        // and installer use.  A system-tray slot is much smaller, so use the
        // tight raster shared with the taskbar instead.
        .icon(crate::window_icon::icon_image("mint"))
        .tooltip("万能下载")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                // Native tray events run on the platform event loop.  Window
                // activation must never hold that loop while the download
                // engine is starting, stopping, or recovering.
                let app = tray.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    reveal_manager(&app, None);
                });
            }
        })
        .build(app)
        .map_err(|error| error.to_string())?;
    app.manage(TrayMenuState { manager_item });
    Ok(())
}

pub fn set_tray_manager_visibility(app: &AppHandle, manager_visible: bool) {
    if let Some(state) = app.try_state::<TrayMenuState>() {
        let _ = state
            .manager_item
            .set_text(tray_manager_item_label(manager_visible));
    }
}

fn background_ui_test_enabled() -> bool {
    std::env::var("VERSADOWN_BACKGROUND_UI_TEST").as_deref() == Ok("1")
}

fn require_background_ui_test() -> Result<(), String> {
    if background_ui_test_enabled() {
        Ok(())
    } else {
        Err("窗口观测仅在后台测试模式可用".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{EXIT_ARIA2_SHUTDOWN_TIMEOUT, EXIT_TRAY_REMOVE_DELAY};

    #[test]
    fn launcher_popup_commands_do_not_reintroduce_window_region_redraws() {
        let source = include_str!("launcher.rs");
        assert!(!source.contains(&["pub fn set", "_launcher_bounds"].concat()));
        assert!(!source.contains(&["pub fn set", "_launcher_compact_shape"].concat()));
        assert!(!source.contains(&["SetWindow", "Rgn"].concat()));
    }

    #[test]
    fn explicit_exit_gives_aria2_a_bounded_grace_period_after_hiding_windows() {
        assert_eq!(EXIT_TRAY_REMOVE_DELAY.as_secs(), 2);
        assert_eq!(EXIT_ARIA2_SHUTDOWN_TIMEOUT.as_secs(), 7);
    }

    #[test]
    fn tray_clicks_are_dispatched_off_the_native_event_loop() {
        let source = include_str!("launcher.rs");
        assert!(source.contains(
            "tauri::async_runtime::spawn(async move {\n                    reveal_manager"
        ));
    }

    #[test]
    fn tray_uses_the_tight_runtime_icon_instead_of_the_padded_bundle_icon() {
        let source = include_str!("launcher.rs");
        assert!(source.contains(".icon(crate::window_icon::icon_image(\"mint\"))"));
        let implementation = source.split("#[cfg(test)]").next().unwrap();
        assert!(!implementation.contains("default_window_icon()"));
    }
}

pub fn should_apply_launcher_window_visibility(
    requested_visible: bool,
    application_exiting: bool,
) -> bool {
    !requested_visible || !application_exiting
}

/// Shows or hides the floating launcher from its own renderer.  Keeping this
/// native-side is important for the isolated background smoke process: that
/// process must be able to exercise the launcher without ever surfacing a
/// window on the user's desktop.
#[tauri::command]
pub fn set_launcher_window_visible(
    window: WebviewWindow,
    visible: bool,
    interaction: State<'_, LauncherInteractionState>,
) -> Result<(), String> {
    if window.label() != "launcher" {
        return Err("只能设置悬浮球窗口的可见性".to_string());
    }
    let application_exiting = window
        .app_handle()
        .try_state::<std::sync::Arc<crate::ApplicationLifecycle>>()
        .is_some_and(|lifecycle| lifecycle.is_exiting());
    if !should_apply_launcher_window_visibility(visible, application_exiting) {
        return Ok(());
    }
    if interaction.is_background_test() {
        return Ok(());
    }
    if visible {
        // The platform input monitor owns click-through; showing a macOS
        // launcher must not reset its last known pointer state.
        window.show().map_err(|error| error.to_string())
    } else {
        // The X11 shape remains installed while a Linux launcher is hidden.
        // Calling Tao's generic toggle would replace it with an empty shape.
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        window
            .set_ignore_cursor_events(true)
            .map_err(|error| error.to_string())?;
        window.hide().map_err(|error| error.to_string())
    }
}

#[tauri::command]
pub fn show_launcher_menu(
    window: WebviewWindow,
    interaction: State<'_, LauncherInteractionState>,
) -> Result<(), String> {
    if window.label() != "launcher" {
        return Err("右键菜单只能从悬浮窗打开".to_string());
    }
    interaction.record_menu();
    Ok(())
}

#[tauri::command]
pub fn run_launcher_menu_action(
    app: AppHandle,
    window: WebviewWindow,
    action: String,
) -> Result<(), String> {
    if window.label() != "launcher" {
        return Err("悬浮球菜单动作只能从悬浮窗执行".to_string());
    }
    if !launcher_menu_entries().iter().any(|(id, _)| *id == action) {
        return Err("未知的悬浮球菜单动作".to_string());
    }
    handle_launcher_menu_action(&app, &action);
    Ok(())
}

#[tauri::command]
pub fn launcher_manager_visible(app: AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|manager| manager.is_visible().ok())
        .unwrap_or(false)
}

#[tauri::command]
pub fn launcher_interaction_snapshot(
    interaction: State<'_, LauncherInteractionState>,
) -> Result<LauncherInteractionSnapshot, String> {
    if !interaction.is_background_test() {
        return Err("交互观测仅在后台测试模式可用".to_string());
    }
    Ok(interaction.snapshot())
}

#[tauri::command]
pub fn manager_window_snapshot(app: AppHandle) -> Result<ManagerWindowSnapshot, String> {
    require_background_ui_test()?;
    let manager = app
        .get_webview_window("main")
        .ok_or_else(|| "下载管理器窗口不存在".to_string())?;
    Ok(ManagerWindowSnapshot {
        exists: true,
        visible: manager.is_visible().map_err(|error| error.to_string())?,
        minimized: manager.is_minimized().map_err(|error| error.to_string())?,
    })
}

#[tauri::command]
pub fn activate_launcher_menu_for_test(
    app: AppHandle,
    action: String,
) -> Result<Option<ExitWindowSnapshot>, String> {
    require_background_ui_test()?;
    if !launcher_menu_entries().iter().any(|(id, _)| *id == action) {
        return Err("未知的悬浮窗菜单动作".to_string());
    }
    handle_launcher_menu_action(&app, &action);
    (action == QUIT)
        .then(|| exit_window_snapshot(&app))
        .transpose()
}

pub fn handle_launcher_menu_action(app: &AppHandle, id: &str) {
    match id {
        HIDE_MANAGER => toggle_manager_visibility(app),
        NEW_DOWNLOAD => {
            reveal_manager(app, Some("downloads"));
            let _ = app.emit_to("main", "launcher-command", "new-download");
        }
        START_ALL => {
            let _ = app.emit_to("main", "launcher-command", "resume-all");
        }
        PAUSE_ALL => {
            let _ = app.emit_to("main", "launcher-command", "pause-all");
        }
        OPEN_SETTINGS => reveal_manager(app, Some("settings")),
        TRAY_LAUNCHER_ALWAYS => {
            let _ = crate::commands::update_launcher_visibility_from_native_menu(
                app,
                crate::LauncherVisibility::Always,
            );
        }
        TRAY_LAUNCHER_DOWNLOADING => {
            let _ = crate::commands::update_launcher_visibility_from_native_menu(
                app,
                crate::LauncherVisibility::Downloading,
            );
        }
        TRAY_LAUNCHER_HIDDEN => {
            let _ = crate::commands::update_launcher_visibility_from_native_menu(
                app,
                crate::LauncherVisibility::Hidden,
            );
        }
        QUIT => begin_application_exit(app),
        _ => {}
    }
}

fn toggle_manager_visibility(app: &AppHandle) {
    let Some(manager) = app.get_webview_window("main") else {
        return;
    };
    if manager.is_visible().unwrap_or(false) {
        let _ = manager.hide();
        set_tray_manager_visibility(app, false);
    } else {
        reveal_manager(app, None);
    }
}

pub(crate) fn reveal_manager(app: &AppHandle, destination: Option<&str>) {
    let Some(manager) = app.get_webview_window("main") else {
        return;
    };
    if let Some(destination) = destination {
        let _ = app.emit_to("main", "launcher-navigate", destination);
    }
    if background_ui_test_enabled() {
        let _ = crate::window_lifecycle::show_manager_without_activation(&manager);
    } else {
        let _ = manager.unminimize();
        let _ = manager.show();
        let _ = manager.set_focus();
    }
    set_tray_manager_visibility(app, true);
}

fn begin_application_exit(app: &AppHandle) {
    if let Some(lifecycle) = app.try_state::<std::sync::Arc<crate::ApplicationLifecycle>>() {
        lifecycle.begin_exit();
    }
    hide_application_windows(app);
    let Some(state) = app.try_state::<crate::commands::AppState>() else {
        app.exit(0);
        return;
    };
    let runtime = std::sync::Arc::clone(&state.runtime);
    #[cfg(target_os = "macos")]
    let native = std::sync::Arc::clone(&state.xunlei_native_backend);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // The user-facing windows disappear immediately, but leave the tray in
        // place briefly so the explicit exit has visible continuity while aria2
        // saves its session.  Do not end the host process before shutdown has
        // either completed gracefully or forced and reaped its owned child.
        let remove_tray_app = app.clone();
        let remove_tray = async move {
            tokio::time::sleep(EXIT_TRAY_REMOVE_DELAY).await;
            #[cfg(target_os = "macos")]
            {
                // Removing the registry entry returns the final TrayIcon.
                // Its Drop calls AppKit, so both removal AND destruction
                // must happen on the main thread before the event loop ends.
                let target = remove_tray_app.clone();
                let (sender, receiver) = tokio::sync::oneshot::channel();
                if remove_tray_app
                    .run_on_main_thread(move || {
                        drop(target.remove_tray_by_id(TRAY_ID));
                        let _ = sender.send(());
                    })
                    .is_ok()
                {
                    let _ = receiver.await;
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = remove_tray_app.remove_tray_by_id(TRAY_ID);
        };
        let shutdown = runtime.shutdown(EXIT_ARIA2_SHUTDOWN_TIMEOUT);
        let (_, report) = tokio::join!(remove_tray, shutdown);
        #[cfg(target_os = "macos")]
        let _ = tauri::async_runtime::spawn_blocking(move || native.shutdown()).await;
        if !report.child_stopped {
            // `shutdown_report` only reaches this branch after its force-kill
            // fallback failed.  Keep the process alive instead of orphaning a
            // still-running child; the user can retry normal exit safely.
            return;
        }
        app.exit(0);
    });
}

fn hide_application_windows(app: &AppHandle) {
    for label in ["main", "launcher"] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = crate::window_lifecycle::hide_window_immediately(&window);
        }
    }
}

fn exit_window_snapshot(app: &AppHandle) -> Result<ExitWindowSnapshot, String> {
    let manager = app
        .get_webview_window("main")
        .ok_or_else(|| "下载管理器窗口不存在".to_string())?;
    // A `hidden` launcher may never have been created on Linux.
    let launcher_visible = match app.get_webview_window("launcher") {
        Some(launcher) => launcher.is_visible().map_err(|error| error.to_string())?,
        None => false,
    };
    Ok(ExitWindowSnapshot {
        manager_visible: manager.is_visible().map_err(|error| error.to_string())?,
        launcher_visible,
    })
}
