#![cfg_attr(
    any(not(debug_assertions), feature = "gui-subsystem"),
    windows_subsystem = "windows"
)]

fn main() {
    if versadown_lib::run_if_chrome() {
        return;
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--xunlei-native-worker"))
    {
        if versadown_lib::run_xunlei_native_worker().is_err() {
            std::process::exit(1);
        }
        return;
    }
    versadown_lib::run();
}
