//! A confirmation refers only to formats actually offered by the local extractor.
//! Keep choices available across concurrent submits and retries of one quality.
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};
use uuid::Uuid;

use super::extractor::{VideoFormat, VideoPreview};

const MAX_OPEN_PREVIEWS: usize = 1024;
const BROWSER_LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub(super) struct Previews(Mutex<VecDeque<(Instant, VideoPreview)>>);

pub(super) struct SelectedVideo {
    pub media_id: String,
    pub page: String,
    pub title: String,
    pub format: VideoFormat,
    pub alternatives: Vec<String>,
}

impl Previews {
    pub fn remember(&self, preview: &VideoPreview) -> Result<(), String> {
        let mut items = self.0.lock().map_err(|_| "video.error.state")?;
        if items.len() >= MAX_OPEN_PREVIEWS {
            items.pop_front();
        }
        items.push_back((Instant::now(), preview.clone()));
        Ok(())
    }

    pub fn select(&self, preview_id: Uuid, format_id: &str) -> Result<SelectedVideo, String> {
        self.select_with_limit(preview_id, format_id, None)
    }

    pub fn select_recent(
        &self,
        preview_id: Uuid,
        format_id: &str,
    ) -> Result<SelectedVideo, String> {
        self.select_with_limit(preview_id, format_id, Some(BROWSER_LIFETIME))
    }

    fn select_with_limit(
        &self,
        preview_id: Uuid,
        format_id: &str,
        lifetime: Option<Duration>,
    ) -> Result<SelectedVideo, String> {
        let items = self.0.lock().map_err(|_| "video.error.state")?;
        let preview = items
            .iter()
            .find(|(inserted, p)| {
                p.preview_id == preview_id
                    && lifetime.is_none_or(|limit| inserted.elapsed() < limit)
            })
            .map(|(_, preview)| preview)
            .ok_or("video.error.previewExpired")?;
        let format = preview
            .formats
            .iter()
            .find(|f| f.id == format_id)
            .ok_or("video.error.formatGone")?;
        Ok(SelectedVideo {
            media_id: preview.media_id.clone(),
            page: preview.page_url.clone(),
            title: preview.title.clone(),
            format: format.clone(),
            alternatives: if lifetime.is_some() {
                preview
                    .alternatives
                    .get(format_id)
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        })
    }

    pub fn discard(&self, id: Uuid) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "video.error.state")?
            .retain(|(_, preview)| preview.preview_id != id);
        Ok(())
    }
}
