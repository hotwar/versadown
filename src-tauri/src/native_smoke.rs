//! Opt-in native integration regression, excluded from shipped builds.
use tauri::{AppHandle, Manager};

pub(crate) fn schedule(app: &AppHandle) -> tauri::Result<()> {
    if std::env::var("VERSADOWN_BACKGROUND_UI_TEST").as_deref() != Ok("1")
        || std::env::var("VERSADOWN_NATIVE_SMOKE").as_deref() != Ok("1")
    {
        return Ok(());
    }
    crate::launcher::setup_manager_tray(app).map_err(std::io::Error::other)?;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = tauri::async_runtime::spawn_blocking({
            let app = app.clone();
            move || -> Result<(), String> {
                let window = app
                    .get_webview_window("main")
                    .ok_or("missing test WebView")?;
                let name = format!("versadown-smoke-{}", uuid::Uuid::new_v4().simple());
                let cookie = tauri::webview::Cookie::build((name.clone(), "synthetic-test-value"))
                    .domain(".versadown-smoke.invalid")
                    .path("/")
                    .secure(true)
                    .http_only(true)
                    .build();
                window
                    .set_cookie(cookie.clone())
                    .map_err(|e| e.to_string())?;
                let values = crate::cloud::commands::browser_cookies(
                    &window,
                    &["https://pan.versadown-smoke.invalid/"],
                )
                .map_err(|e| e.to_string());
                let _ = window.delete_cookie(cookie);
                let values = values?;
                if !values
                    .iter()
                    .any(|value| value == &format!("{name}=synthetic-test-value"))
                {
                    return Err("native parent-domain HttpOnly cookie was lost".into());
                }
                Ok(())
            }
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            eprintln!("NATIVE_COOKIE_SMOKE_FAILED: {result:?}");
            app.state::<std::sync::Arc<crate::ApplicationLifecycle>>()
                .begin_exit();
            app.exit(1);
            return;
        }
        eprintln!("NATIVE_PARENT_DOMAIN_COOKIE_OK");
        if let Err(error) = launcher_geometry(&app).await {
            eprintln!("NATIVE_LAUNCHER_GEOMETRY_FAILED: {error}");
            app.state::<std::sync::Arc<crate::ApplicationLifecycle>>()
                .begin_exit();
            app.exit(1);
            return;
        }
        for _ in 0..250 {
            if app
                .state::<crate::commands::AppState>()
                .runtime
                .status()
                .available
            {
                eprintln!("NATIVE_TRAY_EXIT_START");
                // Exercise exactly the production quit path, including its
                // delayed native tray destruction and engine shutdown.
                crate::launcher::handle_launcher_menu_action(&app, crate::launcher::QUIT);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        eprintln!("NATIVE_ENGINE_START_TIMEOUT");
        app.state::<std::sync::Arc<crate::ApplicationLifecycle>>()
            .begin_exit();
        app.exit(1);
    });
    Ok(())
}

async fn launcher_geometry(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("launcher")
        .ok_or("missing launcher")?;
    window.show().map_err(|e| e.to_string())?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let monitor = window
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("missing monitor")?;
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    // The transparent canvas must be able to leave the work area while its
    // visible 64px icon reaches the top-left and bottom-right corners.
    for (x, y) in [
        (
            area.position.x - (346.0 * scale) as i32,
            area.position.y - (328.0 * scale) as i32,
        ),
        (
            area.position.x + area.size.width as i32 - (410.0 * scale) as i32,
            area.position.y - (328.0 * scale) as i32,
        ),
        (
            area.position.x - (346.0 * scale) as i32,
            area.position.y + area.size.height as i32 - (392.0 * scale) as i32,
        ),
        (
            area.position.x + area.size.width as i32 - (410.0 * scale) as i32,
            area.position.y + area.size.height as i32 - (392.0 * scale) as i32,
        ),
    ] {
        // The renderer's initial bottom-right anchor can finish after show.
        // Wait for our position request to settle rather than racing it.
        let mut actual = window.outer_position().map_err(|e| e.to_string())?;
        for _ in 0..20 {
            window
                .set_position(tauri::PhysicalPosition::new(x, y))
                .map_err(|e| e.to_string())?;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            actual = window.outer_position().map_err(|e| e.to_string())?;
            if (actual.x - x).abs() <= 2 && (actual.y - y).abs() <= 2 {
                break;
            }
        }
        if (actual.x - x).abs() > 2 || (actual.y - y).abs() > 2 {
            return Err(format!(
                "requested ({x}, {y}), actual ({}, {})",
                actual.x, actual.y
            ));
        }
    }
    eprintln!("NATIVE_LAUNCHER_GEOMETRY_OK");
    window.hide().map_err(|e| e.to_string())?;
    Ok(())
}
