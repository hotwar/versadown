use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use versadown_lib::{
    aria2::{
        Aria2Api, Aria2Error, Aria2File, Aria2GlobalStat, Aria2Options, Aria2Status, Aria2Uri,
        RpcCall, RpcFault,
    },
    cloud::{
        commands::{apply_cloud_directory_member_policy, apply_cloud_download_policy},
        CloudProvider,
    },
    prepare_download, DownloadAction, DownloadService, DownloadTask, EventSink, NewTask,
    TaskStatus, TaskStore, UriSubmissionOptions,
};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Default)]
struct RecordingAria2 {
    calls: Mutex<Vec<String>>,
    uris: Mutex<Vec<Vec<String>>>,
    changed_uris: Mutex<Vec<(Vec<String>, Vec<String>)>>,
    options: Mutex<Vec<Aria2Options>>,
    add_error: bool,
    pause_error: bool,
    status_error_message: Option<String>,
    status_override: Option<Aria2Status>,
    completed_status_error_message: Option<String>,
    cancel_release: Option<Arc<tokio::sync::Notify>>,
    next_gid: AtomicUsize,
}

impl RecordingAria2 {
    fn successful() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn record(&self, method: &str) {
        self.calls.lock().unwrap().push(method.to_string());
    }
    fn added_options(&self) -> Vec<Aria2Options> {
        self.options.lock().unwrap().clone()
    }
    fn changed_uris(&self) -> Vec<(Vec<String>, Vec<String>)> {
        self.changed_uris.lock().unwrap().clone()
    }
    fn active(gid: &str) -> Aria2Status {
        Aria2Status {
            gid: gid.to_string(),
            status: "active".to_string(),
            total_length: "1000".to_string(),
            completed_length: "250".to_string(),
            download_speed: "50".to_string(),
            upload_speed: "0".to_string(),
            connections: "2".to_string(),
            files: vec![],
            bittorrent: None,
            followed_by: vec![],
            error_code: None,
            error_message: None,
        }
    }
}

#[derive(Default)]
struct RecordedEvents {
    tasks: Mutex<Vec<DownloadTask>>,
}

impl EventSink for RecordedEvents {
    fn task_updated(&self, task: &DownloadTask) {
        self.tasks.lock().unwrap().push(task.clone());
    }

    fn global_stat_updated(&self, _: &Aria2GlobalStat) {}
}

fn prepared_cloud_share_task(store: &TaskStore, source: &str, name: &str) -> DownloadTask {
    store
        .create(NewTask {
            source: source.into(),
            name: name.into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap()
}

#[tokio::test]
async fn failed_cloud_url_refresh_rebinds_native_checkpoint_to_a_stable_gateway_uri() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_path(&directory.path().join("tasks.sqlite")).unwrap());
    let task = store
        .create(NewTask {
            source: "cloud://pikpak/file-id".into(),
            name: "file.bin".into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap();
    let task = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Queued,
            aria_gid: Some("0123456789abcdef".into()),
            failure_code: Some("cloud_url_refreshing".into()),
            completed_length: 128,
            total_length: Some(1_024),
            ..task
        })
        .unwrap();
    let payload = directory.path().join("file.bin");
    std::fs::File::create(&payload)
        .unwrap()
        .set_len(1_024)
        .unwrap();
    store
        .upsert_files(
            task.id,
            &[(0, payload.to_string_lossy().into_owned(), 1_024, 128, true)],
        )
        .unwrap();
    let state_directory = directory.path().join("aria2-next-state/stream");
    std::fs::create_dir_all(&state_directory).unwrap();
    let checkpoint = rusqlite::Connection::open(state_directory.join("state.db")).unwrap();
    checkpoint
        .execute_batch("CREATE TABLE downloads (gid TEXT PRIMARY KEY, uri TEXT, path TEXT, total_length INTEGER, completed_length INTEGER, completed_ranges TEXT);")
        .unwrap();
    checkpoint
        .execute(
            "INSERT INTO downloads VALUES (?1, ?2, ?3, 1024, 128, '0-128')",
            rusqlite::params![
                "0123456789abcdef",
                "https://expired.example.test/file",
                payload.to_string_lossy(),
            ],
        )
        .unwrap();
    let mut status = RecordingAria2::active("0123456789abcdef");
    status.status = "error".into();
    status.error_code = Some("8".into());
    status.error_message = Some("The requested byte range is no longer satisfiable".into());
    let api = Arc::new(RecordingAria2 {
        status_override: Some(status),
        ..Default::default()
    });
    let service = DownloadService::new(api.clone(), Arc::clone(&store));

    let refreshed = service
        .replace_cloud_uri_with_options(
            task.id,
            "0123456789abcdef",
            vec!["http://127.0.0.1:49152/stable-token".into()],
            UriSubmissionOptions {
                directory: Some(directory.path().to_path_buf()),
                output_name: Some("file.bin".into()),
                continue_download: true,
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(refreshed.aria_gid.as_deref(), Some("gid-1"));
    assert!(
        api.calls().iter().any(|call| call == "aria2.addUri"),
        "a failed GID must submit its stable replacement URI"
    );
    assert_eq!(
        api.uris.lock().unwrap()[0],
        vec!["http://127.0.0.1:49152/stable-token"],
        "the expired CDN URI must not be submitted as an alternate mirror"
    );
}

#[tokio::test]
async fn unconfirmed_magnet_metadata_is_not_published_to_the_queue() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let events = Arc::new(RecordedEvents::default());
    let api = RecordingAria2::successful();
    let task = store
        .create(NewTask {
            source: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
            name: "pending magnet".into(),
            kind: "magnet".into(),
            is_private: false,
        })
        .unwrap();
    let service = DownloadService::new(api.clone(), Arc::clone(&store)).with_events(events.clone());

    let prepared = service
        .prepare_magnet(task.id, task.source.clone())
        .await
        .unwrap();

    assert_eq!(prepared.status, TaskStatus::AwaitingUser);
    assert!(events.tasks.lock().unwrap().is_empty());
    assert_eq!(
        api.added_options()[0]
            .get("follow-torrent")
            .map(String::as_str),
        Some("true")
    );
}

#[async_trait]
impl Aria2Api for RecordingAria2 {
    async fn add_uri(
        &self,
        uris: Vec<String>,
        options: Aria2Options,
    ) -> Result<String, Aria2Error> {
        self.record("aria2.addUri");
        self.uris.lock().unwrap().push(uris);
        self.options.lock().unwrap().push(options);
        if self.add_error {
            Err(Aria2Error::Transport)
        } else {
            Ok(format!(
                "gid-{}",
                self.next_gid.fetch_add(1, Ordering::Relaxed) + 1
            ))
        }
    }
    async fn tell_status(&self, gid: &str) -> Result<Aria2Status, Aria2Error> {
        self.record("aria2.tellStatus");
        if let Some(status) = &self.status_override {
            let mut status = status.clone();
            status.gid = gid.to_string();
            return Ok(status);
        }
        let mut status = Self::active(gid);
        if let Some(message) = &self.status_error_message {
            status.status = "error".into();
            status.error_code = Some("22".into());
            status.error_message = Some(message.clone());
        } else if let Some(message) = &self.completed_status_error_message {
            status.status = "complete".into();
            status.error_code = Some("22".into());
            status.error_message = Some(message.clone());
        }
        Ok(status)
    }
    async fn pause(&self, _gid: &str) -> Result<String, Aria2Error> {
        self.record("aria2.pause");
        if self.pause_error {
            Err(Aria2Error::Transport)
        } else {
            Ok("gid-1".to_string())
        }
    }
    async fn unpause(&self, gid: &str) -> Result<String, Aria2Error> {
        self.record("aria2.unpause");
        Ok(gid.to_string())
    }
    async fn remove(&self, gid: &str) -> Result<String, Aria2Error> {
        self.record("aria2.remove");
        if let Some(release) = &self.cancel_release {
            release.notified().await;
        }
        Ok(gid.to_string())
    }
    async fn force_remove(&self, gid: &str) -> Result<String, Aria2Error> {
        self.record("aria2.forceRemove");
        Ok(gid.to_string())
    }
    async fn add_torrent(
        &self,
        _: String,
        _: Vec<String>,
        _: Aria2Options,
    ) -> Result<String, Aria2Error> {
        unimplemented!()
    }
    async fn tell_active(&self) -> Result<Vec<Aria2Status>, Aria2Error> {
        unimplemented!()
    }
    async fn tell_waiting(&self, _: i64, _: u32) -> Result<Vec<Aria2Status>, Aria2Error> {
        unimplemented!()
    }
    async fn tell_stopped(&self, _: i64, _: u32) -> Result<Vec<Aria2Status>, Aria2Error> {
        unimplemented!()
    }
    async fn get_global_stat(&self) -> Result<Aria2GlobalStat, Aria2Error> {
        unimplemented!()
    }
    async fn multicall(&self, _: Vec<RpcCall>) -> Result<Vec<Result<Value, RpcFault>>, Aria2Error> {
        unimplemented!()
    }
    async fn change_option(
        &self,
        _: &str,
        _: BTreeMap<String, String>,
    ) -> Result<String, Aria2Error> {
        self.record("aria2.changeOption");
        Ok("OK".into())
    }
    async fn change_uri(
        &self,
        _: &str,
        _: u32,
        del_uris: Vec<String>,
        add_uris: Vec<String>,
        _: u32,
    ) -> Result<(u32, u32), Aria2Error> {
        self.record("aria2.changeUri");
        self.changed_uris
            .lock()
            .unwrap()
            .push((del_uris.clone(), add_uris.clone()));
        Ok((del_uris.len() as u32, add_uris.len() as u32))
    }
    async fn get_option(&self, _: &str) -> Result<BTreeMap<String, String>, Aria2Error> {
        unimplemented!()
    }
    async fn save_session(&self) -> Result<String, Aria2Error> {
        unimplemented!()
    }
    async fn shutdown(&self) -> Result<String, Aria2Error> {
        unimplemented!()
    }
}

#[test]
fn cloud_download_connection_policy_caps_xunlei_user_value_of_eight_to_one() {
    let options =
        apply_cloud_download_policy(CloudProvider::Xunlei, 8, UriSubmissionOptions::default());

    assert_eq!(options.max_connections_per_server, Some(1));
}

fn native_resume_fixture() -> (tempfile::TempDir, Arc<TaskStore>, DownloadTask) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_path(&root.path().join("tasks.sqlite")).unwrap());
    let task = store
        .create(NewTask::direct("https://example.test/old"))
        .unwrap();
    let task = store.bind_gid(task.id, "0123456789abcdef").unwrap();
    let task = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Failed,
            total_length: Some(128),
            completed_length: 7,
            ..task
        })
        .unwrap();
    let payload = root.path().join("native.bin");
    std::fs::File::create(&payload)
        .unwrap()
        .set_len(128)
        .unwrap();
    store
        .upsert_files(
            task.id,
            &[(0, payload.to_string_lossy().into_owned(), 128, 7, true)],
        )
        .unwrap();
    let directory = root.path().join("aria2-next-state/stream");
    std::fs::create_dir_all(&directory).unwrap();
    let db = rusqlite::Connection::open(directory.join("state.db")).unwrap();
    db.execute_batch("CREATE TABLE downloads (gid TEXT PRIMARY KEY, uri TEXT, path TEXT, total_length INTEGER, completed_length INTEGER, completed_ranges TEXT);").unwrap();
    db.execute(
        "INSERT INTO downloads VALUES (?1, ?2, ?3, 128, 7, '0-7')",
        rusqlite::params![
            "0123456789abcdef",
            "https://example.test/old",
            payload.to_string_lossy()
        ],
    )
    .unwrap();
    (root, store, task)
}

#[tokio::test]
async fn native_resume_preserves_checkpoint_identity_and_disables_length_fallback() {
    let (root, store, task) = native_resume_fixture();
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store);
    service
        .resubmit_uri_with_options(
            task.id,
            vec!["https://example.test/fresh".into()],
            UriSubmissionOptions {
                continue_download: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let options = api.added_options();
    assert_eq!(
        options[0].get("continue").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        options[0].get("allow-overwrite").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        options[0].get("auto-file-renaming").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        options[0].get("out").map(String::as_str),
        Some("native.bin")
    );
    assert_eq!(
        options[0].get("dir"),
        Some(&root.path().to_string_lossy().into_owned())
    );
    assert_eq!(
        api.uris.lock().unwrap()[0],
        vec!["https://example.test/fresh", "https://example.test/old"]
    );
}

#[tokio::test]
async fn paused_cloud_refresh_removes_old_gateway_uri_without_a_native_checkpoint() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let old_url = "http://127.0.0.1:49152/old-token";
    let new_url = "http://127.0.0.1:49152/new-token";
    let task = store
        .create(NewTask {
            source: "cloud://baidu/file-id".into(),
            name: "file.bin".into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap();
    let task = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Queued,
            aria_gid: Some("0123456789abcdef".into()),
            failure_code: Some("cloud_url_refreshing".into()),
            total_length: Some(1_000),
            completed_length: 250,
            progress: 25,
            ..task
        })
        .unwrap();
    let mut status = RecordingAria2::active("0123456789abcdef");
    status.status = "paused".into();
    status.files = vec![Aria2File {
        index: "1".into(),
        path: "file.bin".into(),
        length: "1000".into(),
        completed_length: "250".into(),
        selected: "true".into(),
        uris: vec![Aria2Uri {
            uri: old_url.into(),
            status: "used".into(),
        }],
    }];
    let api = Arc::new(RecordingAria2 {
        status_override: Some(status),
        ..Default::default()
    });
    let service = DownloadService::new(api.clone(), store);

    service
        .replace_cloud_uri_with_options(
            task.id,
            "0123456789abcdef",
            vec![new_url.into()],
            UriSubmissionOptions {
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        api.changed_uris(),
        vec![(vec![old_url.into()], vec![new_url.into()])]
    );
}

#[tokio::test]
async fn failed_cloud_snapshot_without_lengths_keeps_verified_download_progress() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask {
            source: "cloud://baidu/file-id".into(),
            name: "file.bin".into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap();
    let task = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Downloading,
            aria_gid: Some("0123456789abcdef".into()),
            total_length: Some(1_000),
            completed_length: 700,
            progress: 70,
            ..task
        })
        .unwrap();
    let mut status = RecordingAria2::active("0123456789abcdef");
    status.status = "error".into();
    status.total_length = "0".into();
    status.completed_length = "0".into();
    status.error_code = Some("13".into());
    status.error_message = Some("File already exists".into());
    let api = Arc::new(RecordingAria2 {
        status_override: Some(status),
        ..Default::default()
    });
    let service = DownloadService::new(api, Arc::clone(&store));

    let failed = service.reconcile_task(task.id).await.unwrap();

    assert_eq!(failed.status, TaskStatus::Failed);
    assert_eq!(failed.progress, 70);
    assert_eq!(failed.completed_length, 700);
    assert_eq!(failed.total_length, Some(1_000));
    assert_eq!(store.find(task.id).unwrap().completed_length, 700);
}

#[tokio::test]
async fn baidu_gateway_resume_places_fresh_local_uri_before_checkpoint_mirror() {
    let (_root, store, task) = native_resume_fixture();
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store);
    let loopback = "http://127.0.0.1:49152/fresh-token";
    service
        .resubmit_uri_with_options(
            task.id,
            vec![loopback.into()],
            UriSubmissionOptions {
                continue_download: true,
                direct_loopback: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        api.uris.lock().unwrap()[0],
        vec![loopback, "https://example.test/old"]
    );
    assert!(api.changed_uris().is_empty());
    assert_eq!(
        api.added_options()[0].get("pause").map(String::as_str),
        Some("false")
    );
}

#[tokio::test]
async fn native_resume_rejects_disappeared_checkpoint_without_submitting_or_touching_payload() {
    let (root, store, task) = native_resume_fixture();
    std::fs::remove_file(root.path().join("aria2-next-state/stream/state.db")).unwrap();
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    for native_resume_gid in [task.aria_gid.clone(), None] {
        let result = service
            .resubmit_uri_with_options(
                task.id,
                vec!["https://example.test/fresh".into()],
                UriSubmissionOptions {
                    continue_download: true,
                    native_resume_gid,
                    ..Default::default()
                },
            )
            .await;
        assert!(result.is_err());
    }
    assert!(api.calls().is_empty());
    assert_eq!(
        std::fs::metadata(root.path().join("native.bin"))
            .unwrap()
            .len(),
        128
    );
    assert_eq!(store.find(task.id).unwrap().aria_gid, task.aria_gid);
}

#[tokio::test]
async fn native_resume_identity_survives_cloud_admission_pause() {
    use versadown_lib::cloud::download_dispatch::CloudDownloadDispatch;
    let (_root, store, task) = native_resume_fixture();
    let dispatch = CloudDownloadDispatch::default();
    dispatch
        .prepare_expected_with_retry_intent(&store, &task, false, true)
        .await
        .unwrap();
    let queued = store.find(task.id).unwrap();
    assert!(queued.aria_gid.is_none());
    dispatch
        .control_unsubmitted(&store, task.id, DownloadAction::Pause)
        .await
        .unwrap();
    let options = dispatch.retry_submission_options(task.id, UriSubmissionOptions::default());
    assert_eq!(options.native_resume_gid, task.aria_gid);
    assert!(options.continue_download);
}

#[test]
fn cloud_download_connection_policy_retains_user_value_below_provider_cap() {
    let options =
        apply_cloud_download_policy(CloudProvider::Baidu, 2, UriSubmissionOptions::default());

    assert_eq!(options.max_connections_per_server, Some(2));
}

#[test]
fn baidu_directory_members_use_configured_connections_up_to_provider_cap() {
    let selected = apply_cloud_directory_member_policy(
        CloudProvider::Baidu,
        2,
        UriSubmissionOptions::default(),
    );
    let capped = apply_cloud_directory_member_policy(
        CloudProvider::Baidu,
        8,
        UriSubmissionOptions::default(),
    );

    assert_eq!(selected.max_connections_per_server, Some(2));
    assert_eq!(capped.max_connections_per_server, Some(4));
}

#[test]
fn other_cloud_directory_members_keep_one_connection() {
    let options = apply_cloud_directory_member_policy(
        CloudProvider::Aliyun,
        8,
        UriSubmissionOptions::default(),
    );

    assert_eq!(options.max_connections_per_server, Some(1));
}

#[tokio::test]
async fn uri_downloads_keep_torrent_files_as_regular_files() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let http = prepare_download("https://example.test/release.torrent", &store)
        .unwrap()
        .task;
    let ftp = prepare_download("ftp://example.test/release.torrent", &store)
        .unwrap()
        .task;
    assert_eq!(http.kind, "file");
    assert_eq!(ftp.kind, "ftp");

    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store);
    service
        .submit_uri_with_options(http.id, vec![http.source], UriSubmissionOptions::default())
        .await
        .unwrap();

    assert_eq!(
        api.added_options()[0]
            .get("follow-torrent")
            .map(String::as_str),
        Some("false")
    );
}

#[tokio::test]
async fn independently_sourced_downloads_reserve_distinct_output_paths() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let first = prepare_download("https://one.example.test/releases/archive.zip", &store)
        .unwrap()
        .task;
    let second = prepare_download("https://two.example.test/releases/archive.zip", &store)
        .unwrap()
        .task;
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let options = || UriSubmissionOptions {
        directory: Some(directory.path().to_path_buf()),
        ..Default::default()
    };

    let first = service
        .submit_uri_with_options(first.id, vec![first.source], options())
        .await
        .unwrap();
    let second = service
        .submit_uri_with_options(second.id, vec![second.source], options())
        .await
        .unwrap();

    assert_eq!(first.name, "archive.zip");
    assert_eq!(second.name, "archive (1).zip");
    assert_eq!(
        api.added_options()
            .iter()
            .map(|options| options.get("out").map(String::as_str))
            .collect::<Vec<_>>(),
        vec![Some("archive.zip"), Some("archive (1).zip")]
    );
    assert!(api
        .added_options()
        .iter()
        .all(|options| options.get("auto-file-renaming").map(String::as_str) == Some("false")));
}

#[tokio::test]
async fn cloud_download_policy_sends_capped_connections_and_spaced_bounded_retries() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = prepare_download("https://example.test/cloud-file.bin", &store)
        .unwrap()
        .task;
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store);
    let options =
        apply_cloud_download_policy(CloudProvider::Xunlei, 8, UriSubmissionOptions::default());

    service
        .submit_uri_with_options(task.id, vec![task.source], options)
        .await
        .unwrap();

    assert_eq!(
        api.added_options(),
        vec![BTreeMap::from([
            ("auto-file-renaming".into(), "false".into()),
            ("follow-torrent".into(), "false".into()),
            ("max-connection-per-server".into(), "1".into()),
            ("max-tries".into(), "5".into()),
            ("pause".into(), "false".into()),
            ("retry-wait".into(), "3".into()),
            ("split".into(), "1".into()),
        ])]
    );
}

#[tokio::test]
async fn scheduler_shared_personal_share_defers_resolver_and_other_provider_is_independent() {
    use versadown_lib::cloud::download_dispatch::CloudDownloadDispatch;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dispatch = CloudDownloadDispatch::default();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let mut jobs = Vec::new();
    let mut ids = Vec::new();
    for (index, provider) in [
        CloudProvider::Quark,
        CloudProvider::Quark,
        CloudProvider::Quark,
        CloudProvider::Quark,
        CloudProvider::Quark,
        CloudProvider::Xunlei,
    ]
    .into_iter()
    .enumerate()
    {
        let source = if index % 2 == 0 {
            format!("cloud://{}/file-{index}", provider.key())
        } else {
            versadown_lib::cloud::share::stable_share_source(
                provider,
                "plan",
                &format!("entry-{index}"),
            )
        };
        let task = prepared_cloud_share_task(&store, &source, "file.bin");
        let (queued, version) = dispatch.prepare(&store, task.id).await.unwrap();
        assert_eq!(queued.status, TaskStatus::Queued);
        assert!(queued.aria_gid.is_none());
        ids.push(task.id);
        let (dispatch, store, count) = (dispatch.clone(), store.clone(), count.clone());
        jobs.push(tokio::spawn(async move {
            dispatch
                .run(
                    &store,
                    provider,
                    task.id,
                    version,
                    || async {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                    |_| async {
                        let task = store.bind_gid(task.id, &format!("gid-{index}")).unwrap();
                        Ok(task)
                    },
                )
                .await
        }));
        tokio::task::yield_now().await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while count.load(Ordering::SeqCst) != 5
            || dispatch.scheduler.pending(CloudProvider::Quark).is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        dispatch.scheduler.pending(CloudProvider::Quark),
        vec![ids[4]]
    );
    assert_eq!(count.load(Ordering::SeqCst), 5);
    dispatch.scheduler.release(ids[0]);
    for job in jobs {
        job.await.unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn scheduler_gidless_pause_cancel_resume_never_dispatches_stale_work() {
    use versadown_lib::cloud::download_dispatch::CloudDownloadDispatch;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dispatch = CloudDownloadDispatch::default();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = prepared_cloud_share_task(&store, "cloud://quark/file", "file.bin");
    let (_, old_version) = dispatch.prepare(&store, task.id).await.unwrap();
    let paused = dispatch
        .control_unsubmitted(&store, task.id, DownloadAction::Pause)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.status, TaskStatus::Paused);
    let count = AtomicUsize::new(0);
    dispatch
        .run(
            &store,
            CloudProvider::Quark,
            task.id,
            old_version,
            || async {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| async { panic!("stale dispatch reached aria2") },
        )
        .await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let (_, version) = dispatch.prepare(&store, task.id).await.unwrap();
    let cancelled = dispatch
        .control_unsubmitted(&store, task.id, DownloadAction::Cancel)
        .await
        .unwrap()
        .unwrap();
    dispatch
        .run(
            &store,
            CloudProvider::Quark,
            task.id,
            version,
            || async {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| async { panic!("cancelled dispatch reached aria2") },
        )
        .await;
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let (_, version) = dispatch.prepare(&store, task.id).await.unwrap();
    dispatch
        .run(
            &store,
            CloudProvider::Quark,
            task.id,
            version,
            || async {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| async { Ok(store.bind_gid(task.id, "resumed").unwrap()) },
        )
        .await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.find(task.id).unwrap().aria_gid.as_deref(),
        Some("resumed")
    );
}

#[tokio::test]
async fn scheduler_paused_refresh_retains_checkpoint_and_confirmed_overwrite_intent() {
    use versadown_lib::cloud::download_dispatch::CloudDownloadDispatch;
    let dispatch = CloudDownloadDispatch::default();
    let store = TaskStore::open_in_memory().unwrap();
    let task = prepared_cloud_share_task(&store, "cloud://quark/file", "file.bin");
    let failed = store
        .fail(task.id, "preallocated output is not resumable")
        .unwrap();
    let failed = store.bind_gid(failed.id, "stale-gid").unwrap();
    dispatch
        .prepare_expected_with_retry_intent(&store, &failed, true, false)
        .await
        .unwrap();
    let paused = dispatch
        .control_unsubmitted(&store, task.id, DownloadAction::Pause)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.aria_gid.as_deref(), Some("stale-gid"));

    let options = dispatch.retry_submission_options(
        task.id,
        UriSubmissionOptions {
            continue_download: true,
            ..Default::default()
        },
    );

    assert!(options.allow_overwrite);
    assert!(
        !options.continue_download,
        "a resumed pre-admission overwrite must still discard stale aria2 state"
    );
}

#[tokio::test]
async fn failed_share_resume_dispatch_restores_original_gid_and_resume_eligibility() {
    use versadown_lib::cloud::{
        download_dispatch::CloudDownloadDispatch, share::stable_share_source,
    };
    let root = tempfile::tempdir().unwrap();
    let dispatch = CloudDownloadDispatch::default();
    let store = TaskStore::open_in_memory().unwrap();
    let source = stable_share_source(CloudProvider::Quark, "plan", "entry");
    let task = prepared_cloud_share_task(&store, &source, "partial.bin");
    let failed = store
        .apply_transfer_snapshot(DownloadTask {
            status: TaskStatus::Failed,
            total_length: Some(100),
            completed_length: 25,
            ..task
        })
        .unwrap();
    let failed = store.bind_gid(failed.id, "resume-gid").unwrap();
    let payload = root.path().join("partial.bin");
    std::fs::write(&payload, b"partial").unwrap();
    std::fs::write(format!("{}.aria2", payload.display()), b"control").unwrap();
    store
        .upsert_files(
            failed.id,
            &[(0, payload.to_string_lossy().into_owned(), 100, 25, true)],
        )
        .unwrap();
    let (_, version) = dispatch.prepare(&store, failed.id).await.unwrap();

    dispatch
        .run(
            &store,
            CloudProvider::Quark,
            failed.id,
            version,
            || async {
                Err::<(), _>(versadown_lib::cloud::QuarkError::Transport(
                    "temporary resolver failure".into(),
                ))
            },
            |_: ()| async { panic!("failed resolution must not submit") },
        )
        .await;

    let failed_again = store.find(failed.id).unwrap();
    assert_eq!(failed_again.aria_gid.as_deref(), Some("resume-gid"));
    assert_eq!(
        versadown_lib::duplicate_state(&store, &failed_again).unwrap(),
        versadown_lib::DuplicateDownloadState::ResumeAvailable
    );
}

#[tokio::test]
async fn scheduler_pause_during_resolution_releases_slot_and_resume_uses_a_fresh_resolver() {
    use versadown_lib::cloud::download_dispatch::CloudDownloadDispatch;
    let dispatch = CloudDownloadDispatch::default();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = prepared_cloud_share_task(&store, "cloud://xunlei/file", "file.bin");
    let (_, version) = dispatch.prepare(&store, task.id).await.unwrap();
    let (started, start) = tokio::sync::oneshot::channel();
    let (worker, tasks) = (dispatch.clone(), store.clone());
    let job = tokio::spawn(async move {
        worker
            .run(
                &tasks,
                CloudProvider::Xunlei,
                task.id,
                version,
                || async {
                    started.send(()).unwrap();
                    std::future::pending::<Result<(), versadown_lib::cloud::QuarkError>>().await
                },
                |_| async { panic!("paused resolver must not submit") },
            )
            .await
    });
    start.await.unwrap();
    dispatch
        .control_unsubmitted(&store, task.id, DownloadAction::Pause)
        .await
        .unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(2), job)
        .await
        .unwrap()
        .unwrap()
        .is_none());
    let (_, version) = dispatch.prepare(&store, task.id).await.unwrap();
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    dispatch
        .run(
            &store,
            CloudProvider::Xunlei,
            task.id,
            version,
            || async { Ok(()) },
            |_| async {
                service
                    .submit_uri_with_options(
                        task.id,
                        vec!["https://example.test/fresh".into()],
                        apply_cloud_download_policy(
                            CloudProvider::Xunlei,
                            8,
                            UriSubmissionOptions {
                                continue_download: true,
                                proxy: Some("http://proxy.test:1234".into()),
                                ..Default::default()
                            },
                        ),
                    )
                    .await
                    .map_err(|_| versadown_lib::cloud::QuarkError::Storage)
            },
        )
        .await;
    let options = api.added_options();
    assert_eq!(options[0].get("continue").map(String::as_str), Some("true"));
    assert_eq!(options[0].get("split").map(String::as_str), Some("1"));
    assert_eq!(
        options[0]
            .get("max-connection-per-server")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        options[0].get("all-proxy").map(String::as_str),
        Some("http://proxy.test:1234")
    );
}

#[tokio::test]
async fn scheduler_failure_releases_slot_redacts_error_and_accepted_rpc_retains_slot() {
    use versadown_lib::cloud::{
        download_dispatch::CloudDownloadDispatch, download_scheduler::AdmissionOutcome, QuarkError,
    };
    let dispatch = CloudDownloadDispatch::default();
    let store = TaskStore::open_in_memory().unwrap();
    let task = prepared_cloud_share_task(&store, "cloud://xunlei/file", "file.bin");
    let (_, version) = dispatch.prepare(&store, task.id).await.unwrap();
    dispatch
        .run(
            &store,
            CloudProvider::Xunlei,
            task.id,
            version,
            || async {
                Err::<(), _>(QuarkError::Transport(
                    "https://secret?token=hidden Authorization: secret".into(),
                ))
            },
            |_| async { panic!("failed resolution cannot submit") },
        )
        .await;
    let failed = store.find(task.id).unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(!failed.error.unwrap().contains("secret"));
    let probe = uuid::Uuid::new_v4();
    assert_eq!(
        dispatch
            .scheduler
            .enqueue(CloudProvider::Xunlei, probe)
            .await,
        AdmissionOutcome::Granted
    );
    dispatch.scheduler.release(probe);
    let (_, version) = dispatch.prepare(&store, task.id).await.unwrap();
    dispatch
        .run(
            &store,
            CloudProvider::Xunlei,
            task.id,
            version,
            || async { Ok(()) },
            |_| async {
                store.bind_gid(task.id, "accepted").unwrap();
                Err(QuarkError::Transport("status polling failed".into()))
            },
        )
        .await;
    assert!(store.find(task.id).unwrap().aria_gid.is_some());
    assert!(
        dispatch.scheduler.release(task.id),
        "accepted aria2 task must still own a provider slot"
    );
    assert!(
        !dispatch.scheduler.release(task.id),
        "terminal release is idempotent"
    );
    assert_eq!(
        dispatch
            .scheduler
            .enqueue(CloudProvider::Xunlei, probe)
            .await,
        AdmissionOutcome::Granted
    );
}

#[tokio::test]
async fn scheduler_queued_share_task_can_submit_after_admission() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{stable_share_source, submit_current_share_download},
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = prepared_cloud_share_task(
        &store,
        &stable_share_source(CloudProvider::Quark, "plan", "entry"),
        "file.bin",
    );
    let mut task = task;
    task.status = TaskStatus::Queued;
    let task = store.apply_transfer_snapshot(task).unwrap();
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store);
    let result = submit_current_share_download(
        &service,
        &task,
        ResolvedCloudDownload {
            url: "https://example.test/file?secret=ephemeral".into(),
            name: "file.bin".into(),
            headers: vec![],
        },
        Default::default(),
    )
    .await;
    assert!(
        result.is_ok(),
        "queued task must be accepted after admission: {result:?}"
    );
    assert_eq!(
        api.calls()
            .iter()
            .filter(|call| *call == "aria2.addUri")
            .count(),
        1
    );
}

#[test]
fn scheduler_queued_resolution_failure_is_retryable() {
    let store = TaskStore::open_in_memory().unwrap();
    let mut task = prepared_cloud_share_task(&store, "cloud://quark/file", "file.bin");
    task.status = TaskStatus::Queued;
    let task = store.apply_transfer_snapshot(task).unwrap();
    assert!(store.fail(task.id, "网盘下载准备失败").is_ok());
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Failed);
}

#[tokio::test]
async fn cloud_share_submission_keeps_signed_url_and_headers_out_of_task_store() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{stable_share_source, submit_current_share_download},
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "private-share", "private-file");
    let prepared = prepared_cloud_share_task(&store, &source, "video.mp4");
    let task = submit_current_share_download(
        &service,
        &prepared,
        ResolvedCloudDownload {
            url: "https://cdn.example/video?signature=private-signature".into(),
            name: "video.mp4".into(),
            headers: vec![("x-share-token".into(), "private-header".into())],
        },
        UriSubmissionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        api.uris.lock().unwrap()[0],
        vec!["https://cdn.example/video?signature=private-signature"]
    );
    assert_eq!(
        api.added_options()[0]["header"],
        "x-share-token: private-header"
    );
    assert_eq!(task.source, source);
    let saved = serde_json::to_string(&store.find(task.id).unwrap()).unwrap();
    for secret in [
        "private-share",
        "private-file",
        "private-signature",
        "private-header",
        "cdn.example",
    ] {
        assert!(!saved.contains(secret));
    }
}

#[tokio::test]
async fn legacy_invalid_magnet_never_reaches_aria2() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let task = store
        .create(NewTask {
            source: "magnet:?xt=urn:btih:short".into(),
            name: "legacy magnet".into(),
            kind: "magnet".into(),
            is_private: false,
        })
        .unwrap();

    let error = service
        .prepare_magnet_with_options(
            task.id,
            task.source.clone(),
            UriSubmissionOptions::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "磁链格式无效");
    assert!(api.calls().is_empty());
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Failed);
}

#[tokio::test]
async fn legacy_invalid_direct_source_never_reaches_aria2() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let task = store
        .create(NewTask {
            source: "magnet:?xt=urn:btih:short".into(),
            name: "legacy direct".into(),
            kind: "file".into(),
            is_private: false,
        })
        .unwrap();

    let error = service
        .submit_uri_with_options(
            task.id,
            vec![task.source.clone()],
            UriSubmissionOptions::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "磁链格式无效");
    assert!(api.calls().is_empty());
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Failed);
}

#[tokio::test]
async fn legacy_cloud_share_page_never_reaches_aria2_as_a_direct_file() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let task = store
        .create(NewTask {
            source: "https://pan.baidu.com/s/share_123".into(),
            name: "legacy cloud page".into(),
            kind: "file".into(),
            is_private: false,
        })
        .unwrap();

    let error = service
        .submit_uri_with_options(
            task.id,
            vec![task.source.clone()],
            UriSubmissionOptions::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.to_string(), "未发现受支持的有效下载链接");
    assert!(api.calls().is_empty());
    assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Failed);
}

#[tokio::test]
async fn cloud_share_submission_rejects_share_page_before_task_creation_or_aria2() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{stable_share_source, submit_current_share_download},
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let prepared = prepared_cloud_share_task(&store, &source, "file");
    let result = submit_current_share_download(
        &service,
        &prepared,
        ResolvedCloudDownload {
            url: "https://www.aliyundrive.com/s/share".into(),
            name: "file".into(),
            headers: vec![],
        },
        UriSubmissionOptions::default(),
    )
    .await;
    assert!(result.is_err());
    assert!(api.calls().is_empty());
    assert_eq!(store.find(prepared.id).unwrap().status, TaskStatus::Parsing);
}

#[tokio::test]
async fn cloud_share_aria_failure_redacts_signed_urls_but_retains_http_error_code() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{stable_share_source, submit_current_share_download},
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = Arc::new(RecordingAria2 {
        status_error_message: Some(
            "HTTP failure https://cdn.example/file?signature=secret-token".into(),
        ),
        ..Default::default()
    });
    let service = DownloadService::new(api, store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let prepared = prepared_cloud_share_task(&store, &source, "file");
    let task = submit_current_share_download(
        &service,
        &prepared,
        ResolvedCloudDownload {
            url: "https://cdn.example/file?signature=secret-token".into(),
            name: "file".into(),
            headers: vec![],
        },
        UriSubmissionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.aria_error_code.as_deref(), Some("22"));
    assert!(!task.error.as_ref().unwrap().contains("secret-token"));
    assert!(!serde_json::to_string(&store.find(task.id).unwrap())
        .unwrap()
        .contains("cdn.example"));
}

#[tokio::test]
async fn cloud_share_completion_discards_a_stale_failure_message() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{stable_share_source, submit_current_share_download},
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = Arc::new(RecordingAria2 {
        completed_status_error_message: Some("a stale aria2 error".into()),
        ..Default::default()
    });
    let service = DownloadService::new(api, store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let prepared = prepared_cloud_share_task(&store, &source, "episode.mp4");

    let completed = submit_current_share_download(
        &service,
        &prepared,
        ResolvedCloudDownload {
            url: "https://cdn.example/episode?signature=secret-token".into(),
            name: "episode.mp4".into(),
            headers: vec![],
        },
        UriSubmissionOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(completed.error, None);
    assert_eq!(store.find(completed.id).unwrap().error, None);
}

#[tokio::test]
async fn cloud_share_cancel_after_outer_check_prevents_waiting_refresh_submission() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{
            resubmit_current_share_download, stable_share_source, submit_current_share_download,
        },
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let release = Arc::new(tokio::sync::Notify::new());
    let api = Arc::new(RecordingAria2 {
        status_error_message: Some("HTTP error".into()),
        cancel_release: Some(release.clone()),
        ..Default::default()
    });
    let service = DownloadService::new(api.clone(), store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let resolved = || ResolvedCloudDownload {
        url: "https://cdn.example/file?signature=fresh".into(),
        name: "file".into(),
        headers: vec![],
    };
    let prepared = prepared_cloud_share_task(&store, &source, "file");
    let checked =
        submit_current_share_download(&service, &prepared, resolved(), Default::default())
            .await
            .unwrap();
    assert_eq!(checked.status, TaskStatus::Failed);

    // The caller's outer check has completed. Cancellation now owns the
    // service operation lock while its aria2 remove waits for our signal.
    let cancel = service.control(checked.id, DownloadAction::Cancel);
    tokio::pin!(cancel);
    assert!(futures_util::poll!(&mut cancel).is_pending());
    assert_eq!(api.calls().last().map(String::as_str), Some("aria2.remove"));
    let refresh =
        resubmit_current_share_download(&service, &checked, resolved(), Default::default());
    tokio::pin!(refresh);
    assert!(futures_util::poll!(&mut refresh).is_pending());
    release.notify_one();
    assert_eq!(cancel.await.unwrap().status, TaskStatus::Cancelled);
    assert!(refresh.await.unwrap().is_none());
    assert_eq!(
        api.uris.lock().unwrap().len(),
        1,
        "stale refresh must not call addUri"
    );
    let saved = store.find(checked.id).unwrap();
    assert_eq!(saved.status, TaskStatus::Cancelled);
    assert_eq!(saved.aria_gid, checked.aria_gid);
}

#[tokio::test]
async fn password_cloud_refresh_uses_new_credentials_and_honors_cancellation() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload, share::resubmit_current_cloud_download,
    };
    for provider in ["baidu", "tianyi"] {
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let api = RecordingAria2::successful();
        let service = DownloadService::new(api.clone(), store.clone());
        let source = format!("cloud://{provider}/file");
        let prepared = prepared_cloud_share_task(&store, &source, "original.bin");
        let original = service
            .submit_uri_with_options(
                prepared.id,
                vec!["https://cdn.example/old".into()],
                Default::default(),
            )
            .await
            .unwrap();
        store.fail(original.id, "expired").unwrap();
        let failed = store.find(original.id).unwrap();
        let resolved = || ResolvedCloudDownload {
            url: "https://cdn.example/new".into(),
            name: "original.bin".into(),
            headers: vec![("Cookie".into(), "fresh-session".into())],
        };
        let recovered =
            resubmit_current_cloud_download(&service, &failed, resolved(), Default::default())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(recovered.id, original.id);
        assert_eq!(recovered.status, TaskStatus::Downloading);
        assert_eq!(api.added_options()[1]["header"], "Cookie: fresh-session");
        assert!(!serde_json::to_string(&store.find(original.id).unwrap())
            .unwrap()
            .contains("fresh-session"));

        store.fail(original.id, "expired again").unwrap();
        let stale = store.find(original.id).unwrap();
        service
            .control(original.id, DownloadAction::Cancel)
            .await
            .unwrap();
        assert!(
            resubmit_current_cloud_download(&service, &stale, resolved(), Default::default(),)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(api.uris.lock().unwrap().len(), 2);
        assert_eq!(
            store.find(original.id).unwrap().status,
            TaskStatus::Cancelled
        );
    }
}

#[tokio::test]
async fn cloud_share_refresh_resubmits_same_task_with_current_headers_once() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{
            resubmit_current_share_download, stable_share_source, submit_current_share_download,
            CloudShareRefreshBudget,
        },
        CloudProvider,
    };
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("nested");
    std::fs::create_dir(&directory).unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = Arc::new(RecordingAria2 {
        status_error_message: Some("HTTP error".into()),
        ..Default::default()
    });
    let service = DownloadService::new(api.clone(), store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let resolved = |revision| ResolvedCloudDownload {
        url: format!("https://cdn.example/file?signature={revision}"),
        name: "file".into(),
        headers: vec![("x-share-token".into(), format!("token-{revision}"))],
    };
    let prepared = prepared_cloud_share_task(&store, &source, "file");
    let task = submit_current_share_download(&service, &prepared, resolved(0), Default::default())
        .await
        .unwrap();
    store
        .upsert_files(
            task.id,
            &[(
                1,
                directory.join("original.bin").to_string_lossy().into_owned(),
                1000,
                250,
                true,
            )],
        )
        .unwrap();
    let budget = CloudShareRefreshBudget::default();
    assert!(budget
        .claim(&task.source, task.aria_error_code.as_deref())
        .unwrap());
    let updated = resubmit_current_share_download(&service, &task, resolved(1), Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.id, task.id);
    assert_eq!(updated.source, source);
    assert_eq!(
        api.uris.lock().unwrap()[1],
        vec!["https://cdn.example/file?signature=1"]
    );
    assert_eq!(api.added_options()[1]["header"], "x-share-token: token-1");
    assert_eq!(api.added_options()[1]["dir"], directory.to_string_lossy());
    assert_eq!(api.added_options()[1]["out"], "original.bin");
    assert!(!budget
        .claim(&updated.source, updated.aria_error_code.as_deref())
        .unwrap());
    assert!(!serde_json::to_string(&store.find(task.id).unwrap())
        .unwrap()
        .contains("signature"));
}

#[tokio::test]
async fn cloud_share_stale_gid_is_skipped_even_when_task_is_failed_again() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{
            resubmit_current_share_download, stable_share_source, submit_current_share_download,
        },
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = Arc::new(RecordingAria2 {
        status_error_message: Some("HTTP error".into()),
        ..Default::default()
    });
    let service = DownloadService::new(api.clone(), store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let resolved = || ResolvedCloudDownload {
        url: "https://cdn.example/file?signature=fresh".into(),
        name: "file".into(),
        headers: vec![],
    };
    let prepared = prepared_cloud_share_task(&store, &source, "file");
    let checked =
        submit_current_share_download(&service, &prepared, resolved(), Default::default())
            .await
            .unwrap();
    store
        .rebind_for_resubmission(checked.id, "replacement-gid")
        .unwrap();
    store.fail(checked.id, "new attempt failed").unwrap();
    assert!(
        resubmit_current_share_download(&service, &checked, resolved(), Default::default())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(api.uris.lock().unwrap().len(), 1);
    let saved = store.find(checked.id).unwrap();
    assert_eq!(saved.status, TaskStatus::Failed);
    assert_eq!(saved.aria_gid.as_deref(), Some("replacement-gid"));
}

#[tokio::test]
async fn cloud_share_manual_retry_can_restart_an_explicitly_cancelled_snapshot() {
    use versadown_lib::cloud::{
        commands::ResolvedCloudDownload,
        share::{
            resubmit_current_share_download, stable_share_source, submit_current_share_download,
        },
        CloudProvider,
    };
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    let source = stable_share_source(CloudProvider::Aliyun, "share", "file");
    let resolved = || ResolvedCloudDownload {
        url: "https://cdn.example/file?signature=fresh".into(),
        name: "file".into(),
        headers: vec![],
    };
    let prepared = prepared_cloud_share_task(&store, &source, "file");
    let task = submit_current_share_download(&service, &prepared, resolved(), Default::default())
        .await
        .unwrap();
    let cancelled = service
        .control(task.id, DownloadAction::Cancel)
        .await
        .unwrap();
    let retried =
        resubmit_current_share_download(&service, &cancelled, resolved(), Default::default())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(retried.id, task.id);
    assert_eq!(retried.status, TaskStatus::Downloading);
    assert_eq!(api.uris.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn cloud_submission_forwards_transient_options_without_persisting_them() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = store
        .create(NewTask {
            source: "cloud://quark/file-1".into(),
            name: "file-1".into(),
            kind: "cloud".into(),
            is_private: false,
        })
        .unwrap();
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());

    service
        .submit_uri_with_options(
            task.id,
            vec!["https://signed.example.test/file".into()],
            UriSubmissionOptions {
                directory: Some(PathBuf::from("D:/Downloads")),
                output_name: Some("wallpaper.jpg".into()),
                headers: vec![
                    ("Cookie".into(), "sid=web; __puus=fresh".into()),
                    ("Referer".into(), "https://pan.quark.cn/".into()),
                ],
                proxy: Some("http://127.0.0.1:7890".into()),
                max_connections_per_server: None,
                allow_overwrite: true,
                continue_download: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        api.added_options(),
        vec![BTreeMap::from([
            ("dir".into(), "D:/Downloads".into()),
            ("out".into(), "wallpaper.jpg".into()),
            ("header".into(), "Cookie: sid=web; __puus=fresh".into(),),
            ("referer".into(), "https://pan.quark.cn/".into()),
            ("all-proxy".into(), "http://127.0.0.1:7890".into()),
            ("allow-overwrite".into(), "true".into()),
            ("auto-file-renaming".into(), "false".into()),
            ("continue".into(), "false".into()),
            ("follow-torrent".into(), "false".into()),
            ("pause".into(), "false".into()),
            ("remove-control-file".into(), "true".into()),
        ])]
    );
    assert_eq!(store.find(task.id).unwrap().source, "cloud://quark/file-1");
}

#[tokio::test]
async fn legacy_resumed_submission_keeps_one_task_and_sets_aria2_continue() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = prepare_download("https://example.test/fixture.bin", &store)
        .unwrap()
        .task;
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());
    service
        .submit_uri(task.id, vec![task.source.clone()], BTreeMap::new())
        .await
        .unwrap();
    store.fail(task.id, "connection interrupted").unwrap();
    let root = tempfile::tempdir().unwrap();
    let payload = root.path().join("fixture.bin");
    std::fs::write(&payload, vec![0; 250]).unwrap();
    std::fs::write(
        root.path().join("fixture.bin.aria2"),
        b"legacy control fixture",
    )
    .unwrap();
    store
        .upsert_files(
            task.id,
            &[(0, payload.to_string_lossy().into_owned(), 1000, 250, true)],
        )
        .unwrap();

    let retried = service
        .resubmit_uri_with_options(
            task.id,
            vec![task.source],
            UriSubmissionOptions {
                continue_download: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(retried.id, task.id);
    assert_eq!(retried.status, TaskStatus::Downloading);
    assert_eq!(
        api.calls(),
        [
            "aria2.addUri",
            "aria2.tellStatus",
            "aria2.addUri",
            "aria2.tellStatus"
        ]
    );
    assert_eq!(
        api.added_options()
            .last()
            .and_then(|options| options.get("continue")),
        Some(&"true".to_string())
    );
}

#[tokio::test]
async fn overwrite_submission_starts_fresh_instead_of_inheriting_global_resume() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let task = prepare_download("https://example.test/preallocated.bin", &store)
        .unwrap()
        .task;
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store);

    service
        .submit_uri_with_options(
            task.id,
            vec![task.source],
            UriSubmissionOptions {
                allow_overwrite: true,
                continue_download: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        api.added_options(),
        vec![BTreeMap::from([
            ("allow-overwrite".into(), "true".into()),
            ("auto-file-renaming".into(), "false".into()),
            ("continue".into(), "false".into()),
            ("follow-torrent".into(), "false".into()),
            ("pause".into(), "false".into()),
            ("remove-control-file".into(), "true".into()),
        ])],
        "an explicit overwrite must discard stale aria2 state instead of inheriting --continue=true"
    );
}

#[tokio::test]
async fn direct_http_binds_gid_only_after_rpc_succeeds() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let outcome = prepare_download("https://example.com/file.bin", &store).unwrap();
    assert!(!outcome.requires_confirmation);
    assert_eq!(outcome.task.status, TaskStatus::Parsing);
    let api = RecordingAria2::successful();
    let service = DownloadService::new(api.clone(), store.clone());

    let task = service
        .submit_uri(outcome.task.id, vec![outcome.task.source], BTreeMap::new())
        .await
        .unwrap();

    assert_eq!(task.aria_gid.as_deref(), Some("gid-1"));
    assert_eq!(task.status, TaskStatus::Downloading);
    assert_eq!(api.calls(), ["aria2.addUri", "aria2.tellStatus"]);
}

#[tokio::test]
async fn failed_submission_never_claims_the_task_is_downloading() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let outcome = prepare_download("https://example.com/file.bin", &store).unwrap();
    let api = Arc::new(RecordingAria2 {
        add_error: true,
        ..Default::default()
    });
    let service = DownloadService::new(api, store.clone());

    assert!(service
        .submit_uri(outcome.task.id, vec![outcome.task.source], BTreeMap::new())
        .await
        .is_err());
    let task = store.find(outcome.task.id).unwrap();
    assert_eq!(task.status, TaskStatus::Parsing);
    assert_eq!(task.aria_gid, None);
}

#[tokio::test]
async fn failed_pause_does_not_optimistically_change_the_store() {
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let outcome = prepare_download("https://example.com/file.bin", &store).unwrap();
    let api = Arc::new(RecordingAria2 {
        pause_error: true,
        ..Default::default()
    });
    let service = DownloadService::new(api, store.clone());
    service
        .submit_uri(outcome.task.id, vec![outcome.task.source], BTreeMap::new())
        .await
        .unwrap();
    let before = store.find(outcome.task.id).unwrap();

    assert!(service
        .control(outcome.task.id, DownloadAction::Pause)
        .await
        .is_err());

    assert_eq!(store.find(outcome.task.id).unwrap().status, before.status);
}

#[test]
fn magnet_waits_for_file_confirmation() {
    let store = TaskStore::open_in_memory().unwrap();
    let outcome = prepare_download(
        "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
        &store,
    )
    .unwrap();
    assert!(outcome.requires_confirmation);
    assert_eq!(outcome.task.status, TaskStatus::AwaitingUser);
}

#[test]
fn torrent_stays_in_parsing_until_bytes_are_loaded_and_aria2_returns_files() {
    let directory = tempfile::tempdir().unwrap();
    let torrent = directory.path().join("game.torrent");
    let store = TaskStore::open_in_memory().unwrap();
    let outcome = prepare_download(torrent.to_str().unwrap(), &store).unwrap();
    assert!(outcome.requires_confirmation);
    assert_eq!(outcome.task.kind, "torrent");
    assert_eq!(outcome.task.status, TaskStatus::Parsing);
    assert!(outcome.resources.is_empty());
}

#[test]
fn thunder_wrapper_is_decoded_before_aria2_submission() {
    let store = TaskStore::open_in_memory().unwrap();
    let outcome = prepare_download("thunder://QUFodHRwOi8vYS9iWlo=", &store).unwrap();

    assert!(!outcome.requires_confirmation);
    assert_eq!(outcome.task.source, "http://a/b");
    assert_eq!(outcome.task.status, TaskStatus::Parsing);
}

#[test]
fn malformed_or_unsupported_thunder_wrapper_is_rejected() {
    let store = TaskStore::open_in_memory().unwrap();
    assert!(prepare_download("thunder://not-base64", &store).is_err());
    assert!(store.list().unwrap().is_empty());
}
