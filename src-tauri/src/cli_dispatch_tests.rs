use std::{collections::BTreeMap, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}}};
use async_trait::async_trait;
use serde_json::{json, Value};
use crate::{aria2::*, store::{CliCreateOutcome, CliDispatchIntent}, DownloadAction, DownloadService, NewTask, TaskStatus, TaskStore, UriSubmissionOptions};

#[derive(Default)]
struct Engine {
    jobs: Mutex<BTreeMap<String, Aria2Status>>,
    submitted: Mutex<Vec<Aria2Options>>,
    adds: AtomicUsize,
    lose_add_response: AtomicBool,
    fail_snapshot: AtomicBool,
    fail_save: AtomicBool,
    unsafe_torrent_path: AtomicBool,
}

impl Engine {
    fn statuses(&self, states: &[&str]) -> Vec<Aria2Status> {
        self.jobs.lock().unwrap().values().filter(|s| states.contains(&s.status.as_str())).cloned().collect()
    }
    fn state(&self, gid: &str, state: &str) -> Result<String, Aria2Error> {
        self.jobs.lock().unwrap().get_mut(gid).ok_or(Aria2Error::Transport)?.status = state.into();
        Ok(gid.into())
    }
}

#[async_trait]
impl Aria2Api for Engine {
    async fn add_uri(&self, uris: Vec<String>, options: Aria2Options) -> Result<String, Aria2Error> {
        let gid = options.get("gid").expect("durable GID required").clone();
        assert_eq!(options.get("pause").map(String::as_str), Some("true"));
        assert_eq!(options.get("allow-overwrite").map(String::as_str), Some("false"));
        let path = std::path::Path::new(options.get("dir").unwrap()).join(options.get("out").unwrap());
        let status: Aria2Status = serde_json::from_value(json!({
            "gid": gid, "status": "paused", "totalLength": "100", "completedLength": "0",
            "downloadSpeed": "0", "uploadSpeed": "0", "connections": "0",
            "files": [{"index":"1","path":path,"length":"100","completedLength":"0","selected":"true",
                "uris":[{"uri":uris[0],"status":"used"}]}]
        })).unwrap();
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.contains_key(&gid) { return Err(Aria2Error::Rpc(RpcFault { code: 1, message: "GID already exists".into() })); }
        jobs.insert(gid.clone(), status);
        self.adds.fetch_add(1, Ordering::SeqCst);
        self.submitted.lock().unwrap().push(options);
        if self.lose_add_response.swap(false, Ordering::SeqCst) { Err(Aria2Error::Timeout) } else { Ok(gid) }
    }
    async fn tell_status(&self, gid: &str) -> Result<Aria2Status, Aria2Error> {
        self.jobs.lock().unwrap().get(gid).cloned().ok_or(Aria2Error::Transport)
    }
    async fn tell_active(&self) -> Result<Vec<Aria2Status>, Aria2Error> {
        if self.fail_snapshot.load(Ordering::SeqCst) { return Err(Aria2Error::Transport); }
        Ok(self.statuses(&["active"]))
    }
    async fn tell_waiting(&self, offset: i64, _: u32) -> Result<Vec<Aria2Status>, Aria2Error> {
        Ok(if offset == 0 { self.statuses(&["paused","waiting"]) } else { vec![] })
    }
    async fn tell_stopped(&self, offset: i64, _: u32) -> Result<Vec<Aria2Status>, Aria2Error> {
        Ok(if offset == 0 { self.statuses(&["complete","error","removed"]) } else { vec![] })
    }
    async fn pause(&self, gid: &str) -> Result<String, Aria2Error> { self.state(gid,"paused") }
    async fn unpause(&self, gid: &str) -> Result<String, Aria2Error> { self.state(gid,"active") }
    async fn remove(&self, gid: &str) -> Result<String, Aria2Error> { self.state(gid,"removed") }
    async fn force_remove(&self, gid: &str) -> Result<String, Aria2Error> { self.state(gid,"removed") }
    async fn save_session(&self) -> Result<String, Aria2Error> {
        if self.fail_save.load(Ordering::SeqCst) { Err(Aria2Error::Transport) } else { Ok("OK".into()) }
    }
    async fn add_torrent(&self, torrent:String, _:Vec<String>, options:Aria2Options)->Result<String,Aria2Error>{
        let metadata = crate::cli_bt::inspect(&torrent).unwrap();
        let gid = options.get("gid").expect("durable torrent GID required").clone();
        assert_eq!(options.get("pause").map(String::as_str), Some("true"));
        assert_eq!(options.get("allow-overwrite").map(String::as_str), Some("false"));
        assert_eq!(options.get("continue").map(String::as_str), Some("false"));
        let selected: Vec<u32> = options.get("select-file").unwrap().split(',').map(|value| value.parse().unwrap()).collect();
        let files: Vec<_> = metadata.files.iter().map(|file| {
            let path = std::path::Path::new(options.get("dir").unwrap()).join(&file.path);
            let path = if self.unsafe_torrent_path.load(Ordering::SeqCst) { path.parent().unwrap().join("../outside.bin") } else { path };
            json!({"index":file.index.to_string(),"path":path,"length":file.length.to_string(),"completedLength":"0",
                "selected":if selected.contains(&file.index) {"true"} else {"false"},"uris":[]})
        }).collect();
        let status: Aria2Status = serde_json::from_value(json!({"gid":gid,"status":"paused","totalLength":"1",
            "completedLength":"0","downloadSpeed":"0","uploadSpeed":"0","connections":"0","files":files})).unwrap();
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.contains_key(&gid) { return Err(Aria2Error::Rpc(RpcFault { code:1, message:"GID already exists".into() })); }
        jobs.insert(gid.clone(), status);
        self.adds.fetch_add(1, Ordering::SeqCst);
        self.submitted.lock().unwrap().push(options);
        if self.lose_add_response.swap(false, Ordering::SeqCst) { Err(Aria2Error::Timeout) } else { Ok(gid) }
    }
    async fn get_global_stat(&self)->Result<Aria2GlobalStat,Aria2Error>{unreachable!()}
    async fn multicall(&self,_:Vec<RpcCall>)->Result<Vec<Result<Value,RpcFault>>,Aria2Error>{unreachable!()}
    async fn change_option(&self,_:&str,_:Aria2Options)->Result<String,Aria2Error>{unreachable!()}
    async fn get_option(&self,_:&str)->Result<Aria2Options,Aria2Error>{unreachable!()}
    async fn shutdown(&self)->Result<String,Aria2Error>{unreachable!()}
}

fn accept(store: &TaskStore, directory: &std::path::Path, key: &str) -> CliDispatchIntent {
    let mut task = NewTask::direct(format!("https://example.test/{key}.bin"));
    task.name = format!("{key}.bin");
    let options = UriSubmissionOptions { directory: Some(directory.into()), max_tries: Some(4), retry_wait_seconds: Some(7), ..Default::default() };
    let result = store.create_cli_task_with_intent(key, key, task, &options).unwrap();
    let CliCreateOutcome::Created(task) = result else { panic!("expected accepted task") };
    assert_eq!(task.status, TaskStatus::Queued);
    store.pending_cli_intents().unwrap().into_iter().find(|i| i.task_id == task.id).unwrap()
}

#[tokio::test]
async fn cli_dispatch_recovers_acceptance_after_database_reopen_with_original_options() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("tasks.sqlite");
    let intent = {
        let store = TaskStore::open_path(&db).unwrap();
        accept(&store, directory.path(), "one")
    };
    let store = Arc::new(TaskStore::open_path(&db).unwrap());
    let reopened = store.pending_cli_intents().unwrap().remove(0);
    assert_eq!(reopened.gid, intent.gid);
    assert_eq!(reopened.options, intent.options);
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    let task = service.dispatch_cli_intent(&reopened, &|_| true).await.unwrap();
    assert_eq!(task.status, TaskStatus::Downloading);
    assert_eq!(task.aria_gid.as_deref(), Some(intent.gid.as_str()));
    assert!(!store.cli_dispatch_pending(task.id).unwrap());
    let options = engine.submitted.lock().unwrap();
    assert_eq!(options[0]["dir"], directory.path().to_string_lossy());
    assert_eq!(options[0]["max-tries"], "4");
    assert_eq!(options[0]["retry-wait"], "7");
}

#[tokio::test]
async fn cli_dispatch_lost_add_response_reattaches_same_engine_job_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("tasks.sqlite");
    let engine = Arc::new(Engine::default());
    engine.lose_add_response.store(true, Ordering::SeqCst);
    let intent = {
        let store = Arc::new(TaskStore::open_path(&db).unwrap());
        let intent = accept(&store, directory.path(), "response-loss");
        let service = DownloadService::new(engine.clone(), store.clone());
        assert!(service.dispatch_cli_intent(&intent, &|_| true).await.is_err());
        assert!(store.cli_dispatch_pending(intent.task_id).unwrap());
        assert_eq!(engine.tell_status(&intent.gid).await.unwrap().status, "paused");
        intent
    };
    let store = Arc::new(TaskStore::open_path(&db).unwrap());
    let service = DownloadService::new(engine.clone(), store.clone());
    let report = service.recover_native(directory.path()).await.unwrap();
    assert_eq!(report.imported, 0);
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(store.find(intent.task_id).unwrap().status, TaskStatus::Queued);
    service.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
    assert_eq!(engine.jobs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cli_dispatch_requires_successful_snapshot_and_session_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let intent = accept(&store, directory.path(), "one");
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    engine.fail_snapshot.store(true, Ordering::SeqCst);
    assert!(service.dispatch_cli_intent(&intent, &|_| true).await.is_err());
    assert_eq!(engine.adds.load(Ordering::SeqCst), 0);
    engine.fail_snapshot.store(false, Ordering::SeqCst);
    engine.fail_save.store(true, Ordering::SeqCst);
    assert!(service.dispatch_cli_intent(&intent, &|_| true).await.is_err());
    assert_eq!(engine.tell_status(&intent.gid).await.unwrap().status,"paused");
    engine.fail_save.store(false, Ordering::SeqCst);
    service.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_dispatch_pending_pause_cancel_and_revocation_never_start_new_work() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    let intent = accept(&store, directory.path(), "one");
    service.dispatch_cli_intent(&intent, &|_| false).await.unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst), 0);
    store.control_pending_cli(intent.task_id, DownloadAction::Pause).unwrap();
    service.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst), 0);
    store.control_pending_cli(intent.task_id, DownloadAction::Resume).unwrap();
    engine.lose_add_response.store(true, Ordering::SeqCst);
    assert!(service.dispatch_cli_intent(&intent, &|_| true).await.is_err());
    service.dispatch_cli_intent(&intent, &|_| false).await.unwrap();
    assert_eq!(engine.tell_status(&intent.gid).await.unwrap().status,"paused");
    store.control_pending_cli(intent.task_id, DownloadAction::Cancel).unwrap();
    service.dispatch_cli_intent(&intent, &|_| false).await.unwrap();
    assert_eq!(store.find(intent.task_id).unwrap().status, TaskStatus::Cancelled);
    assert_eq!(engine.tell_status(&intent.gid).await.unwrap().status,"removed");
    assert!(!store.cli_dispatch_pending(intent.task_id).unwrap());
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_dispatch_revalidates_final_path_and_never_overwrites_or_renames_reserved_output() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    let intent = accept(&store, directory.path(), "one");
    service.dispatch_cli_intent(&intent, &|path| path == directory.path()).await.unwrap();
    let output = store.reserved_output_path(intent.task_id).unwrap().unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst),0);
    std::fs::write(&output,b"preserve this file").unwrap();
    assert!(matches!(service.dispatch_cli_intent(&intent, &|_| true).await, Err(crate::DownloadServiceError::InvalidNativeCheckpoint)));
    assert_eq!(std::fs::read(&output).unwrap(), b"preserve this file");
    assert_eq!(engine.adds.load(Ordering::SeqCst),0);
}

#[tokio::test]
async fn cli_dispatch_rechecks_revocation_after_session_rpc_and_pauses_uncertain_active_job() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    let intent = accept(&store, directory.path(), "one");
    engine.lose_add_response.store(true, Ordering::SeqCst);
    assert!(service.dispatch_cli_intent(&intent, &|_| true).await.is_err());
    engine.state(&intent.gid, "active").unwrap();
    let checks = AtomicUsize::new(0);
    // Initial directory and reserved-output checks pass; authorization is
    // revoked by the time saveSession returns and activation is considered.
    service.dispatch_cli_intent(&intent, &|_| checks.fetch_add(1, Ordering::SeqCst) < 2).await.unwrap();
    assert_eq!(engine.tell_status(&intent.gid).await.unwrap().status, "paused");
    assert!(store.cli_dispatch_pending(intent.task_id).unwrap());
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
}

#[test]
fn cli_dispatch_atomic_intent_replay_preserves_identity_at_queue_limit_and_after_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let store = TaskStore::open_in_memory().unwrap();
    let first = accept(&store, directory.path(), "0");
    for index in 1..1000 { accept(&store, directory.path(), &index.to_string()); }
    let task = NewTask::direct("https://example.test/0.bin");
    assert!(matches!(store.create_cli_task_with_intent("0","0",task.clone(),&first.options).unwrap(), CliCreateOutcome::Replayed(_)));
    assert!(matches!(store.create_cli_task_with_intent("overflow","overflow",NewTask::direct("https://example.test/overflow.bin"),&first.options).unwrap(), CliCreateOutcome::Busy));
    store.remove_task_record(first.task_id).unwrap();
    assert!(matches!(store.create_cli_task_with_intent("0","0",task,&first.options), Err(crate::StoreError::NotFound(_))));
    assert_eq!(store.list().unwrap().len(),999);
}

fn torrent_fixture() -> crate::cli_bt::BtPreparation {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        "d4:infod5:filesld6:lengthi1e4:pathl5:a.bineed6:lengthi1e4:pathl5:b.bineee4:name4:Pack12:piece lengthi1e6:pieces40:abcdefghijklmnopqrstabcdefghijklmnopqrstee");
    let mut metadata = crate::cli_bt::inspect(&encoded).unwrap();
    metadata.source = "fixture.torrent".into();
    metadata
}

fn torrent_task(store: &TaskStore, metadata: &crate::cli_bt::BtPreparation) -> crate::DownloadTask {
    store.create(NewTask { source: metadata.source.clone(), name: metadata.name.clone(),
        kind: "torrent".into(), is_private: false }).unwrap()
}

#[tokio::test]
async fn cli_bt_lost_response_recovers_one_fixed_gid_with_exact_file_selection() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.db");
    let metadata = torrent_fixture();
    let engine = Arc::new(Engine::default());
    let options = UriSubmissionOptions { directory: Some(directory.path().into()), ..Default::default() };
    let task_id;
    let gid;
    {
        let store = Arc::new(TaskStore::open_path(&database).unwrap());
        let task = torrent_task(&store, &metadata);
        task_id = task.id;
        let service = DownloadService::new(engine.clone(), store.clone());
        engine.lose_add_response.store(true, Ordering::SeqCst);
        assert!(service.dispatch_cli_torrent(task.id, &metadata, &[2], &options, &|_| true).await.is_err());
        gid = store.find(task.id).unwrap().aria_gid.unwrap();
        assert_eq!(engine.tell_status(&gid).await.unwrap().status, "paused");
    }
    let store = Arc::new(TaskStore::open_path(&database).unwrap());
    let service = DownloadService::new(engine.clone(), store.clone());
    let task = service.dispatch_cli_torrent(task_id, &metadata, &[2], &options, &|_| true).await.unwrap();
    assert_eq!(task.status, TaskStatus::Downloading);
    assert_eq!(task.aria_gid.as_deref(), Some(gid.as_str()));
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
    assert!(store.selection_confirmed(task_id).unwrap());
    assert_eq!(store.files(task_id).unwrap().iter().filter(|file| file.selected).map(|file| file.index).collect::<Vec<_>>(), vec![2]);
    assert_eq!(engine.submitted.lock().unwrap()[0]["select-file"], "2");
    service.dispatch_cli_torrent(task_id, &metadata, &[2], &options, &|_| true).await.unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cli_bt_refuses_unknown_existing_payload_and_unsafe_engine_paths() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let metadata = torrent_fixture();
    let task = torrent_task(&store, &metadata);
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    let options = UriSubmissionOptions { directory: Some(directory.path().into()), ..Default::default() };
    let output = crate::cli_bt::output_directory(task.id, directory.path());
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("user.bin"), b"preserve").unwrap();
    assert!(matches!(service.dispatch_cli_torrent(task.id, &metadata, &[1], &options, &|_| true).await,
        Err(crate::DownloadServiceError::InvalidNativeCheckpoint)));
    assert_eq!(engine.adds.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(output.join("user.bin")).unwrap(), b"preserve");

    let other = torrent_task(&store, &metadata);
    engine.unsafe_torrent_path.store(true, Ordering::SeqCst);
    assert!(matches!(service.dispatch_cli_torrent(other.id, &metadata, &[1], &options, &|_| true).await,
        Err(crate::DownloadServiceError::InvalidNativeCheckpoint)));
    let gid = store.find(other.id).unwrap().aria_gid.unwrap();
    assert_eq!(engine.tell_status(&gid).await.unwrap().status, "paused");
    assert!(!store.selection_confirmed(other.id).unwrap());
}

#[tokio::test]
async fn cli_bt_revocation_after_add_keeps_paused_job_and_retry_does_not_duplicate() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let metadata = torrent_fixture();
    let task = torrent_task(&store, &metadata);
    let engine = Arc::new(Engine::default());
    let service = DownloadService::new(engine.clone(), store.clone());
    let options = UriSubmissionOptions { directory: Some(directory.path().into()), ..Default::default() };
    let checks = AtomicUsize::new(0);
    assert!(matches!(service.dispatch_cli_torrent(task.id, &metadata, &[2], &options,
        &|_| checks.fetch_add(1, Ordering::SeqCst) < 8).await,
        Err(crate::DownloadServiceError::InvalidUriOption("authorization"))));
    let gid = store.find(task.id).unwrap().aria_gid.unwrap();
    assert_eq!(engine.tell_status(&gid).await.unwrap().status, "paused");
    service.dispatch_cli_torrent(task.id, &metadata, &[2], &options, &|_| true).await.unwrap();
    assert_eq!(engine.adds.load(Ordering::SeqCst), 1);
    assert_eq!(engine.tell_status(&gid).await.unwrap().status, "active");
}
