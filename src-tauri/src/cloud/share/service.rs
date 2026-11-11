use super::*;

/// Cooperatively stops blocking share work between provider requests. A reply
/// already received is still processed so created file IDs reach the journal.
pub struct ShareWorkControl {
    cancelled: std::sync::atomic::AtomicBool,
    deadline: std::time::Instant,
}
impl ShareWorkControl {
    pub fn new(timeout: std::time::Duration) -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            deadline: std::time::Instant::now() + timeout,
        }
    }
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn check(&self) -> Result<(), QuarkError> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(QuarkError::Transport("分享下载准备已取消".into()));
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(QuarkError::Transport("分享下载准备超时".into()));
        }
        Ok(())
    }
    pub fn transport<'a>(
        &'a self,
        http: &'a dyn crate::cloud::CloudHttpTransport,
    ) -> impl crate::cloud::CloudHttpTransport + 'a {
        ControlledShareTransport {
            control: self,
            http,
        }
    }
}

pub fn resolve_current_share_download_cancellable(
    plans: &CloudSharePlanStore,
    credentials: &dyn CredentialStore,
    http: &dyn crate::cloud::CloudHttpTransport,
    source: &str,
    control: &ShareWorkControl,
) -> Result<crate::cloud::commands::ResolvedCloudDownload, QuarkError> {
    control.check()?;
    let (provider, _, _) = parse_stable_cloud_share_source(source)?;
    let http = control.transport(http);
    let adapter = provider_share_adapter(provider, credentials, &http)?;
    let result = resolve_current_share_download(plans, adapter.as_ref(), source);
    // Do not commit this operation's private session after caller cancellation
    // or timeout. Completed transfer IDs have already reached the journal.
    control.check()?;
    result
}
struct ControlledShareTransport<'a> {
    control: &'a ShareWorkControl,
    http: &'a dyn crate::cloud::CloudHttpTransport,
}
impl crate::cloud::CloudHttpTransport for ControlledShareTransport<'_> {
    fn send(
        &self,
        request: crate::cloud::CloudHttpRequest,
    ) -> Result<crate::cloud::CloudHttpResponse, QuarkError> {
        self.control.check()?;
        self.http.send(request)
    }
    fn send_with_cookie_snapshot(
        &self,
        request: crate::cloud::CloudHttpRequest,
    ) -> Result<(crate::cloud::CloudHttpResponse, Option<String>), QuarkError> {
        self.control.check()?;
        self.http.send_with_cookie_snapshot(request)
    }
    fn manages_cookie_session(&self) -> bool {
        self.http.manages_cookie_session()
    }
    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, QuarkError> {
        self.http.cookies_for_url(url)
    }
}

/// Retries only transient reads while a public share is being inspected. This
/// sits below provider pagination, so a timeout retries the one failed page
/// instead of replaying a successfully inspected directory or a mutation.
pub(crate) struct RetryingShareReadTransport<'a> {
    inner: &'a dyn crate::cloud::CloudHttpTransport,
}

impl<'a> RetryingShareReadTransport<'a> {
    const ATTEMPTS: usize = 3;

    pub(crate) fn new(inner: &'a dyn crate::cloud::CloudHttpTransport) -> Self {
        Self { inner }
    }
    fn retry<T>(&self, mut work: impl FnMut() -> Result<T, QuarkError>) -> Result<T, QuarkError> {
        for attempt in 0..Self::ATTEMPTS {
            match work() {
                Ok(response) => return Ok(response),
                Err(QuarkError::Transport(_)) if attempt + 1 < Self::ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(
                        250 * (attempt as u64 + 1),
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("a bounded retry loop always returns")
    }
}

impl crate::cloud::CloudHttpTransport for RetryingShareReadTransport<'_> {
    fn send(
        &self,
        request: crate::cloud::CloudHttpRequest,
    ) -> Result<crate::cloud::CloudHttpResponse, QuarkError> {
        self.retry(|| self.inner.send(request.clone()))
    }

    fn send_with_cookie_snapshot(
        &self,
        request: crate::cloud::CloudHttpRequest,
    ) -> Result<(crate::cloud::CloudHttpResponse, Option<String>), QuarkError> {
        self.retry(|| self.inner.send_with_cookie_snapshot(request.clone()))
    }

    fn check_transfer(&self) -> Result<(), QuarkError> {
        self.inner.check_transfer()
    }

    fn isolated_session(
        &self,
    ) -> Result<Option<Box<dyn crate::cloud::CloudHttpTransport>>, QuarkError> {
        self.inner.isolated_session()
    }

    fn manages_cookie_session(&self) -> bool {
        self.inner.manages_cookie_session()
    }

    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, QuarkError> {
        self.inner.cookies_for_url(url)
    }
}

/// The only cloud-share plan projection permitted across the IPC boundary.
/// Its source is an opaque, validated `cloud-share://` reference; credentials,
/// provider session context, and canonical share identifiers remain in encrypted local storage.
#[derive(Clone, Serialize)]
pub struct CloudShareConfirmation {
    pub plan_source: String,
    pub title: String,
    pub resources: Vec<CloudShareConfirmationResource>,
}

#[derive(Clone, Serialize)]
pub struct CloudShareConfirmationResource {
    pub id: String,
    pub name: String,
    pub size: String,
    pub selected: bool,
}

pub fn inspect_share_with_adapter(
    store: &CloudSharePlanStore,
    adapter: &dyn CloudShareAdapter,
    candidate: &crate::parser::CloudShareCandidate,
) -> Result<ShareConfirmationOutcome, QuarkError> {
    let plan = match adapter.inspect(candidate)? {
        ShareInspection::AccessCodeRequired => {
            return Ok(ShareConfirmationOutcome::AccessCodeRequired)
        }
        ShareInspection::Plan(plan) => plan,
    };
    if plan.provider != candidate.provider || plan.entries.is_empty() {
        return Err(QuarkError::InvalidResponse);
    }
    let automatic_source = match &plan.root_kind {
        CloudShareRootKind::File { entry_id }
            if plan.entries.len() == 1 && plan.entries[0].entry_id == *entry_id =>
        {
            Some(plan.source_for(entry_id))
        }
        _ => None,
    };
    let confirmation = CloudShareConfirmation {
        plan_source: plan.source_for(&plan.entries[0].entry_id),
        title: "网盘分享文件".into(),
        resources: plan
            .entries
            .iter()
            .map(|entry| CloudShareConfirmationResource {
                id: entry.entry_fingerprint.clone(),
                name: entry
                    .relative_directory
                    .iter()
                    .chain(std::iter::once(&entry.name))
                    .map(|name| crate::cloud::commands::safe_filename(name))
                    .collect::<Vec<_>>()
                    .join("/"),
                size: entry.size.map(format_confirmation_size).unwrap_or_default(),
                selected: true,
            })
            .collect(),
    };
    store.upsert(&plan)?;
    Ok(match automatic_source {
        Some(source) => ShareConfirmationOutcome::AutoSubmit {
            source,
            confirmation,
        },
        None => ShareConfirmationOutcome::Confirmation(confirmation),
    })
}

pub(super) fn format_confirmation_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudShareDownloadEntry {
    pub selected_id: String,
    pub source: String,
    pub name: String,
    pub relative_directory: Vec<String>,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudShareDownloadSelection {
    pub provider: CloudProvider,
    /// A stable, opaque source from the encrypted plan. It is safe to persist
    /// as the directory-group identity and contains no share code or token.
    pub plan_source: String,
    /// Present only when the selected share resources must be represented by
    /// one durable, expandable directory task.
    pub directory_name: Option<String>,
    pub entries: Vec<CloudShareDownloadEntry>,
}

pub(crate) fn provider_share_directory_name(provider: CloudProvider) -> &'static str {
    crate::cloud::ProviderRegistry::global()
        .get(provider)
        .share_directory_name
}

pub(super) fn parsed_share_directory_name(entries: &[CloudShareDownloadEntry]) -> Option<String> {
    let root = entries.first()?.relative_directory.first()?.trim();
    (!root.is_empty()
        && entries.iter().all(|entry| {
            entry
                .relative_directory
                .first()
                .is_some_and(|component| component == root)
        }))
    .then(|| root.to_owned())
}

pub fn selected_share_download(
    store: &CloudSharePlanStore,
    plan_source: &str,
    selected_ids: &[String],
) -> Result<CloudShareDownloadSelection, QuarkError> {
    if selected_ids.is_empty() {
        return Err(QuarkError::InvalidInput("请至少选择一个分享文件".into()));
    }
    let plan = store.resolve_entry(plan_source)?.plan;
    let mut selected = BTreeSet::new();
    let entries = selected_ids
        .iter()
        .map(|id| {
            let entry = plan
                .entries
                .iter()
                .find(|entry| &entry.entry_fingerprint == id)
                .filter(|_| selected.insert(id))
                .ok_or_else(missing_plan)?;
            Ok(CloudShareDownloadEntry {
                selected_id: id.clone(),
                source: plan.source_for(&entry.entry_id),
                name: entry.name.clone(),
                relative_directory: entry.relative_directory.clone(),
                size: entry.size,
            })
        })
        .collect::<Result<Vec<_>, QuarkError>>()?;
    let exact_shared_file = matches!(
        &plan.root_kind,
        CloudShareRootKind::File { entry_id }
            if entries.len() == 1
                && plan.entries.iter().any(|entry| {
                    entry.entry_id == *entry_id
                        && entry.entry_fingerprint == entries[0].selected_id
                })
    );
    let grouped = !exact_shared_file
        && (matches!(
            plan.root_kind,
            CloudShareRootKind::Directory | CloudShareRootKind::Collection
        ) || entries.len() > 1
            || entries
                .iter()
                .any(|entry| !entry.relative_directory.is_empty()));
    Ok(CloudShareDownloadSelection {
        provider: plan.provider,
        plan_source: plan_source.to_owned(),
        directory_name: grouped.then(|| {
            parsed_share_directory_name(&entries)
                .unwrap_or_else(|| provider_share_directory_name(plan.provider).to_owned())
        }),
        entries,
    })
}

pub fn selected_share_sources(
    store: &CloudSharePlanStore,
    plan_source: &str,
    selected_ids: &[String],
) -> Result<Vec<String>, QuarkError> {
    Ok(selected_share_download(store, plan_source, selected_ids)?
        .entries
        .into_iter()
        .map(|entry| entry.source)
        .collect())
}

pub(crate) fn validate_direct_url(url: &str) -> Result<(), QuarkError> {
    // Do not let URL normalization strip controls that hide authority syntax.
    if url.chars().any(char::is_control) {
        return Err(QuarkError::InvalidResponse);
    }
    let parsed = url::Url::parse(url).map_err(|_| QuarkError::InvalidResponse)?;
    // URL parsing normalizes empty userinfo away. Check the original authority
    // too, without rejecting ordinary @ characters in paths or query strings.
    let authority = url
        .split_once(':')
        .map(|(_, rest)| rest.trim_start_matches(['/', '\\']))
        .and_then(|rest| rest.split(['/', '\\', '?', '#']).next())
        .ok_or(QuarkError::InvalidResponse)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || authority.contains('@')
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(
            crate::parser::extract_cloud_share_candidate(url, None),
            Ok(None)
        )
    {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(())
}

pub fn resolve_current_share_download(
    store: &CloudSharePlanStore,
    adapter: &dyn CloudShareAdapter,
    source: &str,
) -> Result<crate::cloud::commands::ResolvedCloudDownload, QuarkError> {
    let mut entry = store.resolve_entry(source)?;
    // A personal-drive file recorded for this exact source can outlive a
    // revoked/expired share. Refresh it before touching the share endpoint so
    // a normal retry neither depends on the share still existing nor creates a
    // duplicate transfer. The second owner check makes this fail closed after
    // a credential/session rotation.
    if let Some(download) = refresh_reusable_transfer(store, adapter, source, &mut entry)? {
        return Ok(download);
    }
    let resolved = adapter.resolve_with_context(&entry)?;
    if let Some(context) = resolved.updated_context {
        store.update_provider_context(source, &context)?;
        entry.plan.provider_context = context;
    }
    let mut download = match resolved.result? {
        ShareResolution::Direct(download) => download,
        ShareResolution::TransferRequired(request) => {
            if request.source != source {
                return Err(QuarkError::InvalidResponse);
            }
            let reused = refresh_reusable_transfer(store, adapter, source, &mut entry)?;
            if let Some(download) = reused {
                download
            } else {
                let transferred = adapter
                    .transfer_capability()
                    .ok_or(QuarkError::ProviderUnsupported)?
                    .transfer_with_context(&entry, store)?;
                if let Some(context) = transferred.updated_context {
                    store.update_provider_context(source, &context)?;
                }
                transferred.result?
            }
        }
    };
    validate_direct_url(&download.url)?;
    download.name = entry.entry.name;
    Ok(download)
}

fn refresh_reusable_transfer(
    store: &CloudSharePlanStore,
    adapter: &dyn CloudShareAdapter,
    source: &str,
    entry: &mut CloudShareResolvedEntry,
) -> Result<Option<crate::cloud::commands::ResolvedCloudDownload>, QuarkError> {
    if !store.has_pending_transfer(source)? {
        return Ok(None);
    }
    let Some(owner) = adapter
        .transfer_capability()
        .ok_or(QuarkError::ProviderUnsupported)?
        .transfer_owner_fingerprint(entry)?
    else {
        return Ok(None);
    };
    let Some(record) = store.reusable_transfer(source, &owner)? else {
        return Ok(None);
    };
    let refreshed = adapter
        .recorded_capability()
        .ok_or(QuarkError::ProviderUnsupported)?
        .refresh_recorded_transfer(entry, &record.created_file_id)
        .and_then(|refreshed| {
            if let Some(context) = refreshed.updated_context {
                store.update_provider_context(source, &context)?;
                entry.plan.provider_context = context;
            }
            refreshed.result
        });
    match refreshed {
        Ok(RecordedTransferDownload::Available(mut download)) => {
            validate_direct_url(&download.url)
                .map_err(|_| QuarkError::ShareTransferDownloadUnavailable)?;
            download.name = entry.entry.name.clone();
            Ok(Some(download))
        }
        // A saved transfer is merely a cache. If its personal file was
        // deleted or moved to a recycle bin, remove exactly that record and
        // continue through the normal share resolution/transfer flow.
        Err(QuarkError::SourceFileMissing) => {
            store.forget_unavailable_reusable_transfer(source, &record.created_file_id, &owner)?;
            Ok(None)
        }
        Err(QuarkError::InvalidResponse) => Err(QuarkError::ShareTransferDownloadUnavailable),
        Err(error) => Err(error),
    }
}

pub(crate) fn share_id(
    candidate: &crate::parser::CloudShareCandidate,
    provider: CloudProvider,
) -> Result<String, QuarkError> {
    if candidate.provider != provider {
        return Err(QuarkError::InvalidResponse);
    }
    let url = url::Url::parse(&candidate.url).map_err(|_| QuarkError::InvalidResponse)?;
    let descriptor = crate::cloud::ProviderRegistry::global().get(provider);
    if !descriptor.accepts_share_url(&url) {
        return Err(QuarkError::InvalidResponse);
    }
    let id = descriptor
        .share_patterns
        .iter()
        .find_map(|prefix| url.path().strip_prefix(prefix))
        .filter(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .ok_or(QuarkError::InvalidResponse)?;
    Ok(id.into())
}

pub(crate) fn download_headers(
    cookie: &str,
    response_cookies: &[String],
    ua: &str,
    referer: Option<&str>,
) -> Vec<(String, String)> {
    let mut pairs = BTreeMap::new();
    for part in cookie.split(';').chain(
        response_cookies
            .iter()
            .filter_map(|raw| raw.split(';').next()),
    ) {
        if let Some((name, value)) = part.trim().split_once('=') {
            if !name.is_empty() && !value.chars().any(char::is_control) {
                pairs.insert(name, value);
            }
        }
    }
    let mut headers = vec![
        (
            "Cookie".into(),
            pairs
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        ),
        ("User-Agent".into(), ua.into()),
    ];
    if let Some(referer) = referer {
        headers.push(("Referer".into(), referer.into()));
    }
    headers
}

/// This consumed budget is separate from the source's RAII submission lock.
/// Dropping that lock on failure must not re-arm automatic retries.
#[derive(Default)]
pub struct CloudShareRefreshBudget(Mutex<BTreeSet<String>>);

impl CloudShareRefreshBudget {
    pub fn claim(&self, source: &str, aria_error_code: Option<&str>) -> Result<bool, QuarkError> {
        // 22 and 24 mean the signed URL was rejected.  8 means the CDN
        // cannot resume the partial response. All require a newly resolved
        // URL; code 8 preserves the verified local ranges. Native
        // stream code 6 also gets one fresh URL after transport retries are
        // exhausted, but must preserve already verified ranges.
        if !source.starts_with("cloud-share://")
            || !matches!(aria_error_code, Some("6" | "8" | "22" | "24"))
        {
            return Ok(false);
        }
        parse_stable_cloud_share_source(source)?;
        Ok(self
            .0
            .lock()
            .map_err(|_| QuarkError::Storage)?
            .insert(source.into()))
    }
}

/// URL and headers go directly to aria2, while only the opaque source and safe
/// filename enter SQLite. Callers hold the source gate for duplicate detection.
pub async fn submit_current_share_download(
    service: &crate::DownloadService,
    prepared: &crate::DownloadTask,
    resolved: crate::cloud::commands::ResolvedCloudDownload,
    configured: crate::UriSubmissionOptions,
) -> Result<crate::DownloadTask, QuarkError> {
    parse_stable_cloud_share_source(&prepared.source)?;
    let (signed_url, options) = share_submission_options(resolved, configured)?;
    let task = service
        .store()
        .find(prepared.id)
        .map_err(|_| QuarkError::Storage)?;
    if task.source != prepared.source
        || task.kind != "cloud"
        || !matches!(
            task.status,
            crate::TaskStatus::Parsing | crate::TaskStatus::Queued
        )
        || task.aria_gid.is_some()
    {
        return Err(QuarkError::InvalidResponse);
    }
    match service
        .submit_uri_with_options(task.id, vec![signed_url], options)
        .await
    {
        Ok(task) => Ok(task),
        Err(_) => {
            // RPC failures may contain signed URIs. Never persist or expose them.
            let message = "aria2 未能接受网盘分享下载任务";
            let _ = service.store().fail(task.id, message);
            Err(QuarkError::Transport(message.into()))
        }
    }
}

pub async fn resubmit_current_share_download(
    service: &crate::DownloadService,
    expected: &crate::DownloadTask,
    resolved: crate::cloud::commands::ResolvedCloudDownload,
    configured: crate::UriSubmissionOptions,
) -> Result<Option<crate::DownloadTask>, QuarkError> {
    parse_stable_cloud_share_source(&expected.source)?;
    resubmit_current_cloud_download(service, expected, resolved, configured).await
}

/// Compare the durable task again after a potentially interactive login, so
/// cancellation, recycling and manual retries always win over auto recovery.
pub async fn resubmit_current_cloud_download(
    service: &crate::DownloadService,
    expected: &crate::DownloadTask,
    resolved: crate::cloud::commands::ResolvedCloudDownload,
    mut configured: crate::UriSubmissionOptions,
) -> Result<Option<crate::DownloadTask>, QuarkError> {
    if expected.source.starts_with("cloud-share://") {
        parse_stable_cloud_share_source(&expected.source)?;
    } else {
        crate::cloud::commands::parse_stable_cloud_source(&expected.source)?;
    }
    if let Some(file) = service
        .store()
        .files(expected.id)
        .map_err(|_| QuarkError::Storage)?
        .first()
    {
        let path = std::path::Path::new(&file.path);
        if path.is_absolute() {
            configured.directory = path.parent().map(std::path::Path::to_path_buf);
            configured.output_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned);
        }
        if configured.output_name.is_none() {
            configured.output_name = Some(expected.name.clone());
        }
    }
    let (signed_url, options) = share_submission_options(resolved, configured)?;
    service
        .compare_and_resubmit_uri_with_options(expected, vec![signed_url], options)
        .await
        .map_err(crate::cloud::commands::cloud_download_submission_error)
}

pub(crate) fn share_submission_options(
    resolved: crate::cloud::commands::ResolvedCloudDownload,
    configured: crate::UriSubmissionOptions,
) -> Result<(String, crate::UriSubmissionOptions), QuarkError> {
    validate_direct_url(&resolved.url)?;
    let signed_url = resolved.url.clone();
    let mut options = resolved.into_uri_submission_options();
    options.directory = configured.directory;
    options.output_name = configured.output_name.or(options.output_name);
    options.proxy = configured.proxy;
    options.max_connections_per_server = configured.max_connections_per_server;
    options.max_tries = configured.max_tries;
    options.retry_wait_seconds = configured.retry_wait_seconds;
    options.native_resume_gid = configured.native_resume_gid;
    options.allow_overwrite = configured.allow_overwrite;
    options.continue_download = configured.continue_download;
    options
        .clone()
        .into_aria2_options()
        .map_err(|_| QuarkError::InvalidResponse)?;
    Ok((signed_url, options))
}
