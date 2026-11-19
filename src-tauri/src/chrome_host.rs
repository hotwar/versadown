//! Chrome launches this executable as a one-message Native Messaging host. Only a
//! random rendezvous ticket reaches the GUI process's argv, never a media URL.
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tauri::AppHandle;
use uuid::Uuid;

use crate::browser_media::{self, BrowserMediaRequest, BrowserVideoRequest};

const ORIGIN: &str = "chrome-extension://koomjmkmehhhpkncngdkpkbfbjjbnccf/";
const FLAG: &str = "--vd-browser-ticket";
const BACKGROUND: &str = "--vd-browser-background";
// The selected format can carry two independently signed track URLs.
const MAX_MESSAGE: usize = 1024 * 1024;
const WAIT: Duration = Duration::from_secs(35);

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid browser media handoff")
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Request {
    Media(BrowserMediaRequest),
    Video(BrowserVideoRequest),
}

impl Request {
    fn parse(bytes: &[u8]) -> io::Result<Self> {
        let request = serde_json::from_slice(bytes)?;
        let valid = match &request {
            Self::Media(request) => request.validate().is_ok(),
            Self::Video(request) => request.validate(),
        };
        if !valid {
            return Err(invalid());
        }
        Ok(request)
    }
}

fn read_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let size = u32::from_ne_bytes(size) as usize;
    if !(1..=MAX_MESSAGE).contains(&size) {
        return Err(invalid());
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn write_frame(writer: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() || data.len() > MAX_MESSAGE {
        return Err(invalid());
    }
    writer.write_all(&(data.len() as u32).to_ne_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}

// A fresh process handles Chrome's stdio. Another instance (or the existing
// GUI singleton) submits the task, so Chrome can close its host after the reply.
fn forward(bytes: &[u8]) -> io::Result<Value> {
    let request = Request::parse(bytes)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let nonce = Uuid::new_v4();
    let target = format!("{}:{}", listener.local_addr()?.port(), nonce);
    let mut gui = Command::new(std::env::current_exe()?);
    gui.args([FLAG, &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if matches!(request, Request::Video(_)) {
        gui.arg(BACKGROUND);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        gui.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        gui.creation_flags(0x01000000 | 0x00000200); // BREAKAWAY_FROM_JOB | NEW_PROCESS_GROUP
    }
    let mut child = gui.spawn()?;
    let deadline = Instant::now() + WAIT;
    listener.set_nonblocking(true)?;
    loop {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if matches!(child.try_wait()?, Some(status) if !status.success()) {
                    return Err(io::Error::other("VD failed to start"));
                }
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }
            Err(err) => return Err(err),
        };
        configure_peer(&stream)?;
        let mut received = [0; 16];
        if stream.read_exact(&mut received).is_err() || received != *nonce.as_bytes() {
            continue;
        }
        // The GUI has identified itself. Cold engine initialization and queue
        // admission have their own budget, independent of GUI process startup.
        stream.set_read_timeout(Some(Duration::from_secs(125)))?;
        write_frame(&mut stream, bytes)?;
        return Ok(serde_json::from_slice(&read_frame(&mut stream)?)?);
    }
}

// The listener polls while the GUI starts. BSD sockets inherit O_NONBLOCK on
// accept; socket timeouts only take effect once the accepted stream blocks.
fn configure_peer(peer: &TcpStream) -> io::Result<()> {
    peer.set_nonblocking(false)?;
    peer.set_read_timeout(Some(Duration::from_secs(2)))?;
    peer.set_write_timeout(Some(Duration::from_secs(2)))
}

pub fn run_if_chrome() -> bool {
    let origin = std::env::args_os().nth(1);
    if origin.as_deref() != Some(std::ffi::OsStr::new(ORIGIN)) {
        return false;
    }
    let response = match read_frame(&mut io::stdin().lock()) {
        Ok(bytes) => {
            let mut response = forward(&bytes).unwrap_or_else(
                |_| json!({"ok": false, "error": "handoff_failed", "reason": "hostRejected"}),
            );
            if serde_json::from_slice::<Value>(&bytes)
                .is_ok_and(|request| request.get("version").and_then(Value::as_u64) == Some(3))
            {
                response["protocol"] = json!(3);
            }
            response
        }
        Err(_) => json!({"ok": false, "error": "handoff_failed"}),
    };
    let _ = write_frame(&mut io::stdout().lock(), response.to_string().as_bytes());
    true
}

pub(crate) fn background_start(args: &[String]) -> bool {
    ticket(args).is_some() && args.iter().any(|arg| arg == BACKGROUND)
}

fn ticket(args: &[String]) -> Option<(u16, Uuid)> {
    let pair = args.windows(2).find(|pair| pair[0] == FLAG)?;
    let (port, token) = pair[1].split_once(':')?;
    let port = port.parse::<u16>().ok()?;
    (port > 0).then_some((port, Uuid::parse_str(token).ok()?))
}

pub(crate) fn deliver(app: &AppHandle, args: &[String]) -> bool {
    let Some((port, nonce)) = ticket(args) else {
        return false;
    };
    let app = app.clone();
    std::thread::spawn(move || {
        let exchange = || -> io::Result<()> {
            let mut stream = TcpStream::connect_timeout(
                &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(),
                Duration::from_secs(2),
            )?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.set_write_timeout(Some(Duration::from_secs(3)))?;
            stream.write_all(nonce.as_bytes())?;
            let bytes = read_frame(&mut stream)?;
            let response = tauri::async_runtime::block_on(async {
                match Request::parse(&bytes)? {
                    Request::Media(request) => {
                        Ok::<_, io::Error>(match browser_media::receive(&app, request).await {
                            Ok(()) => json!({"ok": true}),
                            Err(_) => json!({"ok": false, "error": "queue_failed"}),
                        })
                    }
                    Request::Video(request) => {
                        Ok(match browser_media::receive_video(&app, request).await {
                            Ok(reply) => reply,
                            Err(reason) => {
                                // Raw filesystem, network and subprocess errors can
                                // contain paths or signed URLs. Only return i18n keys.
                                let reason = if reason.starts_with("video.error.") {
                                    &reason
                                } else {
                                    "hostRejected"
                                };
                                json!({"ok": false, "reason": reason})
                            }
                        })
                    }
                }
            })?;
            write_frame(&mut stream, response.to_string().as_bytes())
        };
        let _ = exchange();
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_frames_and_rejects_truncated_messages() {
        let mut input = Vec::new();
        write_frame(&mut input, b"fixture").unwrap();
        assert_eq!(read_frame(&mut input.as_slice()).unwrap(), b"fixture");
        input.pop();
        assert!(read_frame(&mut input.as_slice()).is_err());
        assert!(read_frame(&mut u32::MAX.to_ne_bytes().as_slice()).is_err());
    }
    #[test]
    #[ignore = "requires local TCP"]
    fn accepted_nonblocking_listener_peer_waits_for_gui_ack() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut gui = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut host, _) = listener.accept().unwrap();
        // Force the macOS inherited mode on every test host, including Linux.
        host.set_nonblocking(true).unwrap();
        configure_peer(&host).unwrap();
        let (admit, wait_for_request) = std::sync::mpsc::channel();
        let sender = std::thread::spawn(move || {
            wait_for_request.recv().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            gui.write_all(&[1]).unwrap();
        });
        admit.send(()).unwrap();
        let mut ack = [0; 1];
        host.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [1]);
        sender.join().unwrap();
    }

    #[test]
    fn ticket_contains_only_local_port_and_nonce() {
        let args = |s: &str| vec![FLAG.to_owned(), s.to_owned()];
        let nonce = Uuid::new_v4();
        assert_eq!(ticket(&args(&format!("6300:{nonce}"))), Some((6300, nonce)));
        assert_eq!(ticket(&args(&format!("https://example.com/{nonce}"))), None);
        assert_eq!(ticket(&args("0:invalid")), None);
    }

    #[test]
    fn browser_video_messages_require_an_http_target_or_a_saved_format() {
        let request_id = Uuid::new_v4();
        let preview_id = Uuid::new_v4();
        let resolve =
            |url| json!({"version":3,"action":"resolve","requestId":request_id,"pageUrl":url});
        let selection = json!({"version":3,"action":"select","requestId":request_id,
            "previewId":preview_id,"formatId":"video+audio"});
        assert!(matches!(
            Request::parse(
                resolve("http://127.0.0.1:8877/list.m3u8")
                    .to_string()
                    .as_bytes()
            ),
            Ok(Request::Video(BrowserVideoRequest::Resolve { .. }))
        ));
        assert!(matches!(
            Request::parse(selection.to_string().as_bytes()),
            Ok(Request::Video(BrowserVideoRequest::Select { .. }))
        ));
        assert!(Request::parse(resolve("file:///test.mp4").to_string().as_bytes()).is_err());
        let mut extra = selection;
        extra["pageUrl"] = json!("https://example.test/replacement");
        assert!(Request::parse(extra.to_string().as_bytes()).is_err());
    }
}
