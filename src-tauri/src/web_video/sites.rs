//! Recognize first-party short links locally and delegate other site rules to yt-dlp.
mod matcher;

use std::collections::HashMap;
use url::Url;

#[derive(Clone, Debug)]
pub(crate) struct PlaybackPage {
    pub site: String,
    pub url: Url,
    inspect_embeds: bool,
}

impl PlaybackPage {
    /// Keep explicit playback URLs (including Bilibili's part number). Only
    /// short links and manually inspected webpages can resolve to another page.
    pub(super) fn resolve(self, reported: Option<&str>) -> Option<Self> {
        let is_short = self.url.host_str() == Some("b23.tv");
        if !is_short && !self.inspect_embeds {
            return Some(self);
        }
        let resolved = reported
            .and_then(|s| Url::parse(s).ok())
            .as_ref()
            .and_then(playback_url)
            .filter(|p| p.url.host_str() != Some("b23.tv"));
        if is_short {
            resolved.filter(|p| p.site == self.site)
        } else {
            resolved.or(Some(self))
        }
    }
}

type SiteMatcher = fn(&Url) -> Option<Url>;
const FAST_PATHS: &[(&str, SiteMatcher)] = &[("YouTube", youtube), ("Bilibili", bilibili)];

pub(crate) fn playback_url(url: &Url) -> Option<PlaybackPage> {
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    FAST_PATHS.iter().find_map(|(site, matcher)| {
        matcher(url).map(|url| PlaybackPage {
            site: (*site).into(),
            url,
            inspect_embeds: false,
        })
    })
}

/// URLs that can enter explicit webpage extraction. Protocol handlers, cloud share
/// hosts, local addresses, and known payloads belong to the regular download path.
pub(crate) fn manual_page(url: &Url) -> Option<PlaybackPage> {
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || crate::parser::registered_share_provider(url).is_some()
    {
        return None;
    }
    // URLs with IP literals, single-label names, and local DNS suffixes are
    // common NAS/intranet downloads. This is a routing decision without DNS I/O.
    let host = url.domain()?;
    if !host.contains('.')
        || host.ends_with('.')
        || host == "home.arpa"
        || [
            ".localhost",
            ".local",
            ".localdomain",
            ".internal",
            ".lan",
            ".home",
            ".home.arpa",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return None;
    }
    let filename = url.path_segments()?.next_back().unwrap_or_default();
    let filename = percent_encoding::percent_decode_str(filename).decode_utf8_lossy();
    if filename
        .rsplit_once('.')
        .is_some_and(|(_, ext)| is_payload_extension(ext))
    {
        return None;
    }
    Some(PlaybackPage {
        site: host.trim_start_matches("www.").to_owned(),
        url: url.clone(),
        inspect_embeds: true,
    })
}

fn is_payload_extension(ext: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "m3u", "m3u8", "mpd", "torrent", "metalink", "meta4", "zip", "7z", "rar", "tar", "gz",
        "bz2", "xz", "zst", "iso", "bin", "exe", "msi", "msix", "dmg", "pkg", "deb", "rpm",
        "appimage", "apk", "mp4", "m4v", "webm", "mkv", "flv", "mov", "avi", "ts", "m2ts", "wmv",
        "mpeg", "mpg", "mp3", "m4a", "ogg", "flac", "wav", "opus", "aac", "pdf", "doc", "docx",
        "xls", "xlsx", "ppt", "pptx", "epub", "txt", "png", "gif", "jpg", "jpeg", "webp", "svg",
        "avif", "heic", "csv", "xml", "json", "css", "js", "vtt", "srt",
    ];
    EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

pub(super) enum PageRoute {
    Automatic(PlaybackPage),
    OnRequest(PlaybackPage),
}

/// URL matching is offline. If the tool is missing, ordinary downloads are
/// still available and an explicit video probe can report the tool error.
pub(super) async fn classify(inputs: &[&str]) -> HashMap<String, PageRoute> {
    let mut routes = HashMap::new();
    let mut candidates = Vec::new();
    for &input in inputs {
        let Ok(url) = Url::parse(input) else { continue };
        if let Some(page) = playback_url(&url) {
            routes.insert(input.to_owned(), PageRoute::Automatic(page));
        } else if let Some(page) = manual_page(&url) {
            candidates.push((input, page));
        }
    }
    if candidates.is_empty() {
        return routes;
    }
    let urls: Vec<_> = candidates.iter().map(|(_, p)| p.url.as_str()).collect();
    let matches = matcher::dedicated_sites(&urls).await.unwrap_or_default();
    for (input, mut page) in candidates {
        let route = match matches.get(page.url.as_str()) {
            Some(site) => {
                page.site.clone_from(site);
                page.inspect_embeds = false;
                PageRoute::Automatic(page)
            }
            None => PageRoute::OnRequest(page),
        };
        routes.insert(input.to_owned(), route);
    }
    routes
}

fn youtube(url: &Url) -> Option<Url> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let id = match (url.host_str()?, parts.as_slice()) {
        ("youtu.be", [id]) => id.to_string(),
        ("youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com", ["watch"]) => {
            url.query_pairs()
                .find(|(key, _)| key == "v")?
                .1
                .into_owned()
        }
        (
            "youtube.com" | "www.youtube.com" | "m.youtube.com",
            ["shorts" | "embed" | "live", id],
        ) => id.to_string(),
        _ => return None,
    };
    if id.len() != 11
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return None;
    }
    Url::parse(&format!("https://www.youtube.com/watch?v={id}")).ok()
}

fn bilibili(url: &Url) -> Option<Url> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    match (url.host_str()?, parts.as_slice()) {
        ("b23.tv", [id]) if !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric()) => {
            Url::parse(&format!("https://b23.tv/{id}")).ok()
        }
        ("bilibili.com" | "www.bilibili.com" | "m.bilibili.com", ["video", id])
            if (id.len() == 12
                && id.starts_with("BV")
                && id.bytes().all(|c| c.is_ascii_alphanumeric()))
                || id
                    .strip_prefix("av")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())) =>
        {
            let mut page = Url::parse(&format!("https://www.bilibili.com/video/{id}")).ok()?;
            if let Some(part) = url
                .query_pairs()
                .find(|(k, _)| k == "p")
                .and_then(|(_, n)| n.parse::<u32>().ok())
                .filter(|n| *n > 1)
            {
                page.query_pairs_mut().append_pair("p", &part.to_string());
            }
            Some(page)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webpage_scan_and_manual_probe_preserve_other_download_handlers() {
        for direct in [
            "ftp://host.example/a",
            "file:///tmp/video.mp4",
            "http://127.0.0.1/video",
            "http://[::1]/movie",
            "http://192.168.1.1/file",
            "http://nas/share",
            "http://disk.home.arpa/share",
            "http://nas.local/media",
            "http://localhost./video",
            "https://user:password@video.example/watch/1",
            "https://video.example:9443/test",
            "https://pan.baidu.com/s/1example",
            "https://pan.baidu.com./s/1example",
            "https://pan.quark.cn/s/abc",
            "https://example.com/latest.AppImage?download=1",
            "https://example.org/content%2EZIP?name=web",
            "https://cdn.example.org/clip.mp4",
            "https://cdn.example.org/live.M3U8?sig=123",
            "https://cdn.example.org/live.mpd",
        ] {
            assert!(
                manual_page(&Url::parse(direct).unwrap()).is_none(),
                "{direct}"
            );
        }
        let url = Url::parse("https://news.example.org/post.html?page=2#video").unwrap();
        let page = manual_page(&url).unwrap();
        assert_eq!(page.url, url);
        assert_eq!(page.site, "news.example.org");
    }

    #[tokio::test]
    #[ignore = "exercises the installed yt-dlp executable, without network"]
    async fn installed_extractor_classifies_a_mixed_batch_offline() {
        let urls = [
            "https://youtu.be/jNQXAC9IVRw?t=3",
            "https://www.bilibili.com/video/BV1bK411W797?p=2",
            "https://vimeo.com/76979871",
            "https://www.dailymotion.com/video/x5kesuj",
            "https://www.acfun.cn/v/ac35457073",
            "https://example.org/article/1",
            "https://example.org/f.zip",
        ];
        let routes = classify(&urls).await;
        for (index, expected) in [
            (0, "YouTube"),
            (1, "Bilibili"),
            (2, "vimeo"),
            (3, "dailymotion"),
            (4, "AcFunVideo"),
        ] {
            let PageRoute::Automatic(page) = &routes[urls[index]] else {
                panic!("missed {expected}")
            };
            assert_eq!(page.site, expected);
            if index == 1 {
                assert_eq!(page.url.query(), Some("p=2"));
            }
        }
        assert!(matches!(routes.get(urls[5]), Some(PageRoute::OnRequest(_))));
        assert!(!routes.contains_key(urls[6]));
    }
}
