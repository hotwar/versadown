use versadown_lib::cloud::{
    commands::{restore_account, CloudState},
    *,
};
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

#[test]
fn logout_serializes_after_inflight_refresh_and_other_provider_remains_available() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(CloudState::new(root.path().to_owned()).unwrap());
    state
        .with_provider(CloudProvider::Pikpak, |s| s.save("pikpak", b"old"))
        .unwrap();
    let (refresh_started_tx, refresh_started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let refresh_state = state.clone();
    let refresh = std::thread::spawn(move || {
        refresh_state.with_provider(CloudProvider::Pikpak, |store| {
            assert!(store.load("pikpak")?.is_some());
            refresh_started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            store.save("pikpak", b"rotated")
        })
    });
    refresh_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let (other_tx, other_rx) = mpsc::channel();
    let other_state = state.clone();
    let other = std::thread::spawn(move || {
        let result = other_state.with_provider(CloudProvider::Quark, |store| store.load("quark"));
        other_tx.send(result).unwrap();
    });
    let other_result = other_rx.recv_timeout(Duration::from_secs(2));
    let (logout_tx, logout_rx) = mpsc::channel();
    let logout_state = state.clone();
    let logout = std::thread::spawn(move || {
        let result =
            logout_state.with_provider(CloudProvider::Pikpak, |store| store.remove("pikpak"));
        logout_tx.send(result).unwrap();
    });
    let premature_logout = logout_rx.recv_timeout(Duration::from_millis(80));
    release_tx.send(()).unwrap();
    refresh.join().unwrap().unwrap();
    logout.join().unwrap();
    other.join().unwrap();
    assert!(
        other_result.unwrap().unwrap().is_none(),
        "Quark must remain independently available"
    );
    assert!(
        premature_logout.is_err(),
        "logout must await earlier refresh"
    );
    assert!(
        state
            .with_provider(CloudProvider::Pikpak, |store| store.load("pikpak"))
            .unwrap()
            .is_none(),
        "refresh cannot resurrect a logged-out session"
    );
    drop(state);
    root.close().unwrap();
}

#[test]
fn initial_account_restore_is_not_authorized_without_a_network_request() {
    struct NoNetwork;
    impl CloudHttpTransport for NoNetwork {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            panic!("fresh install must not make account requests")
        }
    }
    let store = MemoryCredentialStore::default();
    for provider in [CloudProvider::Quark, CloudProvider::Pikpak] {
        let e = restore_account(provider, &store, &NoNetwork).err().unwrap();
        assert_eq!(serde_json::to_value(e).unwrap()["code"], "not_authorized");
    }
    assert!(matches!(
        PikPakClient::new(&NoNetwork, &store).list("0"),
        Err(QuarkError::AuthorizationExpired)
    ));
}

#[test]
fn cancelled_or_expired_share_poll_does_not_send_another_request() {
    use versadown_lib::cloud::share::ShareWorkControl;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Http(AtomicUsize);
    impl CloudHttpTransport for Http {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CloudHttpResponse {
                status: 200,
                body: "{}".into(),
                ..Default::default()
            })
        }
    }
    for cancelled in [true, false] {
        let http = Http(AtomicUsize::new(0));
        let control = ShareWorkControl::new(if cancelled {
            Duration::from_secs(300)
        } else {
            Duration::ZERO
        });
        if cancelled {
            control.cancel();
        }
        let result = control.transport(&http).send(CloudHttpRequest {
            method: "GET",
            url: "https://api-pan.xunlei.com/drive/v1/tasks/task".into(),
            headers: Default::default(),
            body: None,
        });
        assert!(matches!(result, Err(QuarkError::Transport(_))));
        assert_eq!(http.0.load(Ordering::SeqCst), 0);
    }
}

/// Pause the real provider's task poll, not a mock supervisor operation.
#[test]
fn share_transfer_poll_allows_same_provider_account_and_directory_reads() {
    share_transfer_poll_scenario(false);
}

#[test]
fn cancelled_share_poll_keeps_completed_transfer_journal_and_replacement_account() {
    share_transfer_poll_scenario(true);
}

fn share_transfer_poll_scenario(cancelled: bool) {
    use versadown_lib::cloud::{auth_supervisor::AuthSupervisor, share::*, xunlei::*};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier, Mutex,
    };
    struct PollHttp {
        polling: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        transfers: AtomicUsize,
        cancelled: bool,
    }
    impl CloudHttpTransport for PollHttp {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            let path = url::Url::parse(&request.url).unwrap().path().to_owned();
            let body = match path.as_str() {
                "/v1/shield/captcha/init" => {
                    r#"{"captcha_token":"share-captcha","expires_in":300}"#
                }
                "/drive/v1/share" => r#"{"share_status":"OK","pass_code_token":"pass"}"#,
                "/drive/v1/share/file_info" => {
                    r#"{"share_status":"OK","file_info":{"id":"shared-file","kind":"drive#file","web_content_link":""}}"#
                }
                "/drive/v1/share/restore" => {
                    assert_eq!(request.method, "POST");
                    self.transfers.fetch_add(1, Ordering::SeqCst);
                    r#"{"task_id":"transfer-task"}"#
                }
                "/drive/v1/tasks/transfer-task" => {
                    self.polling.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                    if self.cancelled {
                        return Ok(CloudHttpResponse {
                            status: 200,
                            body: r#"{"phase":"PHASE_TYPE_COMPLETE","params":{"trace_file_ids":"[\"created-file\"]"}}"#.into(),
                            ..Default::default()
                        });
                    }
                    // Auth failure after actual mutation must not replay it.
                    return Err(QuarkError::AuthorizationExpired);
                }
                "/drive/v1/files" => r#"{"files":[],"next_page_token":""}"#,
                unexpected => panic!("unexpected request {unexpected}"),
            };
            Ok(CloudHttpResponse {
                status: 200,
                body: body.into(),
                ..Default::default()
            })
        }
    }
    let supervisor = AuthSupervisor::new(|| 1000, || 30);
    let credentials = MemoryCredentialStore::default();
    let mut session = XunleiSession::with_access(
        "access",
        "refresh",
        XunleiClientProfile {
            client_id: "Xqp0kJBXWhwaTpB6".into(),
            client_version: "1.92.98".into(),
            package_name: "pan.xunlei.com".into(),
            device_id: "device".into(),
            user_agent: "browser".into(),
            ..Default::default()
        },
    );
    session.user_id = "account".into();
    session.expires_at = 9_999_999_999;
    credentials
        .save("xunlei", &serde_json::to_vec(&session).unwrap())
        .unwrap();
    supervisor
        .ready(
            &credentials,
            CloudProvider::Xunlei,
            Some("account".into()),
            Some(CloudAuthMethod::Web),
            None,
        )
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let plans = CloudSharePlanStore::new(SystemCredentialStore::new(root.path().into()).unwrap());
    let plan = CloudSharePlan::new(
        CloudProvider::Xunlei,
        "share".into(),
        None,
        vec![CloudShareEntry::new(
            "share",
            "shared-file".into(),
            "file.txt".into(),
            vec![],
            Some(3),
        )],
        serde_json::json!({}),
    );
    let source = plan.source_for("shared-file");
    plans.upsert(&plan).unwrap();
    let (polling, poll_rx) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let http = PollHttp {
        polling,
        release: Mutex::new(release),
        transfers: AtomicUsize::new(0),
        cancelled,
    };
    let control = ShareWorkControl::new(Duration::from_secs(300));
    let refreshes = AtomicUsize::new(0);
    let failures_started = Barrier::new(2);
    let (read_tx, read_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let transfer = scope.spawn(|| {
            supervisor.run_checked(
                &credentials,
                CloudProvider::Xunlei,
                SessionWorkKind::Mutation,
                |snapshot| {
                    resolve_current_share_download_cancellable(
                        &plans, snapshot, &http, &source, &control,
                    )
                },
                |_| {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || control.check(),
            )
        });
        poll_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let read = scope.spawn(|| {
            let result = supervisor.run(
                &credentials,
                CloudProvider::Xunlei,
                SessionWorkKind::Read,
                |snapshot| {
                    let mut driver =
                        ProviderRegistry::global().driver(CloudProvider::Xunlei, snapshot, &http);
                    let account = driver.account()?;
                    let directory = driver.list_directory("folder")?;
                    Ok((account.id, directory.items.len()))
                },
                |_| panic!("valid read must not refresh"),
            );
            read_tx.send(result).unwrap();
        });
        let read_result = read_rx.recv_timeout(Duration::from_secs(2));
        if cancelled {
            control.cancel();
            session.access_token = "replacement-access".into();
            session.user_id = "replacement-account".into();
            supervisor
                .with_provider(CloudProvider::Xunlei, || {
                    credentials.save("xunlei", &serde_json::to_vec(&session).unwrap())
                })
                .unwrap();
            supervisor
                .ready(
                    &credentials,
                    CloudProvider::Xunlei,
                    Some("replacement-account".into()),
                    Some(CloudAuthMethod::Web),
                    None,
                )
                .unwrap();
        }
        // Release before asserting, so a lock regression fails without leaving a hung worker.
        release_tx.send(()).unwrap();
        read.join().unwrap();
        assert_eq!(read_result.unwrap().unwrap(), ("account".into(), 0));
        let transfer_result = transfer.join().unwrap();
        if cancelled {
            assert!(matches!(transfer_result, Err(QuarkError::Transport(_))));
            assert_eq!(refreshes.load(Ordering::SeqCst), 0);
            assert_eq!(
                credentials.load("xunlei").unwrap().unwrap(),
                serde_json::to_vec(&session).unwrap()
            );
            let recorded = plans.resolve_entry(&source).unwrap().plan.transfers;
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].created_file_id, "created-file");
        } else {
            assert!(matches!(
                transfer_result,
                Err(QuarkError::SessionRenewedRetryOperation)
            ));
        }
        let before = refreshes.load(Ordering::SeqCst);
        let readers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut first = true;
                    supervisor.run(
                        &credentials,
                        CloudProvider::Xunlei,
                        SessionWorkKind::Read,
                        |_| {
                            if first {
                                first = false;
                                failures_started.wait();
                                return Err(QuarkError::AuthorizationExpired);
                            }
                            Ok(())
                        },
                        |_| {
                            refreshes.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                })
            })
            .collect();
        for reader in readers {
            reader.join().unwrap().unwrap();
        }
        assert_eq!(refreshes.load(Ordering::SeqCst) - before, 1);
    });
    assert_eq!(http.transfers.load(Ordering::SeqCst), 1);
}
