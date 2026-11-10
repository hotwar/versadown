use super::super::engine::NativeEngineError;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

const SERVICE_PATH: &str = "Contents/XPCServices/DownloadService.xpc";
const SERVICE_FILES: [(&str, u32); 3] = [
    ("Contents/MacOS/DownloadService", 0o700),
    ("Contents/Info.plist", 0o600),
    ("Contents/_CodeSignature/CodeResources", 0o600),
];

struct ServiceVersion {
    app_version: &'static str,
    client_version: u32,
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

pub(super) struct RuntimeVersion {
    pub(super) app_version: String,
    pub(super) client_version: u32,
}

pub(super) struct Runtime {
    pub(super) executable: PathBuf,
    pub(super) version: RuntimeVersion,
    pub(super) needs_probe: bool,
    pub(super) probe_key: String,
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

// Developer ID Application, issued under Apple's Developer ID CA to Xunlei.
// The Team ID and designated identifier must be checked by the verifier, not
// taken from the untrusted bundle and compared as display strings.
const DEVELOPER: &str = r#"anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = "SXKSDVK696""#;

fn verify_signature(bundle: &Path, requirement: &str) -> Result<(), NativeEngineError> {
    let status = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--all-architectures"])
        .arg(format!("-R={DEVELOPER} and {requirement}"))
        .arg(bundle)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| NativeEngineError::OperationFailed)?;
    if !status.success() {
        return Err(NativeEngineError::UnsupportedClientVersion);
    }
    Ok(())
}

fn version_field(plist: &Path, field: &str) -> Result<String, NativeEngineError> {
    let result = Command::new("/usr/bin/plutil")
        .args(["-extract", field, "raw", "-expect", "string"])
        .arg(plist)
        .output()
        .map_err(|_| NativeEngineError::OperationFailed)?;
    let value = std::str::from_utf8(&result.stdout)
        .unwrap_or_default()
        .trim_end();
    if !result.status.success()
        || value.is_empty()
        || value.len() > 64
        || !value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(NativeEngineError::UnsupportedClientVersion);
    }
    Ok(value.to_owned())
}

fn signed_version(app: &Path) -> Result<RuntimeVersion, NativeEngineError> {
    let plist = app.join("Contents/Info.plist");
    let app_version = version_field(&plist, "CFBundleShortVersionString")?;
    let build = version_field(&plist, "CFBundleVersion")?;
    let client_version = build
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or(NativeEngineError::UnsupportedClientVersion)?;
    // Verify the exact values that will be sent to initETM, including when the
    // app is being updated while we read it. Both strings are numeric above.
    verify_signature(
        app,
        &format!(
            r#"identifier "com.xunlei.Thunder" and info[CFBundleShortVersionString] = "{app_version}" and info[CFBundleVersion] = "{build}""#
        ),
    )?;
    Ok(RuntimeVersion {
        app_version,
        client_version,
    })
}

struct ServiceImage {
    files: Vec<Vec<u8>>,
    hashes: Vec<String>,
}

impl ServiceImage {
    fn read(app: &Path) -> Result<Self, NativeEngineError> {
        if !app
            .try_exists()
            .map_err(|_| NativeEngineError::OperationFailed)?
        {
            return Err(NativeEngineError::Unavailable);
        }
        let service = app.join(SERVICE_PATH);
        let files = SERVICE_FILES
            .iter()
            .map(|(file, _)| fs::read(service.join(file)))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    NativeEngineError::UnsupportedClientVersion
                }
                _ => NativeEngineError::OperationFailed,
            })?;
        let hashes = files
            .iter()
            .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
            .collect();
        Ok(Self { files, hashes })
    }

    fn stage(
        self,
        root: &Path,
        version: RuntimeVersion,
        needs_probe: bool,
    ) -> Result<Runtime, NativeEngineError> {
        let io_error = |_| NativeEngineError::OperationFailed;
        let temporary = tempfile::Builder::new()
            .prefix("vd-thunder-")
            .tempdir()
            .map_err(io_error)?;
        let bundle = temporary.path().join("VersaDownThunder.app");
        let service = bundle.join(SERVICE_PATH);
        for ((file, mode), bytes) in SERVICE_FILES.into_iter().zip(&self.files) {
            install_file(&service.join(file), bytes, mode).map_err(io_error)?;
        }
        if needs_probe {
            // Verify the private copy that XPC will launch. The original
            // bundle may have changed since source discovery.
            verify_signature(
                &service,
                r#"identifier "com.xunlei.DownloadService" and info[CFBundleExecutable] = "DownloadService""#,
            )?;
        }
        let executable = bundle.join("Contents/MacOS/worker");
        install_file(&executable, HELPER, 0o700).map_err(io_error)?;
        install_file(&bundle.join("Contents/Info.plist"), HOST_PLIST, 0o600).map_err(io_error)?;

        let image_key = format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}:{}",
                self.hashes.concat(),
                version.app_version,
                version.client_version
            ))
        );
        let state = if needs_probe {
            root.join(format!("state-signed-{image_key}"))
        } else {
            // Existing users' known-version SDK checkpoints live here.
            root.join(format!("state-{}", &self.hashes[0][..12]))
        };
        let probe_key = format!(
            "{:x}",
            Sha256::digest(format!(
                "v1:{image_key}:{}:{}:{:x}",
                std::env::consts::ARCH,
                env!("CARGO_PKG_VERSION"),
                Sha256::digest(HELPER)
            ))
        );
        private_directory(root).map_err(io_error)?;
        private_directory(&state).map_err(io_error)?;
        Ok(Runtime {
            executable,
            version,
            needs_probe,
            probe_key,
            state,
            _bundle: temporary,
        })
    }
}

fn installed_runtime_from(
    root: &Path,
    candidates: &[PathBuf],
    supported: &[ServiceVersion],
) -> Result<Runtime, NativeEngineError> {
    let mut failure = NativeEngineError::Unavailable;
    let mut unlisted = Vec::new();
    for app in candidates {
        match ServiceImage::read(app) {
            Ok(image) => {
                if let Some(known) = supported.iter().find(|v| image.hashes == v.sha256) {
                    return image.stage(
                        root,
                        RuntimeVersion {
                            app_version: known.app_version.to_owned(),
                            client_version: known.client_version,
                        },
                        false,
                    );
                }
                unlisted.push((app, image));
            }
            Err(NativeEngineError::Unavailable) => {}
            Err(error) => failure = error,
        }
    }
    for (app, image) in unlisted {
        match signed_version(app).and_then(|v| image.stage(root, v, true)) {
            Ok(runtime) => return Ok(runtime),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

pub(super) fn installed_runtime(root: &Path) -> Result<Runtime, NativeEngineError> {
    let mut candidates = vec![PathBuf::from("/Applications/Thunder.app")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications/Thunder.app"));
    }
    installed_runtime_from(root, &candidates, SUPPORTED_SERVICES)
}

#[cfg(test)]
pub(super) fn installed_runtime_for_probe_test(
    root: &Path,
    app: &Path,
) -> Result<Runtime, NativeEngineError> {
    installed_runtime_from(root, &[app.to_owned()], &[])
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
            installed_runtime_from(root.path(), &candidates, &versions),
            Err(NativeEngineError::Unavailable)
        ));
        fs::create_dir_all(&candidates[0]).unwrap();
        assert!(matches!(
            installed_runtime_from(root.path(), &candidates, &versions),
            Err(NativeEngineError::UnsupportedClientVersion)
        ));

        let service = candidates[1].join(SERVICE_PATH);
        for ((file, mode), contents) in SERVICE_FILES.into_iter().zip([b"A", b"B", b"B"]) {
            install_file(&service.join(file), contents, mode).unwrap();
        }
        let runtime = installed_runtime_from(root.path(), &candidates, &versions).unwrap();
        assert!(!runtime.needs_probe);
        let version = &runtime.version;
        assert_eq!(
            (version.app_version.as_str(), version.client_version),
            ("2.0", 20)
        );
        for ((file, _), contents) in SERVICE_FILES.into_iter().zip([b"A", b"B", b"B"]) {
            let path = service.join(file);
            fs::write(&path, b"changed").unwrap();
            assert!(matches!(
                installed_runtime_from(root.path(), &candidates, &versions),
                Err(NativeEngineError::UnsupportedClientVersion)
            ));
            fs::write(path, contents).unwrap();
        }
        let unreadable = service.join(SERVICE_FILES[0].0);
        fs::remove_file(&unreadable).unwrap();
        fs::create_dir(unreadable).unwrap();
        assert!(matches!(
            installed_runtime_from(root.path(), &candidates, &versions),
            Err(NativeEngineError::OperationFailed)
        ));
    }
    #[test]
    fn unlisted_service_requires_a_signed_client_even_with_plausible_version_fields() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Thunder.app");
        let source = app.join(SERVICE_PATH);
        for (file, mode) in SERVICE_FILES {
            install_file(&source.join(file), b"unsigned", mode).unwrap();
        }
        fs::write(app.join("Contents/Info.plist"), br#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>6.0.0</string><key>CFBundleVersion</key><string>99999</string></dict></plist>"#).unwrap();
        assert!(matches!(
            installed_runtime_for_probe_test(root.path(), &app),
            Err(NativeEngineError::UnsupportedClientVersion)
        ));
    }
}
