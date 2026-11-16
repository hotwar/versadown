use super::*;

impl DownloadService {
    /// Accepting the selected files is a separate durable transaction in the
    /// resource journal. This method only delivers that accepted intent, using
    /// the same GID before and after an uncertain addTorrent response.
    pub(crate) async fn dispatch_cli_torrent(
        &self, task_id: Uuid, metadata: &crate::cli_bt::BtPreparation, indices: &[u32],
        options: &UriSubmissionOptions, authorize: &(dyn Fn(&Path) -> bool + Send + Sync),
    ) -> Result<DownloadTask, DownloadServiceError> {
        let _guard = self.operation.lock().await;
        self.ensure_accepting()?;
        let checked = crate::cli_bt::inspect(&metadata.torrent)
            .map_err(|_| DownloadServiceError::InvalidUriOption("torrent metadata"))?;
        if checked.info_hash != metadata.info_hash || checked.files.len() != metadata.files.len()
            || checked.files.iter().zip(&metadata.files).any(|(a, b)| a.index != b.index || a.path != b.path || a.length != b.length)
        { return Err(DownloadServiceError::InvalidUriOption("torrent metadata")); }
        let selection: BTreeSet<_> = indices.iter().copied().collect();
        if selection.is_empty() || selection.len() != indices.len()
            || selection.iter().any(|index| !metadata.files.iter().any(|file| file.index == *index))
        { return Err(DownloadServiceError::InvalidUriOption("torrent selection")); }
        if options.allow_overwrite || options.continue_download || options.output_name.is_some()
            || !options.headers.is_empty() { return Err(DownloadServiceError::InvalidUriOption("torrent options")); }
        let parent = options.directory.as_deref().filter(|path| path.is_absolute())
            .ok_or(DownloadServiceError::InvalidUriOption("torrent directory"))?;
        let output = crate::cli_bt::output_directory(task_id, parent);
        let authorized = || authorize(parent) && authorize(&output)
            && metadata.files.iter().all(|file| authorize(&output.join(&file.path)));
        let gid = format!("{:016x}", (task_id.as_u128() >> 64) as u64);
        let mut task = self.store.find(task_id)?;
        if task.is_private || !matches!(task.kind.as_str(), "magnet" | "torrent") {
            return Err(DownloadServiceError::InvalidUriOption("torrent task"));
        }
        if task.aria_gid.as_ref().is_some_and(|existing| existing != &gid) {
            return Err(DownloadServiceError::InvalidNativeCheckpoint);
        }
        // Bind before the first RPC so native recovery cannot import a job
        // whose response was lost as an unrelated task.
        if task.aria_gid.is_none() { task = self.store.bind_gid(task_id, &gid)?; }
        let existing = Self::native_statuses(self.api().as_ref()).await?
            .into_iter().find(|status| status.gid == gid);
        if matches!(task.status, TaskStatus::Paused | TaskStatus::Cancelled | TaskStatus::Recycled | TaskStatus::Completed | TaskStatus::Failed) {
            if let Some(status) = &existing {
                if matches!(task.status, TaskStatus::Cancelled | TaskStatus::Recycled) {
                    if matches!(status.status.as_str(), "active" | "waiting" | "paused")
                        && self.api().remove(&gid).await.is_err() { self.api().force_remove(&gid).await?; }
                } else if task.status == TaskStatus::Paused && matches!(status.status.as_str(), "active" | "waiting") {
                    self.api().force_pause(&gid).await?;
                }
                self.api().save_session().await?;
            }
            return Ok(task);
        }
        if !authorized() {
            if existing.is_some() { let _ = self.api().force_pause(&gid).await; }
            return Err(DownloadServiceError::InvalidUriOption("authorization"));
        }
        let selection_text = selection.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let mut status = if let Some(status) = existing { status } else {
            match self.store.reserved_output_path(task_id)? {
                Some(reserved) if reserved != output => return Err(DownloadServiceError::InvalidNativeCheckpoint),
                None if output.exists() => return Err(DownloadServiceError::InvalidNativeCheckpoint),
                _ => (),
            }
            if !self.store.reserve_output_path(task_id, &output)? {
                return Err(DownloadServiceError::InvalidNativeCheckpoint);
            }
            // Empty reserved directories are safe to retry after a pre-RPC
            // crash. Any payload without the original engine identity needs
            // explicit recovery, never a silent new torrent job.
            if output.exists() && std::fs::read_dir(&output).map_err(StoreError::from)?.next().is_some() {
                return Err(DownloadServiceError::InvalidNativeCheckpoint);
            }
            std::fs::create_dir_all(&output).map_err(StoreError::from)?;
            if !authorized() { return Err(DownloadServiceError::InvalidUriOption("authorization")); }
            let mut aria_options = options.clone().into_aria2_options()?;
            aria_options.insert("dir".into(), output.to_str().ok_or(DownloadServiceError::InvalidUriOption("torrent directory"))?.into());
            aria_options.insert("gid".into(), gid.clone());
            aria_options.insert("pause".into(), "true".into());
            aria_options.insert("select-file".into(), selection_text.clone());
            aria_options.insert("allow-overwrite".into(), "false".into());
            aria_options.insert("continue".into(), "false".into());
            aria_options.insert("auto-file-renaming".into(), "false".into());
            aria_options.insert("check-integrity".into(), "true".into());
            // Agent confirmation requests a download, not indefinite seeding.
            aria_options.insert("seed-time".into(), "0".into());
            let returned = self.add_torrent_for_task(task_id, metadata.torrent.clone(), Vec::new(), aria_options).await?;
            if returned != gid {
                let _ = self.api().force_remove(&returned).await;
                return Err(DownloadServiceError::InvalidNativeCheckpoint);
            }
            self.tell_status_for_task(task_id, &gid).await?
        };
        if !torrent_files_match(&status, metadata, &output) {
            let _ = self.api().force_pause(&gid).await;
            return Err(DownloadServiceError::InvalidNativeCheckpoint);
        }
        let selected_matches = status.files.iter().all(|file| {
            file.index.parse::<u32>().is_ok_and(|index| (file.selected == "true") == selection.contains(&index))
        });
        if !selected_matches {
            self.api().force_pause(&gid).await?;
            let mut selected = Aria2Options::new();
            selected.insert("select-file".into(), selection_text);
            self.api().change_option(&gid, selected).await?;
            status = self.tell_status_for_task(task_id, &gid).await?;
            if !torrent_files_match(&status, metadata, &output)
                || status.files.iter().any(|file| file.index.parse::<u32>().map_or(true,
                    |index| (file.selected == "true") != selection.contains(&index))) {
                return Err(DownloadServiceError::InvalidNativeCheckpoint);
            }
        }
        let latest = self.store.find(task_id)?;
        if latest.is_private || !authorized()
            || matches!(latest.status, TaskStatus::Paused | TaskStatus::Cancelled | TaskStatus::Recycled) {
            let _ = self.api().force_pause(&gid).await;
            return if authorized() { Ok(latest) } else { Err(DownloadServiceError::InvalidUriOption("authorization")) };
        }
        let paused = status.status == "paused";
        let mut snapshot = TransferSnapshot::try_from(status)?;
        if paused { snapshot.status = TaskStatus::Queued; }
        task = self.persist_snapshot(latest, snapshot)?;
        self.store.set_file_selection(task_id, indices)?;
        self.store.confirm_selection(task_id)?;
        self.api().save_session().await?;
        let latest = self.store.find(task_id)?;
        if latest.is_private || !authorized()
            || matches!(latest.status, TaskStatus::Paused | TaskStatus::Cancelled | TaskStatus::Recycled) {
            let _ = self.api().force_pause(&gid).await;
            return if authorized() { Ok(latest) } else { Err(DownloadServiceError::InvalidUriOption("authorization")) };
        }
        if paused { self.unpause_gid(&gid).await?; }
        self.api().save_session().await?;
        // Keep the durable queued intent if tellStatus is uncertain. The
        // journal retries the same GID and observes the accepted unpause.
        let final_status = self.tell_status_for_task(task_id, &gid).await?;
        if !authorized() {
            let _ = self.api().force_pause(&gid).await;
            return Err(DownloadServiceError::InvalidUriOption("authorization"));
        }
        if !torrent_files_match(&final_status, metadata, &output) {
            let _ = self.api().force_pause(&gid).await;
            return Err(DownloadServiceError::InvalidNativeCheckpoint);
        }
        self.persist_snapshot(task, TransferSnapshot::try_from(final_status)?)
    }
}

fn torrent_files_match(status: &Aria2Status, metadata: &crate::cli_bt::BtPreparation, output: &Path) -> bool {
    status.files.len() == metadata.files.len() && metadata.files.iter().all(|expected| {
        status.files.iter().filter(|actual| actual.index.parse::<u32>().ok() == Some(expected.index)).count() == 1
            && status.files.iter().any(|actual| actual.index.parse::<u32>().ok() == Some(expected.index)
                && Path::new(&actual.path) == output.join(&expected.path)
                && actual.length.parse::<u64>().ok() == Some(expected.length))
    })
}
