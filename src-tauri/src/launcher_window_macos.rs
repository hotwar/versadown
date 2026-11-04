//! Configures only the launcher to let its transparent canvas extend above
//! the work area. Its visible icon remains clamped by the drag controller.
use std::ffi::{c_char, c_void};
use tauri::WebviewWindow;

#[link(name = "objc")]
unsafe extern "C" {
    fn object_getClass(object: *mut c_void) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn class_respondsToSelector(class: *mut c_void, selector: *mut c_void) -> bool;
    #[link_name = "objc_msgSend"]
    fn send_bool(object: *mut c_void, selector: *mut c_void, value: i8);
}

/// Runs on the AppKit main thread during launcher creation/setup.
pub(super) fn allow_canvas_above_screen(window: &WebviewWindow) -> Result<(), String> {
    let native = window.ns_window().map_err(|e| e.to_string())?;
    // SAFETY: this selector is registered by our pinned Tao window class,
    // takes one Objective-C BOOL, and never changes the object's class or
    // ownership. The NSWindow is alive and this runs on the main thread.
    unsafe {
        let selector = sel_registerName(c"versadownSetAllowsOffscreen:".as_ptr());
        if !class_respondsToSelector(object_getClass(native), selector) {
            return Err("macOS launcher positioning adapter is unavailable".into());
        }
        send_bool(native, selector, 1);
    }
    Ok(())
}
