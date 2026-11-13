//! Offline URL matching by the installed yt-dlp, including standalone binaries.
//! --list-extractors groups matching input URLs on indented lines under extractor
//! names. This format is covered by a contract test against the local executable.
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
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

type Executable = (PathBuf, u64, Option<SystemTime>);
type Matches = HashMap<String, String>;

#[derive(Default)]
struct Cache {
    executable: Option<Executable>,
    entries: HashMap<[u8; 32], (Instant, Option<String>)>,
}

impl Cache {
    fn lookup<'a>(
        &mut self,
        executable: &Executable,
        urls: &[&'a str],
    ) -> (Matches, Vec<([u8; 32], &'a str)>) {
        if self.executable.as_ref() != Some(executable) {
            self.entries.clear();
            self.executable = Some(executable.clone());
        }
        let now = Instant::now();
        self.entries.retain(|_, (expiry, _)| *expiry > now);
        let mut hits = HashMap::new();
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        for &url in urls {
            if !seen.insert(url) {
                continue;
            }
            // Cache keys contain no raw query strings or account data.
            let key: [u8; 32] = Sha256::digest(url.as_bytes()).into();
            match self.entries.get(&key) {
                Some((_, Some(site))) => {
                    hits.insert(url.to_owned(), site.clone());
                }
                Some((_, None)) => (),
                None => pending.push((key, url)),
            }
        }
        (hits, pending)
    }
}

#[derive(Default)]
struct Matcher {
    cache: Mutex<Cache>,
    misses: Mutex<()>,
}
static MATCHER: LazyLock<Matcher> = LazyLock::new(Matcher::default);

impl Matcher {
    async fn resolve<F, Fut>(
        &self,
        executable: Executable,
        urls: &[&str],
        mut probe: F,
    ) -> Result<Matches, String>
    where
        F: FnMut(Vec<String>) -> Fut,
        Fut: Future<Output = Result<Matches, String>>,
    {
        let (hits, pending) = self.cache.lock().await.lookup(&executable, urls);
        if pending.is_empty() {
            return Ok(hits);
        }

        // Only misses wait for an offline process. Cached routes must remain
        // available while another batch is starting or timing out.
        let _miss = self.misses.lock().await;
        // A preceding caller may have filled these entries while we waited.
        let (mut hits, pending) = self.cache.lock().await.lookup(&executable, urls);
        for chunk in pending.chunks(BATCH_SIZE) {
            let inputs = chunk.iter().map(|(_, url)| (*url).to_owned()).collect();
            let mut batch = probe(inputs).await?;
            let mut cache = self.cache.lock().await;
            // A tool update can invalidate the cache during the probe. Its old
            // result may finish this request but must not enter the new cache.
            let current = cache.executable.as_ref() == Some(&executable);
            if current && cache.entries.len() + chunk.len() > CACHE_SIZE {
                cache.entries.clear();
            }
            let expiry = Instant::now() + CACHE_TTL;
            for &(key, url) in chunk {
                let site = batch.remove(url);
                if let Some(name) = &site {
                    hits.insert(url.to_owned(), name.clone());
                }
                if current {
                    cache.entries.insert(key, (expiry, site));
                }
            }
        }
        Ok(hits)
    }
}

pub(super) async fn dedicated_sites(urls: &[&str]) -> Result<Matches, String> {
    let tool = super::super::extractor::tool("yt-dlp").ok_or("video.error.extractor")?;
    let file = std::fs::metadata(&tool).map_err(|_| "video.error.extractor")?;
    let executable = (tool, file.len(), file.modified().ok());
    MATCHER
        .resolve(executable, urls, |inputs| async move {
            let urls = inputs.iter().map(String::as_str).collect::<Vec<_>>();
            probe(&urls).await
        })
        .await
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
    use std::task::Poll;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn cached_positive_and_negative_routes_do_not_wait_for_a_cold_probe() {
        let matcher = Matcher::default();
        let executable = (PathBuf::from("test-yt-dlp"), 1, None);
        let warm = ["https://video.example/known", "https://plain.example/page"];
        let expected = HashMap::from([(warm[0].to_owned(), "ExampleVideo".to_owned())]);
        matcher
            .resolve(executable.clone(), &warm, |_| async { Ok(expected.clone()) })
            .await
            .unwrap();

        let entered = Notify::new();
        let release = Notify::new();
        let cold_urls = ["https://new.example/clip"];
        let cold = matcher.resolve(executable.clone(), &cold_urls, |_| async {
            entered.notify_one();
            release.notified().await;
            Ok(HashMap::new())
        });
        let cached = async {
            entered.notified().await;
            let hit = matcher.resolve(executable, &warm, |_| async {
                panic!("positive and negative cached routes must not launch another probe")
            });
            tokio::pin!(hit);
            let polled = futures_util::poll!(&mut hit);
            release.notify_one();
            let Poll::Ready(result) = polled else {
                panic!("a cache hit waited behind an unrelated offline probe");
            };
            assert_eq!(result.unwrap(), expected);
        };
        let (cold, ()) = tokio::join!(cold, cached);
        assert!(cold.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_misses_recheck_the_cache_before_starting_another_probe() {
        let matcher = Matcher::default();
        let executable = (PathBuf::from("test-yt-dlp"), 1, None);
        let urls = ["https://video.example/clip"];
        let expected = HashMap::from([(urls[0].to_owned(), "ExampleVideo".to_owned())]);
        let entered = Notify::new();
        let release = Notify::new();
        let first = matcher.resolve(executable.clone(), &urls, |_| async {
            entered.notify_one();
            release.notified().await;
            Ok(expected.clone())
        });
        let second = async {
            entered.notified().await;
            let queued = matcher.resolve(executable, &urls, |_| async {
                panic!("the preceding caller already probed this URL")
            });
            tokio::pin!(queued);
            assert!(futures_util::poll!(&mut queued).is_pending());
            release.notify_one();
            queued.await
        };
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap(), expected);
        assert_eq!(second.unwrap(), expected);
    }

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
