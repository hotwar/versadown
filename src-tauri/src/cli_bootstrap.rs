//! On-demand launch shared by CLI and MCP. Never reads desktop settings or data.
use crate::cli_protocol::Response;
use std::{fs::OpenOptions, future::Future, path::{Path, PathBuf}, process::{Child, Command, Stdio}, time::Duration};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(super) fn automatic_start_allowed(no_start: bool, explicit: bool, environment_override: bool) -> bool {
    !no_start && !explicit && !environment_override
}

fn host_executable(client_executable: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "versadown.exe" } else { "versadown" };
    client_executable.parent().map(|directory| directory.join(name))
}

trait StartedHost {
    fn exit_code(&mut self) -> std::io::Result<Option<i32>>;
}

struct DetachedHost(Option<Child>);

impl StartedHost for DetachedHost {
    fn exit_code(&mut self) -> std::io::Result<Option<i32>> {
        self.0.as_mut().expect("host process handle").try_wait()
            .map(|status| status.map(|status| status.code().unwrap_or(1)))
    }
}

#[cfg(unix)]
impl Drop for DetachedHost {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if matches!(child.try_wait(), Ok(Some(_))) { return; }
            // A long-lived MCP process remains the Unix parent after readiness.
            // Reap eventual exit without killing the host or blocking async work.
            // This detached thread does not keep the client process alive.
            let _ = std::thread::Builder::new().name("vd-host-reaper".into())
                .spawn(move || { let _ = child.wait(); });
        }
    }
}

fn launch(executable: &Path) -> std::io::Result<DetachedHost> {
    let mut command = Command::new(executable);
    command.arg("--agent-host").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console and no inherited Ctrl-C group. Do not kill on client exit.
        command.creation_flags(0x00000008 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // setsid is async-signal-safe; no Rust allocation/locking in pre_exec.
        unsafe { command.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }); }
    }
    command.spawn().map(|child| DetachedHost(Some(child)))
}

pub(super) async fn ensure_host<T, Probe, Pending>(discovery_path: &Path, lang: &str, probe: Probe) -> Result<T, Response>
where Probe: FnMut() -> Pending, Pending: Future<Output = Result<T, Response>> {
    let executable = std::env::current_exe().ok().and_then(|path| host_executable(&path))
        .ok_or_else(|| Response::failure("HOST_UNAVAILABLE", lang))?;
    let profile = discovery_path.parent().and_then(Path::parent)
        .ok_or_else(|| Response::failure("HOST_UNAVAILABLE", lang))?;
    ensure_with(profile, lang, START_TIMEOUT, probe, || launch(&executable)).await
}

async fn ensure_with<T, Probe, Pending, Launch, Process>(profile: &Path, lang: &str, timeout: Duration, mut probe: Probe, mut launch: Launch) -> Result<T, Response>
where Probe: FnMut() -> Pending, Pending: Future<Output = Result<T, Response>>,
      Launch: FnMut() -> std::io::Result<Process>, Process: StartedHost {
    let deadline = tokio::time::Instant::now() + timeout;
    match tokio::time::timeout_at(deadline, probe()).await {
        Ok(Ok(ready)) => return Ok(ready),
        Ok(Err(error)) if unavailable(&error) => (),
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(Response::failure("TIMEOUT", lang)),
    }
    std::fs::create_dir_all(profile).map_err(|_| Response::failure("HOST_UNAVAILABLE", lang))?;
    // Keep a stable inode: removing a lock file can let two processes lock
    // different files with the same name. File locks release on crash/exit.
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(profile.join("agent-start.lock")).map_err(|_| Response::failure("HOST_UNAVAILABLE", lang))?;
    let mut locked = false;
    let mut child: Option<Process> = None;
    loop {
        if tokio::time::Instant::now() >= deadline { return Err(Response::failure("TIMEOUT", lang)); }
        if !locked {
            match lock.try_lock() {
                Ok(()) => locked = true,
                Err(std::fs::TryLockError::WouldBlock) => (),
                Err(_) => return Err(Response::failure("HOST_UNAVAILABLE", lang)),
            }
        }
        match tokio::time::timeout_at(deadline, probe()).await {
            Ok(Ok(ready)) => return Ok(ready),
            Ok(Err(error)) if unavailable(&error) => (),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(Response::failure("TIMEOUT", lang)),
        }
        if locked && child.is_none() {
            // Another client may have made the host ready while we waited for
            // the lock; the probe above always runs before launching.
            child = Some(launch().map_err(|_| Response::failure("HOST_UNAVAILABLE", lang))?);
        }
        if let Some(child) = child.as_mut() {
            match child.exit_code() {
                Ok(Some(4)) => return Err(Response::failure("HOST_DISABLED", lang)),
                Ok(Some(0)) | Ok(None) => (), // Existing host may still be starting.
                _ => return Err(Response::failure("HOST_UNAVAILABLE", lang)),
            }
        }
        tokio::time::sleep_until((tokio::time::Instant::now() + POLL_INTERVAL).min(deadline)).await;
    }
}

fn unavailable(response: &Response) -> bool {
    response.error.as_ref().is_some_and(|error| error.code == "HOST_UNAVAILABLE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

    struct FakeProcess(Option<i32>);
    impl StartedHost for FakeProcess { fn exit_code(&mut self) -> std::io::Result<Option<i32>> { Ok(self.0) } }

    #[test]
    fn custom_discovery_and_no_start_never_launch_the_default_profile() {
        assert!(automatic_start_allowed(false, false, false));
        assert!(!automatic_start_allowed(true, false, false));
        assert!(!automatic_start_allowed(false, true, false));
        assert!(!automatic_start_allowed(false, false, true));
        let directory = tempfile::tempdir().unwrap();
        let executable = host_executable(&directory.path().join("vd-cli.exe")).unwrap();
        assert_eq!(executable.parent(), Some(directory.path()));
        assert_eq!(executable.file_name().unwrap(), if cfg!(windows) { "versadown.exe" } else { "versadown" });
    }

    #[tokio::test]
    async fn ready_or_rejected_hosts_are_not_started() {
        let directory = tempfile::tempdir().unwrap();
        let ready = ensure_with(directory.path(), "en", Duration::from_secs(1), || async { Ok(42) }, || -> std::io::Result<FakeProcess> { panic!("must not spawn") }).await.unwrap();
        assert_eq!(ready, 42);
        for code in ["HOST_DISABLED", "UNAUTHORIZED", "PROTOCOL_ERROR", "TIMEOUT"] {
            let result = ensure_with(directory.path(), "en", Duration::from_secs(1), || async { Err::<(), _>(Response::failure(code, "en")) }, || -> std::io::Result<FakeProcess> { panic!("must not spawn") }).await;
            assert_eq!(result.unwrap_err().error.unwrap().code, code);
        }
    }

    #[tokio::test]
    async fn concurrent_clients_share_one_start_and_recheck_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let starts = Arc::new(AtomicUsize::new(0));
        let mut calls = Vec::new();
        for _ in 0..8 {
            let profile = directory.path().to_owned();
            let ready = ready.clone();
            let starts = starts.clone();
            calls.push(tokio::spawn(async move {
                let launched_ready = ready.clone();
                ensure_with(&profile, "en", Duration::from_secs(3), || {
                    let ready = ready.clone();
                    async move { if ready.load(Ordering::SeqCst) { Ok(()) } else { Err(Response::failure("HOST_UNAVAILABLE", "en")) } }
                }, || {
                    starts.fetch_add(1, Ordering::SeqCst);
                    let ready = launched_ready.clone();
                    tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(150)).await; ready.store(true, Ordering::SeqCst); });
                    Ok(FakeProcess(None))
                }).await.unwrap();
            }));
        }
        for call in calls { call.await.unwrap(); }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn startup_refusal_failure_and_timeout_are_bounded() {
        let directory = tempfile::tempdir().unwrap();
        for (exit, expected) in [(Some(4), "HOST_DISABLED"), (Some(1), "HOST_UNAVAILABLE"), (Some(0), "TIMEOUT"), (None, "TIMEOUT")] {
            let result = ensure_with(directory.path(), "en", Duration::from_millis(30), || async { Err::<(), _>(Response::failure("HOST_UNAVAILABLE", "en")) }, || Ok(FakeProcess(exit))).await;
            assert_eq!(result.unwrap_err().error.unwrap().code, expected);
        }
        let result = ensure_with(directory.path(), "en", Duration::from_millis(30), std::future::pending::<Result<(), Response>>, || -> std::io::Result<FakeProcess> { panic!("hung probe must not spawn") }).await;
        assert_eq!(result.unwrap_err().error.unwrap().code, "TIMEOUT");
        // A timed-out attempt released the startup lock for later clients.
        OpenOptions::new().read(true).write(true).open(directory.path().join("agent-start.lock")).unwrap().try_lock().unwrap();
    }
}
