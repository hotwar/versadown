use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

// All PT_LOAD contents, with only e_shoff/e_shnum normalised for deepin/UOS signing.
const LINUX_X86_64_SHA256: &str =
    "123a1f7cb01c8bc1cb7637884d34ee2b007540efdd091536e33606768b15dcac";
const MACOS_AARCH64_SHA256: &str =
    "be6e8ee76a8124fca03d20fa2a8446bac35ea0f24df7e159aa9206106bcfe47c";
const MACOS_X86_64_SHA256: &str =
    "68b4c64c338b27b2bf929b9110393420908f79de4e33e6037973b38577b715a5";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineBinary {
    resource_name: &'static str,
    checksum: Option<BinaryChecksum>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BinaryChecksum {
    Sha256(&'static str),
    ElfLoadSegmentsSha256(&'static str),
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
                checksum: None,
            }),
            ("linux", "x86_64") => Ok(Self {
                resource_name: "aria2-next-x86_64-unknown-linux-gnu",
                checksum: Some(BinaryChecksum::ElfLoadSegmentsSha256(LINUX_X86_64_SHA256)),
            }),
            ("macos", "aarch64") => Ok(Self {
                resource_name: "aria2-next-aarch64-apple-darwin",
                checksum: Some(BinaryChecksum::Sha256(MACOS_AARCH64_SHA256)),
            }),
            ("macos", "x86_64") => Ok(Self {
                resource_name: "aria2-next-x86_64-apple-darwin",
                checksum: Some(BinaryChecksum::Sha256(MACOS_X86_64_SHA256)),
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
        if let Some(checksum) = self.checksum {
            let bytes = std::fs::read(path).map_err(|_| EngineBinaryError::Read(path.into()))?;
            let (actual, expected) = match checksum {
                BinaryChecksum::Sha256(expected) => (format!("{:x}", Sha256::digest(&bytes)), expected),
                BinaryChecksum::ElfLoadSegmentsSha256(expected) => (
                    normalised_load_segments_digest(&bytes)
                        .ok_or_else(|| EngineBinaryError::Checksum(path.into()))?,
                    expected,
                ),
            };
            if actual != expected {
                return Err(EngineBinaryError::Checksum(path.into()));
            }
        }
        Ok(())
    }
}

/// Authenticate the pinned ELF64 little-endian image while allowing deepin/UOS
/// to append signing metadata and rewrite the section-table offset/count.
/// The first load segment must cover the ELF header and program-header table:
/// the checksum must authenticate the entry point and mapping instructions too.
fn normalised_load_segments_digest(bytes: &[u8]) -> Option<String> {
    const ELF_HEADER_LEN: usize = 64;
    const PROGRAM_HEADER_LEN: usize = 56;
    const PT_LOAD: u32 = 1;
    const E_SHOFF: usize = 0x28;
    const E_SHNUM: usize = 0x3c;

    if bytes.len() < ELF_HEADER_LEN
        || &bytes[..4] != b"\x7fELF"
        || bytes[4..7] != [2, 1, 1] // ELF64, little-endian, current version
        || u16::from_le_bytes(bytes[0x34..0x36].try_into().ok()?) != ELF_HEADER_LEN as u16
    {
        return None;
    }
    let phoff = usize::try_from(u64::from_le_bytes(bytes[0x20..0x28].try_into().ok()?)).ok()?;
    let phentsize = usize::from(u16::from_le_bytes(bytes[0x36..0x38].try_into().ok()?));
    let phnum = usize::from(u16::from_le_bytes(bytes[0x38..0x3a].try_into().ok()?));
    // Extended program-header counts depend on the unauthenticated section table.
    if phoff < ELF_HEADER_LEN || phentsize != PROGRAM_HEADER_LEN || phnum == 0 || phnum == 0xffff {
        return None;
    }
    let table_end = phoff.checked_add(phnum.checked_mul(phentsize)?)?;
    let table = bytes.get(phoff..table_end)?;
    let mut hasher = Sha256::new();
    let mut found_load = false;
    for ph in table.chunks_exact(PROGRAM_HEADER_LEN) {
        if u32::from_le_bytes(ph[..4].try_into().ok()?) != PT_LOAD {
            continue;
        }
        let offset = usize::try_from(u64::from_le_bytes(ph[8..16].try_into().ok()?)).ok()?;
        let size = usize::try_from(u64::from_le_bytes(ph[32..40].try_into().ok()?)).ok()?;
        let end = offset.checked_add(size)?;
        let segment = bytes.get(offset..end)?;
        if !found_load && (offset != 0 || end < table_end) {
            return None;
        }
        found_load = true;
        if offset == 0 {
            if segment.len() < ELF_HEADER_LEN {
                return None;
            }
            hasher.update(&segment[..E_SHOFF]);
            hasher.update([0; 8]);
            hasher.update(&segment[E_SHOFF + 8..E_SHNUM]);
            hasher.update([0; 2]);
            hasher.update(&segment[E_SHNUM + 2..]);
        } else {
            hasher.update(segment);
        }
    }
    found_load.then(|| format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
