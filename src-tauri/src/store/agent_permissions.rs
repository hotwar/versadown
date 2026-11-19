//! Durable per-task grants. A grant is immutable and bound to its original
//! request fingerprint; only trusted host code can create or copy these rows.
use super::*;
use crate::agent_permissions::LocalAccessGrant;

type CodeResult<T> = Result<T, &'static str>;
fn db<T>(value: Result<T, impl std::fmt::Debug>) -> CodeResult<T> {
    value.map_err(|_| "INTERNAL_ERROR")
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}
pub(super) fn create_tables(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_task_grants(
            scope_id TEXT PRIMARY KEY,
            fingerprint TEXT NOT NULL,
            grant_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS agent_grant_claims(
            scope_id TEXT PRIMARY KEY,
            request_id TEXT NOT NULL
        );"
    )
}

fn insert_or_match(connection: &Connection, scope_id: &str, fingerprint: &str, json: &str) -> CodeResult<()> {
    let existing: Option<(String, String)> = db(connection.query_row(
        "SELECT fingerprint,grant_json FROM agent_task_grants WHERE scope_id=?1",
        [scope_id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional())?;
    match existing {
        Some((old_fingerprint, old_json)) if old_fingerprint == fingerprint && old_json == json => Ok(()),
        Some(_) => Err("REQUEST_CONFLICT"),
        None => {
            db(connection.execute(
                "INSERT INTO agent_task_grants(scope_id,fingerprint,grant_json) VALUES(?1,?2,?3)",
                params![scope_id, fingerprint, json],
            ))?;
            Ok(())
        }
    }
}

impl TaskStore {
    pub(crate) fn save_agent_task_grant(
        &self, scope_id: &str, fingerprint: &str, grant: &LocalAccessGrant,
    ) -> CodeResult<()> {
        if !valid_id(scope_id) || !valid_id(fingerprint) { return Err("INVALID_ARGUMENT"); }
        let grant = grant.validated()?;
        let json = db(serde_json::to_string(&grant))?;
        let connection = db(self.connection.lock())?;
        insert_or_match(&connection, scope_id, fingerprint, &json)
    }

    pub(crate) fn agent_task_grant(&self, scope_id: &str) -> CodeResult<Option<LocalAccessGrant>> {
        if !valid_id(scope_id) { return Err("INVALID_ARGUMENT"); }
        let connection = db(self.connection.lock())?;
        let json: Option<String> = db(connection.query_row(
            "SELECT grant_json FROM agent_task_grants WHERE scope_id=?1",
            [scope_id], |row| row.get(0),
        ).optional())?;
        json.map(|json| db(serde_json::from_str(&json))).transpose()
    }

    /// Missing sources are valid for operations needing only saved global
    /// permissions. Copy stored physical paths without revalidating existence:
    /// completed operations retain their receipt after a local source is moved.
    pub(crate) fn copy_agent_task_grant(&self, source_id: &str, destination_id: &str) -> CodeResult<()> {
        if !valid_id(source_id) || !valid_id(destination_id) { return Err("INVALID_ARGUMENT"); }
        let connection = db(self.connection.lock())?;
        let source: Option<(String, String)> = db(connection.query_row(
            "SELECT fingerprint,grant_json FROM agent_task_grants WHERE scope_id=?1",
            [source_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional())?;
        if let Some((fingerprint, json)) = source {
            insert_or_match(&connection, destination_id, &fingerprint, &json)?;
        }
        Ok(())
    }

    /// Bind a prepared scope to one accepted confirmation request. This does
    /// not grant filesystem access. Validate before calling, then retry using
    /// the same request ID if durable acceptance subsequently fails.
    pub(crate) fn claim_agent_grant(&self, scope_id: &str, request_id: &str) -> CodeResult<()> {
        if !valid_id(scope_id) || !valid_id(request_id) { return Err("INVALID_ARGUMENT"); }
        let connection = db(self.connection.lock())?;
        let existing: Option<String> = db(connection.query_row(
            "SELECT request_id FROM agent_grant_claims WHERE scope_id=?1",
            [scope_id], |row| row.get(0),
        ).optional())?;
        match existing {
            Some(existing) if existing == request_id => Ok(()),
            Some(_) => Err("REQUEST_CONFLICT"),
            None => {
                db(connection.execute(
                    "INSERT INTO agent_grant_claims(scope_id,request_id) VALUES(?1,?2)",
                    params![scope_id, request_id],
                ))?;
                Ok(())
            }
        }
    }

    pub(crate) fn remove_agent_task_grant(&self, scope_id: &str) -> CodeResult<()> {
        if !valid_id(scope_id) { return Err("INVALID_ARGUMENT"); }
        let connection = db(self.connection.lock())?;
        db(connection.execute("DELETE FROM agent_task_grants WHERE scope_id=?1", [scope_id]))?;
        // A removed permission must not make a previously claimed request
        // identity reusable for a different confirmation.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload_grant(path: &Path) -> LocalAccessGrant {
        LocalAccessGrant { upload_files: vec![path.to_string_lossy().into_owned()], ..Default::default() }
    }

    #[test]
    fn agent_task_grants_bind_fingerprint_and_never_expand_existing_scope() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let second = root.path().join("second.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let first_grant = upload_grant(&first);
        store.save_agent_task_grant("request-a", "fingerprint-a", &first_grant).unwrap();
        store.save_agent_task_grant("request-a", "fingerprint-a", &first_grant).unwrap();
        assert_eq!(store.save_agent_task_grant("request-a", "fingerprint-b", &first_grant), Err("REQUEST_CONFLICT"));
        assert_eq!(store.save_agent_task_grant("request-a", "fingerprint-a", &upload_grant(&second)), Err("REQUEST_CONFLICT"));
        assert!(store.agent_task_grant("unrelated-request").unwrap().is_none());
        assert_eq!(store.agent_task_grant("request-a").unwrap().unwrap(), first_grant.validated().unwrap());
    }

    #[test]
    fn agent_task_grants_follow_only_explicit_plan_and_operation_bindings_after_reopen() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.txt");
        std::fs::write(&input, b"input").unwrap();
        let database = root.path().join("tasks.sqlite");
        let store = TaskStore::open_path(&database).unwrap();
        store.save_agent_task_grant("interaction", "original-request", &upload_grant(&input)).unwrap();
        store.copy_agent_task_grant("interaction", "plan").unwrap();
        store.copy_agent_task_grant("plan", "operation").unwrap();
        let expected = store.agent_task_grant("operation").unwrap().unwrap();
        drop(store);
        std::fs::remove_file(&input).unwrap();
        let reopened = TaskStore::open_path(&database).unwrap();
        assert_eq!(reopened.agent_task_grant("operation").unwrap(), Some(expected));
        assert!(reopened.agent_task_grant("another-operation").unwrap().is_none());
        reopened.copy_agent_task_grant("operation", "receipt").unwrap();
        assert!(reopened.agent_task_grant("receipt").unwrap().is_some());
    }

    #[test]
    fn agent_task_grants_cannot_replace_another_tasks_binding() {
        let root = tempfile::tempdir().unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let grant = LocalAccessGrant { download_directories: vec![root.path().to_string_lossy().into_owned()], ..Default::default() };
        store.save_agent_task_grant("first", "first-request", &grant).unwrap();
        store.save_agent_task_grant("second", "second-request", &grant).unwrap();
        store.copy_agent_task_grant("first", "destination").unwrap();
        store.copy_agent_task_grant("first", "destination").unwrap();
        assert_eq!(store.copy_agent_task_grant("second", "destination"), Err("REQUEST_CONFLICT"));
        store.copy_agent_task_grant("missing", "another").unwrap();
        assert!(store.agent_task_grant("another").unwrap().is_none());
    }

    #[test]
    fn agent_task_grant_claim_is_single_use_but_retryable_and_survives_revocation() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("tasks.sqlite");
        let store = TaskStore::open_path(&database).unwrap();
        store.claim_agent_grant("plan", "request-a").unwrap();
        store.claim_agent_grant("plan", "request-a").unwrap();
        assert_eq!(store.claim_agent_grant("plan", "request-b"), Err("REQUEST_CONFLICT"));
        store.remove_agent_task_grant("plan").unwrap();
        drop(store);
        let reopened = TaskStore::open_path(&database).unwrap();
        reopened.claim_agent_grant("plan", "request-a").unwrap();
        assert_eq!(reopened.claim_agent_grant("plan", "request-b"), Err("REQUEST_CONFLICT"));
        assert!(reopened.agent_task_grant("plan").unwrap().is_none(), "claim is not an authorization");
    }

    #[test]
    fn agent_task_grant_parallel_confirmations_allow_only_one_request() {
        let store = Arc::new(TaskStore::open_in_memory().unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = ["request-a", "request-b"].into_iter().map(|request_id| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || { barrier.wait(); store.claim_agent_grant("plan", request_id) })
        }).collect();
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| **result == Err("REQUEST_CONFLICT")).count(), 1);
    }
}
