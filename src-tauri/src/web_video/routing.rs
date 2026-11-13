use super::sites::{classify, page_candidate, playback_url, PageRoute, PlaybackPage};
use crate::{CandidateKind, ScannedInput};
use std::{collections::HashMap, future::Future};

/// Only HTTP links left over from the native file/share/protocol handlers need
/// an offline extractor match. Preserve the scanner's order and other errors.
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
    Fut: Future<Output = HashMap<String, PageRoute>>,
{
    let mut entries = crate::scan_download_inputs(input);
    let pending: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == Some(CandidateKind::Direct))
        .filter(|e| {
            url::Url::parse(&e.input).is_ok_and(|url| {
                playback_url(&url).is_none() && page_candidate(&url).is_some()
            })
        })
        .map(|e| e.input.clone())
        .collect();
    // Publish the complete local scan before starting the offline extractor.
    // Only pending rows need to wait before choosing their final download route.
    on_progress(&entries, &pending);
    if pending.is_empty() {
        return entries;
    }
    let routes = classify(pending).await;
    for entry in &mut entries {
        match routes.get(&entry.input) {
            Some(PageRoute::Automatic(_)) => entry.kind = Some(CandidateKind::WebVideo),
            Some(PageRoute::OnRequest(_)) => entry.can_probe_video = true,
            None => (),
        }
    }
    entries
}

pub(crate) async fn resolve(input: &str, probe: bool) -> Result<Option<PlaybackPage>, String> {
    match classify(&[input]).await.remove(input) {
        Some(PageRoute::Automatic(page)) => Ok(Some(page)),
        Some(PageRoute::OnRequest(page)) if probe => Ok(Some(page)),
        None if probe => Err("video.error.unsupported".into()),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn forty_rows_are_published_before_offline_matching_can_finish() {
        let input = (0..8)
            .flat_map(|i| {
                [
                    format!("https://tv.cctv.com/2026/10/01/VIDEgXcuU2L3NYhZgJDaWj8A261001.shtml?sample={i}"),
                    format!("https://v.youku.com/v_show/id_XNDIzNjE2MTc4NA==.html?sample={i}"),
                    format!("https://www.bilibili.com/video/BV1bK411W797?p={}", i + 1),
                    format!("https://youtu.be/62xlzGs8LX{i}"),
                    format!("https://cdn.example.org/archive-{i}.zip"),
                ]
            })
            .collect::<Vec<_>>()
            .join("\n");
        let announced = Cell::new(false);
        let matching = Cell::new(false);
        let release = Notify::new();
        let scan = scan_with_classifier(
            &input,
            |entries, pending| {
                assert_eq!(entries.len(), 40);
                assert!(entries.iter().all(|entry| entry.valid));
                assert_eq!(pending.len(), 16);
                assert_eq!(
                    entries
                        .iter()
                        .filter(|entry| entry.kind == Some(CandidateKind::WebVideo))
                        .count(),
                    16
                );
                for (index, entry) in entries.iter().enumerate() {
                    assert_eq!(pending.contains(&entry.input), matches!(index % 5, 0 | 1));
                }
                assert!(!matching.get());
                announced.set(true);
            },
            |pending| {
                assert!(
                    announced.get(),
                    "matching started before the local list was published"
                );
                matching.set(true);
                async {
                    release.notified().await;
                    pending
                        .into_iter()
                        .map(|input| {
                            let page = super::super::sites::manual_page(&input.parse().unwrap())
                                .unwrap();
                            (input, PageRoute::Automatic(page))
                        })
                        .collect()
                }
            },
        );
        tokio::pin!(scan);
        assert!(futures_util::poll!(&mut scan).is_pending());
        assert!(announced.get());
        assert!(matching.get());
        release.notify_one();
        let final_entries = scan.await;
        assert_eq!(final_entries.len(), 40);
        for (index, entry) in final_entries.iter().enumerate() {
            assert_eq!(
                entry.kind,
                Some(if index % 5 == 4 {
                    CandidateKind::Direct
                } else {
                    CandidateKind::WebVideo
                })
            );
            assert!(!entry.can_probe_video);
        }
    }

    #[tokio::test]
    async fn progress_does_not_turn_payloads_or_unknown_pages_into_automatic_video_routes() {
        let urls = [
            "https://cdn.example.org/fixture.mp4",
            "https://news.example.org/article",
            "https://cdn.example.org/archive.zip",
            "http://127.0.0.1/payload",
        ];
        let entries = scan_with_classifier(
            &urls.join("\n"),
            |entries, pending| {
                assert_eq!(entries.len(), 4);
                assert_eq!(pending, &urls[..2]);
                assert!(entries.iter().all(|entry| !entry.can_probe_video));
            },
            |_| async {
                let page = super::super::sites::manual_page(&urls[1].parse().unwrap()).unwrap();
                HashMap::from([(urls[1].to_owned(), PageRoute::OnRequest(page))])
            },
        )
        .await;
        assert!(entries
            .iter()
            .all(|entry| entry.kind == Some(CandidateKind::Direct)));
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.can_probe_video)
                .collect::<Vec<_>>(),
            [false, true, false, false]
        );
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
