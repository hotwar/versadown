use super::QuarkError;
use reqwest::cookie::CookieStore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Instant,
};

const MAX_EXPLICIT_COOKIE_SESSIONS: usize = 64;

#[cfg(test)]
#[path = "http_cookie_snapshot_tests.rs"]
mod cookie_snapshot_tests;

#[derive(Clone)]
pub struct CloudHttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct CloudHttpResponse {
    pub status: u16,
    pub body: String,
    /// Raw cookies issued by the response host. Attributes remain transient;
    /// only a provider-validated, URL-scoped snapshot may be persisted.
    pub set_cookies: Vec<String>,
}
pub trait CloudHttpTransport: Send + Sync {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError>;

    /// Return the cookie bound to this exact request, before its Set-Cookie
    /// response. Stateless transports leave callers' explicit headers intact.
    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        self.send(request).map(|response| (response, None))
    }

    /// Upload workers can prevent snapshot publication after cooperative cancellation.
    fn check_transfer(&self) -> Result<(), QuarkError> {
        Ok(())
    }

    /// Creates a clean cookie context for account login. Stateless test
    /// transports can keep the default and receive the requests directly.
    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, QuarkError> {
        Ok(None)
    }

    /// Stateful transports scope, expire and replace cookies themselves.
    fn manages_cookie_session(&self) -> bool {
        false
    }

    /// Export only cookies the jar would send to this exact URL. This is used
    /// after provider validation so the persisted snapshot includes cookies
    /// rotated by the validation responses.
    fn cookies_for_url(&self, _url: &str) -> Result<Vec<String>, QuarkError> {
        Ok(Vec::new())
    }
}

pub(crate) struct ControlledUploadHttp<'a, T> {
    pub inner: &'a T,
    pub control: super::UploadControl,
}
impl<T: CloudHttpTransport> CloudHttpTransport for ControlledUploadHttp<'_, T> {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.control.check()?;
        self.inner.send(request)
    }
    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        self.control.check()?;
        self.inner.send_with_cookie_snapshot(request)
    }
    fn check_transfer(&self) -> Result<(), QuarkError> {
        self.control.check()
    }
    fn manages_cookie_session(&self) -> bool {
        self.inner.manages_cookie_session()
    }
    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, QuarkError> {
        self.inner.cookies_for_url(url)
    }
}
impl<T: super::CloudUploadTransport> super::CloudUploadTransport for ControlledUploadHttp<'_, T> {
    fn send_upload(
        &self,
        mut request: super::UploadHttpRequest,
    ) -> Result<super::UploadHttpResponse, QuarkError> {
        self.control.wait_ready_blocking()?;
        request.body = super::UploadBody::Controlled {
            body: Box::new(request.body),
            control: self.control.clone(),
        };
        let response = self.inner.send_upload(request)?;
        self.control.check()?;
        Ok(response)
    }
}
pub struct ReqwestTransport {
    pub(super) client: reqwest::blocking::Client,
    jar: Arc<reqwest::cookie::Jar>,
    explicit_cookie_sessions: Mutex<HashMap<[u8; 32], ExplicitCookieSession>>,
}

struct ExplicitCookieSession {
    client: reqwest::blocking::Client,
    snapshot_client: Option<reqwest::blocking::Client>,
    jar: Arc<reqwest::cookie::Jar>,
    last_used: Instant,
}

impl ExplicitCookieSession {
    fn client(&mut self, snapshot: bool) -> Result<reqwest::blocking::Client, QuarkError> {
        if !snapshot {
            return Ok(self.client.clone());
        }
        if self.snapshot_client.is_none() {
            self.snapshot_client = Some(ReqwestTransport::client_with_jar(
                Arc::clone(&self.jar),
                false,
            )?);
        }
        Ok(self.snapshot_client.as_ref().unwrap().clone())
    }
}

/// Loopback traffic never goes through a proxy; `NO_PROXY` entries are honoured
/// too, because an explicit `Proxy::all` otherwise ignores them.
fn proxy_bypass() -> Option<reqwest::NoProxy> {
    let configured = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    reqwest::NoProxy::from_string(&format!("{configured},localhost,127.0.0.0/8,::1"))
}

impl ReqwestTransport {
    fn client_with_jar(
        jar: Arc<reqwest::cookie::Jar>,
        follow_redirects: bool,
    ) -> Result<reqwest::blocking::Client, QuarkError> {
        let mut builder = reqwest::blocking::Client::builder()
            .cookie_provider(jar)
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36");
        if !follow_redirects {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        if let Some(proxy) = std::env::var_os("HTTPS_PROXY")
            .or_else(|| std::env::var_os("https_proxy"))
            .or_else(|| std::env::var_os("ALL_PROXY"))
            .filter(|value| !value.is_empty())
        {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy.to_string_lossy().as_ref())
                    .map_err(|_| QuarkError::Transport("代理配置无效".into()))?
                    .no_proxy(proxy_bypass()),
            );
        }
        builder
            .build()
            .map_err(|_| QuarkError::Transport("无法初始化 HTTPS 客户端".into()))
    }

    fn explicit_cookie_client(
        &self,
        url: &reqwest::Url,
        cookie_header: &str,
        snapshot: bool,
    ) -> Result<(reqwest::blocking::Client, Arc<reqwest::cookie::Jar>), QuarkError> {
        let origin = url.origin().ascii_serialization();
        let mut hasher = Sha256::new();
        hasher.update(origin.as_bytes());
        hasher.update([0]);
        hasher.update(cookie_header.as_bytes());
        let key: [u8; 32] = hasher.finalize().into();
        let now = Instant::now();
        let mut sessions = self
            .explicit_cookie_sessions
            .lock()
            .map_err(|_| QuarkError::Transport("HTTP 会话状态不可用".into()))?;
        if let Some(session) = sessions.get_mut(&key) {
            session.last_used = now;
            return Ok((session.client(snapshot)?, Arc::clone(&session.jar)));
        }
        if sessions.len() >= MAX_EXPLICIT_COOKIE_SESSIONS {
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, session)| session.last_used)
                .map(|(key, _)| *key)
            {
                sessions.remove(&oldest);
            }
        }
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let root_url =
            reqwest::Url::parse(&format!("{origin}/")).map_err(|_| QuarkError::InvalidResponse)?;
        for cookie in cookie_header
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            // Persisted provider sessions intentionally contain only cookie
            // pairs, not browser-only attributes. Scope those pairs to the
            // provider origin root so auth survives switches between API
            // paths while response Set-Cookie still observes its real scope.
            jar.add_cookie_str(cookie, &root_url);
        }
        let mut session = ExplicitCookieSession {
            client: Self::client_with_jar(Arc::clone(&jar), true)?,
            snapshot_client: None,
            jar: Arc::clone(&jar),
            last_used: now,
        };
        let client = session.client(snapshot)?;
        sessions.insert(key, session);
        Ok((client, jar))
    }

    pub fn new() -> Result<Self, QuarkError> {
        let jar = Arc::new(reqwest::cookie::Jar::default());
        Ok(Self {
            client: Self::client_with_jar(Arc::clone(&jar), true)?,
            jar,
            explicit_cookie_sessions: Mutex::new(HashMap::new()),
        })
    }
    fn send_request(
        &self,
        r: CloudHttpRequest,
        snapshot: bool,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        let has_content_type = r
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("Content-Type"));
        let mut explicit_cookie = None;
        let mut headers = Vec::with_capacity(r.headers.len());
        for (name, value) in r.headers {
            if name.eq_ignore_ascii_case("Cookie") {
                explicit_cookie = Some(value);
            } else {
                headers.push((name, value));
            }
        }
        let mut sent_cookie = None;
        let client = if let Some(cookie) = explicit_cookie.as_deref() {
            let url = reqwest::Url::parse(&r.url).map_err(|_| QuarkError::InvalidResponse)?;
            let (client, jar) = self.explicit_cookie_client(&url, cookie, snapshot)?;
            if snapshot {
                // An empty jar means no Cookie header at all; callers then fall
                // back to their stored session instead of an empty snapshot.
                sent_cookie = match jar.cookies(&url) {
                    Some(value) => Some(
                        value
                            .to_str()
                            .map_err(|_| QuarkError::InvalidResponse)?
                            .to_owned(),
                    )
                    .filter(|cookie| !cookie.is_empty()),
                    None => None,
                };
            }
            client
        } else {
            self.client.clone()
        };
        let mut b = client.request(
            r.method.parse().map_err(|_| QuarkError::InvalidResponse)?,
            &r.url,
        );
        for (k, v) in headers {
            b = b.header(k, v)
        }
        if let Some(cookie) = &sent_cookie {
            // Pin the exact cookie to this request. Another response may rotate
            // the shared jar before send; reqwest must not substitute that value.
            b = b.header(reqwest::header::COOKIE, cookie);
        }
        if let Some(body) = r.body {
            if !has_content_type {
                b = b.header("Content-Type", "application/json");
            }
            b = b.body(body)
        }
        // URL-bearing errors may disclose signed credentials.
        let response = b.send().map_err(|e| {
            QuarkError::Transport(
                if e.is_timeout() {
                    "连接超时"
                } else if e.is_connect() {
                    "连接失败"
                } else {
                    "HTTPS 请求失败"
                }
                .into(),
            )
        })?;
        if sent_cookie.is_some() && response.status().is_redirection() {
            // A first-hop cookie is not evidence of a redirected signing request.
            return Err(QuarkError::InvalidResponse);
        }
        let status = response.status().as_u16();
        let set_cookies = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect();
        let body = response.text().map_err(|_| QuarkError::InvalidResponse)?;
        Ok((
            CloudHttpResponse {
                status,
                body,
                set_cookies,
            },
            sent_cookie,
        ))
    }
}
impl CloudHttpTransport for ReqwestTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.send_request(request, false)
            .map(|(response, _)| response)
    }

    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        self.send_request(request, true)
    }

    fn isolated_session(&self) -> Result<Option<Box<dyn CloudHttpTransport>>, QuarkError> {
        Ok(Some(Box::new(Self::new()?)))
    }

    fn manages_cookie_session(&self) -> bool {
        true
    }

    fn cookies_for_url(&self, url: &str) -> Result<Vec<String>, QuarkError> {
        let url = reqwest::Url::parse(url).map_err(|_| QuarkError::InvalidResponse)?;
        let Some(header) = self.jar.cookies(&url) else {
            return Ok(Vec::new());
        };
        let header = header.to_str().map_err(|_| QuarkError::InvalidResponse)?;
        Ok(header
            .split(';')
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .map(str::to_owned)
            .collect())
    }
}
pub(crate) fn response_json(r: &CloudHttpResponse) -> Result<Value, QuarkError> {
    if r.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    let v: Value = if r.body.is_empty() && (200..300).contains(&r.status) {
        serde_json::json!({})
    } else {
        serde_json::from_str(&r.body).map_err(|_| QuarkError::InvalidResponse)?
    };
    let code = v
        .get("error_code")
        .or_else(|| v.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let status = v.get("status").and_then(Value::as_i64).unwrap_or(200);
    let error = v.get("error").and_then(Value::as_str).unwrap_or("");
    if matches!(code, 31001 | 401 | 16 | 4122)
        || status == 401
        || matches!(error, "unauthenticated" | "invalid_grant" | "invalid_token")
    {
        return Err(QuarkError::AuthorizationExpired);
    }
    if code == 9 || error == "riskLimited" {
        return Err(QuarkError::CaptchaRequired);
    }
    if code == 4031 || error.to_ascii_lowercase().contains("captcha") {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    if !(200..300).contains(&r.status)
        || code != 0
        || !matches!(status, 0 | 200 | 2000000)
        || !error.is_empty()
    {
        return Err(QuarkError::Provider(format!(
            "HTTP {} / code {} / {}",
            r.status,
            code,
            if error.is_empty() {
                "服务端拒绝操作"
            } else {
                error
            }
        )));
    }
    Ok(v)
}

#[cfg(test)]
mod upload_tests {
    #[test]
    fn upload_control_paused_session_check_does_not_wait_for_resume() {
        let control = crate::cloud::UploadControl::default();
        control.pause().unwrap();
        let worker_control = control.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let transport = super::ReqwestTransport::new().unwrap();
            let http = super::ControlledUploadHttp {
                inner: &transport,
                control: worker_control,
            };
            tx.send(super::CloudHttpTransport::check_transfer(&http))
                .unwrap();
        });
        let checked = rx.recv_timeout(std::time::Duration::from_millis(250));
        control.resume().unwrap();
        worker.join().unwrap();
        assert!(
            checked.is_ok(),
            "session checks must not hold auth locks while paused"
        );
        checked.unwrap().unwrap();
    }
    use super::*;
    use crate::cloud::{
        CloudUploadTransport, UploadBody, UploadControl, UploadFileSnapshot, UploadHttpRequest,
    };
    use std::io::{Read, Write};

    #[test]
    fn upload_control_paused_payload_request_waits_until_resume() {
        struct Observed(std::sync::mpsc::Sender<()>);
        impl CloudHttpTransport for Observed {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                unreachable!()
            }
        }
        impl CloudUploadTransport for Observed {
            fn send_upload(
                &self,
                _: UploadHttpRequest,
            ) -> Result<crate::cloud::UploadHttpResponse, QuarkError> {
                self.0.send(()).unwrap();
                Err(QuarkError::Transport("fixture response".into()))
            }
        }
        let control = UploadControl::default();
        control.pause().unwrap();
        let worker_control = control.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ControlledUploadHttp {
                inner: &Observed(tx),
                control: worker_control,
            }
            .send_upload(UploadHttpRequest {
                method: "PUT",
                url: "https://example.invalid/upload".into(),
                headers: BTreeMap::new(),
                body: UploadBody::Bytes(vec![]),
            })
        });
        let during_pause = rx.recv_timeout(std::time::Duration::from_millis(75));
        control.resume().unwrap();
        let after_resume = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert!(worker.join().unwrap().is_err());
        assert!(during_pause.is_err());
        assert!(after_resume.is_ok());
    }

    #[test]
    fn reqwest_streams_exact_content_length_and_file_slice() {
        let source = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"prefix-PAYLOAD-suffix").unwrap();
        let snapshot = UploadFileSnapshot::capture(source.path()).unwrap();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/upload", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
            assert!(headers.contains("content-length: 7\r\n"));
            assert!(!headers.contains("transfer-encoding:"));
            let mut body = [0; 7];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"PAYLOAD");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let control = UploadControl::default();
        let http = ReqwestTransport::new().unwrap();
        let controlled = ControlledUploadHttp {
            inner: &http,
            control: control.clone(),
        };
        let result = controlled.send_upload(UploadHttpRequest {
            method: "PUT",
            url,
            headers: BTreeMap::new(),
            body: snapshot.slice(7, 7).unwrap(),
        });
        worker.join().unwrap();
        assert_eq!(result.unwrap().status, 200);
        assert_eq!(control.bytes_read(), 7);
    }

    #[test]
    fn cancelled_transport_sends_neither_metadata_nor_mutation_requests() {
        struct Never;
        impl CloudHttpTransport for Never {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                panic!("cancelled metadata request started")
            }
        }
        impl CloudUploadTransport for Never {
            fn send_upload(
                &self,
                _: UploadHttpRequest,
            ) -> Result<crate::cloud::UploadHttpResponse, QuarkError> {
                panic!("cancelled transfer started")
            }
        }
        let control = UploadControl::default();
        let http = ControlledUploadHttp {
            inner: &Never,
            control: control.clone(),
        };
        control.cancel();
        assert!(http.check_transfer().is_err());
        assert!(http
            .send(CloudHttpRequest {
                method: "POST",
                url: "https://example.com/commit".into(),
                headers: BTreeMap::new(),
                body: None
            })
            .is_err());
        assert!(http
            .send_upload(UploadHttpRequest {
                method: "POST",
                url: "https://example.com/upload".into(),
                headers: BTreeMap::new(),
                body: UploadBody::Bytes(vec![])
            })
            .is_err());
    }
}
