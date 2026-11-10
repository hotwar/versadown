mod compatibility;
mod probe;
mod runtime;
mod transport;

pub(crate) use runtime::supported_versions;

use super::engine::{
    NativeEngine, NativeEngineError, NativeEngineSession, NativeProgress, NativeSubmission,
    NativeTaskHandle,
};
use crate::DownloadAction;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use transport::Worker;
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
struct SavedTask {
    task_id: Uuid,
    sdk_id: u32,
    account_hash: String,
    gcid: Option<String>,
    size: Option<u64>,
}

pub(super) struct MacWorkerEngine {
    worker: Worker,
    _runtime: Arc<runtime::Runtime>,
    session: NativeEngineSession,
    records: BTreeMap<PathBuf, SavedTask>,
    records_path: PathBuf,
    handles: BTreeMap<u32, u32>,
}

fn sdk_limit(value: u64) -> i64 {
    if value == 0 {
        -1
    } else {
        value.min(i64::MAX as u64) as i64
    }
}

fn account_hash(id: &str) -> String {
    format!("{:x}", Sha256::digest(id.as_bytes()))
}
pub(crate) fn checkpoint(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}.js",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

impl MacWorkerEngine {
    pub fn start(
        root: &Path,
        session: NativeEngineSession,
        peer_id: &str,
    ) -> Result<Self, NativeEngineError> {
        let runtime = Arc::new(runtime::installed_runtime(root)?);
        compatibility::ensure_probed(&runtime, || probe::verify(&runtime))?;
        Self::open(&runtime, &runtime.state, session, peer_id)
    }

    fn open(
        runtime: &Arc<runtime::Runtime>,
        state: &Path,
        session: NativeEngineSession,
        peer_id: &str,
    ) -> Result<Self, NativeEngineError> {
        let records_path = state.join("versadown-tasks.json");
        let records: BTreeMap<PathBuf, SavedTask> = if records_path.exists() {
            serde_json::from_slice(
                &fs::read(&records_path).map_err(|_| NativeEngineError::OperationFailed)?,
            )
            .map_err(|_| NativeEngineError::OperationFailed)?
        } else {
            BTreeMap::new()
        };
        let mut worker = Worker::start(&runtime.executable, state)?;
        let task_ids: Vec<u32> = records.values().map(|v| v.sdk_id).collect();
        worker.call(json!({
            "op": "init", "peerId": peer_id, "taskIds": task_ids,
            "appVersion": runtime.version.app_version,
            "clientVersion": runtime.version.client_version,
        }))?;
        Ok(Self {
            worker,
            session,
            records,
            records_path,
            handles: BTreeMap::new(),
            _runtime: Arc::clone(runtime),
        })
    }

    fn save(&self) -> Result<(), NativeEngineError> {
        let temporary = self.records_path.with_extension("tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| NativeEngineError::OperationFailed)?;
        serde_json::to_writer(&mut file, &self.records)
            .map_err(|_| NativeEngineError::OperationFailed)?;
        file.flush()
            .and_then(|_| file.sync_all())
            .map_err(|_| NativeEngineError::OperationFailed)?;
        fs::rename(temporary, &self.records_path).map_err(|_| NativeEngineError::OperationFailed)
    }

    fn sdk_id(&self, handle: NativeTaskHandle) -> Result<u32, NativeEngineError> {
        if !self.session.owns(handle) {
            return Err(NativeEngineError::TaskUnknown);
        }
        self.handles
            .get(&handle.task_id)
            .copied()
            .ok_or(NativeEngineError::TaskUnknown)
    }

    fn forget(&mut self, sdk_id: u32) -> Result<(), NativeEngineError> {
        self.worker.call(json!({"op":"forget","taskId":sdk_id}))?;
        self.handles.retain(|_, id| *id != sdk_id);
        self.records.retain(|_, record| record.sdk_id != sdk_id);
        self.save()
    }

    pub fn finish(&mut self, handle: NativeTaskHandle) -> Result<(), NativeEngineError> {
        self.forget(self.sdk_id(handle)?)
    }

    pub fn prune_removed_tasks(&mut self, store: &crate::TaskStore) {
        let retired: Vec<_> = self
            .records
            .values()
            .filter(|record| match store.find(record.task_id) {
                Err(crate::StoreError::NotFound(_)) => true,
                Ok(task) => task.status == crate::TaskStatus::Completed,
                Err(_) => false,
            })
            .map(|record| record.sdk_id)
            .collect();
        for id in retired {
            let _ = self.forget(id);
        }
    }

    pub fn is_alive(&mut self) -> bool {
        self.worker.is_alive()
    }
    pub fn shutdown(&mut self) {
        self.worker.shutdown();
    }

    pub fn set_limits(&mut self, download: u64, upload: u64) -> Result<(), NativeEngineError> {
        // VD/aria2 use 0 for unlimited; Thunder uses -1. Passing zero clamps
        // downloads to its 32 KiB/s minimum, making a valid resume look stuck.
        self.worker
            .call(json!({"op":"limits","download":sdk_limit(download),"upload":sdk_limit(upload)}))
            .map(|_| ())
    }

    pub fn submit_with_options(
        &mut self,
        task_id: Uuid,
        request: NativeSubmission,
        vip_type: u16,
        overwrite: bool,
        resume: bool,
    ) -> Result<NativeTaskHandle, NativeEngineError> {
        let path = request.output_path.clone();
        let old = self.records.get(&path).cloned();
        if overwrite {
            // The SDK sidecar name is also a valid, unrelated hidden JavaScript
            // file. Overwrite consent for the payload does not cover that file.
            let sidecar = checkpoint(&path);
            match fs::symlink_metadata(&sidecar) {
                Ok(_) if old.as_ref().is_none_or(|record| record.task_id != task_id) => {
                    return Err(NativeEngineError::OperationFailed);
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(NativeEngineError::OperationFailed);
                }
                _ => {}
            }
            if let Some(old) = &old {
                self.forget(old.sdk_id)?;
            }
            for file in [&path, &sidecar] {
                if file.exists() {
                    fs::remove_file(file).map_err(|_| NativeEngineError::OperationFailed)?;
                }
            }
        }
        self.worker
            .call(json!({"op":"identity","userId":request.user_id,"vipType":vip_type}))?;
        let identity = account_hash(&request.user_id);
        let sdk_id = if resume && !overwrite {
            let old = old.ok_or(NativeEngineError::TaskUnknown)?;
            if old.task_id != task_id
                || old.account_hash != identity
                || old.gcid != request.gcid
                || old.size != request.total_length
                || !path.is_file()
                || !checkpoint(&path).is_file()
            {
                return Err(NativeEngineError::TaskUnknown);
            }
            let info = self.worker.call(json!({"op":"poll","taskId":old.sdk_id}))?;
            verify_path(&info["info"], &path)?;
            // Changing the URL before resume returns 9104 in the Mac service.
            self.worker
                .call(json!({"op":"resume","taskId":old.sdk_id,"url":request.url}))?;
            old.sdk_id
        } else {
            if path.exists() || checkpoint(&path).exists() {
                return Err(NativeEngineError::OperationFailed);
            }
            if let Some(old) = old.filter(|_| !overwrite) {
                self.forget(old.sdk_id)?;
            }
            let header = |key: &str| {
                request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(key))
                    .map(|(_, value)| value.as_str())
            };
            let reply = self.worker.call(json!({"op":"create","url":request.url,"name":request.output_name,
                "directory":path.parent(),"size":request.total_length.unwrap_or(0),
                "referer":header("Referer").unwrap_or(""),"userAgent":header("User-Agent").unwrap_or("Mozilla/5.0")}))?;
            let sdk_id = reply["taskId"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .filter(|id| *id > 0)
                .ok_or(NativeEngineError::OperationFailed)?;
            let info = self.worker.call(json!({"op":"poll","taskId":sdk_id}))?;
            if verify_path(&info["info"], &path).is_err() {
                let _ = self.worker.call(json!({"op":"forget","taskId":sdk_id}));
                return Err(NativeEngineError::OperationFailed);
            }
            self.records.insert(
                path.clone(),
                SavedTask {
                    task_id,
                    sdk_id,
                    account_hash: identity,
                    gcid: request.gcid,
                    size: request.total_length,
                },
            );
            if let Err(error) = self.save() {
                let _ = self.worker.call(json!({"op":"pause","taskId":sdk_id}));
                return Err(error);
            }
            sdk_id
        };
        if let Some(token) = request.acceleration_token {
            if let Err(error) = self
                .worker
                .call(json!({"op":"token","taskId":sdk_id,"token":token}))
            {
                let _ = self.worker.call(json!({"op":"pause","taskId":sdk_id}));
                return Err(error);
            }
        }
        // A late callback for a previous VD binding may never stop a resubmitted task.
        self.handles.retain(|_, id| *id != sdk_id);
        // All workers restarted within one VD process share the session ID.
        // Never let an old poll or cancel address a new worker's binding.
        static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);
        let id = NEXT_HANDLE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| NativeEngineError::OperationFailed)?;
        self.handles.insert(id, sdk_id);
        self.session
            .allocate_handle(id)
            .ok_or(NativeEngineError::OperationFailed)
    }
}

fn verify_path(info: &Value, requested: &Path) -> Result<(), NativeEngineError> {
    let actual = PathBuf::from(
        info["taskDir"]
            .as_str()
            .ok_or(NativeEngineError::TaskUnknown)?,
    )
    .join(
        info["taskName"]
            .as_str()
            .ok_or(NativeEngineError::TaskUnknown)?,
    );
    if actual != requested {
        return Err(NativeEngineError::TaskUnknown);
    }
    Ok(())
}

fn progress(reply: &Value) -> NativeProgress {
    let info = &reply["info"];
    let n = |key: &str| info[key].as_u64().unwrap_or(0);
    let bytes = |key: &str| reply["bytes"][key].as_u64().unwrap_or(0);
    NativeProgress {
        completed: n("downloadedSize"),
        total: n("fileSize"),
        status: match n("state") {
            1 => super::NATIVE_DOWNLOADING,
            2 => super::NATIVE_STOPPED,
            3 => super::NATIVE_COMPLETE,
            _ if n("failedCode") != 0 => super::NATIVE_ERROR,
            _ => 0,
        },
        error_code: n("failedCode") as i32,
        connections: n("usingNumbers") as u32,
        received: bytes("DSKeyTaskRecvTotalBytes"),
        peer_received: bytes("DSKeyTaskRecvDCDNBytes")
            .saturating_add(bytes("DSKeyTaskRecvP2PBytes"))
            .saturating_add(bytes("DSKeyTaskRecvP2SBytes")),
    }
}

impl NativeEngine for MacWorkerEngine {
    fn submit(&mut self, request: NativeSubmission) -> Result<NativeTaskHandle, NativeEngineError> {
        self.submit_with_options(Uuid::new_v4(), request, 0, false, false)
    }
    fn poll(&mut self, handle: NativeTaskHandle) -> Result<NativeProgress, NativeEngineError> {
        let id = self.sdk_id(handle)?;
        self.worker
            .call(json!({"op":"poll","taskId":id}))
            .map(|reply| progress(&reply))
    }
    fn control(
        &mut self,
        handle: NativeTaskHandle,
        action: DownloadAction,
    ) -> Result<(), NativeEngineError> {
        let id = self.sdk_id(handle)?;
        let op = match action {
            DownloadAction::Pause => "pause",
            DownloadAction::Cancel => "cancel",
            DownloadAction::Resume => "resume",
        };
        self.worker.call(json!({"op":op,"taskId":id})).map(|_| ())
    }
    fn release(&mut self, handle: NativeTaskHandle) -> Result<(), NativeEngineError> {
        self.sdk_id(handle)?;
        self.handles.remove(&handle.task_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_unlimited_without_enabling_the_sdk_minimum_limit() {
        assert_eq!(sdk_limit(0), -1);
        assert_eq!(sdk_limit(104_857), 104_857);
        assert_eq!(sdk_limit(u64::MAX), i64::MAX);
    }

    // Run explicitly with a local Range-capable fixture server. No account or
    // Keychain access; uses only a disposable isolated native engine database.
    #[test]
    #[ignore = "requires installed Thunder and VERSADOWN_NATIVE_FIXTURE_URL"]
    fn installed_engine_resumes_same_file_after_process_restart() {
        let url = std::env::var("VERSADOWN_NATIVE_FIXTURE_URL").unwrap();
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("downloads");
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join("fixture.bin");
        let size = 8 * 1024 * 1024;
        let request = |fresh: bool| NativeSubmission {
            url: if fresh {
                format!("{url}?fresh=1")
            } else {
                url.clone()
            },
            headers: vec![],
            output_path: output.clone(),
            output_name: "fixture.bin".into(),
            user_id: "0".into(),
            peer_id: "00000000000000000000000000000001".into(),
            acceleration_token: None,
            cid: None,
            gcid: None,
            total_length: Some(size),
        };
        let session = NativeEngineSession::new();
        let task_id = Uuid::new_v4();
        let mut engine =
            MacWorkerEngine::start(root.path(), session, "00000000000000000000000000000001")
                .unwrap();
        engine.set_limits(256 * 1024, 1024 * 1024).unwrap();
        fs::write(&output, b"existing payload").unwrap();
        let sidecar = checkpoint(&output);
        fs::write(&sidecar, b"unrelated hidden script").unwrap();
        assert!(engine
            .submit_with_options(task_id, request(false), 0, true, false)
            .is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing payload");
        assert_eq!(fs::read(&sidecar).unwrap(), b"unrelated hidden script");
        fs::remove_file(&output).unwrap();
        fs::remove_file(&sidecar).unwrap();
        let handle = engine
            .submit_with_options(task_id, request(false), 0, false, false)
            .unwrap();
        let start = std::time::Instant::now();
        let before = loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let p = engine.poll(handle).unwrap();
            assert!(start.elapsed().as_secs() < 30, "no progress");
            if p.completed >= 256 * 1024 {
                break p.completed;
            }
        };
        engine.control(handle, DownloadAction::Pause).unwrap();
        assert_eq!(
            engine.poll(handle).unwrap().status,
            super::super::NATIVE_STOPPED
        );
        engine.shutdown();
        drop(engine);
        let mut engine =
            MacWorkerEngine::start(root.path(), session, "00000000000000000000000000000001")
                .unwrap();
        assert!(engine.control(handle, DownloadAction::Cancel).is_err());
        engine.set_limits(0, 1024 * 1024).unwrap();

        let previous = handle;
        let handle = engine
            .submit_with_options(task_id, request(true), 0, false, true)
            .unwrap();
        assert!(engine.control(previous, DownloadAction::Cancel).is_err());
        assert!(engine.poll(handle).unwrap().completed >= before);
        let start = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let p = engine.poll(handle).unwrap();
            if start.elapsed().as_secs() >= 60 {
                let raw = engine
                    .worker
                    .call(json!({"op":"poll","taskId":engine.sdk_id(handle).unwrap()}))
                    .unwrap();
                let data = fs::read(&output).unwrap();
                panic!(
                    "resume did not finish: {} / {}, raw {}, valid bytes {}",
                    p.completed,
                    p.total,
                    raw,
                    data.iter()
                        .enumerate()
                        .filter(|(i, b)| **b == (i % 251) as u8)
                        .count()
                );
            }
            if p.status == super::super::NATIVE_COMPLETE {
                break;
            }
        }
        engine.finish(handle).unwrap();
        assert!(engine.records.is_empty());
        engine.shutdown();
        let bytes = fs::read(&output).unwrap();
        assert_eq!(bytes.len(), size as usize);
        assert!(bytes.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|f| !f.file_name().to_string_lossy().starts_with('.'))
                .count(),
            1
        );
    }
    #[test]
    fn rejects_sdk_automatic_rename() {
        assert!(verify_path(
            &json!({"taskDir":"/tmp","taskName":"file(1).bin"}),
            Path::new("/tmp/file.bin")
        )
        .is_err());
        assert!(verify_path(
            &json!({"taskDir":"/tmp/","taskName":"file.bin"}),
            Path::new("/tmp/file.bin")
        )
        .is_ok());
        assert_eq!(
            checkpoint(Path::new("/tmp/file.bin")),
            Path::new("/tmp/.file.bin.js")
        );
    }
    #[test]
    fn normalizes_mac_states_and_real_resource_counters() {
        let p = progress(
            &json!({"info":{"state":1,"downloadedSize":50,"fileSize":100},"bytes":{"DSKeyTaskRecvTotalBytes":60,"DSKeyTaskRecvDCDNBytes":10,"DSKeyTaskRecvOriginBytes":50}}),
        );
        assert_eq!(p.status, super::super::NATIVE_DOWNLOADING);
        assert_eq!((p.received, p.peer_received, p.completed), (60, 10, 50));
        assert_eq!(
            progress(&json!({"info":{"state":2}})).status,
            super::super::NATIVE_STOPPED
        );
        assert_eq!(
            progress(&json!({"info":{"state":3}})).status,
            super::super::NATIVE_COMPLETE
        );
    }
}
