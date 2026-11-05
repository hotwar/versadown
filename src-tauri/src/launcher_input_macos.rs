//! AppKit ignores the transparent WebView's CSS hit regions. Poll the global
//! pointer so we can restore input even after the native window ignores it.
use super::{point_hits_regions, PhysicalLauncherRegion};
use crate::launcher_poll::{next_step, PollStep};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, RwLock,
};
use tauri::{Manager, WebviewWindow};

#[derive(Default)]
pub(crate) struct InputMonitor {
    regions: RwLock<Vec<PhysicalLauncherRegion>>,
    /// Latest launcher window with the generation that installed it.
    target: RwLock<Option<(u64, WebviewWindow)>>,
    generation: AtomicU64,
    started: AtomicBool,
}

impl InputMonitor {
    fn snapshot(&self) -> Option<(u64, WebviewWindow)> {
        self.target.read().ok().and_then(|target| target.clone())
    }

    /// Called when the polled window went invalid. Returns true if a newer
    /// window was installed meanwhile and this task should keep running.
    fn should_resume(&self, seen: u64) -> bool {
        self.started.store(false, Ordering::Release);
        self.generation.load(Ordering::Acquire) != seen
            && !self.started.swap(true, Ordering::AcqRel)
    }
}

pub(super) fn install(
    window: WebviewWindow,
    regions: Vec<PhysicalLauncherRegion>,
) -> Result<(), String> {
    let monitor = window.state::<Arc<InputMonitor>>().inner().clone();
    *monitor.regions.write().map_err(|error| error.to_string())? = regions;
    let generation = monitor.generation.fetch_add(1, Ordering::AcqRel) + 1;
    *monitor.target.write().map_err(|error| error.to_string())? =
        Some((generation, window.clone()));
    if monitor.started.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    tauri::async_runtime::spawn(poll_loop(monitor, window.app_handle().clone()));
    Ok(())
}

async fn poll_loop(monitor: Arc<InputMonitor>, app: tauri::AppHandle) {
    let mut previous = None;
    loop {
        let Some((seen, window)) = monitor.snapshot() else {
            monitor.started.store(false, Ordering::Release);
            return;
        };
        let exiting = app
            .try_state::<Arc<crate::ApplicationLifecycle>>()
            .is_some_and(|state| state.is_exiting());
        match next_step(exiting, window.is_visible().ok()) {
            PollStep::Stop => {
                if exiting || !monitor.should_resume(seen) {
                    monitor.started.store(false, Ordering::Release);
                    return;
                }
                previous = None;
            }
            PollStep::Idle(delay) => {
                previous = None;
                tokio::time::sleep(delay).await;
            }
            PollStep::Track(delay) => {
                if !track_pointer(&monitor, &window, &mut previous)
                    && !monitor.should_resume(seen)
                {
                    return;
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Returns false when the window became invalid or state is poisoned.
fn track_pointer(
    monitor: &InputMonitor,
    window: &WebviewWindow,
    previous: &mut Option<bool>,
) -> bool {
    let (Ok(pointer), Ok(origin)) = (window.cursor_position(), window.inner_position()) else {
        return false;
    };
    let point = (
        (pointer.x - f64::from(origin.x)).round() as i32,
        (pointer.y - f64::from(origin.y)).round() as i32,
    );
    let Ok(regions) = monitor.regions.read() else {
        return false;
    };
    let ignore = !point_hits_regions(point, &regions);
    drop(regions);
    if *previous != Some(ignore) && window.set_ignore_cursor_events(ignore).is_ok() {
        *previous = Some(ignore);
    }
    true
}
