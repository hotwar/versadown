use super::sites::{classify, PageRoute, PlaybackPage};
use crate::{CandidateKind, ScannedInput};

/// Only HTTP links left over from the native file/share/protocol handlers need
/// an offline extractor match. Preserve the scanner's order and other errors.
pub(crate) async fn scan(input: &str) -> Vec<ScannedInput> {
    let mut entries = crate::scan_download_inputs(input);
    let candidates: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == Some(CandidateKind::Direct))
        .map(|e| e.input.as_str())
        .collect();
    let routes = classify(&candidates).await;
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
