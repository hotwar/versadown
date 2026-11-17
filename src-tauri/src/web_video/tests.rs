use super::*;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires prepared bundled tools, VERSADOWN_VIDEO_TEST_URL and public network"]
async fn bundled_tools_download_a_public_page_with_audio() {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    extractor::set_resource_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    let source = std::env::var("VERSADOWN_VIDEO_TEST_URL").expect("provide one public video URL");
    let scanned = routing::scan(&source).await;
    assert_eq!(scanned.len(), 1, "provide exactly one public video URL");
    let entry = &scanned[0];
    let probe = std::env::var("VERSADOWN_VIDEO_TEST_PROBE").as_deref() == Ok("1");
    println!("VD-TEST-ROUTE {}", serde_json::json!({
        "kind": entry.kind, "canProbeVideo": entry.can_probe_video, "manualProbe": probe
    }));
    assert!(
        entry.kind == Some(crate::CandidateKind::WebVideo) || (probe && entry.can_probe_video),
        "a supported playback page must enter the normal video workflow automatically, or allow the requested manual probe"
    );
    let page = routing::resolve(&entry.input, probe)
        .await
        .unwrap()
        .expect("provide a supported playback URL");
    let proxy = std::env::var("VERSADOWN_VIDEO_TEST_PROXY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = VideoDownloads::new(store.clone(), 1, |_| {});
    let started = std::time::Instant::now();
    let options = VideoOptions {
        proxy,
        cookie_file: std::env::var_os("VERSADOWN_VIDEO_TEST_COOKIES")
            .filter(|value| !value.is_empty()).map(PathBuf::from),
        ..Default::default()
    };
    let preview = engine.prepare(page, &options).await.unwrap();
    println!("VD-TEST-PREVIEW {}", serde_json::to_string(&preview).unwrap());
    let requested = std::env::var("VERSADOWN_VIDEO_TEST_FORMAT").ok();
    let choice = match requested.as_ref() {
        Some(id) => preview.formats.iter().find(|format| &format.id == id),
        None => preview
            .formats
            .iter()
            .rev()
            .find(|format| format.height > 0 && format.height <= 360)
            .or_else(|| preview.formats.last()),
    }
    .expect("requested format must exist or the preview must offer a format")
    .clone();
    println!(
        "Bundled preview: {:.2}s; format {}",
        started.elapsed().as_secs_f64(),
        choice.id
    );
    let task = engine
        .submit(
            preview.preview_id,
            &choice.id,
            directory.path(),
            false,
            None,
            options,
        )
        .await
        .unwrap();
    assert_eq!(
        store.video_engine(task.id).unwrap(),
        VideoTransferEngine::Aria2
    );
    let mut saw_progress = false;
    let result = tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let task = store.find(task.id).unwrap();
            saw_progress |= task.status == TaskStatus::Downloading
                && task.completed_length > 0
                && task.download_speed > 0;
            match task.status {
                TaskStatus::Completed => return Ok(task),
                TaskStatus::Failed => return Err(task.error),
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await;
    engine.shutdown().await;
    let finished = result
        .expect("download timed out")
        .expect("download failed");
    assert_eq!(finished.progress, 100);
    assert!(finished.completed_length > 0);
    println!("VD-TEST-COMPLETED {}", serde_json::json!({
        "bytes": finished.completed_length, "format": choice.id,
        "width": choice.width, "height": choice.height, "ext": choice.ext
    }));
    let path = store.reserved_output_path(task.id).unwrap().unwrap();
    let ffmpeg = extractor::tool("ffmpeg").unwrap();
    let output = std::process::Command::new(&ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(&path)
        .output()
        .unwrap();
    let streams = String::from_utf8_lossy(&output.stderr);
    let allow_silent =
        std::env::var("VERSADOWN_VIDEO_TEST_ALLOW_SILENT").as_deref() == Ok("1");
    assert!(streams.contains("Video:"));
    assert!(allow_silent || streams.contains("Audio:"));
    if choice.width > 0 && choice.height > 0 {
        assert!(streams.contains(&format!("{}x{}", choice.width, choice.height)));
    }
    let verified = std::process::Command::new(&ffmpeg)
        .args(["-nostdin", "-v", "error", "-xerror", "-i"])
        .arg(&path)
        .args([
            "-map", "0:v:0", "-map",
            if allow_silent { "0:a:0?" } else { "0:a:0" },
            "-c", "copy",
        ])
        .arg(directory.path().join("verified.mkv"))
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "complete audio/video remux failed: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        finished.completed_length
    );
    assert!(!transfer::staging(&path, task.id).exists());
    let mut file = std::fs::File::open(&path).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let sha256 = format!("{:x}", hash.finalize());
    if let Some(output_directory) = std::env::var_os("VERSADOWN_VIDEO_TEST_OUTPUT")
        .filter(|value| !value.is_empty())
    {
        let retained_directory = std::path::PathBuf::from(output_directory).join(task.id.to_string());
        std::fs::create_dir_all(&retained_directory).unwrap();
        let retained_path = retained_directory.join(path.file_name().unwrap());
        assert_eq!(
            std::fs::copy(&path, &retained_path).unwrap(),
            finished.completed_length
        );
        println!("Retained complete video: {}", retained_path.display());
    }
    println!(
        "Bundled extraction, download and merge: {:.2}s; {} bytes; SHA256 {}; observed live progress: {}",
        started.elapsed().as_secs_f64(),
        finished.completed_length,
        sha256,
        saw_progress
    );
}

#[tokio::test]
#[ignore = "requires public sites, yt-dlp, FFmpeg and ffprobe"]
async fn public_pages_download_with_selected_resolution_and_audio() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = VideoDownloads::new(store.clone(), 2, |_| {});
    let pages = [
        "https://www.youtube.com/watch?v=jNQXAC9IVRw",
        "https://www.bilibili.com/video/BV1bK411W797?p=1",
        "https://framatube.org/videos/watch/9c9de5e8-0a1e-484a-b099-e80766180a6d",
        "https://www.dailymotion.com/video/x5kesuj",
    ];
    let batch = format!(
        "{}\nhttps://example.org/releases/sample.zip",
        pages.join("\n")
    );
    let scanned = routing::scan(&batch).await;
    assert_eq!(scanned.len(), pages.len() + 1);
    assert_eq!(
        scanned.last().unwrap().kind,
        Some(crate::CandidateKind::Direct)
    );

    let mut submitted = Vec::new();
    for entry in &scanned[..pages.len()] {
        let source = &entry.input;
        assert_eq!(entry.kind, Some(crate::CandidateKind::WebVideo), "{source}");
        let page = routing::resolve(source, true).await.unwrap().expect(source);
        let preview = engine.prepare(page, &VideoOptions::default()).await.expect(source);
        println!(
            "{} · {} · {:?}",
            preview.site,
            preview.title,
            preview
                .formats
                .iter()
                .map(|f| (f.width, f.height))
                .collect::<Vec<_>>()
        );
        let f = preview.formats.last().unwrap().clone();
        // One page can independently produce two exact quality jobs.
        let choices = if preview.site == "YouTube" && preview.formats.len() > 1 {
            vec![f, preview.formats[0].clone()]
        } else {
            vec![f]
        };
        for choice in choices {
            let task = engine
                .submit(
                    preview.preview_id,
                    &choice.id,
                    directory.path(),
                    false,
                    None,
                    VideoOptions {
                        rate: if preview.site == "Bilibili" { 96_000 } else { 0 },
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            if preview.site == "Bilibili" {
                tokio::time::timeout(Duration::from_secs(45), async {
                    while store.find(task.id).unwrap().completed_length == 0 {
                        let status = store.find(task.id).unwrap();
                        assert_ne!(status.status, TaskStatus::Failed, "{:?}", status.error);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .expect("expected real download progress");
                let paused = engine.stop(task.id, TaskStatus::Paused).await.unwrap();
                assert_eq!(paused.status, TaskStatus::Paused);
                assert!(paused.completed_length > 0);
                let final_path = store.reserved_output_path(task.id).unwrap().unwrap();
                assert!(!final_path.exists());
                assert!(std::fs::read_dir(transfer::staging(&final_path, task.id))
                    .unwrap()
                    .next()
                    .is_some());
                println!(
                    "Paused after {} bytes; resuming the same task",
                    paused.completed_length
                );
                engine.start(paused, VideoOptions::default()).await.unwrap();
            }
            submitted.push((task.id, choice));
        }
    }
    for (id, format) in submitted {
        tokio::time::timeout(Duration::from_secs(210), async {
            let mut retried_network = false;
            loop {
                let task = store.find(id).unwrap();
                if task.status == TaskStatus::Completed {
                    break;
                }
                // Exercise the same retry path offered by the queue when a live CDN
                // briefly drops the connection. Permanent errors must still fail.
                if !retried_network
                    && task.status == TaskStatus::Failed
                    && task.error.as_deref() == Some("video.error.network")
                {
                    retried_network = true;
                    println!("Retrying transient network failure for {}", task.name);
                    engine.start(task, VideoOptions::default()).await.unwrap();
                    continue;
                }
                assert_ne!(
                    task.status,
                    TaskStatus::Failed,
                    "{}: {:?}",
                    task.name,
                    task.error
                );
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await
        .unwrap();
        let path = store.reserved_output_path(id).unwrap().unwrap();
        let out = std::process::Command::new(extractor::tool("ffprobe").unwrap())
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,width,height",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let streams: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let streams = streams["streams"].as_array().unwrap();
        assert!(streams.iter().any(|s| s["codec_type"] == "video"
            && s["width"] == format.width
            && s["height"] == format.height));
        assert!(streams.iter().any(|s| s["codec_type"] == "audio"));
        println!(
            "Verified {} · {} bytes · {}x{} + audio",
            path.file_name().unwrap().to_string_lossy(),
            std::fs::metadata(&path).unwrap().len(),
            format.width,
            format.height
        );
        assert!(!transfer::staging(&path, id).exists());
    }
}

#[test]
fn mixed_paste_preserves_direct_downloads_and_normalizes_real_playback_pages() {
    use crate::parser::{scan_download_inputs, CandidateKind};
    let entries = scan_download_inputs("https://youtu.be/jNQXAC9IVRw?t=2\nhttps://m.bilibili.com/video/BV1bK411W797?p=2&share_source=app\nhttps://cdn.example.org/a.m3u8\nhttps://example.test/a.mp4");
    assert_eq!(entries.len(), 4);
    assert_eq!(
        entries
            .iter()
            .map(|e| e.kind.as_ref().unwrap())
            .collect::<Vec<_>>(),
        [
            &CandidateKind::WebVideo,
            &CandidateKind::WebVideo,
            &CandidateKind::Hls,
            &CandidateKind::Direct
        ]
    );
    assert_eq!(
        entries[0].input,
        "https://www.youtube.com/watch?v=jNQXAC9IVRw"
    );
    assert_eq!(
        entries[1].input,
        "https://www.bilibili.com/video/BV1bK411W797?p=2"
    );
    for source in [
        "https://www.youtube.com.evil.test/watch?v=jNQXAC9IVRw",
        "https://someone@www.youtube.com/watch?v=jNQXAC9IVRw",
        "https://www.youtube.com/playlist?list=123",
        "https://bilibili.com/read/test",
    ] {
        assert!(playback_url(&source.parse().unwrap()).is_none());
    }
}

#[test]
fn media_suffixed_playback_pages_remain_valid_video_task_sources() {
    let source = "https://commons.wikimedia.org/wiki/File:Example.webm";
    crate::parser::validate_task_source("web_video", source).unwrap();
    let store = TaskStore::open_in_memory().unwrap();
    let task = store
        .create_video(
            NewTask {
                kind: "web_video".into(),
                name: "Example".into(),
                source: source.into(),
                is_private: false,
            },
            "webm-480p",
            "Example.webm",
        )
        .unwrap();
    assert_eq!(store.find(task.id).unwrap().source, source);
    for invalid in [
        "https://pan.baidu.com/s/example.webm",
        "http://127.0.0.1/example.webm",
        "https://user:password@video.example/example.webm",
    ] {
        assert!(crate::parser::validate_task_source("web_video", invalid).is_err());
    }
}

#[test]
fn interrupted_video_jobs_keep_selection_and_do_not_change_other_engines() {
    let store = TaskStore::open_in_memory().unwrap();
    let mut task = store
        .create_video(
            NewTask {
                kind: "web_video".into(),
                name: "movie".into(),
                source: "https://youtu.be/jNQXAC9IVRw".into(),
                is_private: false,
            },
            "137+140",
            "jNQXAC9IVRw",
        )
        .unwrap();
    let plain = store
        .create(NewTask::direct("https://example.test/file.zip"))
        .unwrap();
    task.status = TaskStatus::Downloading;
    task.completed_length = 123;
    store.apply_transfer_snapshot(task.clone()).unwrap();
    store.recover_video_downloads().unwrap();
    let saved = store.find(task.id).unwrap();
    assert_eq!(
        (saved.status, saved.completed_length),
        (TaskStatus::Paused, 123)
    );
    assert_eq!(
        store.video_selector(task.id).unwrap(),
        ("137+140".into(), Some("jNQXAC9IVRw".into()))
    );
    assert_eq!(store.find(plain.id).unwrap().status, plain.status);
}

#[test]
fn confirmation_uses_only_the_source_and_formats_from_that_preview() {
    use super::{extractor::VideoFormat, previews::Previews};
    let previews = Previews::default();
    let format = VideoFormat {
        id: "dash-720+audio".into(),
        width: 1280,
        height: 720,
        ext: "mp4".into(),
        bytes: None,
        estimated: false,
    };
    let video = VideoPreview {
        media_id: "clip-1".into(),
        preview_id: Uuid::new_v4(),
        page_url: "https://example.org/clip/1".into(),
        site: "example.org".into(),
        title: "Clip".into(),
        thumbnail: None,
        duration: None,
        formats: vec![format.clone()],
    };
    previews.remember(&video).unwrap();
    assert_eq!(
        previews.select(video.preview_id, &format.id).unwrap().page,
        video.page_url
    );
    assert!(previews.select(video.preview_id, "best").is_err());
    assert!(previews.select(Uuid::new_v4(), &format.id).is_err());
    previews.discard(video.preview_id).unwrap();
    assert!(previews.select(video.preview_id, &format.id).is_err());
}

#[tokio::test]
async fn explicit_webpage_probe_never_submits_a_protected_input() {
    for input in [
        "magnet:?xt=urn:btih:deadbeef",
        "https://pan.baidu.com/s/1abc",
        "http://localhost/internal",
        "https://example.org/media/show.m3u8",
        "https://example.org/release.zip",
    ] {
        assert!(
            routing::resolve(input, false).await.unwrap().is_none(),
            "{input}"
        );
        assert_eq!(
            routing::resolve(input, true).await.unwrap_err(),
            "video.error.unsupported"
        );
    }
}

/// Serve an ordinary article and its video through the application's HTTP proxy.
/// The reserved .test host is never contacted; both extraction and transfer use
/// the installed yt-dlp. This exercises Generic without depending on a live site.
#[tokio::test]
#[ignore = "requires yt-dlp, FFmpeg, ffprobe and local TCP"]
async fn a_manually_probed_article_downloads_its_embedded_video() {
    use axum::{body::Bytes, response::Html, routing::get, Router};
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("fixture.mp4");
    assert!(
        std::process::Command::new(extractor::tool("ffmpeg").unwrap())
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=size=160x90:rate=10",
                "-f",
                "lavfi",
                "-i",
                "anullsrc",
                "-t",
                "1",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac"
            ])
            .arg(&media)
            .status()
            .unwrap()
            .success()
    );
    let bytes = Bytes::from(std::fs::read(&media).unwrap());
    let site = Router::new()
        .route("/article", get(|| async { Html("<html><title>Embedded sample</title><video controls src='/fixture.mp4'></video></html>") }))
        .route("/empty", get(|| async { Html("<title>Article without video</title><p>Plain text</p>") }))
        .route("/fixture.mp4", get(move || { let bytes = bytes.clone(); async move { ([("Content-Type","video/mp4")],bytes) } }));
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(socket, site).await.unwrap() });
    let store = Arc::new(TaskStore::open_in_memory().unwrap());
    let engine = VideoDownloads::new(store.clone(), 1, |_| {});
    let url = "http://embedded.example.test/article";
    let options = VideoOptions { proxy: Some(proxy), ..Default::default() };
    let scanned = routing::scan(url).await;
    assert!(scanned[0].can_probe_video);
    assert_eq!(scanned[0].kind, Some(crate::CandidateKind::Direct));
    let empty = routing::resolve("http://embedded.example.test/empty", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        engine.prepare(empty, &options).await.unwrap_err(),
        "video.error.unsupported"
    );
    let page = routing::resolve(url, true).await.unwrap().unwrap();
    let preview = engine.prepare(page, &options).await.unwrap();
    let choice = &preview.formats[0];
    let task = engine
        .submit(
            preview.preview_id,
            &choice.id,
            dir.path(),
            false,
            None,
            options,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let task = store.find(task.id).unwrap();
            if task.status == TaskStatus::Completed {
                break;
            }
            assert_ne!(task.status, TaskStatus::Failed, "{:?}", task.error);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let output = store.reserved_output_path(task.id).unwrap().unwrap();
    assert_eq!(
        std::fs::read(output).unwrap(),
        std::fs::read(media).unwrap()
    );
    server.abort();
}

#[tokio::test]
#[ignore = "requires installed yt-dlp and local TCP"]
async fn installed_extractor_aria2_reports_progress_and_resumes_with_a_fresh_url() {
    use axum::{
        body::Body,
        http::{HeaderMap, Response, StatusCode},
        routing::get,
        Router,
    };
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };

    let bytes: Arc<Vec<u8>> = Arc::new(
        (0..8 * 1024 * 1024)
            .map(|i| ((i ^ (i / 997)) % 251) as u8)
            .collect(),
    );
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::new(AtomicU64::new(0));
    let (data, requests, transferred) = (bytes.clone(), ranges.clone(), sent.clone());
    let site = Router::new()
        .route("/denied", get(|| async { StatusCode::FORBIDDEN }))
        .route(
            "/clip",
            get(move |headers: HeaderMap| {
                let (data, requests, transferred) =
                    (data.clone(), requests.clone(), transferred.clone());
                async move {
                    let range = headers
                        .get("range")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("");
                    let (start, end) = range
                        .strip_prefix("bytes=")
                        .and_then(|s| s.split_once('-'))
                        .map(|(start, end)| {
                            (
                                start.parse::<usize>().unwrap(),
                                end.parse::<usize>().unwrap_or(data.len() - 1),
                            )
                        })
                        .unwrap_or((0, data.len() - 1));
                    requests.lock().unwrap().push(start);
                    let response = Response::builder()
                        .status(if range.is_empty() {
                            StatusCode::OK
                        } else {
                            StatusCode::PARTIAL_CONTENT
                        })
                        .header("Content-Type", "video/mp4")
                        .header("Accept-Ranges", "bytes")
                        .header("Content-Length", end - start + 1)
                        .header(
                            "Content-Range",
                            format!("bytes {start}-{end}/{}", data.len()),
                        );
                    let stream =
                        futures_util::stream::unfold((data, start), move |(data, offset)| {
                            let transferred = transferred.clone();
                            async move {
                                if offset > end {
                                    return None;
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                let next = (offset + 16 * 1024).min(end + 1);
                                let chunk = data[offset..next].to_vec();
                                transferred.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                                Some((Ok::<_, std::io::Error>(chunk), (data, next)))
                            }
                        });
                    response.body(Body::from_stream(stream)).unwrap()
                }
            }),
        );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(socket, site).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("complete.mp4");
    let id = Uuid::new_v4();
    let mut paused_at = 0;
    for token in ["expired", "original", "refreshed"] {
        let metadata = dir.path().join("meta.json");
        let url = if token == "expired" {
            format!("{origin}/denied")
        } else {
            format!("{origin}/clip?token={token}")
        };
        std::fs::write(
            &metadata,
            serde_json::json!({
                "id":"aria-fixture", "title":"Clip", "extractor":"Test", "extractor_key":"Test",
                "url":url, "protocol":"http", "ext":"mp4",
                "format_id":"1", "filesize":bytes.len(), "vcodec":"avc1", "acodec":"aac"
            })
            .to_string(),
        )
        .unwrap();
        let mut cmd = transfer::transfer_command(None, 0).unwrap();
        cmd.env("no_proxy", "127.0.0.1,localhost")
            .args(["--proxy", "", "--load-info-json"])
            .arg(&metadata);
        let (cancel, stop) = oneshot::channel();
        let mut cancel = Some(cancel);
        let mut live_progress = false;
        let result = tokio::time::timeout(
            Duration::from_secs(45),
            transfer::download(
                cmd,
                "https://example.org/clip",
                "1",
                Some("aria-fixture"),
                VideoTransferEngine::Aria2,
                id,
                &output,
                stop,
                |progress| {
                    live_progress |= progress.downloaded > 0
                        && progress.downloaded < bytes.len() as u64
                        && progress.speed > 0;
                    if token == "original" && progress.downloaded >= 2 * 1024 * 1024 {
                        paused_at = progress.downloaded;
                        if let Some(cancel) = cancel.take() {
                            let _ = cancel.send(());
                        }
                    }
                },
            ),
        )
        .await
        .expect("external downloader did not exit");
        if token == "expired" {
            assert_eq!(result.unwrap_err(), "video.error.accessDenied");
            assert!(
                !output.exists(),
                "server rejection must not publish an incomplete file"
            );
            continue;
        }
        let result = result.unwrap();
        assert!(
            live_progress,
            "live progress must come before yt-dlp's final completion hook"
        );
        if token == "original" {
            assert_eq!(result, None);
            assert!(paused_at < bytes.len() as u64);
            assert!(!output.exists());
            assert!(transfer::staging(&output, id).exists());
            // Let abandoned HTTP bodies finish cancellation before counting retry traffic.
            tokio::time::sleep(Duration::from_millis(100)).await;
            sent.store(0, Ordering::Relaxed);
            ranges.lock().unwrap().clear();
        } else {
            assert_eq!(result, Some(bytes.len() as u64));
            assert!(ranges.lock().unwrap().iter().any(|start| *start > 0));
            assert!(
                sent.load(Ordering::Relaxed) < bytes.len() as u64,
                "retry must reuse checkpoint ranges instead of fetching the whole file"
            );
            assert_eq!(std::fs::read(&output).unwrap(), **bytes);
            assert!(!transfer::staging(&output, id).exists());
        }
    }
    server.abort();
}

#[tokio::test]
#[ignore = "requires installed yt-dlp and local TCP"]
async fn installed_extractor_preserves_and_resumes_a_rejected_download() {
    use axum::{
        body::Body,
        http::{HeaderMap, Response, StatusCode},
        routing::get,
        Router,
    };
    use std::sync::Mutex;

    let bytes: Vec<u8> = (0..32_768).map(|i| (i % 251) as u8).collect();
    let requested_range = Arc::new(Mutex::new(None::<String>));
    let observed = requested_range.clone();
    let content = bytes.clone();
    let site = Router::new()
        .route("/expired", get(|| async { StatusCode::FORBIDDEN }))
        .route(
            "/fresh",
            get(move |headers: HeaderMap| {
                let observed = observed.clone();
                let content = content.clone();
                async move {
                    let range = headers
                        .get("range")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    *observed.lock().unwrap() = Some(range.clone());
                    let start = range
                        .strip_prefix("bytes=")
                        .and_then(|s| s.split('-').next())
                        .and_then(|n| n.parse::<usize>().ok())
                        .unwrap_or(0);
                    Response::builder()
                        .status(if range.is_empty() {
                            StatusCode::OK
                        } else {
                            StatusCode::PARTIAL_CONTENT
                        })
                        .header("Content-Type", "video/mp4")
                        .header(
                            "Content-Range",
                            format!("bytes {start}-{}/{}", content.len() - 1, content.len()),
                        )
                        .body(Body::from(content[start..].to_vec()))
                        .unwrap()
                }
            }),
        );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(socket, site).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("complete.mp4");
    let id = Uuid::new_v4();
    let staging = transfer::staging(&output, id);
    std::fs::create_dir(&staging).unwrap();
    let partial = staging.join("payload.mp4.part");
    std::fs::write(&partial, &bytes[..4096]).unwrap();
    for endpoint in ["expired", "fresh"] {
        let metadata = dir.path().join(format!("{endpoint}.json"));
        std::fs::write(
            &metadata,
            serde_json::json!({
                "id":"resume-fixture", "title":"Clip", "extractor":"Test", "extractor_key":"Test",
                "url":format!("{origin}/{endpoint}"), "protocol":"http", "ext":"mp4",
                "format_id":"1", "filesize":bytes.len(), "vcodec":"avc1", "acodec":"aac"
            })
            .to_string(),
        )
        .unwrap();
        let mut cmd = transfer::transfer_command(None, 0).unwrap();
        cmd.args(["--proxy", "", "--load-info-json"]).arg(&metadata);
        let (_hold, stop) = oneshot::channel();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            transfer::download(
                cmd,
                "https://example.org/clip",
                "1",
                Some("resume-fixture"),
                VideoTransferEngine::Native,
                id,
                &output,
                stop,
                |_| {},
            ),
        )
        .await
        .unwrap();
        if endpoint == "expired" {
            assert_eq!(result.unwrap_err(), "video.error.accessDenied");
            assert_eq!(std::fs::read(&partial).unwrap(), &bytes[..4096]);
            assert!(!output.exists());
        } else {
            assert_eq!(result.unwrap(), Some(bytes.len() as u64));
            assert_eq!(
                requested_range.lock().unwrap().as_deref(),
                Some("bytes=4096-")
            );
            assert_eq!(std::fs::read(&output).unwrap(), bytes);
            assert!(!staging.exists());
        }
    }
    server.abort();
}

#[tokio::test]
#[ignore = "requires installed yt-dlp and local TCP"]
async fn installed_extractor_rejects_missing_fragments_without_publishing() {
    use axum::{http::StatusCode, routing::get, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let missing_requests = Arc::new(AtomicUsize::new(0));
    let hits = missing_requests.clone();
    let site = Router::new()
        .route(
            "/first",
            get(|| async { b"first video fragment".as_slice() }),
        )
        .route(
            "/missing",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NOT_FOUND
                }
            }),
        );
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(socket, site).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let metadata = dir.path().join("meta.json");
    std::fs::write(
        &metadata,
        serde_json::json!({
            "id": "fragment-fixture", "title": "Clip", "extractor": "Test", "extractor_key": "Test",
            "url": format!("{origin}/manifest.mpd"), "protocol": "http_dash_segments",
            "ext": "mp4", "format_id": "1", "vcodec": "avc1", "acodec": "aac",
            "fragments": [{"url": format!("{origin}/first")}, {"url": format!("{origin}/missing")}]
        })
        .to_string(),
    )
    .unwrap();

    for engine in [VideoTransferEngine::Native, VideoTransferEngine::Aria2] {
        let started = std::time::Instant::now();
        println!("Missing-fragment fixture: starting {engine:?}");
        missing_requests.store(0, Ordering::SeqCst);
        let mut cmd = transfer::transfer_command(None, 0).unwrap();
        cmd.args(["--proxy", "", "--fragment-retries", "0", "--load-info-json"])
            .arg(&metadata);
        let output = dir.path().join("complete.mp4");
        let (_hold, stop) = oneshot::channel();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            transfer::download(
                cmd,
                "https://example.org/clip",
                "1",
                Some("fragment-fixture"),
                engine,
                Uuid::new_v4(),
                &output,
                stop,
                |_| {},
            ),
        )
        .await;
        println!(
            "Missing-fragment fixture: {engine:?}; elapsed {:.2}s; missing requests {}; result {result:?}",
            started.elapsed().as_secs_f64(),
            missing_requests.load(Ordering::SeqCst),
        );
        assert!(missing_requests.load(Ordering::SeqCst) > 0);
        assert!(
            result.unwrap().is_err(),
            "a missing fragment must fail the download"
        );
        assert!(
            !output.exists(),
            "an incomplete video must not be published"
        );
    }
    server.abort();
}

#[tokio::test]
#[ignore = "exercises yt-dlp's real match-filter and completion-marker contract offline"]
async fn installed_extractor_rejects_changed_media_before_reusing_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("fixture.mp4");
    std::fs::write(&origin, b"public offline fixture").unwrap();
    let source = url::Url::from_file_path(&origin).unwrap();
    let original_id = r#"009&海"雪"#;
    let metadata = tmp.path().join("meta.json");
    std::fs::write(
        &metadata,
        serde_json::json!({
            "id": original_id, "title": "clip", "extractor": "Test", "extractor_key": "Test",
            "url":source.as_str(), "ext":"mp4", "format_id":"1", "vcodec":"avc1", "acodec":"aac"
        })
        .to_string(),
    )
    .unwrap();
    let cmd = || {
        let mut cmd = transfer::transfer_command(None, 0).unwrap();
        cmd.args(["--enable-file-urls", "--load-info-json"])
            .arg(&metadata);
        cmd
    };
    let final_file = tmp.path().join("complete.mp4");
    let id = Uuid::new_v4();
    let staging = transfer::staging(&final_file, id);
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join("payload.mp4"), b"old payload").unwrap();
    let (_hold, stop) = oneshot::channel();
    let label = "https://example.org/watch";
    let res = transfer::download(
        cmd(),
        label,
        "1",
        Some("008"),
        VideoTransferEngine::Native,
        id,
        &final_file,
        stop,
        |_| {},
    )
    .await;
    assert_eq!(res.unwrap_err(), "video.error.identity");
    assert!(!final_file.exists());
    assert_eq!(
        std::fs::read(staging.join("payload.mp4")).unwrap(),
        b"old payload"
    );
    std::fs::remove_file(staging.join("payload.mp4")).unwrap();
    let (_hold, stop) = oneshot::channel();
    transfer::download(
        cmd(),
        label,
        "1",
        Some(original_id),
        VideoTransferEngine::Native,
        id,
        &final_file,
        stop,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(final_file).unwrap(),
        b"public offline fixture"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn silent_extractor_success_cannot_publish_a_leftover_file() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("final.mp4");
    let id = Uuid::new_v4();
    let staging = transfer::staging(&output, id);
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join("payload.mp4"), b"stale data").unwrap();
    let mut silent = tokio::process::Command::new("sh");
    silent.args(["-c", "exit 0"]);
    let (_hold, stop) = oneshot::channel();
    let result = transfer::download(
        silent,
        "https://example.org/clip",
        "1",
        Some("expected"),
        VideoTransferEngine::Native,
        id,
        &output,
        stop,
        |_| {},
    )
    .await;
    assert_eq!(result.unwrap_err(), "video.error.identity");
    assert!(!output.exists());
}

#[test]
fn opening_an_older_video_task_preserves_its_selected_format() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("tasks.sqlite");
    let store = TaskStore::open_path(&db_path).unwrap();
    let task = store
        .create_video(
            NewTask {
                kind: "web_video".into(),
                ..NewTask::direct("https://youtu.be/jNQXAC9IVRw")
            },
            "18",
            "jNQXAC9IVRw",
        )
        .unwrap();
    drop(store);
    // Recreate the on-disk layout used before media ID binding.
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("ALTER TABLE video_task_formats DROP COLUMN media_id")
        .unwrap();
    drop(db);
    let reopened = TaskStore::open_path(&db_path).unwrap();
    assert_eq!(
        reopened.video_selector(task.id).unwrap(),
        ("18".to_owned(), None)
    );
}
