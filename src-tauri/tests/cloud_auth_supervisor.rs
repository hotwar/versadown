use versadown_lib::cloud::{auth_supervisor::*, *};
use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc,
};

fn fixture(now: i64, jitter: i64) -> (AuthSupervisor, Arc<AtomicI64>, MemoryCredentialStore) {
    let clock = Arc::new(AtomicI64::new(now));
    let read = clock.clone();
    (
        AuthSupervisor::new(move || read.load(Ordering::SeqCst), move || jitter),
        clock,
        MemoryCredentialStore::default(),
    )
}

#[test]
fn review_account_replacement_never_replays_an_old_read_on_the_new_account() {
    let (supervisor, _, store) = fixture(1000, 30);
    let provider = CloudProvider::Aliyun;
    store.save(provider.key(), b"account-A").unwrap();
    supervisor
        .ready(&store, provider, Some("A".into()), None, None)
        .unwrap();
    let started = std::sync::Barrier::new(2);
    let replaced = std::sync::Barrier::new(2);
    let calls = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let operation = scope.spawn(|| {
            supervisor.run(
                &store,
                provider,
                SessionWorkKind::Read,
                |snapshot| {
                    let count = calls.fetch_add(1, Ordering::SeqCst);
                    if count == 0 {
                        assert_eq!(snapshot.load(provider.key())?.unwrap(), b"account-A");
                        started.wait();
                        replaced.wait();
                        Err(CloudError::AuthorizationExpired)
                    } else {
                        Ok(snapshot.load(provider.key())?.unwrap())
                    }
                },
                |_| panic!("account replacement is not a refresh"),
            )
        });
        started.wait();
        supervisor
            .with_provider(provider, || store.save(provider.key(), b"account-B"))
            .unwrap();
        supervisor
            .ready(&store, provider, Some("B".into()), None, None)
            .unwrap();
        replaced.wait();
        assert!(matches!(
            operation.join().unwrap(),
            Err(CloudError::NotAuthorized | CloudError::AuthorizationExpired)
        ));
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn review_refresh_publication_never_exposes_new_session_with_old_revision() {
    let (supervisor, _, store) = fixture(1000, 30);
    let provider = CloudProvider::Aliyun;
    store.save(provider.key(), b"old-token").unwrap();
    supervisor
        .ready(&store, provider, Some("A".into()), None, None)
        .unwrap();
    let written = std::sync::Barrier::new(2);
    let release = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let refresh = scope.spawn(|| {
            supervisor.maintain(&store, provider, None, true, || {
                store.save(provider.key(), b"new-token")?;
                written.wait();
                release.wait();
                Ok(())
            })
        });
        written.wait();
        let (tx, rx) = std::sync::mpsc::channel();
        let supervisor = &supervisor;
        let store = &store;
        let read = scope.spawn(move || {
            tx.send(supervisor.run(
                store,
                provider,
                SessionWorkKind::Read,
                |snapshot| snapshot.load(provider.key()),
                |_| panic!(),
            ))
            .unwrap();
        });
        let observed = rx.recv_timeout(std::time::Duration::from_secs(2));
        release.wait();
        refresh.join().unwrap().unwrap();
        read.join().unwrap();
        assert_eq!(observed.unwrap().unwrap().unwrap(), b"old-token");
    });
    let latest = supervisor
        .run(
            &store,
            provider,
            SessionWorkKind::Read,
            |snapshot| snapshot.load(provider.key()),
            |_| panic!(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(latest, b"new-token");
}

#[test]
fn review_captcha_on_the_second_read_persists_risk_blocked_without_a_third_read() {
    let (supervisor, _, store) = fixture(1000, 30);
    let provider = CloudProvider::Pikpak;
    store.save(provider.key(), b"old").unwrap();
    let calls = AtomicUsize::new(0);
    let result: Result<(), _> = supervisor.run(
        &store,
        provider,
        SessionWorkKind::Read,
        |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(CloudError::AuthorizationExpired)
            } else {
                Err(CloudError::CaptchaRequired)
            }
        },
        |_| store.save(provider.key(), b"renewed"),
    );
    assert!(matches!(result, Err(CloudError::CaptchaRequired)));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        supervisor.record(&store, provider).unwrap().health,
        AuthHealth::RiskBlocked
    );
}

#[test]
fn web_authorization_required_uses_saved_password_before_requesting_user_login() {
    use versadown_lib::cloud::driver::ProviderAuth;

    struct SavedPassword<'a>(&'a AtomicUsize);
    impl ProviderAuth for SavedPassword<'_> {
        fn refresh_session(&mut self) -> Result<bool, CloudError> {
            Ok(false)
        }

        fn recover_password(
            &mut self,
            credential: &CloudPasswordCredential,
        ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
            assert_eq!(credential.username(), "alice");
            assert_eq!(credential.password(), "secret");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok((
                CloudAccount {
                    provider: CloudProvider::Pikpak,
                    id: "account-a".into(),
                    display_name: "Alice".into(),
                    auth_method: CloudAuthMethod::Password,
                },
                b"renewed-session".to_vec(),
            ))
        }
    }

    let (supervisor, _, store) = fixture(1000, 30);
    let provider = CloudProvider::Pikpak;
    store.save(provider.key(), b"expired-session").unwrap();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(provider, "account-a", "alice", "secret"),
    )
    .unwrap();
    supervisor
        .ready(&store, provider, Some("account-a".into()), None, None)
        .unwrap();
    let password_logins = AtomicUsize::new(0);
    let reads = AtomicUsize::new(0);

    let result = supervisor.run(
        &store,
        provider,
        SessionWorkKind::Read,
        |snapshot| {
            reads.fetch_add(1, Ordering::SeqCst);
            if snapshot.load(provider.key())?.as_deref() == Some(b"renewed-session") {
                Ok(())
            } else {
                Err(CloudError::WebAuthorizationRequired)
            }
        },
        |phase| {
            recover_in_order(
                provider,
                &store,
                &mut SavedPassword(&password_logins),
                phase,
                || panic!("successful saved-password login must not require a browser profile"),
            )
        },
    );

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(password_logins.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

#[test]
fn review_proactive_auth_failure_can_continue_to_profile_in_the_same_run() {
    use versadown_lib::cloud::driver::ProviderAuth;
    struct Rejected<'a>(&'a AtomicUsize);
    impl ProviderAuth for Rejected<'_> {
        fn refresh_session(&mut self) -> Result<bool, CloudError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(CloudError::AuthorizationExpired)
        }
    }
    for provider in [CloudProvider::Aliyun, CloudProvider::Xunlei] {
        let (supervisor, clock, store) = fixture(1000, 30);
        store
            .save(provider.key(), br#"{"expires_at":2000}"#)
            .unwrap();
        supervisor
            .ready(&store, provider, Some("A".into()), None, Some(2000))
            .unwrap();
        clock.store(1670, Ordering::SeqCst);
        let refreshes = AtomicUsize::new(0);
        let profiles = AtomicUsize::new(0);
        let reads = AtomicUsize::new(0);
        let result = supervisor.run(
            &store,
            provider,
            SessionWorkKind::Read,
            |snapshot| {
                reads.fetch_add(1, Ordering::SeqCst);
                if snapshot.load(provider.key())?.as_deref() == Some(b"profile-session") {
                    Ok(())
                } else {
                    Err(CloudError::AuthorizationExpired)
                }
            },
            |phase| {
                recover_in_order(provider, &store, &mut Rejected(&refreshes), phase, || {
                    profiles.fetch_add(1, Ordering::SeqCst);
                    store.save(provider.key(), b"profile-session")?;
                    Ok(true)
                })
            },
        );
        assert!(result.is_ok(), "{provider:?}: {result:?}");
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(profiles.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn review_failed_profile_continuation_cannot_escape_backoff_in_another_run_or_restart() {
    let (supervisor, clock, store) = fixture(1000, 30);
    let provider = CloudProvider::Aliyun;
    store
        .save(provider.key(), br#"{"expires_at":2000}"#)
        .unwrap();
    supervisor
        .ready(&store, provider, Some("A".into()), None, Some(2000))
        .unwrap();
    clock.store(1670, Ordering::SeqCst);
    let phases = std::sync::Mutex::new(Vec::new());
    let result: Result<(), _> = supervisor.run(
        &store,
        provider,
        SessionWorkKind::Mutation,
        |snapshot| {
            snapshot.save(provider.key(), br#"{"expires_at":2000,"ticket":"rotated"}"#)?;
            Err(CloudError::AuthorizationExpired)
        },
        |phase| {
            phases.lock().unwrap().push(phase);
            Err(CloudError::WebAuthorizationRequired)
        },
    );
    assert!(matches!(result, Err(CloudError::WebAuthorizationRequired)));
    let phases = phases.into_inner().unwrap();
    assert_eq!(phases.len(), 2);
    assert!(phases[0] == RecoveryPhase::Proactive && phases[1] == RecoveryPhase::ProfileOnly);
    let record = supervisor.record(&store, provider).unwrap();
    assert_eq!(record.consecutive_failures, 1);
    assert_eq!(record.next_allowed_attempt_at, Some(1970));
    let restarted = AuthSupervisor::new(|| 1700, || 30);
    restarted.restore(&store, provider).unwrap();
    for current in [&supervisor, &restarted] {
        let result: Result<(), _> = current.run(
            &store,
            provider,
            SessionWorkKind::Read,
            |_| Err(CloudError::AuthorizationExpired),
            |_| panic!("persisted backoff must apply"),
        );
        assert!(matches!(result, Err(CloudError::WebAuthorizationRequired)));
        assert_eq!(
            current
                .record(&store, provider)
                .unwrap()
                .consecutive_failures,
            1
        );
    }
}

#[test]
fn rotated_read_keeps_its_same_run_profile_continuation() {
    rotated_operation_continues_profile(SessionWorkKind::Read);
}

#[test]
fn rotated_mutation_continues_profile_without_replaying_the_mutation() {
    rotated_operation_continues_profile(SessionWorkKind::Mutation);
}

fn rotated_operation_continues_profile(kind: SessionWorkKind) {
    let (supervisor, clock, store) = fixture(1000, 30);
    let provider = CloudProvider::Xunlei;
    store
        .save(provider.key(), br#"{"expires_at":2000}"#)
        .unwrap();
    supervisor
        .ready(&store, provider, Some("A".into()), None, Some(2000))
        .unwrap();
    clock.store(1670, Ordering::SeqCst);
    let calls = AtomicUsize::new(0);
    let phases = std::sync::Mutex::new(Vec::new());
    let result = supervisor.run(
        &store,
        provider,
        kind,
        |snapshot| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                snapshot.save(provider.key(), br#"{"expires_at":2000,"ticket":"rotated"}"#)?;
                Err(CloudError::AuthorizationExpired)
            } else {
                assert_eq!(snapshot.load(provider.key())?.unwrap(), b"profile-session");
                Ok(())
            }
        },
        |phase| {
            phases.lock().unwrap().push(phase);
            match phase {
                RecoveryPhase::Proactive => Err(CloudError::WebAuthorizationRequired),
                RecoveryPhase::ProfileOnly => store.save(provider.key(), b"profile-session"),
                RecoveryPhase::AuthFailure | RecoveryPhase::OperationVerification => {
                    panic!("the same attempt must continue at profile only")
                }
            }
        },
    );
    assert_eq!(
        store.load(provider.key()).unwrap().unwrap(),
        b"profile-session"
    );
    let phases = phases.into_inner().unwrap();
    assert!(phases == [RecoveryPhase::Proactive, RecoveryPhase::ProfileOnly]);
    match kind {
        SessionWorkKind::Read => {
            result.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
        SessionWorkKind::Mutation => {
            assert!(matches!(
                result,
                Err(CloudError::SessionRenewedRetryOperation)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        SessionWorkKind::Logout => unreachable!(),
    }
    let record = supervisor.record(&store, provider).unwrap();
    assert_eq!(record.health, AuthHealth::Ready);
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(record.next_allowed_attempt_at, None);
}

#[test]
fn rotated_profile_continuation_rejects_changed_owner_lineage_or_attempt() {
    for change in ["account", "lineage", "failures", "deadline", "risk"] {
        let (supervisor, clock, store) = fixture(1000, 30);
        let provider = CloudProvider::Aliyun;
        let initial = br#"{"expires_at":2000}"#;
        store.save(provider.key(), initial).unwrap();
        supervisor
            .ready(&store, provider, Some("A".into()), None, Some(2000))
            .unwrap();
        clock.store(1670, Ordering::SeqCst);
        let calls = AtomicUsize::new(0);
        let phases = std::sync::Mutex::new(Vec::new());
        let result: Result<(), _> = supervisor.run(
            &store,
            provider,
            SessionWorkKind::Read,
            |snapshot| {
                assert_eq!(
                    calls.fetch_add(1, Ordering::SeqCst),
                    0,
                    "{change}: invalid continuation must not retry"
                );
                match change {
                    "account" => {
                        store.save(provider.key(), b"account-B")?;
                        supervisor.ready(&store, provider, Some("B".into()), None, None)?;
                    }
                    "lineage" => {
                        for bytes in [b"other-session".as_slice(), initial.as_slice()] {
                            store.save(provider.key(), bytes)?;
                            supervisor.run(
                                &store,
                                provider,
                                SessionWorkKind::Read,
                                |current| current.load(provider.key()),
                                |_| panic!("no recovery"),
                            )?;
                        }
                    }
                    _ => {
                        let mut record = supervisor.record(&store, provider)?;
                        match change {
                            "failures" => record.consecutive_failures += 1,
                            "deadline" => record.next_allowed_attempt_at = Some(2270),
                            "risk" => record.health = AuthHealth::RiskBlocked,
                            _ => unreachable!(),
                        }
                        store.save(
                            "cloud-auth-maintenance-aliyun",
                            &serde_json::to_vec(&record).unwrap(),
                        )?;
                    }
                }
                snapshot.save(provider.key(), br#"{"expires_at":2000,"ticket":"rotated"}"#)?;
                Err(CloudError::AuthorizationExpired)
            },
            |phase| {
                phases.lock().unwrap().push(phase);
                assert!(
                    phase == RecoveryPhase::Proactive,
                    "{change}: no profile grant"
                );
                Err(CloudError::WebAuthorizationRequired)
            },
        );
        assert!(
            matches!(
                result,
                Err(CloudError::NotAuthorized | CloudError::WebAuthorizationRequired)
            ),
            "{change}: {result:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(phases.into_inner().unwrap().len(), 1);
    }
}

#[test]
fn review_logout_and_relogin_of_the_same_account_does_not_revive_an_old_operation() {
    let (supervisor, _, store) = fixture(1000, 30);
    let provider = CloudProvider::Pikpak;
    store.save(provider.key(), b"old").unwrap();
    supervisor
        .ready(&store, provider, Some("A".into()), None, None)
        .unwrap();
    let calls = AtomicUsize::new(0);
    let result: Result<(), _> = supervisor.run(
        &store,
        provider,
        SessionWorkKind::Mutation,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            store.remove(provider.key())?;
            supervisor.disconnect(&store, provider)?;
            store.save(provider.key(), b"new-login")?;
            supervisor.ready(&store, provider, Some("A".into()), None, None)?;
            Err(CloudError::AuthorizationExpired)
        },
        |_| panic!(),
    );
    assert!(matches!(result, Err(CloudError::NotAuthorized)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn known_expiry_is_scheduled_before_eighty_percent_or_five_minutes_with_bounded_jitter() {
    for (expires, jitter, expected) in [
        (11000, 30, 8970),
        (2000, 120, 1580),
        (11000, -1, 8970),
        (11000, 999, 8880),
    ] {
        let (supervisor, clock, store) = fixture(1000, jitter);
        let provider = CloudProvider::Pikpak;
        supervisor
            .ready(
                &store,
                provider,
                Some("account".into()),
                Some(CloudAuthMethod::Password),
                Some(expires),
            )
            .unwrap();
        let record = supervisor.record(&store, provider).unwrap();
        assert_eq!(record.next_refresh_at, Some(expected));
        assert_eq!(record.health, AuthHealth::Ready);
        clock.store(expected - 1, Ordering::SeqCst);
        assert!(!supervisor.is_due(&store, provider).unwrap());
        clock.store(expected, Ordering::SeqCst);
        assert!(supervisor.is_due(&store, provider).unwrap());
        supervisor
            .maintain(&store, provider, None, false, || {
                assert_eq!(
                    supervisor.record(&store, provider)?.health,
                    AuthHealth::Refreshing
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(
            supervisor.record(&store, provider).unwrap().health,
            AuthHealth::Ready
        );
    }
}

#[test]
fn cookie_sessions_have_no_timer_keepalive_even_with_expiry_metadata() {
    let (supervisor, _, store) = fixture(1000, 30);
    for provider in [
        CloudProvider::Quark,
        CloudProvider::Baidu,
        CloudProvider::Uc,
    ] {
        supervisor
            .ready(
                &store,
                provider,
                None,
                Some(CloudAuthMethod::Web),
                Some(1001),
            )
            .unwrap();
        assert_eq!(
            supervisor.record(&store, provider).unwrap().next_refresh_at,
            None
        );
        assert!(!supervisor.is_due(&store, provider).unwrap());
    }
}

#[test]
fn failed_attempts_persist_escalating_backoff_across_restart_and_success_resets_it() {
    let (supervisor, clock, store) = fixture(1000, 30);
    let provider = CloudProvider::Pikpak;
    for (attempt, now, next) in [(1, 1000, 1300), (2, 1300, 3100), (3, 3100, 10300)] {
        clock.store(now, Ordering::SeqCst);
        assert!(matches!(
            supervisor.maintain(&store, provider, None, true, || Err(
                CloudError::AuthorizationExpired
            )),
            Err(CloudError::AuthorizationExpired)
        ));
        let record = supervisor.record(&store, provider).unwrap();
        assert_eq!(record.consecutive_failures, attempt);
        assert_eq!(record.next_allowed_attempt_at, Some(next));
        let read = clock.clone();
        let restarted = AuthSupervisor::new(move || read.load(Ordering::SeqCst), || 30);
        let result = restarted.maintain(&store, provider, None, true, || {
            panic!("backoff must survive restart")
        });
        assert!(matches!(result, Err(CloudError::WebAuthorizationRequired)));
    }
    clock.store(10300, Ordering::SeqCst);
    assert!(matches!(
        supervisor.maintain(&store, provider, None, true, || panic!(
            "three failures need user authorization"
        )),
        Err(CloudError::WebAuthorizationRequired)
    ));
    supervisor
        .ready(
            &store,
            provider,
            Some("new-account".into()),
            Some(CloudAuthMethod::Web),
            Some(20000),
        )
        .unwrap();
    let record = supervisor.record(&store, provider).unwrap();
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(record.next_allowed_attempt_at, None);
    assert_eq!(record.account_id.as_deref(), Some("new-account"));
    let value = serde_json::to_value(record).unwrap();
    assert_eq!(
        value.as_object().unwrap().len(),
        10,
        "maintenance contains metadata only"
    );
}

#[test]
fn successful_recovery_resets_previous_failure() {
    let (supervisor, clock, store) = fixture(1000, 30);
    let provider = CloudProvider::Pikpak;
    let _ = supervisor.maintain(&store, provider, None, true, || {
        Err(CloudError::AuthorizationExpired)
    });
    clock.store(1300, Ordering::SeqCst);
    supervisor
        .maintain(&store, provider, None, true, || Ok(()))
        .unwrap();
    let record = supervisor.record(&store, provider).unwrap();
    assert_eq!(record.health, AuthHealth::Ready);
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(record.next_allowed_attempt_at, None);
}

#[test]
fn mutation_is_invoked_once_and_requires_explicit_retry_after_renewal() {
    let (supervisor, _, store) = fixture(1000, 30);
    store.save("pikpak", b"old").unwrap();
    let calls = AtomicUsize::new(0);
    let result: Result<(), _> = supervisor.run(
        &store,
        CloudProvider::Pikpak,
        SessionWorkKind::Mutation,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(CloudError::AuthorizationExpired)
        },
        |_| store.save("pikpak", b"renewed"),
    );
    assert!(matches!(
        result,
        Err(CloudError::SessionRenewedRetryOperation)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.load("pikpak").unwrap(), Some(b"renewed".to_vec()));
}

fn xunlei_web_session(ticket: &str, expires_at: u64) -> Vec<u8> {
    let mut session = xunlei::XunleiSession::with_access(
        "fixture-access",
        "fixture-refresh",
        xunlei::XunleiClientProfile {
            client_id: "Xqp0kJBXWhwaTpB6".into(),
            client_version: "1.93.22".into(),
            package_name: "pan.xunlei.com".into(),
            device_id: "fixture-device".into(),
            user_agent: "fixture-browser".into(),
            ..Default::default()
        },
    );
    session.user_id = "A".into();
    session.expires_at = 10000;
    session.web_security.captcha_token = ticket.into();
    session.web_security.expires_at = expires_at;
    serde_json::to_vec(&session).unwrap()
}

#[test]
fn xunlei_unchanged_or_unusable_verification_never_reports_login_restored() {
    for (ticket, expires_at) in [
        ("rejected-ticket", 2000),
        ("rejected-ticket", 3000),
        ("", 3000),
        ("new-ticket", 1000),
    ] {
        let (supervisor, _, store) = fixture(1000, 30);
        let original = xunlei_web_session("rejected-ticket", 2000);
        store.save("xunlei", &original).unwrap();
        supervisor
            .ready(&store, CloudProvider::Xunlei, Some("A".into()), None, Some(10000))
            .unwrap();
        let recoveries = AtomicUsize::new(0);
        for _ in 0..2 {
            let result: Result<(), _> = supervisor.run(
                &store,
                CloudProvider::Xunlei,
                SessionWorkKind::Mutation,
                |_| Err(CloudError::WebAuthorizationRequired),
                |phase| {
                    assert!(phase == RecoveryPhase::ProfileOnly);
                    recoveries.fetch_add(1, Ordering::SeqCst);
                    store.save("xunlei", &xunlei_web_session(ticket, expires_at))
                },
            );
            assert!(matches!(result, Err(CloudError::WebAuthorizationRequired)));
        }
        assert_eq!(recoveries.load(Ordering::SeqCst), 1);
        assert_eq!(store.load("xunlei").unwrap(), Some(original));
        assert_eq!(
            supervisor.record(&store, CloudProvider::Xunlei).unwrap().consecutive_failures,
            1,
        );
    }
}

#[test]
fn xunlei_new_verification_retries_reads_and_never_replays_mutations() {
    for kind in [SessionWorkKind::Read, SessionWorkKind::Mutation] {
        let (supervisor, _, store) = fixture(1000, 30);
        store
            .save("xunlei", &xunlei_web_session("rejected-ticket", 2000))
            .unwrap();
        supervisor
            .ready(&store, CloudProvider::Xunlei, Some("A".into()), None, Some(10000))
            .unwrap();
        let calls = AtomicUsize::new(0);
        let result = supervisor.run(
            &store,
            CloudProvider::Xunlei,
            kind,
            |_| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(CloudError::WebAuthorizationRequired)
                } else {
                    Ok(())
                }
            },
            |phase| {
                assert!(phase == RecoveryPhase::ProfileOnly);
                store.save("xunlei", &xunlei_web_session("new-ticket", 3000))
            },
        );
        if kind == SessionWorkKind::Read {
            result.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        } else {
            assert!(matches!(result, Err(CloudError::SessionRenewedRetryOperation)));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn concurrent_failed_reads_share_one_refresh_and_plain_reads_do_not_wait_for_it() {
    let (supervisor, _, store) = fixture(1000, 30);
    store.save("pikpak", b"old").unwrap();
    let both_started = std::sync::Barrier::new(2);
    let refreshes = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut first = true;
                    supervisor.run(
                        &store,
                        CloudProvider::Pikpak,
                        SessionWorkKind::Read,
                        |snapshot| {
                            if first {
                                first = false;
                                both_started.wait();
                                return Err(CloudError::AuthorizationExpired);
                            }
                            Ok(snapshot.load("pikpak")?.unwrap())
                        },
                        |_| {
                            refreshes.fetch_add(1, Ordering::SeqCst);
                            // Reentrant ordinary reads and other providers must not acquire this refresh lock.
                            supervisor
                                .run(
                                    &store,
                                    CloudProvider::Pikpak,
                                    SessionWorkKind::Read,
                                    |snapshot| snapshot.load("pikpak"),
                                    |_| panic!(),
                                )
                                .unwrap();
                            supervisor
                                .maintain(&store, CloudProvider::Aliyun, None, true, || Ok(()))
                                .unwrap();
                            store.save("pikpak", b"renewed")
                        },
                    )
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().unwrap(), b"renewed");
        }
    });
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
}

#[test]
fn native_snapshot_operation_never_refreshes_or_replays_rejected_request() {
    struct Rejected(AtomicUsize);
    impl CloudHttpTransport for Rejected {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CloudHttpResponse {
                status: 401,
                body: "{}".into(),
                set_cookies: vec![],
            })
        }
    }
    let store = MemoryCredentialStore::default();
    store.save("pikpak", br#"{"access_token":"access","refresh_token":"refresh","expires_at":9999999999,"device_id":"device"}"#).unwrap();
    store.save("aliyun", br#"{"access_token":"access","refresh_token":"refresh","drive_id":"drive","browser_access_ready":true}"#).unwrap();
    for provider in [CloudProvider::Pikpak, CloudProvider::Aliyun] {
        let snapshot = SessionSnapshot::new(&store, provider).unwrap();
        let http = Rejected(AtomicUsize::new(0));
        let result = ProviderRegistry::global()
            .driver(provider, &snapshot, &http)
            .rename("file", "new-name");
        assert!(
            matches!(result, Err(CloudError::AuthorizationExpired)),
            "{provider:?}"
        );
        assert_eq!(
            http.0.load(Ordering::SeqCst),
            1,
            "{provider:?}: snapshot cannot rotate credentials or replay"
        );
    }
}

#[test]
fn stale_snapshot_cannot_overwrite_a_new_account_session() {
    let (supervisor, _, store) = fixture(1000, 30);
    store.save("pikpak", b"old-account").unwrap();
    supervisor
        .run(
            &store,
            CloudProvider::Pikpak,
            SessionWorkKind::Read,
            |snapshot| {
                store.save("pikpak", b"new-account")?;
                supervisor.ready(
                    &store,
                    CloudProvider::Pikpak,
                    Some("new".into()),
                    Some(CloudAuthMethod::Web),
                    None,
                )?;
                snapshot.save("pikpak", b"stale-rotated-token")
            },
            |_| panic!(),
        )
        .unwrap();
    assert_eq!(store.load("pikpak").unwrap().unwrap(), b"new-account");
}

#[test]
fn recovery_tries_refresh_then_saved_password_then_profile_only_for_real_auth_errors() {
    use versadown_lib::cloud::driver::ProviderAuth;
    use std::sync::Mutex;
    struct Auth<'a>(&'a Mutex<Vec<&'static str>>);
    impl ProviderAuth for Auth<'_> {
        fn refresh_session(&mut self) -> Result<bool, CloudError> {
            self.0.lock().unwrap().push("refresh");
            Err(CloudError::AuthorizationExpired)
        }
        fn recover_password(
            &mut self,
            credential: &CloudPasswordCredential,
        ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
            assert_eq!(credential.password(), "backend-secret");
            self.0.lock().unwrap().push("password");
            Err(CloudError::AuthorizationExpired)
        }
    }
    let store = MemoryCredentialStore::default();
    save_password_credential(
        &store,
        CloudPasswordCredential::new(CloudProvider::Pikpak, "account", "user", "backend-secret"),
    )
    .unwrap();
    for genuine in [false, true] {
        let calls = Mutex::new(vec![]);
        let result = recover_in_order(
            CloudProvider::Pikpak,
            &store,
            &mut Auth(&calls),
            genuine,
            || {
                calls.lock().unwrap().push("profile");
                Ok(true)
            },
        );
        if genuine {
            assert!(result.is_ok());
            assert_eq!(
                *calls.lock().unwrap(),
                vec!["refresh", "password", "profile"]
            );
        } else {
            assert!(result.is_err());
            assert_eq!(*calls.lock().unwrap(), vec!["refresh", "password"]);
        }
    }
}

#[test]
fn transport_failure_does_not_start_password_or_profile_login() {
    use versadown_lib::cloud::driver::ProviderAuth;
    struct Offline;
    impl ProviderAuth for Offline {
        fn refresh_session(&mut self) -> Result<bool, CloudError> {
            Err(CloudError::Transport("offline".into()))
        }
        fn recover_password(
            &mut self,
            _: &CloudPasswordCredential,
        ) -> Result<(CloudAccount, Vec<u8>), CloudError> {
            panic!()
        }
    }
    assert!(matches!(
        recover_in_order(
            CloudProvider::Pikpak,
            &MemoryCredentialStore::default(),
            &mut Offline,
            true,
            || panic!()
        ),
        Err(CloudError::Transport(_))
    ));
}

#[test]
fn restoring_legacy_sessions_schedules_known_expiry_but_preserves_existing_failure_record() {
    let (supervisor, clock, store) = fixture(1000, 30);
    store.save("pikpak", br#"{"expires_at":2000}"#).unwrap();
    supervisor.restore(&store, CloudProvider::Pikpak).unwrap();
    assert_eq!(
        supervisor
            .record(&store, CloudProvider::Pikpak)
            .unwrap()
            .next_refresh_at,
        Some(1670)
    );
    let _ = supervisor.maintain(&store, CloudProvider::Pikpak, None, true, || {
        Err(CloudError::AuthorizationExpired)
    });
    clock.store(1100, Ordering::SeqCst);
    supervisor.restore(&store, CloudProvider::Pikpak).unwrap();
    assert_eq!(
        supervisor
            .record(&store, CloudProvider::Pikpak)
            .unwrap()
            .next_allowed_attempt_at,
        Some(1300)
    );
}

#[test]
fn actual_security_challenge_blocks_background_attempts_until_explicit_authorization() {
    let (supervisor, clock, store) = fixture(1000, 30);
    store.save("pikpak", br#"{"expires_at":2000}"#).unwrap();
    supervisor
        .ready(&store, CloudProvider::Pikpak, None, None, Some(2000))
        .unwrap();
    let result: Result<(), _> = supervisor.run(
        &store,
        CloudProvider::Pikpak,
        SessionWorkKind::Read,
        |_| Err(CloudError::CaptchaRequired),
        |_| panic!(),
    );
    assert!(matches!(result, Err(CloudError::CaptchaRequired)));
    assert_eq!(
        supervisor
            .record(&store, CloudProvider::Pikpak)
            .unwrap()
            .health,
        AuthHealth::RiskBlocked
    );
    clock.store(5000, Ordering::SeqCst);
    assert!(!supervisor.is_due(&store, CloudProvider::Pikpak).unwrap());
}

#[cfg(windows)]
#[test]
fn windows_maintenance_backoff_survives_reopening_the_credential_store() {
    let root = tempfile::tempdir().unwrap();
    {
        let store = SystemCredentialStore::new(root.path().to_path_buf()).unwrap();
        let supervisor = AuthSupervisor::new(|| 1000, || 30);
        assert!(matches!(
            supervisor.maintain(&store, CloudProvider::Tianyi, None, true, || Err(
                CloudError::AuthorizationExpired
            )),
            Err(CloudError::AuthorizationExpired)
        ));
    }
    let store = SystemCredentialStore::new(root.path().to_path_buf()).unwrap();
    let restarted = AuthSupervisor::new(|| 1100, || 30);
    assert_eq!(
        restarted
            .record(&store, CloudProvider::Tianyi)
            .unwrap()
            .next_allowed_attempt_at,
        Some(1300)
    );
    assert!(matches!(
        restarted.maintain(&store, CloudProvider::Tianyi, None, true, || panic!(
            "no network before persisted deadline"
        )),
        Err(CloudError::WebAuthorizationRequired)
    ));
    assert!(
        std::fs::read_dir(root.path()).unwrap().next().is_none(),
        "Windows credentials must remain in Credential Manager, not local files"
    );
    store.remove("cloud-auth-maintenance-tianyi").unwrap();
}
