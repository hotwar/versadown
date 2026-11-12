//! Authenticated loopback transport. Only the GUI owns services and storage.
use axum::{extract::{DefaultBodyLimit, State}, http::{HeaderMap, StatusCode}, routing::post, Json, Router};
use crate::cli_protocol::{Discovery, Request, Response, PROTOCOL_VERSION};
use std::{path::{Path, PathBuf}, sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}}, time::{Instant, Duration}};
use tauri::Manager;

struct HostController {
    data_dir: PathBuf,
    instance_id: String,
    active: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    generation: AtomicU64,
}

pub(crate) fn instance_id(app: &tauri::AppHandle) -> String {
    app.state::<HostController>().instance_id.clone()
}

struct Host {
    app: tauri::AppHandle,
    token: String,
    authority: String,
    generation: u64,
    // Query calls must not wait behind a slow engine or a different task action.
    slots: Arc<tokio::sync::Semaphore>,
    rate: Mutex<(Instant, u32)>,
}

fn authorized(headers: &HeaderMap, token: &str, authority: &str) -> bool {
    !headers.contains_key("origin")
        && !headers.contains_key("sec-fetch-site")
        && headers.get("host").and_then(|v| v.to_str().ok()) == Some(authority)
        && headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(format!("Bearer {token}").as_str())
        && headers.get("content-type").and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("application/json"))
}

fn audit_request_id(value: Option<&str>) -> Option<&str> {
    value.filter(|id| !id.is_empty() && id.len() <= 128
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte)))
}

fn audit_task_id(value: Option<&str>) -> Option<String> {
    // Synthetic display-group IDs can contain user paths. Only persist the
    // normalized UUID of real tasks in diagnostics, including rejected calls.
    value.and_then(|id| uuid::Uuid::parse_str(id).ok()).map(|id| id.to_string())
}

async fn call(State(host): State<Arc<Host>>, headers: HeaderMap, body: axum::body::Bytes) -> (StatusCode, Json<Response>) {
    if host.generation != host.app.state::<HostController>().generation.load(Ordering::SeqCst)
        || !authorized(&headers, &host.token, &host.authority) {
        return (StatusCode::UNAUTHORIZED, Json(Response::failure("UNAUTHORIZED", "en")));
    }
    let request: Request = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(Response::failure("INVALID_ARGUMENT", "en"))),
    };
    if !crate::agent_access::is_enabled(&host.app.state::<crate::commands::AppState>()) {
        return (StatusCode::FORBIDDEN, Json(Response::failure("HOST_DISABLED", "en")));
    }
    {
        let mut rate = host.rate.lock().unwrap_or_else(|e| e.into_inner());
        if rate.0.elapsed() >= Duration::from_secs(1) { *rate = (Instant::now(), 0); }
        if rate.1 >= 64 { return (StatusCode::TOO_MANY_REQUESTS, Json(Response::failure("BUSY", "en"))); }
        rate.1 += 1;
    }
    let Ok(permit) = host.slots.clone().try_acquire_owned() else {
        return (StatusCode::TOO_MANY_REQUESTS, Json(Response::failure("BUSY", "en")));
    };
    // A spawned operation survives HTTP disconnects. Idempotency is enforced
    // by the task service, not the lifetime of a transport response.
    let result = tauri::async_runtime::spawn(async move {
        let _permit = permit;
        let started = Instant::now();
        let command = match request.command.as_str() {
            "health" | "add" | "list" | "status" | "result" | "pause" | "resume" | "cancel" => request.command.clone(),
            _ => "invalid".into(),
        };
        let request_id = audit_request_id(request.request_id.as_deref()).map(str::to_owned);
        let task_id = audit_task_id(request.task_id.as_deref());
        let result = if host.generation == host.app.state::<HostController>().generation.load(Ordering::SeqCst) {
            crate::cli_service::execute(host.app.clone(), request).await
        } else { Response::failure("UNAUTHORIZED", "en") };
        let task_id = task_id.or_else(|| if result.ok && command == "add" {
            audit_task_id(result.data.as_ref().and_then(|data| data.get("id")).and_then(|id| id.as_str()))
        } else { None });
        eprintln!("{}", serde_json::json!({"event":"agent_call", "command":command,
            "request_id":request_id, "task_id":task_id, "ok":result.ok,
            "code":result.error.as_ref().map(|e| &e.code), "elapsed_ms":started.elapsed().as_millis()}));
        result
    }).await.unwrap_or_else(|_| Response::failure("INTERNAL_ERROR", "en"));
    (StatusCode::OK, Json(result))
}

#[cfg(windows)]
fn process_owner_sid() -> std::io::Result<String> {
    use std::os::windows::process::CommandExt;
    let identity = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"]).creation_flags(0x08000000).output()?;
    let output = String::from_utf8_lossy(&identity.stdout);
    let sid = output.trim().rsplit(',').next().unwrap_or("").trim_matches('"');
    if !identity.status.success() || !sid.starts_with("S-1-") || !sid.chars().all(|c| c.is_ascii_digit() || c == 'S' || c == '-') {
        return Err(std::io::Error::other("Unable to identify CLI owner"));
    }
    Ok(sid.to_owned())
}

#[cfg(windows)]
fn replace_directory_dacl(path: &Path, sddl: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR, Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::{DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, GetSecurityDescriptorDacl,
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SE_FILE_OBJECT, SDDL_REVISION_1}},
    }};
    let descriptor_text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // The conversion allocates a descriptor with LocalAlloc. The DACL pointer
    // borrows that allocation and remains valid until SetNamedSecurityInfoW
    // has copied it. Always free the descriptor, including on failure.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(descriptor_text.as_ptr()), SDDL_REVISION_1, &mut descriptor, None)
            .map_err(std::io::Error::other)?;
        let result = (|| {
            let mut present = windows::core::BOOL::default();
            let mut defaulted = windows::core::BOOL::default();
            let mut dacl = std::ptr::null_mut();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
                .map_err(std::io::Error::other)?;
            if !present.as_bool() || dacl.is_null() {
                return Err(std::io::Error::other("CLI discovery requires a restrictive DACL"));
            }
            SetNamedSecurityInfoW(PCWSTR(path.as_ptr()), SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None, None, Some(dacl), None).ok().map_err(std::io::Error::other)
        })();
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}

fn private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("CLI directory must not be a symlink"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        // Replace the entire protected DACL: removing inheritance alone leaves
        // explicit grants to other users intact on an existing directory.
        let sid = process_owner_sid()?;
        replace_directory_dacl(path, &format!("D:P(A;OICI;FA;;;{sid})"))?;
    }
    Ok(())
}

pub(crate) fn start(app: &tauri::AppHandle, data_dir: &Path) -> std::io::Result<()> {
    app.manage(HostController { data_dir: data_dir.to_path_buf(),
        instance_id: uuid::Uuid::new_v4().to_string(), active: Mutex::new(None), generation: AtomicU64::new(0) });
    refresh(app)
}

/// Called only by desktop settings. Rotate credentials on any permission change
/// and stop publishing an endpoint immediately when access is disabled.
pub(crate) fn refresh(app: &tauri::AppHandle) -> std::io::Result<()> {
    let controller = app.state::<HostController>();
    let mut active = controller.active.lock().map_err(|_| std::io::Error::other("host lock"))?;
    let generation = controller.generation.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(stop) = active.take() { let _ = stop.send(()); }
    let directory = controller.data_dir.join("cli");
    let discovery_path = directory.join("connection.json");
    match std::fs::remove_file(&discovery_path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    if !crate::agent_access::is_enabled(&app.state::<crate::commands::AppState>()) { return Ok(()); }
    private_directory(&directory)?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let discovery = Discovery { version: PROTOCOL_VERSION, port, token: token.clone() };
    let host = Arc::new(Host { app: app.clone(), token, authority: format!("127.0.0.1:{port}"), generation,
        slots: Arc::new(tokio::sync::Semaphore::new(16)), rate: Mutex::new((Instant::now(), 0)) });
    // Create a fresh file rather than following a preexisting link.
    let temporary = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(&discovery)?)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, discovery_path)?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    *active = Some(stop);
    tauri::async_runtime::spawn(async move {
        if let Ok(listener) = tokio::net::TcpListener::from_std(listener) {
            let router = Router::new().route("/v1/call", post(call))
                .layer(DefaultBodyLimit::max(16 * 1024)).with_state(host);
            let _ = axum::serve(listener, router).with_graceful_shutdown(async { let _ = stopped.await; }).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_audit_rejects_untrusted_ids_and_paths() {
        assert_eq!(audit_request_id(Some("job-1.test:retry_2")), Some("job-1.test:retry_2"));
        for invalid in ["", "https://example.test/?token=secret", "id\nsecret", &"x".repeat(129)] {
            assert_eq!(audit_request_id(Some(invalid)), None);
        }
        let id = uuid::Uuid::new_v4().to_string();
        assert_eq!(audit_task_id(Some(&id)), Some(id));
        for invalid in ["group:secret/path", "https://example.test/?token=secret", &"x".repeat(16000)] {
            assert_eq!(audit_task_id(Some(invalid)), None);
        }
    }
    #[test]
    fn cli_transport_rejects_browser_and_wrong_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:1234".parse().unwrap());
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        assert!(authorized(&headers, "secret", "127.0.0.1:1234"));
        assert!(!authorized(&headers, "wrong", "127.0.0.1:1234"));
        assert!(!authorized(&headers, "secret", "evil.example:1234"));
        for name in ["origin", "sec-fetch-site"] {
            headers.insert(name, "null".parse().unwrap());
            assert!(!authorized(&headers, "secret", "127.0.0.1:1234"));
            headers.remove(name);
        }
    }

    #[cfg(windows)]
    #[test]
    fn cli_discovery_acl_removes_explicit_everyone_access() {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::{PCWSTR, PWSTR}, Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
                Authorization::{GetNamedSecurityInfoW, ConvertSecurityDescriptorToStringSecurityDescriptorW, SE_FILE_OBJECT, SDDL_REVISION_1}},
        }};
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("cli");
        std::fs::create_dir(&directory).unwrap();
        let sid = process_owner_sid().unwrap();
        replace_directory_dacl(&directory, &format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FR;;;WD)")).unwrap();
        private_directory(&directory).unwrap();
        // Verify both the directory and a newly published discovery file: the
        // one owner ACE must inherit, without reviving the explicit Everyone ACE.
        let file = directory.join("connection.json");
        std::fs::write(&file, "{}").unwrap();
        for path in [&directory, &file] {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            let mut dacl = std::ptr::null_mut();
            unsafe {
                GetNamedSecurityInfoW(PCWSTR(wide.as_ptr()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                    None, None, Some(&mut dacl), None, &mut descriptor).ok().unwrap();
                assert!(!dacl.is_null());
                let count = (*dacl).AceCount;
                let mut text = PWSTR::null();
                ConvertSecurityDescriptorToStringSecurityDescriptorW(descriptor, SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION, &mut text, None).unwrap();
                let text_value = text.to_string().unwrap();
                let _ = LocalFree(Some(HLOCAL(text.0.cast())));
                let _ = LocalFree(Some(HLOCAL(descriptor.0)));
                assert_eq!(count, 1, "unexpected ACL: {text_value}");
                assert!(text_value.contains(&format!(";;;{sid})")), "wrong owner: {text_value}");
                assert!(!text_value.contains(";;;WD)"));
            }
        }
    }
}
