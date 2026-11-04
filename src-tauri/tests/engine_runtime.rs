#![cfg(any(windows, target_os = "macos", target_os = "linux"))]

use versadown_lib::{
    aria2::{Aria2GlobalStat, Aria2RpcClient, EventSink, RpcSecret},
    DownloadTask, EngineAvailability, EngineRuntime, EngineStateSink, NewTask, TaskStatus,
    TaskStore,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn bundled_engine() -> PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    versadown_lib::engine_binary::EngineBinary::for_current_target()
        .unwrap().resolve(root, root).unwrap()
}

#[derive(Default)]
struct Events {
    tasks: std::sync::Mutex<Vec<DownloadTask>>,
    states: std::sync::Mutex<Vec<EngineAvailability>>,
}

impl EventSink for Events {
    fn task_updated(&self, task: &DownloadTask) {
        self.tasks.lock().unwrap().push(task.clone());
    }

    fn global_stat_updated(&self, _: &Aria2GlobalStat) {}
}

impl EngineStateSink for Events {
    fn engine_state_updated(&self, state: &EngineAvailability) {
        self.states.lock().unwrap().push(state.clone());
    }
}

fn launch_config(root: &std::path::Path, binary: PathBuf) -> versadown_lib::Aria2LaunchConfig {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut aria2 = versadown_lib::Aria2Settings::default();
    aria2.rpc_port = listener.local_addr().unwrap().port();
    aria2.rpc_token = format!("engine-runtime-test-{}", aria2.rpc_port);
    drop(listener);
    versadown_lib::Aria2LaunchConfig {
        binary,
        data_dir: root.join("data"),
        download_dir: root.join("downloads"),
        parent_pid: std::process::id(),
        peer_discovery: false,
        aria2,
    }
}

#[tokio::test]
async fn failed_start_keeps_runtime_available_and_marks_active_tasks_recoverable() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct("http://127.0.0.1/file.bin"))
        .unwrap();
    let mut task = store.bind_gid(task.id, "stale-gid").unwrap();
    task.status = TaskStatus::Downloading;
    task.download_speed = 100;
    let active_id = task.id;
    store.apply_transfer_snapshot(task).unwrap();
    let parsing = store
        .create(NewTask::direct("http://127.0.0.1/not-submitted.bin"))
        .unwrap();
    let parsing_id = parsing.id;
    let events = Arc::new(Events::default());
    let runtime = EngineRuntime::new(
        launch_config(root.path(), root.path().join("missing-aria2c.exe")),
        Arc::clone(&store),
        events.clone(),
        events.clone(),
    );

    let state = runtime.initialize().await;

    assert!(!state.available);
    assert!(!state.retrying);
    assert!(state.error.as_deref().unwrap().contains("下载引擎不可用"));
    let unavailable = match runtime.service().await {
        Ok(_) => panic!("offline runtime unexpectedly exposed a download service"),
        Err(error) => error,
    };
    assert!(unavailable.to_string().contains("下载引擎不可用"));
    let task = store.find(active_id).unwrap();
    assert_eq!(task.status, TaskStatus::Paused);
    assert_eq!(task.download_speed, 0);
    assert!(task.error.as_deref().unwrap().contains("下载引擎不可用"));
    let parsing = store.find(parsing_id).unwrap();
    assert_eq!(parsing.status, TaskStatus::Failed);
    assert!(parsing.error.as_deref().unwrap().contains("重新添加"));
    assert_eq!(events.tasks.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn concurrent_manual_retries_start_one_engine_and_shutdown_cleanly() {
    let root = tempfile::tempdir().unwrap();
    let staged_binary = root.path().join("aria2c.exe");
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let events = Arc::new(Events::default());
    let runtime = Arc::new(EngineRuntime::new(
        launch_config(root.path(), staged_binary.clone()),
        Arc::clone(&store),
        events.clone(),
        events,
    ));
    assert!(!runtime.initialize().await.available);
    std::fs::copy(
        bundled_engine(),
        staged_binary,
    )
    .unwrap();

    let (first, second) = tokio::join!(runtime.retry(), runtime.retry());

    assert!(first.available && second.available);
    let first_service = runtime.service().await.unwrap();
    let second_service = runtime.service().await.unwrap();
    assert!(Arc::ptr_eq(&first_service, &second_service));
    assert_eq!(first_service.engine_running().await.unwrap(), Some(true));
    let stat = runtime.global_stat().await.unwrap();
    assert_eq!(stat.num_active, 0);
    let report = runtime.shutdown(Duration::from_secs(5)).await;
    assert!(report.child_stopped);
    assert!(!runtime.status().available);
}

#[tokio::test]
async fn runtime_reconfigures_to_the_saved_rpc_port_and_token_immediately() {
    let root = tempfile::tempdir().unwrap();
    let binary = bundled_engine();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let events = Arc::new(Events::default());
    let runtime = EngineRuntime::new(
        launch_config(root.path(), binary.clone()),
        store,
        events.clone(),
        events,
    );
    assert!(runtime.initialize().await.available);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut updated = launch_config(root.path(), binary);
    updated.aria2.rpc_port = port;
    updated.aria2.rpc_token = "reconfigured-local-token".into();
    updated.aria2.max_concurrent_downloads = 3;
    updated.aria2.max_connections_per_server = 4;
    updated.aria2.bt_seed_ratio = 1.5;

    assert!(
        runtime
            .reconfigure(updated.clone())
            .await
            .unwrap()
            .available
    );
    let client = Aria2RpcClient::new(
        format!("http://127.0.0.1:{port}/jsonrpc"),
        RpcSecret::from_token(updated.aria2.rpc_token).unwrap(),
    )
    .unwrap();
    assert_eq!(client.get_version().await.unwrap().version, "3.0.9");
    assert!(runtime.shutdown(Duration::from_secs(5)).await.child_stopped);
}

#[tokio::test]
async fn failed_reconfiguration_restores_the_previous_running_engine() {
    let root = tempfile::tempdir().unwrap();
    let binary = bundled_engine();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let events = Arc::new(Events::default());
    let runtime = EngineRuntime::new(
        launch_config(root.path(), binary),
        store,
        events.clone(),
        events,
    );
    assert!(runtime.initialize().await.available);

    let mut invalid = launch_config(root.path(), root.path().join("missing-aria2c.exe"));
    invalid.aria2.rpc_token = "invalid-reconfiguration-token".into();
    let error = runtime.reconfigure(invalid).await.unwrap_err();

    assert!(error.contains("下载引擎不可用"));
    assert!(runtime.status().available);
    assert_eq!(runtime.global_stat().await.unwrap().num_active, 0);
    assert!(runtime.shutdown(Duration::from_secs(5)).await.child_stopped);
}

#[tokio::test]
async fn monitor_restarts_an_exited_owned_engine_once_without_exposing_the_old_service() {
    let root = tempfile::tempdir().unwrap();
    let binary = bundled_engine();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let events = Arc::new(Events::default());
    let runtime = Arc::new(EngineRuntime::new(
        launch_config(root.path(), binary),
        store,
        events.clone(),
        events,
    ));
    assert!(runtime.initialize().await.available);
    runtime.start_monitor(Duration::from_millis(20));
    let old_service = runtime.service().await.unwrap();
    assert_eq!(old_service.api().shutdown().await.unwrap(), "OK");
    for _ in 0..100 {
        if old_service.engine_running().await.unwrap() == Some(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let replacement = tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if let Ok(service) = runtime.service().await {
                if !Arc::ptr_eq(&old_service, &service)
                    && service.engine_running().await.unwrap() == Some(true)
                {
                    return service;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("engine monitor did not replace the exited child");

    assert!(!Arc::ptr_eq(&old_service, &replacement));
    assert!(runtime.status().available);
    assert!(runtime.shutdown(Duration::from_secs(5)).await.child_stopped);
}
