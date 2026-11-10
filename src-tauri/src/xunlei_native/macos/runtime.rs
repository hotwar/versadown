use super::super::engine::NativeEngineError;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const SERVICE_PATH: &str = "Contents/XPCServices/DownloadService.xpc";
const SERVICE_FILES: [(&str, u32); 3] = [
    ("Contents/MacOS/DownloadService", 0o700),
    ("Contents/Info.plist", 0o600),
    ("Contents/_CodeSignature/CodeResources", 0o600),
];

pub(super) struct ServiceVersion {
    pub(super) app_version: &'static str,
    pub(super) client_version: u32,
    // Hashes follow SERVICE_FILES order. The plist selects the executable XPC loads.
    sha256: [&'static str; SERVICE_FILES.len()],
}

const SUPPORTED_SERVICES: &[ServiceVersion] = &[ServiceVersion {
    app_version: "5.80.7",
    client_version: 66660,
    sha256: [
        "661f0f435dfbfb23ae5788c8f3fe9454f24f43c5e6f4ec5605eb9826b491071d",
        "d6d091a26236bf07f1d6a28ace8ee68c3a9025d3cd43857cd3c7bdbbb6b66904",
        "6686de10a28a2fe11b36cbb86dcbacc827cfc4ea116b4dabf1845e5aee629e9b",
    ],
}];

pub(crate) fn supported_versions() -> String {
    SUPPORTED_SERVICES
        .iter()
        .map(|v| format!("{} ({})", v.app_version, v.client_version))
        .collect::<Vec<_>>()
        .join(", ")
}

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/versadown-xunlei-worker"));
const HOST_PLIST: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.versadown.thunder-worker</string><key>CFBundleExecutable</key><string>worker</string><key>CFBundlePackageType</key><string>APPL</string><key>LSUIElement</key><true/></dict></plist>"#;

pub(super) struct Runtime {
    pub(super) executable: PathBuf,
    pub(super) version: &'static ServiceVersion,
    pub(super) state: PathBuf,
    _bundle: TempDir,
}

fn private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn install_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    private_directory(path.parent().unwrap())?;
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn verified_service<'a>(
    candidates: &[PathBuf],
    supported: &'a [ServiceVersion],
) -> Result<(&'a ServiceVersion, Vec<Vec<u8>>), NativeEngineError> {
    let mut failure = NativeEngineError::Unavailable;
    for app in candidates {
        match app.try_exists() {
            Ok(false) => continue,
            Err(_) => {
                failure = NativeEngineError::OperationFailed;
                continue;
            }
            Ok(true) => {}
        }
        let service = app.join(SERVICE_PATH);
        let files = SERVICE_FILES
            .iter()
            .map(|(file, _)| fs::read(service.join(file)))
            .collect::<std::io::Result<Vec<_>>>();
        match files {
            Ok(files) => {
                let hashes: Vec<_> = files
                    .iter()
                    .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
                    .collect();
                if let Some(version) = supported.iter().find(|v| hashes == v.sha256) {
                    return Ok((version, files));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(_) => {
                failure = NativeEngineError::OperationFailed;
                continue;
            }
        }
        if failure == NativeEngineError::Unavailable {
            failure = NativeEngineError::UnsupportedClientVersion;
        }
    }
    Err(failure)
}

pub(super) fn installed_runtime(root: &Path) -> Result<Runtime, NativeEngineError> {
    let mut candidates = vec![PathBuf::from("/Applications/Thunder.app")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications/Thunder.app"));
    }
    let (version, files) = verified_service(&candidates, SUPPORTED_SERVICES)?;
    // NSXPCConnection looks for the service inside its host bundle. Assemble
    // that bundle with only the verified bytes; never execute a cached worker.
    let io_error = |_| NativeEngineError::OperationFailed;
    let temporary = tempfile::Builder::new()
        .prefix("vd-thunder-")
        .tempdir()
        .map_err(io_error)?;
    let bundle = temporary.path().join("VersaDownThunder.app");
    for ((file, mode), bytes) in SERVICE_FILES.into_iter().zip(files) {
        install_file(&bundle.join(SERVICE_PATH).join(file), &bytes, mode).map_err(io_error)?;
    }
    let executable = bundle.join("Contents/MacOS/worker");
    install_file(&executable, HELPER, 0o700).map_err(io_error)?;
    install_file(&bundle.join("Contents/Info.plist"), HOST_PLIST, 0o600).map_err(io_error)?;
    private_directory(root).map_err(io_error)?;
    let state = root.join(format!("state-{}", &version.sha256[0][..12]));
    private_directory(&state).map_err(io_error)?;
    Ok(Runtime {
        executable,
        version,
        state,
        _bundle: temporary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_installed_service_by_all_three_verified_files() {
        const A: &str = "559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd";
        const B: &str = "df7e70e5021544f4834bbee64a9e3789febc4be81470df629cad6ddb03320a5c";
        let versions = [
            ServiceVersion {
                app_version: "1.0",
                client_version: 10,
                sha256: [A, A, A],
            },
            ServiceVersion {
                app_version: "2.0",
                client_version: 20,
                sha256: [A, B, B],
            },
        ];
        let root = tempfile::tempdir().unwrap();
        let candidates = [
            root.path().join("system/Thunder.app"),
            root.path().join("user/Thunder.app"),
        ];
        assert!(matches!(
            verified_service(&candidates, &versions),
            Err(NativeEngineError::Unavailable)
        ));
        fs::create_dir_all(&candidates[0]).unwrap();
        assert!(matches!(
            verified_service(&candidates, &versions),
            Err(NativeEngineError::UnsupportedClientVersion)
        ));

        let service = candidates[1].join(SERVICE_PATH);
        for ((file, mode), contents) in SERVICE_FILES.into_iter().zip([b"A", b"B", b"B"]) {
            install_file(&service.join(file), contents, mode).unwrap();
        }
        let (version, _) = verified_service(&candidates, &versions).unwrap();
        assert_eq!((version.app_version, version.client_version), ("2.0", 20));
        for ((file, _), contents) in SERVICE_FILES.into_iter().zip([b"A", b"B", b"B"]) {
            let path = service.join(file);
            fs::write(&path, b"changed").unwrap();
            assert!(matches!(
                verified_service(&candidates, &versions),
                Err(NativeEngineError::UnsupportedClientVersion)
            ));
            fs::write(path, contents).unwrap();
        }
        let unreadable = service.join(SERVICE_FILES[0].0);
        fs::remove_file(&unreadable).unwrap();
        fs::create_dir(unreadable).unwrap();
        assert!(matches!(
            verified_service(&candidates, &versions),
            Err(NativeEngineError::OperationFailed)
        ));
    }
}
