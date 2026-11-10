use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

#[cfg(test)]
#[path = "store_share_membership_tests.rs"]
mod share_membership_tests;

use crate::{
    cloud::CloudProvider,
    models::{
        may_transition, CloudDirectoryGroup, CloudDirectoryGroupItem, DownloadTask, FtpSegment,
        NewCloudDirectoryGroup, NewCloudDirectoryGroupItem, NewTask, TaskStatus,
    },
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("database mutex poisoned")]
    Poisoned,
    #[error("task {0} was not found")]
    NotFound(Uuid),
    #[error("invalid task status stored in database")]
    InvalidStoredStatus,
    #[error("cannot transition task from {from:?} to {to:?}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
    #[error("value for {field} is outside SQLite's signed 64-bit range")]
    IntegerOutOfRange { field: &'static str },
    #[error("invalid cloud directory group data: {0}")]
    InvalidCloudDirectoryGroupData(&'static str),
    #[error("invalid FTP segment data: {0}")]
    InvalidFtpSegmentData(&'static str),
    #[error("file operation failed: {0}")]
    Io(#[from] std::io::Error),
}

pub struct TaskStore {
    connection: Mutex<Connection>,
    leaf_gates: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
}

impl TaskStore {
    pub(crate) fn native_stream_state_path(&self) -> Option<std::path::PathBuf> {
        let connection = self.connection.lock().ok()?;
        let path = Path::new(connection.path()?);
        Some(path.parent()?.join("aria2-next-state/stream/state.db"))
    }

    /// Publish a share leaf only together with its durable directory membership.
    /// Share resolution has not selected an account yet, so this does not bind one.
    pub(crate) fn prepare_cloud_share_directory_leaf_task(
        &self,
        group: Uuid,
        item: Uuid,
        mut new_task: NewTask,
        retry_task: Option<Uuid>,
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let group_is_private: Option<bool> = transaction
            .query_row(
                "SELECT g.is_private FROM cloud_directory_groups g
             JOIN cloud_directory_group_items i ON i.group_id = g.id
             WHERE g.id = ?1 AND i.id = ?2
             AND g.status NOT IN ('recycled', 'cancelled', 'completed')
             AND (i.task_id IS NULL OR i.task_id = ?3)",
                params![
                    group.to_string(),
                    item.to_string(),
                    retry_task.map(|id| id.to_string())
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(group_is_private) = group_is_private else {
            return Err(StoreError::NotFound(item));
        };
        // The group is the durable privacy authority for every leaf. Reading
        // it inside this transaction prevents a just-converted directory from
        // later creating a leaf with the stale caller-provided flag.
        new_task.is_private = group_is_private;
        let task = if let Some(id) = retry_task {
            let current = find_with_connection(&transaction, id)?;
            let linked_elsewhere: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM cloud_directory_group_items
                 WHERE task_id = ?1 AND (group_id != ?2 OR id != ?3))",
                params![id.to_string(), group.to_string(), item.to_string()],
                |row| row.get(0),
            )?;
            if linked_elsewhere
                || current.source != new_task.source
                || current.kind != "cloud"
                || current.aria_gid.is_some()
                || !matches!(current.status, TaskStatus::Parsing | TaskStatus::Failed)
            {
                return Err(StoreError::InvalidCloudDirectoryGroupData(
                    "ineligible share retry task",
                ));
            }
            transaction.execute(
                "UPDATE tasks SET status = 'parsing', error = NULL, failure_code = NULL,
                 is_private = ?1, updated_at = ?2 WHERE id = ?3",
                params![new_task.is_private, current_timestamp(), id.to_string()],
            )?;
            find_with_connection(&transaction, id)?
        } else {
            Self::create_with_connection(&transaction, new_task)?
        };
        transaction.execute(
            "UPDATE cloud_directory_group_items SET task_id = ?1, failure_code = NULL,
             failure_message = NULL WHERE id = ?2 AND group_id = ?3",
            params![task.id.to_string(), item.to_string(), group.to_string()],
        )?;
        transaction.execute(
            "UPDATE cloud_directory_groups SET updated_at = ?1 WHERE id = ?2",
            params![current_timestamp(), group.to_string()],
        )?;
        transaction.commit()?;
        Ok(task)
    }

    /// Run once at application startup, before any directory submission workers.
    /// Engine recovery only sees linked tasks: unsubmitted directory leaves have
    /// no GID and their submission loop does not survive a process exit.
    pub fn recover_interrupted_cloud_directories(&self) -> Result<usize, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(connection.execute(
            "UPDATE cloud_directory_groups SET status = 'paused', updated_at = ?1
             WHERE status IN ('queued', 'parsing', 'downloading') AND EXISTS (
                 SELECT 1 FROM cloud_directory_group_items i
                 LEFT JOIN tasks t ON t.id = i.task_id
                 WHERE i.group_id = cloud_directory_groups.id AND (
                     (i.task_id IS NULL AND i.failure_code IS NULL AND i.failure_message IS NULL)
                     OR (t.aria_gid IS NULL AND t.status IN ('queued', 'parsing', 'downloading', 'paused'))
                 )
             )",
            [current_timestamp()],
        )?)
    }

    /// Cloud tasks persist an expiring provider URL in their download backend.
    /// Before that backend session is loaded, pause every active stable cloud
    /// source and every task whose signed URL was rejected behind the shared
    /// authorization check. The original GID and checkpoint remain available
    /// for native continuation after the provider session and signed URL have
    /// been refreshed.
    pub fn defer_startup_cloud_downloads(&self) -> Result<Vec<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT id FROM tasks
             WHERE (source LIKE 'cloud://%' OR source LIKE 'cloud-share://%')
               AND (status IN ('queued', 'downloading') OR
                    (status = 'parsing' AND aria_gid IS NULL) OR
                    (status = 'failed' AND aria_error_code IN ('6', '8', '22', '24')))",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let now = current_timestamp();
        for id in &ids {
            connection.execute(
                "UPDATE tasks SET status = 'paused', error = NULL,
                 status_message = ?1,
                 failure_code = 'authorization_checking',
                 download_speed = 0,
                 upload_speed = 0, connections = 0, eta_seconds = NULL,
                 updated_at = ?2 WHERE id = ?3",
                params!["正在检查网盘登录状态", now, id],
            )?;
        }
        ids.into_iter()
            .map(|id| {
                Uuid::parse_str(&id)
                    .map_err(|_| StoreError::InvalidStoredStatus)
                    .and_then(|id| find_with_connection(&connection, id))
            })
            .collect()
    }

    pub fn set_startup_cloud_authorization_recovering(
        &self,
        provider: CloudProvider,
    ) -> Result<Vec<DownloadTask>, StoreError> {
        self.update_startup_cloud_authorization(
            provider,
            "authorization_recovering",
            None,
            Some("正在自动恢复网盘登录"),
        )
    }

    pub fn set_startup_cloud_authorization_required(
        &self,
        provider: CloudProvider,
    ) -> Result<Vec<DownloadTask>, StoreError> {
        self.update_startup_cloud_authorization(
            provider,
            "authorization_pending",
            Some("恢复网盘登录后继续下载"),
            None,
        )
    }

    pub fn set_startup_cloud_authorization_check_failed(
        &self,
        provider: CloudProvider,
        failure_code: &str,
        message: &str,
    ) -> Result<Vec<DownloadTask>, StoreError> {
        self.update_startup_cloud_authorization(provider, failure_code, Some(message), None)
    }

    fn update_startup_cloud_authorization(
        &self,
        provider: CloudProvider,
        failure_code: &str,
        error: Option<&str>,
        status_message: Option<&str>,
    ) -> Result<Vec<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let direct = format!("cloud://{}/%", provider.key());
        let share = format!("cloud-share://{}/%", provider.key());
        connection.execute(
            "UPDATE tasks SET failure_code = ?1, error = ?2, status_message = ?3,
             updated_at = ?4 WHERE status = 'paused'
             AND failure_code IN ('authorization_checking', 'authorization_recovering')
             AND (source LIKE ?5 OR source LIKE ?6)",
            params![
                failure_code,
                error,
                status_message,
                current_timestamp(),
                direct,
                share
            ],
        )?;
        let mut statement = connection.prepare(&format!(
            "{TASK_SELECT} WHERE status = 'paused' AND failure_code = ?1
             AND (source LIKE ?2 OR source LIKE ?3)"
        ))?;
        let tasks = statement
            .query_map(params![failure_code, direct, share], task_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    pub fn claim_cloud_directory_leaf_task(
        &self,
        group: Uuid,
        item: Uuid,
        provider: CloudProvider,
        account: &str,
        mut task: NewTask,
    ) -> Result<Option<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let group_is_private: Option<bool> = transaction.query_row("SELECT g.is_private FROM cloud_directory_groups g JOIN cloud_directory_group_items i ON i.group_id = g.id WHERE g.id = ?1 AND i.id = ?2 AND g.provider = ?3 AND g.status NOT IN ('recycled', 'cancelled', 'completed') AND i.task_id IS NULL AND NOT EXISTS (SELECT 1 FROM cloud_download_accounts a WHERE a.id = g.id AND (a.provider != ?3 OR a.account_id != ?4))", params![group.to_string(), item.to_string(), provider.key(), account], |row| row.get(0)).optional()?;
        let Some(group_is_private) = group_is_private else {
            return Ok(None);
        };
        // See the share-leaf path above: unsubmitted leaves inherit the
        // group's persisted value rather than an outdated submission closure.
        task.is_private = group_is_private;
        let task = Self::create_with_connection(&transaction, task)?;
        transaction.execute(
            "INSERT INTO cloud_download_accounts (id, provider, account_id) VALUES (?1, ?2, ?3)",
            params![task.id.to_string(), provider.key(), account],
        )?;
        transaction.execute("UPDATE cloud_directory_group_items SET task_id = ?1, failure_code = NULL, failure_message = NULL WHERE id = ?2 AND group_id = ?3 AND task_id IS NULL", params![task.id.to_string(), item.to_string(), group.to_string()])?;
        transaction.commit()?;
        Ok(Some(task))
    }

    pub async fn lock_cloud_directory_leaf(
        &self,
        item: Uuid,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, StoreError> {
        let gate = self
            .leaf_gates
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .entry(item)
            .or_default()
            .clone();
        Ok(gate.lock_owned().await)
    }
    /// Bind once, before submission. A later account replacement cannot adopt old downloads.
    pub fn bind_cloud_download_account(
        &self,
        id: Uuid,
        provider: CloudProvider,
        account: &str,
    ) -> Result<(), StoreError> {
        if account.is_empty() {
            return Err(StoreError::InvalidCloudDirectoryGroupData(
                "missing account",
            ));
        }
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute("INSERT OR IGNORE INTO cloud_download_accounts (id, provider, account_id) VALUES (?1, ?2, ?3)", params![id.to_string(), provider.key(), account])?;
        let matches: bool = connection.query_row(
            "SELECT provider = ?2 AND account_id = ?3 FROM cloud_download_accounts WHERE id = ?1",
            params![id.to_string(), provider.key(), account],
            |row| row.get(0),
        )?;
        if !matches {
            return Err(StoreError::InvalidCloudDirectoryGroupData(
                "download belongs to another account",
            ));
        }
        Ok(())
    }

    pub fn cloud_download_account(&self, id: Uuid) -> Result<Option<String>, StoreError> {
        use rusqlite::OptionalExtension;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT account_id FROM cloud_download_accounts WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Discover from durable failure state, never from a process-local pending list.
    /// Unknown legacy owners are deliberately excluded from automatic recovery.
    pub fn reauthorized_cloud_downloads(
        &self,
        provider: CloudProvider,
        account: &str,
    ) -> Result<(Vec<DownloadTask>, Vec<(Uuid, Uuid)>), StoreError> {
        let (task_ids, leaves) = {
            let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
            let mut statement = connection.prepare("SELECT t.id FROM tasks t JOIN cloud_download_accounts a ON a.id = t.id WHERE a.provider = ?1 AND a.account_id = ?2 AND ((t.status = 'failed' AND t.failure_code IN ('not_authorized', 'credentials_expired', 'web_auth_required')) OR (t.status = 'paused' AND t.failure_code IN ('authorization_checking', 'authorization_recovering', 'authorization_pending'))) AND (t.source LIKE ?3 OR t.source LIKE ?4) AND NOT EXISTS (SELECT 1 FROM cloud_directory_group_items i JOIN cloud_directory_groups g ON g.id = i.group_id WHERE i.task_id = t.id AND g.status IN ('cancelled', 'recycled', 'completed'))")?;
            let task_ids = statement
                .query_map(
                    params![
                        provider.key(),
                        account,
                        format!("cloud://{}/%", provider.key()),
                        format!("cloud-share://{}/%", provider.key())
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let mut statement = connection.prepare("SELECT g.id, i.id FROM cloud_directory_group_items i JOIN cloud_directory_groups g ON g.id = i.group_id JOIN cloud_download_accounts a ON a.id = g.id WHERE a.provider = ?1 AND g.provider = ?1 AND a.account_id = ?2 AND i.task_id IS NULL AND i.failure_code IN ('not_authorized', 'credentials_expired', 'web_auth_required', 'authorization_pending') AND g.status NOT IN ('cancelled', 'recycled', 'completed')")?;
            let leaves = statement
                .query_map(params![provider.key(), account], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (task_ids, leaves)
        };
        let parse = |id: &str| Uuid::parse_str(id).map_err(|_| StoreError::InvalidStoredStatus);
        Ok((
            task_ids
                .iter()
                .map(|id| self.find(parse(id)?))
                .collect::<Result<_, _>>()?,
            leaves
                .iter()
                .map(|(group, item)| Ok((parse(group)?, parse(item)?)))
                .collect::<Result<_, StoreError>>()?,
        ))
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open(Connection::open_in_memory()?)
    }

    pub fn open_path(path: &std::path::Path) -> Result<Self, StoreError> {
        Self::open(Connection::open(path)?)
    }

    fn open(mut connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY NOT NULL,
                source TEXT NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                progress INTEGER NOT NULL DEFAULT 0,
                error TEXT
            );",
        )?;
        migrate(&mut connection)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS cloud_download_accounts (id TEXT PRIMARY KEY NOT NULL, provider TEXT NOT NULL, account_id TEXT NOT NULL);")?;
        Ok(Self {
            connection: Mutex::new(connection),
            leaf_gates: Mutex::new(HashMap::new()),
        })
    }

    pub fn create(&self, new_task: NewTask) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        Self::create_with_connection(&connection, new_task)
    }

    fn create_with_connection(
        connection: &Connection,
        new_task: NewTask,
    ) -> Result<DownloadTask, StoreError> {
        let created_at = current_timestamp();
        let task = DownloadTask {
            id: Uuid::new_v4(),
            source: new_task.source,
            name: new_task.name,
            kind: new_task.kind,
            status: TaskStatus::Parsing,
            progress: 0,
            error: None,
            status_message: None,
            failure_code: None,
            aria_gid: None,
            followed_by_gid: None,
            metadata_gid: None,
            total_length: None,
            completed_length: 0,
            download_speed: 0,
            upload_speed: 0,
            connections: 0,
            discovered_nodes: 0,
            eta_seconds: None,
            aria_error_code: None,
            created_at,
            updated_at: created_at,
            completed_at: None,
            p2p_sharing: false,
            is_private: new_task.is_private,
            post_processing_note: None,
        };
        connection.execute("INSERT INTO tasks (id, source, name, kind, status, progress, error, created_at, updated_at, is_private) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![task.id.to_string(), task.source, task.name, task.kind, task.status.as_db(), task.progress, task.error, task.created_at, task.updated_at, task.is_private])?;
        Ok(task)
    }

    pub fn create_cloud_directory_group(
        &self,
        new_group: NewCloudDirectoryGroup,
        new_items: &[NewCloudDirectoryGroupItem],
    ) -> Result<CloudDirectoryGroup, StoreError> {
        validate_new_cloud_directory_group(&new_group)?;
        for item in new_items {
            validate_new_cloud_directory_group_item(item)?;
        }

        let timestamp = current_timestamp();
        let group = CloudDirectoryGroup {
            id: Uuid::new_v4(),
            provider: new_group.provider,
            remote_directory_id: new_group.remote_directory_id,
            relative_directory: new_group.relative_directory,
            output_directory: new_group.output_directory,
            status: TaskStatus::Queued,
            created_at: timestamp,
            updated_at: timestamp,
            is_private: new_group.is_private,
            post_processing_note: None,
        };
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO cloud_directory_groups (id, provider, remote_directory_id, relative_directory, output_directory, status, created_at, updated_at, is_private)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                group.id.to_string(),
                group.provider.key(),
                group.remote_directory_id,
                group.relative_directory,
                group.output_directory.to_string_lossy(),
                group.status.as_db(),
                group.created_at,
                group.updated_at,
                group.is_private,
            ],
        )?;
        for (item_index, new_item) in new_items.iter().enumerate() {
            let item_index = u32::try_from(item_index)
                .map_err(|_| StoreError::InvalidCloudDirectoryGroupData("too many group items"))?;
            transaction.execute(
                "INSERT INTO cloud_directory_group_items (id, group_id, item_index, remote_file_id, relative_directory, output_name, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    Uuid::new_v4().to_string(),
                    group.id.to_string(),
                    item_index,
                    new_item.remote_file_id,
                    serde_json::to_string(&new_item.relative_directory)
                        .map_err(|_| StoreError::InvalidCloudDirectoryGroupData("relative directory"))?,
                    new_item.output_name,
                    new_item.size.map(|value| sqlite_i64("size", value)).transpose()?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(group)
    }

    pub fn cloud_directory_group(
        &self,
        id: Uuid,
    ) -> Result<Option<CloudDirectoryGroup>, StoreError> {
        validate_uuid(id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                &format!("{CLOUD_DIRECTORY_GROUP_SELECT} WHERE id = ?1"),
                [id.to_string()],
                cloud_directory_group_from_row,
            )
            .map(Some)
            .or_else(no_rows_to_none)
    }

    pub fn cloud_directory_groups(&self) -> Result<Vec<CloudDirectoryGroup>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(&format!(
            "{CLOUD_DIRECTORY_GROUP_SELECT} ORDER BY created_at, id"
        ))?;
        let groups = statement
            .query_map([], cloud_directory_group_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(groups)
    }

    /// Membership and transfer rows must belong to the same SQLite snapshot.
    /// During submission a new link must never be projected with an older list
    /// of tasks, otherwise a real member briefly appears missing with zero bytes.
    pub(crate) fn cloud_directory_snapshot(
        &self,
        id: Uuid,
    ) -> Result<
        Option<(
            CloudDirectoryGroup,
            Vec<CloudDirectoryGroupItem>,
            Vec<DownloadTask>,
        )>,
        StoreError,
    > {
        use rusqlite::OptionalExtension;
        validate_uuid(id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let group = transaction
            .query_row(
                &format!("{CLOUD_DIRECTORY_GROUP_SELECT} WHERE id = ?1"),
                [id.to_string()],
                cloud_directory_group_from_row,
            )
            .optional()?;
        let Some(group) = group else {
            return Ok(None);
        };
        let items = {
            let mut statement = transaction.prepare(
                "SELECT id, group_id, item_index, remote_file_id, relative_directory, output_name, size, task_id, failure_code, failure_message
                 FROM cloud_directory_group_items WHERE group_id = ?1 ORDER BY item_index",
            )?;
            let rows =
                statement.query_map([id.to_string()], cloud_directory_group_item_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let tasks = {
            let mut statement = transaction.prepare(&format!(
                "{TASK_SELECT} WHERE id IN (SELECT task_id FROM cloud_directory_group_items WHERE group_id = ?1)"
            ))?;
            let rows = statement.query_map([id.to_string()], task_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        transaction.commit()?;
        Ok(Some((group, items, tasks)))
    }

    pub fn set_cloud_directory_group_private(
        &self,
        id: Uuid,
        is_private: bool,
    ) -> Result<CloudDirectoryGroup, StoreError> {
        validate_uuid(id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let timestamp = current_timestamp();
        let changed = transaction.execute(
            "UPDATE cloud_directory_groups SET is_private = ?1, updated_at = ?2 WHERE id = ?3",
            params![is_private, timestamp, id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        // Keep every already-created leaf in lockstep with the synthetic
        // group root. Future leaves read the group field when they are claimed.
        transaction.execute(
            "UPDATE tasks SET is_private = ?1, updated_at = ?2
             WHERE id IN (
                SELECT task_id FROM cloud_directory_group_items
                WHERE group_id = ?3 AND task_id IS NOT NULL
             )",
            params![is_private, timestamp, id.to_string()],
        )?;
        let group = transaction.query_row(
            &format!("{CLOUD_DIRECTORY_GROUP_SELECT} WHERE id = ?1"),
            [id.to_string()],
            cloud_directory_group_from_row,
        )?;
        transaction.commit()?;
        Ok(group)
    }

    pub fn set_cloud_directory_group_post_processing_note(
        &self,
        id: Uuid,
        note: Option<&str>,
    ) -> Result<CloudDirectoryGroup, StoreError> {
        validate_uuid(id)?;
        let note = note.map(str::trim).filter(|value| !value.is_empty());
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE cloud_directory_groups SET post_processing_note = ?1, updated_at = ?2 WHERE id = ?3",
            params![note, current_timestamp(), id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        connection
            .query_row(
                &format!("{CLOUD_DIRECTORY_GROUP_SELECT} WHERE id = ?1"),
                [id.to_string()],
                cloud_directory_group_from_row,
            )
            .map_err(StoreError::from)
    }

    pub fn cloud_directory_group_items(
        &self,
        group_id: Uuid,
    ) -> Result<Vec<CloudDirectoryGroupItem>, StoreError> {
        validate_uuid(group_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT id, group_id, item_index, remote_file_id, relative_directory, output_name, size, task_id, failure_code, failure_message
             FROM cloud_directory_group_items WHERE group_id = ?1 ORDER BY item_index",
        )?;
        let items = statement
            .query_map([group_id.to_string()], cloud_directory_group_item_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    pub(crate) fn cloud_directory_group_item_is_linked(
        &self,
        group_id: Uuid,
        item_id: Uuid,
    ) -> Result<bool, StoreError> {
        validate_uuid(group_id)?;
        validate_uuid(item_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                "SELECT task_id IS NOT NULL FROM cloud_directory_group_items
                 WHERE group_id = ?1 AND id = ?2",
                params![group_id.to_string(), item_id.to_string()],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub(crate) fn cloud_directory_group_item_task(
        &self,
        group_id: Uuid,
        item_id: Uuid,
    ) -> Result<Option<DownloadTask>, StoreError> {
        validate_uuid(group_id)?;
        validate_uuid(item_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                &format!(
                    "{TASK_SELECT} WHERE id = (
                        SELECT task_id FROM cloud_directory_group_items
                        WHERE group_id = ?1 AND id = ?2
                    )"
                ),
                params![group_id.to_string(), item_id.to_string()],
                task_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn recycle_cloud_directory_group(
        &self,
        group_id: Uuid,
    ) -> Result<CloudDirectoryGroup, StoreError> {
        validate_uuid(group_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        // Shares the connection lock with leaf claim: a committed Parsing task
        // reserves the parent until engine acceptance/failure has been recorded.
        let active: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM cloud_directory_group_items i JOIN tasks t ON t.id = i.task_id WHERE i.group_id = ?1 AND t.status NOT IN ('completed', 'failed', 'cancelled', 'recycled'))", [group_id.to_string()], |row| row.get(0))?;
        if active {
            return Err(StoreError::InvalidCloudDirectoryGroupData(
                "directory still has active downloads",
            ));
        }
        let changed = connection.execute(
            "UPDATE cloud_directory_groups SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                TaskStatus::Recycled.as_db(),
                current_timestamp(),
                group_id.to_string()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(group_id));
        }
        connection
            .query_row(
                &format!("{CLOUD_DIRECTORY_GROUP_SELECT} WHERE id = ?1"),
                [group_id.to_string()],
                cloud_directory_group_from_row,
            )
            .map_err(StoreError::from)
    }

    /// Records cancellation before waiting for an in-flight leaf submission.
    /// No local task or payload file is removed here.
    pub fn cancel_cloud_directory_group(&self, group_id: Uuid) -> Result<(), StoreError> {
        validate_uuid(group_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE cloud_directory_groups SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND status <> ?4",
            params![
                TaskStatus::Cancelled.as_db(),
                current_timestamp(),
                group_id.to_string(),
                TaskStatus::Recycled.as_db()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(group_id));
        }
        Ok(())
    }

    /// Durable user intent, independent of currently linked/running children.
    pub fn set_cloud_directory_paused(
        &self,
        group_id: Uuid,
        paused: bool,
    ) -> Result<(), StoreError> {
        validate_uuid(group_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE cloud_directory_groups SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND status NOT IN ('cancelled', 'recycled', 'completed')",
            params![
                if paused {
                    TaskStatus::Paused.as_db()
                } else {
                    TaskStatus::Queued.as_db()
                },
                current_timestamp(),
                group_id.to_string()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(group_id));
        }
        Ok(())
    }

    pub fn link_cloud_directory_group_item_task(
        &self,
        group_id: Uuid,
        item_id: Uuid,
        task_id: Uuid,
    ) -> Result<(), StoreError> {
        validate_uuid(group_id)?;
        validate_uuid(item_id)?;
        validate_uuid(task_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE cloud_directory_group_items
             SET task_id = ?1, failure_code = NULL, failure_message = NULL
             WHERE id = ?2 AND group_id = ?3 AND (task_id IS NULL OR task_id = ?1)",
            params![
                task_id.to_string(),
                item_id.to_string(),
                group_id.to_string()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(item_id));
        }
        transaction.execute(
            "UPDATE cloud_directory_groups SET updated_at = ?1 WHERE id = ?2",
            params![current_timestamp(), group_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn fail_cloud_directory_group_item(
        &self,
        group_id: Uuid,
        item_id: Uuid,
        failure_code: impl AsRef<str>,
        failure_message: impl AsRef<str>,
    ) -> Result<(), StoreError> {
        validate_uuid(group_id)?;
        validate_uuid(item_id)?;
        validate_plain_text(failure_code.as_ref(), "failure code")?;
        validate_plain_text(failure_message.as_ref(), "failure message")?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE cloud_directory_group_items
             SET failure_code = ?1, failure_message = ?2
             WHERE id = ?3 AND group_id = ?4 AND task_id IS NULL",
            params![
                failure_code.as_ref(),
                failure_message.as_ref(),
                item_id.to_string(),
                group_id.to_string()
            ],
        )?;
        if changed == 0 {
            let exists: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM cloud_directory_group_items WHERE id = ?1 AND group_id = ?2)", params![item_id.to_string(), group_id.to_string()], |row| row.get(0))?;
            if !exists {
                return Err(StoreError::NotFound(item_id));
            }
        }
        transaction.execute(
            "UPDATE cloud_directory_groups SET updated_at = ?1 WHERE id = ?2",
            params![current_timestamp(), group_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn cloud_directory_group_for_task(
        &self,
        task_id: Uuid,
    ) -> Result<Option<CloudDirectoryGroup>, StoreError> {
        validate_uuid(task_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                &format!(
                    "{CLOUD_DIRECTORY_GROUP_SELECT} WHERE id = (
                        SELECT group_id FROM cloud_directory_group_items
                        WHERE task_id = ?1 LIMIT 1
                    )"
                ),
                [task_id.to_string()],
                cloud_directory_group_from_row,
            )
            .map(Some)
            .or_else(no_rows_to_none)
    }

    /// Deletes a durable directory group and its virtual member records.
    /// Callers must remove every linked local task first so an incomplete
    /// lifecycle cannot silently detach a still-visible download.
    pub fn remove_cloud_directory_group(&self, id: Uuid) -> Result<(), StoreError> {
        validate_uuid(id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "DELETE FROM cloud_directory_groups WHERE id = ?1",
            [id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        Ok(())
    }

    pub fn transition(&self, id: Uuid, next: TaskStatus) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let current = find_with_connection(&connection, id)?;
        if !may_transition(current.status, next) {
            return Err(StoreError::InvalidTransition {
                from: current.status,
                to: next,
            });
        }
        let progress = if next == TaskStatus::Completed {
            100
        } else {
            current.progress
        };
        let completed_at = if next == TaskStatus::Completed {
            current.completed_at.or(Some(current_timestamp()))
        } else {
            current.completed_at
        };
        connection.execute(
            "UPDATE tasks SET status = ?1, progress = ?2, completed_at = ?3 WHERE id = ?4",
            params![next.as_db(), progress, completed_at, id.to_string()],
        )?;
        Ok(DownloadTask {
            status: next,
            progress,
            completed_at,
            ..current
        })
    }

    pub fn list(&self) -> Result<Vec<DownloadTask>, StoreError> {
        self.list_matching("")
    }

    /// Discards private history during the macOS prerelease credential reset.
    /// Runs before the engine starts; tombstones prevent session resurrection.
    /// This transaction never opens or deletes downloaded payload files.
    #[cfg(any(target_os = "macos", test))]
    pub fn reset_private_task_history(&self) -> Result<(), StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let selection = "is_private = 1 OR id IN (SELECT i.task_id FROM cloud_directory_group_items i JOIN cloud_directory_groups g ON g.id=i.group_id WHERE g.is_private=1)";
        let gids = {
            let mut query = transaction.prepare(&format!("SELECT aria_gid, metadata_gid, followed_by_gid FROM tasks WHERE {selection}"))?;
            let rows = query.query_map([], |row| Ok([row.get::<_, Option<String>>(0)?, row.get(1)?, row.get(2)?]))?;
            let mut gids = Vec::new();
            for row in rows { gids.extend(row?.into_iter().flatten()); }
            gids
        };
        remember_deleted_engine_gids(&transaction, gids)?;
        transaction.execute(&format!("DELETE FROM cloud_download_accounts WHERE id IN (SELECT id FROM tasks WHERE {selection}) OR id IN (SELECT id FROM cloud_directory_groups WHERE is_private=1)"), [])?;
        transaction.execute(&format!("DELETE FROM tasks WHERE {selection}"), [])?;
        transaction.execute("DELETE FROM cloud_directory_groups WHERE is_private=1", [])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn has_private_tasks(&self) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM tasks WHERE is_private = 1
                    UNION ALL
                    SELECT 1 FROM cloud_directory_groups WHERE is_private = 1
                )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|exists| exists != 0)
            .map_err(StoreError::from)
    }

    pub fn set_task_private(&self, id: Uuid, is_private: bool) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET is_private = ?1, updated_at = ?2 WHERE id = ?3",
            params![is_private, current_timestamp(), id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    /// Compatibility wrapper for existing creation and confirmation flows.
    /// New callers should use the explicit task-scoped name.
    pub fn set_private(&self, id: Uuid, is_private: bool) -> Result<DownloadTask, StoreError> {
        self.set_task_private(id, is_private)
    }

    pub fn set_post_processing_note(
        &self,
        id: Uuid,
        note: Option<&str>,
    ) -> Result<DownloadTask, StoreError> {
        let note = note.map(str::trim).filter(|value| !value.is_empty());
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET post_processing_note = ?1, updated_at = ?2 WHERE id = ?3",
            params![note, current_timestamp(), id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    pub fn list_active(&self) -> Result<Vec<DownloadTask>, StoreError> {
        self.list_matching("WHERE status NOT IN ('completed', 'failed', 'cancelled', 'recycled')")
    }

    pub fn list_terminal(&self) -> Result<Vec<DownloadTask>, StoreError> {
        self.list_matching("WHERE status IN ('completed', 'failed', 'cancelled', 'recycled')")
    }

    fn list_matching(&self, filter: &str) -> Result<Vec<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement =
            connection.prepare(&format!("{TASK_SELECT} {filter} ORDER BY rowid DESC"))?;
        let rows = statement.query_map([], task_from_row)?;
        let tasks = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(tasks)
    }

    pub fn find(&self, id: Uuid) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        find_with_connection(&connection, id)
    }

    /// Finds the most recently created task for an identical stable source.
    /// Cloud tasks persist only `cloud://provider/file-id`, so this never
    /// compares an expiring signed download URL or a session credential.
    pub fn find_by_source(&self, source: &str) -> Result<Option<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                &format!("{TASK_SELECT} WHERE source = ?1 ORDER BY rowid DESC LIMIT 1"),
                [source],
                task_from_row,
            )
            .map(Some)
            .or_else(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(StoreError::Database(error))
                }
            })
    }

    /// Returns the output path reserved for this task before its download is
    /// submitted. Reservations make independently sourced downloads choose
    /// different local names even before aria2 has created a payload file.
    pub fn reserved_output_path(&self, id: Uuid) -> Result<Option<std::path::PathBuf>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                "SELECT path FROM task_output_reservations WHERE task_id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|path| path.map(std::path::PathBuf::from))
            .map_err(StoreError::from)
    }

    /// Claims a concrete output path for a task. The comparison deliberately
    /// follows Windows' case-insensitive file-name behavior, because Windows
    /// is a supported target and the same payload directory can be shared by
    /// HTTP, FTP, and cloud downloads.
    pub fn reserve_output_path(&self, id: Uuid, path: &Path) -> Result<bool, StoreError> {
        let path = path.to_string_lossy().into_owned();
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        if find_with_transaction(&transaction, id)?.is_none() {
            return Err(StoreError::NotFound(id));
        }
        let current = transaction
            .query_row(
                "SELECT path FROM task_output_reservations WHERE task_id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(current) = current {
            return Ok(current.eq_ignore_ascii_case(&path));
        }
        let occupied = transaction
            .query_row(
                "SELECT 1 FROM task_output_reservations WHERE path = ?1 COLLATE NOCASE LIMIT 1",
                [&path],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if occupied {
            return Ok(false);
        }
        transaction.execute(
            "INSERT INTO task_output_reservations (task_id, path) VALUES (?1, ?2)",
            params![id.to_string(), path],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn rename_task(&self, id: Uuid, name: &str) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, current_timestamp(), id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    /// Removes only the durable task record. `task_files` rows cascade, while
    /// the actual downloaded payload is never touched by this operation.
    pub fn remove_task_record(&self, id: Uuid) -> Result<(), StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let gids = transaction
            .query_row(
                "SELECT aria_gid, metadata_gid, followed_by_gid FROM tasks WHERE id = ?1",
                [id.to_string()],
                |row| Ok([row.get::<_, Option<String>>(0)?, row.get(1)?, row.get(2)?]),
            )
            .optional()?;
        let Some(gids) = gids else {
            return Err(StoreError::NotFound(id));
        };
        remember_deleted_engine_gids(&transaction, gids.into_iter().flatten())?;
        let changed = transaction.execute("DELETE FROM tasks WHERE id = ?1", [id.to_string()])?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Removes a collection of durable task records in one SQLite transaction.
    /// All IDs are validated before any row is deleted, so a stale selection
    /// cannot leave a partial purge behind. `task_files` rows cascade, while
    /// the actual downloaded payload is never touched by this operation.
    pub fn remove_task_records(&self, ids: &[Uuid]) -> Result<(), StoreError> {
        const SQLITE_ID_BATCH_SIZE: usize = 900;

        let mut seen = HashSet::with_capacity(ids.len());
        let ids = ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        let id_strings = ids.iter().map(Uuid::to_string).collect::<Vec<_>>();
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let mut found = HashSet::with_capacity(ids.len());
        let mut gids = HashSet::new();

        for batch in id_strings.chunks(SQLITE_ID_BATCH_SIZE) {
            let placeholders = std::iter::repeat("?")
                .take(batch.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("SELECT id, aria_gid, metadata_gid, followed_by_gid FROM tasks WHERE id IN ({placeholders})");
            let mut statement = transaction.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(batch.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    [row.get::<_, Option<String>>(1)?, row.get(2)?, row.get(3)?],
                ))
            })?;
            for row in rows {
                let (id, task_gids) = row?;
                found.insert(id);
                gids.extend(task_gids.into_iter().flatten());
            }
        }
        if let Some(missing) = ids.iter().find(|id| !found.contains(&id.to_string())) {
            return Err(StoreError::NotFound(*missing));
        }

        remember_deleted_engine_gids(&transaction, gids)?;
        for batch in id_strings.chunks(SQLITE_ID_BATCH_SIZE) {
            let placeholders = std::iter::repeat("?")
                .take(batch.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("DELETE FROM tasks WHERE id IN ({placeholders})");
            transaction.execute(&sql, params_from_iter(batch.iter()))?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn was_engine_gid_deleted(&self, gid: &str) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM deleted_engine_gids WHERE gid = ?1)",
            [gid],
            |row| row.get(0),
        )?)
    }

    pub fn fail(&self, id: Uuid, message: impl Into<String>) -> Result<DownloadTask, StoreError> {
        self.fail_with_code(id, None, message)
    }

    /// Stores a durable, machine-readable failure kind alongside the safe
    /// text displayed to the user. A missing code deliberately clears an old
    /// cloud-auth classification left by an earlier attempt.
    pub fn fail_with_code(
        &self,
        id: Uuid,
        failure_code: Option<&str>,
        message: impl Into<String>,
    ) -> Result<DownloadTask, StoreError> {
        let mut task = self.transition(id, TaskStatus::Failed)?;
        let message = message.into();
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "UPDATE tasks SET error = ?1, failure_code = ?2 WHERE id = ?3",
            params![message, failure_code, id.to_string()],
        )?;
        task.error = Some(message);
        task.failure_code = failure_code.map(str::to_owned);
        Ok(task)
    }

    /// A cloud task keeps its old GID only as a native continuation checkpoint
    /// while a provider resolves a replacement URL.  Do not let a resolver or
    /// submission failure turn that stale URL back into an engine-owned task.
    pub fn fail_cloud_url_refresh(
        &self,
        id: Uuid,
        message: impl Into<String>,
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET status = 'failed', error = ?1,
             failure_code = 'cloud_url_refresh_failed',
             status_message = NULL, download_speed = 0, upload_speed = 0,
             connections = 0, eta_seconds = NULL, updated_at = ?2
             WHERE id = ?3 AND status = 'queued'
             AND failure_code = 'cloud_url_refreshing'",
            params![message.into(), current_timestamp(), id.to_string()],
        )?;
        if changed == 0 {
            return find_with_connection(&connection, id).and_then(|task| {
                if matches!(task.status, TaskStatus::Cancelled | TaskStatus::Recycled) {
                    Ok(task)
                } else {
                    Err(StoreError::Database(rusqlite::Error::QueryReturnedNoRows))
                }
            });
        }
        find_with_connection(&connection, id)
    }

    /// Cleanup may finish after a user action. Update only its safe message,
    /// never restore an old status or transfer snapshot over the current task.
    pub(crate) fn set_completed_cleanup_message(
        &self,
        id: Uuid,
        pending: bool,
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let message =
            pending.then_some("cleanup_pending：下载已完成，网盘转存文件清理未完成，可重试清理");
        connection.execute(
            "UPDATE tasks SET error = ?1 WHERE id = ?2 AND status = 'completed' AND (?1 IS NOT NULL OR substr(error, 1, length('cleanup_pending：')) = 'cleanup_pending：')",
            params![message, id.to_string()],
        )?;
        find_with_connection(&connection, id)
    }

    pub(crate) fn note_download_route(
        &self,
        id: Uuid,
        gid: &str,
        message: &str,
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "UPDATE tasks SET status_message = ?1 WHERE id = ?2 AND aria_gid = ?3
             AND status IN ('parsing', 'queued', 'downloading', 'completed') AND status_message IS NULL",
            params![message, id.to_string(), gid],
        )?;
        find_with_connection(&connection, id)
    }

    /// A resume can replace the binding between reading the queue and clicking
    /// cancel. Only retire the paused row that still owns the observed GID.
    pub(crate) fn cancel_paused_task_if_bound(
        &self,
        id: Uuid,
        gid: &str,
    ) -> Result<Option<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET status='cancelled', status_message=NULL, error=NULL, failure_code=NULL,
             aria_error_code=NULL, download_speed=0, upload_speed=0, eta_seconds=NULL, updated_at=?3
             WHERE id=?1 AND aria_gid=?2 AND status='paused'",
            params![id.to_string(), gid, current_timestamp()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        find_with_connection(&connection, id).map(Some)
    }

    pub fn bind_gid(&self, id: Uuid, gid: impl AsRef<str>) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        update_gid(&connection, id, gid.as_ref(), None)?;
        find_with_connection(&connection, id)
    }

    /// An aria2 session can lose its in-memory GID list after an interrupted
    /// restart. Keep the durable task, its file selection, and its payload so
    /// the user can explicitly resume it with a fresh engine submission.
    pub fn detach_missing_engine_task(&self, id: Uuid) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET status='paused', error=?1, failure_code=NULL, aria_gid=NULL,
             followed_by_gid=NULL, metadata_gid=NULL, download_speed=0, upload_speed=0,
             connections=0, discovered_nodes=0, eta_seconds=NULL, aria_error_code=NULL,
             updated_at=?2 WHERE id=?3",
            params![
                "下载引擎未恢复该任务；点击开始可继续下载",
                current_timestamp(),
                id.to_string()
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    /// A completed download is historical user state, even when the engine
    /// later rechecks a moved payload as incomplete. Disown the old GID and
    /// prevent it from being imported as an external task on the next start.
    pub fn retire_completed_engine_task(
        &self,
        id: Uuid,
        observed_gids: impl IntoIterator<Item = String>,
    ) -> Result<DownloadTask, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let task = find_with_transaction(&transaction, id)?.ok_or(StoreError::NotFound(id))?;
        if task.status != TaskStatus::Completed {
            return Err(StoreError::InvalidTransition {
                from: task.status,
                to: TaskStatus::Completed,
            });
        }
        let gids = [task.aria_gid, task.metadata_gid, task.followed_by_gid]
            .into_iter()
            .flatten()
            .chain(observed_gids);
        remember_deleted_engine_gids(&transaction, gids)?;
        transaction.execute(
            "UPDATE tasks SET aria_gid = NULL, metadata_gid = NULL,
             followed_by_gid = NULL, p2p_sharing = 0, download_speed = 0,
             upload_speed = 0, connections = 0, discovered_nodes = 0,
             eta_seconds = NULL, updated_at = ?2 WHERE id = ?1",
            params![id.to_string(), current_timestamp()],
        )?;
        transaction.commit()?;
        find_with_connection(&connection, id)
    }

    /// Records an aria2 GID accepted for an explicit retry without creating a
    /// second task. Output-file rows intentionally remain in place so a later
    /// retry can still decide whether it may resume or needs overwrite consent.
    pub fn rebind_for_resubmission(
        &self,
        id: Uuid,
        gid: impl AsRef<str>,
    ) -> Result<DownloadTask, StoreError> {
        let gid = gid.as_ref();
        if gid.is_empty() {
            return Err(StoreError::Database(rusqlite::Error::InvalidParameterName(
                "empty aria2 gid".into(),
            )));
        }
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current = find_with_transaction(&transaction, id)?.ok_or(StoreError::NotFound(id))?;
        if !matches!(
            current.status,
            TaskStatus::Paused | TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Completed
        ) && !(current.status == TaskStatus::Queued && current.aria_gid.is_none())
        {
            return Err(StoreError::InvalidTransition {
                from: current.status,
                to: TaskStatus::Parsing,
            });
        }
        transaction.execute(
            "UPDATE tasks SET status='parsing',progress=0,error=NULL,failure_code=NULL,aria_gid=?1,followed_by_gid=NULL,
             metadata_gid=NULL,total_length=NULL,completed_length=0,download_speed=0,upload_speed=0,
             connections=0,discovered_nodes=0,eta_seconds=NULL,aria_error_code=NULL,selection_confirmed=0,completed_at=NULL,updated_at=?2
             WHERE id=?3",
            params![gid, current_timestamp(), id.to_string()],
        )?;
        remember_deleted_engine_gids(
            &transaction,
            [
                current.aria_gid,
                current.metadata_gid,
                current.followed_by_gid,
            ]
            .into_iter()
            .flatten()
            .filter(|old_gid| old_gid != gid),
        )?;
        transaction.commit()?;
        find_with_connection(&connection, id)
    }

    /// Atomically promotes a newly accepted provider URL over the stale GID
    /// that was retained solely for native continuation.  Unlike generic
    /// resubmission this intentionally preserves the old progress projection
    /// until aria2 has reported the replacement GID.
    pub fn replace_cloud_url_gid(
        &self,
        id: Uuid,
        expected_gid: &str,
        replacement_gid: &str,
    ) -> Result<DownloadTask, StoreError> {
        if expected_gid.is_empty() || replacement_gid.is_empty() {
            return Err(StoreError::Database(rusqlite::Error::InvalidParameterName(
                "empty aria2 gid".into(),
            )));
        }
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current = find_with_transaction(&transaction, id)?.ok_or(StoreError::NotFound(id))?;
        let changed = transaction.execute(
            "UPDATE tasks SET status = 'parsing', error = NULL, status_message = NULL,
             failure_code = NULL, aria_gid = ?1, followed_by_gid = NULL,
             metadata_gid = NULL, download_speed = 0, upload_speed = 0,
             connections = 0, discovered_nodes = 0, eta_seconds = NULL,
             aria_error_code = NULL, selection_confirmed = 0,
             completed_at = NULL, updated_at = ?2
             WHERE id = ?3 AND aria_gid = ?4 AND status = 'queued'
             AND failure_code = 'cloud_url_refreshing'",
            params![
                replacement_gid,
                current_timestamp(),
                id.to_string(),
                expected_gid
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Database(rusqlite::Error::QueryReturnedNoRows));
        }
        remember_deleted_engine_gids(
            &transaction,
            [
                current.aria_gid,
                current.metadata_gid,
                current.followed_by_gid,
            ]
            .into_iter()
            .flatten()
            .filter(|old_gid| old_gid != replacement_gid),
        )?;
        transaction.commit()?;
        find_with_connection(&connection, id)
    }

    /// Atomically claims an aria2 GID that could not be cleaned up after submission.
    /// This keeps recovery from importing the same live session entry as a second task.
    pub fn bind_gid_and_fail(
        &self,
        id: Uuid,
        gid: impl AsRef<str>,
        message: impl AsRef<str>,
    ) -> Result<DownloadTask, StoreError> {
        self.bind_gid_and_fail_with_code(id, gid, message, None)
    }

    pub(crate) fn bind_gid_and_fail_with_code(
        &self,
        id: Uuid,
        gid: impl AsRef<str>,
        message: impl AsRef<str>,
        failure_code: Option<&str>,
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET aria_gid=?1,followed_by_gid=NULL,metadata_gid=NULL,status='failed',error=?2,failure_code=?5,
             download_speed=0,upload_speed=0,eta_seconds=NULL,selection_confirmed=0,updated_at=?3
             WHERE id=?4",
            params![
                gid.as_ref(),
                message.as_ref(),
                current_timestamp(),
                id.to_string(),
                failure_code
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    pub fn bind_metadata_gid(&self, id: Uuid, gid: &str) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed=connection.execute("UPDATE tasks SET aria_gid=?1,metadata_gid=?1,status='awaiting_user',selection_confirmed=0,updated_at=?2 WHERE id=?3",params![gid,current_timestamp(),id.to_string()])?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    pub fn bind_awaiting_gid(&self, id: Uuid, gid: &str) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed=connection.execute("UPDATE tasks SET aria_gid=?1,metadata_gid=NULL,status='awaiting_user',selection_confirmed=0,updated_at=?2 WHERE id=?3",params![gid,current_timestamp(),id.to_string()])?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        find_with_connection(&connection, id)
    }

    /// An acknowledged BT confirmation is application intent, independent of the
    /// last successful aria2 transfer snapshot and durable across service restarts.
    pub fn confirm_selection(&self, id: Uuid) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE tasks SET selection_confirmed=1 WHERE id=?1",
            [id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id));
        }
        Ok(())
    }

    pub fn selection_confirmed(&self, id: Uuid) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        find_with_connection(&connection, id)?;
        Ok(connection.query_row(
            "SELECT selection_confirmed FROM tasks WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn replace_gid(
        &self,
        id: Uuid,
        expected_gid: impl AsRef<str>,
        replacement_gid: impl AsRef<str>,
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE tasks SET aria_gid = ?1, followed_by_gid = NULL, updated_at = ?2 WHERE id = ?3 AND aria_gid = ?4",
            params![replacement_gid.as_ref(), current_timestamp(), id.to_string(), expected_gid.as_ref()],
        )?;
        if changed == 0 {
            return Err(if find_with_transaction(&transaction, id)?.is_none() {
                StoreError::NotFound(id)
            } else {
                StoreError::Database(rusqlite::Error::QueryReturnedNoRows)
            });
        }
        transaction.commit()?;
        find_with_connection(&connection, id)
    }

    pub fn apply_transfer_snapshot(
        &self,
        snapshot: DownloadTask,
    ) -> Result<DownloadTask, StoreError> {
        self.apply_transfer_snapshot_with_files(snapshot, &[])
    }

    pub fn apply_transfer_snapshot_with_files(
        &self,
        snapshot: DownloadTask,
        files: &[(u32, String, u64, u64, bool)],
    ) -> Result<DownloadTask, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let current = find_with_transaction(&transaction, snapshot.id)?
            .ok_or(StoreError::NotFound(snapshot.id))?;
        let completed_at = if snapshot.status == TaskStatus::Completed {
            current.completed_at.or(Some(current_timestamp()))
        } else {
            current.completed_at
        };
        let changed = transaction.execute(
            "UPDATE tasks SET name = ?1, status = ?2, progress = ?3, error = ?4, failure_code = ?5, aria_gid = ?6, followed_by_gid = ?7,
             total_length = ?8, completed_length = ?9, download_speed = ?10, upload_speed = ?11,
             connections = ?12, discovered_nodes = ?13, eta_seconds = ?14, aria_error_code = ?15, updated_at = ?16, completed_at = ?17, p2p_sharing = ?18, status_message = ?19 WHERE id = ?20",
            params![
                snapshot.name, snapshot.status.as_db(), snapshot.progress, snapshot.error, snapshot.failure_code, snapshot.aria_gid,
                snapshot.followed_by_gid, snapshot.total_length.map(|value| sqlite_i64("total_length", value)).transpose()?,
                sqlite_i64("completed_length", snapshot.completed_length)?, sqlite_i64("download_speed", snapshot.download_speed)?,
                sqlite_i64("upload_speed", snapshot.upload_speed)?, snapshot.connections, snapshot.discovered_nodes,
                snapshot.eta_seconds.map(|value| sqlite_i64("eta_seconds", value)).transpose()?,
                snapshot.aria_error_code, current_timestamp(), completed_at, snapshot.p2p_sharing, snapshot.status_message, snapshot.id.to_string(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(snapshot.id));
        }
        for (index, path, length, completed_length, selected) in files {
            transaction.execute(
                "INSERT INTO task_files (task_id,file_index,path,length,completed_length,selected) VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(task_id,file_index) DO UPDATE SET path=excluded.path,length=excluded.length,completed_length=excluded.completed_length,selected=excluded.selected",
                params![snapshot.id.to_string(), index, path, sqlite_i64("length", *length)?, sqlite_i64("completed_length", *completed_length)?, selected],
            )?;
        }
        transaction.commit()?;
        find_with_connection(&connection, snapshot.id)
    }

    pub fn upsert_files(
        &self,
        id: Uuid,
        files: &[(u32, String, u64, u64, bool)],
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        if find_with_transaction(&transaction, id)?.is_none() {
            return Err(StoreError::NotFound(id));
        }
        for (index, path, length, completed_length, selected) in files {
            transaction.execute(
                "INSERT INTO task_files (task_id, file_index, path, length, completed_length, selected)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(task_id, file_index) DO UPDATE SET path = excluded.path, length = excluded.length,
                 completed_length = excluded.completed_length, selected = excluded.selected",
                params![id.to_string(), index, path, sqlite_i64("length", *length)?, sqlite_i64("completed_length", *completed_length)?, selected],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Persist the user's intended BT file selection while a magnet metadata
    /// request is still resolving to its real download GID.
    pub fn set_file_selection(&self, id: Uuid, indices: &[u32]) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        if find_with_transaction(&transaction, id)?.is_none() {
            return Err(StoreError::NotFound(id));
        }
        transaction.execute(
            "UPDATE task_files SET selected = 0 WHERE task_id = ?1",
            [id.to_string()],
        )?;
        for index in indices {
            let changed = transaction.execute(
                "UPDATE task_files SET selected = 1 WHERE task_id = ?1 AND file_index = ?2",
                params![id.to_string(), index],
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound(id));
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn find_by_gid(&self, gid: impl AsRef<str>) -> Result<Option<DownloadTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                &format!("{TASK_SELECT} WHERE aria_gid = ?1 OR metadata_gid = ?1 LIMIT 1"),
                [gid.as_ref()],
                task_from_row,
            )
            .map(Some)
            .or_else(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(StoreError::Database(error))
                }
            })
    }

    pub fn files(&self, id: Uuid) -> Result<Vec<crate::aria2::TransferFile>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        find_with_connection(&connection, id)?;
        let mut statement = connection.prepare("SELECT file_index,path,length,completed_length,selected FROM task_files WHERE task_id=?1 ORDER BY file_index")?;
        let rows = statement.query_map([id.to_string()], |row| {
            Ok(crate::aria2::TransferFile {
                index: row.get(0)?,
                path: row.get(1)?,
                length: required_u64(row, 2)?,
                completed_length: required_u64(row, 3)?,
                selected: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn replace_ftp_segments(
        &self,
        id: Uuid,
        segments: &[FtpSegment],
    ) -> Result<(), StoreError> {
        validate_ftp_segments(segments)?;
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        if find_with_transaction(&transaction, id)?.is_none() {
            return Err(StoreError::NotFound(id));
        }
        transaction.execute(
            "DELETE FROM ftp_segments WHERE task_id = ?1",
            [id.to_string()],
        )?;
        for segment in segments {
            transaction.execute(
                "INSERT INTO ftp_segments (task_id, segment_index, start_offset, end_offset, completed_length)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.to_string(),
                    segment.index,
                    sqlite_i64("start_offset", segment.start_offset)?,
                    sqlite_i64("end_offset", segment.end_offset)?,
                    sqlite_i64("completed_length", segment.completed_length)?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn ftp_segments(&self, id: Uuid) -> Result<Vec<FtpSegment>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT segment_index, start_offset, end_offset, completed_length
             FROM ftp_segments WHERE task_id = ?1 ORDER BY segment_index",
        )?;
        let rows = statement.query_map([id.to_string()], |row| {
            Ok(FtpSegment {
                index: row.get(0)?,
                start_offset: required_u64(row, 1)?,
                end_offset: required_u64(row, 2)?,
                completed_length: required_u64(row, 3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn update_ftp_segment(
        &self,
        id: Uuid,
        index: u32,
        completed_length: u64,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let segment = transaction
            .query_row(
                "SELECT start_offset, end_offset FROM ftp_segments WHERE task_id = ?1 AND segment_index = ?2",
                params![id.to_string(), index],
                |row| Ok((required_u64(row, 0)?, required_u64(row, 1)?)),
            )
            .optional()?;
        let Some((start_offset, end_offset)) = segment else {
            return Err(if find_with_transaction(&transaction, id)?.is_none() {
                StoreError::NotFound(id)
            } else {
                StoreError::InvalidFtpSegmentData("segment does not exist")
            });
        };
        if completed_length > end_offset.saturating_sub(start_offset) {
            return Err(StoreError::InvalidFtpSegmentData(
                "completed length exceeds the segment range",
            ));
        }
        transaction.execute(
            "UPDATE ftp_segments SET completed_length = ?3 WHERE task_id = ?1 AND segment_index = ?2",
            params![
                id.to_string(),
                index,
                sqlite_i64("completed_length", completed_length)?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn clear_ftp_segments(&self, id: Uuid) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "DELETE FROM ftp_segments WHERE task_id = ?1",
            [id.to_string()],
        )?;
        Ok(())
    }
}

const TASK_SELECT: &str = "SELECT id, source, name, kind, status, progress, error, failure_code, aria_gid, followed_by_gid, total_length, completed_length, download_speed, upload_speed, connections, discovered_nodes, eta_seconds, aria_error_code, updated_at, completed_at, metadata_gid, created_at, is_private, post_processing_note, p2p_sharing, status_message FROM tasks";
const CLOUD_DIRECTORY_GROUP_SELECT: &str = "SELECT id, provider, remote_directory_id, relative_directory, output_directory, status, created_at, updated_at, is_private, post_processing_note FROM cloud_directory_groups";

fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let transaction = connection.transaction()?;
    if version < 7 {
        for (name, definition) in [
            ("aria_gid", "TEXT"),
            ("followed_by_gid", "TEXT"),
            ("total_length", "INTEGER"),
            ("completed_length", "INTEGER NOT NULL DEFAULT 0"),
            ("download_speed", "INTEGER NOT NULL DEFAULT 0"),
            ("upload_speed", "INTEGER NOT NULL DEFAULT 0"),
            ("connections", "INTEGER NOT NULL DEFAULT 0"),
            ("eta_seconds", "INTEGER"),
            ("aria_error_code", "TEXT"),
            ("failure_code", "TEXT"),
            ("updated_at", "INTEGER NOT NULL DEFAULT 0"),
            ("completed_at", "INTEGER"),
            ("selection_confirmed", "INTEGER NOT NULL DEFAULT 0"),
            ("metadata_gid", "TEXT"),
            ("created_at", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !column_exists(&transaction, "tasks", name)? {
                transaction.execute_batch(&format!(
                    "ALTER TABLE tasks ADD COLUMN {name} {definition};"
                ))?;
            }
        }
        transaction.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS tasks_aria_gid_unique ON tasks(aria_gid) WHERE aria_gid IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS tasks_metadata_gid_unique ON tasks(metadata_gid) WHERE metadata_gid IS NOT NULL;
         CREATE TABLE IF NOT EXISTS task_files (
            task_id TEXT NOT NULL,
            file_index INTEGER NOT NULL,
            path TEXT NOT NULL,
            length INTEGER NOT NULL,
            completed_length INTEGER NOT NULL,
            selected INTEGER NOT NULL,
            PRIMARY KEY(task_id, file_index),
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS task_output_reservations (
            task_id TEXT PRIMARY KEY NOT NULL,
            path TEXT NOT NULL COLLATE NOCASE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE
         );
         CREATE UNIQUE INDEX IF NOT EXISTS task_output_reservations_path_unique
            ON task_output_reservations(path COLLATE NOCASE);
         CREATE TABLE IF NOT EXISTS cloud_directory_groups (
            id TEXT PRIMARY KEY NOT NULL,
            provider TEXT NOT NULL,
            remote_directory_id TEXT NOT NULL,
            relative_directory TEXT NOT NULL,
            output_directory TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'queued',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            post_processing_note TEXT
         );
         CREATE TABLE IF NOT EXISTS cloud_directory_group_items (
            id TEXT PRIMARY KEY NOT NULL,
            group_id TEXT NOT NULL,
            item_index INTEGER NOT NULL,
            remote_file_id TEXT NOT NULL,
            relative_directory TEXT NOT NULL,
            output_name TEXT NOT NULL,
            size INTEGER,
            task_id TEXT,
            failure_code TEXT,
            failure_message TEXT,
            FOREIGN KEY(group_id) REFERENCES cloud_directory_groups(id) ON DELETE CASCADE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE SET NULL,
            UNIQUE(group_id, item_index)
         );
         CREATE INDEX IF NOT EXISTS cloud_directory_group_items_task_id ON cloud_directory_group_items(task_id) WHERE task_id IS NOT NULL;
         UPDATE tasks SET created_at = updated_at WHERE created_at = 0;
         UPDATE tasks SET completed_at = updated_at WHERE status = 'completed' AND completed_at IS NULL AND updated_at > 0;",
        )?;
    }
    if !column_exists(&transaction, "cloud_directory_groups", "status")? {
        transaction.execute_batch(
            "ALTER TABLE cloud_directory_groups ADD COLUMN status TEXT NOT NULL DEFAULT 'queued';",
        )?;
    }
    if !column_exists(&transaction, "tasks", "is_private")? {
        transaction
            .execute_batch("ALTER TABLE tasks ADD COLUMN is_private INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !column_exists(&transaction, "tasks", "discovered_nodes")? {
        transaction.execute_batch(
            "ALTER TABLE tasks ADD COLUMN discovered_nodes INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !column_exists(&transaction, "tasks", "post_processing_note")? {
        transaction.execute_batch("ALTER TABLE tasks ADD COLUMN post_processing_note TEXT;")?;
    }
    if !column_exists(&transaction, "tasks", "p2p_sharing")? {
        transaction.execute_batch(
            "ALTER TABLE tasks ADD COLUMN p2p_sharing INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !column_exists(&transaction, "tasks", "status_message")? {
        transaction.execute_batch("ALTER TABLE tasks ADD COLUMN status_message TEXT;")?;
    }
    if !column_exists(&transaction, "cloud_directory_groups", "is_private")? {
        transaction.execute_batch(
            "ALTER TABLE cloud_directory_groups ADD COLUMN is_private INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !column_exists(
        &transaction,
        "cloud_directory_groups",
        "post_processing_note",
    )? {
        transaction.execute_batch(
            "ALTER TABLE cloud_directory_groups ADD COLUMN post_processing_note TEXT;",
        )?;
    }
    if version < 10 {
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS ftp_segments (
                task_id TEXT NOT NULL,
                segment_index INTEGER NOT NULL,
                start_offset INTEGER NOT NULL,
                end_offset INTEGER NOT NULL,
                completed_length INTEGER NOT NULL,
                PRIMARY KEY(task_id, segment_index),
                FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE
            );",
        )?;
    }
    // `task_output_reservations` was originally created as part of the
    // pre-v7 migration. Databases that had already recorded that version
    // before the table was introduced therefore never received it. The
    // reservation is required before aria2 accepts a new URI, so repair the
    // independently-created table on every open instead of depending on an
    // unrelated schema version boundary.
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_output_reservations (
            task_id TEXT PRIMARY KEY NOT NULL,
            path TEXT NOT NULL COLLATE NOCASE,
            FOREIGN KEY(task_id) REFERENCES tasks(id) ON DELETE CASCADE
         );
         CREATE UNIQUE INDEX IF NOT EXISTS task_output_reservations_path_unique
            ON task_output_reservations(path COLLATE NOCASE);",
    )?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS deleted_engine_gids (
            gid TEXT PRIMARY KEY NOT NULL
         );
         PRAGMA user_version = 15;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn remember_deleted_engine_gids(
    transaction: &Transaction<'_>,
    gids: impl IntoIterator<Item = String>,
) -> Result<(), StoreError> {
    for gid in gids {
        if !gid.is_empty() && !crate::xunlei_native::is_native_gid(Some(&gid)) {
            transaction.execute(
                "INSERT OR IGNORE INTO deleted_engine_gids (gid) VALUES (?1)",
                [gid],
            )?;
        }
    }
    Ok(())
}

fn column_exists(
    transaction: &Transaction<'_>,
    table: &str,
    name: &str,
) -> Result<bool, StoreError> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(columns
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|column| column == name))
}

fn sqlite_i64(field: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::IntegerOutOfRange { field })
}

fn validate_ftp_segments(segments: &[FtpSegment]) -> Result<(), StoreError> {
    let mut seen = HashSet::with_capacity(segments.len());
    for segment in segments {
        if !seen.insert(segment.index) {
            return Err(StoreError::InvalidFtpSegmentData("duplicate segment index"));
        }
        if segment.start_offset >= segment.end_offset {
            return Err(StoreError::InvalidFtpSegmentData("empty or inverted range"));
        }
        if segment.completed_length > segment.end_offset - segment.start_offset {
            return Err(StoreError::InvalidFtpSegmentData(
                "completed length exceeds the segment range",
            ));
        }
        sqlite_i64("start_offset", segment.start_offset)?;
        sqlite_i64("end_offset", segment.end_offset)?;
        sqlite_i64("completed_length", segment.completed_length)?;
    }
    Ok(())
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn update_gid(
    connection: &Connection,
    id: Uuid,
    gid: &str,
    followed_by_gid: Option<&str>,
) -> Result<(), StoreError> {
    let changed = connection.execute(
        "UPDATE tasks SET aria_gid = ?1, followed_by_gid = ?2, metadata_gid = NULL, updated_at = ?3 WHERE id = ?4",
        params![gid, followed_by_gid, current_timestamp(), id.to_string()],
    )?;
    if changed == 0 {
        return Err(StoreError::NotFound(id));
    }
    Ok(())
}

fn find_with_connection(connection: &Connection, id: Uuid) -> Result<DownloadTask, StoreError> {
    connection
        .query_row(
            &format!("{TASK_SELECT} WHERE id = ?1"),
            [id.to_string()],
            task_from_row,
        )
        .map_err(|error| {
            if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                StoreError::NotFound(id)
            } else {
                StoreError::Database(error)
            }
        })
}

fn find_with_transaction(
    transaction: &Transaction<'_>,
    id: Uuid,
) -> Result<Option<DownloadTask>, StoreError> {
    transaction
        .query_row(
            &format!("{TASK_SELECT} WHERE id = ?1"),
            [id.to_string()],
            task_from_row,
        )
        .map(Some)
        .or_else(|error| {
            if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(StoreError::Database(error))
            }
        })
}

fn task_from_row(row: &Row<'_>) -> rusqlite::Result<DownloadTask> {
    let id: String = row.get(0)?;
    let status: String = row.get(4)?;
    Ok(DownloadTask {
        id: Uuid::parse_str(&id).map_err(|_| rusqlite::Error::InvalidQuery)?,
        source: row.get(1)?,
        name: row.get(2)?,
        kind: row.get(3)?,
        status: TaskStatus::from_db(&status).ok_or(rusqlite::Error::InvalidQuery)?,
        progress: row.get(5)?,
        error: row.get(6)?,
        status_message: row.get(25)?,
        failure_code: row.get(7)?,
        aria_gid: row.get(8)?,
        followed_by_gid: row.get(9)?,
        total_length: optional_u64(row, 10)?,
        completed_length: required_u64(row, 11)?,
        download_speed: required_u64(row, 12)?,
        upload_speed: required_u64(row, 13)?,
        connections: row.get(14)?,
        discovered_nodes: row.get(15)?,
        eta_seconds: optional_u64(row, 16)?,
        aria_error_code: row.get(17)?,
        updated_at: row.get(18)?,
        completed_at: row.get(19)?,
        metadata_gid: row.get(20)?,
        created_at: row.get(21)?,
        is_private: row.get(22)?,
        post_processing_note: row.get(23)?,
        p2p_sharing: row.get(24)?,
    })
}

fn optional_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| u64::try_from(value).map_err(|_| rusqlite::Error::InvalidQuery))
        .transpose()
}

fn required_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn cloud_directory_group_from_row(row: &Row<'_>) -> rusqlite::Result<CloudDirectoryGroup> {
    let id: String = row.get(0)?;
    let provider: String = row.get(1)?;
    let status: String = row.get(5)?;
    Ok(CloudDirectoryGroup {
        id: parse_cloud_directory_uuid(&id)?,
        provider: cloud_provider_from_db(&provider)?,
        remote_directory_id: row.get(2)?,
        relative_directory: row.get(3)?,
        output_directory: std::path::PathBuf::from(row.get::<_, String>(4)?),
        status: TaskStatus::from_db(&status).ok_or(rusqlite::Error::InvalidQuery)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        is_private: row.get(8)?,
        post_processing_note: row.get(9)?,
    })
}

fn cloud_directory_group_item_from_row(row: &Row<'_>) -> rusqlite::Result<CloudDirectoryGroupItem> {
    let id: String = row.get(0)?;
    let group_id: String = row.get(1)?;
    let relative_directory: String = row.get(4)?;
    let task_id: Option<String> = row.get(7)?;
    Ok(CloudDirectoryGroupItem {
        id: parse_cloud_directory_uuid(&id)?,
        group_id: parse_cloud_directory_uuid(&group_id)?,
        item_index: row.get(2)?,
        remote_file_id: row.get(3)?,
        relative_directory: serde_json::from_str(&relative_directory)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        output_name: row.get(5)?,
        size: optional_u64(row, 6)?,
        task_id: task_id
            .as_deref()
            .map(parse_cloud_directory_uuid)
            .transpose()?,
        failure_code: row.get(8)?,
        failure_message: row.get(9)?,
    })
}

fn parse_cloud_directory_uuid(value: &str) -> rusqlite::Result<Uuid> {
    let id = Uuid::parse_str(value).map_err(|_| rusqlite::Error::InvalidQuery)?;
    if id.is_nil() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(id)
}

fn cloud_provider_from_db(value: &str) -> rusqlite::Result<CloudProvider> {
    match value {
        "quark" => Ok(CloudProvider::Quark),
        "pikpak" => Ok(CloudProvider::Pikpak),
        "baidu" => Ok(CloudProvider::Baidu),
        "tianyi" => Ok(CloudProvider::Tianyi),
        "aliyun" => Ok(CloudProvider::Aliyun),
        "xunlei" => Ok(CloudProvider::Xunlei),
        "uc" => Ok(CloudProvider::Uc),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn no_rows_to_none<T>(error: rusqlite::Error) -> Result<Option<T>, StoreError> {
    if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
        Ok(None)
    } else {
        Err(StoreError::Database(error))
    }
}

fn validate_uuid(id: Uuid) -> Result<(), StoreError> {
    if id.is_nil() {
        return Err(StoreError::InvalidCloudDirectoryGroupData("nil UUID"));
    }
    Ok(())
}

fn validate_new_cloud_directory_group(group: &NewCloudDirectoryGroup) -> Result<(), StoreError> {
    validate_plain_text(&group.remote_directory_id, "remote directory ID")?;
    validate_path_component(&group.relative_directory)?;
    if group.output_directory.as_os_str().is_empty() {
        return Err(StoreError::InvalidCloudDirectoryGroupData(
            "output directory",
        ));
    }
    Ok(())
}

fn validate_new_cloud_directory_group_item(
    item: &NewCloudDirectoryGroupItem,
) -> Result<(), StoreError> {
    validate_plain_text(&item.remote_file_id, "remote file ID")?;
    validate_path_component(&item.output_name)?;
    for component in &item.relative_directory {
        validate_path_component(component)?;
    }
    Ok(())
}

fn validate_plain_text(value: &str, field: &'static str) -> Result<(), StoreError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(StoreError::InvalidCloudDirectoryGroupData(field));
    }
    Ok(())
}

fn validate_path_component(component: &str) -> Result<(), StoreError> {
    validate_plain_text(component, "path component")?;
    if matches!(component, "." | "..") || component.contains(['/', '\\']) {
        return Err(StoreError::InvalidCloudDirectoryGroupData("path component"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cloud::CloudProvider,
        models::{NewCloudDirectoryGroup, NewCloudDirectoryGroupItem},
    };

    #[test]
    fn migration_repairs_missing_output_reservation_table() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut connection = store.connection.lock().unwrap();
        connection
            .execute_batch(
                "DROP TABLE task_output_reservations;
                 PRAGMA user_version = 14;",
            )
            .unwrap();

        migrate(&mut connection).unwrap();

        let table_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'table' AND name = 'task_output_reservations'
                )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(table_exists);
    }

    #[test]
    fn startup_recovers_detached_cloud_submissions_for_every_provider_and_share_source() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let mut expected = Vec::new();
        {
            let store = TaskStore::open_path(&database).unwrap();
            for provider in crate::cloud::ProviderRegistry::global().catalog() {
                for scheme in ["cloud", "cloud-share"] {
                    for status in [
                        TaskStatus::Queued,
                        TaskStatus::Parsing,
                        TaskStatus::Downloading,
                    ] {
                        let task = store
                            .create(NewTask {
                                source: format!("{scheme}://{}/fixture", provider.provider.key()),
                                name: "partial.bin".into(),
                                kind: "cloud".into(),
                                is_private: false,
                            })
                            .unwrap();
                        let mut snapshot = task;
                        snapshot.status = status;
                        snapshot.completed_length = 153_694_208;
                        snapshot.total_length = Some(969_107_298);
                        snapshot.progress = 15;
                        snapshot.download_speed = 123;
                        snapshot.connections = 2;
                        let task = store.apply_transfer_snapshot(snapshot).unwrap();
                        store
                            .upsert_files(
                                task.id,
                                &[(
                                    0,
                                    "C:/Downloads/partial.bin".into(),
                                    969_107_298,
                                    153_694_208,
                                    true,
                                )],
                            )
                            .unwrap();
                        store
                            .bind_cloud_download_account(task.id, provider.provider, "owner")
                            .unwrap();
                        expected.push((task.id, scheme, provider.provider));
                    }
                }
            }
        }
        // Reopen the database with no in-memory worker or retry intent.
        let store = TaskStore::open_path(&database).unwrap();
        let recovered = store.defer_startup_cloud_downloads().unwrap();
        assert_eq!(recovered.len(), expected.len());
        for (id, _scheme, provider) in expected {
            let task = store.find(id).unwrap();
            assert_eq!(task.status, TaskStatus::Paused);
            assert_eq!(task.completed_length, 153_694_208);
            assert_eq!(task.total_length, Some(969_107_298));
            assert_eq!(task.progress, 15);
            assert!(task.aria_gid.is_none());
            assert_eq!(task.download_speed, 0);
            assert_eq!(task.connections, 0);
            assert_eq!(store.files(id).unwrap().len(), 1);
            let (ready, _) = store
                .reauthorized_cloud_downloads(provider, "owner")
                .unwrap();
            assert!(ready.iter().any(|candidate| candidate.id == id));
            assert_eq!(task.failure_code.as_deref(), Some("authorization_checking"));
            assert_eq!(task.status_message.as_deref(), Some("正在检查网盘登录状态"));
            assert!(task.error.is_none());
        }
        assert!(store.defer_startup_cloud_downloads().unwrap().is_empty());
    }

    #[test]
    fn startup_defers_expired_cloud_urls_for_every_provider() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut expected = Vec::new();
        for provider in crate::cloud::ProviderRegistry::global().catalog() {
            let gid = format!("expired-gid-{}", provider.provider.key());
            let task = store
                .create(NewTask::direct(format!(
                    "cloud://{}/expired-url",
                    provider.provider.key()
                )))
                .unwrap();
            let task = store
                .apply_transfer_snapshot(DownloadTask {
                    status: TaskStatus::Failed,
                    aria_gid: Some(gid.clone()),
                    aria_error_code: Some("24".into()),
                    completed_length: 42,
                    total_length: Some(100),
                    ..task
                })
                .unwrap();
            store
                .bind_cloud_download_account(task.id, provider.provider, "owner")
                .unwrap();
            expected.push((task.id, provider.provider, gid));
        }

        let recovered = store.defer_startup_cloud_downloads().unwrap();
        assert_eq!(recovered.len(), expected.len());
        for (id, provider, gid) in expected {
            let task = store.find(id).unwrap();
            assert_eq!(task.status, TaskStatus::Paused);
            assert_eq!(task.aria_gid.as_deref(), Some(gid.as_str()));
            assert_eq!(task.aria_error_code.as_deref(), Some("24"));
            assert_eq!(task.completed_length, 42);
            assert_eq!(task.failure_code.as_deref(), Some("authorization_checking"));
            assert!(store
                .reauthorized_cloud_downloads(provider, "owner")
                .unwrap()
                .0
                .iter()
                .any(|candidate| candidate.id == id));
        }
    }

    #[test]
    fn queued_cloud_dispatch_can_atomically_bind_a_replacement_gid() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://aliyun/file-id"))
            .unwrap();
        let queued = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Queued,
                ..task
            })
            .unwrap();

        let rebound = store
            .rebind_for_resubmission(queued.id, "replacement-gid")
            .unwrap();

        assert_eq!(rebound.status, TaskStatus::Parsing);
        assert_eq!(rebound.aria_gid.as_deref(), Some("replacement-gid"));
    }

    #[test]
    fn resubmission_retires_the_previous_engine_gid() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://baidu/file-id"))
            .unwrap();
        let paused = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Paused,
                aria_gid: Some("old-gid".into()),
                ..task
            })
            .unwrap();

        store.rebind_for_resubmission(paused.id, "new-gid").unwrap();

        assert!(store.was_engine_gid_deleted("old-gid").unwrap());
        assert!(!store.was_engine_gid_deleted("new-gid").unwrap());
        assert_eq!(
            store.find(paused.id).unwrap().aria_gid.as_deref(),
            Some("new-gid")
        );
    }

    #[test]
    fn cloud_url_replacement_switches_only_after_the_new_gid_is_accepted() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://aliyun/file-id"))
            .unwrap();
        let queued = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Queued,
                aria_gid: Some("0123456789abcdef".into()),
                failure_code: Some("cloud_url_refreshing".into()),
                aria_error_code: Some("24".into()),
                completed_length: 42,
                total_length: Some(100),
                ..task
            })
            .unwrap();

        let replaced = store
            .replace_cloud_url_gid(queued.id, "0123456789abcdef", "fedcba9876543210")
            .unwrap();

        assert_eq!(replaced.status, TaskStatus::Parsing);
        assert_eq!(replaced.aria_gid.as_deref(), Some("fedcba9876543210"));
        assert_eq!(replaced.completed_length, 42);
        assert_eq!(replaced.total_length, Some(100));
        assert!(replaced.failure_code.is_none());
        assert!(replaced.aria_error_code.is_none());
    }

    #[test]
    fn cloud_url_replacement_retires_the_previous_engine_gid() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("cloud://baidu/file-id"))
            .unwrap();
        let queued = store
            .apply_transfer_snapshot(DownloadTask {
                status: TaskStatus::Queued,
                aria_gid: Some("old-gid".into()),
                failure_code: Some("cloud_url_refreshing".into()),
                ..task
            })
            .unwrap();

        store
            .replace_cloud_url_gid(queued.id, "old-gid", "new-gid")
            .unwrap();

        assert!(store.was_engine_gid_deleted("old-gid").unwrap());
        assert!(!store.was_engine_gid_deleted("new-gid").unwrap());
        assert_eq!(
            store.find(queued.id).unwrap().aria_gid.as_deref(),
            Some("new-gid")
        );
    }

    #[test]
    fn startup_preserves_manual_pause_terminal_tasks_and_non_cloud_downloads() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut untouched = Vec::new();
        for scheme in ["cloud", "cloud-share"] {
            for status in [
                TaskStatus::Paused,
                TaskStatus::Completed,
                TaskStatus::Failed,
                TaskStatus::Cancelled,
                TaskStatus::Recycled,
            ] {
                let mut task = store
                    .create(NewTask::direct(format!("{scheme}://pikpak/fixture")))
                    .unwrap();
                task.status = status;
                task.error = Some("keep original message".into());
                untouched.push(store.apply_transfer_snapshot(task).unwrap());
            }
        }
        let mut direct = store
            .create(NewTask::direct("https://example.test/file"))
            .unwrap();
        direct.status = TaskStatus::Queued;
        untouched.push(store.apply_transfer_snapshot(direct).unwrap());
        assert!(store.defer_startup_cloud_downloads().unwrap().is_empty());
        for before in untouched {
            let after = store.find(before.id).unwrap();
            assert_eq!(after.status, before.status);
            assert_eq!(after.error, before.error);
            assert_eq!(after.aria_gid, before.aria_gid);
        }
    }

    #[test]
    fn startup_detached_directory_member_keeps_its_root_and_becomes_resumable() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(CloudProvider::Pikpak, "root", "Album", "C:/Downloads"),
                &[NewCloudDirectoryGroupItem::new(
                    "file",
                    &["Album"],
                    "partial.bin",
                    Some(100),
                )],
            )
            .unwrap();
        let item = store
            .cloud_directory_group_items(group.id)
            .unwrap()
            .remove(0);
        let mut task = store
            .create(NewTask::direct("cloud-share://pikpak/fixture"))
            .unwrap();
        task.status = TaskStatus::Queued;
        task.completed_length = 40;
        let task = store.apply_transfer_snapshot(task).unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, item.id, task.id)
            .unwrap();
        assert_eq!(store.recover_interrupted_cloud_directories().unwrap(), 1);
        store.defer_startup_cloud_downloads().unwrap();
        assert_eq!(
            store
                .cloud_directory_group(group.id)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::Paused
        );
        assert_eq!(
            store
                .cloud_directory_group_for_task(task.id)
                .unwrap()
                .unwrap()
                .id,
            group.id
        );
        assert_eq!(store.find(task.id).unwrap().status, TaskStatus::Paused);
        assert_eq!(store.find(task.id).unwrap().completed_length, 40);
    }

    #[test]
    fn startup_defers_active_cloud_tasks_for_silent_authorization_recovery() {
        let store = TaskStore::open_in_memory().unwrap();
        let cloud = store
            .create(NewTask {
                source: "cloud://quark/file-1".into(),
                name: "cloud.bin".into(),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        let mut cloud_snapshot = store.find(cloud.id).unwrap();
        cloud_snapshot.status = TaskStatus::Downloading;
        cloud_snapshot.aria_gid = Some("cloud-gid".into());
        store.apply_transfer_snapshot(cloud_snapshot).unwrap();

        let direct = store
            .create(NewTask::direct("https://example.test/file.bin"))
            .unwrap();
        let mut direct_snapshot = store.find(direct.id).unwrap();
        direct_snapshot.status = TaskStatus::Downloading;
        direct_snapshot.aria_gid = Some("direct-gid".into());
        store.apply_transfer_snapshot(direct_snapshot).unwrap();

        let deferred = store.defer_startup_cloud_downloads().unwrap();
        assert_eq!(deferred.len(), 1);
        let deferred = &deferred[0];
        assert_eq!(deferred.id, cloud.id);
        assert_eq!(deferred.status, TaskStatus::Paused);
        assert_eq!(deferred.aria_gid.as_deref(), Some("cloud-gid"));
        assert_eq!(
            deferred.failure_code.as_deref(),
            Some("authorization_checking")
        );
        assert!(deferred.error.is_none());
        assert_eq!(
            deferred.status_message.as_deref(),
            Some("正在检查网盘登录状态")
        );
        assert_eq!(
            store.find(direct.id).unwrap().status,
            TaskStatus::Downloading
        );
    }

    #[test]
    fn startup_authorization_prompts_only_after_silent_recovery_fails() {
        let store = TaskStore::open_in_memory().unwrap();
        let mut task = store
            .create(NewTask::direct("cloud-share://xunlei/share/file"))
            .unwrap();
        task.status = TaskStatus::Downloading;
        let task = store.apply_transfer_snapshot(task).unwrap();
        store.defer_startup_cloud_downloads().unwrap();

        let recovering = store
            .set_startup_cloud_authorization_recovering(CloudProvider::Xunlei)
            .unwrap();
        assert_eq!(recovering.len(), 1);
        assert_eq!(
            recovering[0].failure_code.as_deref(),
            Some("authorization_recovering")
        );
        assert_eq!(
            recovering[0].status_message.as_deref(),
            Some("正在自动恢复网盘登录")
        );
        assert!(recovering[0].error.is_none());

        let required = store
            .set_startup_cloud_authorization_required(CloudProvider::Xunlei)
            .unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].id, task.id);
        assert_eq!(
            required[0].failure_code.as_deref(),
            Some("authorization_pending")
        );
        assert_eq!(required[0].error.as_deref(), Some("恢复网盘登录后继续下载"));
        assert!(required[0].status_message.is_none());
    }

    #[test]
    fn reauthorized_lookup_includes_startup_deferred_cloud_task() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask {
                source: "cloud://quark/file-1".into(),
                name: "cloud.bin".into(),
                kind: "cloud".into(),
                is_private: false,
            })
            .unwrap();
        let mut snapshot = store.find(task.id).unwrap();
        snapshot.status = TaskStatus::Downloading;
        snapshot.aria_gid = Some("cloud-gid".into());
        store.apply_transfer_snapshot(snapshot).unwrap();
        store
            .bind_cloud_download_account(task.id, CloudProvider::Quark, "account-1")
            .unwrap();

        store.defer_startup_cloud_downloads().unwrap();
        let (tasks, leaves) = store
            .reauthorized_cloud_downloads(CloudProvider::Quark, "account-1")
            .unwrap();
        assert!(leaves.is_empty());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id);
    }

    #[test]
    fn post_processing_note_survives_reopen_and_blank_input_clears_it() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let task_id = {
            let store = TaskStore::open_path(&database).unwrap();
            let task = store
                .create(NewTask::direct("https://example.test/archive.zip"))
                .unwrap();
            let updated = store
                .set_post_processing_note(task.id, Some("解压密码：88in\n完成后拼接分卷"))
                .unwrap();
            assert_eq!(
                updated.post_processing_note.as_deref(),
                Some("解压密码：88in\n完成后拼接分卷")
            );
            task.id
        };

        let store = TaskStore::open_path(&database).unwrap();
        assert_eq!(
            store.find(task_id).unwrap().post_processing_note.as_deref(),
            Some("解压密码：88in\n完成后拼接分卷")
        );
        assert!(store
            .set_post_processing_note(task_id, Some(" \n "))
            .unwrap()
            .post_processing_note
            .is_none());
    }

    #[test]
    fn cloud_directory_post_processing_note_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let group_id = {
            let store = TaskStore::open_path(&database).unwrap();
            let group = store
                .create_cloud_directory_group(
                    NewCloudDirectoryGroup::new(
                        CloudProvider::Quark,
                        "remote-root",
                        "合集",
                        directory.path(),
                    ),
                    &[NewCloudDirectoryGroupItem::new(
                        "remote-file",
                        &[] as &[&str],
                        "视频.mp4",
                        Some(42),
                    )],
                )
                .unwrap();
            let updated = store
                .set_cloud_directory_group_post_processing_note(
                    group.id,
                    Some("解压密码：88in\n完成后合并"),
                )
                .unwrap();
            assert_eq!(
                updated.post_processing_note.as_deref(),
                Some("解压密码：88in\n完成后合并")
            );
            group.id
        };

        let store = TaskStore::open_path(&database).unwrap();
        assert_eq!(
            store
                .cloud_directory_group(group_id)
                .unwrap()
                .unwrap()
                .post_processing_note
                .as_deref(),
            Some("解压密码：88in\n完成后合并")
        );
    }

    #[test]
    fn removing_many_task_records_keeps_every_record_when_one_requested_id_is_missing() {
        let store = TaskStore::open_in_memory().unwrap();
        let first = store
            .create(NewTask::direct("https://example.test/first"))
            .unwrap();
        let second = store
            .create(NewTask::direct("https://example.test/second"))
            .unwrap();
        store.bind_gid(first.id, "first-gid").unwrap();
        let missing = Uuid::new_v4();

        assert!(matches!(
            store.remove_task_records(&[first.id, missing, second.id]),
            Err(StoreError::NotFound(id)) if id == missing
        ));

        assert!(store.find(first.id).is_ok());
        assert!(store.find(second.id).is_ok());
        assert!(!store.was_engine_gid_deleted("first-gid").unwrap());
    }

    #[test]
    fn removing_one_task_records_its_engine_gid() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/one"))
            .unwrap();
        store.bind_gid(task.id, "single-gid").unwrap();

        store.remove_task_record(task.id).unwrap();

        assert!(store.was_engine_gid_deleted("single-gid").unwrap());
        assert!(matches!(store.find(task.id), Err(StoreError::NotFound(_))));
    }

    #[test]
    fn private_history_reset_preserves_public_tasks_and_payload_files() {
        let directory = tempfile::tempdir().unwrap();
        let payload = directory.path().join("private.zip");
        std::fs::write(&payload, b"downloaded content").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let public = store.create(NewTask::direct("https://example.test/public")).unwrap();
        let private = store.create(NewTask { source: "https://example.test/private".into(), name: payload.to_string_lossy().into(), kind: "file".into(), is_private: true }).unwrap();
        store.bind_gid(private.id, "private-engine-gid").unwrap();
        store.create_cloud_directory_group(NewCloudDirectoryGroup::new(CloudProvider::Quark, "private-root", "Private", directory.path()).with_private_download(true), &[]).unwrap();
        store.reset_private_task_history().unwrap();
        store.reset_private_task_history().unwrap();
        assert!(!store.has_private_tasks().unwrap());
        assert!(store.find(public.id).is_ok());
        assert!(store.find(private.id).is_err());
        assert!(store.was_engine_gid_deleted("private-engine-gid").unwrap());
        assert_eq!(std::fs::read(&payload).unwrap(), b"downloaded content");
    }

    #[test]
    fn private_task_flag_survives_a_database_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let task_id = {
            let store = TaskStore::open_path(&database).unwrap();
            store
                .create(NewTask {
                    source: "https://example.test/private.zip".into(),
                    name: "private.zip".into(),
                    kind: "file".into(),
                    is_private: true,
                })
                .unwrap()
                .id
        };

        let store = TaskStore::open_path(&database).unwrap();
        assert!(store.find(task_id).unwrap().is_private);
    }

    #[test]
    fn private_directory_group_counts_as_private_task_history() {
        let store = TaskStore::open_in_memory().unwrap();
        let base = std::path::PathBuf::from("C:/downloads");
        assert!(!store.has_private_tasks().unwrap());

        store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(CloudProvider::Quark, "private-root", "Private", &base)
                    .with_private_download(true),
                &[],
            )
            .unwrap();

        assert!(store.has_private_tasks().unwrap());
    }

    #[test]
    fn setting_a_directory_group_private_updates_all_linked_tasks() {
        let store = TaskStore::open_in_memory().unwrap();
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(
                    CloudProvider::Quark,
                    "collection",
                    "Collection",
                    "C:/downloads",
                ),
                &[
                    NewCloudDirectoryGroupItem::new("remote-one", &[] as &[&str], "one.bin", None),
                    NewCloudDirectoryGroupItem::new("remote-two", &[] as &[&str], "two.bin", None),
                ],
            )
            .unwrap();
        let first = store
            .create(NewTask::direct("https://example.test/one.bin"))
            .unwrap();
        let second = store
            .create(NewTask::direct("https://example.test/two.bin"))
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[0].id, first.id)
            .unwrap();
        store
            .link_cloud_directory_group_item_task(group.id, items[1].id, second.id)
            .unwrap();

        let group = store
            .set_cloud_directory_group_private(group.id, true)
            .unwrap();

        assert!(group.is_private);
        assert!(store.find(first.id).unwrap().is_private);
        assert!(store.find(second.id).unwrap().is_private);
    }

    #[test]
    fn cloud_directory_group_persists_items_across_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let base = directory.path().join("downloads");

        let (group_id, item_id) = {
            let store = TaskStore::open_path(&database).unwrap();
            let group = store
                .create_cloud_directory_group(
                    NewCloudDirectoryGroup::new(CloudProvider::Quark, "root-id", "Season 1", &base),
                    &[NewCloudDirectoryGroupItem::new(
                        "remote-1",
                        &["Season 1", "Extras"],
                        "behind.mkv",
                        None,
                    )],
                )
                .unwrap();
            let item = store
                .cloud_directory_group_items(group.id)
                .unwrap()
                .remove(0);
            assert_eq!(item.relative_directory, vec!["Season 1", "Extras"]);
            (group.id, item.id)
        };

        let store = TaskStore::open_path(&database).unwrap();
        let group = store.cloud_directory_group(group_id).unwrap().unwrap();
        let item = store
            .cloud_directory_group_items(group_id)
            .unwrap()
            .remove(0);
        assert_eq!(group.id, group_id);
        assert_eq!(group.provider, CloudProvider::Quark);
        assert_eq!(group.remote_directory_id, "root-id");
        assert_eq!(group.relative_directory, "Season 1");
        assert_eq!(group.output_directory, base);
        assert_eq!(item.id, item_id);
        assert_eq!(item.remote_file_id, "remote-1");
        assert_eq!(item.relative_directory, vec!["Season 1", "Extras"]);
        assert_eq!(item.output_name, "behind.mkv");
    }

    #[test]
    fn legacy_version_eight_directory_groups_gain_the_recycle_status_column() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.sqlite");
        let group_id = Uuid::new_v4();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cloud_directory_groups (
                    id TEXT PRIMARY KEY NOT NULL,
                    provider TEXT NOT NULL,
                    remote_directory_id TEXT NOT NULL,
                    relative_directory TEXT NOT NULL,
                    output_directory TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                 );
                 PRAGMA user_version = 8;",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cloud_directory_groups (id, provider, remote_directory_id, relative_directory, output_directory, created_at, updated_at)
                 VALUES (?1, 'quark', 'remote-wallpapers', '夸克精选壁纸', 'C:/Downloads', 1, 1)",
                [group_id.to_string()],
            )
            .unwrap();
        drop(connection);

        let store = TaskStore::open_path(&database).unwrap();
        let group = store.cloud_directory_group(group_id).unwrap().unwrap();

        assert_eq!(group.status, TaskStatus::Queued);
        assert_eq!(group.relative_directory, "夸克精选壁纸");
    }

    #[test]
    fn cloud_directory_group_returns_only_linked_tasks_and_keeps_failed_item_virtual() {
        let store = TaskStore::open_in_memory().unwrap();
        let base = std::path::PathBuf::from("C:/downloads");
        let group = store
            .create_cloud_directory_group(
                NewCloudDirectoryGroup::new(CloudProvider::Quark, "root-id", "Season 1", &base),
                &[
                    NewCloudDirectoryGroupItem::new(
                        "remote-episode",
                        &["Season 1"],
                        "episode.mkv",
                        None,
                    ),
                    NewCloudDirectoryGroupItem::new(
                        "remote-failed",
                        &["Season 1", "Extras"],
                        "behind.mkv",
                        None,
                    ),
                ],
            )
            .unwrap();
        let items = store.cloud_directory_group_items(group.id).unwrap();
        let task = store
            .create(NewTask::direct("https://example.test/episode.mkv"))
            .unwrap();

        store
            .link_cloud_directory_group_item_task(group.id, items[0].id, task.id)
            .unwrap();
        store
            .fail_cloud_directory_group_item(
                group.id,
                items[1].id,
                "provider_error",
                "link generation failed",
            )
            .unwrap();

        assert_eq!(
            store.cloud_directory_group_for_task(task.id).unwrap(),
            Some(group.clone())
        );
        assert_eq!(
            store.cloud_directory_group_items(group.id).unwrap()[1].task_id,
            None
        );
        assert_eq!(
            store.cloud_directory_group_items(group.id).unwrap()[1]
                .failure_code
                .as_deref(),
            Some("provider_error")
        );
    }
}
