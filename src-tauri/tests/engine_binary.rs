use versadown_lib::engine_binary::{EngineBinary, EngineBinaryError};

#[test]
fn linux_x86_64_uses_the_pinned_resource_name() {
    assert_eq!(
        EngineBinary::for_target("linux", "x86_64")
            .unwrap()
            .resource_name(),
        "aria2-next-x86_64-unknown-linux-gnu"
    );
}

#[test]
fn windows_x86_64_resource_mapping_is_unchanged() {
    assert_eq!(
        EngineBinary::for_target("windows", "x86_64")
            .unwrap()
            .resource_name(),
        "aria2-next-x86_64-pc-windows-msvc.exe"
    );
}

#[test]
fn macos_aarch64_uses_the_pinned_resource_name() {
    assert_eq!(
        EngineBinary::for_target("macos", "aarch64")
            .unwrap()
            .resource_name(),
        "aria2-next-aarch64-apple-darwin"
    );
}

#[test]
fn bundled_intel_macos_engine_matches_its_target_and_checksum() {
    let engine = EngineBinary::for_target("macos", "x86_64").unwrap();
    assert_eq!(engine.resource_name(), "aria2-next-x86_64-apple-darwin");
    engine.verify(&std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries").join(engine.resource_name())).unwrap();
}

#[test]
fn unsupported_target_returns_a_typed_error() {
    assert!(matches!(
        EngineBinary::for_target("linux", "aarch64"),
        Err(EngineBinaryError::UnsupportedTarget { .. })
    ));
}

#[test]
fn bundled_linux_engine_matches_the_runtime_checksum_pin() {
    // A sidecar upgrade must update the pin, or the app refuses to start.
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries/aria2-next-x86_64-unknown-linux-gnu");
    EngineBinary::for_target("linux", "x86_64")
        .unwrap()
        .verify(&binary)
        .unwrap();
}

#[test]
fn bundled_macos_engine_matches_the_runtime_checksum_pin() {
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries/aria2-next-aarch64-apple-darwin");
    EngineBinary::for_target("macos", "aarch64")
        .unwrap()
        .verify(&binary)
        .unwrap();
}

#[test]
fn missing_macos_engine_returns_a_typed_error() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing-aria2-next");
    assert!(matches!(
        EngineBinary::for_target("macos", "aarch64")
            .unwrap()
            .verify(&missing),
        Err(EngineBinaryError::Missing(path)) if path == missing
    ));
}

#[test]
fn macos_engine_checksum_mismatch_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("invalid-aria2-next");
    std::fs::write(&binary, b"not the pinned aria2-next binary").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();
    }
    assert!(matches!(
        EngineBinary::for_target("macos", "aarch64")
            .unwrap()
            .verify(&binary),
        Err(EngineBinaryError::Checksum(path)) if path == binary
    ));
}
