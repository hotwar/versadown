//! Explicit, visible development/test runs with their own writable files.
//! This is independent of the hidden background-UI test mode.

use std::{ffi::OsStr, io, path::{Path, PathBuf}};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, Runtime, WebviewWindowBuilder};

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceRuntime {
    pub(crate) root: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PortableMarker {
    schema_version: u32,
    run_root_relative: PathBuf,
}

fn resolve_root(explicit: Option<&OsStr>, resources: &Path) -> io::Result<Option<PathBuf>> {
    if let Some(explicit) = explicit {
        let root = PathBuf::from(explicit);
        if !root.is_absolute() {
            return Err(io::Error::other("VERSADOWN_RUN_ROOT must be an absolute path"));
        }
        return Ok(Some(root));
    }
    let marker = resources.join("workspace-runtime.json");
    let contents = match std::fs::read(&marker) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let marker: PortableMarker = serde_json::from_slice(&contents).map_err(io::Error::other)?;
    if marker.schema_version != 1 || marker.run_root_relative.as_os_str().is_empty()
        || marker.run_root_relative.is_absolute()
    {
        return Err(io::Error::other("Invalid workspace-runtime.json"));
    }
    Ok(Some(resources.join(marker.run_root_relative)))
}

impl WorkspaceRuntime {
    pub(crate) fn initialize(resources: &Path) -> io::Result<Self> {
        let explicit = std::env::var_os("VERSADOWN_RUN_ROOT");
        let Some(root) = resolve_root(explicit.as_deref(), resources)? else {
            return Ok(Self::default());
        };
        std::fs::create_dir_all(&root)?;
        let root = std::fs::canonicalize(root)?;
        for child in ["data", "downloads", "temp", "logs"] {
            std::fs::create_dir_all(root.join(child))?;
        }
        // Called before creating the Tauri runtime/worker threads. Child tools
        // inherit the same temporary directory without modifying the host env.
        for name in ["TMPDIR", "TMP", "TEMP"] {
            std::env::set_var(name, root.join("temp"));
        }
        std::fs::write(root.join("runtime-paths.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "schemaVersion": 1,
            "root": root,
            "data": root.join("data"),
            "downloads": root.join("downloads"),
            "temp": root.join("temp"),
            "webviewStorage": if cfg!(target_os = "macos") { "system-managed; isolated data-store identifiers on macOS 14+" } else { "data/webviews and data/web-auth" },
            "credentials": "OS credential storage may retain keys outside this directory"
        })).map_err(io::Error::other)?)?;
        Ok(Self { root: Some(root) })
    }
}

pub(crate) fn root<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    app.try_state::<WorkspaceRuntime>().and_then(|runtime| runtime.root.clone())
}

fn link_association_allowed_for(identifier: &str, workspace: bool) -> bool {
    !workspace
        && !["com.versadown.workspace", "com.versadown.versiontest"]
            .iter()
            .any(|prefix| identifier == *prefix || identifier.starts_with(&format!("{prefix}.")))
}

pub(crate) fn allows_link_association<R: Runtime>(app: &AppHandle<R>) -> bool {
    // Exported development installers deliberately omit the portable marker,
    // but must still never take ownership of system download links.
    link_association_allowed_for(&app.config().identifier, root(app).is_some())
}

pub(crate) fn data_dir<R: Runtime>(app: &AppHandle<R>) -> io::Result<PathBuf> {
    if let Some(root) = root(app) {
        return Ok(root.join("data"));
    }
    if std::env::var("VERSADOWN_BACKGROUND_UI_TEST").as_deref() == Ok("1") {
        return std::env::var_os("VERSADOWN_TEST_DATA_DIR").map(PathBuf::from)
            .ok_or_else(|| io::Error::other("Background UI tests require VERSADOWN_TEST_DATA_DIR"));
    }
    app.path().app_data_dir().map_err(io::Error::other)
}

fn profile_identifier(path: &Path) -> [u8; 16] {
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

pub(crate) trait WorkspaceWebviewProfile<R: Runtime> {
    fn workspace_data_directory(self, app: &AppHandle<R>, directory: PathBuf) -> Self;
}

impl<R: Runtime, M: Manager<R>> WorkspaceWebviewProfile<R> for WebviewWindowBuilder<'_, R, M> {
    fn workspace_data_directory(self, app: &AppHandle<R>, directory: PathBuf) -> Self {
        #[cfg(target_os = "macos")]
        if root(app).is_some() {
            // WKWebView ignores filesystem data_directory. Its named data
            // stores are supported on macOS 14+, and still live in Library.
            return self.data_store_identifier(profile_identifier(&directory));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = app;
        self.data_directory(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_takes_precedence_and_requires_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("workspace-runtime.json"), "invalid JSON").unwrap();
        assert_eq!(resolve_root(Some(dir.path().as_os_str()), dir.path()).unwrap(), Some(dir.path().to_owned()));
        assert!(resolve_root(Some(OsStr::new("relative")), dir.path()).is_err());
    }

    #[test]
    fn absent_marker_preserves_normal_mode_and_portable_marker_tracks_its_location() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_root(None, dir.path()).unwrap(), None);
        let resources = dir.path().join("app/VersaDown.app/Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("workspace-runtime.json"), r#"{"schemaVersion":1,"runRootRelative":"../../../../"}"#).unwrap();
        assert_eq!(std::fs::canonicalize(resolve_root(None, &resources).unwrap().unwrap()).unwrap(), std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn malformed_marker_does_not_silently_use_production_data() {
        let dir = tempfile::tempdir().unwrap();
        for contents in [r#"{"schemaVersion":2,"runRootRelative":".."}"#, r#"{"schemaVersion":1,"runRootRelative":"/outside"}"#, "invalid"] {
            std::fs::write(dir.path().join("workspace-runtime.json"), contents).unwrap();
            assert!(resolve_root(None, dir.path()).is_err());
        }
    }

    #[test]
    fn webview_profile_ids_are_stable_and_separate_runs_and_providers() {
        assert_eq!(profile_identifier(Path::new("/run-a/data/web-auth/baidu")), profile_identifier(Path::new("/run-a/data/web-auth/baidu")));
        assert_ne!(profile_identifier(Path::new("/run-a/data/web-auth/baidu")), profile_identifier(Path::new("/run-b/data/web-auth/baidu")));
        assert_ne!(profile_identifier(Path::new("/run-a/data/web-auth/baidu")), profile_identifier(Path::new("/run-a/data/web-auth/xunlei")));
    }

    #[test]
    fn development_installers_cannot_register_links_without_a_portable_marker() {
        assert!(!link_association_allowed_for("com.versadown.workspace.s123.t456", false));
        assert!(!link_association_allowed_for("com.versadown.workspace.live.worktree", false));
        assert!(!link_association_allowed_for("com.versadown.versiontest.fork.s123", false));
        assert!(!link_association_allowed_for("com.versadown.desktop", true));
        assert!(link_association_allowed_for("com.versadown.desktop", false));
    }
}
