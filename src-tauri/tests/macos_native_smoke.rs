#[cfg(target_os = "macos")]
fn main() {
    if std::env::var("VERSADOWN_NATIVE_SMOKE_CHILD").as_deref() != Ok("1") {
        // Tauri resolves macOS resources from a real application bundle.
        let staging = tempfile::tempdir().unwrap();
        let contents = staging.path().join("NativeSmoke.app/Contents");
        let executables = contents.join("MacOS");
        let resources = contents.join("Resources/binaries");
        std::fs::create_dir_all(&executables).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        let executable = executables.join("macos_native_smoke");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let engine = versadown_lib::engine_binary::EngineBinary::for_current_target().unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join(engine.resource_name()),
            resources.join(engine.resource_name()),
        )
        .unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>macos_native_smoke</string>
<key>CFBundleIdentifier</key><string>com.versadown.desktop.native-smoke</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        let status = std::process::Command::new(executable.canonicalize().unwrap())
            .env("VERSADOWN_NATIVE_SMOKE_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success(), "native app quit failed: {status}");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    // The real app runs on this process's main thread, with no user tasks,
    // login attempts or visible manager window. The geometry regression
    // briefly shows only the isolated launcher.
    std::env::set_var("VERSADOWN_BACKGROUND_UI_TEST", "1");
    std::env::set_var("VERSADOWN_NATIVE_SMOKE", "1");
    std::env::set_var("VERSADOWN_TEST_DATA_DIR", root.path().join("data"));
    std::env::set_var("VERSADOWN_TEST_DOWNLOAD_DIR", root.path().join("downloads"));
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let settings = serde_json::json!({"aria2": {"rpcPort": port.local_addr().unwrap().port()}});
    std::fs::create_dir_all(root.path().join("data")).unwrap();
    std::fs::write(root.path().join("data/settings.json"), settings.to_string()).unwrap();
    drop(port);
    versadown_lib::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS native smoke test: skipped on this platform");
}
