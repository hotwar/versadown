use super::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::Duration,
};

fn receive(listener: &TcpListener) -> (TcpStream, String) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    (stream, String::from_utf8(headers).unwrap())
}

fn cookie(headers: &str) -> &str {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("cookie").then_some(value.trim())
        })
        .unwrap_or_default()
}

fn reply(mut stream: TcpStream, status: &str, extra: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: 2\r\n{extra}Connection: close\r\n\r\n{{}}",
    )
    .unwrap();
}

fn request(base: &str, path: &str, cookie: &str) -> CloudHttpRequest {
    CloudHttpRequest {
        method: "GET",
        url: format!("{base}/{path}"),
        headers: BTreeMap::from([("Cookie".into(), cookie.into())]),
        body: None,
    }
}

#[test]
fn request_cookie_snapshot_survives_concurrent_rotation_and_keeps_accounts_isolated() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (first_arrived, first_arrival) = mpsc::channel();
    let (release_first, wait_for_release) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (seed, headers) = receive(&listener);
        assert_eq!(cookie(&headers), "sid=original");
        reply(seed, "200 OK", "Set-Cookie: sid=rotated; Path=/\r\n");

        let (first, headers) = receive(&listener);
        assert_eq!(cookie(&headers), "sid=rotated");
        first_arrived.send(()).unwrap();
        let (second, headers) = receive(&listener);
        assert_eq!(cookie(&headers), "sid=rotated");
        reply(second, "200 OK", "Set-Cookie: sid=newest; Path=/\r\n");
        // The second client has processed its Set-Cookie before the first
        // response completes. Reading the shared jar afterwards is incorrect.
        wait_for_release
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        reply(first, "200 OK", "");

        let (following, headers) = receive(&listener);
        assert_eq!(cookie(&headers), "sid=newest");
        reply(following, "200 OK", "");
        let (other_account, headers) = receive(&listener);
        assert_eq!(cookie(&headers), "sid=other-account");
        reply(other_account, "200 OK", "");
    });
    let http = Arc::new(ReqwestTransport::new().unwrap());
    http.send(request(&base, "seed", "sid=original")).unwrap();
    let first_http = http.clone();
    let first_request = request(&base, "first", "sid=original");
    let first =
        std::thread::spawn(move || first_http.send_with_cookie_snapshot(first_request).unwrap());
    first_arrival.recv_timeout(Duration::from_secs(5)).unwrap();
    let (response, sent) = http
        .send_with_cookie_snapshot(request(&base, "second", "sid=original"))
        .unwrap();
    assert_eq!(sent.as_deref(), Some("sid=rotated"));
    assert_eq!(response.set_cookies, ["sid=newest; Path=/"]);
    release_first.send(()).unwrap();
    let (response, sent) = first.join().unwrap();
    assert!(response.set_cookies.is_empty());
    assert_eq!(sent.as_deref(), Some("sid=rotated"));
    let (_, sent) = http
        .send_with_cookie_snapshot(request(&base, "following", "sid=original"))
        .unwrap();
    assert_eq!(sent.as_deref(), Some("sid=newest"));
    let (_, sent) = http
        .send_with_cookie_snapshot(request(&base, "other", "sid=other-account"))
        .unwrap();
    assert_eq!(sent.as_deref(), Some("sid=other-account"));
    server.join().unwrap();
}

#[test]
fn tracked_signing_rejects_redirects_without_changing_ordinary_send() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (tracked, headers) = receive(&listener);
        assert!(headers.starts_with("GET /tracked "));
        reply(tracked, "302 Found", "Location: /target\r\n");
        let (ordinary, headers) = receive(&listener);
        assert!(headers.starts_with("GET /ordinary "));
        reply(ordinary, "302 Found", "Location: /target\r\n");
        let (target, headers) = receive(&listener);
        assert!(headers.starts_with("GET /target "));
        reply(target, "200 OK", "");
    });
    let http = ReqwestTransport::new().unwrap();
    assert!(matches!(
        http.send_with_cookie_snapshot(request(&base, "tracked", "sid=fake")),
        Err(QuarkError::InvalidResponse),
    ));
    assert_eq!(
        http.send(request(&base, "ordinary", "sid=fake"))
            .unwrap()
            .status,
        200
    );
    server.join().unwrap();
}
