mod aria2_checkpoint;
mod aria2_transfer;
mod cookies;
mod extractor;
mod previews;
mod proxy;
pub(crate) mod routing;
mod sites;
mod transfer;

use crate::commands::{self, AppState};
use crate::{DownloadTask, NewTask, TaskStatus, TaskStore};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tauri::State;
use tokio::sync::{oneshot, Mutex, Semaphore};
use uuid::Uuid;

pub(crate) use extractor::{set_resource_dir, VideoPreview};
use sites::PlaybackPage;
pub(crate) use sites::{page_candidate, playback_url};

type Listener = Arc<dyn Fn(DownloadTask) + Send + Sync>;
type Job = (oneshot::Sender<()>, tauri::async_runtime::JoinHandle<()>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoTransferEngine {
    Native,
    Aria2,
}

#[derive(Default)]
pub(crate) struct VideoOptions {
    pub proxy: Option<String>,
    pub rate: u64,
    pub cookie_file: Option<PathBuf>,
}

pub(crate) struct VideoDownloads {
    store: Arc<TaskStore>,
    emit: Listener,
    slots: Arc<Semaphore>,
    jobs: Mutex<HashMap<Uuid, Job>>,
    previews: previews::Previews,
}

impl VideoDownloads {
    pub fn new(
        store: Arc<TaskStore>,
        limit: u16,
        emit: impl Fn(DownloadTask) + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            emit: Arc::new(emit),
            slots: Arc::new(Semaphore::new(usize::from(limit.clamp(1, 3)))),
            jobs: Mutex::default(),
            previews: Default::default(),
        }
    }

    pub(crate) async fn prepare(
        &self,
        page: PlaybackPage,
        options: &VideoOptions,
    ) -> Result<VideoPreview, String> {
        let preview = extractor::inspect(
            page,
            options.proxy.as_deref(),
            options.cookie_file.as_deref(),
        )
        .await?;
        self.previews.remember(&preview)?;
        Ok(preview)
    }

    async fn submit(
        &self,
        preview_id: Uuid,
        format_id: &str,
        directory: &Path,
        private_download: bool,
        post_processing_note: Option<&str>,
        options: VideoOptions,
    ) -> Result<DownloadTask, String> {
        let previews::SelectedVideo {
            media_id,
            page,
            title,
            format,
        } = self.previews.select(preview_id, format_id)?;
        let title: String = title
            .chars()
            .scan(0, |bytes, c| {
                *bytes += c.len_utf8();
                (*bytes <= 160).then_some(c)
            })
            .collect();
        let quality = match (format.width, format.height) {
            (0, 0) => String::new(),
            (0, height) => format!(" [{height}p]"),
            (width, 0) => format!(" [w{width}]"),
            (width, height) => format!(" [{width}x{height}]"),
        };
        let name = crate::cloud::commands::safe_filename(&format!(
            "{}{quality}.{}",
            title.trim(),
            format.ext
        ));
        let store = &self.store;
        let new = NewTask {
            source: page,
            name,
            kind: "web_video".into(),
            is_private: private_download,
        };
        let task = store
            .create_video(new, &format.id, &media_id)
            .map_err(|e| e.to_string())?;
        let preparation = (|| {
            let path = crate::output_path::reserve_unique_output_path(
                store, task.id, directory, &task.name,
            )?;
            let mut record = task.clone();
            record.name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            record.total_length = format.bytes;
            store.set_post_processing_note(record.id, post_processing_note)?;
            store.apply_transfer_snapshot_with_files(
                record,
                &[(
                    1,
                    path.to_string_lossy().into_owned(),
                    format.bytes.unwrap_or(0),
                    0,
                    true,
                )],
            )
        })();
        let result = match preparation {
            Ok(record) => self.start(record, options).await,
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(record) => Ok(record),
            Err(e) => {
                let _ = store.remove_task_record(task.id);
                Err(e)
            }
        }
    }

    async fn start(
        &self,
        mut task: DownloadTask,
        options: VideoOptions,
    ) -> Result<DownloadTask, String> {
        let mut jobs = self.jobs.lock().await;
        jobs.retain(|_, (_, job)| !job.inner().is_finished());
        task = self.store.find(task.id).map_err(|e| e.to_string())?;
        if jobs.contains_key(&task.id)
            && matches!(task.status, TaskStatus::Queued | TaskStatus::Downloading)
        {
            return Ok(task);
        }
        // A terminal update is visible just before its worker returns. A retry
        // arriving in that interval still needs to start a new transfer.
        if let Some((_, worker)) = jobs.remove(&task.id) {
            let _ = worker.await;
        }
        if !matches!(
            task.status,
            TaskStatus::Parsing | TaskStatus::Paused | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Err("video.error.state".into());
        }
        let (selector, media_id) = self
            .store
            .video_selector(task.id)
            .map_err(|e| e.to_string())?;
        let engine = self.store.video_engine(task.id).map_err(|_| "video.error.state")?;
        let output = self
            .store
            .reserved_output_path(task.id)
            .map_err(|e| e.to_string())?
            .ok_or("video.error.output")?;
        let cookies = cookies::CookieFile::load(options.cookie_file.as_deref())?;
        let mut cmd = transfer::transfer_command(options.proxy.as_deref(), options.rate)?;
        if let Some(cookies) = &cookies {
            cookies.apply(&mut cmd);
        }
        task.status = TaskStatus::Queued;
        task.error = None;
        task.download_speed = 0;
        let queued = self
            .store
            .apply_transfer_snapshot(task)
            .map_err(|e| e.to_string())?;
        (self.emit)(queued.clone());
        let mut task = queued.clone();
        let (stop, mut stopped) = oneshot::channel();
        let (slots, store, emit) = (self.slots.clone(), self.store.clone(), self.emit.clone());
        let job = tauri::async_runtime::spawn(async move {
            // yt-dlp reads and writes this private snapshot. Keep it until the
            // worker (including cancellation and merging) has fully exited.
            let _cookies = cookies;
            let _permit = tokio::select! {
                biased;
                _ = &mut stopped => return,
                permit = slots.acquire_owned() => match permit { Ok(p) => p, Err(_) => return },
            };
            task.status = TaskStatus::Downloading;
            let publish = |task: &DownloadTask| {
                if let Ok(saved) = store.apply_transfer_snapshot(task.clone()) {
                    emit(saved);
                }
            };
            publish(&task);
            let page = task.source.clone();
            let result = transfer::download(
                cmd,
                &page,
                &selector,
                media_id.as_deref(),
                engine,
                task.id,
                &output,
                stopped,
                |p| {
                    task.completed_length = p.downloaded;
                    task.download_speed = p.speed;
                    if let Some(total) = task.total_length.filter(|n| *n > 0) {
                        task.progress = (p.downloaded.saturating_mul(100) / total).min(99) as u8;
                    }
                    publish(&task);
                },
            )
            .await;
            match result {
                Ok(None) => (),
                Ok(Some(bytes)) => {
                    task.status = TaskStatus::Completed;
                    task.download_speed = 0;
                    task.total_length = Some(bytes);
                    task.completed_length = bytes;
                    task.progress = 100;
                    let file = (1, output.to_string_lossy().into_owned(), bytes, bytes, true);
                    if let Ok(saved) = store.apply_transfer_snapshot_with_files(task, &[file]) {
                        emit(saved);
                    }
                }
                Err(message) => {
                    task.status = TaskStatus::Failed;
                    task.error = Some(message);
                    task.download_speed = 0;
                    publish(&task);
                }
            }
        });
        jobs.insert(queued.id, (stop, job));
        Ok(queued)
    }

    async fn stop(&self, id: Uuid, next: TaskStatus) -> Result<DownloadTask, String> {
        let mut jobs = self.jobs.lock().await;
        if let Some((stop, job)) = jobs.remove(&id) {
            let _ = stop.send(());
            let _ = job.await;
        }
        let mut task = self.store.find(id).map_err(|e| e.to_string())?;
        if matches!(
            task.status,
            TaskStatus::Queued | TaskStatus::Downloading | TaskStatus::Paused
        ) {
            task.status = next;
            task.download_speed = 0;
            task = self
                .store
                .apply_transfer_snapshot(task)
                .map_err(|e| e.to_string())?;
            (self.emit)(task.clone());
        }
        Ok(task)
    }

    pub async fn shutdown(&self) {
        let ids: Vec<_> = self.jobs.lock().await.keys().copied().collect();
        for id in ids {
            let _ = self.stop(id, TaskStatus::Paused).await;
        }
    }

    pub async fn control(
        &self,
        state: &AppState,
        task: DownloadTask,
        action: &str,
    ) -> Result<DownloadTask, String> {
        match action {
            "pause" => self.stop(task.id, TaskStatus::Paused).await,
            "cancel" => self.stop(task.id, TaskStatus::Cancelled).await,
            "retry" | "resume" => {
                self.start(task, network_options(state)?).await
            }
            _ => Err("video.error.state".into()),
        }
    }
}

pub(crate) fn network_options(state: &AppState) -> Result<VideoOptions, String> {
    let settings = state.settings.lock().map_err(|_| "video.error.state")?;
    Ok(VideoOptions {
        proxy: commands::proxy_url(&settings.proxy)?,
        rate: settings.aria2.overall_download_limit,
        cookie_file: (!settings.video_cookie_file.is_empty())
            .then(|| PathBuf::from(&settings.video_cookie_file)),
    })
}

pub(crate) fn validate_cookie_file(path: &str) -> Result<(), String> {
    cookies::CookieFile::load((!path.is_empty()).then(|| Path::new(path)))?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn submit_video_download(
    preview_id: Uuid,
    format_id: String,
    destination_directory: Option<String>,
    private_download: bool,
    post_processing_note: Option<String>,
    state: State<'_, AppState>,
) -> Result<commands::DownloadTaskPayload, String> {
    commands::ensure_private_download_enabled(&state, private_download)?;
    let options = commands::download_submission_options_for_directory(
        &state,
        destination_directory.as_deref(),
    )?;
    let directory = options.directory.ok_or("video.error.output")?;
    let mut network = network_options(&state)?;
    network.proxy = options.proxy;
    let task = state
        .web_video
        .submit(
            preview_id,
            &format_id,
            &directory,
            private_download,
            post_processing_note.as_deref(),
            network,
        )
        .await?;
    commands::task_payload(&state.store, task)
}

#[tauri::command]
pub(crate) fn discard_video_preview(
    preview_id: Uuid,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.web_video.previews.discard(preview_id)
}

pub(crate) fn clear_partial(store: &TaskStore, task: &DownloadTask) -> Result<(), String> {
    if task.kind != "web_video" {
        return Ok(());
    }
    if let Some(path) = store
        .reserved_output_path(task.id)
        .map_err(|e| e.to_string())?
    {
        if path.is_absolute() {
            let dir = transfer::staging(Path::new(&path), task.id);
            match std::fs::remove_dir_all(dir) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err("video.error.output".into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cookie_tests;
