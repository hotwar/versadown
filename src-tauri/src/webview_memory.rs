//! Linux WebKitGTK memory defaults.
//!
//! The JSC JIT reserves executable memory that this small React UI does not
//! need; it must be disabled before WebKitGTK spawns its web process, and a
//! value the user already exported always wins. Accelerated compositing stays
//! on: the transparent launcher needs it (see the test below), and WebKitGTK
//! 2.52 kills the web process when a live view's acceleration policy changes.
//!
//! The NVIDIA proprietary driver cannot back WebKitGTK's DMABUF renderer with
//! GBM buffers (`DRM_IOCTL_MODE_CREATE_DUMB failed`), which leaves every window
//! blank. On that driver the renderer keeps running but hands frames over in
//! shared memory instead of GBM buffers. Do not use
//! `WEBKIT_DISABLE_DMABUF_RENDERER` instead: WebKitGTK 2.52 then creates no
//! accelerated backing store, yet a page that still enters compositing mode
//! (the Baidu login page does) dereferences it and crashes the whole app.

/// Environment defaults applied before Tauri starts WebKitGTK.
const WEBKIT_ENV_DEFAULTS: [(&str, &str); 1] = [("JSC_useJIT", "0")];

/// Extra default for the NVIDIA proprietary driver.
const NVIDIA_ENV_DEFAULT: (&str, &str) = ("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1");

/// Returns the defaults the process environment does not already define.
pub fn missing_webkit_env_defaults(
    is_defined: impl Fn(&str) -> bool,
    nvidia_proprietary: bool,
) -> Vec<(&'static str, &'static str)> {
    WEBKIT_ENV_DEFAULTS
        .into_iter()
        .chain(nvidia_proprietary.then_some(NVIDIA_ENV_DEFAULT))
        .filter(|(name, _)| !is_defined(name))
        .collect()
}

/// The proprietary module creates this file; nouveau does not, and nouveau
/// handles GBM buffers fine.
#[cfg(target_os = "linux")]
fn nvidia_proprietary_driver_loaded() -> bool {
    std::path::Path::new("/proc/driver/nvidia/version").exists()
}

/// Must run before any thread is spawned: `set_var` is not synchronised with
/// concurrent C `getenv` readers.
#[cfg(target_os = "linux")]
pub fn apply_process_defaults() {
    for (name, value) in missing_webkit_env_defaults(
        |name| std::env::var_os(name).is_some(),
        nvidia_proprietary_driver_loaded(),
    ) {
        std::env::set_var(name, value);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply_process_defaults() {}

/// The UI is a local app, not a browser: drop WebKit's page and resource
/// caches. The web context is shared by every window, so this is idempotent.
#[cfg(target_os = "linux")]
pub fn configure_webview(window: &tauri::WebviewWindow) {
    use webkit2gtk::{CacheModel, WebContextExt, WebViewExt};

    let result = window.with_webview(|webview| {
        if let Some(context) = webview.inner().context() {
            context.set_cache_model(CacheModel::DocumentViewer);
        }
    });
    if let Err(error) = result {
        eprintln!("无法设置 WebKit 缓存模式: {error}");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn configure_webview(_window: &tauri::WebviewWindow) {}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_every_default_to_a_clean_environment() {
        assert_eq!(
            missing_webkit_env_defaults(|_| false, false),
            vec![("JSC_useJIT", "0")]
        );
    }

    #[test]
    fn forces_shared_memory_frames_on_nvidia_proprietary_driver() {
        assert_eq!(
            missing_webkit_env_defaults(|_| false, true),
            vec![("JSC_useJIT", "0"), ("WEBKIT_DMABUF_RENDERER_FORCE_SHM", "1")]
        );
    }

    #[test]
    fn compositing_is_not_disabled_process_wide() {
        // A process-wide switch also reaches the transparent launcher, which
        // then leaves stale popup pixels and repaints its canvas on every drag.
        assert!(missing_webkit_env_defaults(|_| false, true)
            .iter()
            .all(|(name, _)| *name != "WEBKIT_DISABLE_COMPOSITING_MODE"
                && *name != "WEBKIT_DISABLE_DMABUF_RENDERER"));
    }

    #[test]
    fn keeps_values_the_user_already_exported() {
        assert!(missing_webkit_env_defaults(
            |name| name == "JSC_useJIT" || name == "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
            true
        )
        .is_empty());
    }
}
