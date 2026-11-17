//! Durable cloud write receipts. A process restart never replays a running write.
use super::*;
use crate::{cli_cloud::CloudPlan,cli_cloud_protocol::CloudOperationDto};
type ResultCode<T> = Result<T,&'static str>;
fn db<T>(value:Result<T,impl std::fmt::Debug>)->ResultCode<T>{value.map_err(|_|"INTERNAL_ERROR")}
pub(super) fn create_tables(c:&Connection)->Result<(),rusqlite::Error>{c.execute_batch("CREATE TABLE IF NOT EXISTS agent_cloud_plans(id TEXT PRIMARY KEY,expires_at INTEGER NOT NULL,plan_json TEXT NOT NULL); CREATE TABLE IF NOT EXISTS agent_cloud_operations(id TEXT PRIMARY KEY,request_id TEXT UNIQUE NOT NULL,plan_json TEXT NOT NULL,dto_json TEXT NOT NULL,status TEXT NOT NULL);")}
impl TaskStore {
    pub(crate) fn save_agent_cloud_plan(&self,plan:&CloudPlan,now:i64)->ResultCode<()> {
        let c=db(self.connection.lock())?;
        db(c.execute("DELETE FROM agent_cloud_plans WHERE expires_at<=?1",[now]))?;
        let count:i64=db(c.query_row("SELECT COUNT(*) FROM agent_cloud_plans",[],|r|r.get(0)))?;
        if count>=256{return Err("BUSY")}
        db(c.execute("INSERT INTO agent_cloud_plans VALUES(?1,?2,?3)",params![plan.id,plan.expires_at,db(serde_json::to_string(plan))?]))?;Ok(())
    }
    pub(crate) fn agent_cloud_plan(&self,id:&str,now:i64)->ResultCode<CloudPlan>{
        let c=db(self.connection.lock())?;
        let json:Option<String>=db(c.query_row("SELECT plan_json FROM agent_cloud_plans WHERE id=?1 AND expires_at>?2",params![id,now],|r|r.get(0)).optional())?;
        db(serde_json::from_str(&json.ok_or("PREPARATION_EXPIRED")?))
    }
    pub(crate) fn agent_cloud_operation(&self,id:&str)->ResultCode<(CloudPlan,CloudOperationDto)>{
        let c=db(self.connection.lock())?;
        let row:Option<(String,String)>=db(c.query_row("SELECT plan_json,dto_json FROM agent_cloud_operations WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
        let (p,d)=row.ok_or("NOT_FOUND")?;Ok((db(serde_json::from_str(&p))?,db(serde_json::from_str(&d))?))
    }
    pub(crate) fn agent_cloud_receipt(&self,request_id:&str,fingerprint:&str)->ResultCode<Option<(CloudPlan,CloudOperationDto)>> {
        let id={let c=db(self.connection.lock())?;
            let row:Option<(String,String)>=db(c.query_row("SELECT fingerprint,task_id FROM cli_requests WHERE request_id=?1",[request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
            match row{None=>return Ok(None),Some((f,id))=>{if f!=fingerprint{return Err("REQUEST_CONFLICT")}id}}};
        self.agent_cloud_operation(&id).map(Some)
    }
    pub(crate) fn accept_agent_cloud(&self,plan:&CloudPlan,dto:&CloudOperationDto,fingerprint:&str)->ResultCode<CloudOperationDto>{
        let mut c=db(self.connection.lock())?;let tx=db(c.transaction())?;
        let row:Option<(String,String)>=db(tx.query_row("SELECT fingerprint,task_id FROM cli_requests WHERE request_id=?1",[&dto.request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
        if let Some((f,id))=row {if f!=fingerprint{return Err("REQUEST_CONFLICT")}
            let json:String=db(tx.query_row("SELECT dto_json FROM agent_cloud_operations WHERE id=?1",[id],|r|r.get(0)))?;
            return db(serde_json::from_str(&json));}
        let count:i64=db(tx.query_row("SELECT COUNT(*) FROM agent_cloud_operations WHERE status IN ('queued','running','cancelling')",[],|r|r.get(0)))?;
        if count>=64{return Err("BUSY")}
        db(tx.execute("INSERT INTO cli_requests VALUES(?1,?2,?3)",params![dto.request_id,fingerprint,dto.id]))?;
        db(tx.execute("INSERT INTO agent_cloud_operations VALUES(?1,?2,?3,?4,?5)",params![dto.id,dto.request_id,db(serde_json::to_string(plan))?,db(serde_json::to_string(dto))?,dto.status]))?;
        db(tx.commit())?;Ok(dto.clone())
    }
    pub(crate) fn agent_cloud_pending(&self)->ResultCode<Vec<String>>{
        let c=db(self.connection.lock())?;let mut q=db(c.prepare("SELECT id FROM agent_cloud_operations WHERE status='queued' LIMIT 16"))?;
        let rows=db(q.query_map([],|r|r.get(0)))?;rows.map(db).collect()
    }
    pub(crate) fn update_agent_cloud(&self,dto:&CloudOperationDto,expected:&[&str])->ResultCode<bool>{
        let c=db(self.connection.lock())?;
        let old:Option<String>=db(c.query_row("SELECT status FROM agent_cloud_operations WHERE id=?1",[&dto.id],|r|r.get(0)).optional())?;
        if !old.as_deref().is_some_and(|s|expected.contains(&s)){return Ok(false)}
        db(c.execute("UPDATE agent_cloud_operations SET dto_json=?1,status=?2 WHERE id=?3",params![db(serde_json::to_string(dto))?,dto.status,dto.id]))?;Ok(true)
    }
    pub(crate) fn recover_agent_cloud(&self)->ResultCode<()> {
        let c=db(self.connection.lock())?;let mut q=db(c.prepare("SELECT dto_json FROM agent_cloud_operations WHERE status IN ('running','cancelling')"))?;
        let rows=db(q.query_map([],|r|r.get::<_,String>(0)))?;
        for row in rows {let mut dto:CloudOperationDto=db(serde_json::from_str(&db(row)?))?;
            dto.status="needs_review".into();dto.supported_actions.clear();dto.error=Some(crate::cli_protocol::ApiError::new("CLOUD_RESULT_UNCERTAIN","en"));
            db(c.execute("UPDATE agent_cloud_operations SET dto_json=?1,status=?2 WHERE id=?3",params![db(serde_json::to_string(&dto))?,dto.status,dto.id]))?;
        }Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_cloud::FileSnapshot;
    use crate::cli_resources::ResourceTarget;
    use crate::cloud::CloudProvider;

    fn fixture(request: &str) -> (CloudPlan, CloudOperationDto) {
        let plan = CloudPlan {
            id: Uuid::new_v4().to_string(),
            expires_at: 200,
            operation: "upload".into(),
            provider: CloudProvider::Quark,
            account_id: "fixture-account".into(),
            targets: vec![ResourceTarget::Cloud {
                provider: CloudProvider::Quark,
                account_id: "fixture-account".into(),
                file_id: "destination".into(),
                name: "Uploads".into(),
                size_bytes: None,
                folder: true,
                directory_chain: vec!["0".into(), "destination".into()],
            }],
            sources: vec![FileSnapshot {
                path: std::path::PathBuf::from("fixture-input.txt"),
                name: "fixture-input.txt".into(),
                size: 7,
                modified: 123,
            }],
            share_options: None,
        };
        let dto = CloudOperationDto {
            id: Uuid::new_v4().to_string(),
            request_id: request.into(),
            operation: "upload".into(),
            provider: "quark".into(),
            account_ref: crate::cli_cloud_auth::account_ref(plan.provider, &plan.account_id),
            status: "queued".into(),
            completed_bytes: 0,
            total_bytes: Some(7),
            supported_actions: vec!["cancel".into()],
            files: Vec::new(),
            share: None,
            failures: Vec::new(),
            error: None,
        };
        (plan, dto)
    }

    fn operation_count(store: &TaskStore) -> i64 {
        store.connection.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM agent_cloud_operations", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn agent_cloud_receipt_retries_return_original_operation_and_reject_conflicting_arguments() {
        let store = TaskStore::open_in_memory().unwrap();
        let (plan, dto) = fixture("cloud-request");
        let accepted = store.accept_agent_cloud(&plan, &dto, "cloud-plan-a").unwrap();
        let mut retry = dto.clone();
        retry.id = Uuid::new_v4().to_string();
        let replay = store.accept_agent_cloud(&plan, &retry, "cloud-plan-a").unwrap();
        assert_eq!(replay.id, accepted.id);
        assert_eq!(operation_count(&store), 1);
        assert!(matches!(store.accept_agent_cloud(&plan, &retry, "cloud-plan-b"), Err("REQUEST_CONFLICT")));
        assert!(matches!(store.agent_cloud_receipt("cloud-request", "cloud-plan-b"), Err("REQUEST_CONFLICT")));
        assert_eq!(store.agent_cloud_receipt("cloud-request", "cloud-plan-a").unwrap().unwrap().1.id, accepted.id);
    }

    #[test]
    fn agent_cloud_request_ids_cannot_collide_with_existing_cli_downloads_in_either_direction() {
        let store = TaskStore::open_in_memory().unwrap();
        assert!(matches!(store.create_cli_task(
            "http-request", "http-input", crate::NewTask::direct("https://example.test/a.bin"),
        ).unwrap(), CliCreateOutcome::Created(_)));
        let (plan, dto) = fixture("http-request");
        assert!(matches!(store.accept_agent_cloud(&plan, &dto, "cloud-input"), Err("REQUEST_CONFLICT")));
        assert!(matches!(store.agent_cloud_receipt("http-request", "cloud-input"), Err("REQUEST_CONFLICT")));
        assert_eq!(operation_count(&store), 0);
        let (plan, dto) = fixture("cloud-request");
        store.accept_agent_cloud(&plan, &dto, "cloud-input").unwrap();
        assert!(matches!(store.create_cli_task(
            "cloud-request", "another-http-input", crate::NewTask::direct("https://example.test/b.bin"),
        ).unwrap(), CliCreateOutcome::Conflict));
        assert_eq!(operation_count(&store), 1);
    }

    #[test]
    fn agent_cloud_receipt_survives_preparation_expiry_cleanup_and_database_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.sqlite");
        let store = TaskStore::open_path(&path).unwrap();
        let (plan, dto) = fixture("persisted-cloud-request");
        store.save_agent_cloud_plan(&plan, 100).unwrap();
        store.accept_agent_cloud(&plan, &dto, "persisted-input").unwrap();
        assert!(matches!(store.agent_cloud_plan(&plan.id, 200), Err("PREPARATION_EXPIRED")));
        let (mut newer, _) = fixture("unused-preview");
        newer.expires_at = 400;
        store.save_agent_cloud_plan(&newer, 201).unwrap();
        let remaining: i64 = store.connection.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM agent_cloud_plans WHERE id=?1", [&plan.id], |row| row.get(0),
        ).unwrap();
        assert_eq!(remaining, 0);
        drop(store);
        let store = TaskStore::open_path(&path).unwrap();
        let (persisted_plan, receipt) = store.agent_cloud_receipt("persisted-cloud-request", "persisted-input")
            .unwrap().unwrap();
        assert_eq!(receipt.id, dto.id);
        assert_eq!(persisted_plan.id, plan.id);
        assert_eq!(persisted_plan.account_id, "fixture-account");
        assert_eq!(persisted_plan.sources[0].name, "fixture-input.txt");
        assert_eq!(operation_count(&store), 1);
    }

    #[test]
    fn agent_cloud_restart_requires_review_for_inflight_writes_without_replaying_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.sqlite");
        let store = TaskStore::open_path(&path).unwrap();
        let mut inflight = Vec::new();
        for status in ["running", "cancelling"] {
            let (plan, mut dto) = fixture(status);
            store.accept_agent_cloud(&plan, &dto, status).unwrap();
            dto.status = status.into();
            dto.completed_bytes = 3;
            assert!(store.update_agent_cloud(&dto, &["queued"]).unwrap());
            inflight.push(dto);
        }
        let (queued_plan, queued) = fixture("not-started");
        store.accept_agent_cloud(&queued_plan, &queued, "not-started").unwrap();
        drop(store);
        let store = TaskStore::open_path(&path).unwrap();
        store.recover_agent_cloud().unwrap();
        store.recover_agent_cloud().unwrap();
        assert_eq!(store.agent_cloud_pending().unwrap(), vec![queued.id]);
        for previous in inflight {
            let (_, current) = store.agent_cloud_operation(&previous.id).unwrap();
            assert_eq!(current.status, "needs_review");
            assert_eq!(current.completed_bytes, 3);
            assert!(current.supported_actions.is_empty());
            assert_eq!(current.error.as_ref().unwrap().code, "CLOUD_RESULT_UNCERTAIN");
            assert!(!store.update_agent_cloud(&previous, &["queued"]).unwrap());
            assert_eq!(store.agent_cloud_receipt(&previous.request_id, &previous.request_id).unwrap().unwrap().1.status, "needs_review");
        }
    }

    #[test]
    fn agent_cloud_cancelling_queued_work_prevents_a_stale_worker_from_claiming_it() {
        let store = TaskStore::open_in_memory().unwrap();
        let (plan, queued) = fixture("cancel-before-start");
        store.accept_agent_cloud(&plan, &queued, "input").unwrap();
        let mut cancelled = queued.clone();
        cancelled.status = "cancelled".into();
        cancelled.supported_actions.clear();
        assert!(store.update_agent_cloud(&cancelled, &["queued"]).unwrap());
        let mut stale_claim = queued;
        stale_claim.status = "running".into();
        assert!(!store.update_agent_cloud(&stale_claim, &["queued"]).unwrap());
        assert!(store.agent_cloud_pending().unwrap().is_empty());
        assert_eq!(store.agent_cloud_operation(&cancelled.id).unwrap().1.status, "cancelled");
    }

    #[test]
    fn agent_cloud_progress_cannot_overwrite_cancel_requests_or_terminal_results() {
        let store = TaskStore::open_in_memory().unwrap();
        let (plan, mut dto) = fixture("progress-race");
        store.accept_agent_cloud(&plan, &dto, "input").unwrap();
        dto.status = "running".into();
        assert!(store.update_agent_cloud(&dto, &["queued"]).unwrap());
        let mut stale_progress = dto.clone();
        stale_progress.completed_bytes = 5;
        dto.status = "cancelling".into();
        assert!(store.update_agent_cloud(&dto, &["running"]).unwrap());
        assert!(!store.update_agent_cloud(&stale_progress, &["running"]).unwrap());
        assert_eq!(store.agent_cloud_operation(&dto.id).unwrap().1.status, "cancelling");
        dto.status = "cancelled".into();
        dto.completed_bytes = 2;
        dto.supported_actions.clear();
        assert!(store.update_agent_cloud(&dto, &["running", "cancelling"]).unwrap());
        assert!(!store.update_agent_cloud(&stale_progress, &["running", "cancelling"]).unwrap());
        let current = store.agent_cloud_operation(&dto.id).unwrap().1;
        assert_eq!(current.status, "cancelled");
        assert_eq!(current.completed_bytes, 2);
        assert!(current.supported_actions.is_empty());
    }

    #[test]
    fn agent_cloud_acceptance_rolls_back_its_request_receipt_if_operation_storage_fails() {
        let store = TaskStore::open_in_memory().unwrap();
        let (plan, dto) = fixture("atomic-acceptance");
        store.connection.lock().unwrap().execute_batch(
            "CREATE TRIGGER reject_cloud_operation BEFORE INSERT ON agent_cloud_operations
             BEGIN SELECT RAISE(FAIL,'fixture'); END;",
        ).unwrap();
        assert!(store.accept_agent_cloud(&plan, &dto, "input").is_err());
        assert!(store.agent_cloud_receipt(&dto.request_id, "input").unwrap().is_none());
        assert_eq!(operation_count(&store), 0);
        store.connection.lock().unwrap().execute_batch("DROP TRIGGER reject_cloud_operation").unwrap();
        assert_eq!(store.accept_agent_cloud(&plan, &dto, "input").unwrap().id, dto.id);
    }
}
