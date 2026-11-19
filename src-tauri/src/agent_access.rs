//! User-controlled access policy shared by the local CLI and MCP transports.
//! The GUI owns this configuration; callers cannot expand their own permissions.
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::{commands::AppState, AppSettings};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentAccessSettings {
    pub enabled: bool,
    pub allowed_directories: Vec<String>,
    pub upload_directories: Vec<String>,
    pub cloud_grants: Vec<AgentCloudGrant>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCloudGrant {
    pub provider: String,
    pub account_id: String,
    /// Local presentation only; never used to identify accounts or grant scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
}

impl AgentAccessSettings {
    /// Only a GUI-originated configuration change may grant a root. Store the
    /// physical root so replacing a selected link cannot silently change it.
    pub fn validated(&self) -> Result<Self, &'static str> {
        let directories = validated_roots(&self.allowed_directories)?;
        let upload_directories = validated_roots(&self.upload_directories)?;
        if self.cloud_grants.len() > 32 { return Err("settings.agent.error.invalidCloudGrant"); }
        let mut cloud_grants = Vec::new();
        for grant in &self.cloud_grants {
            validate_cloud_grant(grant)?;
            let mut grant = grant.clone();
            normalize_grant_labels(&mut grant);
            if !cloud_grants.contains(&grant) { cloud_grants.push(grant); }
        }
        Ok(Self { enabled: self.enabled, allowed_directories: directories, upload_directories, cloud_grants })
    }

    pub fn authorize_directory(&self, path: &Path) -> Result<PathBuf, &'static str> {
        self.authorize(path, true, &self.allowed_directories)
    }

    pub fn authorize_file(&self, path: &Path) -> Result<PathBuf, &'static str> {
        self.authorize(path, false, &self.allowed_directories)
    }

    pub fn authorize_upload_file(&self, path: &Path) -> Result<PathBuf, &'static str> {
        let resolved = self.authorize_upload_path(path)?;
        if !resolved.is_file() { return Err("INVALID_ARGUMENT"); }
        Ok(resolved)
    }

    pub fn authorize_upload_path(&self, path: &Path) -> Result<PathBuf, &'static str> {
        self.authorize(path, false, &self.upload_directories)
    }

    pub fn authorize_upload_directory(&self, path: &Path) -> Result<PathBuf, &'static str> {
        let resolved = self.authorize(path, true, &self.upload_directories)?;
        if !resolved.is_dir() { return Err("INVALID_ARGUMENT"); }
        Ok(resolved)
    }

    /// VD approves access to an account. Provider capabilities and actual
    /// resource ownership remain checked at each cloud operation.
    pub fn require_cloud_account(&self, provider: &str, account_id: &str) -> Result<(), &'static str> {
        if self.enabled && self.cloud_grants.iter().any(|grant| {
            grant.provider == provider && grant.account_id == account_id
        }) { Ok(()) } else { Err("CLOUD_PERMISSION_REQUIRED") }
    }

    fn is_restriction_of(&self, previous: &Self) -> bool {
        (!self.enabled || previous.enabled)
            && self.allowed_directories.iter().all(|root| previous.allowed_directories.contains(root))
            && self.upload_directories.iter().all(|root| previous.upload_directories.contains(root))
            && self.cloud_grants.iter().all(|grant| previous.cloud_grants.iter().any(|old| {
                old.provider == grant.provider && old.account_id == grant.account_id
            }))
    }

    fn authorize(&self, path: &Path, directory: bool, roots: &[String]) -> Result<PathBuf, &'static str> {
        if !self.enabled { return Err("FORBIDDEN"); }
        validate_components(path)?;
        let resolved = resolve_existing_ancestor(path)?;
        if directory && resolved.exists() && !resolved.is_dir() {
            return Err("INVALID_ARGUMENT");
        }
        for allowed in roots {
            let root = Path::new(allowed);
            // Re-read the root on every operation. A directory replaced by a
            // junction/symlink after authorization must not grant its new target.
            let Ok(current_root) = std::fs::canonicalize(root) else { continue; };
            let current_root = normal_path(current_root);
            if !current_root.is_dir() || !same_path(root, &current_root) { continue; }
            if within(&resolved, &current_root) {
                return Ok(resolved);
            }
        }
        Err("FORBIDDEN")
    }
}

pub(crate) fn validated_roots(roots: &[String]) -> Result<Vec<String>, &'static str> {
    if roots.len() > 16 { return Err("settings.agent.error.tooManyDirectories"); }
    let mut directories = Vec::new();
    for directory in roots {
        let path = Path::new(directory.trim());
        validate_components(path).map_err(|_| "settings.agent.error.invalidDirectory")?;
        let canonical = std::fs::canonicalize(path).map_err(|_| "settings.agent.error.invalidDirectory")?;
        if !canonical.is_dir() { return Err("settings.agent.error.invalidDirectory"); }
        let value = normal_path(canonical).to_str().ok_or("settings.agent.error.invalidDirectory")?.to_owned();
        if !directories.contains(&value) { directories.push(value); }
    }
    Ok(directories)
}

fn validate_cloud_grant(grant: &AgentCloudGrant) -> Result<(), &'static str> {
    crate::cloud::ProviderRegistry::global().by_key(&grant.provider)
        .filter(|descriptor| descriptor.provider.is_enabled()).ok_or("settings.agent.error.invalidCloudGrant")?;
    if grant.account_id.is_empty() || grant.account_id.len() > 4096
        || grant.account_id.chars().any(char::is_control) {
        return Err("settings.agent.error.invalidCloudGrant");
    }
    Ok(())
}

fn normalize_grant_labels(grant: &mut AgentCloudGrant) {
    grant.account_label = grant.account_label.as_deref().map(|value| {
        value.chars().filter(|character| !character.is_control()).take(512).collect::<String>().trim().to_owned()
    }).filter(|value| !value.is_empty());
}

pub(crate) fn require_cloud_account(state: &AppState, provider: &str, account_id: &str) -> Result<(), &'static str> {
    state.settings.lock().map_err(|_| "INTERNAL_ERROR")?.agent_access.require_cloud_account(provider, account_id)
}

/// Called only after a trusted VD interaction; this helper is not an IPC command.
pub(crate) fn approve_cloud_account(app: &AppHandle, provider: &str, account_id: &str, label: Option<String>) -> Result<(), &'static str> {
    use tauri::Manager;
    let mut grant = AgentCloudGrant { provider: provider.into(), account_id: account_id.into(), account_label: label };
    validate_cloud_grant(&grant).map_err(|_| "INVALID_ARGUMENT")?;
    normalize_grant_labels(&mut grant);
    let provider_id = crate::cloud::ProviderRegistry::global().by_key(provider).ok_or("UNSUPPORTED")?.provider;
    crate::cloud::commands::require_download_account(app, provider_id, account_id).map_err(|_| "ACCOUNT_MISMATCH")?;
    let state = app.state::<AppState>();
    let mut current = state.settings.lock().map_err(|_| "INTERNAL_ERROR")?;
    if !current.agent_access.enabled { return Err("HOST_DISABLED"); }
    let mut updated = current.clone();
    if let Some(existing) = updated.agent_access.cloud_grants.iter_mut().find(|existing| {
        existing.provider == provider && existing.account_id == account_id
    }) { *existing = grant; }
    else {
        if updated.agent_access.cloud_grants.len() >= 32 { return Err("RESOURCE_LIMIT"); }
        updated.agent_access.cloud_grants.push(grant);
    }
    persist(&state.settings_path, &updated).map_err(|_| "INTERNAL_ERROR")?;
    *current = updated.clone();
    drop(current);
    use tauri::Emitter;
    let _ = app.emit_to("main", "agent-access-changed", updated);
    Ok(())
}

pub(crate) fn validate_components(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute() { return Err("INVALID_ARGUMENT"); }
    for component in path.components() {
        if matches!(component, Component::ParentDir) { return Err("FORBIDDEN"); }
        if let Component::Normal(name) = component {
            let name = name.to_str().ok_or("INVALID_ARGUMENT")?;
            // Win32 device names, alternate streams and trimmed path aliases
            // do not identify ordinary files inside the selected directory.
            #[cfg(windows)]
            {
                let base = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
                let device = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
                    || base.strip_prefix("COM").or_else(|| base.strip_prefix("LPT"))
                        .is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"));
                if name.contains(':') || name.ends_with(['.', ' ']) || device {
                    return Err("FORBIDDEN");
                }
            }
            if name.contains('\0') { return Err("INVALID_ARGUMENT"); }
        }
    }
    Ok(())
}

pub(crate) fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf, &'static str> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(ancestor).map_err(|_| "FORBIDDEN")?;
                if !suffix.is_empty() && !resolved.is_dir() { return Err("INVALID_ARGUMENT"); }
                for name in suffix.iter().rev() { resolved.push(name); }
                return Ok(normal_path(resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(ancestor.file_name().ok_or("INVALID_ARGUMENT")?.to_os_string());
                ancestor = ancestor.parent().ok_or("INVALID_ARGUMENT")?;
            }
            Err(_) => return Err("FORBIDDEN"),
        }
    }
}

pub(crate) fn normal_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") { return PathBuf::from(format!(r"\\{rest}")); }
        if let Some(rest) = text.strip_prefix(r"\\?\") { return PathBuf::from(rest); }
    }
    path
}

fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
    #[cfg(windows)]
    { a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()) }
    #[cfg(not(windows))]
    { a == b }
}

fn within(path: &Path, root: &Path) -> bool {
    let mut candidate = path.components();
    root.components().all(|part| candidate.next().is_some_and(|next| same_component(part, next)))
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    a.components().count() == b.components().count() && within(a, b)
}

pub(crate) fn is_enabled(state: &AppState) -> bool {
    state.settings.lock().is_ok_and(|settings| settings.agent_access.enabled)
}

pub(crate) fn authorize_directory(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    crate::agent_permissions::authorize_download_directory(state, path)
}

pub(crate) fn authorize_file(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    crate::agent_permissions::authorize_download_file(state, path)
}

pub(crate) fn authorize_upload_path(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    crate::agent_permissions::authorize_upload_path(state, path)
}

pub(crate) fn authorize_upload_file(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    crate::agent_permissions::authorize_upload_file(state, path)
}

pub(crate) fn authorize_upload_directory(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    crate::agent_permissions::authorize_upload_directory(state, path)
}

fn persist(path: &Path, settings: &AppSettings) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(settings).map_err(|_| "settings.agent.error.save".to_string())?;
    std::fs::write(path, bytes).map_err(|_| "settings.agent.error.save".to_string())
}

fn refresh_or_disable(
    settings: &std::sync::Mutex<AppSettings>,
    settings_path: &Path,
    mut refresh: impl FnMut() -> std::io::Result<()>,
) -> Result<(), String> {
    if refresh().is_ok() { return Ok(()); }
    {
        // Never restore the previous grant: it may contain a directory the
        // user just revoked. Keep the latest directory selection and revoke
        // every Agent entry point even when persistence also fails.
        let mut current = settings.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        current.agent_access.enabled = false;
        let _ = persist(settings_path, &current);
    }
    // Invalidate any endpoint/credential left by a partial refresh. Its
    // requests must also see the disabled in-memory policy if cleanup fails.
    let _ = refresh();
    Err("settings.agent.error.host".into())
}

#[tauri::command]
pub async fn configure_agent_access(
    mut settings: AgentAccessSettings,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    // Restricting access must work offline, even after a directory disappears
    // or a cloud account expires. New or re-enabled grants are verified first.
    let previous = state.settings.lock().map_err(|_| "settings.agent.error.save".to_string())?
        .agent_access.clone();
    if settings.cloud_grants.len() > 32 { return Err("settings.agent.error.invalidCloudGrant".into()); }
    for grant in &mut settings.cloud_grants {
        validate_cloud_grant(grant).map_err(str::to_owned)?;
        normalize_grant_labels(grant);
    }
    let settings = if settings.is_restriction_of(&previous) {
        settings
    } else {
        let settings = settings.validated().map_err(str::to_owned)?;
        for grant in &settings.cloud_grants {
            let only_this_grant = AgentAccessSettings {
                enabled: settings.enabled, cloud_grants: vec![grant.clone()], ..Default::default()
            };
            if only_this_grant.is_restriction_of(&previous) { continue; }
            let provider = crate::cloud::ProviderRegistry::global().by_key(&grant.provider)
                .ok_or("settings.agent.error.invalidCloudGrant")?.provider;
            let account = crate::cloud::commands::cloud_account(app.clone(), provider).await
                .map_err(|_| "settings.agent.error.cloudVerification".to_string())?;
            if account.id != grant.account_id { return Err("settings.agent.error.cloudVerification".into()); }
            crate::cloud::commands::require_download_account(&app, provider, &grant.account_id)
                .map_err(|_| "settings.agent.error.cloudVerification".to_string())?;
        }
        settings
    };
    {
        let mut current = state.settings.lock().map_err(|_| "settings.agent.error.save".to_string())?;
        // An asynchronous provider check must not restore a concurrent revocation.
        if current.agent_access != previous { return Err("settings.agent.error.changed".into()); }
        let mut updated = current.clone();
        updated.agent_access = settings;
        persist(&state.settings_path, &updated)?;
        *current = updated;
    }
    refresh_or_disable(&state.settings, &state.settings_path, || crate::cli_host::refresh(&app))?;
    state.settings.lock().map(|settings| settings.clone()).map_err(|_| "settings.agent.error.save".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(root: &Path) -> AgentAccessSettings {
        AgentAccessSettings { enabled: true, allowed_directories: vec![root.to_string_lossy().into_owned()], ..Default::default() }.validated().unwrap()
    }

    fn grant() -> AgentCloudGrant {
        AgentCloudGrant { provider: "quark".into(), account_id: "account-a".into(),
            ..Default::default() }
    }

    #[test]
    fn agent_upload_sources_require_separate_existing_file_permission() {
        let downloads = tempfile::tempdir().unwrap();
        let uploads = tempfile::tempdir().unwrap();
        let downloaded = downloads.path().join("download.txt");
        let source = uploads.path().join("source.txt");
        std::fs::write(&downloaded, b"download").unwrap();
        std::fs::write(&source, b"upload").unwrap();
        let mut access = policy(downloads.path());
        assert_eq!(access.authorize_upload_file(&downloaded), Err("FORBIDDEN"));
        access.upload_directories = vec![uploads.path().to_string_lossy().into_owned()];
        let access = access.validated().unwrap();
        assert!(access.authorize_upload_file(&source).is_ok());
        assert!(access.authorize_upload_directory(uploads.path()).is_ok());
        assert_eq!(access.authorize_file(&source), Err("FORBIDDEN"));
        assert_eq!(access.authorize_upload_file(&uploads.path().join("missing.txt")), Err("INVALID_ARGUMENT"));
        assert_eq!(access.authorize_upload_file(uploads.path()), Err("INVALID_ARGUMENT"));
        assert_eq!(access.authorize_upload_directory(&source), Err("INVALID_ARGUMENT"));
        let revoked = AgentAccessSettings { upload_directories: Vec::new(), ..access };
        assert_eq!(revoked.authorize_upload_file(&source), Err("FORBIDDEN"));
    }

    #[test]
    fn agent_cloud_access_binds_provider_and_account_without_directory_prompts() {
        let policy = AgentAccessSettings { enabled: true, cloud_grants: vec![grant()], ..Default::default() };
        assert!(policy.require_cloud_account("quark", "account-a").is_ok());
        assert_eq!(policy.require_cloud_account("quark", "account-b"), Err("CLOUD_PERMISSION_REQUIRED"));
        assert_eq!(policy.require_cloud_account("baidu", "account-a"), Err("CLOUD_PERMISSION_REQUIRED"));
        assert_eq!(AgentAccessSettings { enabled: false, ..policy }.require_cloud_account("quark", "account-a"), Err("CLOUD_PERMISSION_REQUIRED"));
    }

    #[test]
    fn agent_cloud_account_grants_reject_invalid_provider_and_identity() {
        let mut invalid = grant();
        invalid.provider = "unknown".into();
        assert!(validate_cloud_grant(&invalid).is_err());
        invalid = grant(); invalid.account_id.clear();
        assert!(validate_cloud_grant(&invalid).is_err());
        invalid = grant(); invalid.account_id = "bad\nidentity".into();
        assert!(validate_cloud_grant(&invalid).is_err());
    }

    #[test]
    fn agent_cloud_display_labels_never_replace_account_identity() {
        let mut grant = grant();
        grant.account_label = Some(" Display\nAccount ".into());
        normalize_grant_labels(&mut grant);
        assert_eq!(grant.account_label.as_deref(), Some("DisplayAccount"));
        let policy = AgentAccessSettings { enabled: true, cloud_grants: vec![grant], ..Default::default() };
        assert_eq!(policy.require_cloud_account("quark", "DisplayAccount"), Err("CLOUD_PERMISSION_REQUIRED"));
    }

    #[test]
    fn agent_permission_restrictions_need_no_filesystem_or_provider_validation() {
        let previous = AgentAccessSettings { enabled: true,
            allowed_directories: vec!["no-longer-mounted".into()],
            upload_directories: vec!["offline-upload-root".into()], cloud_grants: vec![grant()] };
        let mut restricted = previous.clone();
        restricted.cloud_grants.clear();
        restricted.upload_directories.clear();
        assert!(restricted.is_restriction_of(&previous));
        assert!(AgentAccessSettings::default().is_restriction_of(&previous));
        let mut other = grant(); other.account_id = "account-b".into();
        restricted.cloud_grants.push(other);
        assert!(!restricted.is_restriction_of(&previous));
        let disabled = AgentAccessSettings { enabled: false, ..previous.clone() };
        assert!(disabled.is_restriction_of(&previous));
        assert!(!previous.is_restriction_of(&disabled));
    }

    #[test]
    fn agent_access_refresh_failure_disables_and_preserves_latest_directory_selection() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("revoked");
        let new_root = dir.path().join("selected");
        std::fs::create_dir(&old_root).unwrap();
        std::fs::create_dir(&new_root).unwrap();
        let old_policy = policy(&old_root);
        assert!(old_policy.authorize_directory(&old_root).is_ok());
        let settings = std::sync::Mutex::new(AppSettings {
            agent_access: policy(&new_root), ..Default::default()
        });
        let latest_roots = settings.lock().unwrap().agent_access.allowed_directories.clone();
        let settings_path = dir.path().join("settings.json");
        persist(&settings_path, &settings.lock().unwrap()).unwrap();
        let mut attempts = 0;
        let error = refresh_or_disable(&settings, &settings_path, || {
            attempts += 1;
            if attempts == 2 { assert!(!settings.lock().unwrap().agent_access.enabled); }
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fixture refresh failure"))
        }).unwrap_err();
        assert_eq!(error, "settings.agent.error.host");
        assert_eq!(attempts, 2, "failed refresh must attempt endpoint invalidation");
        let current = settings.lock().unwrap();
        assert!(!current.agent_access.enabled);
        assert_eq!(current.agent_access.allowed_directories, latest_roots);
        assert_eq!(current.agent_access.authorize_directory(&old_root), Err("FORBIDDEN"));
        assert_eq!(current.agent_access.authorize_directory(&new_root), Err("FORBIDDEN"));
        let disk: AppSettings = serde_json::from_slice(&std::fs::read(settings_path).unwrap()).unwrap();
        assert_eq!(disk.agent_access, current.agent_access);
    }

    #[test]
    fn agent_access_refresh_failure_stays_disabled_when_persistence_fails() {
        let dir = tempfile::tempdir().unwrap();
        let settings = std::sync::Mutex::new(AppSettings {
            agent_access: policy(dir.path()), ..Default::default()
        });
        let roots = settings.lock().unwrap().agent_access.allowed_directories.clone();
        let mut attempts = 0;
        // A directory cannot be overwritten with serialized settings.
        assert!(refresh_or_disable(&settings, dir.path(), || {
            attempts += 1;
            Err(std::io::Error::other("fixture refresh failure"))
        }).is_err());
        let current = settings.lock().unwrap();
        assert!(!current.agent_access.enabled);
        assert_eq!(current.agent_access.allowed_directories, roots);
        assert_eq!(current.agent_access.authorize_file(&dir.path().join("file.bin")), Err("FORBIDDEN"));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn agent_access_defaults_closed_and_requires_an_existing_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(AgentAccessSettings::default().authorize_directory(dir.path()), Err("FORBIDDEN"));
        let query_only = AgentAccessSettings { enabled: true, ..Default::default() }.validated().unwrap();
        assert_eq!(query_only.authorize_directory(dir.path()), Err("FORBIDDEN"));
        assert!(AgentAccessSettings { enabled: true, allowed_directories: vec![dir.path().join("missing").to_string_lossy().into_owned()], ..Default::default() }.validated().is_err());
    }

    #[test]
    fn agent_access_allows_nested_new_paths_but_not_prefix_siblings_or_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("downloads");
        let sibling = dir.path().join("downloads-secret");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let access = policy(&root);
        assert!(access.authorize_directory(&root.join("中文 folder").join("new")).is_ok());
        assert!(access.authorize_file(&root.join("missing.zip")).is_ok());
        assert_eq!(access.authorize_directory(&sibling), Err("FORBIDDEN"));
        assert_eq!(access.authorize_file(&root.join("..").join("outside.zip")), Err("FORBIDDEN"));
        assert_eq!(access.authorize_file(Path::new("relative.zip")), Err("INVALID_ARGUMENT"));
    }

    #[test]
    fn agent_access_result_checks_existing_files_and_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("completed.bin");
        std::fs::write(&file, b"done").unwrap();
        let mut access = policy(dir.path());
        assert!(access.authorize_file(&file).is_ok());
        assert_eq!(access.authorize_directory(&file), Err("INVALID_ARGUMENT"));
        access.enabled = false;
        assert_eq!(access.authorize_file(&file), Err("FORBIDDEN"));
    }

    #[cfg(unix)]
    #[test]
    fn agent_access_rejects_symlinks_escaping_root_and_replaced_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let access = policy(&root);
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        assert_eq!(access.authorize_file(&root.join("escape/new.bin")), Err("FORBIDDEN"));
        std::fs::remove_file(root.join("escape")).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::os::unix::fs::symlink(&outside, &root).unwrap();
        assert_eq!(access.authorize_directory(&root), Err("FORBIDDEN"));
    }

    #[cfg(windows)]
    #[test]
    fn agent_access_rejects_windows_streams_devices_and_junction_escape() {
        use std::os::windows::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let access = policy(&root);
        for name in ["file.bin:secret", "NUL", "COM1.txt", "LPT²", "CONOUT$", "NUL .txt", "file. "] {
            assert_eq!(access.authorize_file(&root.join(name)), Err("FORBIDDEN"));
        }
        let junction = root.join("escape");
        let status = std::process::Command::new("cmd.exe").args(["/d", "/c", "mklink", "/J"])
            .arg(&junction).arg(&outside).creation_flags(0x08000000).output().unwrap();
        assert!(status.status.success(), "junction fixture creation failed");
        assert_eq!(access.authorize_file(&junction.join("new.bin")), Err("FORBIDDEN"));
        // Remove only the junction itself, never recursively its destination.
        std::fs::remove_dir(&junction).unwrap();
        std::fs::remove_dir(&root).unwrap();
        let status = std::process::Command::new("cmd.exe").args(["/d", "/c", "mklink", "/J"])
            .arg(&root).arg(&outside).creation_flags(0x08000000).output().unwrap();
        assert!(status.status.success(), "replacement root fixture creation failed");
        assert_eq!(access.authorize_directory(&root), Err("FORBIDDEN"));
        std::fs::remove_dir(&root).unwrap();
    }
}
