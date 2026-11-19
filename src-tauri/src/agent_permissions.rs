//! Task-local filesystem grants. Approval is owned by VD, never by a tool argument.
use std::{future::Future, path::{Path, PathBuf}};
use serde::{Deserialize, Serialize};
use crate::{agent_access::{self, AgentAccessSettings}, commands::AppState};

type CodeResult<T> = Result<T, &'static str>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalAccessGrant {
    pub download_directories: Vec<String>,
    pub upload_files: Vec<String>,
    pub upload_directories: Vec<String>,
}

#[derive(Clone, Copy)]
enum Access { DownloadDirectory, DownloadFile, UploadPath, UploadFile, UploadDirectory }

impl LocalAccessGrant {
    /// Called when VD commits a user decision, not when replaying a receipt.
    pub(crate) fn validated(&self) -> CodeResult<Self> {
        if self.upload_files.len() > 100 { return Err("RESOURCE_LIMIT"); }
        let mut download_directories = agent_access::validated_roots(&self.download_directories)
            .map_err(|_| "INVALID_ARGUMENT")?;
        let mut upload_directories = agent_access::validated_roots(&self.upload_directories)
            .map_err(|_| "INVALID_ARGUMENT")?;
        let mut upload_files = Vec::new();
        for value in &self.upload_files {
            let path = Path::new(value);
            agent_access::validate_components(path)?;
            let canonical = agent_access::normal_path(std::fs::canonicalize(path).map_err(|_| "INVALID_ARGUMENT")?);
            if !canonical.is_file() { return Err("INVALID_ARGUMENT"); }
            let value = canonical.to_str().ok_or("INVALID_ARGUMENT")?.to_owned();
            if !upload_files.contains(&value) { upload_files.push(value); }
        }
        download_directories.sort(); upload_directories.sort(); upload_files.sort();
        Ok(Self { download_directories, upload_files, upload_directories })
    }

    fn authorize(&self, path: &Path, access: Access) -> CodeResult<PathBuf> {
        let policy = AgentAccessSettings {
            enabled: true, allowed_directories: self.download_directories.clone(),
            upload_directories: self.upload_directories.clone(), ..Default::default()
        };
        match access {
            Access::DownloadDirectory => policy.authorize_directory(path),
            Access::DownloadFile => policy.authorize_file(path),
            Access::UploadDirectory => policy.authorize_upload_directory(path),
            Access::UploadPath | Access::UploadFile => {
                let from_directory = policy.authorize_upload_path(path);
                let resolved = match from_directory {
                    Ok(path) => path,
                    Err("FORBIDDEN") => {
                        agent_access::validate_components(path)?;
                        let resolved = agent_access::resolve_existing_ancestor(path)?;
                        // Resolve again on every use: replacing the approved path
                        // or its parent with a junction cannot redirect the grant.
                        if !self.upload_files.iter().any(|approved| {
                            agent_access::same_path(Path::new(approved), &resolved)
                        }) { return Err("FORBIDDEN"); }
                        resolved
                    }
                    Err(error) => return Err(error),
                };
                if matches!(access, Access::UploadFile) && !resolved.is_file() {
                    return Err("INVALID_ARGUMENT");
                }
                Ok(resolved)
            }
        }
    }
}

tokio::task_local! { static REQUEST_SCOPE: String; }

/// This context is deliberately not inherited by tokio::spawn/spawn_blocking.
/// Background workers must use the scope attached to their durable task/plan.
pub(crate) async fn with_scope<T>(scope_id: String, future: impl Future<Output = T>) -> T {
    REQUEST_SCOPE.scope(scope_id, future).await
}
pub(crate) fn current_scope_id() -> Option<String> {
    REQUEST_SCOPE.try_with(Clone::clone).ok()
}

pub(crate) fn inherit_scope(state: &AppState, destination_id: &str) -> CodeResult<()> {
    if let Some(source_id) = current_scope_id() { copy_scope(state, &source_id, destination_id)?; }
    Ok(())
}
pub(crate) fn copy_scope(state: &AppState, source_id: &str, destination_id: &str) -> CodeResult<()> {
    state.store.copy_agent_task_grant(source_id, destination_id)
}

fn authorize(state: &AppState, scope_id: Option<&str>, path: &Path, access: Access) -> CodeResult<PathBuf> {
    let policy = state.settings.lock().map_err(|_| "INTERNAL_ERROR")?.agent_access.clone();
    if !policy.enabled { return Err("FORBIDDEN"); }
    // Existing explicitly saved settings stay effective. Task approvals never
    // modify them; call policy methods directly to avoid a free-helper cycle.
    let global = match access {
        Access::DownloadDirectory => policy.authorize_directory(path),
        Access::DownloadFile => policy.authorize_file(path),
        Access::UploadPath => policy.authorize_upload_path(path),
        Access::UploadFile => policy.authorize_upload_file(path),
        Access::UploadDirectory => policy.authorize_upload_directory(path),
    };
    match global {
        Ok(path) => return Ok(path),
        Err("FORBIDDEN") => (),
        Err(error) => return Err(error),
    }
    let grant = match scope_id {
        Some(id) => state.store.agent_task_grant(id)?,
        None => None,
    }.ok_or("FORBIDDEN")?;
    grant.authorize(path, access)
}

macro_rules! authorizers {
    ($($current:ident, $explicit:ident, $kind:ident);+ $(;)?) => {$(
        pub(crate) fn $current(state: &AppState, path: &Path) -> CodeResult<PathBuf> {
            authorize(state, current_scope_id().as_deref(), path, Access::$kind)
        }
        pub(crate) fn $explicit(state: &AppState, scope_id: &str, path: &Path) -> CodeResult<PathBuf> {
            authorize(state, Some(scope_id), path, Access::$kind)
        }
    )+};
}
authorizers! {
    authorize_download_directory, authorize_download_directory_in_scope, DownloadDirectory;
    authorize_download_file, authorize_download_file_in_scope, DownloadFile;
    authorize_upload_path, authorize_upload_path_in_scope, UploadPath;
    authorize_upload_file, authorize_upload_file_in_scope, UploadFile;
    authorize_upload_directory, authorize_upload_directory_in_scope, UploadDirectory;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_task_grant_exact_upload_file_does_not_grant_its_siblings_or_download_access() {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("selected.txt");
        let sibling = directory.path().join("other.txt");
        std::fs::write(&selected, b"selected").unwrap();
        std::fs::write(&sibling, b"other").unwrap();
        let grant = LocalAccessGrant { upload_files: vec![selected.to_string_lossy().into_owned()], ..Default::default() }.validated().unwrap();
        assert!(grant.authorize(&selected, Access::UploadFile).is_ok());
        assert_eq!(grant.authorize(&sibling, Access::UploadFile), Err("FORBIDDEN"));
        assert_eq!(grant.authorize(directory.path(), Access::UploadDirectory), Err("FORBIDDEN"));
        assert_eq!(grant.authorize(&selected, Access::DownloadFile), Err("FORBIDDEN"));
        std::fs::remove_file(&selected).unwrap();
        // Receipt checks may still refer to the approved path, while a new
        // upload requires a real file and its original content snapshot.
        assert!(grant.authorize(&selected, Access::UploadPath).is_ok());
        assert_eq!(grant.authorize(&selected, Access::UploadFile), Err("INVALID_ARGUMENT"));
    }

    #[test]
    fn agent_task_grant_download_scope_rejects_prefix_siblings_and_parent_traversal() {
        let root = tempfile::tempdir().unwrap();
        let allowed = root.path().join("downloads");
        let sibling = root.path().join("downloads-other");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        let grant = LocalAccessGrant { download_directories: vec![allowed.to_string_lossy().into_owned()], ..Default::default() }.validated().unwrap();
        assert!(grant.authorize(&allowed.join("new/file.txt"), Access::DownloadFile).is_ok());
        assert_eq!(grant.authorize(&sibling.join("file.txt"), Access::DownloadFile), Err("FORBIDDEN"));
        assert_eq!(grant.authorize(&allowed.join("../downloads-other/file.txt"), Access::DownloadFile), Err("FORBIDDEN"));
        assert_eq!(grant.authorize(&allowed, Access::UploadDirectory), Err("FORBIDDEN"));
    }

    #[tokio::test]
    async fn agent_task_scope_isolated_across_concurrent_requests_and_not_inherited_by_workers() {
        assert!(current_scope_id().is_none());
        let (first, second) = tokio::join!(
            with_scope("first".into(), async {
                tokio::task::yield_now().await;
                assert!(tokio::spawn(async { current_scope_id() }).await.unwrap().is_none());
                current_scope_id()
            }),
            with_scope("second".into(), async { tokio::task::yield_now().await; current_scope_id() })
        );
        assert_eq!(first.as_deref(), Some("first"));
        assert_eq!(second.as_deref(), Some("second"));
        assert!(current_scope_id().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn agent_task_grant_rejects_an_exact_file_replaced_by_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let approved = root.path().join("approved");
        let outside = root.path().join("outside");
        std::fs::write(&approved, b"a").unwrap();
        std::fs::write(&outside, b"b").unwrap();
        let grant = LocalAccessGrant { upload_files: vec![approved.to_string_lossy().into_owned()], ..Default::default() }.validated().unwrap();
        std::fs::remove_file(&approved).unwrap();
        std::os::unix::fs::symlink(&outside, &approved).unwrap();
        assert_eq!(grant.authorize(&approved, Access::UploadFile), Err("FORBIDDEN"));
    }
}
