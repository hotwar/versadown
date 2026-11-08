use super::super::engine::NativeEngineError;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

const SERVICE_SHA256: &str = "661f0f435dfbfb23ae5788c8f3fe9454f24f43c5e6f4ec5605eb9826b491071d";
const SERVICE_PATH: &str = "Contents/XPCServices/DownloadService.xpc";
const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/versadown-xunlei-worker"));

pub(super) fn private_directory(path: &Path) -> Result<(), NativeEngineError> {
    fs::create_dir_all(path).map_err(|_| NativeEngineError::OperationFailed)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| NativeEngineError::OperationFailed)
}

pub(super) fn installed_runtime(root: &Path) -> Result<(PathBuf, PathBuf), NativeEngineError> {
    let mut candidates = vec![PathBuf::from("/Applications/Thunder.app")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications/Thunder.app"));
    }
    let app = candidates
        .into_iter()
        .find(|p| p.join(SERVICE_PATH).is_dir())
        .ok_or(NativeEngineError::Unavailable)?;
    let source = app.join(SERVICE_PATH);
    let bytes = fs::read(source.join("Contents/MacOS/DownloadService"))
        .map_err(|_| NativeEngineError::Unavailable)?;
    if format!("{:x}", Sha256::digest(bytes)) != SERVICE_SHA256 {
        return Err(NativeEngineError::Incompatible);
    }
    private_directory(root)?;
    let helper_hash = format!("{:x}", Sha256::digest(HELPER));
    let runtime = root.join(format!(
        "runtime-{}-{}",
        &SERVICE_SHA256[..12],
        &helper_hash[..12]
    ));
    let bundle = runtime.join("VersaDownThunder.app");
    let executable = bundle.join("Contents/MacOS/worker");
    if !runtime.join("ready").is_file() {
        private_directory(&runtime)?;
        private_directory(&bundle.join("Contents/MacOS"))?;
        fs::write(&executable, HELPER).map_err(|_| NativeEngineError::OperationFailed)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .map_err(|_| NativeEngineError::OperationFailed)?;
        fs::write(bundle.join("Contents/Info.plist"), br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.versadown.thunder-worker</string><key>CFBundleExecutable</key><string>worker</string><key>CFBundlePackageType</key><string>APPL</string><key>LSUIElement</key><true/></dict></plist>"#).map_err(|_| NativeEngineError::OperationFailed)?;
        let copied = Command::new("/usr/bin/ditto")
            .arg(&source)
            .arg(bundle.join(SERVICE_PATH))
            .status()
            .map_err(|_| NativeEngineError::OperationFailed)?;
        if !copied.success() {
            return Err(NativeEngineError::OperationFailed);
        }
        fs::write(runtime.join("ready"), b"1").map_err(|_| NativeEngineError::OperationFailed)?;
    }
    let state = root.join(format!("state-{}", &SERVICE_SHA256[..12]));
    private_directory(&state)?;
    Ok((executable, state))
}
