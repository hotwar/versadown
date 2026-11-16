use super::*;
use crate::{models::NewTask, store::{CliCreateOutcome, CliDispatchIntent}};

struct NoEvents;
impl EventSink for NoEvents {
    fn task_updated(&self, _: &DownloadTask) {}
    fn global_stat_updated(&self, _: &crate::Aria2GlobalStat) {}
}

fn accept(store: &TaskStore, directory: &Path) -> CliDispatchIntent {
    let task = NewTask { source: "ftp://example.test/archive.bin".into(), name: "archive.bin".into(),
        kind: "ftp".into(), is_private: false };
    let options = UriSubmissionOptions { directory: Some(directory.into()), ..Default::default() };
    let CliCreateOutcome::Created(task) = store.create_cli_task_with_intent("ftp-one", "ftp-one", task, &options).unwrap()
        else { panic!("task must be accepted"); };
    assert!(task.aria_gid.is_none(), "FTP must not be attached to an aria2 job");
    store.pending_cli_intents().unwrap().remove(0)
}

async fn idle_engine(store: &Arc<TaskStore>, directory: &Path) -> Arc<FtpDownloadEngine> {
    let engine = Arc::new(FtpDownloadEngine::new(directory, Arc::clone(store), Arc::new(NoEvents)));
    engine.set_limits(1, 1);
    // Occupy the only scheduler slot. Tests observe scheduling without making
    // network calls or spawning a process, then exercise normal queue controls.
    let (sender, _) = watch::channel(WorkerCommand::Running);
    engine.scheduler.lock().await.active.insert(Uuid::new_v4(), sender);
    engine
}

#[tokio::test]
async fn cli_ftp_recovery_requires_authorization_and_reuses_one_worker_and_output() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let intent = accept(&store, directory.path());
    let engine = idle_engine(&store, directory.path()).await;
    engine.recover().await.unwrap();
    assert_eq!(store.find(intent.task_id).unwrap().status, TaskStatus::Queued);
    assert!(engine.scheduler.lock().await.queued.is_empty());
    engine.dispatch_cli_intent(&intent, &|_| false).await.unwrap();
    assert!(store.files(intent.task_id).unwrap().is_empty());
    engine.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    let files = store.files(intent.task_id).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(engine.scheduler.lock().await.queued.len(), 1);
    assert!(!store.cli_dispatch_pending(intent.task_id).unwrap());
    assert_eq!(store.pending_cli_intents().unwrap().len(), 1, "active FTP remains recoverable after acknowledgment");
    engine.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert_eq!(engine.scheduler.lock().await.queued.len(), 1);
    assert_eq!(store.files(intent.task_id).unwrap()[0].path, files[0].path);
    engine.control(intent.task_id, DownloadAction::Pause).await.unwrap();
    engine.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert!(engine.scheduler.lock().await.queued.is_empty());
    assert_eq!(store.find(intent.task_id).unwrap().status, TaskStatus::Paused);
}

#[tokio::test]
async fn cli_ftp_recovers_persisted_preparation_before_worker_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("tasks.db");
    let intent;
    let original_output;
    {
        let store = Arc::new(TaskStore::open_path(&database).unwrap());
        intent = accept(&store, directory.path());
        let engine = idle_engine(&store, directory.path()).await;
        let task = store.find(intent.task_id).unwrap();
        let (_, output) = engine.prepare_submission(task.id, &task.source, intent.options.clone()).await.unwrap();
        original_output = output;
        assert!(engine.scheduler.lock().await.queued.is_empty());
    }
    let store = Arc::new(TaskStore::open_path(&database).unwrap());
    let engine = idle_engine(&store, directory.path()).await;
    engine.recover().await.unwrap();
    engine.dispatch_cli_intent(&intent, &|_| false).await.unwrap();
    assert!(engine.scheduler.lock().await.queued.is_empty());
    engine.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert_eq!(engine.scheduler.lock().await.queued.len(), 1);
    assert_eq!(Path::new(&store.files(intent.task_id).unwrap()[0].path), original_output);
    engine.control(intent.task_id, DownloadAction::Cancel).await.unwrap();
    engine.dispatch_cli_intent(&intent, &|_| true).await.unwrap();
    assert!(engine.scheduler.lock().await.queued.is_empty());
    assert_eq!(store.find(intent.task_id).unwrap().status, TaskStatus::Cancelled);
}

#[tokio::test]
async fn cli_ftp_revocation_during_preparation_prevents_worker_and_unknown_payload_is_preserved() {
    use std::sync::atomic::AtomicUsize;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let intent = accept(&store, directory.path());
    let engine = idle_engine(&store, directory.path()).await;
    let checks = AtomicUsize::new(0);
    engine.dispatch_cli_intent(&intent, &|_| checks.fetch_add(1, Ordering::SeqCst) == 0).await.unwrap();
    assert!(engine.scheduler.lock().await.queued.is_empty());
    assert_eq!(store.find(intent.task_id).unwrap().status, TaskStatus::Queued);

    let other_store = Arc::new(TaskStore::open_in_memory().unwrap());
    let other = accept(&other_store, directory.path());
    let output = reserve_unique_output_path(&other_store, other.task_id, directory.path(), "unowned.bin").unwrap();
    std::fs::write(&output, b"keep me").unwrap();
    let other_engine = idle_engine(&other_store, directory.path()).await;
    assert!(matches!(other_engine.dispatch_cli_intent(&other, &|_| true).await,
        Err(FtpDownloadError::InvalidOption("recovery checkpoint"))));
    assert_eq!(std::fs::read(&output).unwrap(), b"keep me");
    assert!(other_engine.scheduler.lock().await.queued.is_empty());
}
