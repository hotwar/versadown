fn main() {
    emit_git_build_info();

    // The quick build injects a content signature for the Vite source tree.
    // This forces Tauri to re-embed the matching dist assets without clearing
    // Cargo's incremental Rust target.
    println!("cargo:rerun-if-env-changed=VERSADOWN_FRONTEND_SIGNATURE");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let source = "src/xunlei_native/macos/worker.m";
        println!("cargo:rerun-if-changed={source}");
        let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
            Ok("aarch64") => "arm64",
            Ok("x86_64") => "x86_64",
            _ => panic!("unsupported macOS architecture"),
        };
        let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("versadown-xunlei-worker");
        let status = std::process::Command::new("xcrun")
            .args([
                "clang",
                "-arch",
                arch,
                "-mmacosx-version-min=12.0",
                "-fobjc-arc",
                "-fblocks",
                "-O2",
                "-framework",
                "Foundation",
                source,
                "-o",
            ])
            .arg(output)
            .status()
            .expect("macOS clang is required");
        assert!(status.success(), "failed to build the Xunlei XPC bridge");
    }

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

fn git_output(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        // Reading status must not refresh the index we ask Cargo to watch.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

fn emit_git_build_info() {
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
    );
    let repo = manifest
        .parent()
        .expect("the application lives inside the repository");
    let commit = git_output(repo, &["rev-parse", "--verify", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_owned();
    let branch = git_output(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_owned();
    let dirty = git_output(repo, &["status", "--porcelain=v1", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty());
    println!("cargo:rustc-env=VERSADOWN_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=VERSADOWN_BUILD_BRANCH={branch}");
    println!("cargo:rustc-env=VERSADOWN_BUILD_DIRTY={dirty}");

    // Resolve Git's own paths instead of assuming .git is a directory. Linked
    // worktrees keep HEAD/index separately from their shared branch references.
    for name in ["HEAD", "index", "refs", "packed-refs"] {
        if let Some(path) = git_output(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        ) {
            let path = std::path::Path::new(path.trim());
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    // Cargo must refresh the dirty snapshot when tracked source changes, even
    // if HEAD does not. Avoid recursively watching dependencies and build caches.
    if let Some(files) = git_output(repo, &["ls-files", "-z"]) {
        for file in files.split('\0').filter(|file| !file.is_empty()) {
            println!("cargo:rerun-if-changed={}", repo.join(file).display());
        }
    }
}
