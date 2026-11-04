//! AppKit ignores the transparent WebView's CSS hit regions. Poll the global
//! pointer so we can restore input even after the native window ignores it.
use super::{point_hits_regions, PhysicalLauncherRegion};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};
use tauri::{Manager, WebviewWindow};

#[derive(Default)]
pub(crate) struct InputMonitor {
    regions: RwLock<Vec<PhysicalLauncherRegion>>,
    started: AtomicBool,
}

pub(super) fn install(
    window: WebviewWindow,
    regions: Vec<PhysicalLauncherRegion>,
) -> Result<(), String> {
    let monitor = window.state::<Arc<InputMonitor>>().inner().clone();
    *monitor.regions.write().map_err(|error| error.to_string())? = regions;
    if monitor.started.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    tauri::async_runtime::spawn(async move {
        let mut previous = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if window
                .app_handle()
                .try_state::<Arc<crate::ApplicationLifecycle>>()
                .is_some_and(|state| state.is_exiting())
            {
                break;
            }
            if !window.is_visible().unwrap_or(false) {
                continue;
            }
            let (Ok(pointer), Ok(origin)) = (window.cursor_position(), window.inner_position())
            else {
                break;
            };
            let point = (
                (pointer.x - f64::from(origin.x)).round() as i32,
                (pointer.y - f64::from(origin.y)).round() as i32,
            );
            let Ok(regions) = monitor.regions.read() else {
                break;
            };
            let ignore = !point_hits_regions(point, &regions);
            drop(regions);
            if previous != Some(ignore) && window.set_ignore_cursor_events(ignore).is_ok() {
                previous = Some(ignore);
            }
        }
        monitor.started.store(false, Ordering::Release);
    });
    Ok(())
}
