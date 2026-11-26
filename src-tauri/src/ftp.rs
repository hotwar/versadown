use std::{
    collections::{HashMap, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use futures_util::stream::{FuturesUnordered, StreamExt};
use percent_encoding::percent_decode_str;
use suppaftp::{tokio::AsyncFtpStream, types::FileType};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{watch, Mutex},
};
use url::Url;
use uuid::Uuid;

use crate::{
    aria2::{DownloadAction, EventSink, UriSubmissionOptions},
    bandwidth::BandwidthLimiter,
    reserve_unique_output_path, DownloadTask, StoreError, TaskStatus, TaskStore,
};

const ANONYMOUS_PASSWORD: &str = "anonymous@versadown.local";
const MIN_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SESSION_LIMIT_RETRIES_PER_SEGMENT: u8 = 3;
const SESSION_LIMIT_RETRY_DELAY: Duration = Duration::from_millis(250);

#[cfg(test)]
#[path = "cli_ftp_tests.rs"]
mod cli_tests;

#[derive(Clone)]
pub struct FtpTarget {
    host: String,
    port: u16,
    username: String,
    password: String,
    remote_path: String,
    file_name: String,
    display_source: String,
}

impl FtpTarget {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn remote_path(&self) -> &str {
        &self.remote_path
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn display_source(&self) -> &str {
        &self.display_source
    }
}

impl fmt::Debug for FtpTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FtpTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("remote_path", &self.remote_path)
            .field("file_name", &self.file_name)
            .field("display_source", &self.display_source)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum FtpDownloadError {
    #[error("FTP 链接无效：{0}")]
    InvalidUrl(String),
    #[error("FTP 链接必须包含文件路径")]
    MissingFilePath,
    #[error("FTP 下载参数无效：{0}")]
    InvalidOption(&'static str),
    #[error("FTP 服务器操作失败：{0}")]
    Protocol(String),
    #[error("FTP 本地文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn decode_component(value: &str, label: &str) -> Result<String, FtpDownloadError> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| FtpDownloadError::InvalidUrl(format!("{label} 不是有效的 UTF-8")))
}

pub fn parse_ftp_url(source: &str) -> Result<FtpTarget, FtpDownloadError> {
    let url =
        Url::parse(source).map_err(|error| FtpDownloadError::InvalidUrl(error.to_string()))?;
    if url.scheme() != "ftp" {
        return Err(FtpDownloadError::InvalidUrl("仅支持 ftp:// 链接".into()));
    }
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| FtpDownloadError::InvalidUrl("缺少服务器地址".into()))?
        .to_owned();
    let port = url.port().unwrap_or(21);
    let username = if url.username().is_empty() {
        "anonymous".to_owned()
    } else {
        decode_component(url.username(), "用户名")?
    };
    let password = match url.password() {
        Some(password) => decode_component(password, "密码")?,
        None if username == "anonymous" => ANONYMOUS_PASSWORD.to_owned(),
        None => String::new(),
    };
    let remote_path = decode_component(url.path(), "文件路径")?;
    let file_name = remote_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or(FtpDownloadError::MissingFilePath)?
        .to_owned();
    let mut display = url.clone();
    let _ = display.set_username("");
    let _ = display.set_password(None);

    Ok(FtpTarget {
        host,
        port,
        username,
        password,
        remote_path,
        file_name,
        display_source: display.to_string(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerCommand {
    Running,
    Pause,
    Cancel,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FtpDownloadLimits {
    max_active_tasks: usize,
    max_connections_per_task: usize,
}

impl FtpDownloadLimits {
    pub fn new(max_active_tasks: u16, max_connections_per_task: u8) -> Self {
        Self {
            max_active_tasks: usize::from(max_active_tasks.max(1)),
            max_connections_per_task: usize::from(max_connections_per_task.max(1)),
        }
    }

    pub fn max_active_tasks(self) -> usize {
        self.max_active_tasks
    }

    pub fn max_connections_per_task(self) -> usize {
        self.max_connections_per_task
    }
}

impl Default for FtpDownloadLimits {
    fn default() -> Self {
        Self::new(5, 8)
    }
}

struct QueuedWorker {
    task_id: Uuid,
    source: String,
    output: PathBuf,
}

#[derive(Default)]
struct WorkerScheduler {
    active: HashMap<Uuid, watch::Sender<WorkerCommand>>,
    queued: VecDeque<QueuedWorker>,
}

struct SegmentPublishState {
    sample_started: Instant,
    sample_completed: u64,
    last_published: Instant,
}

pub struct FtpDownloadEngine {
    default_directory: RwLock<PathBuf>,
    store: Arc<TaskStore>,
    events: Arc<dyn EventSink>,
    scheduler: Mutex<WorkerScheduler>,
    limits: RwLock<FtpDownloadLimits>,
    download_limiter: Arc<BandwidthLimiter>,
    accepting: AtomicBool,
}

impl FtpDownloadEngine {
    pub fn new(
        default_directory: impl AsRef<Path>,
        store: Arc<TaskStore>,
        events: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            default_directory: RwLock::new(default_directory.as_ref().to_path_buf()),
            store,
            events,
            scheduler: Mutex::new(WorkerScheduler::default()),
            limits: RwLock::new(FtpDownloadLimits::default()),
            download_limiter: Arc::new(BandwidthLimiter::new(0)),
            accepting: AtomicBool::new(true),
        }
    }

    pub fn set_limits(self: &Arc<Self>, max_active_tasks: u16, max_connections_per_task: u8) {
        *self
            .limits
            .write()
            .unwrap_or_else(|error| error.into_inner()) =
            FtpDownloadLimits::new(max_active_tasks, max_connections_per_task);
        self.schedule_pump();
    }

    pub fn limits(&self) -> FtpDownloadLimits {
        *self
            .limits
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn set_download_limit(&self, bytes_per_second: u64) {
        self.download_limiter.set_limit(bytes_per_second);
    }

    pub async fn submit_uri(
        self: &Arc<Self>,
        task_id: Uuid,
        source: &str,
        options: UriSubmissionOptions,
    ) -> Result<DownloadTask, FtpDownloadError> {
        let (task, output) = self.prepare_submission(task_id, source, options).await?;
        self.start_worker(task_id, source.to_owned(), output).await?;
        Ok(task)
    }

    async fn prepare_submission(
        &self, task_id: Uuid, source: &str, options: UriSubmissionOptions,
    ) -> Result<(DownloadTask, PathBuf), FtpDownloadError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(FtpDownloadError::Protocol("应用正在退出".into()));
        }
        if !options.headers.is_empty() {
            return Err(FtpDownloadError::InvalidOption(
                "FTP 不支持自定义 HTTP 请求头",
            ));
        }
        if options.proxy.is_some() {
            return Err(FtpDownloadError::InvalidOption("FTP 暂不支持 HTTP 代理"));
        }
        let target = parse_ftp_url(source)?;
        let directory = options.directory.unwrap_or_else(|| {
            self.default_directory
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        });
        if directory.as_os_str().is_empty() {
            return Err(FtpDownloadError::InvalidOption("下载目录为空"));
        }
        tokio::fs::create_dir_all(&directory).await?;
        let output_name = options
            .output_name
            .unwrap_or_else(|| target.file_name.clone());
        if !valid_output_name(&output_name) {
            return Err(FtpDownloadError::InvalidOption("输出文件名无效"));
        }
        let output = reserve_unique_output_path(&self.store, task_id, &directory, &output_name)?;
        if options.allow_overwrite {
            if tokio::fs::try_exists(&output).await? {
                tokio::fs::remove_file(&output).await?;
            }
            self.store.clear_ftp_segments(task_id)?;
        }

        let mut task = self.store.find(task_id)?;
        task.name = output
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(target.file_name())
            .to_owned();
        task.status = TaskStatus::Queued;
        task.error = None;
        task.failure_code = None;
        task.aria_gid = None;
        task.download_speed = 0;
        task.upload_speed = 0;
        task.connections = 0;
        task.eta_seconds = None;
        let saved_segments = self.store.ftp_segments(task_id)?;
        let existing = if saved_segments.is_empty() {
            tokio::fs::metadata(&output)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        } else {
            saved_segments
                .iter()
                .map(|segment| segment.completed_length)
                .sum()
        };
        task.completed_length = existing;
        task.progress = 0;
        task = self.persist(task, &output, 0, existing)?;

        Ok((task, output))
    }

    /// CLI recovery is owned by the authorized dispatcher. The in-process FTP
    /// scheduler identifies work by task ID, including the crash window after
    /// preparing a file but before acknowledging submission in the journal.
    pub(crate) async fn dispatch_cli_intent(
        self: &Arc<Self>, intent: &crate::store::CliDispatchIntent,
        authorize: &(dyn Fn(&Path) -> bool + Send + Sync),
    ) -> Result<DownloadTask, FtpDownloadError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(FtpDownloadError::InvalidOption("engine is stopping"));
        }
        let mut task = self.store.find(intent.task_id)?;
        if task.kind != "ftp" { return Err(FtpDownloadError::InvalidOption("task kind")); }
        if !matches!(task.status, TaskStatus::Queued | TaskStatus::Downloading) {
            if matches!(task.status, TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Recycled | TaskStatus::Failed) {
                self.store.finish_cli_dispatch(task.id)?;
            }
            return Ok(task);
        }
        if task.is_private || !intent.options.directory.as_ref().is_some_and(|path| authorize(path)) {
            if self.has_worker(task.id).await {
                return self.control(task.id, DownloadAction::Pause).await;
            }
            return Ok(task);
        }
        let output = match self.store.files(task.id)?.first() {
            Some(file) => PathBuf::from(&file.path),
            None => {
                if let Some(path) = self.store.reserved_output_path(task.id)? {
                    if !authorize(&path) { return Ok(task); }
                    // A reserved payload with no persisted transfer record is
                    // not enough evidence that its contents belong to us.
                    if path.exists() { return Err(FtpDownloadError::InvalidOption("recovery checkpoint")); }
                }
                let prepared = self.prepare_submission(task.id, &task.source, intent.options.clone()).await?;
                task = prepared.0;
                prepared.1
            }
        };
        let latest = self.store.find(task.id)?;
        if latest.is_private || !matches!(latest.status, TaskStatus::Queued | TaskStatus::Downloading)
            || !intent.options.directory.as_ref().is_some_and(|path| authorize(path))
            || !authorize(&output)
        {
            if self.has_worker(task.id).await { return self.control(task.id, DownloadAction::Pause).await; }
            return Ok(latest);
        }
        if self.store.reserved_output_path(task.id)?.as_ref() != Some(&output) {
            return Err(FtpDownloadError::InvalidOption("recovery checkpoint"));
        }
        self.start_worker(task.id, task.source.clone(), output).await?;
        self.store.finish_cli_dispatch(task.id)?;
        Ok(latest)
    }

    async fn has_worker(&self, id: Uuid) -> bool {
        let scheduler = self.scheduler.lock().await;
        scheduler.active.contains_key(&id) || scheduler.queued.iter().any(|work| work.task_id == id)
    }

    pub async fn control(
        self: &Arc<Self>,
        task_id: Uuid,
        action: DownloadAction,
    ) -> Result<DownloadTask, FtpDownloadError> {
        let current = self.store.find(task_id)?;
        if current.kind != "ftp" {
            return Err(FtpDownloadError::InvalidOption("任务不是 FTP 下载"));
        }
        match action {
            DownloadAction::Pause => {
                self.signal_and_wait(task_id, WorkerCommand::Pause).await;
                let latest = self.store.find(task_id)?;
                if latest.status == TaskStatus::Completed {
                    return Ok(latest);
                }
                self.set_terminal_transfer_state(latest, TaskStatus::Paused, None)
            }
            DownloadAction::Cancel => {
                self.signal_and_wait(task_id, WorkerCommand::Cancel).await;
                let latest = self.store.find(task_id)?;
                if latest.status == TaskStatus::Completed {
                    return Ok(latest);
                }
                let cancelled =
                    self.set_terminal_transfer_state(latest, TaskStatus::Cancelled, None)?;
                self.store.clear_ftp_segments(task_id)?;
                Ok(cancelled)
            }
            DownloadAction::Resume => {
                if current.status != TaskStatus::Paused {
                    return Err(FtpDownloadError::InvalidOption("当前 FTP 任务不能继续"));
                }
                let output = self
                    .store
                    .files(task_id)?
                    .first()
                    .map(|file| PathBuf::from(&file.path))
                    .ok_or(FtpDownloadError::InvalidOption("缺少 FTP 输出文件路径"))?;
                let source = current.source.clone();
                let mut queued = current;
                queued.status = TaskStatus::Queued;
                queued.error = None;
                queued.download_speed = 0;
                queued.connections = 0;
                let completed = queued.completed_length;
                let total = queued.total_length.unwrap_or(0);
                let queued = self.persist(queued, &output, total, completed)?;
                self.start_worker(task_id, source, output).await?;
                Ok(queued)
            }
        }
    }

    pub async fn recover(self: &Arc<Self>) -> Result<(), FtpDownloadError> {
        for task in self.store.list()? {
            if task.kind != "ftp"
                || !matches!(task.status, TaskStatus::Queued | TaskStatus::Downloading)
                || self.store.has_cli_intent(task.id)?
            {
                continue;
            }
            let Some(output) = self
                .store
                .files(task.id)?
                .first()
                .map(|file| PathBuf::from(&file.path))
            else {
                let _ = self.store.fail(task.id, "FTP 恢复失败：缺少输出文件路径");
                continue;
            };
            if let Err(error) = self
                .start_worker(task.id, task.source.clone(), output)
                .await
            {
                let _ = self.store.fail(task.id, format!("FTP 恢复失败：{error}"));
            }
        }
        Ok(())
    }

    pub fn set_default_directory(&self, directory: impl AsRef<Path>) {
        *self
            .default_directory
            .write()
            .unwrap_or_else(|error| error.into_inner()) = directory.as_ref().to_path_buf();
    }

    pub fn global_stat(&self) -> Result<crate::Aria2GlobalStat, FtpDownloadError> {
        let tasks = self.store.list()?;
        Ok(crate::Aria2GlobalStat {
            download_speed: tasks
                .iter()
                .filter(|task| task.kind == "ftp")
                .map(|task| task.download_speed)
                .sum(),
            upload_speed: 0,
            num_active: tasks
                .iter()
                .filter(|task| task.kind == "ftp" && task.status == TaskStatus::Downloading)
                .count() as u64,
            num_waiting: tasks
                .iter()
                .filter(|task| task.kind == "ftp" && task.status == TaskStatus::Queued)
                .count() as u64,
            num_stopped: tasks
                .iter()
                .filter(|task| {
                    task.kind == "ftp"
                        && matches!(
                            task.status,
                            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                        )
                })
                .count() as u64,
            num_stopped_total: tasks
                .iter()
                .filter(|task| {
                    task.kind == "ftp"
                        && matches!(
                            task.status,
                            TaskStatus::Completed
                                | TaskStatus::Failed
                                | TaskStatus::Cancelled
                                | TaskStatus::Recycled
                        )
                })
                .count() as u64,
        })
    }

    pub async fn shutdown(&self) {
        self.accepting.store(false, Ordering::Release);
        let scheduler = self.scheduler.lock().await;
        for sender in scheduler.active.values() {
            let _ = sender.send(WorkerCommand::Shutdown);
        }
    }

    async fn start_worker(
        self: &Arc<Self>,
        task_id: Uuid,
        source: String,
        output: PathBuf,
    ) -> Result<(), FtpDownloadError> {
        {
            let mut scheduler = self.scheduler.lock().await;
            if scheduler.active.contains_key(&task_id)
                || scheduler.queued.iter().any(|work| work.task_id == task_id)
            {
                return Ok(());
            }
            scheduler.queued.push_back(QueuedWorker {
                task_id,
                source,
                output,
            });
        }
        self.pump_queue().await;
        Ok(())
    }

    async fn pump_queue(self: &Arc<Self>) {
        let limits = self.limits();
        let mut starts = Vec::new();
        {
            let mut scheduler = self.scheduler.lock().await;
            while scheduler.active.len() < limits.max_active_tasks() {
                let Some(work) = scheduler.queued.pop_front() else {
                    break;
                };
                let (sender, receiver) = watch::channel(WorkerCommand::Running);
                scheduler.active.insert(work.task_id, sender);
                starts.push((work, receiver));
            }
        }
        for (work, receiver) in starts {
            let engine = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let result = engine
                    .run_worker(work.task_id, &work.source, &work.output, receiver)
                    .await;
                engine.scheduler.lock().await.active.remove(&work.task_id);
                if let Err(error) = result {
                    if let Ok(current) = engine.store.find(work.task_id) {
                        if !matches!(current.status, TaskStatus::Paused | TaskStatus::Cancelled) {
                            if let Ok(failed) = engine.store.fail(work.task_id, error.to_string()) {
                                engine.events.task_updated(&failed);
                            }
                        }
                    }
                }
                engine.schedule_pump();
            });
        }
    }

    fn schedule_pump(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            engine.pump_queue().await;
        });
    }

    async fn run_worker(
        &self,
        task_id: Uuid,
        source: &str,
        output: &Path,
        mut command: watch::Receiver<WorkerCommand>,
    ) -> Result<(), FtpDownloadError> {
        if *command.borrow_and_update() != WorkerCommand::Running {
            return Ok(());
        }
        let saved_segments = self.store.ftp_segments(task_id)?;
        let file_length = tokio::fs::metadata(output)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let completed_before_connect = if saved_segments.is_empty() {
            file_length
        } else {
            saved_segments
                .iter()
                .map(|segment| segment.completed_length)
                .sum()
        };
        let mut connecting = self.store.find(task_id)?;
        connecting.status = TaskStatus::Downloading;
        connecting.completed_length = completed_before_connect;
        connecting.download_speed = 0;
        connecting.connections = 0;
        connecting.error = None;
        let known_total = connecting.total_length.unwrap_or(0);
        let connecting = self.persist(connecting, output, known_total, completed_before_connect)?;
        self.events.task_updated(&connecting);

        let target = parse_ftp_url(source)?;
        let mut ftp = connect_ftp(&target).await?;
        let total = ftp
            .size(target.remote_path())
            .await
            .ok()
            .and_then(|size| u64::try_from(size).ok());
        let total = match total {
            Some(total) => total,
            None if saved_segments.is_empty() => {
                return self
                    .run_single_transfer(task_id, &target, output, None, file_length, ftp, command)
                    .await;
            }
            None => {
                let total = saved_segments
                    .last()
                    .map(|segment| segment.end_offset)
                    .ok_or_else(|| FtpDownloadError::Protocol("缺少 FTP 分片范围".into()))?;
                if !segments_match_total(&saved_segments, total) {
                    return Err(FtpDownloadError::Protocol("FTP 分片范围无效".into()));
                }
                total
            }
        };

        if !saved_segments.is_empty() {
            if !segments_match_total(&saved_segments, total) {
                return Err(FtpDownloadError::Protocol(
                    "远程文件大小已变化，无法安全续传".into(),
                ));
            }
            let completed: u64 = saved_segments
                .iter()
                .map(|segment| segment.completed_length)
                .sum();
            if completed == total {
                let task = self.completed_snapshot(task_id, output, total)?;
                self.events.task_updated(&task);
                let _ = ftp.quit().await;
                return Ok(());
            }
            let _ = ftp.quit().await;
            return self
                .run_segmented_transfer(task_id, &target, output, total, saved_segments, command)
                .await;
        }

        let mut completed = file_length;
        if completed > total {
            tokio::fs::File::create(output).await?;
            completed = 0;
        }
        if completed == total {
            let task = self.completed_snapshot(task_id, output, total)?;
            self.events.task_updated(&task);
            let _ = ftp.quit().await;
            return Ok(());
        }

        let segments = plan_segments(total, self.limits().max_connections_per_task());
        if completed == 0 && segments.len() > 1 && self.supports_parallel_ranges(&target).await {
            let _ = ftp.quit().await;
            return self
                .run_segmented_transfer(task_id, &target, output, total, segments, command)
                .await;
        }

        self.run_single_transfer(
            task_id,
            &target,
            output,
            Some(total),
            completed,
            ftp,
            command,
        )
        .await
    }

    async fn supports_parallel_ranges(&self, target: &FtpTarget) -> bool {
        let Ok(mut ftp) = connect_ftp(target).await else {
            return false;
        };
        let supports_rest = ftp.resume_transfer(1).await.is_ok();
        let _ = ftp.quit().await;
        supports_rest
    }

    async fn run_single_transfer(
        &self,
        task_id: Uuid,
        target: &FtpTarget,
        output: &Path,
        total: Option<u64>,
        mut completed: u64,
        mut ftp: AsyncFtpStream,
        mut command: watch::Receiver<WorkerCommand>,
    ) -> Result<(), FtpDownloadError> {
        if completed > 0 {
            ftp.resume_transfer(
                usize::try_from(completed)
                    .map_err(|_| FtpDownloadError::Protocol("续传偏移量过大".into()))?,
            )
            .await
            .map_err(|error| {
                FtpDownloadError::Protocol(format!(
                    "服务器不支持从 {completed} 字节处续传：{error}"
                ))
            })?;
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(completed > 0)
            .write(true)
            .truncate(completed == 0)
            .open(output)
            .await?;
        let mut data = ftp
            .retr_as_stream(target.remote_path())
            .await
            .map_err(protocol_error)?;
        let mut task = self.store.find(task_id)?;
        task.status = TaskStatus::Downloading;
        task.total_length = total;
        task.completed_length = completed;
        task.connections = 1;
        task.error = None;
        task.progress = total.map(|total| progress(completed, total)).unwrap_or(0);
        task = self.persist(task, output, total.unwrap_or(0), completed)?;
        self.events.task_updated(&task);

        let mut buffer = vec![0u8; 64 * 1024];
        let mut sample_started = Instant::now();
        let mut sample_completed = completed;
        let mut last_published = Instant::now();
        loop {
            let read = tokio::select! {
                result = data.read(&mut buffer) => result?,
                changed = command.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    match *command.borrow_and_update() {
                        WorkerCommand::Running => continue,
                        WorkerCommand::Pause | WorkerCommand::Cancel | WorkerCommand::Shutdown => return Ok(()),
                    }
                }
            };
            if read == 0 {
                break;
            }
            self.download_limiter.consume_async(read).await;
            file.write_all(&buffer[..read]).await?;
            completed = completed.saturating_add(read as u64);
            if last_published.elapsed() >= Duration::from_millis(200)
                || total.is_some_and(|total| completed >= total)
            {
                let elapsed = sample_started.elapsed().as_secs_f64().max(0.001);
                let speed = ((completed.saturating_sub(sample_completed)) as f64 / elapsed) as u64;
                let mut task = self.store.find(task_id)?;
                if task.status != TaskStatus::Downloading {
                    return Ok(());
                }
                task.total_length = total;
                task.completed_length = completed;
                task.download_speed = speed;
                task.connections = 1;
                task.progress = total.map(|total| progress(completed, total)).unwrap_or(0);
                task.eta_seconds = total
                    .filter(|_| speed > 0)
                    .map(|total| total.saturating_sub(completed) / speed);
                let task = self.persist(task, output, total.unwrap_or(0), completed)?;
                self.events.task_updated(&task);
                sample_started = Instant::now();
                sample_completed = completed;
                last_published = Instant::now();
            }
        }
        file.flush().await?;
        ftp.finalize_retr_stream(data)
            .await
            .map_err(protocol_error)?;
        let _ = ftp.quit().await;
        if let Some(total) = total {
            if completed != total {
                return Err(FtpDownloadError::Protocol(format!(
                    "传输提前结束：已接收 {completed} / {total} 字节"
                )));
            }
        }
        let task = self.completed_snapshot(task_id, output, total.unwrap_or(completed))?;
        self.events.task_updated(&task);
        Ok(())
    }

    async fn run_segmented_transfer(
        &self,
        task_id: Uuid,
        target: &FtpTarget,
        output: &Path,
        total: u64,
        mut segments: Vec<crate::FtpSegment>,
        command: watch::Receiver<WorkerCommand>,
    ) -> Result<(), FtpDownloadError> {
        let persisted = self.store.ftp_segments(task_id)?;
        if persisted.is_empty() {
            self.store.replace_ftp_segments(task_id, &segments)?;
        } else {
            segments = persisted;
        }

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(output)
            .await?;
        file.set_len(total).await?;
        drop(file);

        let completed: u64 = segments
            .iter()
            .map(|segment| segment.completed_length)
            .sum();
        let connections = segments
            .iter()
            .filter(|segment| segment.completed_length < segment.end_offset - segment.start_offset)
            .count() as u32;
        self.publish_segment_snapshot(task_id, output, total, completed, 0, connections)?;

        let publish_state = Arc::new(Mutex::new(SegmentPublishState {
            sample_started: Instant::now(),
            sample_completed: completed,
            last_published: Instant::now(),
        }));
        let mut pending_segments: VecDeque<_> = segments
            .into_iter()
            .filter(|segment| segment.completed_length < segment.end_offset - segment.start_offset)
            .collect();
        let mut max_parallel_connections = connections.max(1) as usize;
        let mut workers = FuturesUnordered::new();
        let mut session_limit_retries = HashMap::new();
        let mut stopped = false;
        loop {
            while !stopped && workers.len() < max_parallel_connections {
                let Some(segment) = pending_segments.pop_front() else {
                    break;
                };
                let segment_index = segment.index;
                let segment_target = target.clone();
                let segment_output = output.to_path_buf();
                let segment_command = command.clone();
                let segment_publish_state = Arc::clone(&publish_state);
                workers.push(async move {
                    (
                        segment_index,
                        self.run_segment(
                            task_id,
                            segment_target,
                            segment_output,
                            total,
                            segment,
                            segment_command,
                            segment_publish_state,
                            max_parallel_connections as u32,
                        )
                        .await,
                    )
                });
            }

            let Some((segment_index, result)) = workers.next().await else {
                break;
            };
            match result {
                Ok(segment_stopped) => stopped |= segment_stopped,
                Err(error) if is_session_limit_error(&error) => {
                    let retries = session_limit_retries.entry(segment_index).or_insert(0);
                    if *retries >= MAX_SESSION_LIMIT_RETRIES_PER_SEGMENT {
                        return Err(error);
                    }
                    *retries += 1;
                    max_parallel_connections = max_parallel_connections.min(workers.len().max(1));
                    let retry_segment = self
                        .store
                        .ftp_segments(task_id)?
                        .into_iter()
                        .find(|segment| segment.index == segment_index)
                        .ok_or_else(|| {
                            FtpDownloadError::Protocol("缺少待重试的 FTP 分片".into())
                        })?;
                    if retry_segment.completed_length
                        < retry_segment.end_offset - retry_segment.start_offset
                    {
                        pending_segments.push_back(retry_segment);
                    }
                    if workers.is_empty() {
                        tokio::time::sleep(SESSION_LIMIT_RETRY_DELAY).await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if stopped {
            return Ok(());
        }
        let completed: u64 = self
            .store
            .ftp_segments(task_id)?
            .iter()
            .map(|segment| segment.completed_length)
            .sum();
        if completed != total {
            return Err(FtpDownloadError::Protocol(format!(
                "分片传输提前结束：已接收 {completed} / {total} 字节"
            )));
        }
        let task = self.completed_snapshot(task_id, output, total)?;
        self.events.task_updated(&task);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_segment(
        &self,
        task_id: Uuid,
        target: FtpTarget,
        output: PathBuf,
        total: u64,
        mut segment: crate::FtpSegment,
        mut command: watch::Receiver<WorkerCommand>,
        publish_state: Arc<Mutex<SegmentPublishState>>,
        connections: u32,
    ) -> Result<bool, FtpDownloadError> {
        let mut ftp = connect_ftp(&target).await?;
        let start = segment
            .start_offset
            .checked_add(segment.completed_length)
            .ok_or_else(|| FtpDownloadError::Protocol("分片续传偏移量溢出".into()))?;
        ftp.resume_transfer(
            usize::try_from(start)
                .map_err(|_| FtpDownloadError::Protocol("分片续传偏移量过大".into()))?,
        )
        .await
        .map_err(protocol_error)?;
        let mut data = ftp
            .retr_as_stream(target.remote_path())
            .await
            .map_err(protocol_error)?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&output)
            .await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let remaining = segment
                .end_offset
                .saturating_sub(segment.start_offset + segment.completed_length);
            if remaining == 0 {
                break;
            }
            let limit = remaining.min(buffer.len() as u64) as usize;
            let read = tokio::select! {
                result = data.read(&mut buffer[..limit]) => result?,
                changed = command.changed() => {
                    if changed.is_err() {
                        return Ok(true);
                    }
                    match *command.borrow_and_update() {
                        WorkerCommand::Running => continue,
                        WorkerCommand::Pause | WorkerCommand::Cancel | WorkerCommand::Shutdown => return Ok(true),
                    }
                }
            };
            if read == 0 {
                return Err(FtpDownloadError::Protocol(format!(
                    "分片 {} 提前结束",
                    segment.index
                )));
            }
            self.download_limiter.consume_async(read).await;
            file.write_all(&buffer[..read]).await?;
            segment.completed_length = segment.completed_length.saturating_add(read as u64);
            self.store
                .update_ftp_segment(task_id, segment.index, segment.completed_length)?;

            let mut state = publish_state.lock().await;
            let completed: u64 = self
                .store
                .ftp_segments(task_id)?
                .iter()
                .map(|segment| segment.completed_length)
                .sum();
            if state.last_published.elapsed() >= Duration::from_millis(200) || completed == total {
                let elapsed = state.sample_started.elapsed().as_secs_f64().max(0.001);
                let speed =
                    ((completed.saturating_sub(state.sample_completed)) as f64 / elapsed) as u64;
                self.publish_segment_snapshot(
                    task_id,
                    &output,
                    total,
                    completed,
                    speed,
                    connections,
                )?;
                state.sample_started = Instant::now();
                state.sample_completed = completed;
                state.last_published = Instant::now();
            }
        }
        file.flush().await?;
        if segment.end_offset == total {
            ftp.finalize_retr_stream(data)
                .await
                .map_err(protocol_error)?;
        } else {
            drop(data);
            let _ = ftp.quit().await;
        }
        Ok(false)
    }

    fn publish_segment_snapshot(
        &self,
        task_id: Uuid,
        output: &Path,
        total: u64,
        completed: u64,
        speed: u64,
        connections: u32,
    ) -> Result<DownloadTask, FtpDownloadError> {
        let mut task = self.store.find(task_id)?;
        if task.status != TaskStatus::Downloading {
            return Ok(task);
        }
        task.total_length = Some(total);
        task.completed_length = completed;
        task.download_speed = speed;
        task.connections = connections;
        task.progress = progress(completed, total);
        task.eta_seconds = (speed > 0).then(|| total.saturating_sub(completed) / speed);
        let task = self.persist(task, output, total, completed)?;
        self.events.task_updated(&task);
        Ok(task)
    }

    fn completed_snapshot(
        &self,
        task_id: Uuid,
        output: &Path,
        total: u64,
    ) -> Result<DownloadTask, FtpDownloadError> {
        let mut task = self.store.find(task_id)?;
        task.status = TaskStatus::Completed;
        task.progress = 100;
        task.error = None;
        task.total_length = Some(total);
        task.completed_length = total;
        task.download_speed = 0;
        task.upload_speed = 0;
        task.connections = 0;
        task.eta_seconds = Some(0);
        self.store.clear_ftp_segments(task_id)?;
        self.persist(task, output, total, total)
    }

    fn set_terminal_transfer_state(
        &self,
        mut task: DownloadTask,
        status: TaskStatus,
        error: Option<String>,
    ) -> Result<DownloadTask, FtpDownloadError> {
        task.status = status;
        task.error = error;
        task.download_speed = 0;
        task.upload_speed = 0;
        task.connections = 0;
        task.eta_seconds = None;
        let files = self.store.files(task.id)?;
        let output = files
            .first()
            .map(|file| PathBuf::from(&file.path))
            .unwrap_or_else(|| {
                self.default_directory
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .join(&task.name)
            });
        let total = task.total_length.unwrap_or(0);
        let segments = self.store.ftp_segments(task.id)?;
        let completed = if segments.is_empty() {
            std::fs::metadata(&output)
                .map(|metadata| metadata.len())
                .unwrap_or(task.completed_length)
        } else {
            segments
                .iter()
                .map(|segment| segment.completed_length)
                .sum()
        };
        task.completed_length = completed;
        task.progress = progress(completed, total);
        let task = self.persist(task, &output, total, completed)?;
        self.events.task_updated(&task);
        Ok(task)
    }

    fn persist(
        &self,
        task: DownloadTask,
        output: &Path,
        total: u64,
        completed: u64,
    ) -> Result<DownloadTask, FtpDownloadError> {
        Ok(self.store.apply_transfer_snapshot_with_files(
            task,
            &[(
                1,
                output.to_string_lossy().into_owned(),
                total,
                completed,
                true,
            )],
        )?)
    }

    async fn signal_and_wait(&self, task_id: Uuid, command: WorkerCommand) {
        let sender = {
            let mut scheduler = self.scheduler.lock().await;
            scheduler.queued.retain(|work| work.task_id != task_id);
            scheduler.active.get(&task_id).cloned()
        };
        if let Some(sender) = sender {
            let _ = sender.send(command);
            for _ in 0..100 {
                if !self.scheduler.lock().await.active.contains_key(&task_id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

async fn connect_ftp(target: &FtpTarget) -> Result<AsyncFtpStream, FtpDownloadError> {
    let address = format!("{}:{}", target.host(), target.port());
    let mut ftp = tokio::time::timeout(Duration::from_secs(20), AsyncFtpStream::connect(address))
        .await
        .map_err(|_| FtpDownloadError::Protocol("连接超时".into()))?
        .map_err(protocol_error)?;
    ftp.login(target.username(), target.password())
        .await
        .map_err(protocol_error)?;
    ftp.transfer_type(FileType::Binary)
        .await
        .map_err(protocol_error)?;
    Ok(ftp)
}

fn plan_segments(total: u64, connection_limit: usize) -> Vec<crate::FtpSegment> {
    let count = connection_limit
        .max(1)
        .min(total.saturating_add(MIN_SEGMENT_BYTES - 1) as usize / MIN_SEGMENT_BYTES as usize)
        .max(1);
    (0..count)
        .map(|index| crate::FtpSegment {
            index: index as u32,
            start_offset: total * index as u64 / count as u64,
            end_offset: total * (index as u64 + 1) / count as u64,
            completed_length: 0,
        })
        .collect()
}

fn segments_match_total(segments: &[crate::FtpSegment], total: u64) -> bool {
    segments
        .iter()
        .enumerate()
        .try_fold(0u64, |expected_start, (index, segment)| {
            (segment.index == index as u32
                && segment.start_offset == expected_start
                && segment.end_offset > segment.start_offset
                && segment.completed_length <= segment.end_offset - segment.start_offset)
                .then_some(segment.end_offset)
        })
        == Some(total)
}

fn protocol_error(error: suppaftp::FtpError) -> FtpDownloadError {
    FtpDownloadError::Protocol(error.to_string())
}

fn is_session_limit_error(error: &FtpDownloadError) -> bool {
    let FtpDownloadError::Protocol(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("530")
        && [
            "maximum number of client",
            "maximum number of connection",
            "maximum number of session",
            "too many client",
            "too many connection",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

fn valid_output_name(value: &str) -> bool {
    !value.trim().is_empty()
        && !value.contains(['\r', '\n', '/', '\\', '<', '>', ':', '"', '|', '?', '*'])
        && !value.ends_with(['.', ' '])
}

fn progress(completed: u64, total: u64) -> u8 {
    if total == 0 {
        0
    } else {
        ((u128::from(completed) * 100 / u128::from(total)).min(100)) as u8
    }
}
