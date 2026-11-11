use super::{
    aria2_transfer::{completed_tracks, Aria2Transfer, Sample},
    extractor::{command, tool, Diagnostics},
    VideoTransferEngine,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Child,
    sync::oneshot,
};
use uuid::Uuid;

pub(super) fn staging(output: &Path, id: Uuid) -> PathBuf {
    output.with_file_name(format!(".versadown-video-{id}"))
}

#[derive(Default)]
pub(super) struct Progress {
    bytes: HashMap<String, u64>,
    pub downloaded: u64,
    pub speed: u64,
}

impl Progress {
    fn samples(&mut self, samples: Vec<Sample>) -> bool {
        if samples.is_empty() {
            return false;
        }
        self.speed = samples.iter().map(|sample| sample.speed).sum();
        for sample in samples {
            self.bytes.insert(sample.track, sample.downloaded);
        }
        self.downloaded = self.bytes.values().sum();
        true
    }

    fn update(&mut self, line: &str) -> bool {
        let fields: Vec<_> = line.split('\t').collect();
        let ["VD", track, bytes, speed] = fields.as_slice() else {
            return false;
        };
        let Ok(bytes) = bytes.parse::<u64>() else {
            return false;
        };
        self.bytes.insert(track.to_string(), bytes);
        self.downloaded = self.bytes.values().sum();
        self.speed = speed.parse::<f64>().unwrap_or_default() as u64;
        true
    }
}

async fn stop_child(child: &mut Child) {
    if let Some(pid) = child.id() {
        // Merging spawns ffmpeg. Stop that child too before allowing retry or deletion.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = tokio::process::Command::new("taskkill")
                .creation_flags(0x08000000)
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output()
                .await;
        }
    }
    let _ = child.kill().await;
}

pub(super) fn valid_media_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)
}

fn identity_filter(id: &str) -> Result<String, String> {
    if !valid_media_id(id) {
        return Err("video.error.identity".into());
    }
    // yt-dlp splits filters on unescaped '&', then unescapes the quoted value.
    let escaped = id.replace('&', r"\&").replace('"', r#"\""#);
    Ok(format!(r#"id="{escaped}""#))
}

pub(super) async fn download(
    mut cmd: tokio::process::Command,
    page: &str,
    selector: &str,
    media_id: Option<&str>,
    engine: VideoTransferEngine,
    id: Uuid,
    output: &Path,
    mut stop: oneshot::Receiver<()>,
    mut report: impl FnMut(&Progress),
) -> Result<Option<u64>, String> {
    let folder = staging(output, id);
    std::fs::create_dir_all(&folder).map_err(|_| "video.error.output")?;
    let ext = output
        .extension()
        .and_then(|e| e.to_str())
        .ok_or("video.error.output")?;
    if selector.contains('+') && tool("ffmpeg").is_none() {
        return Err("video.error.ffmpeg".into());
    }
    let exact = super::extractor::exact_selector(selector)?;
    let aria2 = if engine == VideoTransferEngine::Aria2 {
        super::aria2_checkpoint::validate(&folder)?;
        Some(Aria2Transfer::prepare(&mut cmd, selector).await?)
    } else {
        None
    };
    if let Some(expected) = media_id {
        // A regular match-filter silently returns success after skipping a video.
        // Require a breaking filter AND a completion marker before publishing.
        cmd.args([
            "--encoding",
            "utf-8",
            "--break-match-filters",
            &identity_filter(expected)?,
            "--print",
            "after_move:VD-DONE\t%(id)s",
        ]);
    }
    cmd.current_dir(&folder)
        .args([
            "--quiet",
            "--progress",
            "--newline",
            "--progress-delta",
            "0.5",
            "--no-simulate",
            "--progress-template",
            "download:VD\t%(info.format_id)s\t%(progress.downloaded_bytes)s\t%(progress.speed)s",
            "--continue",
            "--no-overwrites",
            "--merge-output-format",
            ext,
            "-f",
            &exact,
            "-o",
            "payload.%(ext)s",
            "--",
            page,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|_| "video.error.extractor")?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut diagnostics = Diagnostics::default();
    let mut progress = Progress::default();
    if progress.samples(completed_tracks(&folder, selector)) {
        report(&progress);
    }
    let mut poll = tokio::time::interval(Duration::from_millis(500));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut identity_confirmed = media_id.is_none(); // Old tasks predate identity binding.
    let (mut reading, mut reading_errors) = (true, true);
    while reading || reading_errors {
        tokio::select! {
            biased;
            _ = &mut stop => {
                let paused = if let Some(aria2) = &aria2 { aria2.pause(&folder).await } else { Ok(()) };
                stop_child(&mut child).await;
                paused?;
                return Ok(None);
            }
            line = lines.next_line(), if reading => match line {
                Ok(Some(line)) => {
                    if let Some(actual_id) = line.strip_prefix("VD-DONE\t") {
                        identity_confirmed = media_id.is_none_or(|expected| expected == actual_id);
                    } else if progress.update(&line) {
                        report(&progress);
                    }
                },
                _ => reading = false,
            },
            line = errors.next_line(), if reading_errors => match line {
                Ok(Some(line)) => {
                    diagnostics.observe(&line);
                },
                _ => reading_errors = false,
            },
            _ = poll.tick(), if aria2.is_some() => {
                match aria2.as_ref().unwrap().poll().await {
                    Ok(samples) => {
                        if progress.samples(samples) { report(&progress); }
                    }
                    Err(error) => {
                        let _ = aria2.as_ref().unwrap().pause(&folder).await;
                        stop_child(&mut child).await;
                        return Err(error.into());
                    }
                }
                // On Windows a closed RPC port can consume the whole interval.
                // Read yt-dlp output/EOF first and wait before the next probe;
                // native fragment transfers never start an aria2 RPC server.
                poll.reset();
            }
        }
    }
    let status = tokio::select! {
        biased;
        _ = &mut stop => { stop_child(&mut child).await; return Ok(None); }
        status = child.wait() => status.map_err(|_| "video.error.download")?,
    };
    if media_id.is_some() && status.code() == Some(101) {
        return Err("video.error.identity".into());
    }
    if !status.success() {
        return Err(diagnostics.message("video.error.download").into());
    }
    if !identity_confirmed {
        return Err("video.error.identity".into());
    }
    let finished = folder.join(format!("payload.{ext}"));
    let length = std::fs::metadata(&finished)
        .map_err(|_| "video.error.output")?
        .len();
    if length == 0 {
        return Err("video.error.output".into());
    }
    let file = tempfile::TempPath::try_from_path(finished).map_err(|_| "video.error.output")?;
    if let Err(error) = file.persist_noclobber(output) {
        let _ = error.path.keep();
        return Err("video.error.output".into());
    }
    let _ = std::fs::remove_dir_all(folder);
    Ok(Some(length))
}

pub(super) fn transfer_command(
    proxy: Option<&str>,
    rate_limit: u64,
) -> Result<tokio::process::Command, String> {
    let mut cmd = command(proxy)?;
    // Transfers need yt-dlp's normal retry budget, rather than the shorter
    // preview budget. Back off between transport failures and resume the range.
    cmd.args(["--retries", "10", "--retry-sleep", "http:exp=1:8"]);
    // yt-dlp applies its rate limit to each fragment request independently.
    // Keep limited transfers serial so concurrency cannot multiply that limit.
    cmd.args([
        "--concurrent-fragments",
        if rate_limit > 0 { "1" } else { "4" },
        "--abort-on-unavailable-fragments",
    ]);
    if rate_limit > 0 {
        cmd.arg("--limit-rate").arg(rate_limit.to_string());
    }
    Ok(cmd)
}
