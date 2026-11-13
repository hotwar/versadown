use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

use tempfile::NamedTempFile;
use tokio::process::Command;
use zeroize::Zeroizing;

const MAX_COOKIE_BYTES: u64 = 2 * 1024 * 1024;
const COOKIE_ERROR: &str = "video.error.cookieFile";

/// A private working jar: yt-dlp reads and writes the file passed to --cookies.
/// Keep this guard alive until the child exits, so its jar survives the download.
pub(super) struct CookieFile {
    jar: NamedTempFile,
}

impl CookieFile {
    pub(super) fn load(path: Option<&Path>) -> Result<Option<Self>, String> {
        let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) else {
            return Ok(None);
        };
        Self::load_file(path)
            .map(Some)
            .map_err(|_| COOKIE_ERROR.to_owned())
    }

    fn load_file(path: &Path) -> Result<Self, ()> {
        if !path.is_absolute() || !fs::metadata(path).map_err(|_| ())?.is_file() {
            return Err(());
        }
        let source = File::open(path).map_err(|_| ())?;
        let metadata = source.metadata().map_err(|_| ())?;
        if !metadata.is_file() || metadata.len() > MAX_COOKIE_BYTES {
            return Err(());
        }
        let mut bytes = Zeroizing::new(Vec::new());
        source
            .take(MAX_COOKIE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() as u64 > MAX_COOKIE_BYTES {
            return Err(());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| ())?;
        validate(text)?;

        let mut jar = tempfile::Builder::new()
            .prefix(".versadown-cookies-")
            .suffix(".txt")
            .tempfile()
            .map_err(|_| ())?;
        // Restrict access before writing any cookie values. NamedTempFile then
        // removes this working copy on every normal return and unwind path.
        restrict_permissions(&jar).map_err(|_| ())?;
        jar.write_all(&bytes).map_err(|_| ())?;
        jar.flush().map_err(|_| ())?;
        Ok(Self { jar })
    }

    pub(super) fn apply(&self, command: &mut Command) {
        command.arg("--cookies").arg(self.jar.path());
    }
}

fn validate(text: &str) -> Result<(), ()> {
    if text.contains('\0') {
        return Err(());
    }
    let mut lines = text.lines();
    if !matches!(
        lines.next(),
        Some("# Netscape HTTP Cookie File" | "# HTTP Cookie File")
    ) {
        return Err(());
    }
    for line in lines {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 7
            || fields[0].is_empty()
            || fields[0].chars().any(char::is_whitespace)
            || !matches!(fields[1], "TRUE" | "FALSE")
            || fields[0].starts_with('.') != (fields[1] == "TRUE")
            || !matches!(fields[3], "TRUE" | "FALSE")
            || !valid_expiry(fields[4])
        {
            return Err(());
        }
    }
    Ok(())
}

fn valid_expiry(expiry: &str) -> bool {
    fn digits(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
    }
    // yt-dlp accepts empty/zero session expiries and decimal timestamps.
    expiry.is_empty()
        || digits(expiry)
        || expiry
            .split_once('.')
            .is_some_and(|(whole, fraction)| digits(whole) && digits(fraction))
}

#[cfg(unix)]
fn restrict_permissions(jar: &NamedTempFile) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    jar.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn restrict_permissions(jar: &NamedTempFile) -> std::io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor: *const u16,
            revision: u32,
            security: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn SetFileSecurityW(path: *const u16, information: u32, security: *const c_void) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    // A protected DACL grants full access only to the file owner, regardless
    // of inherited permissions on a customized Windows temporary directory.
    let descriptor: Vec<u16> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
    let path: Vec<u16> = jar
        .path()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut security = ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut security,
            ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION.
        let result = SetFileSecurityW(path.as_ptr(), 0x8000_0004, security);
        let error = (result == 0).then(std::io::Error::last_os_error);
        LocalFree(security);
        error.map_or(Ok(()), Err)
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict_permissions(_jar: &NamedTempFile) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private cookie files are unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIES: &str = "# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tTRUE\t0\tsession\tsynthetic-value\n";

    fn source(contents: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn no_configuration_keeps_anonymous_requests() {
        assert!(CookieFile::load(None).unwrap().is_none());
        assert!(CookieFile::load(Some(Path::new(""))).unwrap().is_none());
    }

    #[test]
    fn working_jar_is_separate_writable_and_removed_on_drop() {
        let original = source(COOKIES.as_bytes());
        let cookies = CookieFile::load(Some(original.path())).unwrap().unwrap();
        let working = cookies.jar.path().to_owned();
        assert_ne!(working, original.path());
        assert_eq!(fs::read(&working).unwrap(), COOKIES.as_bytes());
        // Model yt-dlp saving server-updated cookies at process exit.
        fs::write(&working, "# HTTP Cookie File\n").unwrap();
        assert_eq!(fs::read(original.path()).unwrap(), COOKIES.as_bytes());
        drop(cookies);
        assert!(!working.exists());
        assert!(original.path().is_file());
    }

    #[test]
    fn command_uses_working_jar_as_one_argument() {
        let original = source(COOKIES.as_bytes());
        let cookies = CookieFile::load(Some(original.path())).unwrap().unwrap();
        let mut command = Command::new("yt-dlp");
        cookies.apply(&mut command);
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args,
            [
                std::ffi::OsStr::new("--cookies"),
                cookies.jar.path().as_os_str()
            ]
        );
        assert!(!args.contains(&original.path().as_os_str()));
    }

    #[test]
    fn accepts_http_only_crlf_and_session_cookie_formats() {
        let contents = "# HTTP Cookie File\r\n# exported cookies\r\n#HttpOnly_.example.test\tTRUE\t/\tTRUE\t\tsession\tsynthetic\r\nexample.test\tFALSE\t/\tFALSE\t1234567890.5\tname\t\r\n";
        let original = source(contents.as_bytes());
        let cookies = CookieFile::load(Some(original.path())).unwrap().unwrap();
        assert_eq!(fs::read(cookies.jar.path()).unwrap(), contents.as_bytes());
    }

    #[test]
    fn rejects_invalid_cookie_content_without_disclosing_it() {
        for contents in [
            "",
            "[{\"name\":\"secret\"}]",
            "# Netscape HTTP Cookie File\nsecret\n",
            "# Netscape HTTP Cookie File\n.example.test\tFALSE\t/\tTRUE\t0\tname\tsecret\n",
            "# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tYES\t0\tname\tsecret\n",
            "# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tTRUE\tinvalid\tname\tsecret\n",
            "# Netscape HTTP Cookie File\n.example.test\tTRUE\t/\tTRUE\t0\tname\tsecret\0\n",
        ] {
            let original = source(contents.as_bytes());
            assert_eq!(
                CookieFile::load(Some(original.path())).err().unwrap(),
                COOKIE_ERROR
            );
        }
        let original = source(&[0xff, 0xfe]);
        assert_eq!(
            CookieFile::load(Some(original.path())).err().unwrap(),
            COOKIE_ERROR
        );
    }

    #[test]
    fn rejects_relative_missing_directory_and_oversized_inputs() {
        assert_eq!(
            CookieFile::load(Some(Path::new("cookies.txt")))
                .err()
                .unwrap(),
            COOKIE_ERROR
        );
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            CookieFile::load(Some(directory.path())).err().unwrap(),
            COOKIE_ERROR
        );
        assert_eq!(
            CookieFile::load(Some(&directory.path().join("missing.txt")))
                .err()
                .unwrap(),
            COOKIE_ERROR
        );
        let oversized = source(&vec![b'x'; MAX_COOKIE_BYTES as usize + 1]);
        assert_eq!(
            CookieFile::load(Some(oversized.path())).err().unwrap(),
            COOKIE_ERROR
        );
    }

    #[cfg(unix)]
    #[test]
    fn working_jar_is_readable_and_writable_only_by_owner() {
        use std::os::unix::fs::PermissionsExt;
        let original = source(COOKIES.as_bytes());
        let cookies = CookieFile::load(Some(original.path())).unwrap().unwrap();
        assert_eq!(
            fs::metadata(cookies.jar.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
