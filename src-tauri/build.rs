fn main() {
    // The quick build injects a content signature for the Vite source tree.
    // This forces Tauri to re-embed the matching dist assets without clearing
    // Cargo's incremental Rust target.
    println!("cargo:rerun-if-env-changed=VERSADOWN_FRONTEND_SIGNATURE");

    let mut attributes = tauri_build::Attributes::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // Tauri's resource manifest is linked only into application binaries.
        // Tests also link GUI APIs that require Common Controls v6, so embed
        // the same manifest in every target, including lib unit tests.
        // https://github.com/tauri-apps/tauri/issues/13419
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        let manifest = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
        )
        .join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }

    tauri_build::try_build(attributes).expect("failed to build Tauri resources");
}
