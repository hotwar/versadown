use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use tauri::{AppHandle, Manager, WebviewWindow, Window, WindowEvent};

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> *mut std::ffi::c_void;
    fn ShowWindow(window: *mut std::ffi::c_void, command: i32) -> i32;
}

const BACKGROUND_UI_TEST: &str = "VERSADOWN_BACKGROUND_UI_TEST";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagerCloseDisposition {
    Hide,
    KeepOpen,
    Exit,
}

pub fn manager_close_disposition(
    window_label: &str,
    application_exiting: bool,
) -> ManagerCloseDisposition {
    if application_exiting {
        return ManagerCloseDisposition::Exit;
    }
    match window_label {
        "main" => ManagerCloseDisposition::Hide,
        "launcher" => ManagerCloseDisposition::KeepOpen,
        _ => ManagerCloseDisposition::Exit,
    }
}

/// The event loop must remain resident unless the application's explicit quit
/// action has marked the lifecycle as exiting. This prevents an unrelated
/// native exit request from terminating active downloads.
pub fn should_prevent_implicit_exit(application_exiting: bool, _exit_code: Option<i32>) -> bool {
    !application_exiting
}

pub struct ApplicationLifecycle {
    exiting: AtomicBool,
    manager_close_requests: AtomicUsize,
}

impl ApplicationLifecycle {
    pub fn new() -> Self {
        Self {
            exiting: AtomicBool::new(false),
            manager_close_requests: AtomicUsize::new(0),
        }
    }

    pub fn begin_exit(&self) {
        self.exiting.store(true, Ordering::Release);
    }

    pub fn is_exiting(&self) -> bool {
        self.exiting.load(Ordering::Acquire)
    }

    pub fn record_manager_close_request(&self) {
        self.manager_close_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn manager_close_requests(&self) -> usize {
        self.manager_close_requests.load(Ordering::Relaxed)
    }
}

impl Default for ApplicationLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

pub fn handle_manager_window_event(
    window: &Window,
    event: &WindowEvent,
    lifecycle: &ApplicationLifecycle,
) {
    if !matches!(event, WindowEvent::CloseRequested { .. }) {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        match manager_close_disposition(window.label(), lifecycle.is_exiting()) {
            ManagerCloseDisposition::Exit => return,
            ManagerCloseDisposition::KeepOpen => {
                api.prevent_close();
                let app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    let Some(launcher) = app.get_webview_window("launcher") else {
                        return;
                    };
                    if !launcher.is_visible().unwrap_or(false) {
                        let _ = show_manager_without_activation(&launcher);
                    }
                });
                return;
            }
            ManagerCloseDisposition::Hide => api.prevent_close(),
        }
        lifecycle.record_manager_close_request();
        let app = window.app_handle().clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let _ = crate::launcher::hide_manager(&app);
        });
    }
}

pub fn show_manager_without_activation(window: &WebviewWindow) -> Result<(), String> {
    #[cfg(windows)]
    {
        const SW_SHOWNOACTIVATE: i32 = 4;
        let hwnd = window.hwnd().map_err(|error| error.to_string())?;
        unsafe {
            ShowWindow(hwnd.0, SW_SHOWNOACTIVATE);
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        window.show().map_err(|error| error.to_string())
    }
}

pub fn hide_window_immediately(window: &WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|error| error.to_string())?;

    #[cfg(windows)]
    if window.is_visible().unwrap_or(false) {
        const SW_HIDE: i32 = 0;
        let hwnd = window.hwnd().map_err(|error| error.to_string())?;
        unsafe {
            ShowWindow(hwnd.0, SW_HIDE);
        }
    }

    Ok(())
}

fn require_background_ui_test() -> Result<(), String> {
    if std::env::var(BACKGROUND_UI_TEST).as_deref() == Ok("1") {
        Ok(())
    } else {
        Err("窗口观测仅在后台测试模式可用".to_string())
    }
}

#[tauri::command]
pub fn request_native_manager_close_for_test(app: AppHandle) -> Result<(), String> {
    require_background_ui_test()?;
    let manager = app
        .get_webview_window("main")
        .ok_or_else(|| "下载管理器窗口不存在".to_string())?;

    manager.close().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn request_native_launcher_close_for_test(app: AppHandle) -> Result<(), String> {
    require_background_ui_test()?;
    let launcher = app
        .get_webview_window("launcher")
        .ok_or_else(|| "悬浮窗不存在".to_string())?;

    launcher.close().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn launcher_visible_for_test(app: AppHandle) -> Result<bool, String> {
    require_background_ui_test()?;
    app.get_webview_window("launcher")
        .ok_or_else(|| "悬浮窗不存在".to_string())?
        .is_visible()
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn foreground_window_handle_for_test() -> Result<Option<u64>, String> {
    require_background_ui_test()?;

    #[cfg(windows)]
    {
        let hwnd = unsafe { GetForegroundWindow() };
        Ok((!hwnd.is_null()).then_some(hwnd as usize as u64))
    }

    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

#[tauri::command]
pub fn manager_close_request_count_for_test(
    lifecycle: tauri::State<'_, Arc<ApplicationLifecycle>>,
) -> Result<usize, String> {
    require_background_ui_test()?;
    Ok(lifecycle.manager_close_requests())
}
