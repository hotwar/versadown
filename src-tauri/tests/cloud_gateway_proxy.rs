use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use versadown_lib::cloud::range_gateway::RangeGateway;

/// Reverting the gateway's remote-target client to `no_proxy()` must fail this
/// test: the fixture origin exists only behind this process-local HTTP proxy.
#[test]
fn remote_cloud_range_uses_the_system_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.ends_with(b"\r\n\r\n") {
                        let read = stream.read(&mut buffer).unwrap();
                        if read == 0 || request.len() + read > 4096 {
                            return false;
                        }
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    if !request.contains("get http://proxy-only.invalid/file http/1.1")
                        || !request.contains("range: bytes=0-3")
                    {
                        return false;
                    }
                    stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nConnection: close\r\n\r\nDATA").unwrap();
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("proxy fixture failed: {error}"),
            }
        }
    });
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "system_proxy_child", "--ignored", "--nocapture"])
        .env("VERSADOWN_PROXY_CHILD", "1")
        .env("HTTP_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    let served = worker.join().unwrap();
    assert!(
        served,
        "gateway did not send the requested range through the proxy"
    );
    assert!(
        output.status.success(),
        "gateway child failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
#[ignore = "run only through remote_cloud_range_uses_the_system_proxy"]
async fn system_proxy_child() {
    assert_eq!(std::env::var("VERSADOWN_PROXY_CHILD").as_deref(), Ok("1"));
    let gateway = RangeGateway::start().await.unwrap();
    let registration = gateway
        .register(
            uuid::Uuid::new_v4(),
            "http://proxy-only.invalid/file".into(),
            vec![],
            None,
        )
        .unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap()
        .get(registration.url())
        .header(reqwest::header::RANGE, "bytes=0-3")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"DATA");
}
