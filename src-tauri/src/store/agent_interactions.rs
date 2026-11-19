//! Durable continuation records. CAS updates serialize user decisions and cancellation.
use super::*;
use crate::agent_interactions::PendingInteraction;

type CodeResult<T> = Result<T, &'static str>;
pub(super) fn create_tables(c: &Connection) -> Result<(), rusqlite::Error> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS agent_interactions (
        id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL UNIQUE,
        status TEXT NOT NULL, record TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0);")
}
impl TaskStore {
    /// Preparation is a fresh read/preview, not a mutation receipt. Preserve
    /// its interaction ID for status queries without caching its handles forever.
    pub(crate) fn release_agent_interaction_lookup(&self, id: &str) -> CodeResult<()> {
        let c = self.connection.lock().map_err(|_| "INTERNAL_ERROR")?;
        c.execute("UPDATE agent_interactions SET fingerprint=id WHERE id=?1 AND status='succeeded'",[id]).map_err(|_|"INTERNAL_ERROR")?;
        Ok(())
    }
    pub(crate) fn agent_interaction(&self, id: &str) -> CodeResult<PendingInteraction> {
        let c = self.connection.lock().map_err(|_| "INTERNAL_ERROR")?;
        let json: Option<String> = c.query_row("SELECT record FROM agent_interactions WHERE id=?1", [id], |r| r.get(0)).optional().map_err(|_| "INTERNAL_ERROR")?;
        serde_json::from_str(&json.ok_or("NOT_FOUND")?).map_err(|_| "INTERNAL_ERROR")
    }
    pub(crate) fn find_agent_interaction(&self, fingerprint: &str) -> CodeResult<Option<PendingInteraction>> {
        let c = self.connection.lock().map_err(|_| "INTERNAL_ERROR")?;
        let json: Option<String> = c.query_row("SELECT record FROM agent_interactions WHERE fingerprint=?1", [fingerprint], |r| r.get(0)).optional().map_err(|_| "INTERNAL_ERROR")?;
        json.map(|v| serde_json::from_str(&v).map_err(|_| "INTERNAL_ERROR")).transpose()
    }
    pub(crate) fn insert_agent_interaction(&self, record: &PendingInteraction) -> CodeResult<()> {
        let c = self.connection.lock().map_err(|_| "INTERNAL_ERROR")?;
        let count: i64 = c.query_row("SELECT count(*) FROM agent_interactions WHERE status IN ('awaiting_user','awaiting_login','resuming')", [], |r| r.get(0)).map_err(|_| "INTERNAL_ERROR")?;
        if count >= 32 { return Err("BUSY"); }
        let json = serde_json::to_string(record).map_err(|_| "INTERNAL_ERROR")?;
        let changed = c.execute("INSERT OR IGNORE INTO agent_interactions(id,fingerprint,status,record) VALUES (?1,?2,?3,?4)", params![record.id,record.fingerprint,record.status,json]).map_err(|_| "INTERNAL_ERROR")?;
        if changed == 0 { return Err("BUSY"); }
        Ok(())
    }
    pub(crate) fn update_agent_interaction(&self, record: &mut PendingInteraction, previous: &str) -> CodeResult<()> {
        let mut next=record.clone();next.revision+=1;
        let json = serde_json::to_string(&next).map_err(|_| "INTERNAL_ERROR")?;
        let c = self.connection.lock().map_err(|_| "INTERNAL_ERROR")?;
        let changed = c.execute("UPDATE agent_interactions SET status=?1,record=?2,revision=?5 WHERE id=?3 AND status=?4 AND revision=?6", params![record.status,json,record.id,previous,next.revision,record.revision]).map_err(|_| "INTERNAL_ERROR")?;
        if changed == 0 { return Err("REQUEST_CONFLICT"); }
        record.revision=next.revision;
        Ok(())
    }
    pub(crate) fn pending_agent_interactions(&self) -> CodeResult<Vec<PendingInteraction>> {
        let c = self.connection.lock().map_err(|_| "INTERNAL_ERROR")?;
        let mut statement = c.prepare("SELECT record FROM agent_interactions WHERE status IN ('awaiting_user','awaiting_login','resuming') ORDER BY rowid").map_err(|_| "INTERNAL_ERROR")?;
        let rows = statement.query_map([], |r| r.get::<_,String>(0)).map_err(|_| "INTERNAL_ERROR")?;
        rows.map(|r| serde_json::from_str(&r.map_err(|_| "INTERNAL_ERROR")?).map_err(|_| "INTERNAL_ERROR")).collect()
    }
}
