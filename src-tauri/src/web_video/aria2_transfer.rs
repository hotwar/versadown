use super::extractor::aria2_binary;
use crate::aria2::{Aria2Api, Aria2RpcClient, Aria2Status, RpcCall, RpcSecret};
use serde_json::json;
use std::{
    collections::HashMap,
    net::TcpListener,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, sync::OnceCell};
use uuid::Uuid;

// yt-dlp selects its ExternalFD by executable basename. Keep one alias for the
// application lifetime, without changing the packaged executable or using a shell.
struct ToolAlias {
    _directory: tempfile::TempDir,
    executable: PathBuf,
}

static TOOL: OnceCell<ToolAlias> = OnceCell::const_new();

async fn executable() -> Result<&'static Path, String> {
    let alias = TOOL
        .get_or_try_init(|| async {
            let binary = aria2_binary()?
                .canonicalize()
                .map_err(|_| "video.error.downloader")?;
            let directory = tempfile::Builder::new()
                .prefix("versadown-video-tools-")
                .tempdir()
                .map_err(|_| "video.error.downloader")?;
            let executable = directory
                .path()
                .join(format!("aria2c{}", std::env::consts::EXE_SUFFIX));
            #[cfg(unix)]
            std::os::unix::fs::symlink(&binary, &executable)
                .map_err(|_| "video.error.downloader")?;
            #[cfg(windows)]
            if std::fs::hard_link(&binary, &executable).is_err() {
                std::fs::copy(&binary, &executable).map_err(|_| "video.error.downloader")?;
            }
            let mut version = Command::new(&executable);
            version.arg("--version").kill_on_drop(true);
            #[cfg(windows)]
            version.creation_flags(0x08000000);
            let result = tokio::time::timeout(Duration::from_secs(5), version.output())
                .await
                .map_err(|_| "video.error.downloader")?
                .map_err(|_| "video.error.downloader")?;
            if !result.status.success()
                || !String::from_utf8_lossy(&result.stdout).starts_with("aria2")
            {
                return Err("video.error.downloader".to_owned());
            }
            Ok(ToolAlias {
                _directory: directory,
                executable,
            })
        })
        .await?;
    Ok(&alias.executable)
}

pub(super) struct Sample {
    pub track: String,
    pub downloaded: u64,
    pub speed: u64,
}

pub(super) struct Aria2Transfer {
    rpc: Aria2RpcClient,
    tracks: Vec<String>,
}

impl Aria2Transfer {
    pub async fn prepare(cmd: &mut Command, selector: &str) -> Result<Self, String> {
        let executable = executable().await?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| "video.error.downloader")?;
        let port = listener
            .local_addr()
            .map_err(|_| "video.error.downloader")?
            .port();
        let secret = Uuid::new_v4().simple().to_string();
        let rpc = Aria2RpcClient::with_timeout(
            format!("http://127.0.0.1:{port}/jsonrpc"),
            RpcSecret::from_token(&secret).map_err(|_| "video.error.downloader")?,
            Duration::from_millis(500),
        )
        .map_err(|_| "video.error.downloader")?;
        // "http:" covers both HTTP and HTTPS in yt-dlp. HLS/DASH keep their
        // native downloader, including its fragment checkpoints and decryption.
        cmd.arg("--downloader")
            .arg(format!("http:{}", executable.display()));
        cmd.arg("--downloader-args").arg(format!(
            "aria2c:--enable-rpc=true --rpc-listen-all=false --rpc-allow-origin-all=false \
             --rpc-listen-port={port} --rpc-secret={secret} --continue=true \
             --state-dir=aria2-state --max-connection-per-server=4 --split=4 \
             --max-concurrent-downloads=1 --min-split-size=1M --max-tries=10 \
             --retry-wait=1 --connect-timeout=15 --timeout=30 --auto-save-interval=1 \
             --stop-with-process={} --console-log-level=error --summary-interval=0 \
             --download-result=hide --show-console-readout=false",
            std::process::id()
        ));
        // Each yt-dlp job receives its own loopback endpoint and secret. It must
        // never attach to the application's ordinary aria2 queue.
        drop(listener);
        Ok(Self {
            rpc,
            tracks: selector.split('+').map(str::to_owned).collect(),
        })
    }

    pub async fn poll(&self) -> Result<Vec<Sample>, &'static str> {
        let Ok(replies) = self
            .rpc
            .multicall(vec![
                RpcCall::new("aria2.tellActive", vec![]),
                RpcCall::new("aria2.tellWaiting", vec![json!(0), json!(10)]),
                RpcCall::new("aria2.tellStopped", vec![json!(0), json!(10)]),
            ])
            .await
        else {
            // Extraction and merging have no aria2 child; each HTTP track also
            // starts a fresh child, so an unavailable endpoint is expected.
            return Ok(vec![]);
        };
        let mut statuses = HashMap::new();
        for reply in replies {
            let value = reply.map_err(|_| "video.error.downloader")?;
            let entries: Vec<Aria2Status> =
                serde_json::from_value(value).map_err(|_| "video.error.downloader")?;
            for status in entries {
                statuses.insert(status.gid.clone(), status);
            }
        }
        let samples = self.samples(statuses.values())?;
        if !statuses.is_empty() && statuses.values().all(|status| status.status == "complete") {
            // RPC mode deliberately stays alive after completion. Release the
            // external downloader so yt-dlp can start the next track and merge.
            // Never do this for errors: exit 0 could publish an incomplete file.
            let _ = self.rpc.shutdown().await;
        }
        Ok(samples)
    }

    fn samples<'a>(
        &self,
        statuses: impl Iterator<Item = &'a Aria2Status>,
    ) -> Result<Vec<Sample>, &'static str> {
        statuses
            .map(|status| {
                if status.status == "error" {
                    return Err(transfer_error(status));
                }
                let file = status.files.first().ok_or("video.error.downloader")?;
                let track = track_for_file(&self.tracks, Path::new(&file.path))
                    .ok_or("video.error.downloader")?;
                Ok(Sample {
                    track: track.to_owned(),
                    downloaded: status
                        .completed_length
                        .parse()
                        .map_err(|_| "video.error.downloader")?,
                    speed: status
                        .download_speed
                        .parse()
                        .map_err(|_| "video.error.downloader")?,
                })
            })
            .collect()
    }

    pub async fn pause(&self, folder: &Path) -> Result<(), String> {
        if let Ok(active) = self.rpc.tell_active().await {
            for status in active {
                // Next saves its real completed ranges in aria2-state/stream;
                // the preallocated .part file length is not a resume checkpoint.
                let _ = self.rpc.force_pause(&status.gid).await;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                loop {
                    if let Ok(paused) = self.rpc.tell_status(&status.gid).await {
                        if matches!(paused.status.as_str(), "paused" | "complete" | "error")
                            && super::aria2_checkpoint::saved(folder, &paused)
                        {
                            break;
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err("video.error.checkpoint".into());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        Ok(())
    }
}

fn track_for_file<'a>(tracks: &'a [String], path: &Path) -> Option<&'a str> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".part").unwrap_or(name);
    let (stem, extension) = name.rsplit_once('.')?;
    if !matches!(extension, "mp4" | "webm" | "mkv" | "m4a") {
        return None;
    }
    if tracks.len() == 1 && stem == "payload" {
        return Some(&tracks[0]);
    }
    tracks
        .iter()
        .find(|track| stem == format!("payload.f{track}"))
        .map(String::as_str)
}

pub(super) fn completed_tracks(folder: &Path, selector: &str) -> Vec<Sample> {
    let tracks: Vec<_> = selector.split('+').map(str::to_owned).collect();
    let Ok(files) = std::fs::read_dir(folder) else {
        return vec![];
    };
    files
        .filter_map(Result::ok)
        .filter_map(|file| {
            let path = file.path();
            let extension = path.extension()?.to_str()?;
            if !matches!(extension, "mp4" | "webm" | "mkv" | "m4a") {
                return None;
            }
            let track = track_for_file(&tracks, &path)?;
            let metadata = file.metadata().ok()?;
            metadata.is_file().then(|| Sample {
                track: track.to_owned(),
                downloaded: metadata.len(),
                speed: 0,
            })
        })
        .collect()
}

fn transfer_error(status: &Aria2Status) -> &'static str {
    let message = status
        .error_message
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // aria2 reports HTTP status separately from its own numeric exit reason.
    if [
        "http 429",
        "status=429",
        "status code 429",
        "http error 429",
    ]
    .iter()
    .any(|s| message.contains(s))
    {
        "video.error.rateLimited"
    } else if [401, 403, 412].into_iter().any(|code| {
        message.contains(&format!("http {code}"))
            || message.contains(&format!("status={code}"))
            || message.contains(&format!("status code {code}"))
            || message.contains(&format!("http error {code}"))
    }) {
        "video.error.accessDenied"
    } else {
        match status.error_code.as_deref() {
            Some("3") => "video.error.unavailable",
            Some("9" | "13" | "16" | "17" | "18") => "video.error.output",
            _ => "video.error.network",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_http_errors_keep_the_server_reason() {
        for (message, expected) in [
            ("HTTP 403", "video.error.accessDenied"),
            ("HTTP 429", "video.error.rateLimited"),
            ("SSL connection interrupted", "video.error.network"),
        ] {
            let status: Aria2Status = serde_json::from_value(json!({
                "gid": "0123456789abcdef", "status": "error", "totalLength": "0",
                "completedLength": "0", "downloadSpeed": "0", "uploadSpeed": "0",
                "connections": "0", "errorCode": "22", "errorMessage": message,
            }))
            .unwrap();
            assert_eq!(transfer_error(&status), expected);
        }
    }

    #[test]
    fn completed_tracks_exclude_preallocated_and_native_partial_files() {
        let folder = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("payload.f401.mp4.part", 4096),
            ("payload.f140.m4a", 128),
            ("payload.f401.mp4.ytdl", 100),
        ] {
            std::fs::File::create(folder.path().join(name))
                .unwrap()
                .set_len(bytes)
                .unwrap();
        }
        let completed = completed_tracks(folder.path(), "401+140");
        assert_eq!(completed.len(), 1);
        assert_eq!(
            (&*completed[0].track, completed[0].downloaded),
            ("140", 128)
        );
    }

    #[test]
    fn track_names_match_complete_format_ids() {
        let tracks = vec!["dash.4".into(), "140".into()];
        assert_eq!(
            track_for_file(&tracks, Path::new("/tmp/payload.fdash.4.mp4.part")),
            Some("dash.4")
        );
        assert_eq!(
            track_for_file(&tracks, Path::new("payload.f1400.m4a.part")),
            None
        );
        assert_eq!(
            track_for_file(
                &["1".into(), "1.a".into()],
                Path::new("payload.f1.a.mp4.part")
            ),
            Some("1.a")
        );
        assert_eq!(
            track_for_file(&["401".into()], Path::new("payload.temp.mp4")),
            None
        );
    }
}
