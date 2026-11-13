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

/// Candidates for dedicated extractor matching. A playback page may itself end
/// in a video extension; known non-video payloads still belong to ordinary downloads.
pub(crate) fn page_candidate(url: &Url) -> Option<PlaybackPage> {
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
    if payload_extension(url).is_some_and(|ext| !is_video_extension(&ext)) {
        return None;
    }
    Some(PlaybackPage {
        site: host.trim_start_matches("www.").to_owned(),
        url: url.clone(),
        inspect_embeds: true,
    })
}

/// URLs that can enter explicit generic webpage extraction. Known payloads
/// remain ordinary downloads unless a dedicated extractor recognizes them.
pub(crate) fn manual_page(url: &Url) -> Option<PlaybackPage> {
    let page = page_candidate(url)?;
    if payload_extension(url).is_some() {
        return None;
    }
    Some(page)
}

fn payload_extension(url: &Url) -> Option<String> {
    let filename = url.path_segments()?.next_back().unwrap_or_default();
    let filename = percent_encoding::percent_decode_str(filename).decode_utf8_lossy();
    let (_, ext) = filename.rsplit_once('.')?;
    is_payload_extension(ext).then(|| ext.to_ascii_lowercase())
}

fn is_video_extension(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "m4v" | "webm" | "mkv" | "flv" | "mov" | "avi" | "ts" | "m2ts" | "wmv"
            | "mpeg" | "mpg"
    )
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

fn candidate_route(mut page: PlaybackPage, dedicated_site: Option<&str>) -> Option<PageRoute> {
    if let Some(site) = dedicated_site {
        page.site = site.to_owned();
        page.inspect_embeds = false;
        Some(PageRoute::Automatic(page))
    } else {
        manual_page(&page.url).map(PageRoute::OnRequest)
    }
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
        } else if let Some(page) = page_candidate(&url) {
            candidates.push((input, page));
        }
    }
    if candidates.is_empty() {
        return routes;
    }
    let urls: Vec<_> = candidates.iter().map(|(_, p)| p.url.as_str()).collect();
    let matches = matcher::dedicated_sites(&urls).await.unwrap_or_default();
    for (input, page) in candidates {
        let dedicated_site = matches.get(page.url.as_str()).map(String::as_str);
        if let Some(route) = candidate_route(page, dedicated_site) {
            routes.insert(input.to_owned(), route);
        }
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
        for suffix in ["html", "shtml"] {
            let url = Url::parse(&format!("https://news.example.org/post.{suffix}?page=2#video"))
                .unwrap();
            let page = manual_page(&url).unwrap();
            assert_eq!(page.url, url);
            assert_eq!(page.site, "news.example.org");
        }
    }

    #[test]
    fn dedicated_extractors_take_precedence_over_payload_suffixes() {
        let page_url = Url::parse("https://commons.wikimedia.org/wiki/File:Die_Temperaturkurve_der_Erde_(ZDF,_Terra_X)_720p_HD_50FPS.webm").unwrap();
        let candidate = page_candidate(&page_url).unwrap();
        assert!(manual_page(&page_url).is_none());
        let Some(PageRoute::Automatic(page)) =
            candidate_route(candidate, Some("wikimedia.org"))
        else {
            panic!("a recognized playback page must not be treated as a media payload");
        };
        assert_eq!(page.url, page_url);
        assert_eq!(page.site, "wikimedia.org");
        assert!(!page.inspect_embeds);

        for direct in [
            "https://cdn.example.org/clip.mp4",
            "https://cdn.example.org/clip.webm",
        ] {
            let candidate = page_candidate(&Url::parse(direct).unwrap()).unwrap();
            assert!(candidate_route(candidate, None).is_none(), "{direct}");
        }

        // Dedicated extractors may match arbitrary files on their host, so a
        // match alone must not turn documents, archives, or images into videos.
        for direct in [
            "https://example.org/f.zip",
            "https://example.org/content%2EZIP?name=web",
            "https://cdn.example.org/live.M3U8?sig=123",
            "https://cdn.example.org/live.mpd",
            "https://www.dropbox.com/s/exampleid/report.zip?dl=1",
            "https://www.dropbox.com/scl/fi/exampleid/report.pdf?dl=1&rlkey=examplekey",
            "https://commons.wikimedia.org/wiki/File:Example.jpg",
        ] {
            assert!(
                page_candidate(&Url::parse(direct).unwrap()).is_none(),
                "{direct}"
            );
        }

        let candidate = page_candidate(&Url::parse("https://news.example.org/post.shtml").unwrap())
            .unwrap();
        assert!(matches!(
            candidate_route(candidate, None),
            Some(PageRoute::OnRequest(_))
        ));
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
            "https://tv.cctv.com/2026/10/01/VIDEgXcuU2L3NYhZgJDaWj8A261001.shtml",
            "https://www.bilibili.com/video/BV1ps411o7yC/",
            "https://www.acfun.cn/v/ac48980313",
            "https://v.youku.com/v_show/id_XNTkxMjQzNDQzNg==.html",
            "https://youtu.be/62xlzGs8LXA?si=LHdIh5rMIAdNE3Dz",
            "https://commons.wikimedia.org/wiki/File:Die_Temperaturkurve_der_Erde_(ZDF,_Terra_X)_720p_HD_50FPS.webm",
            "https://cdn.example.org/clip.mp4",
            "https://www.dropbox.com/s/exampleid/report.zip?dl=1",
            "https://www.dropbox.com/scl/fi/exampleid/report.pdf?dl=1&rlkey=examplekey",
            "https://commons.wikimedia.org/wiki/File:Example.jpg",
        ];
        let routes = classify(&urls).await;
        for (index, expected) in [
            (0, "YouTube"),
            (1, "Bilibili"),
            (2, "vimeo"),
            (3, "dailymotion"),
            (4, "AcFunVideo"),
            (7, "CCTV"),
            (8, "Bilibili"),
            (9, "AcFunVideo"),
            (10, "youku"),
            (11, "YouTube"),
            (12, "wikimedia.org"),
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
        for index in [6, 13, 14, 15, 16] {
            assert!(!routes.contains_key(urls[index]), "{}", urls[index]);
        }
    }
}
