use tauri::{AppHandle, Manager, WebviewWindow, WebviewWindowBuilder};

use crate::{launcher, launcher_input, LauncherVisibility};

#[cfg(target_os = "macos")]
#[path = "launcher_window_macos.rs"]
mod macos;

const LAUNCHER_LABEL: &str = "launcher";

/// A hidden launcher never renders, so it should not own a web process. The
/// `downloading` mode still needs its renderer to watch download activity.
pub fn should_create_launcher_window(visibility: LauncherVisibility) -> bool {
    visibility != LauncherVisibility::Hidden
}

/// Returns the launcher window, creating it when the setting needs one.
///
/// Linux defers the window (`create: false` in its config) and builds it as a
/// related view of the manager, so both pages share one WebKit web process
/// instead of paying for a second renderer. Must run on the main thread there.
pub fn ensure_launcher_window(
    app: &AppHandle,
    visibility: LauncherVisibility,
) -> Result<Option<WebviewWindow>, String> {
    if let Some(window) = app.get_webview_window(LAUNCHER_LABEL) {
        #[cfg(target_os = "macos")]
        macos::allow_canvas_above_screen(&window)?;
        return Ok(Some(window));
    }
    if !should_create_launcher_window(visibility) {
        return Ok(None);
    }
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == LAUNCHER_LABEL)
        .ok_or_else(|| "悬浮窗配置不存在".to_string())?;
    let builder =
        WebviewWindowBuilder::from_config(app, config).map_err(|error| error.to_string())?;
    #[cfg(target_os = "linux")]
    let builder = match manager_web_view(app) {
        Some(manager) => builder.with_related_view(manager),
        None => {
            eprintln!("无法获取主窗口 WebView，悬浮窗将使用独立渲染进程");
            builder
        }
    };
    let window = builder.build().map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    macos::allow_canvas_above_screen(&window)?;
    #[cfg(target_os = "linux")]
    allow_launcher_above_screen_top(&window)?;
    crate::webview_memory::configure_webview(&window);
    Ok(Some(window))
}

/// The icon sits in the middle of a 720px transparent canvas, so reaching the
/// top of the screen needs a negative window origin. KWin clamps ordinary
/// windows to the top edge; notification windows are exempt, yet unlike docks
/// still take focus, which the context menu's blur-to-close relies on. GTK
/// only honours the hint before the window is first shown.
#[cfg(target_os = "linux")]
fn allow_launcher_above_screen_top(window: &WebviewWindow) -> Result<(), String> {
    use gtk::prelude::GtkWindowExt;

    window
        .gtk_window()
        .map_err(|error| error.to_string())?
        .set_type_hint(gtk::gdk::WindowTypeHint::Notification);
    Ok(())
}

/// Settings may turn a never-created launcher back on from any thread.
pub fn sync_launcher_window(app: &AppHandle, visibility: LauncherVisibility) {
    if !should_create_launcher_window(visibility) || app.get_webview_window(LAUNCHER_LABEL).is_some()
    {
        return;
    }
    let handle = app.clone();
    let result = app.run_on_main_thread(move || {
        match ensure_launcher_window(&handle, visibility) {
            Ok(Some(window)) if visibility == LauncherVisibility::Always => {
                let background_test = handle
                    .state::<launcher::LauncherInteractionState>()
                    .is_background_test();
                if !background_test {
                    if let Err(error) = present_launcher(window) {
                        eprintln!("无法显示悬浮窗: {error}");
                    }
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!("无法创建悬浮窗: {error}"),
        }
    });
    if let Err(error) = result {
        eprintln!("无法创建悬浮窗: {error}");
    }
}

/// `with_webview` runs inline on the main thread, and `webkit2gtk::WebView` is
/// not `Send`, so hand the view back through a main-thread local slot.
#[cfg(target_os = "linux")]
fn manager_web_view(app: &AppHandle) -> Option<webkit2gtk::WebView> {
    use std::cell::RefCell;

    thread_local! {
        static MANAGER_VIEW: RefCell<Option<webkit2gtk::WebView>> = const { RefCell::new(None) };
    }
    let manager = app.get_webview_window("main")?;
    manager
        .with_webview(|webview| {
            MANAGER_VIEW.with(|slot| *slot.borrow_mut() = Some(webview.inner()));
        })
        .ok()?;
    MANAGER_VIEW.with(|slot| slot.borrow_mut().take())
}

/// Sizes, shapes and shows an `always` launcher.
pub fn present_launcher(launcher: WebviewWindow) -> tauri::Result<()> {
    // The launcher canvas is authored in CSS logical pixels.
    // A PhysicalSize here shrinks its WebKitGTK viewport on
    // fractional-scale X11 desktops, placing the icon outside
    // the rendered surface.
    launcher.set_size(tauri::LogicalSize::new(756.0, 720.0))?;
    // A transparent 756x720 WebView is larger than its
    // visible icon. Make its initial state click-through
    // before it can be shown; the region monitor restores
    // input only under the icon/popup once it is ready.
    #[cfg(windows)]
    if let Err(error) = launcher.set_ignore_cursor_events(true) {
        eprintln!("无法初始化悬浮窗鼠标穿透: {error}");
    }
    // The command installs the HWND subclass on its owner thread.
    // On Linux the GTK GdkWindow does not exist until it is shown,
    // so show first and install the X11 input shape afterwards.
    tauri::async_runtime::spawn(async move {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if let Err(error) = launcher.show() {
                eprintln!("无法显示悬浮窗: {error}");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if let Err(error) = launcher_input::install_linux_context_menu(&launcher) {
                eprintln!("无法安装悬浮窗右键菜单: {error}");
            }
            if let Err(error) = launcher_input::install_linux_drag(&launcher) {
                eprintln!("无法安装悬浮窗拖动: {error}");
            }
        }
        let result = launcher_input::set_launcher_input_regions(
            launcher.clone(),
            vec![launcher_input::LauncherInputRegion {
                x: 346.0,
                y: 328.0,
                width: 64.0,
                height: 64.0,
            }],
            launcher.scale_factor().unwrap_or(1.0),
            launcher.state::<launcher::LauncherInteractionState>(),
        )
        .await;
        match result {
            Ok(()) => {}
            // A WebView child can be created slightly after the
            // launcher root HWND. The renderer will retry its
            // input-region update once mounted; never leave an
            // otherwise valid launcher permanently hidden while
            // that native refinement catches up.
            Err(error) => {
                eprintln!("无法初始化悬浮窗输入区域，将由界面重试: {error}")
            }
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        if let Err(error) = launcher.show() {
            eprintln!("无法显示悬浮窗: {error}");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_create_launcher_window;
    use crate::LauncherVisibility;

    #[test]
    fn only_a_hidden_launcher_skips_its_window() {
        assert!(should_create_launcher_window(LauncherVisibility::Always));
        assert!(should_create_launcher_window(LauncherVisibility::Downloading));
        assert!(!should_create_launcher_window(LauncherVisibility::Hidden));
    }
}
