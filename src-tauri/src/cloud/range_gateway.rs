//! Process-local adapter between aria2 and provider storage endpoints that cannot
//! be handed to aria2 directly: CDNs that reject large Range requests (Baidu,
//! PikPak, which only need their ranges reshaped) and providers whose objects
//! are end-to-end encrypted (a [`DownloadTransform`] decrypts them on the fly).
//! aria2 remains the only writer and checkpoint owner; this service only
//! reshapes HTTP and, when asked, the bytes.

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use super::download_transform::{Decryptor, DownloadTransform};
use futures_util::stream;
use rand::RngCore;
use reqwest::{Client, Proxy};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, RwLock},
};
use thiserror::Error;
use tokio::task::JoinHandle;
use uuid::Uuid;

const CHUNK_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("invalid download target")]
    InvalidTarget,
    #[error("invalid download header")]
    InvalidHeader,
    #[error("invalid proxy configuration")]
    InvalidProxy,
    #[error("unable to initialize local download transport")]
    Transport,
    #[error("unable to bind local download gateway")]
    Bind(#[from] io::Error),
}

struct Target {
    task_id: Uuid,
    upstream: String,
    headers: HeaderMap,
    client: Client,
    /// When present the upstream is addressed by URL path ranges and its bytes
    /// are transformed (decrypted) before they reach aria2.
    transform: Option<DownloadTransform>,
    quota_waits: Arc<QuotaWaits>,
}

/// What an upstream said when it answered HTTP 509 (transfer quota used up),
/// per task. aria2 reports only `HTTP 509`, so the details travel here until
/// the task's failure is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaWait {
    /// Seconds the upstream asked the client to wait, when it said so.
    pub seconds: Option<u64>,
    /// The download URL was issued against a signed-in account's quota.
    pub account: bool,
}

impl QuotaWait {
    /// The failure to show for the refusal: the account's quota is named even
    /// when MEGA gave no wait; the IP quota only with a known wait, otherwise
    /// the generic bandwidth-limit message stays.
    pub fn failure(self) -> Option<super::mega::MegaFailure> {
        use super::mega::MegaFailure;
        if self.account {
            Some(MegaFailure::AccountQuotaExceeded {
                retry_after_secs: self.seconds,
            })
        } else {
            self.seconds.map(|seconds| MegaFailure::QuotaExceeded {
                retry_after_secs: Some(seconds),
            })
        }
    }
}

#[derive(Default)]
struct QuotaWaits(Mutex<HashMap<Uuid, QuotaWait>>);

#[derive(Default)]
struct Shared {
    targets: RwLock<HashMap<String, Arc<Target>>>,
    quota_waits: Arc<QuotaWaits>,
}

/// MEGA's storage nodes answer HTTP 509 with this header carrying the seconds
/// until the quota resets (megajs `lib/file.mjs`, `handleMegaErrors`).
const QUOTA_WAIT_HEADER: &str = "x-mega-time-left";

fn quota_wait_seconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(QUOTA_WAIT_HEADER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
}

pub struct RangeGateway {
    port: u16,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

pub struct GatewayRegistration {
    token: String,
    url: String,
    shared: Arc<Shared>,
    previous_target: Option<Arc<Target>>,
    committed: bool,
}

impl GatewayRegistration {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for GatewayRegistration {
    fn drop(&mut self) {
        if !self.committed {
            let mut targets = self.shared.targets.write().unwrap();
            if let Some(previous_target) = self.previous_target.take() {
                targets.insert(self.token.clone(), previous_target);
            } else {
                targets.remove(&self.token);
            }
        }
    }
}

impl RangeGateway {
    pub async fn start() -> Result<Self, GatewayError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared::default());
        let app = Router::new()
            .route("/{token}", get(serve).head(serve))
            .with_state(Arc::clone(&shared));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            port,
            shared,
            server,
        })
    }

    pub fn register(
        &self,
        task_id: Uuid,
        upstream: String,
        headers: Vec<(String, String)>,
        proxy: Option<String>,
    ) -> Result<GatewayRegistration, GatewayError> {
        self.register_inner(task_id, upstream, headers, proxy, false, None)
    }

    /// Registers an upstream whose bytes must be transformed before they are
    /// served. `refreshing` retains the paused task's local endpoint like
    /// [`Self::register_refreshing`].
    pub fn register_transformed(
        &self,
        task_id: Uuid,
        upstream: String,
        headers: Vec<(String, String)>,
        proxy: Option<String>,
        transform: DownloadTransform,
        refreshing: bool,
    ) -> Result<GatewayRegistration, GatewayError> {
        self.register_inner(task_id, upstream, headers, proxy, refreshing, Some(transform))
    }

    /// Refresh the upstream while retaining the paused task's local endpoint,
    /// avoiding unnecessary engine URI rotation.
    pub fn register_refreshing(
        &self,
        task_id: Uuid,
        upstream: String,
        headers: Vec<(String, String)>,
        proxy: Option<String>,
    ) -> Result<GatewayRegistration, GatewayError> {
        self.register_inner(task_id, upstream, headers, proxy, true, None)
    }

    fn register_inner(
        &self,
        task_id: Uuid,
        upstream: String,
        headers: Vec<(String, String)>,
        proxy: Option<String>,
        reuse_existing: bool,
        transform: Option<DownloadTransform>,
    ) -> Result<GatewayRegistration, GatewayError> {
        let parsed = url::Url::parse(&upstream).map_err(|_| GatewayError::InvalidTarget)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(GatewayError::InvalidTarget);
        }
        let mut upstream_headers = HeaderMap::new();
        for (name, value) in headers {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| GatewayError::InvalidHeader)?;
            if matches!(
                name.as_str(),
                "range"
                    | "host"
                    | "accept-encoding"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
            ) {
                continue;
            }
            let value = HeaderValue::from_str(&value).map_err(|_| GatewayError::InvalidHeader)?;
            upstream_headers.insert(name, value);
        }
        upstream_headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(90));
        if let Some(proxy) = proxy {
            builder = builder.proxy(Proxy::all(proxy).map_err(|_| GatewayError::InvalidProxy)?);
        } else if match parsed.host() {
            Some(url::Host::Ipv4(host)) => host.is_loopback(),
            Some(url::Host::Ipv6(host)) => host.is_loopback(),
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            None => false,
        } {
            // Fixture and internal origins must remain local even when the
            // system proxy does not exclude loopback addresses.
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| GatewayError::Transport)?;
        let mut targets = self.shared.targets.write().unwrap();
        // A fresh submission starts a fresh quota episode.
        self.shared.quota_waits.0.lock().unwrap().remove(&task_id);
        let existing = reuse_existing
            .then(|| {
                targets
                    .iter()
                    .find(|(_, target)| target.task_id == task_id)
                    .map(|(token, _)| token.clone())
            })
            .flatten();
        let token = if let Some(existing) = existing {
            existing
        } else {
            loop {
                let mut random = [0u8; 24];
                rand::thread_rng().fill_bytes(&mut random);
                let candidate: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
                if !targets.contains_key(&candidate) {
                    break candidate;
                }
            }
        };
        let previous_target = targets.insert(
            token.clone(),
            Arc::new(Target {
                task_id,
                upstream,
                headers: upstream_headers,
                client,
                transform,
                quota_waits: Arc::clone(&self.shared.quota_waits),
            }),
        );
        Ok(GatewayRegistration {
            url: format!("http://127.0.0.1:{}/{token}", self.port),
            token,
            shared: Arc::clone(&self.shared),
            previous_target,
            committed: false,
        })
    }

    pub fn retire(&self, task_id: Uuid, keep_token: &str) {
        self.shared
            .targets
            .write()
            .unwrap()
            .retain(|token, target| target.task_id != task_id || token == keep_token);
    }

    pub fn remove_token(&self, token: &str) {
        self.shared.targets.write().unwrap().remove(token);
    }

    /// The quota refusal most recently seen for `task_id`, consumed once.
    pub fn take_quota_wait(&self, task_id: Uuid) -> Option<QuotaWait> {
        self.shared.quota_waits.0.lock().unwrap().remove(&task_id)
    }

    pub fn remove_task(&self, task_id: Uuid) {
        self.shared
            .targets
            .write()
            .unwrap()
            .retain(|_, target| target.task_id != task_id);
    }
}

impl Drop for RangeGateway {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[derive(Clone, Copy)]
struct RequestedRange {
    start: u64,
    end: Option<u64>,
    partial: bool,
}

fn requested_range(value: Option<&HeaderValue>) -> Result<RequestedRange, StatusCode> {
    let Some(value) = value else {
        return Ok(RequestedRange {
            start: 0,
            end: None,
            partial: false,
        });
    };
    let raw = value
        .to_str()
        .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
    let (start, end) = raw
        .strip_prefix("bytes=")
        .and_then(|range| range.split_once('-'))
        .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?;
    if start.is_empty() || end.contains(',') {
        return Err(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    let start = start
        .parse::<u64>()
        .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
    let end = if end.is_empty() {
        None
    } else {
        let end = end
            .parse::<u64>()
            .map_err(|_| StatusCode::RANGE_NOT_SATISFIABLE)?;
        if end < start {
            return Err(StatusCode::RANGE_NOT_SATISFIABLE);
        }
        Some(end)
    };
    Ok(RequestedRange {
        start,
        end,
        partial: true,
    })
}

fn parse_content_range(value: &HeaderValue) -> Option<(u64, u64, u64)> {
    let value = value.to_str().ok()?.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (total > 0 && start <= end && end < total).then_some((start, end, total))
}

impl Target {
    async fn fetch(
        &self,
        start: u64,
        end: u64,
        expected_total: Option<u64>,
    ) -> Result<UpstreamRange, StatusCode> {
        match &self.transform {
            None => self.fetch_http_range(start, end, expected_total).await,
            Some(transform) => self.fetch_path_range(transform, start, end).await,
        }
    }

    /// Fetch one plain range of a path-addressed upstream whose body is
    /// ciphertext of the same length (no `Content-Range` is reported).
    async fn fetch_path_range(
        &self,
        transform: &DownloadTransform,
        start: u64,
        end: u64,
    ) -> Result<UpstreamRange, StatusCode> {
        let total = transform.total_size();
        if start >= total {
            return Err(StatusCode::RANGE_NOT_SATISFIABLE);
        }
        let end = end.min(total - 1);
        if end - start >= CHUNK_BYTES {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let url = transform
            .range_url(&self.upstream, start, end)
            .ok_or(StatusCode::BAD_GATEWAY)?;
        let response = self
            .client
            .get(url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        match response.status() {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {}
            StatusCode::FORBIDDEN => return Err(StatusCode::FORBIDDEN),
            // Transfer quota exhausted. Surface the provider's own status so
            // the engine does not report a generic gateway failure.
            status if status.as_u16() == 509 => {
                self.quota_waits.0.lock().unwrap().insert(
                    self.task_id,
                    QuotaWait {
                        seconds: quota_wait_seconds(response.headers()),
                        account: self
                            .transform
                            .as_ref()
                            .is_some_and(|transform| transform.uses_account_quota()),
                    },
                );
                return Err(status);
            }
            _ => return Err(StatusCode::BAD_GATEWAY),
        }
        if response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|value| value != "identity")
        {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let expected_length = end - start + 1;
        if response
            .content_length()
            .is_some_and(|length| length != expected_length)
        {
            return Err(StatusCode::BAD_GATEWAY);
        }
        Ok(UpstreamRange {
            response,
            end,
            total,
            remaining: expected_length,
            decryptor: Some(transform.decryptor(start)),
        })
    }

    async fn fetch_http_range(
        &self,
        start: u64,
        end: u64,
        expected_total: Option<u64>,
    ) -> Result<UpstreamRange, StatusCode> {
        let response = self
            .client
            .get(&self.upstream)
            .headers(self.headers.clone())
            .header(header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            // A rejected or expired credential is reported as 403 so the
            // engine's failure leads to a refreshed URL and token.
            return Err(if matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
            ) {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_GATEWAY
            });
        }
        if response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|value| value != "identity")
        {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let (actual_start, actual_end, total) = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(parse_content_range)
            .ok_or(StatusCode::BAD_GATEWAY)?;
        if actual_start != start
            || actual_end > end.min(total - 1)
            || expected_total.is_some_and(|expected| expected != total)
            || end - start >= CHUNK_BYTES
        {
            return Err(StatusCode::BAD_GATEWAY);
        }
        let expected_length = actual_end - start + 1;
        if response
            .content_length()
            .is_some_and(|length| length != expected_length)
        {
            return Err(StatusCode::BAD_GATEWAY);
        }
        Ok(UpstreamRange {
            response,
            end: actual_end,
            total,
            remaining: expected_length,
            decryptor: None,
        })
    }
}

struct UpstreamRange {
    response: reqwest::Response,
    end: u64,
    total: u64,
    remaining: u64,
    decryptor: Option<Decryptor>,
}

impl UpstreamRange {
    async fn chunk(&mut self) -> io::Result<Bytes> {
        let chunk = self
            .response
            .chunk()
            .await
            .map_err(|_| io::Error::other("upstream body failed"))?
            .ok_or_else(|| io::Error::other("upstream range truncated"))?;
        if chunk.len() as u64 > self.remaining {
            return Err(io::Error::other(
                "upstream range exceeds its declared length",
            ));
        }
        self.remaining -= chunk.len() as u64;
        if self.remaining == 0 {
            // Keep the last chunk until EOF is verified. Even chunked origins
            // without Content-Length must not make an invalid range complete.
            if self
                .response
                .chunk()
                .await
                .map_err(|_| io::Error::other("upstream body failed"))?
                .is_some()
            {
                return Err(io::Error::other(
                    "upstream range exceeds its declared length",
                ));
            }
        }
        Ok(match self.decryptor.as_mut() {
            Some(decryptor) => {
                let mut plain = chunk.to_vec();
                decryptor.apply(&mut plain);
                Bytes::from(plain)
            }
            None => chunk,
        })
    }
}

struct RangeStream {
    target: Arc<Target>,
    current: UpstreamRange,
    final_end: u64,
    failed: bool,
}

impl RangeStream {
    async fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
        if self.current.remaining == 0 {
            if self.current.end >= self.final_end {
                return Ok(None);
            }
            let start = self.current.end + 1;
            let end = start.saturating_add(CHUNK_BYTES - 1).min(self.final_end);
            self.current = self
                .target
                .fetch(start, end, Some(self.current.total))
                .await
                .map_err(|_| io::Error::other("upstream range failed"))?;
        }
        self.current.chunk().await.map(Some)
    }
}

async fn serve(
    State(shared): State<Arc<Shared>>,
    Path(token): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let target = shared.targets.read().unwrap().get(&token).cloned();
    let Some(target) = target else {
        return empty(StatusCode::GONE);
    };
    let requested = if method == Method::HEAD {
        RequestedRange {
            start: 0,
            end: Some(0),
            partial: false,
        }
    } else {
        match requested_range(headers.get(header::RANGE)) {
            Ok(range) => range,
            Err(status) => return empty(status),
        }
    };
    if method == Method::HEAD {
        // The transform already knows the size; do not spend upstream quota.
        if let Some(transform) = &target.transform {
            return head_response(transform.total_size());
        }
    }
    let first_end = requested
        .start
        .saturating_add(CHUNK_BYTES - 1)
        .min(requested.end.unwrap_or(u64::MAX));
    let first = match target.fetch(requested.start, first_end, None).await {
        Ok(result) => result,
        Err(status) => return empty(status),
    };
    let total = first.total;
    let final_end = requested.end.unwrap_or(total - 1).min(total - 1);
    let mut builder = Response::builder()
        .status(if requested.partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, final_end - requested.start + 1);
    if requested.partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{final_end}/{total}", requested.start),
        );
    }
    if method == Method::HEAD {
        return head_response(total);
    }
    // Forward each received chunk immediately after validating range headers.
    // Buffering a whole 4 MiB range hides healthy slow CDN traffic from aria2,
    // whose eight-second stall detector then cancels the local connection.
    let chunks = stream::unfold(
        RangeStream {
            target,
            current: first,
            final_end,
            failed: false,
        },
        |mut state| async move {
            if state.failed {
                return None;
            }
            match state.next_chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), state)),
                Ok(None) => None,
                Err(error) => {
                    state.failed = true;
                    Some((Err(error), state))
                }
            }
        },
    );
    builder.body(Body::from_stream(chunks)).unwrap()
}

fn head_response(total: u64) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, total)
        .body(Body::empty())
        .unwrap()
}

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}
