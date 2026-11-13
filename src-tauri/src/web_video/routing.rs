use super::sites::{classify, manual_page, page_candidate, playback_url, PlaybackPage};
use crate::{CandidateKind, ScannedInput};
use std::{collections::HashMap, future::Future};

/// Route webpages immediately after native file/share/protocol handling. Only
/// media-shaped URLs wait for offline matching; preserve order and other errors.
pub(crate) async fn scan(input: &str) -> Vec<ScannedInput> {
    scan_with_progress(input, |_, _| {}).await
}

pub(crate) async fn scan_with_progress(
    input: &str,
    on_progress: impl FnOnce(&[ScannedInput], &[String]),
) -> Vec<ScannedInput> {
    scan_with_classifier(input, on_progress, |pending| async move {
        let candidates = pending.iter().map(String::as_str).collect::<Vec<_>>();
        classify(&candidates).await
    })
    .await
}

async fn scan_with_classifier<F, Fut>(
    input: &str,
    on_progress: impl FnOnce(&[ScannedInput], &[String]),
    classify: F,
) -> Vec<ScannedInput>
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = HashMap<String, PlaybackPage>>,
{
    let mut entries = crate::scan_download_inputs(input);
    let mut pending = Vec::new();
    for entry in entries.iter_mut().filter(|entry| entry.kind == Some(CandidateKind::Direct)) {
        let Ok(url) = url::Url::parse(&entry.input) else { continue };
        if manual_page(&url).is_some() {
            entry.kind = Some(CandidateKind::WebVideo);
            // A failed webpage probe may still be explicitly downloaded as a file.
            entry.can_probe_video = true;
        } else if page_candidate(&url).is_some() {
            pending.push(entry.input.clone());
        }
    }
    // Publish the complete local scan before starting the offline extractor.
    // Only pending rows need to wait before choosing their final download route.
    on_progress(&entries, &pending);
    if pending.is_empty() {
        return entries;
    }
    let routes = classify(pending).await;
    for entry in &mut entries {
        if routes.contains_key(&entry.input) {
            entry.kind = Some(CandidateKind::WebVideo);
        }
    }
    entries
}

pub(crate) async fn resolve(input: &str, probe: bool) -> Result<Option<PlaybackPage>, String> {
    resolve_with_classifier(input, probe, |input| async move {
        classify(&[&input]).await.remove(&input)
    })
    .await
}

async fn resolve_with_classifier<F, Fut>(
    input: &str,
    probe: bool,
    classify: F,
) -> Result<Option<PlaybackPage>, String>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Option<PlaybackPage>>,
{
    // Automatic and explicit video preparation share the same local webpage
    // decision. The command handles an explicit ordinary-file choice upstream.
    if let Some(page) = url::Url::parse(input)
        .ok()
        .and_then(|url| playback_url(&url).or_else(|| manual_page(&url)))
    {
        return Ok(Some(page));
    }
    match classify(input.to_owned()).await {
        Some(page) => Ok(Some(page)),
        None if probe => Err("video.error.unsupported".into()),
        _ => Ok(None),
    }
}

#[cfg(test)]
pub(super) async fn scan_with_matcher_result_for_test(
    input: &str,
    on_progress: impl FnOnce(&[ScannedInput], &[String]),
    result: Result<HashMap<String, String>, String>,
) -> Vec<ScannedInput> {
    scan_with_classifier(input, on_progress, |pending| async move {
        let urls = pending.iter().map(String::as_str).collect::<Vec<_>>();
        super::sites::classify_with_matcher_result_for_test(&urls, result).await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn webpages_route_to_extraction_without_needing_the_matcher() {
        let urls = [
            "https://www.acfun.cn/v/ac35457073",
            "https://tv.cctv.com/2026/10/01/VIDEgXcuU2L3NYhZgJDaWj8A261001.shtml",
            "https://cdn.example.org/archive.zip",
        ];
        let scanned = scan_with_matcher_result_for_test(
            &urls.join("\n"),
            |_, _| {},
            Err("video.error.extract".into()),
        )
        .await;
        for (entry, expected) in scanned.iter().zip(urls) {
            assert_eq!(entry.input, expected);
            assert!(entry.valid);
        }
        assert_eq!(scanned[2].kind, Some(CandidateKind::Direct));
        assert!(!scanned[2].can_probe_video);
        for entry in &scanned[..2] {
            assert_eq!(entry.kind, Some(CandidateKind::WebVideo));
            assert!(entry.can_probe_video);
            let page = resolve_with_classifier(&entry.input, true, |_| async {
                panic!("an already requested probe must not repeat the failed offline matcher")
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(page.url.as_str(), entry.input);
        }
    }

    #[tokio::test]
    async fn automatic_and_explicit_webpage_probes_bypass_matching() {
        let input = "https://news.example.org/post.shtml";
        for probe in [false, true] {
            assert!(resolve_with_classifier(input, probe, |_| async {
                panic!("webpage preparation should call extraction directly")
            }).await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn payload_shaped_urls_still_require_a_dedicated_extractor_match() {
        let payload = "https://cdn.example.org/clip.mp4";
        assert!(resolve_with_classifier(payload, false, |_| async { None })
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            resolve_with_classifier(payload, true, |_| async { None }).await.unwrap_err(),
            "video.error.unsupported"
        );
        let playback = "https://commons.wikimedia.org/wiki/File:Example.webm";
        let page = resolve_with_classifier(playback, true, |input| async move {
            page_candidate(&input.parse().unwrap())
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(page.url.as_str(), playback);
    }

    #[tokio::test]
    async fn six_and_forty_webpage_rows_are_ready_for_extraction_in_the_first_snapshot() {
        for count in [6, 40] {
            let urls = (0..count).map(|i| match i % 3 {
                0 => format!("https://tv.cctv.com/2026/10/01/VIDEgXcuU2L3NYhZgJDaWj8A261001.shtml?sample={i}"),
                1 => format!("https://v.youku.com/v_show/id_XNDIzNjE2MTc4NA==.html?sample={i}"),
                _ => format!("https://www.bilibili.com/video/BV1bK411W797?p={}", i + 1),
            }).collect::<Vec<_>>();
            let announced = Cell::new(false);
            let final_entries = scan_with_classifier(&urls.join("\n"), |entries, pending| {
                assert_eq!(entries.len(), count);
                assert!(pending.is_empty());
                assert!(entries.iter().all(|entry| entry.valid && entry.kind == Some(CandidateKind::WebVideo)));
                for (index, entry) in entries.iter().enumerate() {
                    assert_eq!(entry.can_probe_video, index % 3 != 2);
                }
                announced.set(true);
            }, |_| async {
                panic!("ordinary webpage batches must not launch the offline matcher")
            }).await;
            assert!(announced.get());
            assert_eq!(final_entries.len(), count);
            assert!(final_entries.iter().all(|entry| entry.kind == Some(CandidateKind::WebVideo)));
        }
    }

    #[tokio::test]
    async fn webpages_are_released_while_media_shaped_urls_wait_for_matching() {
        let urls = [
            "https://www.acfun.cn/v/ac48980313",
            "https://commons.wikimedia.org/wiki/File:Example.mp4",
            "https://cdn.example.org/clip.mp4",
            "https://cdn.example.org/archive.zip",
            "http://127.0.0.1/payload",
            "https://pan.baidu.com/s/1example",
        ];
        let input = urls.join("\n");
        let announced = Cell::new(false);
        let matching = Cell::new(false);
        let release = Notify::new();
        let scan = scan_with_classifier(
            &input,
            |entries, pending| {
                assert_eq!(entries.len(), urls.len());
                assert!(entries.iter().all(|entry| entry.valid));
                assert_eq!(pending, &urls[1..3]);
                assert_eq!(entries[0].kind, Some(CandidateKind::WebVideo));
                assert!(entries[0].can_probe_video);
                assert!(entries[1..].iter().all(|entry| !entry.can_probe_video));
                assert!(!matching.get());
                announced.set(true);
            },
            |pending| {
                assert!(
                    announced.get(),
                    "matching started before the local list was published"
                );
                assert_eq!(pending, urls[1..3]);
                matching.set(true);
                async {
                    release.notified().await;
                    HashMap::from([(urls[1].to_owned(), page_candidate(&urls[1].parse().unwrap()).unwrap())])
                }
            },
        );
        tokio::pin!(scan);
        assert!(futures_util::poll!(&mut scan).is_pending());
        assert!(announced.get());
        assert!(matching.get());
        release.notify_one();
        let final_entries = scan.await;
        assert_eq!(final_entries.len(), urls.len());
        assert_eq!(final_entries[0].kind, Some(CandidateKind::WebVideo));
        assert_eq!(final_entries[1].kind, Some(CandidateKind::WebVideo));
        for entry in &final_entries[2..5] {
            assert_eq!(entry.kind, Some(CandidateKind::Direct));
        }
        assert_eq!(final_entries[5].kind, Some(CandidateKind::CloudShare));
    }

    #[tokio::test]
    async fn purely_local_routes_publish_without_calling_the_offline_classifier() {
        let input = "https://youtu.be/62xlzGs8LXA\nhttps://cdn.example.org/archive.zip";
        let announced = Cell::new(false);
        let result = scan_with_progress(input, |entries, pending| {
            assert_eq!(entries.len(), 2);
            assert!(pending.is_empty());
            announced.set(true);
        })
        .await;
        assert!(announced.get());
        assert_eq!(result[0].kind, Some(CandidateKind::WebVideo));
        assert_eq!(result[1].kind, Some(CandidateKind::Direct));
    }
}
