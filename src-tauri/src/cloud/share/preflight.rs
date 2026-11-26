#[cfg(test)]
use super::CloudShareEntry;
use super::{
    format_confirmation_size, CloudShareAdapter, CloudShareCleanupState, CloudShareDownloadEntry,
    CloudShareDownloadSelection, CloudSharePlan, CloudSharePlanStore, CloudShareResolvedEntry,
    CloudShareTransferOwnerScheme, QuarkError, ShareResolution,
};
use crate::cloud::{CloudProvider, ProviderRegistry};
use std::collections::BTreeSet;

pub use super::CloudTransferCapacity;

/// Verify that every pending share entry can be copied to the current personal
/// drive before the command layer creates any local download tasks.
pub fn preflight_selected_share_download(
    store: &CloudSharePlanStore,
    adapter: &dyn CloudShareAdapter,
    selection: &CloudShareDownloadSelection,
) -> Result<(), QuarkError> {
    let Some(transfer) = adapter.transfer_capability() else {
        return Ok(());
    };
    let mut plan = store.preflight_snapshot(&selection.plan_source)?.plan;
    if plan.provider != selection.provider {
        return Err(QuarkError::InvalidResponse);
    }
    let first_selected = selection
        .entries
        .first()
        .ok_or_else(|| QuarkError::InvalidInput("请至少选择一个分享文件".into()))?;
    let first = resolved_selection_entry(&plan, first_selected)?;
    let owner = transfer.transfer_owner_fingerprint(&first)?;
    let reusable = reusable_entry_fingerprints(&plan, owner.as_deref());
    let pending = selection
        .entries
        .iter()
        .filter(|selected| !reusable.contains(&selected.selected_id))
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(());
    }

    let mut representative = resolved_selection_entry(&plan, pending[0])?;
    let resolution = adapter.resolve_with_context(&representative)?;
    if let Some(context) = resolution.updated_context {
        store.update_provider_context(&selection.plan_source, &context)?;
        plan.provider_context = context;
        representative.plan = plan.clone();
    }
    match resolution.result? {
        ShareResolution::Direct(_) => Ok(()),
        ShareResolution::TransferRequired(request) if request.source == pending[0].source => {
            let required = required_transfer_bytes(&pending)?;
            let capacity = transfer
                .transfer_capacity(&representative)
                .map_err(|error| capacity_query_error(selection.provider, error))?;
            ensure_capacity(selection.provider, required, capacity)
        }
        ShareResolution::TransferRequired(_) => Err(QuarkError::InvalidResponse),
    }
}

fn resolved_selection_entry(
    plan: &CloudSharePlan,
    selected: &CloudShareDownloadEntry,
) -> Result<CloudShareResolvedEntry, QuarkError> {
    let entry = plan
        .entries
        .iter()
        .find(|entry| entry.entry_fingerprint == selected.selected_id)
        .cloned()
        .ok_or(QuarkError::InvalidResponse)?;
    if selected.source != plan.source_for(&entry.entry_id) {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(CloudShareResolvedEntry {
        provider: plan.provider,
        plan: plan.clone(),
        entry,
    })
}

fn reusable_entry_fingerprints(plan: &CloudSharePlan, owner: Option<&str>) -> BTreeSet<String> {
    let Some(owner) = owner else {
        return BTreeSet::new();
    };
    plan.transfers
        .iter()
        .filter(|transfer| {
            transfer.cleanup_state == CloudShareCleanupState::Pending
                && transfer.owner_scheme == CloudShareTransferOwnerScheme::AccountV2
                && transfer.owner_fingerprint == owner
        })
        .map(|transfer| transfer.entry_fingerprint.clone())
        .collect()
}

fn required_transfer_bytes(pending: &[&CloudShareDownloadEntry]) -> Result<u64, QuarkError> {
    pending.iter().try_fold(0_u64, |total, selected| {
        let size = selected.size.ok_or_else(|| {
            QuarkError::InvalidInput(
                "无法确定本次转存所需空间，未建立下载任务。请取消大小未知的文件后重试。".into(),
            )
        })?;
        total.checked_add(size).ok_or_else(|| {
            QuarkError::InvalidInput(
                "无法确定本次转存所需空间，未建立下载任务。请取消大小未知的文件后重试。".into(),
            )
        })
    })
}

fn capacity_query_error(provider: CloudProvider, error: QuarkError) -> QuarkError {
    match error {
        error @ (QuarkError::NotAuthorized
        | QuarkError::AuthorizationExpired
        | QuarkError::SessionRenewedRetryOperation
        | QuarkError::WebAuthorizationRequired
        | QuarkError::CaptchaRequired
        | QuarkError::PikpakOperationVerificationRequired(_)) => error,
        _ => QuarkError::InvalidInput(format!(
            "无法读取{}可用转存空间，未建立下载任务，请稍后重试。",
            ProviderRegistry::global().get(provider).share_display_name
        )),
    }
}

fn ensure_capacity(
    provider: CloudProvider,
    required: u64,
    capacity: CloudTransferCapacity,
) -> Result<(), QuarkError> {
    let CloudTransferCapacity::Limited {
        total_bytes,
        used_bytes,
    } = capacity
    else {
        return Ok(());
    };
    let available = total_bytes
        .checked_sub(used_bytes)
        .ok_or_else(|| capacity_query_error(provider, QuarkError::InvalidResponse))?;
    if required <= available {
        return Ok(());
    }
    Err(QuarkError::InvalidInput(format!(
        "{}转存空间不足：本次需要 {}，当前可用 {}，未建立下载任务。",
        ProviderRegistry::global().get(provider).share_display_name,
        format_confirmation_size(required),
        format_confirmation_size(available),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::share::{
        selected_share_download, test_support, ShareInspect, ShareInspection, ShareOperationResult,
        ShareResolve, ShareTransfer, TransferRequest,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    enum ProbeReply {
        Direct(String),
        TransferRequired,
        RotatedError(serde_json::Value, QuarkError),
    }

    struct PreflightAdapter {
        owner: Option<String>,
        probe: Mutex<Option<ProbeReply>>,
        capacity: Mutex<Option<Result<CloudTransferCapacity, QuarkError>>>,
        resolve_calls: AtomicUsize,
        capacity_calls: AtomicUsize,
    }

    impl PreflightAdapter {
        fn new() -> Self {
            Self {
                owner: Some("a".repeat(64)),
                probe: Mutex::new(None),
                capacity: Mutex::new(Some(Ok(CloudTransferCapacity::Unlimited { used_bytes: 0 }))),
                resolve_calls: AtomicUsize::new(0),
                capacity_calls: AtomicUsize::new(0),
            }
        }

        fn resolve_direct(&self, url: &str) {
            *self.probe.lock().unwrap() = Some(ProbeReply::Direct(url.into()));
        }

        fn require_transfer(&self) {
            *self.probe.lock().unwrap() = Some(ProbeReply::TransferRequired);
        }

        fn capacity(&self, value: CloudTransferCapacity) {
            *self.capacity.lock().unwrap() = Some(Ok(value));
        }

        fn capacity_error(&self, error: QuarkError) {
            *self.capacity.lock().unwrap() = Some(Err(error));
        }

        fn resolve_calls(&self) -> usize {
            self.resolve_calls.load(Ordering::SeqCst)
        }

        fn capacity_calls(&self) -> usize {
            self.capacity_calls.load(Ordering::SeqCst)
        }
    }

    impl ShareInspect for PreflightAdapter {
        fn inspect(
            &self,
            _: &crate::parser::CloudShareCandidate,
        ) -> Result<ShareInspection, QuarkError> {
            unreachable!("preflight never inspects a share")
        }
    }

    impl ShareResolve for PreflightAdapter {
        fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
            self.resolve_with_context(entry)?.result
        }

        fn resolve_with_context(
            &self,
            entry: &CloudShareResolvedEntry,
        ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            match self
                .probe
                .lock()
                .unwrap()
                .take()
                .expect("one configured probe")
            {
                ProbeReply::Direct(url) => Ok(ShareOperationResult {
                    result: Ok(ShareResolution::Direct(
                        crate::cloud::commands::ResolvedCloudDownload {
                            transform: None,
                            url,
                            name: entry.entry.name.clone(),
                            headers: vec![],
                        },
                    )),
                    updated_context: None,
                }),
                ProbeReply::TransferRequired => Ok(ShareOperationResult {
                    result: Ok(ShareResolution::TransferRequired(TransferRequest {
                        source: entry.plan.source_for(&entry.entry.entry_id),
                    })),
                    updated_context: None,
                }),
                ProbeReply::RotatedError(context, error) => Ok(ShareOperationResult {
                    result: Err(error),
                    updated_context: Some(context),
                }),
            }
        }
    }

    impl ShareTransfer for PreflightAdapter {
        fn transfer_capacity(
            &self,
            _: &CloudShareResolvedEntry,
        ) -> Result<CloudTransferCapacity, QuarkError> {
            self.capacity_calls.fetch_add(1, Ordering::SeqCst);
            self.capacity
                .lock()
                .unwrap()
                .take()
                .expect("one configured capacity")
        }

        fn transfer_owner_fingerprint(
            &self,
            _: &CloudShareResolvedEntry,
        ) -> Result<Option<String>, QuarkError> {
            Ok(self.owner.clone())
        }

        fn transfer(
            &self,
            _: &CloudShareResolvedEntry,
            _: &CloudSharePlanStore,
        ) -> Result<crate::cloud::commands::ResolvedCloudDownload, QuarkError> {
            panic!("capacity preflight must not transfer")
        }
    }

    impl CloudShareAdapter for PreflightAdapter {
        fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
            Some(self)
        }
    }

    struct ReadOnlyAdapter;

    impl ShareInspect for ReadOnlyAdapter {
        fn inspect(
            &self,
            _: &crate::parser::CloudShareCandidate,
        ) -> Result<ShareInspection, QuarkError> {
            unreachable!("preflight never inspects a share")
        }
    }

    impl ShareResolve for ReadOnlyAdapter {
        fn resolve(&self, _: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
            unreachable!("a provider without transfer capability skips preflight")
        }
    }

    impl CloudShareAdapter for ReadOnlyAdapter {}

    struct PreflightFixture {
        store: CloudSharePlanStore,
        reads: std::sync::Arc<test_support::CountingPlanCredentials>,
        selection: CloudShareDownloadSelection,
        adapter: PreflightAdapter,
    }

    fn preflight_fixture(count: usize, size: Option<u64>) -> PreflightFixture {
        let (store, reads) = test_support::counted_plan_store();
        let entries = (0..count)
            .map(|index| {
                CloudShareEntry::new(
                    "share-capacity",
                    format!("entry-{index}"),
                    format!("file-{index}.bin"),
                    vec![],
                    size,
                )
            })
            .collect::<Vec<_>>();
        let plan = CloudSharePlan::new(
            CloudProvider::Aliyun,
            "share-capacity".into(),
            None,
            entries,
            serde_json::json!({"token":"old"}),
        );
        let plan_source = plan.source_for(&plan.entries[0].entry_id);
        let selected_ids = plan
            .entries
            .iter()
            .map(|entry| entry.entry_fingerprint.clone())
            .collect::<Vec<_>>();
        store.upsert(&plan).unwrap();
        let selection = selected_share_download(&store, &plan_source, &selected_ids).unwrap();
        reads.reads.store(0, Ordering::SeqCst);
        reads.writes.store(0, Ordering::SeqCst);
        PreflightFixture {
            store,
            reads,
            selection,
            adapter: PreflightAdapter::new(),
        }
    }

    #[test]
    fn capacity_accepts_integer_and_decimal_string_fields() {
        assert_eq!(
            CloudTransferCapacity::from_json(&json!(100), &json!(40), false).unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 100,
                used_bytes: 40
            }
        );
        assert_eq!(
            CloudTransferCapacity::from_json(&json!("100"), &json!("40"), false).unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 100,
                used_bytes: 40
            }
        );
        assert_eq!(
            CloudTransferCapacity::Limited {
                total_bytes: 100,
                used_bytes: 40
            }
            .available_bytes(),
            Some(60)
        );
    }

    #[test]
    fn capacity_accepts_only_the_explicit_unlimited_sentinel() {
        assert_eq!(
            CloudTransferCapacity::from_json(&json!(-1), &json!(40), true).unwrap(),
            CloudTransferCapacity::Unlimited { used_bytes: 40 }
        );
        assert_eq!(
            CloudTransferCapacity::Unlimited { used_bytes: 40 }.available_bytes(),
            None
        );
        assert!(CloudTransferCapacity::from_json(&json!(-1), &json!(40), false).is_err());
    }

    #[test]
    fn capacity_rejects_missing_fractional_negative_and_inverted_values() {
        for (total, used) in [
            (json!(null), json!(1)),
            (json!(10.5), json!(1)),
            (json!(10), json!(-1)),
            (json!(10), json!(11)),
            (json!("+10"), json!("1")),
            (json!("ten"), json!("1")),
        ] {
            assert!(CloudTransferCapacity::from_json(&total, &used, false).is_err());
        }
    }

    #[test]
    fn direct_share_discards_probe_and_never_queries_capacity() {
        let fixture = preflight_fixture(2, Some(10));
        fixture
            .adapter
            .resolve_direct("https://cdn.example/signed?secret=1");
        preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection)
            .unwrap();
        assert_eq!(fixture.adapter.resolve_calls(), 1);
        assert_eq!(fixture.adapter.capacity_calls(), 0);
    }

    #[test]
    fn transfer_required_with_insufficient_space_reports_no_task_error() {
        let fixture = preflight_fixture(2, Some(60));
        fixture.adapter.require_transfer();
        fixture.adapter.capacity(CloudTransferCapacity::Limited {
            total_bytes: 100,
            used_bytes: 10,
        });
        let error =
            preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection)
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "阿里云盘转存空间不足：本次需要 120 B，当前可用 90 B，未建立下载任务。"
        );
        assert_eq!(fixture.adapter.resolve_calls(), 1);
        assert_eq!(fixture.adapter.capacity_calls(), 1);
    }

    #[test]
    fn unknown_size_and_capacity_failure_fail_closed() {
        let unknown = preflight_fixture(1, None);
        unknown.adapter.require_transfer();
        let error =
            preflight_selected_share_download(&unknown.store, &unknown.adapter, &unknown.selection)
                .unwrap_err();
        assert!(error.to_string().contains("无法确定本次转存所需空间"));
        assert_eq!(unknown.adapter.capacity_calls(), 0);

        let failed = preflight_fixture(1, Some(7));
        failed.adapter.require_transfer();
        failed
            .adapter
            .capacity_error(QuarkError::Transport("offline".into()));
        assert_eq!(
            preflight_selected_share_download(&failed.store, &failed.adapter, &failed.selection)
                .unwrap_err()
                .to_string(),
            "无法读取阿里云盘可用转存空间，未建立下载任务，请稍后重试。"
        );
    }

    #[test]
    fn capacity_failure_does_not_persist_a_preflight_snapshot() {
        let fixture = preflight_fixture(1, Some(7));
        fixture.adapter.require_transfer();
        fixture
            .adapter
            .capacity_error(QuarkError::Transport("offline".into()));

        assert!(preflight_selected_share_download(
            &fixture.store,
            &fixture.adapter,
            &fixture.selection,
        )
        .is_err());
        assert_eq!(fixture.reads.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_capacity_is_reported_as_a_preflight_read_failure() {
        let fixture = preflight_fixture(1, Some(7));
        fixture.adapter.require_transfer();
        fixture.adapter.capacity(CloudTransferCapacity::Limited {
            total_bytes: 6,
            used_bytes: 7,
        });

        assert_eq!(
            preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection)
                .unwrap_err()
                .to_string(),
            "无法读取阿里云盘可用转存空间，未建立下载任务，请稍后重试。"
        );
    }

    #[test]
    fn provider_without_transfer_capability_skips_preflight() {
        let fixture = preflight_fixture(1, Some(7));
        preflight_selected_share_download(&fixture.store, &ReadOnlyAdapter, &fixture.selection)
            .unwrap();
        assert_eq!(fixture.reads.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn current_owner_pending_transfers_are_excluded_but_other_records_are_not() {
        let fixture = preflight_fixture(2, Some(60));
        fixture
            .store
            .record_transfer_with_owner(
                &fixture.selection.entries[0].source,
                "personal-0",
                &"a".repeat(64),
            )
            .unwrap();
        fixture
            .store
            .record_transfer_with_owner(
                &fixture.selection.entries[1].source,
                "other-personal-1",
                &"b".repeat(64),
            )
            .unwrap();
        fixture.adapter.require_transfer();
        fixture.adapter.capacity(CloudTransferCapacity::Limited {
            total_bytes: 100,
            used_bytes: 30,
        });
        preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection)
            .unwrap();
    }

    #[test]
    fn all_current_owner_transfers_skip_resolve_and_capacity() {
        let fixture = preflight_fixture(2, Some(60));
        for (index, entry) in fixture.selection.entries.iter().enumerate() {
            fixture
                .store
                .record_transfer_with_owner(
                    &entry.source,
                    &format!("personal-{index}"),
                    &"a".repeat(64),
                )
                .unwrap();
        }
        preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection)
            .unwrap();
        assert_eq!(fixture.adapter.resolve_calls(), 0);
        assert_eq!(fixture.adapter.capacity_calls(), 0);
    }

    #[test]
    fn rotated_context_is_persisted_before_resolve_error_returns() {
        let fixture = preflight_fixture(1, Some(7));
        *fixture.adapter.probe.lock().unwrap() = Some(ProbeReply::RotatedError(
            serde_json::json!({"token":"rotated"}),
            QuarkError::AuthorizationExpired,
        ));
        assert!(matches!(
            preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection),
            Err(QuarkError::AuthorizationExpired)
        ));
        assert_eq!(fixture.reads.reads.load(Ordering::SeqCst), 2);
        assert_eq!(
            fixture
                .store
                .resolve_entry(&fixture.selection.plan_source)
                .unwrap()
                .plan
                .provider_context["token"],
            "rotated"
        );
    }

    #[test]
    fn large_selection_loads_once_resolves_once_and_queries_capacity_once() {
        let fixture = preflight_fixture(2_427, Some(1));
        fixture.adapter.require_transfer();
        fixture.adapter.capacity(CloudTransferCapacity::Limited {
            total_bytes: 10_000,
            used_bytes: 0,
        });
        preflight_selected_share_download(&fixture.store, &fixture.adapter, &fixture.selection)
            .unwrap();
        assert_eq!(fixture.reads.reads.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.adapter.resolve_calls(), 1);
        assert_eq!(fixture.adapter.capacity_calls(), 1);
    }

    #[test]
    fn capacity_overflow_fails_before_capacity_query() {
        let fixture = preflight_fixture(2, Some(u64::MAX));
        fixture.adapter.require_transfer();
        assert!(preflight_selected_share_download(
            &fixture.store,
            &fixture.adapter,
            &fixture.selection
        )
        .is_err());
        assert_eq!(fixture.adapter.capacity_calls(), 0);
    }
}
