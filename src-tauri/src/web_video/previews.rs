//! A confirmation refers only to formats actually offered by the local extractor.
//! Keep choices available across concurrent submits and retries of one quality.
use std::{collections::VecDeque, sync::Mutex};
use uuid::Uuid;

use super::extractor::{VideoFormat, VideoPreview};

const MAX_OPEN_PREVIEWS: usize = 1024;

#[derive(Default)]
pub(super) struct Previews(Mutex<VecDeque<VideoPreview>>);

pub(super) struct SelectedVideo {
    pub media_id: String,
    pub page: String,
    pub title: String,
    pub format: VideoFormat,
}

impl Previews {
    pub fn remember(&self, preview: &VideoPreview) -> Result<(), String> {
        let mut items = self.0.lock().map_err(|_| "video.error.state")?;
        if items.len() >= MAX_OPEN_PREVIEWS {
            items.pop_front();
        }
        items.push_back(preview.clone());
        Ok(())
    }

    pub fn select(&self, preview_id: Uuid, format_id: &str) -> Result<SelectedVideo, String> {
        let items = self.0.lock().map_err(|_| "video.error.state")?;
        let preview = items
            .iter()
            .find(|p| p.preview_id == preview_id)
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
        })
    }

    pub fn discard(&self, id: Uuid) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "video.error.state")?
            .retain(|preview| preview.preview_id != id);
        Ok(())
    }
}
