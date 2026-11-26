#[path = "aria2_sync.rs"]
mod fixture;

use versadown_lib::{DownloadService, NewTask, TaskStatus, TaskStore};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

async fn slow_server(
    delay: Duration,
) -> (
    versadown_lib::aria2::Aria2RpcClient,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/jsonrpc", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/jsonrpc",
        axum::routing::post(
            move |axum::Json(request): axum::Json<serde_json::Value>| async move {
                tokio::time::sleep(delay).await;
                axum::Json(json!({"jsonrpc":"2.0","id":request["id"],"result":"OK"}))
            },
        ),
    );
    let worker = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        versadown_lib::aria2::Aria2RpcClient::new(
            endpoint,
            versadown_lib::aria2::RpcSecret::from_bytes([1; 32]),
        )
        .unwrap(),
        worker,
    )
}

#[tokio::test]
async fn shutdown_one_deadline_covers_save_and_shutdown() {
    let (client, server) = slow_server(Duration::from_millis(90)).await;
    let service = DownloadService::new(
        Arc::new(client),
        Arc::new(TaskStore::open_in_memory().unwrap()),
    );
    let began = std::time::Instant::now();
    let report = service.shutdown_report(Duration::from_millis(120)).await;
    assert!(report.session_saved);
    assert!(!report.shutdown_requested);
    assert!(began.elapsed() < Duration::from_millis(170));
    server.abort();
}

#[tokio::test]
async fn native_recovery_reconciles_only_owned_job() {
    let (client, server) = fixture::server(|request| match request["method"].as_str().unwrap() {
        "aria2.tellActive" => json!([fixture::status("owned", "active", 25)]),
        "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
        other => panic!("{other}"),
    })
    .await;
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct("https://example.test/owned"))
        .unwrap();
    store.bind_gid(task.id, "owned").unwrap();

    let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();
    assert_eq!(report.reconciled, 1);
    assert_eq!(report.imported, 0);
    assert_eq!(store.find(task.id).unwrap().progress, 25);
    server.abort();
}

#[tokio::test]
async fn native_recovery_retries_an_empty_startup_snapshot_before_detaching_tasks() {
    let active_requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = Arc::clone(&active_requests);
    let (client, server) =
        fixture::server(move |request| match request["method"].as_str().unwrap() {
            "aria2.tellActive" => {
                if observed_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    json!([])
                } else {
                    json!([fixture::status("owned", "active", 25)])
                }
            }
            "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
            other => panic!("{other}"),
        })
        .await;
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct("https://example.test/owned"))
        .unwrap();
    store.bind_gid(task.id, "owned").unwrap();

    let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();

    assert_eq!(report.reconciled, 1);
    assert_eq!(active_requests.load(Ordering::SeqCst), 2);
    let restored = store.find(task.id).unwrap();
    assert_eq!(restored.aria_gid.as_deref(), Some("owned"));
    assert_eq!(restored.progress, 25);
    server.abort();
}

#[tokio::test]
async fn reconcile_repairs_a_recovery_placeholder_from_the_reported_file_name() {
    let (client, server) = fixture::server(|request| match request["method"].as_str().unwrap() {
        "aria2.tellStatus" => {
            let mut status = fixture::status("late-name", "active", 30);
            status["files"] = json!([{
                "index": "1",
                "path": "C:\\Downloads\\稍后取得的名称.pdf",
                "length": "100",
                "completedLength": "30",
                "selected": "true"
            }]);
            status
        }
        other => panic!("{other}"),
    })
    .await;
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask {
            source: "aria2:late-name".into(),
            name: "恢复的下载任务".into(),
            kind: "file".into(),
            is_private: false,
        })
        .unwrap();
    store.bind_gid(task.id, "late-name").unwrap();

    let updated = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .reconcile_task(task.id)
        .await
        .unwrap();

    assert_eq!(updated.name, "稍后取得的名称.pdf");
    assert_eq!(store.find(task.id).unwrap().name, "稍后取得的名称.pdf");
    server.abort();
}

#[tokio::test]
async fn native_recovery_imports_unowned_active_engine_job_without_deleting_payload() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("orphan.bin");
    std::fs::write(&output, b"partial").unwrap();
    std::fs::write(format!("{}.aria2", output.display()), b"legacy").unwrap();
    let wire_output = output.to_string_lossy().into_owned();
    let (client, server) =
        fixture::server(move |request| match request["method"].as_str().unwrap() {
            "aria2.tellActive" => {
                let mut status = fixture::status("orphan", "active", 20);
                status["files"] = json!([{
                    "index": "1", "path": wire_output, "length": "100",
                    "completedLength": "7", "selected": "true"
                }]);
                json!([status])
            }
            "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
            other => panic!("{other}"),
        })
        .await;

    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .with_external_rpc_slots(4)
        .recover_native(root.path())
        .await
        .unwrap();
    assert_eq!(report.imported, 1);
    assert_eq!(report.removed_engine_tasks, 0);
    assert_eq!(report.removed_files, 0);
    let imported = store.list().unwrap();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].aria_gid.as_deref(), Some("orphan"));
    assert_eq!(imported[0].status, TaskStatus::Downloading);
    assert!(store.selection_confirmed(imported[0].id).unwrap());
    assert!(output.exists());
    assert!(std::path::PathBuf::from(format!("{}.aria2", output.display())).exists());
    server.abort();
}

#[tokio::test]
async fn native_recovery_does_not_restore_a_permanently_deleted_engine_job() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("tasks.sqlite");
    {
        let store = TaskStore::open_path(&database).unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/deleted.bin"))
            .unwrap();
        store.bind_gid(task.id, "deleted-gid").unwrap();
        store.remove_task_records(&[task.id]).unwrap();
    }
    let removals = Arc::new(AtomicUsize::new(0));
    let observed_removals = Arc::clone(&removals);
    let (client, server) =
        fixture::server(move |request| match request["method"].as_str().unwrap() {
            "aria2.tellActive" => json!([fixture::status("deleted-gid", "active", 25)]),
            "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
            "aria2.forceRemove" | "aria2.removeDownloadResult" => {
                observed_removals.fetch_add(1, Ordering::SeqCst);
                json!("deleted-gid")
            }
            other => panic!("{other}"),
        })
        .await;
    let store = Arc::new(TaskStore::open_path(&database).unwrap());

    let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();

    assert_eq!(report.imported, 0);
    assert!(store.list().unwrap().is_empty());
    assert!(removals.load(Ordering::SeqCst) > 0);
    server.abort();
}

#[tokio::test]
async fn native_recovery_preserves_missing_nonterminal_record_and_payload_for_resume() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("missing.bin");
    std::fs::write(&output, b"partial").unwrap();
    std::fs::write(format!("{}.aria2", output.display()), b"legacy").unwrap();
    let (client, server) = fixture::server(|request| match request["method"].as_str().unwrap() {
        "aria2.tellActive" | "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
        other => panic!("{other}"),
    })
    .await;
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask {
            source: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
            name: "missing".into(),
            kind: "magnet".into(),
            is_private: false,
        })
        .unwrap();
    let mut task = store.bind_gid(task.id, "missing-native-gid").unwrap();
    task.status = TaskStatus::Downloading;
    store.apply_transfer_snapshot(task.clone()).unwrap();
    store
        .upsert_files(
            task.id,
            &[(1, output.to_string_lossy().into_owned(), 100, 7, true)],
        )
        .unwrap();
    store.confirm_selection(task.id).unwrap();

    let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();
    assert_eq!(report.removed_tasks, 0);
    let preserved = store.find(task.id).unwrap();
    assert_eq!(preserved.status, TaskStatus::Paused);
    assert_eq!(preserved.aria_gid, None);
    assert_eq!(store.files(task.id).unwrap().len(), 1);
    assert!(store.selection_confirmed(task.id).unwrap());
    assert!(output.exists());
    assert!(std::path::PathBuf::from(format!("{}.aria2", output.display())).exists());
    server.abort();
}

#[tokio::test]
async fn native_recovery_preserves_completed_history_when_job_is_absent() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("complete.bin");
    std::fs::write(&output, b"done").unwrap();
    let (client, server) = fixture::server(|request| match request["method"].as_str().unwrap() {
        "aria2.tellActive" | "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
        other => panic!("{other}"),
    })
    .await;
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct("https://example.test/complete.bin"))
        .unwrap();
    let mut task = store.bind_gid(task.id, "completed-native-gid").unwrap();
    task.status = TaskStatus::Completed;
    task.completed_length = 4;
    task.total_length = Some(4);
    store.apply_transfer_snapshot(task.clone()).unwrap();
    store
        .upsert_files(
            task.id,
            &[(1, output.to_string_lossy().into_owned(), 4, 4, true)],
        )
        .unwrap();

    let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();
    assert_eq!(report.removed_tasks, 0);
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Completed);
    assert!(output.exists());
    server.abort();
}

#[tokio::test]
async fn native_recovery_keeps_completed_history_when_engine_rechecks_moved_files() {
    for (kind, engine_status) in [
        ("magnet", "paused"),
        ("torrent", "paused"),
        ("ed2k", "paused"),
        ("file", "paused"),
        ("file", "complete"),
        ("cloud", "paused"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("tasks.sqlite");
        let gid = format!("completed-{kind}");
        let missing_output = root.path().join(format!("moved-{kind}.bin"));
        let task_id = {
            let store = TaskStore::open_path(&database).unwrap();
            let task = store
                .create(NewTask {
                    source: format!("{kind}://completed-fixture"),
                    name: format!("{kind}-fixture"),
                    kind: kind.into(),
                    is_private: false,
                })
                .unwrap();
            let mut task = store.bind_gid(task.id, &gid).unwrap();
            task.status = TaskStatus::Completed;
            task.progress = 100;
            task.total_length = Some(100);
            task.completed_length = 100;
            store.apply_transfer_snapshot(task.clone()).unwrap();
            store
                .upsert_files(
                    task.id,
                    &[(
                        1,
                        missing_output.to_string_lossy().into_owned(),
                        100,
                        100,
                        true,
                    )],
                )
                .unwrap();
            task.id
        };
        let removals = Arc::new(AtomicUsize::new(0));
        let observed_removals = Arc::clone(&removals);
        let reported_gid = gid.clone();
        let (client, server) =
            fixture::server(move |request| match request["method"].as_str().unwrap() {
                "aria2.tellActive" => json!([]),
                "aria2.tellWaiting" if engine_status == "paused" => {
                    json!([fixture::status(&reported_gid, engine_status, 0)])
                }
                "aria2.tellStopped" if engine_status == "complete" => {
                    json!([fixture::status(&reported_gid, engine_status, 0)])
                }
                "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
                "aria2.forceRemove" | "aria2.removeDownloadResult" => {
                    observed_removals.fetch_add(1, Ordering::SeqCst);
                    json!(reported_gid)
                }
                other => panic!("{other}"),
            })
            .await;
        let store = Arc::new(TaskStore::open_path(&database).unwrap());
        let service = DownloadService::new(Arc::new(client), Arc::clone(&store));

        service.recover_native(root.path()).await.unwrap();
        service.recover_native(root.path()).await.unwrap();

        let restored = store.find(task_id).unwrap();
        assert_eq!(
            restored.status,
            TaskStatus::Completed,
            "{kind}/{engine_status}"
        );
        assert_eq!(restored.progress, 100, "{kind}/{engine_status}");
        assert_eq!(restored.completed_length, 100, "{kind}/{engine_status}");
        assert_eq!(restored.aria_gid, None, "{kind}/{engine_status}");
        assert_eq!(store.list().unwrap().len(), 1, "{kind}/{engine_status}");
        assert!(
            removals.load(Ordering::SeqCst) > 0,
            "{kind}/{engine_status}"
        );
        server.abort();
    }
}

#[tokio::test]
async fn native_recovery_keeps_completed_progress_when_engine_reports_larger_total() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct("https://example.test/finished.bin"))
        .unwrap();
    let mut task = store.bind_gid(task.id, "grown-total-gid").unwrap();
    task.status = TaskStatus::Completed;
    task.progress = 100;
    task.total_length = Some(100);
    task.completed_length = 100;
    store.apply_transfer_snapshot(task.clone()).unwrap();
    let (client, server) = fixture::server(|request| match request["method"].as_str().unwrap() {
        "aria2.tellActive" | "aria2.tellWaiting" => json!([]),
        "aria2.tellStopped" => {
            let mut status = fixture::status("grown-total-gid", "complete", 100);
            status["totalLength"] = json!("200");
            json!([status])
        }
        "aria2.removeDownloadResult" => json!("grown-total-gid"),
        other => panic!("{other}"),
    })
    .await;

    DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();

    let restored = store.find(task.id).unwrap();
    assert_eq!(restored.status, TaskStatus::Completed);
    assert_eq!(restored.progress, 100);
    assert_eq!(restored.total_length, Some(100));
    assert_eq!(restored.aria_gid, None);
    server.abort();
}

#[tokio::test]
async fn native_recovery_keeps_completed_history_when_engine_snapshot_is_malformed() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask::direct("https://example.test/finished.bin"))
        .unwrap();
    let mut task = store.bind_gid(task.id, "malformed-complete-gid").unwrap();
    task.status = TaskStatus::Completed;
    task.progress = 100;
    task.total_length = Some(100);
    task.completed_length = 100;
    store.apply_transfer_snapshot(task.clone()).unwrap();
    let removals = Arc::new(AtomicUsize::new(0));
    let observed_removals = Arc::clone(&removals);
    let (client, server) =
        fixture::server(move |request| match request["method"].as_str().unwrap() {
            "aria2.tellActive" => json!([]),
            "aria2.tellWaiting" => {
                let mut status = fixture::status("malformed-complete-gid", "paused", 0);
                status["completedLength"] = json!("not-a-number");
                json!([status])
            }
            "aria2.tellStopped" => json!([]),
            "aria2.forceRemove" => {
                observed_removals.fetch_add(1, Ordering::SeqCst);
                json!("malformed-complete-gid")
            }
            other => panic!("{other}"),
        })
        .await;

    DownloadService::new(Arc::new(client), Arc::clone(&store))
        .recover_native(root.path())
        .await
        .unwrap();

    let restored = store.find(task.id).unwrap();
    assert_eq!(restored.status, TaskStatus::Completed);
    assert_eq!(restored.progress, 100);
    assert_eq!(restored.aria_gid, None);
    assert_eq!(removals.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn native_recovery_does_not_import_a_completed_magnet_child_after_bad_parent_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask {
            source: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
            name: "completed-magnet".into(),
            kind: "magnet".into(),
            is_private: false,
        })
        .unwrap();
    let mut task = store.bind_gid(task.id, "metadata-parent").unwrap();
    task.status = TaskStatus::Completed;
    task.progress = 100;
    task.total_length = Some(100);
    task.completed_length = 100;
    store.apply_transfer_snapshot(task.clone()).unwrap();
    let phase = Arc::new(AtomicUsize::new(0));
    let observed_phase = Arc::clone(&phase);
    let (client, server) =
        fixture::server(move |request| match request["method"].as_str().unwrap() {
            "aria2.tellActive" if observed_phase.load(Ordering::SeqCst) == 0 => {
                let mut parent = fixture::status("metadata-parent", "active", 100);
                parent["followedBy"] = json!(["payload-child"]);
                json!([parent])
            }
            "aria2.tellActive" => json!([fixture::status("payload-child", "active", 0)]),
            "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
            "aria2.tellStatus" => {
                let mut child = fixture::status("payload-child", "paused", 0);
                child["completedLength"] = json!("not-a-number");
                child
            }
            "aria2.forceRemove" => json!("removed"),
            other => panic!("{other}"),
        })
        .await;
    let service = DownloadService::new(Arc::new(client), Arc::clone(&store));

    service.recover_native(root.path()).await.unwrap();
    phase.store(1, Ordering::SeqCst);
    service.recover_native(root.path()).await.unwrap();

    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Completed);
    server.abort();
}


fn integrity_task(
    store: &TaskStore,
    state: TaskStatus,
    code: &str,
) -> versadown_lib::DownloadTask {
    let task = store.create(NewTask {
        source: "cloud://mega/AbCd1234".into(),
        name: "verified.bin".into(),
        kind: "cloud".into(),
        is_private: false,
    }).unwrap();
    let mut task = store.bind_gid(task.id, "mega-integrity-gid").unwrap();
    task.status = state;
    task.failure_code = Some(code.into());
    task.total_length = Some(100);
    task.completed_length = 100;
    task.progress = 99;
    store.apply_transfer_snapshot(task).unwrap()
}

#[tokio::test]
async fn native_recovery_preserves_persisted_mega_integrity_when_engine_state_is_stale_or_missing() {
    for (state, code) in [
        (TaskStatus::Processing, "mega_integrity_pending:fixture"),
        (TaskStatus::Failed, "mega_integrity_failed"),
        (TaskStatus::Failed, "mega_integrity_unavailable"),
    ] {
        for engine_state in [Some("active"), Some("paused"), Some("complete"), None] {
            let root = tempfile::tempdir().unwrap();
            let database = root.path().join("tasks.sqlite");
            let id = {
                let store = TaskStore::open_path(&database).unwrap();
                integrity_task(&store, state, code).id
            };
            // Exercise a new process's durable state, not an in-memory marker.
            let store = Arc::new(TaskStore::open_path(&database).unwrap());
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let (client, server) = fixture::server(move |request| match request["method"].as_str().unwrap() {
                "aria2.tellActive" => {
                    observed.fetch_add(1, Ordering::SeqCst);
                    match engine_state {
                        Some(value) => json!([fixture::status("mega-integrity-gid", value, 0)]),
                        None => json!([]),
                    }
                },
                "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
                other => panic!("integrity recovery must not call {other}"),
            }).await;
            let report = DownloadService::new(Arc::new(client), Arc::clone(&store))
                .recover_native(root.path()).await.unwrap();
            let task = store.find(id).unwrap();
            assert_eq!(task.status, state);
            assert_eq!(task.failure_code.as_deref(), Some(code));
            assert_eq!(task.aria_gid.as_deref(), Some("mega-integrity-gid"));
            assert_eq!(task.completed_length, 100);
            assert_eq!(report.reconciled, 0);
            assert_eq!(report.imported, 0);
            assert_eq!(calls.load(Ordering::SeqCst), 1, "no engine retry is needed for local verification");
            server.abort();
        }
    }
}

#[tokio::test]
async fn reconciliation_leaves_mega_integrity_to_the_verifier() {
    for (state, code) in [
        (TaskStatus::Processing, "mega_integrity_pending:fixture"),
        (TaskStatus::Failed, "mega_integrity_failed"),
        (TaskStatus::Failed, "mega_integrity_unavailable"),
    ] {
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let task = integrity_task(&store, state, code);
        let (client, server) = fixture::server(|request| match request["method"].as_str().unwrap() {
            "aria2.tellActive" => json!([fixture::status("mega-integrity-gid", "active", 0)]),
            "aria2.tellWaiting" => json!([]),
            "aria2.getGlobalStat" => fixture::global(),
            other => panic!("integrity task must not be synchronized by {other}"),
        }).await;
        let service = DownloadService::new(Arc::new(client), Arc::clone(&store));
        service.reconcile_now().await.unwrap();
        let observed = service.reconcile_task(task.id).await.unwrap();
        assert_eq!(observed.status, state);
        assert_eq!(observed.failure_code.as_deref(), Some(code));
        assert_eq!(observed.completed_length, 100);
        assert_eq!(store.list().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn an_in_flight_rpc_cannot_replace_a_new_mega_integrity_check() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let mut task = store.create(NewTask::direct("cloud://mega/AbCd1234")).unwrap();
    task = store.bind_gid(task.id, "mega-integrity-gid").unwrap();
    let id = task.id;
    let changed_store = Arc::clone(&store);
    let (client, server) = fixture::server(move |request| match request["method"].as_str().unwrap() {
        "aria2.tellStatus" => {
            let mut current = changed_store.find(id).unwrap();
            current.status = TaskStatus::Processing;
            current.failure_code = Some("mega_integrity_pending:fixture".into());
            current.total_length = Some(100);
            current.completed_length = 100;
            changed_store.apply_transfer_snapshot(current).unwrap();
            fixture::status("mega-integrity-gid", "paused", 0)
        },
        other => panic!("unexpected {other}"),
    }).await;
    let service = DownloadService::new(Arc::new(client), Arc::clone(&store));
    let current = service.reconcile_task(id).await.unwrap();
    assert_eq!(current.status, TaskStatus::Processing);
    assert_eq!(current.failure_code.as_deref(), Some("mega_integrity_pending:fixture"));
    assert_eq!(current.completed_length, 100);
    server.abort();
}


#[tokio::test]
async fn pending_mega_integrity_can_be_cancelled_without_a_live_engine_job() {
    use versadown_lib::aria2::DownloadAction;
    let root = tempfile::tempdir().unwrap();
    let payload = root.path().join("verified.bin");
    std::fs::write(&payload, b"downloaded bytes retained on cancel").unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = integrity_task(&store, TaskStatus::Processing, "mega_integrity_pending:fixture");
    store.upsert_files(task.id, &[(1, payload.to_string_lossy().into_owned(), 35, 35, true)]).unwrap();
    let (client, server) = fixture::server(|request| {
        panic!("local integrity control must not call aria2: {}", request["method"])
    }).await;
    let service = DownloadService::new(Arc::new(client), Arc::clone(&store));
    for action in [DownloadAction::Pause, DownloadAction::Resume] {
        assert!(service.control(task.id, action).await.is_err());
        assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Processing);
    }
    let cancelled = service.control(task.id, DownloadAction::Cancel).await.unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Cancelled);
    assert_eq!(std::fs::read(&payload).unwrap(), b"downloaded bytes retained on cancel");
    server.abort();
}
