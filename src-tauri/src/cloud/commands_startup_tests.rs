use super::{run_when_initialized, CloudProvider, CloudState, QuarkError};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};
use std::time::Duration;

#[test]
fn unavailable_credential_storage_does_not_reset_or_delete_saved_accounts() {
    struct Unavailable;
    impl crate::cloud::CredentialStore for Unavailable {
        fn load(&self, _: &str) -> Result<Option<Vec<u8>>, QuarkError> { Err(QuarkError::Storage) }
        fn save(&self, _: &str, _: &[u8]) -> Result<(), QuarkError> { panic!("must not overwrite credentials after a failed read") }
        fn remove(&self, _: &str) -> Result<(), QuarkError> { panic!("must not remove credentials after a failed read") }
    }
    let supervisor = crate::cloud::auth_supervisor::AuthSupervisor::default();
    assert!(super::restore_startup_authorizations(&supervisor, &Unavailable));
    assert!(matches!(supervisor.restore(&Unavailable, CloudProvider::Quark), Err(QuarkError::Storage)));
}

// An unusable credential directory must degrade to "credentials unavailable"
// instead of aborting application startup.
#[test]
fn credential_store_init_failure_degrades_instead_of_aborting_startup() {
    let root = tempfile::tempdir().unwrap();
    let blocker = root.path().join("sessions");
    std::fs::write(&blocker, b"a file where a directory is required").unwrap();
    let state = CloudState::new(blocker).expect("startup must not abort");
    assert!(state.credential_restore_failed);
}

fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

// A command arriving before setup registers CloudState must eventually run
// its real provider restore, rather than panicking inside the blocking task.
#[test]
fn delayed_registration_allows_all_enabled_provider_startup_restores() {
    let root = tempfile::tempdir().unwrap();
    // Construct the real HTTP client outside any async runtime; no requests
    // occur because all seven isolated credential-store entries are absent.
    let state = Arc::new(CloudState::new(root.path().join("sessions")).unwrap());
    let registry = Arc::new(OnceLock::new());
    let initial_lookups = Arc::new(AtomicUsize::new(0));
    let providers = CloudProvider::ALL
        .into_iter()
        .filter(|provider| provider.is_enabled())
        .collect::<Vec<_>>();
    let provider_count = providers.len();
    test_runtime().block_on(async {
        let published = Arc::clone(&registry);
        let ready_state = Arc::clone(&state);
        let arrived = Arc::clone(&initial_lookups);
        let registration = tokio::spawn(async move {
            while arrived.load(Ordering::SeqCst) < provider_count {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(published.set(ready_state).is_ok());
        });
        let restores = providers.into_iter().map(|provider| {
            let registry = Arc::clone(&registry);
            let initial_lookups = Arc::clone(&initial_lookups);
            run_when_initialized(
                move || {
                    let state = registry.get().cloned();
                    if state.is_none() {
                        initial_lookups.fetch_add(1, Ordering::SeqCst);
                    }
                    state
                },
                provider,
                move |state, http| super::restore_account(provider, &state.store, http),
                Duration::from_secs(1),
            )
        });
        for result in futures_util::future::join_all(restores).await {
            assert!(
                matches!(result, Err(QuarkError::NotAuthorized)),
                "{result:?}"
            );
        }
        registration.await.unwrap();
    });
}

// Missing initialization has a bounded, specific outcome and must not run
// account work or turn an absent state into an undifferentiated JoinError.
#[test]
fn missing_registration_times_out_without_running_provider_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    test_runtime().block_on(async {
        let started = tokio::time::Instant::now();
        let work_calls = Arc::clone(&calls);
        let result = run_when_initialized(
            || None,
            CloudProvider::Xunlei,
            move |_, _| {
                work_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            Duration::from_millis(100),
        )
        .await;
        assert!(matches!(result, Err(QuarkError::Transport(message))
            if message.contains("初始化") && !message.contains("任务中断")));
        assert_eq!(started.elapsed(), Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

// Once initialization succeeds, real operation errors must escape unchanged
// and the operation must never be retried by the readiness wait.
#[test]
fn initialized_provider_error_is_returned_once_without_startup_retries() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(CloudState::new(root.path().join("sessions")).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    test_runtime().block_on(async {
        let work_calls = Arc::clone(&calls);
        let lookup_state = Arc::clone(&state);
        let result: Result<(), QuarkError> = run_when_initialized(
            move || Some(Arc::clone(&lookup_state)),
            CloudProvider::Baidu,
            move |_, _| {
                work_calls.fetch_add(1, Ordering::SeqCst);
                Err(QuarkError::Provider("fixture-provider-rejection".into()))
            },
            Duration::from_secs(1),
        ).await;
        assert!(matches!(result, Err(QuarkError::Provider(message)) if message == "fixture-provider-rejection"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}
