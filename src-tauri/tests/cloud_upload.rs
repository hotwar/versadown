use versadown_lib::cloud::*;
#[path = "support/upload.rs"]
mod upload_support;
use std::{collections::BTreeMap, sync::Mutex};

#[test]
fn sparse_file_slice_is_lazy_exact_and_reports_reader_progress() {
    use std::io::{Read, Seek, SeekFrom, Write};
    struct SliceTransport(UploadControl);
    impl CloudUploadTransport for SliceTransport {
        fn send_upload(
            &self,
            request: UploadHttpRequest,
        ) -> Result<UploadHttpResponse, QuarkError> {
            assert!(
                matches!(request.body, UploadBody::FileSlice { .. }),
                "no file Vec may cross the transport boundary"
            );
            assert_eq!(request.body.len(), 7);
            let mut reader = request.body.reader(self.0.clone())?;
            let mut bytes = [0; 32];
            assert_eq!(reader.read(&mut bytes).unwrap(), 7);
            assert_eq!(&bytes[..7], b"PAYLOAD");
            assert_eq!(reader.read(&mut bytes).unwrap(), 0);
            Ok(UploadHttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: String::new(),
            })
        }
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let sparse = std::process::Command::new("fsutil.exe")
            .args(["sparse", "setflag"])
            .arg(file.path())
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            sparse.status.success(),
            "could not mark the Windows fixture sparse"
        );
    }
    // 512 MiB sparse fixture exceeds the 8 MiB upload-memory budget.
    file.as_file().set_len(512 * 1024 * 1024).unwrap();
    file.seek(SeekFrom::Start(400 * 1024 * 1024)).unwrap();
    file.write_all(b"before-PAYLOAD-after").unwrap();
    file.flush().unwrap();
    let snapshot = UploadFileSnapshot::capture(file.path()).unwrap();
    let body = snapshot.slice(400 * 1024 * 1024 + 7, 7).unwrap();
    let control = UploadControl::default();
    SliceTransport(control.clone())
        .send_upload(UploadHttpRequest {
            method: "PUT",
            url: "https://fixture.invalid/upload".into(),
            headers: BTreeMap::new(),
            body,
        })
        .unwrap();
    assert_eq!(control.bytes_read(), 7);
    let body = snapshot.slice(0, 10).unwrap();
    file.as_file().set_len(4).unwrap();
    assert!(
        body.reader(UploadControl::default()).is_err(),
        "body opens and validates only when sent"
    );
}

#[test]
fn file_reader_rejects_same_length_rewrite_replacement_and_cancellation() {
    use std::io::Read;
    for change in ["mtime", "replacement", "truncate", "cancel"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let snapshot = UploadFileSnapshot::capture(&path).unwrap();
        let control = UploadControl::default();
        let mut reader = snapshot
            .slice(0, 8)
            .unwrap()
            .reader(control.clone())
            .unwrap();
        assert_eq!(reader.read(&mut [0; 2]).unwrap(), 2);
        match change {
            "mtime" => {
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100))
                    .unwrap();
            }
            "replacement" => {
                std::fs::rename(&path, dir.path().join("old")).unwrap();
                std::fs::write(&path, b"abcdefgh").unwrap();
            }
            "truncate" => std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(3)
                .unwrap(),
            _ => control.cancel(),
        }
        assert!(
            reader.read(&mut [0; 8]).is_err(),
            "{change} must stop the reader"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_uploads_preserve_order_and_failure_does_not_drop_queued_files() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar,
    };
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let worker_gate = gate.clone();
    let worker_maximum = maximum.clone();
    let limit = ProviderRegistry::global()
        .get(CloudProvider::Pikpak)
        .max_upload_concurrency;
    let task = tokio::spawn(schedule_uploads(
        vec![0, 1, 2, 3],
        limit,
        UploadControl::default(),
        move |index, _| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            worker_maximum.fetch_max(count, Ordering::SeqCst);
            started.send(index).unwrap();
            let (lock, notify) = &*worker_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = notify.wait(released).unwrap();
            }
            active.fetch_sub(1, Ordering::SeqCst);
            if index == 1 {
                Err(QuarkError::Provider("one failure".into()))
            } else {
                Ok(index)
            }
        },
    ));
    let first = tokio::time::timeout(std::time::Duration::from_secs(3), starts.recv()).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(3), starts.recv()).await;
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let results = task.await.unwrap();
    assert!(
        first.is_ok() && second.is_ok(),
        "two transfers must start before release"
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert_eq!(results.len(), 4);
    assert_eq!(*results[0].as_ref().unwrap(), 0);
    assert!(results[1].is_err());
    assert_eq!(*results[2].as_ref().unwrap(), 2);
    assert_eq!(*results[3].as_ref().unwrap(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_starts_no_queued_uploads() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    };
    let gate = Arc::new(Barrier::new(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let control = UploadControl::default();
    let worker_gate = gate.clone();
    let worker_calls = calls.clone();
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(schedule_uploads(
        vec![0, 1, 2, 3],
        2,
        control.clone(),
        move |index, _| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            started.send(()).unwrap();
            worker_gate.wait();
            Ok(index)
        },
    ));
    starts.recv().await.unwrap();
    starts.recv().await.unwrap();
    control.cancel();
    tokio::task::spawn_blocking(move || gate.wait())
        .await
        .unwrap();
    let results = task.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(results[2].is_err() && results[3].is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_upload_scheduler_cancels_active_readers_and_never_admits_queued_files() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    };
    let gate = Arc::new(Barrier::new(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let control = UploadControl::default();
    let worker_gate = gate.clone();
    let worker_calls = calls.clone();
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let (finished, mut finishes) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(schedule_uploads(
        vec![0, 1, 2, 3],
        2,
        control.clone(),
        move |index, control| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            started.send(()).unwrap();
            worker_gate.wait();
            finished.send(control.check().is_err()).unwrap();
            Ok(index)
        },
    ));
    starts.recv().await.unwrap();
    starts.recv().await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::task::spawn_blocking(move || gate.wait())
        .await
        .unwrap();
    assert_eq!(finishes.recv().await, Some(true));
    assert_eq!(finishes.recv().await, Some(true));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn changed_file_after_initialization_never_reaches_multipart_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upload.txt");
    std::fs::write(&path, b"hello").unwrap();
    let server = UploadServer {
        requests: Mutex::new(Vec::new()),
        fail_complete: false,
    };
    let result = upload_pikpak_file(&server, "0", &path, |_| {
        std::fs::write(&path, b"mixed-content").unwrap();
        Ok(
            serde_json::json!({"file":{"id":"real-file","name":"upload.txt","kind":"drive#file","phase":"PHASE_TYPE_PENDING"},"resumable":{"params":{"endpoint":"bucket.oss-ap-southeast-1.aliyuncs.com","bucket":"bucket","key":"objects/item","access_key_id":"temporary-id","access_key_secret":"temporary-key","security_token":"temporary-token"}}}),
        )
    });
    assert!(
        matches!(result, Err(QuarkError::InvalidInput(_))),
        "changed file must fail before commit"
    );
    assert!(server.requests.lock().unwrap().len() < 3);
}

#[test]
fn changed_file_after_last_slice_is_rejected_before_commit() {
    struct ChangeAfterPart<'a> {
        server: UploadServer,
        path: &'a std::path::Path,
        change: &'a str,
    }
    impl CloudUploadTransport for ChangeAfterPart<'_> {
        fn send_upload(
            &self,
            request: UploadHttpRequest,
        ) -> Result<UploadHttpResponse, QuarkError> {
            let part = request.method == "PUT";
            let response = self.server.send_upload(request)?;
            if part {
                match self.change {
                    "truncate" => std::fs::OpenOptions::new()
                        .write(true)
                        .open(self.path)
                        .unwrap()
                        .set_len(1)
                        .unwrap(),
                    "mtime" => std::fs::OpenOptions::new()
                        .write(true)
                        .open(self.path)
                        .unwrap()
                        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100))
                        .unwrap(),
                    _ => {
                        std::fs::rename(self.path, self.path.with_extension("old")).unwrap();
                        std::fs::write(self.path, b"hello").unwrap();
                    }
                }
            }
            Ok(response)
        }
    }
    for change in ["truncate", "mtime", "identity"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"hello").unwrap();
        let transport = ChangeAfterPart {
            path: &path,
            change,
            server: UploadServer {
                requests: Mutex::new(Vec::new()),
                fail_complete: false,
            },
        };
        let result = upload_pikpak_file(&transport, "0", &path, |_| {
            Ok(
                serde_json::json!({"file":{"id":"file","name":"file","kind":"drive#file","phase":"PHASE_TYPE_PENDING"},"resumable":{"params":{"endpoint":"bucket.oss-ap-southeast-1.aliyuncs.com","bucket":"bucket","key":"objects/item","access_key_id":"id","access_key_secret":"secret","security_token":"token"}}}),
            )
        });
        assert!(
            matches!(result, Err(QuarkError::InvalidInput(_))),
            "{change}"
        );
        assert_eq!(
            transport.server.requests.lock().unwrap().len(),
            2,
            "no commit after {change}"
        );
    }
}

#[test]
fn multipart_rejects_file_change_before_emitting_the_commit_boundary() {
    use std::io::Read;
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"abcdefgh").unwrap();
    let snapshot = UploadFileSnapshot::capture(file.path()).unwrap();
    let body = UploadBody::Multipart {
        prefix: b"prefix".to_vec(),
        file: Box::new(snapshot.slice(0, 8).unwrap()),
        suffix: b"commit-boundary".to_vec(),
    };
    assert_eq!(body.len(), 29);
    let mut reader = body.reader(UploadControl::default()).unwrap();
    let mut buffer = [0; 32];
    assert_eq!(reader.read(&mut buffer).unwrap(), 6);
    assert_eq!(reader.read(&mut buffer).unwrap(), 8);
    file.as_file().set_len(2).unwrap();
    assert!(reader.read(&mut buffer).is_err());
}
struct UploadServer {
    requests: Mutex<Vec<UploadHttpRequest>>,
    fail_complete: bool,
}
impl CloudHttpTransport for UploadServer {
    fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let body: serde_json::Value = serde_json::from_str(r.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["hash"], "E039D0592261B67B6966F14814C41CC43012A1C6");
        assert_eq!(body["upload_type"], "UPLOAD_TYPE_RESUMABLE");
        Ok(CloudHttpResponse{status:200,body:r#"{"file":{"id":"real-file","name":"upload.txt","kind":"drive#file","phase":"PHASE_TYPE_PENDING"},"resumable":{"params":{"endpoint":"bucket.oss-ap-southeast-1.aliyuncs.com","bucket":"bucket","key":"objects/item","access_key_id":"temporary-id","access_key_secret":"temporary-key","security_token":"temporary-token"}}}"#.into(), ..Default::default()})
    }
}
impl CloudUploadTransport for UploadServer {
    fn send_upload(&self, r: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        let r = upload_support::consume(r)?;
        let mut requests = self.requests.lock().unwrap();
        let response=match requests.len(){0=>UploadHttpResponse{status:200,headers:BTreeMap::new(),body:"<InitiateMultipartUploadResult><UploadId>upload-id</UploadId></InitiateMultipartUploadResult>".into()},1=>UploadHttpResponse{status:200,headers:BTreeMap::from([("etag".into(),"\"part-etag\"".into())]),body:String::new()},2=>UploadHttpResponse{status:200,headers:BTreeMap::new(),body:if self.fail_complete{"<Error><Code>InvalidPart</Code></Error>"}else{"<CompleteMultipartUploadResult><ETag>object-etag</ETag></CompleteMultipartUploadResult>"}.into()},_=>panic!("unexpected upload")};
        requests.push(r);
        Ok(response)
    }
}
fn upload(fail: bool) -> (Result<CloudFile, QuarkError>, UploadServer) {
    let root = std::env::temp_dir().join(format!("cloud-upload-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("upload.txt");
    std::fs::write(&path, b"hello").unwrap();
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let server = UploadServer {
        requests: Mutex::new(Vec::new()),
        fail_complete: fail,
    };
    let result = PikPakClient::new(&server, &store).upload_file("0", &path);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(root).unwrap();
    (result, server)
}
#[test]
fn pikpak_upload_transfers_real_bytes_and_completes_multipart() {
    let (result, server) = upload(false);
    assert_eq!(result.unwrap().id, "real-file");
    let r = server.requests.lock().unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(upload_support::bytes(&r[1].body), b"hello");
    assert!(r[1].headers["Authorization"].starts_with("OSS temporary-id:"));
    assert!(String::from_utf8_lossy(upload_support::bytes(&r[2].body))
        .contains("&quot;part-etag&quot;"));
}
#[test]
fn oss_http_200_error_is_not_upload_success() {
    let (result, _) = upload(true);
    assert!(result.is_err());
}

#[test]
fn pikpak_upload_preserves_server_destination_for_instant_and_multipart_uploads() {
    let local = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(local.path(), b"hello").unwrap();
    for complete in [true, false] {
        for (requested, returned, expected) in [
            ("0", Some("actual-upload-folder"), "actual-upload-folder"),
            (
                "selected-folder",
                Some("selected-folder"),
                "selected-folder",
            ),
            ("selected-folder", Some(""), "0"),
            ("selected-folder", None, "selected-folder"),
        ] {
            let server = UploadServer {
                requests: Mutex::new(Vec::new()),
                fail_complete: false,
            };
            let file = upload_pikpak_file(&server, requested, local.path(), |request| {
                assert_eq!(request["parent_id"], if requested == "0" { "" } else { requested });
                let mut response = serde_json::json!({
                    "file": { "id": "uploaded", "name": "hello.txt", "kind": "drive#file",
                        "phase": if complete { "PHASE_TYPE_COMPLETE" } else { "PHASE_TYPE_PENDING" } },
                    "resumable": { "params": { "endpoint": "bucket.oss-ap-southeast-1.aliyuncs.com",
                        "bucket": "bucket", "key": "objects/item", "access_key_id": "temporary-id",
                        "access_key_secret": "temporary-key", "security_token": "temporary-token" } }
                });
                if let Some(parent) = returned { response["file"]["parent_id"] = parent.into(); }
                Ok(response)
            }).unwrap();
            assert_eq!(file.parent_id, expected);
            assert_eq!(
                server.requests.lock().unwrap().len(),
                if complete { 0 } else { 3 }
            );
        }
    }
}

#[test]
fn pikpak_upload_rejects_the_virtual_recycle_bin_target_before_network() {
    let root = std::env::temp_dir().join(format!("cloud-upload-recycle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("upload.txt");
    std::fs::write(&path, b"hello").unwrap();
    let store = MemoryCredentialStore::default();
    store.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap"}"#).unwrap();
    let server = UploadServer {
        requests: Mutex::new(Vec::new()),
        fail_complete: false,
    };

    let error = PikPakClient::new(&server, &store)
        .upload_file("__versa_down_pikpak_recycle_bin__", &path)
        .unwrap_err();

    assert!(matches!(error, QuarkError::InvalidInput(_)));
    assert!(server.requests.lock().unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn upload_transfer_allows_same_account_list_and_logout_without_resurrecting_session() {
    upload_does_not_lock_account(false);
}
#[test]
fn logout_during_upload_init_prevents_late_refresh_from_restoring_credentials() {
    upload_does_not_lock_account(true);
}

#[test]
fn rejected_upload_initialization_renews_session_but_never_replays_the_mutation() {
    use versadown_lib::cloud::commands::CloudState;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct RejectedInit(AtomicUsize);
    impl CloudHttpTransport for RejectedInit {
        fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            if r.url.contains("/auth/token") {
                return Ok(CloudHttpResponse {
                    status: 200,
                    body: r#"{"access_token":"new","refresh_token":"rotated","expires_in":3600}"#
                        .into(),
                    ..Default::default()
                });
            }
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CloudHttpResponse {
                status: 401,
                body: "{}".into(),
                ..Default::default()
            })
        }
    }
    impl CloudUploadTransport for RejectedInit {
        fn send_upload(&self, _: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
            panic!("no upload without accepted initialization")
        }
    }
    let root = tempfile::tempdir().unwrap();
    let state = CloudState::new(root.path().join("sessions")).unwrap();
    state.store.save("pikpak", br#"{"access_token":"old","refresh_token":"refresh","expires_at":9999999999,"device_id":"device"}"#).unwrap();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, b"hello").unwrap();
    let http = RejectedInit(AtomicUsize::new(0));
    assert!(matches!(
        state.upload_pikpak_file(&http, "0", file.path()),
        Err(QuarkError::SessionRenewedRetryOperation)
    ));
    assert_eq!(http.0.load(Ordering::SeqCst), 1);
}
fn upload_does_not_lock_account(block_init: bool) {
    use versadown_lib::cloud::commands::CloudState;
    use std::{
        sync::{mpsc, Arc},
        time::Duration,
    };
    struct BlockingUpload {
        inner: UploadServer,
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        block_init: bool,
    }
    impl CloudHttpTransport for BlockingUpload {
        fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            if self.block_init {
                assert!(
                    r.url.ends_with("/drive/v1/files"),
                    "no stale refresh request is allowed"
                );
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                return Ok(CloudHttpResponse {
                    status: 401,
                    body: r#"{"error_code":16}"#.into(),
                    ..Default::default()
                });
            }
            self.inner.send(r)
        }
    }
    impl CloudUploadTransport for BlockingUpload {
        fn send_upload(&self, r: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
            if self.inner.requests.lock().unwrap().is_empty() {
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            self.inner.send_upload(r)
        }
    }
    struct Directory;
    impl CloudHttpTransport for Directory {
        fn send(&self, r: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            assert!(r.url.contains("/drive/v1/files?"));
            Ok(CloudHttpResponse {
                status: 200,
                body: r#"{"files":[],"next_page_token":""}"#.into(),
                ..Default::default()
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("upload.txt");
    std::fs::write(&file, b"hello").unwrap();
    let state = Arc::new(CloudState::new(root.path().join("sessions")).unwrap());
    state.with_provider(CloudProvider::Pikpak,|s|s.save("pikpak",br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device","captcha_token":"cap","user_id":"same-account"}"#)).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let upload_state = state.clone();
    let upload_path = file.clone();
    let upload = std::thread::spawn(move || {
        let http = BlockingUpload {
            inner: UploadServer {
                requests: Mutex::new(Vec::new()),
                fail_complete: false,
            },
            started: started_tx,
            release: Mutex::new(release_rx),
            block_init,
        };
        upload_state.upload_pikpak_file(&http, "0", &upload_path)
    });
    match started_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(()) => (),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let result = upload.join().expect("upload thread panicked before initialization");
            panic!("upload returned before signaling initialization: {result:?}");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("upload did not signal initialization within three seconds");
        }
    }
    let (done_tx, done_rx) = mpsc::channel();
    let manage_state = state.clone();
    let manage = std::thread::spawn(move || {
        let result = manage_state.with_provider(CloudProvider::Pikpak, |s| {
            PikPakClient::new(&Directory, s).list("0")?;
            s.remove("pikpak")
        });
        done_tx.send(result).unwrap();
    });
    let during_transfer = done_rx.recv_timeout(Duration::from_millis(250));
    release_tx.send(()).unwrap();
    let upload_result = upload.join().unwrap();
    manage.join().unwrap();
    assert!(state
        .with_provider(CloudProvider::Pikpak, |s| s.load("pikpak"))
        .unwrap()
        .is_none());
    assert!(
        during_transfer.is_ok(),
        "same-account list/logout must finish while upload network is blocked"
    );
    during_transfer.unwrap().unwrap();
    if block_init {
        assert!(matches!(
            upload_result,
            Err(QuarkError::AuthorizationExpired)
        ))
    } else {
        assert_eq!(upload_result.unwrap().id, "real-file")
    }
    drop(state);
    root.close().unwrap();
}
