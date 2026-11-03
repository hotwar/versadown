//! Process-local adapter for Baidu CDN endpoints that reject large Range requests.
//! aria2 remains the only writer and checkpoint owner; this service only reshapes HTTP.

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use futures_util::stream;
use rand::RngCore;
use reqwest::{Client, Proxy};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, RwLock},
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
}

#[derive(Default)]
struct Shared {
    targets: RwLock<HashMap<String, Arc<Target>>>,
}

pub struct BaiduRangeGateway {
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

impl BaiduRangeGateway {
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
        self.register_inner(task_id, upstream, headers, proxy, false)
    }

    /// Refresh the upstream without changing the local URI that identifies
    /// aria2-next's persisted byte ranges for a paused task.
    pub fn register_refreshing(
        &self,
        task_id: Uuid,
        upstream: String,
        headers: Vec<(String, String)>,
        proxy: Option<String>,
    ) -> Result<GatewayRegistration, GatewayError> {
        self.register_inner(task_id, upstream, headers, proxy, true)
    }

    fn register_inner(
        &self,
        task_id: Uuid,
        upstream: String,
        headers: Vec<(String, String)>,
        proxy: Option<String>,
        reuse_existing: bool,
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

    pub fn remove_task(&self, task_id: Uuid) {
        self.shared
            .targets
            .write()
            .unwrap()
            .retain(|_, target| target.task_id != task_id);
    }
}

impl Drop for BaiduRangeGateway {
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
        let response = self
            .client
            .get(&self.upstream)
            .headers(self.headers.clone())
            .header(header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(if response.status() == StatusCode::FORBIDDEN {
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
        })
    }
}

struct UpstreamRange {
    response: reqwest::Response,
    end: u64,
    total: u64,
    remaining: u64,
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
        Ok(chunk)
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
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, total)
            .body(Body::empty())
            .unwrap();
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

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}
