#![cfg_attr(
    any(not(debug_assertions), feature = "gui-subsystem"),
    windows_subsystem = "windows"
)]

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--xunlei-native-worker"))
    {
        if versadown_lib::run_xunlei_native_worker().is_err() {
            std::process::exit(1);
        }
        return;
    }
    let exit_code = versadown_lib::run();
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}
