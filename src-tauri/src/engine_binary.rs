use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

const LINUX_X86_64_SHA256: &str =
    "ca0bfc952f15d5aa79afa73189f6665694a61b6ed0f55d44009389f8f08cc6db";
const MACOS_AARCH64_SHA256: &str =
    "be6e8ee76a8124fca03d20fa2a8446bac35ea0f24df7e159aa9206106bcfe47c";
const MACOS_X86_64_SHA256: &str =
    "68b4c64c338b27b2bf929b9110393420908f79de4e33e6037973b38577b715a5";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineBinary {
    resource_name: &'static str,
    sha256: Option<&'static str>,
}

#[derive(Debug, Error)]
pub enum EngineBinaryError {
    #[error("unsupported download-engine target: {os}/{arch}")]
    UnsupportedTarget { os: String, arch: String },
    #[error("download engine is missing: {}", .0.display())]
    Missing(PathBuf),
    #[error("download engine checksum does not match: {}", .0.display())]
    Checksum(PathBuf),
    #[error("download engine is not executable: {}", .0.display())]
    NotExecutable(PathBuf),
    #[error("cannot read download engine: {}", .0.display())]
    Read(PathBuf),
}

impl EngineBinary {
    pub fn for_current_target() -> Result<Self, EngineBinaryError> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn for_target(os: &str, arch: &str) -> Result<Self, EngineBinaryError> {
        match (os, arch) {
            ("windows", "x86_64") => Ok(Self {
                resource_name: "aria2-next-x86_64-pc-windows-msvc.exe",
                sha256: None,
            }),
            ("linux", "x86_64") => Ok(Self {
                resource_name: "aria2-next-x86_64-unknown-linux-gnu",
                sha256: Some(LINUX_X86_64_SHA256),
            }),
            ("macos", "aarch64") => Ok(Self {
                resource_name: "aria2-next-aarch64-apple-darwin",
                sha256: Some(MACOS_AARCH64_SHA256),
            }),
            ("macos", "x86_64") => Ok(Self {
                resource_name: "aria2-next-x86_64-apple-darwin",
                sha256: Some(MACOS_X86_64_SHA256),
            }),
            _ => Err(EngineBinaryError::UnsupportedTarget {
                os: os.into(),
                arch: arch.into(),
            }),
        }
    }

    pub fn resource_name(self) -> &'static str {
        self.resource_name
    }

    pub fn resolve(
        self,
        manifest_dir: &Path,
        resource_dir: &Path,
    ) -> Result<PathBuf, EngineBinaryError> {
        let bundled = resource_dir.join("binaries").join(self.resource_name);
        let candidate = if bundled.is_file() {
            bundled
        } else {
            manifest_dir.join("binaries").join(self.resource_name)
        };
        self.verify(&candidate)?;
        Ok(candidate)
    }

    pub fn verify(self, path: &Path) -> Result<(), EngineBinaryError> {
        if !path.is_file() {
            return Err(EngineBinaryError::Missing(path.into()));
        }
        #[cfg(unix)]
        if std::fs::metadata(path)
            .map_err(|_| EngineBinaryError::Read(path.into()))?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(EngineBinaryError::NotExecutable(path.into()));
        }
        if let Some(expected) = self.sha256 {
            let bytes = std::fs::read(path).map_err(|_| EngineBinaryError::Read(path.into()))?;
            if format!("{:x}", Sha256::digest(bytes)) != expected {
                return Err(EngineBinaryError::Checksum(path.into()));
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
