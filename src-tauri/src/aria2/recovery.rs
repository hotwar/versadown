use super::{Aria2Api, Aria2Status, DownloadService, DownloadServiceError};
use crate::{DownloadTask, TaskStatus};
use serde::Serialize;
use std::{collections::BTreeSet, path::Path, time::Duration};

const EMPTY_STARTUP_SNAPSHOT_RETRIES: usize = 10;
const EMPTY_STARTUP_SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(200);

fn pending_recovery_name(gid: &str) -> String {
    format!("等待获取文件名（{gid}）")
}

pub(super) fn needs_recovery_name(task: &DownloadTask) -> bool {
    task.source.strip_prefix("aria2:").is_some_and(|gid| {
        task.name == "恢复的下载任务"
            || task.name == pending_recovery_name(gid)
            || task.name.trim().is_empty()
    })
}

pub(super) fn recovery_name(status: &Aria2Status) -> Option<String> {
    if let Some(name) = status
        .bittorrent
        .as_ref()
        .and_then(|torrent| torrent.info.as_ref())
        .and_then(|info| info.name.as_deref())
        .filter(|name| !name.trim().is_empty())
    {
        return Some(name.to_owned());
    }
    recovery_file_name(status.files.iter().map(|file| file.path.as_str()))
}

pub(super) fn recovery_file_name<'a>(paths: impl Iterator<Item = &'a str>) -> Option<String> {
    let paths: Vec<_> = paths
        .filter(|path| !path.trim().is_empty())
        .map(|path| {
            if path.as_bytes().get(1) == Some(&b':') || path.starts_with("\\\\") {
                path.replace('\\', "/")
            } else {
                path.to_owned()
            }
        })
        .collect();
    let first = paths.first()?;
    let parts: Vec<_> = first.split('/').filter(|part| !part.is_empty()).collect();
    let filename = *parts.last()?;
    if paths.len() > 1 {
        let mut common = parts.len().saturating_sub(1);
        for path in paths.iter().skip(1) {
            let others: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
            common = parts[..common]
                .iter()
                .zip(others.iter().take(others.len().saturating_sub(1)))
                .take_while(|(left, right)| left == right)
                .count();
        }
        if common > 0 && !parts[common - 1].ends_with(':') {
            return Some(parts[common - 1].to_owned());
        }
    }
    (!filename.ends_with(':') && filename != "." && filename != "..").then(|| filename.to_owned())
}

#[derive(Default, Debug, Serialize)]
pub struct ShutdownReport {
    pub session_saved: bool,
    pub shutdown_requested: bool,
    pub force_killed: bool,
    pub child_stopped: bool,
}

#[derive(Default, Debug, Serialize)]
pub struct RecoveryReport {
    pub reconciled: usize,
    /// External aria2 jobs discovered while restoring the engine.
    pub imported: usize,
    /// Newly imported active/waiting jobs that must resume after aria2 starts
    /// the saved session in its safe paused mode. This is runtime-only state.
    #[serde(skip)]
    pub resumable_task_ids: Vec<uuid::Uuid>,
    /// Completion is now reported by aria2-next rather than inferred locally.
    pub completed: usize,
    /// Resume is owned by aria2-next and is never synthesized by the app.
    pub resumed: usize,
    pub removed_tasks: usize,
    pub removed_engine_tasks: usize,
    pub removed_files: usize,
    pub failed: usize,
}

impl DownloadService {
    pub async fn recover_native(
        &self,
        _download_root: &Path,
    ) -> Result<RecoveryReport, DownloadServiceError> {
        let _guard = self.operation.lock().await;
        let api = self.api();
        let store = self.store();
        let has_persisted_engine_tasks = store.list()?.into_iter().any(|task| {
            !matches!(task.status, TaskStatus::Completed | TaskStatus::Recycled)
                && task.aria_gid.is_some()
                && !crate::xunlei_native::is_native_gid(task.aria_gid.as_deref())
        });
        let mut statuses = Self::native_statuses(api.as_ref()).await?;
        if has_persisted_engine_tasks && statuses.is_empty() {
            for _ in 0..EMPTY_STARTUP_SNAPSHOT_RETRIES {
                tokio::time::sleep(EMPTY_STARTUP_SNAPSHOT_RETRY_DELAY).await;
                statuses = Self::native_statuses(api.as_ref()).await?;
                if !statuses.is_empty() {
                    break;
                }
            }
        }

        // A completed metadata parent can bind its followed child while being
        // reconciled. Process already-owned GIDs first so that child is not
        // mistaken for an orphan merely because aria2 returned it earlier.
        statuses.sort_by_key(|status| store.find_by_gid(&status.gid).ok().flatten().is_none());
        let native_gids = statuses
            .iter()
            .flat_map(|status| {
                std::iter::once(status.gid.clone()).chain(status.followed_by.iter().cloned())
            })
            .collect::<BTreeSet<_>>();
        let mut report = RecoveryReport::default();

        for status in statuses {
            let Some(task) = store.find_by_gid(&status.gid)? else {
                if self.discard_deleted_engine_job(&status).await? {
                    continue;
                }
                // An application restart loses the in-memory knowledge of
                // work that another RPC client submitted while this app was
                // closed. Active and waiting jobs become ordinary local tasks;
                // they intentionally receive no live-session extra slot.
                // Historical stopped results are left to aria2's own result
                // retention and are not turned into new UI history entries.
                if matches!(status.status.as_str(), "active" | "waiting" | "paused") {
                    let imported = self.import_external_rpc_status_unlocked(status).await?;
                    report.imported += 1;
                    report.resumable_task_ids.push(imported.id);
                }
                continue;
            };

            // Accepted CLI work owns its GID before RPC. Leave its durable
            // user intent untouched until the dispatcher checks authorization.
            if store.cli_dispatch_pending(task.id)? { continue; }

            if crate::cloud::download_dispatch::cloud_task_requires_provider_dispatch(&task) {
                // The persisted engine task still contains a short-lived
                // provider URL.  Its GID is retained only so the fresh URL can
                // reuse native byte ranges; never let generic startup recovery
                // overwrite the provider-dispatch state with this stale URL.
                continue;
            }

            // A replacement RPC client starts with no in-memory GID mapping.
            // Restore it during recovery so later status/control faults retain
            // the task ID in the local diagnostic log.
            api.record_task_operation(
                "aria2.recoveryBind",
                &task.id.to_string(),
                Some(&status.gid),
                None,
            );

            if self
                .apply_status(task.clone(), status.clone(), false)
                .await
                .is_ok()
            {
                report.reconciled += 1;
                continue;
            }

            report.failed += 1;
            let mut paused = store.find(task.id)?;
            if paused.status == TaskStatus::Completed {
                // A bad engine snapshot cannot undo a completed download.
                // Disown its stale GID without touching the historical payload.
                let retired = store.retire_completed_engine_task(
                    paused.id,
                    std::iter::once(status.gid.clone()).chain(status.followed_by.iter().cloned()),
                )?;
                self.discard_deleted_engine_job(&status).await?;
                self.publish_task(&retired)?;
                continue;
            }
            if matches!(paused.status, TaskStatus::Cancelled | TaskStatus::Recycled) {
                continue;
            }
            // For incomplete work, keep the payload and the observed GID so
            // a later recovery can reconcile the same engine entry.
            paused.status = TaskStatus::Paused;
            paused.error = Some("下载引擎任务状态无法同步；请稍后重试".into());
            paused.failure_code = None;
            paused.download_speed = 0;
            paused.upload_speed = 0;
            paused.connections = 0;
            paused.discovered_nodes = 0;
            paused.eta_seconds = None;
            paused.aria_error_code = None;
            let paused = store.apply_transfer_snapshot(paused)?;
            self.publish_task(&paused)?;
        }

        let missing = store
            .list()?
            .into_iter()
            .filter(|task| {
                !matches!(task.status, TaskStatus::Completed | TaskStatus::Recycled)
                    && task.aria_gid.as_ref().is_some_and(|gid| {
                        !crate::xunlei_native::is_native_gid(Some(gid))
                            && !native_gids.contains(gid)
                    })
            })
            .map(|task| task.id)
            .collect::<Vec<_>>();
        for id in missing {
            if store.cli_dispatch_pending(id)? { continue; }
            let task = store.detach_missing_engine_task(id)?;
            self.publish_task(&task)?;
        }
        Ok(report)
    }

    pub(super) async fn native_statuses(api: &dyn Aria2Api) -> Result<Vec<Aria2Status>, DownloadServiceError> {
        let mut statuses = api.tell_active().await?;
        for stopped in [false, true] {
            let mut offset = 0;
            loop {
                let page = if stopped {
                    api.tell_stopped(offset, 1000).await?
                } else {
                    api.tell_waiting(offset, 1000).await?
                };
                let count = page.len();
                statuses.extend(page);
                if count < 1000 {
                    break;
                }
                offset += 1000;
            }
        }
        Ok(statuses)
    }
}
