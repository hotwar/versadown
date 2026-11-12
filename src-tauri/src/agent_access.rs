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
}

impl AgentAccessSettings {
    /// Only a GUI-originated configuration change may grant a root. Store the
    /// physical root so replacing a selected link cannot silently change it.
    pub fn validated(&self) -> Result<Self, &'static str> {
        if self.allowed_directories.len() > 16 {
            return Err("settings.agent.error.tooManyDirectories");
        }
        let mut directories = Vec::new();
        for directory in &self.allowed_directories {
            let path = Path::new(directory.trim());
            validate_components(path).map_err(|_| "settings.agent.error.invalidDirectory")?;
            let canonical = std::fs::canonicalize(path)
                .map_err(|_| "settings.agent.error.invalidDirectory")?;
            if !canonical.is_dir() {
                return Err("settings.agent.error.invalidDirectory");
            }
            let canonical = normal_path(canonical);
            let value = canonical.to_str().ok_or("settings.agent.error.invalidDirectory")?.to_owned();
            if !directories.contains(&value) { directories.push(value); }
        }
        if self.enabled && directories.is_empty() {
            return Err("settings.agent.error.directoryRequired");
        }
        Ok(Self { enabled: self.enabled, allowed_directories: directories })
    }

    pub fn authorize_directory(&self, path: &Path) -> Result<PathBuf, &'static str> {
        self.authorize(path, true)
    }

    pub fn authorize_file(&self, path: &Path) -> Result<PathBuf, &'static str> {
        self.authorize(path, false)
    }

    fn authorize(&self, path: &Path, directory: bool) -> Result<PathBuf, &'static str> {
        if !self.enabled { return Err("FORBIDDEN"); }
        validate_components(path)?;
        let resolved = resolve_existing_ancestor(path)?;
        if directory && resolved.exists() && !resolved.is_dir() {
            return Err("INVALID_ARGUMENT");
        }
        for allowed in &self.allowed_directories {
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

fn validate_components(path: &Path) -> Result<(), &'static str> {
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

fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf, &'static str> {
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

fn normal_path(path: PathBuf) -> PathBuf {
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

fn same_path(a: &Path, b: &Path) -> bool {
    a.components().count() == b.components().count() && within(a, b)
}

pub(crate) fn is_enabled(state: &AppState) -> bool {
    state.settings.lock().is_ok_and(|settings| settings.agent_access.enabled)
}

pub(crate) fn authorize_directory(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    state.settings.lock().map_err(|_| "INTERNAL_ERROR")?.agent_access.authorize_directory(path)
}

pub(crate) fn authorize_file(state: &AppState, path: &Path) -> Result<PathBuf, &'static str> {
    state.settings.lock().map_err(|_| "INTERNAL_ERROR")?.agent_access.authorize_file(path)
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
pub fn configure_agent_access(
    settings: AgentAccessSettings,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    // Disabling must work even if a formerly authorized directory is gone.
    // New roots can still be saved while disabled, but never grant access.
    let previous_roots = state.settings.lock().map_err(|_| "settings.agent.error.save".to_string())?
        .agent_access.allowed_directories.clone();
    let settings = if !settings.enabled && settings.allowed_directories == previous_roots {
        settings
    } else {
        settings.validated().map_err(str::to_owned)?
    };
    {
        let mut current = state.settings.lock().map_err(|_| "settings.agent.error.save".to_string())?;
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
        AgentAccessSettings { enabled: true, allowed_directories: vec![root.to_string_lossy().into_owned()] }.validated().unwrap()
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
        assert!(AgentAccessSettings { enabled: true, ..Default::default() }.validated().is_err());
        assert!(AgentAccessSettings { enabled: true, allowed_directories: vec![dir.path().join("missing").to_string_lossy().into_owned()] }.validated().is_err());
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
