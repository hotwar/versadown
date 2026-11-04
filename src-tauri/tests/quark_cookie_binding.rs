//! A signed download must retain the cookie used by its link request, including
//! rotations learned by the API transport before that request.
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};
use versadown_lib::cloud::{
    CloudHttpRequest, CloudHttpResponse, CloudHttpTransport, CloudProvider, CredentialStore,
    MemoryCredentialStore, ProviderRegistry, QuarkError, ReqwestTransport,
};

struct LoopbackApi {
    inner: ReqwestTransport,
    origin: String,
}
impl LoopbackApi {
    fn rewrite(&self, mut request: CloudHttpRequest) -> CloudHttpRequest {
        let path = if request.url.contains("/file/sort") {
            "/directory"
        } else {
            "/download"
        };
        request.url = format!("{}{path}", self.origin);
        request
    }
}
impl CloudHttpTransport for LoopbackApi {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
        self.inner.send(self.rewrite(request))
    }
    fn send_with_cookie_snapshot(
        &self,
        request: CloudHttpRequest,
    ) -> Result<(CloudHttpResponse, Option<String>), QuarkError> {
        self.inner.send_with_cookie_snapshot(self.rewrite(request))
    }
}

#[test]
fn rotated_directory_cookie_downloads_bytes_before_and_after_reopening() {
    for rotate_again in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let download_origin = origin.clone();
        let expected = if rotate_again {
            "__puus=download-fixture"
        } else {
            "__puus=directory-fixture"
        };
        let server = std::thread::spawn(move || {
            let mut observed = Vec::new();
            for index in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                let field = |name: &str| {
                    header.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_owned())
                    })
                };
                let cookie = field("cookie").unwrap_or_default();
                let length = field("content-length")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                stream.read_exact(&mut vec![0; length]).unwrap();
                observed.push(cookie.clone());
                let (status, extra, body) = match index {
                    0 => ("200 OK", "Set-Cookie: __puus=directory-fixture; Path=/\r\n", r#"{"code":0,"data":{"list":[]}}"#.to_owned()),
                    1 | 3 => ("200 OK", if rotate_again { "Set-Cookie: __puus=download-fixture; Path=/\r\n" } else { "" },
                        serde_json::json!({"code":0,"data":[{"fid":"fixture","file_name":"fixture.bin","download_url":format!("{download_origin}/bytes")}]}).to_string()),
                    _ if cookie == expected => ("200 OK", "", "fixture-download-bytes".to_owned()),
                    _ => ("412 Precondition Failed", "", "cookie mismatch".to_owned()),
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            observed
        });
        let store = MemoryCredentialStore::default();
        store.save("quark", b"__puus=original-fixture").unwrap();
        let http = LoopbackApi {
            inner: ReqwestTransport::new().unwrap(),
            origin: origin.clone(),
        };
        ProviderRegistry::global()
            .driver(CloudProvider::Quark, &store, &http)
            .list_directory("0")
            .unwrap();
        let mut responses = Vec::new();
        let mut persisted = Vec::new();
        for transport in [
            http,
            LoopbackApi {
                inner: ReqwestTransport::new().unwrap(),
                origin,
            },
        ] {
            let resolved = ProviderRegistry::global()
                .driver(CloudProvider::Quark, &store, &transport)
                .resolve_download("fixture")
                .unwrap();
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .unwrap();
            let mut request = client.get(&resolved.url);
            for (name, value) in resolved.headers {
                request = request.header(name, value);
            }
            let response = request.send().unwrap();
            responses.push((response.status().as_u16(), response.text().unwrap()));
            persisted.push(store.load("quark").unwrap().unwrap());
        }
        let observed = server.join().unwrap();
        assert_eq!(observed[0], "__puus=original-fixture");
        assert_eq!(observed[1], "__puus=directory-fixture");
        assert_eq!(observed[2], expected);
        assert_eq!(observed[3], expected);
        assert_eq!(observed[4], expected);
        for (status, bytes) in responses {
            assert_eq!(status, 200);
            assert_eq!(bytes, "fixture-download-bytes");
        }
        for cookie in persisted {
            assert_eq!(cookie, expected.as_bytes());
        }
    }
}
