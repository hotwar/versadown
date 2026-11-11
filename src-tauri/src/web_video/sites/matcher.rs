//! Offline URL matching by the installed yt-dlp, including standalone binaries.
//! --list-extractors groups matching input URLs on indented lines under extractor
//! names. This format is covered by a contract test against the local executable.
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Stdio,
    sync::LazyLock,
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

const BATCH_SIZE: usize = 64;
const CACHE_SIZE: usize = 512;
const CACHE_TTL: Duration = Duration::from_secs(120);
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const MATCH_ERROR: &str = "video.error.extract";

#[derive(Default)]
struct Cache {
    executable: Option<(PathBuf, u64, Option<SystemTime>)>,
    entries: HashMap<[u8; 32], (Instant, Option<String>)>,
}
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

pub(super) async fn dedicated_sites(urls: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut hits = HashMap::new();
    let mut pending = Vec::new();
    // Serialize misses so simultaneous previews don't launch one 1,700-rule
    // process per URL. Cache keys contain no raw query strings or account data.
    let tool = super::super::extractor::tool("yt-dlp").ok_or("video.error.extractor")?;
    let file = std::fs::metadata(&tool).map_err(|_| "video.error.extractor")?;
    let executable = (tool, file.len(), file.modified().ok());
    let mut cache = CACHE.lock().await;
    if cache.executable.as_ref() != Some(&executable) {
        cache.entries.clear();
        cache.executable = Some(executable);
    }
    let now = Instant::now();
    cache.entries.retain(|_, (expiry, _)| *expiry > now);
    let mut seen = HashSet::new();
    for &url in urls {
        if !seen.insert(url) {
            continue;
        }
        let key: [u8; 32] = Sha256::digest(url.as_bytes()).into();
        match cache.entries.get(&key) {
            Some((_, Some(site))) => {
                hits.insert(url.to_owned(), site.clone());
            }
            Some((_, None)) => (),
            None => pending.push((key, url)),
        }
    }
    for chunk in pending.chunks(BATCH_SIZE) {
        let inputs = chunk.iter().map(|(_, url)| *url).collect::<Vec<_>>();
        let mut batch = probe(&inputs).await?;
        if cache.entries.len() + chunk.len() > CACHE_SIZE {
            cache.entries.clear();
        }
        let expiry = Instant::now() + CACHE_TTL;
        for &(key, url) in chunk {
            let site = batch.remove(url);
            if let Some(name) = &site {
                hits.insert(url.to_owned(), name.clone());
            }
            cache.entries.insert(key, (expiry, site));
        }
    }
    Ok(hits)
}

async fn probe(urls: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut cmd = super::super::extractor::base_command()?;
    cmd.args([
        "--encoding",
        "utf-8",
        "--list-extractors",
        "--batch-file",
        "-",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|_| MATCH_ERROR)?;
    let mut stdin = child.stdin.take().ok_or(MATCH_ERROR)?;
    let stdout = child.stdout.take().ok_or(MATCH_ERROR)?;
    let input = urls.join("\n");
    let mut output = Vec::new();
    let read_and_wait = async {
        let write = async move { stdin.write_all(input.as_bytes()).await };
        let read = async {
            stdout
                .take(OUTPUT_LIMIT as u64 + 1)
                .read_to_end(&mut output)
                .await
        };
        tokio::try_join!(write, read, child.wait())
    };
    let (_, length, status) = tokio::time::timeout(Duration::from_secs(10), read_and_wait)
        .await
        .map_err(|_| MATCH_ERROR)?
        .map_err(|_| MATCH_ERROR)?;
    if !status.success() || length > OUTPUT_LIMIT {
        return Err(MATCH_ERROR.into());
    }
    let text = std::str::from_utf8(&output).map_err(|_| MATCH_ERROR)?;
    parse_matches(text, urls)
}

fn parse_matches(text: &str, urls: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut missing: HashSet<_> = urls.iter().copied().collect();
    let mut hits = HashMap::new();
    let mut extractor = None;
    for line in text.lines() {
        if let Some(url) = line.strip_prefix("  ") {
            missing.remove(url);
            if let Some(name) = extractor {
                hits.entry(url.to_owned())
                    .or_insert_with(|| String::from(name));
            }
        } else {
            // These are catch-alls or diagnostic-only extractors. Depending
            // on the installed version a URL may appear under multiple names.
            extractor = (!line.is_empty()
                && !line.ends_with(" (CURRENTLY BROKEN)")
                && !matches!(
                    line,
                    "generic"
                        | "generic:quoted-html"
                        | "DRM"
                        | "Piracy"
                        | "Liability"
                        | "CommonMistakes"
                        | "UnicodeBOM"
                        | "youtube:truncated_id"
                        | "youtube:truncated_url"
                ))
            .then_some(line);
        }
    }
    if !missing.is_empty() {
        return Err(MATCH_ERROR.into());
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_and_diagnostic_matches_do_not_consume_regular_downloads() {
        let video = "https://video.example/movie";
        let unsupported = "https://shop.example/movie";
        let plain = "https://sample.example/file";
        let output = format!("generic\n  {video}\n  {plain}\nDRM\n  {unsupported}\nacme (CURRENTLY BROKEN)\n  {plain}\nAcmeVideo\n  {video}\n");
        let hits = parse_matches(&output, &[video, unsupported, plain]).unwrap();
        assert_eq!(
            hits,
            HashMap::from([(video.to_owned(), "AcmeVideo".to_owned())])
        );
        assert!(parse_matches(&output, &["https://missing.example/test"]).is_err());
    }
}
