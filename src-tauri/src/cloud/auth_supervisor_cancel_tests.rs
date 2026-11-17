use super::*;
use std::cell::Cell;

fn fixture() -> (AuthSupervisor, MemoryCredentialStore) {
    let supervisor = AuthSupervisor::new(|| 100, || 30);
    let store = MemoryCredentialStore::default();
    store.save(CloudProvider::Quark.key(), b"original-session").unwrap();
    supervisor.ready(&store, CloudProvider::Quark, Some("account".into()), Some(CloudAuthMethod::Web), None).unwrap();
    (supervisor, store)
}

#[test]
fn auth_mutation_late_cancel_preserves_known_success_without_saving_credentials_or_recovering() {
    let (supervisor, store) = fixture();
    let cancelled = Cell::new(false);
    let before = supervisor.record(&store, CloudProvider::Quark).unwrap().session_revision;
    let result = supervisor.run_checked(
        &store, CloudProvider::Quark, SessionWorkKind::Mutation,
        |snapshot| {
            snapshot.save(CloudProvider::Quark.key(), b"rotated-session")?;
            cancelled.set(true);
            Ok("remote-file-created")
        },
        |_| panic!("cancelled mutation must not recover or replay"),
        || if cancelled.get() { Err(crate::cloud::upload_cancelled()) } else { Ok(()) },
    );
    assert_eq!(result.unwrap(), "remote-file-created");
    assert_eq!(store.load(CloudProvider::Quark.key()).unwrap().unwrap(), b"original-session");
    assert_eq!(supervisor.record(&store, CloudProvider::Quark).unwrap().session_revision, before);
}

#[test]
fn auth_mutation_cancel_at_persistence_gate_keeps_success_and_discards_rotated_credentials() {
    let (supervisor, store) = fixture();
    let checks = Cell::new(0);
    let result = supervisor.run_checked(
        &store, CloudProvider::Quark, SessionWorkKind::Mutation,
        |snapshot| {
            snapshot.save(CloudProvider::Quark.key(), b"rotated-session")?;
            Ok("remote-share-created")
        },
        |_| panic!("cancelled mutation must not recover"),
        || {
            checks.set(checks.get() + 1);
            // Initial check, pre-work check, post-work check pass. Cancellation
            // becomes visible only once persistence owns the provider gate.
            if checks.get() >= 4 { Err(crate::cloud::upload_cancelled()) } else { Ok(()) }
        },
    );
    assert_eq!(result.unwrap(), "remote-share-created");
    assert_eq!(checks.get(), 4);
    assert_eq!(store.load(CloudProvider::Quark.key()).unwrap().unwrap(), b"original-session");
}

#[test]
fn auth_mutation_cancel_does_not_hide_an_uncertain_transport_result() {
    let (supervisor, store) = fixture();
    let cancelled = Cell::new(false);
    let result: Result<(), CloudError> = supervisor.run_checked(
        &store, CloudProvider::Quark, SessionWorkKind::Mutation,
        |snapshot| {
            snapshot.save(CloudProvider::Quark.key(), b"rotated-session")?;
            cancelled.set(true);
            Err(CloudError::Transport("commit response lost".into()))
        },
        |_| panic!("unknown mutation result must not recover or replay"),
        || if cancelled.get() { Err(crate::cloud::upload_cancelled()) } else { Ok(()) },
    );
    assert!(matches!(result, Err(CloudError::Transport(message)) if message == "commit response lost"));
    assert_eq!(store.load(CloudProvider::Quark.key()).unwrap().unwrap(), b"original-session");
}

#[test]
fn auth_cancelled_reads_still_discard_results_and_credentials() {
    let (supervisor, store) = fixture();
    let cancelled = Cell::new(false);
    let result = supervisor.run_checked(
        &store, CloudProvider::Quark, SessionWorkKind::Read,
        |snapshot| {
            snapshot.save(CloudProvider::Quark.key(), b"rotated-session")?;
            cancelled.set(true);
            Ok("private-read-result")
        },
        |_| panic!("cancelled reads must not recover"),
        || if cancelled.get() { Err(crate::cloud::upload_cancelled()) } else { Ok(()) },
    );
    assert!(result.is_err());
    assert_eq!(store.load(CloudProvider::Quark.key()).unwrap().unwrap(), b"original-session");
}
