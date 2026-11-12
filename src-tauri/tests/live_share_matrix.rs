//! Opt-in checks of an external share matrix against the logged-in VD accounts.
//!
//! Matrix JSON is an array of {id, provider, input}, with optional access_code
//! and select_name. Inputs may contain the complete copied share message.
//! Required: VERSADOWN_LIVE_CLOUD=1, VERSADOWN_LIVE_SHARE_MATRIX and
//! VERSADOWN_LIVE_SHARE_WORK_DIR (a dedicated, persistent test directory).
//! VERSADOWN_LIVE_SESSION_DIR selects the existing VD cloud-sessions directory;
//! it is required outside Windows and defaults to VD's APPDATA directory on Windows.
//! VERSADOWN_LIVE_SHARE_CASE optionally selects one case by id.
//! VERSADOWN_LIVE_UC_DIAGNOSTICS=1 adds UC-only HTTP/code and CSRF-presence
//! states. It never prints cookie, header, response or token values.
//!
//! Inspection never submits a download or transfers a file. The separate range
//! check additionally requires VERSADOWN_LIVE_SHARE_TRANSFER=1 and may save at
//! most one file per case when required by the provider. It retains the test's
//! encrypted transfer journal for subsequent runs and never deletes cloud files.
//! Range reads write at most 64 KiB per case to the dedicated downloads directory.
//! Reports contain only case/provider, phase/state, resource and byte counts.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use versadown_lib::cloud::{
    commands::{restore_account, ResolvedCloudDownload},
    share::{
        inspect_share_with_adapter, preflight_selected_share_download, provider_share_adapter,
        resolve_current_share_download_cancellable, selected_share_download,
        selected_share_sources, CloudShareConfirmation, CloudSharePlanStore,
        ShareConfirmationOutcome, ShareWorkControl,
    },
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider, QuarkError,
    ReqwestTransport, SystemCredentialStore,
};

const PROBE_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
struct ShareCase {
    id: String,
    provider: CloudProvider,
    input: String,
    #[serde(default)]
    access_code: Option<String>,
    #[serde(default)]
    select_name: Option<String>,
}

#[derive(Serialize)]
struct Observation {
    case: String,
    provider: CloudProvider,
    phase: &'static str,
    state: &'static str,
    resources: usize,
    bytes: u64,
}

struct MatrixContext {
    cases: Vec<ShareCase>,
    sessions: SystemCredentialStore,
    plans: CloudSharePlanStore,
    work: PathBuf,
}

struct MatrixTransport {
    inner: ReqwestTransport,
    uc_case: Option<String>,
    uc_token: std::sync::Mutex<Option<String>>,
}

impl MatrixTransport {
    fn new(case: &ShareCase) -> Result<Self, QuarkError> {
        Ok(Self {
            inner: ReqwestTransport::new()?,
            uc_case: (case.provider == CloudProvider::Uc
                && std::env::var("VERSADOWN_LIVE_UC_DIAGNOSTICS").as_deref() == Ok("1"))
            .then(|| case.id.clone()),
            uc_token: std::sync::Mutex::new(None),
        })
    }

    fn uc_request_state(&self, request: &CloudHttpRequest) -> Option<(&'static str, bool, bool)> {
        self.uc_case.as_ref()?;
        let url = url::Url::parse(&request.url).ok()?;
        if url.host_str() != Some("pc-api.uc.cn") {
            return None;
        }
        let phase = match url.path() {
            "/1/clouddrive/member" => "uc-member",
            "/1/clouddrive/share/sharepage/token" => "uc-share-token",
            "/1/clouddrive/share/sharepage/detail" => "uc-share-list",
            "/1/clouddrive/share/sharepage/download" => "uc-share-download",
            "/1/clouddrive/share/sharepage/save" => "uc-share-save",
            "/1/clouddrive/file/download" => "uc-file-download",
            _ => "uc-other",
        };
        let header_csrf = request
            .headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-csrf-token") && !value.is_empty());
        let cookie_csrf = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .is_some_and(|(_, cookie)| {
                cookie.split(';').any(|pair| {
                    pair.trim()
                        .split_once('=')
                        .is_some_and(|(name, value)| name == "csrf_token" && !value.is_empty())
                })
            });
        Some((phase, header_csrf, cookie_csrf))
    }

    fn report_uc(
        &self,
        request: Option<(&'static str, bool, bool)>,
        response: Result<&CloudHttpResponse, &QuarkError>,
    ) {
        let (Some(case), Some((phase, header_csrf, cookie_csrf))) = (&self.uc_case, request) else {
            return;
        };
        match response {
            Ok(response) => {
                let json: serde_json::Value =
                    serde_json::from_str(&response.body).unwrap_or_default();
                if phase == "uc-share-token" {
                    if let Some(token) = json.pointer("/data/stoken").and_then(|v| v.as_str()) {
                        let mut previous = self.uc_token.lock().expect("diagnostic token state");
                        let changed = previous.as_deref().map(|old| old != token);
                        println!("LIVE_UC_CONTEXT case={case} phase={phase} token_changed={changed:?}");
                        *previous = Some(token.to_owned());
                    }
                }
                let code = json["code"]
                    .as_i64()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into());
                let issued_cookie_csrf = response.set_cookies.iter().any(|cookie| {
                    cookie
                        .split(';')
                        .next()
                        .and_then(|pair| pair.trim().split_once('='))
                        .is_some_and(|(name, value)| name == "csrf_token" && !value.is_empty())
                });
                let json_csrf = json
                    .pointer("/data/csrf_token")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty());
                let message = json["message"]
                    .as_str()
                    .or_else(|| json["error"].as_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let tags = [
                    ("parameter", ["参数", "parameter"]),
                    ("folder", ["目录", "文件夹"]),
                    ("missing", ["不存在", "missing"]),
                    ("permission", ["权限", "permission"]),
                    ("verification", ["验证", "verify"]),
                    ("login", ["登录", "login"]),
                    ("risk", ["风控", "risk"]),
                    ("quota", ["空间", "quota"]),
                    ("busy", ["繁忙", "频繁"]),
                    ("transfer", ["转存", "保存"]),
                    ("self", ["自己", "own"]),
                    ("invalid", ["非法", "invalid"]),
                    ("expired", ["失效", "expired"]),
                ]
                .iter()
                .filter(|(_, words)| words.iter().any(|word| message.contains(*word)))
                .map(|(tag, _)| *tag)
                .collect::<Vec<_>>()
                .join("-");
                // Chinese-only diagnostic text cannot contain the ASCII
                // cookie/token/URL values that a provider might echo.
                let chinese_message: String = message
                    .chars()
                    .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
                    .take(80)
                    .collect();
                let is_owner = ["/data/is_owner", "/data/share/is_owner", "/is_owner"]
                    .iter()
                    .filter_map(|path| json.pointer(path))
                    .find_map(|value| value.as_bool().or_else(|| match value.as_i64() {
                        Some(0) => Some(false),
                        Some(1) => Some(true),
                        _ => None,
                    }));
                println!(
                    "LIVE_SHARE case={case} provider=uc phase={phase} state=http-{}-code-{code}-header_csrf-{header_csrf}-cookie_csrf-{cookie_csrf}-issued_csrf-{issued_cookie_csrf}-json_csrf-{json_csrf}-tags-{tags} resources=0 bytes=0",
                    response.status,
                );
                if !chinese_message.is_empty() || is_owner.is_some() {
                    println!("LIVE_UC_DETAIL case={case} phase={phase} owner={is_owner:?} message_zh={chinese_message}");
                }
            }
            Err(error) => println!(
                "LIVE_SHARE case={case} provider=uc phase={phase} state={} resources=0 bytes=0",
                error_state(error)
            ),
        }
    }
}

impl CloudHttpTransport for MatrixTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        let state = self.uc_request_state(&request);
        if let (Some(case), Some((phase, _, _))) = (&self.uc_case, &state) {
            let transfer_cookie = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                .is_some_and(|(_, cookie)| cookie.split(';').any(|pair| {
                    pair.trim().split_once('=').is_some_and(|(name, value)| {
                        name == "UDRIVE_TRANSFER_SESS" && !value.is_empty()
                    })
                }));
            println!("LIVE_UC_CONTEXT case={case} phase={phase} transfer_session={transfer_cookie}");
        }
        let response = self.inner.send(request);
        self.report_uc(state, response.as_ref());
        response
    }

    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        let state = self.uc_request_state(&request);
        let response = self.inner.send_with_cookie_snapshot(request);
        self.report_uc(state, response.as_ref().map(|(response, _)| response));
        response
    }

    fn check_transfer(&self) -> Result<(), QuarkError> {
        self.inner.check_transfer()
    }

    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, QuarkError> {
        self.inner.isolated_session()
    }

    fn manages_cookie_session(&self) -> bool {
        self.inner.manages_cookie_session()
    }

    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, QuarkError> {
        self.inner.cookies_for_url(url)
    }
}

fn required_path(name: &str) -> Result<PathBuf, &'static str> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("required absolute matrix/session/work directory setting is missing")
}

fn load_context() -> Result<MatrixContext, &'static str> {
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Err("requires explicit VERSADOWN_LIVE_CLOUD=1");
    }
    let matrix = required_path("VERSADOWN_LIVE_SHARE_MATRIX")?;
    let bytes = std::fs::read(matrix).map_err(|_| "cannot read share matrix")?;
    let mut cases: Vec<ShareCase> =
        serde_json::from_slice(&bytes).map_err(|_| "invalid share matrix JSON")?;
    let mut ids = HashSet::new();
    for case in &cases {
        if case.id.is_empty()
            || case.id.len() > 64
            || !case
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            || !ids.insert(case.id.clone())
            || case.input.trim().is_empty()
            || !case.provider.is_enabled()
        {
            return Err(
                "matrix case ids must be unique safe labels with an enabled provider/input",
            );
        }
    }
    if let Ok(id) = std::env::var("VERSADOWN_LIVE_SHARE_CASE") {
        cases.retain(|case| case.id == id);
    }
    if cases.is_empty() {
        return Err("share matrix/filter contains no cases");
    }

    let session_path = match std::env::var_os("VERSADOWN_LIVE_SESSION_DIR") {
        Some(_) => required_path("VERSADOWN_LIVE_SESSION_DIR")?,
        None => {
            #[cfg(windows)]
            {
                required_path("APPDATA")?
                    .join("com.versadown.desktop")
                    .join("cloud-sessions")
            }
            #[cfg(not(windows))]
            {
                return Err("requires explicit VERSADOWN_LIVE_SESSION_DIR outside Windows");
            }
        }
    };
    if !session_path.is_dir() {
        return Err("the existing VD session directory does not exist");
    }
    let sessions_root =
        std::fs::canonicalize(&session_path).map_err(|_| "cannot resolve VD session directory")?;
    let work = required_path("VERSADOWN_LIVE_SHARE_WORK_DIR")?;
    std::fs::create_dir_all(&work).map_err(|_| "cannot create isolated matrix work directory")?;
    let work = std::fs::canonicalize(work).map_err(|_| "cannot resolve matrix work directory")?;
    if work.starts_with(&sessions_root) || sessions_root.starts_with(&work) {
        return Err("test work and existing VD session directories must be separate");
    }
    let plans_root = work.join("plans");
    std::fs::create_dir_all(&plans_root).map_err(|_| "cannot create isolated plan directory")?;
    std::fs::create_dir_all(work.join("downloads"))
        .map_err(|_| "cannot create isolated probe directory")?;
    let sessions =
        SystemCredentialStore::new(session_path).map_err(|_| "session store unavailable")?;
    let plans = CloudSharePlanStore::new(
        SystemCredentialStore::new(plans_root).map_err(|_| "isolated plan store unavailable")?,
    );
    Ok(MatrixContext {
        cases,
        sessions,
        plans,
        work,
    })
}

fn error_state(error: &QuarkError) -> &'static str {
    match error {
        QuarkError::NotAuthorized => "not-authorized",
        QuarkError::AccountMismatch => "account-mismatch",
        QuarkError::AuthorizationExpired => "authorization-expired",
        QuarkError::SessionRenewedRetryOperation => "session-renewed-retry-operation",
        QuarkError::WebAuthorizationRequired => "web-authorization-required",
        QuarkError::XunleiVerificationUnavailable(_) => "xunlei-verification-unavailable",
        QuarkError::CaptchaRequired => "captcha-required",
        QuarkError::PikpakOperationVerificationRequired(_) => "operation-verification-required",
        QuarkError::BaiduOperationVerificationRequired => "operation-verification-required",
        QuarkError::PikpakRegionalMembershipRestricted => "regional-membership-restricted",
        QuarkError::AuthorizationPending => "authorization-pending",
        QuarkError::SourceFileMissing => "source-file-missing",
        QuarkError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        QuarkError::DownloadEngineUnavailable => "download-engine-unavailable",
        QuarkError::DownloadSubmissionFailed => "download-submission-failed",
        QuarkError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        QuarkError::BaiduTransferPending => "baidu-transfer-pending",
        QuarkError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
        QuarkError::InvalidResponse => "invalid-response",
        QuarkError::Storage => "storage-error",
        QuarkError::Transport(_) => "network-error",
        QuarkError::Provider(_) => "provider-error",
        QuarkError::InvalidInput(_) => "invalid-input",
        QuarkError::UploadUnavailable | QuarkError::ProviderUnsupported => "provider-unsupported",
    }
}

fn inspect(
    case: &ShareCase,
    context: &MatrixContext,
    http: &MatrixTransport,
) -> Result<CloudShareConfirmation, (&'static str, &'static str)> {
    let candidate =
        versadown_lib::extract_cloud_share_candidate(&case.input, case.access_code.as_deref())
            .map_err(|_| ("input", "invalid-share-input"))?
            .ok_or(("input", "share-not-recognized"))?;
    if candidate.provider != case.provider {
        return Err(("input", "provider-mismatch"));
    }
    restore_account(case.provider, &context.sessions, http)
        .map_err(|error| ("account", error_state(&error)))?;
    let adapter = provider_share_adapter(case.provider, &context.sessions, http)
        .map_err(|error| ("inspect", error_state(&error)))?;
    match inspect_share_with_adapter(&context.plans, adapter.as_ref(), &candidate)
        .map_err(|error| ("inspect", error_state(&error)))?
    {
        ShareConfirmationOutcome::Confirmation(confirmation)
        | ShareConfirmationOutcome::AutoSubmit { confirmation, .. } => Ok(confirmation),
        ShareConfirmationOutcome::AccessCodeRequired => Err(("inspect", "access-code-required")),
    }
}

fn bounded_range_read(
    resolved: &ResolvedCloudDownload,
    output: &Path,
) -> Result<(u64, &'static str), &'static str> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|_| "probe-client-error")?;
    let mut request = client.get(&resolved.url);
    for (name, value) in &resolved.headers {
        // Native-engine routing markers contain account material and are not
        // HTTP headers for the provider's signed file URL.
        if name.to_ascii_lowercase().starts_with("x-downloader-")
            || name.eq_ignore_ascii_case("range")
            || name.eq_ignore_ascii_case("accept-encoding")
        {
            continue;
        }
        request = request.header(name, value);
    }
    let response = request
        .header("Range", format!("bytes=0-{}", PROBE_BYTES - 1))
        .header("Accept-Encoding", "identity")
        .send()
        .map_err(|_| "probe-network-error")?;
    let state = match response.status().as_u16() {
        206 => {
            if !response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("bytes 0-"))
            {
                return Err("probe-invalid-content-range");
            }
            "partial-content"
        }
        200 => "bounded-read-range-ignored",
        401 | 403 => return Err("probe-access-denied"),
        404 => return Err("probe-file-missing"),
        429 => return Err("probe-rate-limited"),
        _ => return Err("probe-http-error"),
    };
    let mut content = Vec::new();
    response
        .take(PROBE_BYTES)
        .read_to_end(&mut content)
        .map_err(|_| "probe-body-error")?;
    if content.is_empty() {
        return Err("probe-empty-body");
    }
    std::fs::write(output, &content).map_err(|_| "probe-output-error")?;
    Ok((content.len() as u64, state))
}

fn resolve_and_probe(
    case: &ShareCase,
    context: &MatrixContext,
    http: &MatrixTransport,
    confirmation: &CloudShareConfirmation,
) -> Result<(u64, &'static str), (&'static str, &'static str)> {
    let sources = selected_share_sources(
        &context.plans,
        &confirmation.plan_source,
        &confirmation
            .resources
            .iter()
            .map(|resource| resource.id.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(|error| ("selection", error_state(&error)))?;
    let source = sources
        .into_iter()
        .filter_map(|source| {
            context
                .plans
                .resolve_entry(&source)
                .ok()
                .map(|entry| (source, entry))
        })
        .filter(|(_, entry)| {
            case.select_name.as_deref().is_none_or(|name| {
                entry.entry.name == name
                    || entry
                        .entry
                        .relative_directory
                        .iter()
                        .chain(std::iter::once(&entry.entry.name))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("/")
                        == name
            })
        })
        .min_by_key(|(_, entry)| {
            (
                entry.entry.size.unwrap_or(u64::MAX),
                entry.entry.name.clone(),
            )
        })
        .map(|(source, _)| source)
        .ok_or(("selection", "selected-file-not-found"))?;
    let entry = context
        .plans
        .resolve_entry(&source)
        .map_err(|error| ("selection", error_state(&error)))?;
    let selection = selected_share_download(
        &context.plans,
        &confirmation.plan_source,
        &[entry.entry.entry_fingerprint.clone()],
    )
    .map_err(|error| ("selection", error_state(&error)))?;
    let adapter = provider_share_adapter(case.provider, &context.sessions, http)
        .map_err(|error| ("preflight", error_state(&error)))?;
    preflight_selected_share_download(&context.plans, adapter.as_ref(), &selection)
        .map_err(|error| ("preflight", error_state(&error)))?;
    let resolved = resolve_current_share_download_cancellable(
        &context.plans,
        &context.sessions,
        http,
        &source,
        &ShareWorkControl::new(Duration::from_secs(180)),
    )
    .map_err(|error| ("resolve", error_state(&error)))?;
    bounded_range_read(
        &resolved,
        &context
            .work
            .join("downloads")
            .join(format!("{}.probe", case.id)),
    )
    .map_err(|state| ("probe", state))
}

fn run_matrix(transfer: bool) {
    if transfer {
        assert_eq!(
            std::env::var("VERSADOWN_LIVE_SHARE_TRANSFER").as_deref(),
            Ok("1"),
            "range checks require explicit VERSADOWN_LIVE_SHARE_TRANSFER=1"
        );
    }
    let context = load_context().unwrap_or_else(|state| panic!("matrix configuration: {state}"));
    let mut observations = Vec::new();
    let mut failures = 0;
    for case in &context.cases {
        let mut observation = Observation {
            case: case.id.clone(),
            provider: case.provider,
            phase: "inspect",
            state: "ok",
            resources: 0,
            bytes: 0,
        };
        let result = match MatrixTransport::new(case) {
            Ok(http) => match inspect(case, &context, &http) {
                Ok(confirmation) => {
                    observation.resources = confirmation.resources.len();
                    if transfer {
                        resolve_and_probe(case, &context, &http, &confirmation).map(
                            |(bytes, state)| {
                                observation.phase = "probe";
                                observation.state = state;
                                observation.bytes = bytes;
                            },
                        )
                    } else {
                        Ok(())
                    }
                }
                Err(failure) => Err(failure),
            },
            Err(error) => Err(("transport", error_state(&error))),
        };
        if let Err((phase, state)) = result {
            failures += 1;
            observation.phase = phase;
            observation.state = state;
        }
        println!(
            "LIVE_SHARE case={} provider={} phase={} state={} resources={} bytes={}",
            observation.case,
            observation.provider.key(),
            observation.phase,
            observation.state,
            observation.resources,
            observation.bytes,
        );
        observations.push(observation);
    }
    let report = serde_json::to_vec_pretty(&observations).expect("redacted report serialization");
    std::fs::write(
        context.work.join(if transfer {
            "range-report.json"
        } else {
            "inspect-report.json"
        }),
        report,
    )
    .expect("write isolated redacted report");
    assert_eq!(
        failures, 0,
        "one or more share cases failed; see redacted per-case states"
    );
}

#[test]
#[ignore = "requires an external matrix, isolated work directory, explicit opt-in and existing authorized VD sessions"]
fn live_share_matrix_inspects_all_cases() {
    run_matrix(false);
}

#[test]
#[ignore = "may save one selected file per case; additionally requires VERSADOWN_LIVE_SHARE_TRANSFER=1"]
fn live_share_matrix_resolves_and_reads_bounded_ranges() {
    run_matrix(true);
}
