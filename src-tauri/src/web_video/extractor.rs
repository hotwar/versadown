use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use uuid::Uuid;

use super::sites::PlaybackPage;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoFormat {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub ext: String,
    pub bytes: Option<u64>,
    pub estimated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPreview {
    pub preview_id: Uuid,
    #[serde(skip)]
    pub(super) media_id: String,
    pub page_url: String,
    pub site: String,
    pub title: String,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub formats: Vec<VideoFormat>,
}

#[derive(Deserialize, Default)]
struct Format {
    #[serde(default)]
    format_id: String,
    width: Option<f64>,
    height: Option<f64>,
    ext: Option<String>,
    protocol: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    // yt-dlp uses true for confirmed DRM and can return "maybe" for unchecked streams.
    #[serde(default)]
    has_drm: serde_json::Value,
    filesize: Option<f64>,
    filesize_approx: Option<f64>,
    fps: Option<f64>,
}

impl Format {
    fn downloadable(&self) -> bool {
        // These transports need an external streaming client. Bundled FFmpeg
        // only processes files already fetched by yt-dlp.
        let remote_transport = self.protocol.as_deref().is_some_and(|p| {
            p.starts_with("rtmp") || matches!(p, "rtsp" | "mms" | "mmsh" | "mmst")
        });
        self.has_drm != true && safe_id(&self.format_id) && !remote_transport
    }

    fn size(&self) -> Option<u64> {
        self.filesize
            .or(self.filesize_approx)
            .filter(|n| *n > 0. && *n < i64::MAX as f64)
            .map(|n| n.round() as u64)
    }

    fn is_audio_track(&self) -> bool {
        // HLS audio renditions can omit acodec. Missing means unknown;
        // only the literal "none" rules out an audio stream.
        self.vcodec.as_deref() == Some("none")
            && self.acodec.as_deref() != Some("none")
    }

    fn video_ext(&self) -> Option<&str> {
        self.ext.as_deref().filter(|&ext| {
            self.vcodec.as_deref() != Some("none") && matches!(ext, "mp4" | "webm" | "mkv")
        })
    }
}

#[derive(Deserialize)]
struct Extracted {
    id: Option<String>,
    #[serde(rename = "_type")]
    kind: Option<String>,
    title: Option<String>,
    webpage_url: Option<String>,
    extractor_key: Option<String>,
    thumbnail: Option<String>,
    duration: Option<f64>,
    formats: Option<Vec<Format>>,
    #[serde(rename = "url")]
    media_url: Option<String>,
    is_live: Option<bool>,
    live_status: Option<String>,
    availability: Option<String>,
    #[serde(rename = "_has_drm")]
    has_drm: Option<bool>,
}

// When a page only supplies one media URL, -J can put its fields on the
// result itself and omit formats[]. Read only format fields in this case.
fn decode(bytes: &[u8]) -> Result<Extracted, String> {
    let mut raw: Extracted = serde_json::from_slice(bytes).map_err(|_| "video.error.extract")?;
    if raw.formats.as_ref().is_none_or(Vec::is_empty)
        && raw
            .media_url
            .take()
            .and_then(|u| url::Url::parse(&u).ok())
            .is_some_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        && raw.has_drm != Some(true)
        && matches!(raw.kind.as_deref(), None | Some("video"))
    {
        let single: Format = serde_json::from_slice(bytes).map_err(|_| "video.error.extract")?;
        if !single.format_id.is_empty() {
            raw.formats = Some(vec![single]);
        }
    }
    Ok(raw)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@'))
}

fn safe_selection(ids: &str) -> bool {
    let parts = ids.split('+');
    parts.clone().count() <= 2 && parts.into_iter().all(safe_id)
}

// An ID can itself be a reserved selector such as "mp4" or "best". Quoted
// equality keeps those and numeric IDs literal (an unquoted number compares numerically).
pub(super) fn exact_selector(ids: &str) -> Result<String, String> {
    if !safe_selection(ids) {
        return Err("video.error.formats".into());
    }
    Ok(ids
        .split('+')
        .map(|id| format!(r#"all[format_id="{id}"]"#))
        .collect::<Vec<_>>()
        .join("+"))
}

impl VideoFormat {
    pub fn valid(&self) -> bool {
        self.width <= 65535
            && self.height <= 65535
            && matches!(self.ext.as_str(), "mp4" | "webm" | "mkv")
            && safe_selection(&self.id)
    }
}

fn compatible_audio(video_ext: &str, audio_ext: Option<&str>) -> bool {
    matches!(
        (video_ext, audio_ext),
        ("mp4", Some("m4a" | "mp4")) | ("webm", Some("webm"))
    )
}

fn quality(f: &Format, ext: &str, audio: Option<&Format>) -> VideoFormat {
    let container = match audio {
        Some(track) if !compatible_audio(ext, track.ext.as_deref()) => "mkv",
        _ => ext,
    };
    VideoFormat {
        id: audio.map_or_else(
            || f.format_id.clone(),
            |a| format!("{}+{}", f.format_id, a.format_id),
        ),
        width: f.width.unwrap_or_default().round() as u32,
        height: f.height.unwrap_or_default().round() as u32,
        ext: container.into(),
        bytes: match audio {
            Some(track) => f
                .size()
                .zip(track.size())
                .and_then(|(v, a)| v.checked_add(a)),
            None => f.size(),
        },
        estimated: audio.is_some() || f.filesize.is_none(),
    }
}

fn choices(raw: &Extracted, can_merge: bool) -> Result<Vec<VideoFormat>, String> {
    let formats = raw.formats.as_deref().unwrap_or_default();
    let playable: Vec<_> = formats.iter().filter(|f| f.downloadable()).collect();
    let audio: Vec<_> = playable
        .iter()
        .copied()
        .filter(|f| f.is_audio_track())
        .collect();
    let mut qualities = BTreeMap::new();
    let mut needs_merge = false;
    for f in playable {
        let Some(ext) = f.video_ext() else {
            continue;
        };
        let track = if f.acodec.as_deref() == Some("none") && !audio.is_empty() {
            if !can_merge {
                needs_merge = true;
                continue;
            }
            audio
                .iter()
                .rev()
                .copied()
                .find(|a| compatible_audio(ext, a.ext.as_deref()))
                .or_else(|| audio.last().copied())
        } else {
            None
        };
        let choice = quality(f, ext, track);
        let key = (choice.height, choice.width);
        // Keep one format per pixel size. Prefer AVC, then frame rate, then yt-dlp's order.
        let codec = f.vcodec.as_deref().unwrap_or_default();
        let rank = (
            codec.starts_with("avc") || codec == "h264",
            f.fps.unwrap_or_default() as u32,
        );
        if choice.valid() && qualities.get(&key).is_none_or(|(old, _)| rank >= *old) {
            qualities.insert(key, (rank, choice));
        }
    }

    if qualities.is_empty() {
        let error = if needs_merge {
            "video.error.ffmpeg"
        } else if raw.has_drm == Some(true) || formats.iter().any(|f| f.has_drm == true) {
            "video.error.drm"
        } else if matches!(
            raw.availability.as_deref(),
            Some("private" | "premium_only" | "subscriber_only" | "needs_auth")
        ) {
            "video.error.restricted"
        } else {
            "video.error.formats"
        };
        return Err(error.into());
    }
    // DASH and HLS variants sometimes describe the same resolution differently.
    // Prefer the measured rectangle over a matching height- or width-only label.
    let (widths, heights): (HashSet<_>, HashSet<_>) = qualities
        .keys()
        .filter(|(h, w)| *h > 0 && *w > 0)
        .map(|&(h, w)| (w, h))
        .unzip();
    let has_other_choices = qualities.len() > 1;
    qualities.retain(|&(h, w), _| match (h, w) {
        (0, 0) => !has_other_choices,
        (0, width) => !widths.contains(&width),
        (height, 0) => !heights.contains(&height),
        _ => true,
    });
    Ok(qualities
        .into_values()
        .rev()
        .map(|(_, format)| format)
        .collect())
}

fn preview(page: PlaybackPage, raw: Extracted, can_merge: bool) -> Result<VideoPreview, String> {
    match raw.kind.as_deref() {
        Some("playlist" | "multi_video" | "compat_list") => {
            return Err("video.error.collection".into())
        }
        None | Some("video") => (),
        _ => return Err("video.error.unsupported".into()),
    }
    if raw.is_live == Some(true)
        || matches!(
            raw.live_status.as_deref(),
            Some("is_live" | "is_upcoming" | "post_live")
        )
    {
        return Err("video.error.live".into());
    }
    let formats = choices(&raw, can_merge)?;
    let page = page
        .resolve(raw.webpage_url.as_deref())
        .ok_or("video.error.unavailable")?;
    let label = match page.site.as_str() {
        "YouTube" | "Bilibili" => page.site,
        _ => raw
            .extractor_key
            .filter(|s| safe_id(s) && !s.eq_ignore_ascii_case("generic"))
            .unwrap_or(page.site),
    };
    let title = raw
        .title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| label.clone());
    Ok(VideoPreview {
        preview_id: Uuid::new_v4(),
        media_id: raw
            .id
            .filter(|id| super::transfer::valid_media_id(id))
            .ok_or("video.error.identity")?,
        page_url: page.url.into(),
        site: label,
        title,
        thumbnail: raw.thumbnail.as_deref().and_then(secure_thumbnail),
        duration: raw.duration,
        formats,
    })
}

// Some extractors return HTTP covers even when the CDN supports HTTPS.
fn secure_thumbnail(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return None;
    }
    match url.scheme() {
        "https" => (),
        "http" => url.set_scheme("https").ok()?,
        _ => return None,
    }
    Some(url.into())
}

// Tauri resolves the resource root for .app, NSIS, AppImage and native Linux packages.
static BUNDLED_TOOLS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static RESOURCE_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub(crate) fn set_resource_dir(resources: &Path) {
    let _ = RESOURCE_ROOT.set(resources.to_owned());
    let _ = BUNDLED_TOOLS.set(
        resources
            .join("binaries")
            .join(format!("video-{}", std::env::consts::OS)),
    );
}

pub(super) fn aria2_binary() -> Result<PathBuf, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    crate::engine_binary::EngineBinary::for_current_target()
        .and_then(|binary| {
            binary.resolve(
                manifest,
                RESOURCE_ROOT.get().map_or(manifest, PathBuf::as_path),
            )
        })
        .map_err(|_| "video.error.downloader".into())
}

fn bundled_tool(name: &str) -> Option<PathBuf> {
    let file = BUNDLED_TOOLS
        .get()?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    file.is_file().then_some(file)
}

pub(super) fn tool(name: &str) -> Option<PathBuf> {
    if let Some(file) = bundled_tool(name) {
        return Some(file);
    }
    let bin = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let mut paths = Vec::new();
    #[cfg(debug_assertions)]
    if name == "yt-dlp" {
        let bin_dir = if cfg!(windows) { "Scripts" } else { "bin" };
        paths.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../.tools/video")
                .join(bin_dir),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path).filter(|p| p.is_absolute()));
    }
    #[cfg(target_os = "macos")]
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    paths
        .into_iter()
        .map(|dir| dir.join(&bin))
        .find(|path| path.is_file())
}

pub(super) fn command(proxy: Option<&str>) -> Result<Command, String> {
    let mut cmd = base_command()?;
    super::proxy::apply(&mut cmd, proxy)?;
    Ok(cmd)
}

pub(super) fn base_command() -> Result<Command, String> {
    let mut cmd = Command::new(tool("yt-dlp").ok_or("video.error.extractor")?);
    cmd.kill_on_drop(true).stdin(Stdio::null()).args([
        "--ignore-config",
        "--no-plugin-dirs",
        "--no-playlist",
        "--playlist-items",
        "1",
        "--no-cache-dir",
        "--no-colors",
        "--downloader",
        "m3u8,dash:native",
        "--socket-timeout",
        "15",
        "--retries",
        "2",
        "--extractor-retries",
        "1",
    ]);
    if let Some(qjs) = bundled_tool("qjs") {
        cmd.args(["--no-js-runtimes", "--js-runtimes"])
            .arg(format!("quickjs:{}", qjs.display()));
    } else if let Some(node) = tool("node") {
        cmd.arg("--js-runtimes")
            .arg(format!("node:{}", node.display()));
    }
    if let Some(ffmpeg) = tool("ffmpeg") {
        cmd.arg("--ffmpeg-location").arg(ffmpeg);
    }
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    Ok(cmd)
}

#[derive(Default)]
pub(super) struct Diagnostics {
    error: Option<&'static str>,
    warning: Option<&'static str>,
}

impl Diagnostics {
    pub fn observe(&mut self, line: &str) {
        let line = line.trim_start();
        let reason = classify_message(line);
        // yt-dlp can put the final downloader error on the line after ERROR:.
        if line.starts_with("ERROR:") || line.starts_with("[download] Got error:") {
            // Missing-fragment and other summary errors must not hide the
            // server's more specific rejection or rate-limit response.
            if self.error.is_some_and(|error| {
                reason == "video.error.extract"
                    || (matches!(
                        error,
                        "video.error.accessDenied" | "video.error.rateLimited"
                    ) && reason == "video.error.network")
            }) {
                return;
            }
            self.error = Some(reason);
        } else if line.starts_with("WARNING:")
            && self.error.is_none()
            && reason != "video.error.extract"
        {
            self.warning = Some(reason);
        }
    }

    pub fn message(&self, default: &'static str) -> &'static str {
        self.error
            .or(self.warning)
            .filter(|reason| *reason != "video.error.extract")
            .unwrap_or(default)
    }
}

pub(super) fn diagnostic(raw: &str) -> &'static str {
    let mut diagnostics = Diagnostics::default();
    for line in raw.lines() {
        diagnostics.observe(line);
    }
    diagnostics.message("video.error.extract")
}

fn classify_message(message: &str) -> &'static str {
    let text = message.to_ascii_lowercase();
    let contains = |terms: &[&str]| terms.iter().any(|term| text.contains(term));
    if contains(&[
        "none of these impersonate targets are available",
        "no impersonate target is available",
    ]) {
        "video.error.impersonation"
    } else if text.contains("drm") {
        "video.error.drm"
    } else if contains(&[
        "unsupported url",
        "no video found",
        "no video could be found",
        "protocol not found",
    ]) {
        "video.error.unsupported"
    } else if text.contains("requested format is not available") {
        "video.error.formatGone"
    } else if contains(&[
        "failed to load cookies",
        "does not look like a netscape format cookies file",
        "does not look like a netscape format cookie file",
    ]) {
        "video.error.cookieFile"
    } else if contains(&["fresh cookies", "--cookies"]) {
        "video.error.cookies"
    } else if contains(&[
        "sign in",
        "log in",
        "login",
        "logged-in",
        "members-only",
        "members only",
        "not a bot",
    ]) {
        "video.error.restricted"
    } else if text.contains("http error 429") {
        "video.error.rateLimited"
    } else if contains(&["http error 401", "http error 403", "http error 412"]) {
        "video.error.accessDenied"
    } else if contains(&[
        "http error 5",
        "bad gateway",
        "service unavailable",
        "connection",
        "timed out",
        "unable to download webpage",
        "proxy",
        "did not get any data blocks",
        "fragment not found",
        "incomplete read",
        "unexpected_eof",
        "certificate verify failed",
    ]) || (text.contains("fragment ") && text.contains("not found"))
    {
        "video.error.network"
    } else if contains(&[
        "unavailable",
        "removed",
        "private video",
        "not available",
        "http error 404",
        "geo-restricted",
    ]) {
        "video.error.unavailable"
    } else if contains(&["no video formats", "only images are available"]) {
        "video.error.formats"
    } else {
        "video.error.extract"
    }
}

async fn read_bounded(
    reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "video.error.extract")?;
    if bytes.len() > limit {
        return Err("video.error.metadataLimit");
    }
    Ok(bytes)
}

async fn metadata(mut cmd: Command) -> Result<Vec<u8>, String> {
    // A long HLS/DASH stream can produce JSON containing per-fragment URLs.
    // Bound both pipes before deserializing and always reap an interrupted probe.
    const JSON_LIMIT: usize = 32 * 1024 * 1024;
    const STDERR_LIMIT: usize = 256 * 1024;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "video.error.extractor")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output = tokio::time::timeout(Duration::from_secs(90), async {
        let (bytes, error, status) = tokio::try_join!(
            read_bounded(stdout, JSON_LIMIT),
            read_bounded(stderr, STDERR_LIMIT),
            async { child.wait().await.map_err(|_| "video.error.extract") }
        )?;
        if !status.success() {
            return Err(diagnostic(&String::from_utf8_lossy(&error)));
        }
        Ok(bytes)
    })
    .await
    .unwrap_or(Err("video.error.network"));
    if output.is_err() && child.id().is_some() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    output.map_err(str::to_owned)
}

pub(crate) async fn inspect(
    page: PlaybackPage,
    proxy: Option<&str>,
    cookie_file: Option<&Path>,
) -> Result<VideoPreview, String> {
    let cookies = super::cookies::CookieFile::load(cookie_file)?;
    let mut cmd = command(proxy)?;
    if let Some(cookies) = &cookies {
        cookies.apply(&mut cmd);
    }
    cmd.args([
        "--dump-single-json",
        "--skip-download",
        "--flat-playlist",
        "--",
    ])
    .arg(page.url.as_str());
    let bytes = metadata(cmd).await?;
    preview(page, decode(&bytes)?, tool("ffmpeg").is_some())
}

#[cfg(test)]
mod tests {
    use super::super::sites::{manual_page, playback_url};
    use super::*;
    use serde_json::json;

    #[test]
    fn server_rejections_do_not_imply_login_and_rate_limits_advise_waiting() {
        for status in [
            "401: Unauthorized",
            "403: Forbidden",
            "412: Precondition Failed",
        ] {
            assert_eq!(
                diagnostic(&format!(
                    "ERROR: unable to download video data: HTTP Error {status}"
                )),
                "video.error.accessDenied"
            );
        }
        assert_eq!(
            diagnostic("ERROR: unable to download video data: HTTP Error 429: Too Many Requests"),
            "video.error.rateLimited"
        );
    }

    #[test]
    fn cookie_requests_and_invalid_files_are_distinct_from_login_requirements() {
        for message in [
            "ERROR: [douyin] clip: Fresh cookies (not necessarily logged in) are needed",
            "ERROR: [youtube] clip: Sign in to confirm you're not a bot. Use --cookies-from-browser or --cookies for authentication.",
            "ERROR: [vimeo] clip: The web client only works when logged-in. Use --cookies to provide account credentials.",
        ] {
            assert_eq!(diagnostic(message), "video.error.cookies");
        }
        for message in [
            "ERROR: failed to load cookies",
            "ERROR: 'cookies.txt' does not look like a Netscape format cookies file",
        ] {
            assert_eq!(diagnostic(message), "video.error.cookieFile");
        }
        assert_eq!(
            diagnostic("ERROR: [site] clip: Sign in to view this video"),
            "video.error.restricted"
        );
        assert_eq!(
            diagnostic("ERROR: [site] clip: Login required"),
            "video.error.restricted"
        );
    }

    #[test]
    fn terminal_error_survives_login_advice_and_unrelated_titles() {
        let mut diagnostics = Diagnostics::default();
        diagnostics.observe("ERROR: unable to download video data: HTTP Error 403: Forbidden");
        diagnostics.observe("WARNING: Sign in using cookies if access requires authentication");
        diagnostics.observe("[download] Destination: Login and cookies.mp4");
        assert_eq!(
            diagnostics.message("video.error.download"),
            "video.error.accessDenied"
        );

        assert_eq!(
            diagnostic(
                "WARNING: Some formats may yield HTTP Error 403\nERROR: ffmpeg exited with code 1"
            ),
            "video.error.extract"
        );
    }

    #[test]
    fn specific_http_error_survives_generic_fragment_summary() {
        assert_eq!(
            diagnostic("ERROR: [download] Got error: HTTP Error 403: Forbidden. Giving up after 2 retries\nERROR: fragment 7 not found, unable to continue"),
            "video.error.accessDenied"
        );
        assert_eq!(
            diagnostic("ERROR: fragment 7 not found, unable to continue"),
            "video.error.network"
        );
    }

    #[test]
    fn multiline_tls_failure_does_not_keep_an_earlier_login_warning() {
        let raw = "WARNING: Sign in using cookies if access requires authentication\nERROR: \n[download] Got error: [SSL: UNEXPECTED_EOF_WHILE_READING] EOF occurred in violation of protocol (_ssl.c:1081). Giving up after 2 retries";
        assert_eq!(diagnostic(raw), "video.error.network");
        let mut diagnostics = Diagnostics::default();
        for line in raw.lines() {
            diagnostics.observe(line);
        }
        assert_eq!(
            diagnostics.message("video.error.download"),
            "video.error.network"
        );
    }

    fn data(formats: serde_json::Value) -> Extracted {
        serde_json::from_value(json!({"id":"test-clip","title":"test","webpage_url":"https://www.bilibili.com/video/BV1bK411W797","formats":formats})).unwrap()
    }
    fn page() -> PlaybackPage {
        playback_url(
            &"https://www.bilibili.com/video/BV1bK411W797?p=2"
                .parse()
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn keeps_exact_parts_and_picks_one_playable_variant_per_resolution() {
        let formats = json!([
            {"format_id":"audio-opus","vcodec":"none","acodec":"opus","ext":"webm","filesize":90},
            {"format_id":"audio-aac","vcodec":"none","acodec":"aac","ext":"m4a","filesize":80},
            {"format_id":"mp4-1080","vcodec":"avc1","acodec":"none","ext":"mp4","width":1920,"height":1080,"filesize":1000},
            {"format_id":"vp9-1080","vcodec":"vp9","acodec":"none","ext":"webm","width":1920,"height":1080},
            {"format_id":"single","vcodec":"avc1","acodec":"aac","ext":"mp4","width":640,"height":360,"filesize":500},
            {"format_id":"paid","has_drm":true,"vcodec":"avc1","ext":"mp4","width":3840,"height":2160},
            {"format_id":"legacy","protocol":"rtmp_ffmpeg","ext":"mp4","width":3840,"height":2160},
            {"format_id":"board","ext":"mhtml","width":80,"height":45}
        ]);
        let mut extracted = data(formats.clone());
        extracted.thumbnail = Some("http://i0.hdslb.com/bfs/archive/cover.jpg".into());
        let full = preview(page(), extracted, true).unwrap();
        assert_eq!(
            full.thumbnail.as_deref(),
            Some("https://i0.hdslb.com/bfs/archive/cover.jpg")
        );
        assert_eq!(
            full.page_url,
            "https://www.bilibili.com/video/BV1bK411W797?p=2"
        );
        assert_eq!(
            full.formats
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            ["mp4-1080+audio-aac", "single"]
        );
        assert_eq!(full.formats[0].bytes, Some(1080));
        let without_ffmpeg = preview(page(), data(formats), false).unwrap();
        assert_eq!(without_ffmpeg.formats.len(), 1);
        assert_eq!(without_ffmpeg.formats[0].id, "single");
    }

    #[test]
    fn missing_merger_or_live_source_is_actionable() {
        let mut raw = data(json!([
            {"format_id":"v","vcodec":"avc1","acodec":"none","ext":"mp4","width":640,"height":360},
            {"format_id":"a","vcodec":"none","acodec":"aac","ext":"m4a"}
        ]));
        assert_eq!(
            preview(page(), data(json!([])), true).unwrap_err(),
            "video.error.formats"
        );
        raw.is_live = Some(true);
        assert_eq!(preview(page(), raw, true).unwrap_err(), "video.error.live");
        let raw = data(json!([
            {"format_id":"v","vcodec":"avc1","acodec":"none","ext":"mp4","width":640,"height":360},
            {"format_id":"a","vcodec":"none","acodec":"aac","ext":"m4a"}
        ]));
        assert_eq!(
            preview(page(), raw, false).unwrap_err(),
            "video.error.ffmpeg"
        );
    }

    #[test]
    fn hls_audio_with_unknown_codec_is_merged_and_requires_ffmpeg() {
        let formats = json!([
            {"format_id":"hls-audio0-low","vcodec":"none","ext":"mp4","protocol":"m3u8_native"},
            {"format_id":"hls-audio0-medium","vcodec":"none","ext":"mp4","protocol":"m3u8_native"},
            {"format_id":"hls-145","vcodec":"avc1.42c00d","acodec":"none","ext":"mp4","width":320,"height":180},
            {"format_id":"hls-216","vcodec":"avc1.42c01e","acodec":"none","ext":"mp4","width":320,"height":180}
        ]);
        let full = preview(page(), data(formats.clone()), true).unwrap();
        assert_eq!(full.formats.len(), 1);
        let f = &full.formats[0];
        assert_eq!(f.id, "hls-216+hls-audio0-medium");
        assert_eq!((f.width, f.height, f.ext.as_str()), (320, 180, "mp4"));
        assert_eq!(
            preview(page(), data(formats), false).unwrap_err(),
            "video.error.ffmpeg"
        );
    }

    #[test]
    fn unknown_audio_codec_keeps_container_compatibility_and_muxed_streams() {
        let formats = json!([
            {"format_id":"audio","vcodec":"none","acodec":null,"ext":"webm"},
            {"format_id":"frames","vcodec":"none","acodec":"none","ext":"mp4"},
            {"format_id":"video-mp4","vcodec":"avc1","acodec":"none","ext":"mp4","width":640,"height":360},
            {"format_id":"video-webm","vcodec":"vp9","acodec":"none","ext":"webm","width":1280,"height":720},
            {"format_id":"muxed-unknown","ext":"mp4","width":320,"height":180}
        ]);
        let full = preview(page(), data(formats), true).unwrap();
        assert_eq!(
            full.formats
                .iter()
                .map(|f| (f.id.as_str(), f.ext.as_str()))
                .collect::<Vec<_>>(),
            [
                ("video-webm+audio", "webm"),
                ("video-mp4+audio", "mkv"),
                ("muxed-unknown", "mp4")
            ]
        );
    }

    #[test]
    fn genuinely_silent_video_does_not_require_a_merger() {
        let formats = json!([
            {"format_id":"silent","vcodec":"avc1","acodec":"none","ext":"mp4","width":640,"height":360},
            {"format_id":"frames","vcodec":"none","acodec":"none","ext":"mhtml"}
        ]);
        let full = preview(page(), data(formats), false).unwrap();
        assert_eq!(full.formats.len(), 1);
        assert_eq!(full.formats[0].id, "silent");
    }

    #[test]
    fn single_embedded_stream_with_unspecified_dimensions_keeps_its_literal_id() {
        let raw = decode(
            br#"{"_type":"video","id":"clip","title":"Clip","url":"https://cdn.example/payload",
                              "extractor_key":"Generic","format_id":"mp4","ext":"mp4"}"#,
        )
        .unwrap();
        let origin: url::Url = "https://public.example/article/123".parse().unwrap();
        let preview = preview(manual_page(&origin).unwrap(), raw, false).unwrap();
        assert_eq!(preview.page_url, origin.as_str());
        assert_eq!(preview.site, "public.example");
        let f = &preview.formats[0];
        assert_eq!((f.width, f.height, &*f.id), (0, 0, "mp4"));
        assert_eq!(exact_selector(&f.id).unwrap(), r#"all[format_id="mp4"]"#);
        assert_eq!(
            exact_selector("1080p:h264@60+140").unwrap(),
            r#"all[format_id="1080p:h264@60"]+all[format_id="140"]"#
        );
        for invalid in ["", "18/22", "18++140", "all[width>500]", "18+140+99"] {
            assert!(exact_selector(invalid).is_err());
        }
    }

    #[test]
    fn partial_dimensions_remain_usable_and_unlabeled_audio_and_storyboards_are_excluded() {
        let formats = json!([
            {"format_id":"audio","vcodec":"none","ext":"webm","acodec":"opus"},
            {"format_id":"frames","vcodec":"none","acodec":"none","width":320,"height":180,"ext":"mhtml"},
            {"format_id":"default","ext":"mp4"},
            {"format_id":"720","ext":"mp4","height":720.0,"filesize_approx":125.5,"has_drm":"maybe"}
        ]);
        let p = preview(page(), data(formats), true).unwrap();
        assert_eq!(p.formats.len(), 1);
        assert_eq!(
            (
                &*p.formats[0].id,
                p.formats[0].width,
                p.formats[0].height,
                p.formats[0].bytes
            ),
            ("720", 0, 720, Some(126))
        );
    }

    #[test]
    fn classify_collection_protection_and_login_even_without_a_title_or_formats() {
        for (raw, error) in [
            (
                json!({"_type":"playlist","entries":[{}]}),
                "video.error.collection",
            ),
            (
                json!({"_type":"multi_video","formats":null}),
                "video.error.collection",
            ),
            (json!({"_type":"video","_has_drm":true}), "video.error.drm"),
            (
                json!({"_type":"video","availability":"needs_auth"}),
                "video.error.restricted",
            ),
            (
                json!({"_type":"video","formats":[{"format_id":"v","ext":"mp4","has_drm":true}]}),
                "video.error.drm",
            ),
        ] {
            let decoded = decode(&serde_json::to_vec(&raw).unwrap()).unwrap();
            assert_eq!(preview(page(), decoded, true).unwrap_err(), error);
        }
        assert_eq!(
            diagnostic("WARNING: try cookies\nERROR: [vimeo] 404123: Connection timed out"),
            "video.error.network"
        );
        assert_eq!(
            diagnostic("ERROR: Unsupported URL: https://example.org/article"),
            "video.error.unsupported"
        );
    }
    #[test]
    fn merge_partial_resolution_labels_with_measured_variants_without_collapsing_aspect_ratios() {
        let dims = [
            (0, 240),
            (426, 240),
            (320, 240),
            (426, 0),
            (0, 0),
            (0, 720),
            (720, 0),
        ];
        let items = dims
            .into_iter()
            .enumerate()
            .map(|(i, (w, h))| {
                json!({
                    "format_id": i.to_string(), "ext": "mp4", "width": w, "height": h
                })
            })
            .collect::<Vec<_>>();
        let video = preview(page(), data(json!(items)), false).unwrap();
        let kept = video
            .formats
            .iter()
            .map(|f| (f.width, f.height))
            .collect::<Vec<_>>();
        assert_eq!(kept, [(0, 720), (426, 240), (320, 240), (720, 0)]);
    }
    #[test]
    fn missing_media_url_does_not_create_a_default_stream_or_mask_a_dependency_error() {
        for url in [
            serde_json::Value::Null,
            json!(""),
            json!("not a URL"),
            json!("file:///etc/hosts"),
        ] {
            let raw = decode(
                &serde_json::to_vec(&json!({"formats":[],"format_id":"mp4","ext":"mp4","url":url}))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                preview(page(), raw, true).unwrap_err(),
                "video.error.formats"
            );
        }
        assert_eq!(diagnostic("ERROR: The extractor is attempting impersonation, but none of these impersonate targets are available: firefox."), "video.error.impersonation");
    }
    #[tokio::test]
    async fn rejects_oversized_probe_output_instead_of_deserializing_truncated_json() {
        assert_eq!(read_bounded(&b"{}"[..], 2).await.unwrap(), b"{}");
        assert_eq!(
            read_bounded(&b"{} "[..], 2).await.unwrap_err(),
            "video.error.metadataLimit"
        );
    }
    #[test]
    fn a_manual_embed_keeps_its_canonical_playback_page_but_never_an_expiring_cdn_url() {
        let article: url::Url = "https://public.example/article/123".parse().unwrap();
        let youtube = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
        let bili = "https://www.bilibili.com/video/BV1bK411W797?p=2";
        for (source, expected) in [
            (youtube, youtube),
            (bili, bili),
            ("https://cdn.example/video.mp4?expires=1", article.as_str()),
        ] {
            let mut raw = data(json!([{"format_id": "18", "ext":"mp4", "height":360}]));
            raw.webpage_url = Some(source.into());
            let found = preview(manual_page(&article).unwrap(), raw, true).unwrap();
            assert_eq!(found.page_url, expected);
        }
    }
}
