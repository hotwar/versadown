//! Exercise a previously unseen local SDK in disposable state, before it sees a cloud task.
use super::{
    runtime::Runtime, MacWorkerEngine, NativeEngine, NativeEngineError, NativeEngineSession,
    NativeProgress, NativeSubmission, NativeTaskHandle,
};
use crate::DownloadAction;
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, Method, Response, StatusCode},
    routing::get,
    Router,
};
use futures_util::stream;
use rand::RngCore;
use std::{
    convert::Infallible,
    fs, io,
    net::{Ipv4Addr, TcpListener},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const SIZE: usize = 8 * 1024 * 1024;

pub(super) fn verify(runtime: &Arc<Runtime>) -> Result<(), NativeEngineError> {
    let io_error = |_| NativeEngineError::OperationFailed;
    let scratch = tempfile::tempdir().map_err(io_error)?;
    let state = scratch.path().join("state");
    let directory = scratch.path().join("downloads");
    fs::create_dir(&state).map_err(io_error)?;
    fs::create_dir(&directory).map_err(io_error)?;
    let target = directory.join("fixture.bin");
    let fixture = Fixture::start().map_err(io_error)?;
    let peer = Uuid::new_v4().simple().to_string();
    let task = Uuid::new_v4();
    let open = || MacWorkerEngine::open(runtime, &state, NativeEngineSession::new(), &peer);
    let request = |url| NativeSubmission {
        url,
        output_path: target.clone(),
        output_name: "fixture.bin".into(),
        user_id: "0".into(),
        peer_id: peer.clone(),
        acceleration_token: None,
        gcid: None,
        cid: None,
        headers: Vec::new(),
        total_length: Some(SIZE as u64),
    };

    #[cfg(test)]
    eprintln!("Probe: starting the isolated worker");
    let mut engine = open()?;
    engine.set_limits(256 * 1024, 0)?;
    let handle =
        engine.submit_with_options(task, request(fixture.url("original")), 0, false, false)?;
    #[cfg(test)]
    eprintln!("Probe: task created, waiting for data");
    let partial = wait_for(&mut engine, handle, |p| {
        p.status == super::super::NATIVE_DOWNLOADING && p.completed >= 256 * 1024
    })?;
    #[cfg(test)]
    eprintln!("Probe: pausing after {} bytes", partial.completed);
    if partial.completed >= SIZE as u64 {
        return Err(NativeEngineError::Incompatible);
    }
    engine.control(handle, DownloadAction::Pause)?;
    if engine.poll(handle)?.status != super::super::NATIVE_STOPPED {
        return Err(NativeEngineError::Incompatible);
    }
    drop(engine);

    #[cfg(test)]
    eprintln!("Probe: reopening worker for the saved checkpoint");
    fixture.restoring.store(true, Ordering::Release);
    let mut engine = open()?;
    engine.set_limits(0, 0)?;
    let handle =
        engine.submit_with_options(task, request(fixture.url("renewed")), 0, false, true)?;
    let restored = engine.poll(handle)?;
    #[cfg(test)]
    eprintln!("Probe: restored {} bytes", restored.completed);
    if restored.completed < partial.completed {
        return Err(NativeEngineError::Incompatible);
    }
    wait_for(&mut engine, handle, |p| {
        p.status == super::super::NATIVE_COMPLETE
    })?;
    #[cfg(test)]
    eprintln!("Probe: download completed, checking Range and file contents");
    engine.finish(handle)?;
    if !fixture.resumed_range.load(Ordering::Acquire)
        || fs::metadata(&target).map_err(io_error)?.len() != SIZE as u64
        || fs::read(target).map_err(io_error)?[..] != fixture.content[..]
    {
        return Err(NativeEngineError::Incompatible);
    }
    Ok(())
}

fn wait_for(
    engine: &mut MacWorkerEngine,
    handle: NativeTaskHandle,
    ready: impl Fn(&NativeProgress) -> bool,
) -> Result<NativeProgress, NativeEngineError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let progress = engine.poll(handle)?;
        if ready(&progress) {
            return Ok(progress);
        }
        if progress.status == super::super::NATIVE_ERROR || Instant::now() >= deadline {
            #[cfg(test)]
            eprintln!("Probe: SDK state {}, error {}, bytes {}", progress.status, progress.error_code, progress.completed);
            return Err(NativeEngineError::Incompatible);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

struct Fixture {
    base_url: String,
    content: Bytes,
    restoring: Arc<AtomicBool>,
    resumed_range: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> io::Result<Self> {
        let mut bytes = vec![0; SIZE];
        rand::thread_rng().fill_bytes(&mut bytes);
        let content = Bytes::from(bytes);
        let restoring = Arc::new(AtomicBool::new(false));
        let resumed_range = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let path = format!("/{}", Uuid::new_v4());
        let base_url = format!("http://{}{path}", listener.local_addr()?);
        let mut router = Router::new();
        for (suffix, resumed) in [("original", false), ("renewed", true)] {
            let data = content.clone();
            let phase = restoring.clone();
            let ranges = resumed_range.clone();
            router = router.route(
                &format!("{path}/{suffix}"),
                get(move |method: Method, headers: HeaderMap| {
                    let data = data.clone();
                    let expired = phase.load(Ordering::Acquire) != resumed;
                    let ranges = ranges.clone();
                    async move {
                        if expired {
                            return Response::builder()
                                .status(StatusCode::GONE)
                                .body(Body::empty())
                                .unwrap();
                        }
                        let (start, end, partial) = match headers.get(header::RANGE) {
                            None => (0, data.len() - 1, false),
                            Some(value) => {
                                match value.to_str().ok().and_then(|s| byte_range(s, data.len())) {
                                    Some((start, end)) => (start, end, true),
                                    None => {
                                        return Response::builder()
                                            .status(StatusCode::RANGE_NOT_SATISFIABLE)
                                            .body(Body::empty())
                                            .unwrap()
                                    }
                                }
                            }
                        };
                        if method == Method::GET && resumed && start > 0 {
                            ranges.store(true, Ordering::Release);
                        }
                        let mut response = Response::builder()
                            .header(header::CONTENT_TYPE, "application/octet-stream")
                            .header(header::CONTENT_LENGTH, end + 1 - start)
                            .header(header::ACCEPT_RANGES, "bytes");
                        if partial {
                            response = response.status(StatusCode::PARTIAL_CONTENT).header(
                                header::CONTENT_RANGE,
                                format!("bytes {start}-{end}/{}", data.len()),
                            );
                        }
                        let data = data.slice(start..=end);
                        // Pace the initial response so pause observes a real partial payload.
                        let body = if resumed {
                            Body::from(data)
                        } else {
                            Body::from_stream(stream::unfold(data, |mut remaining| async move {
                                if remaining.is_empty() {
                                    return None;
                                }
                                tokio::time::sleep(Duration::from_millis(15)).await;
                                let chunk = remaining.split_to(remaining.len().min(16 * 1024));
                                Some((Ok::<_, Infallible>(chunk), remaining))
                            }))
                        };
                        response.body(body).unwrap()
                    }
                }),
            );
        }
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let listener = {
            let _entered = executor.enter();
            tokio::net::TcpListener::from_std(listener)?
        };
        let server = thread::Builder::new()
            .name("thunder-compatibility-http".into())
            .spawn(move || {
                executor.block_on(async {
                    // Dropping the listener and runtime also cancels incomplete HTTP responses.
                    tokio::select! {
                        _ = async { let _ = axum::serve(listener, router).await; } => {}
                        _ = stopped => {}
                    }
                });
            })?;
        Ok(Self {
            base_url,
            content,
            restoring,
            resumed_range,
            stop: Some(stop),
            server: Some(server),
        })
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}/{suffix}", self.base_url)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn byte_range(value: &str, size: usize) -> Option<(usize, usize)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<usize>().ok()?.min(size - 1)
    };
    (start <= end).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    #[test]
    #[ignore = "requires an official signed Thunder.app and loopback networking"]
    fn signed_service_passes_anonymous_probe() {
        let app = std::env::var_os("VERSADOWN_THUNDER_FIXTURE_APP")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Applications/Thunder.app"));
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            super::super::runtime::installed_runtime_for_probe_test(root.path(), &app).unwrap(),
        );
        assert!(runtime.needs_probe);
        super::super::compatibility::ensure_probed(&runtime, || {
            let result = super::verify(&runtime);
            if let Err(reason) = &result {
                eprintln!("Anonymous probe failed: {reason:?}");
            }
            result
        })
        .unwrap();
        super::super::compatibility::ensure_probed(&runtime, || {
            panic!("successful probe should be cached")
        })
        .unwrap();
        assert!(!runtime.state.join("versadown-tasks.json").exists());
        eprintln!("Thunder {} ({}) passed anonymous download, worker restart, Range resume and byte comparison", runtime.version.app_version, runtime.version.client_version);
    }
}
