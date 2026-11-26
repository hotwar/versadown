use versadown_lib::engine_binary::{EngineBinary, EngineBinaryError};

fn bundled_linux_bytes() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries/aria2-next-x86_64-unknown-linux-gnu"),
    )
    .unwrap()
}

// These helpers parse the trusted, bundled fixture, not the malformed test inputs.
fn read_u64(bytes: &[u8], offset: usize) -> usize {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
}

fn read_u16(bytes: &[u8], offset: usize) -> usize {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize
}

fn load_headers(bytes: &[u8]) -> Vec<usize> {
    let start = read_u64(bytes, 0x20);
    let stride = read_u16(bytes, 0x36);
    (0..read_u16(bytes, 0x38))
        .map(|i| start + i * stride)
        .filter(|&offset| bytes[offset..offset + 4] == 1u32.to_le_bytes())
        .collect()
}

fn write_executable(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn assert_linux_checksum_rejected(bytes: &[u8], case: &str) {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("aria2-next");
    write_executable(&binary, bytes);
    let result = EngineBinary::for_target("linux", "x86_64")
        .unwrap()
        .verify(&binary);
    assert!(
        matches!(result, Err(EngineBinaryError::Checksum(ref path)) if path == &binary),
        "{case}: {result:?}"
    );
}

#[test]
fn linux_engine_accepts_appended_store_signature_metadata() {
    use sha2::{Digest, Sha256};

    let original = bundled_linux_bytes();
    let mut signed = original.clone();
    let old_shoff = read_u64(&original, 0x28);
    let shentsize = read_u16(&original, 0x3a);
    let shnum = read_u16(&original, 0x3c);
    let shstrndx = read_u16(&original, 0x3e);
    let mut section_headers = original[old_shoff..old_shoff + shnum * shentsize].to_vec();

    // Model the store's ELF changes, without requiring its private signing key:
    // append signature data, an extended section-name table and a new section table.
    // Only e_shoff and e_shnum change within the original loaded bytes.
    let signature_offset = signed.len();
    let signature = b"fixture signature";
    signed.extend_from_slice(signature);
    let str_header = shstrndx * shentsize;
    let str_offset = read_u64(&section_headers, str_header + 24);
    let str_size = read_u64(&section_headers, str_header + 32);
    let new_str_offset = signed.len();
    signed.extend_from_slice(&original[str_offset..str_offset + str_size]);
    signed.extend_from_slice(b".signature\0");
    section_headers[str_header + 24..str_header + 32]
        .copy_from_slice(&(new_str_offset as u64).to_le_bytes());
    section_headers[str_header + 32..str_header + 40]
        .copy_from_slice(&((str_size + 11) as u64).to_le_bytes());

    let mut signature_header = vec![0u8; shentsize];
    signature_header[..4].copy_from_slice(&(str_size as u32).to_le_bytes());
    signature_header[4..8].copy_from_slice(&1u32.to_le_bytes()); // SHT_PROGBITS, not SHF_ALLOC
    signature_header[24..32].copy_from_slice(&(signature_offset as u64).to_le_bytes());
    signature_header[32..40].copy_from_slice(&(signature.len() as u64).to_le_bytes());
    signature_header[48..56].copy_from_slice(&1u64.to_le_bytes());
    signed.resize((signed.len() + 7) & !7, 0);
    let new_shoff = signed.len();
    signed.extend_from_slice(&section_headers);
    signed.extend_from_slice(&signature_header);
    signed[0x28..0x30].copy_from_slice(&(new_shoff as u64).to_le_bytes());
    signed[0x3c..0x3e].copy_from_slice(&((shnum + 1) as u16).to_le_bytes());
    assert_ne!(Sha256::digest(&original), Sha256::digest(&signed));

    let directory = tempfile::tempdir().unwrap();
    let resources = directory.path().join("resources");
    std::fs::create_dir_all(resources.join("binaries")).unwrap();
    let engine = EngineBinary::for_target("linux", "x86_64").unwrap();
    let binary = resources.join("binaries").join(engine.resource_name());
    write_executable(&binary, &signed);
    engine.verify(&binary).unwrap();
    assert_eq!(
        engine
            .resolve(&directory.path().join("manifest"), &resources)
            .unwrap(),
        binary
    );
}

#[test]
fn linux_engine_rejects_tampering_in_every_load_segment() {
    let original = bundled_linux_bytes();
    let headers = load_headers(&original);
    assert_eq!(
        headers.len(),
        4,
        "update this fixture when upgrading the engine"
    );
    for (index, header) in headers.into_iter().enumerate() {
        let mut changed = original.clone();
        let offset = read_u64(&original, header + 8);
        let size = read_u64(&original, header + 32);
        changed[offset + size - 1] ^= 1;
        assert_linux_checksum_rejected(&changed, &format!("PT_LOAD {index}"));
    }
}

#[test]
fn linux_engine_rejects_execution_metadata_changes() {
    let original = bundled_linux_bytes();
    let first_load = load_headers(&original)[0];
    for (case, offset) in [
        ("entry point", 0x18),
        ("section header entry size", 0x3a),
        ("load permissions", first_load + 4),
        ("load virtual address", first_load + 16),
        ("load memory size", first_load + 40),
    ] {
        let mut changed = original.clone();
        changed[offset] ^= 1;
        assert_linux_checksum_rejected(&changed, case);
    }
}

#[test]
fn linux_engine_rejects_malformed_elf_without_panicking() {
    let original = bundled_linux_bytes();
    let first_load = load_headers(&original)[0];
    for size in [0, 4, 63, 128, original.len() / 2] {
        assert_linux_checksum_rejected(&original[..size], "truncated ELF");
    }
    for (case, offset, value) in [
        ("wrong ELF magic", 0, &[0][..]),
        ("wrong ELF class", 4, &[1][..]),
        ("wrong byte order", 5, &[2][..]),
        ("program table offset overflow", 0x20, &[0xff; 8][..]),
        ("short program header", 0x36, &[1, 0][..]),
        ("no program headers", 0x38, &[0, 0][..]),
        ("extended program count", 0x38, &[0xff, 0xff][..]),
        ("load offset overflow", first_load + 8, &[0xff; 8][..]),
        ("load size overflow", first_load + 32, &[0xff; 8][..]),
    ] {
        let mut changed = original.clone();
        changed[offset..offset + value.len()].copy_from_slice(value);
        assert_linux_checksum_rejected(&changed, case);
    }
}

#[test]
fn linux_engine_rejects_unhashed_execution_headers() {
    let original = bundled_linux_bytes();
    let first_load = load_headers(&original)[0];
    assert_eq!(read_u64(&original, first_load + 8), 0);
    let first_size = read_u64(&original, first_load + 32);

    // A PT_LOAD-only hash must not trust an unhashed ELF header/program table.
    // Move an already-normalised copy of the original first segment to EOF, then
    // point the real program table at it and change the real executable entry.
    // Naive hashing still sees the original segment digest and misses the change.
    let mut copied_segment = original[..first_size].to_vec();
    copied_segment[0x28..0x30].fill(0);
    copied_segment[0x3c..0x3e].fill(0);
    let mut changed = original.clone();
    changed.resize((changed.len() + 4095) & !4095, 0);
    let new_offset = changed.len();
    changed.extend_from_slice(&copied_segment);
    changed[first_load + 8..first_load + 16].copy_from_slice(&(new_offset as u64).to_le_bytes());
    changed[0x18] ^= 1;
    assert_linux_checksum_rejected(&changed, "relocated load segment hides execution headers");
}

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
    engine
        .verify(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .join(engine.resource_name()),
        )
        .unwrap();
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
