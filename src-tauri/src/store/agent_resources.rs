//! Durable resource previews, acceptance receipts and conservative dispatch journal.
use super::*;
use crate::cli_resources::{ResourcePlan, ResourceSelection, ResourceTarget};
use crate::cli_protocol::ResourceConfirmDto;
use std::path::PathBuf;

type ResultCode<T> = Result<T, &'static str>;
fn storage<T>(result: Result<T, impl std::fmt::Debug>) -> ResultCode<T> {
    result.map_err(|_| "INTERNAL_ERROR")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(names:&[&str])->ResourcePlan {
        ResourcePlan{id:Uuid::new_v4().to_string(),expires_at:200,kind:"video".into(),effects:vec![],bt:None,
            choices:names.iter().map(|name|ResourceSelection{id:Uuid::new_v4().to_string(),name:(*name).into(),kind:"video_format".into(),size_bytes:Some(7),detail:None,
                target:ResourceTarget::Video{page:format!("https://example.test/{name}"),selector:"720p".into(),media_id:(*name).into(),name:(*name).into(),size_bytes:Some(7)}}).collect()}
    }
    fn count(store:&TaskStore,table:&str)->i64 {
        store.connection.lock().unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"),[],|r|r.get(0)).unwrap()
    }
    #[test]
    fn agent_resource_prepare_has_no_task_and_replay_survives_restart_and_expiry() {
        let root=tempfile::tempdir().unwrap();let database=root.path().join("tasks.sqlite");
        let store=TaskStore::open_path(&database).unwrap();let plan=plan(&["clip.mp4"]);
        store.save_agent_resource_plan(&plan,100).unwrap();
        assert_eq!(count(&store,"tasks"),0);
        let receipt=store.accept_agent_resources(&plan,&plan.choices,root.path(),"request-1","resource-fingerprint",100).unwrap();
        let id=Uuid::parse_str(&receipt.task_ids[0]).unwrap();
        assert!(store.agent_resource_dispatch_pending(id).unwrap());
        assert_eq!(count(&store,"video_task_formats"),1);
        drop(store);
        let store=TaskStore::open_path(&database).unwrap();
        assert!(matches!(store.agent_resource_plan(&plan.id,201),Err("PREPARATION_EXPIRED")));
        let replay=store.accept_agent_resources(&plan,&plan.choices,root.path(),"request-1","resource-fingerprint",201).unwrap();
        assert_eq!(replay.task_ids,receipt.task_ids);
        assert_eq!(count(&store,"tasks"),1);
        assert!(matches!(store.agent_resource_receipt("request-1","different"),Err("REQUEST_CONFLICT")));
    }
    #[test]
    fn agent_resource_acceptance_rolls_back_every_task_and_receipt_on_partial_failure() {
        let root=tempfile::tempdir().unwrap();let store=TaskStore::open_in_memory().unwrap();let plan=plan(&["one.mp4","two.mp4"]);
        store.save_agent_resource_plan(&plan,100).unwrap();
        store.connection.lock().unwrap().execute_batch("CREATE TRIGGER reject_second_agent_task BEFORE INSERT ON tasks WHEN NEW.name='two.mp4' BEGIN SELECT RAISE(FAIL,'fixture'); END;").unwrap();
        assert!(store.accept_agent_resources(&plan,&plan.choices,root.path(),"request-2","fingerprint",100).is_err());
        for table in ["tasks","video_task_formats","cli_requests","agent_resource_receipts","agent_resource_dispatch"] {assert_eq!(count(&store,table),0,"{table}");}
        store.connection.lock().unwrap().execute_batch("DROP TRIGGER reject_second_agent_task").unwrap();
        assert_eq!(store.accept_agent_resources(&plan,&plan.choices,root.path(),"request-2","fingerprint",100).unwrap().task_ids.len(),2);
    }
    #[test]
    fn agent_resource_foreign_selection_and_cross_operation_request_are_rejected() {
        let root=tempfile::tempdir().unwrap();let store=TaskStore::open_in_memory().unwrap();let plan=plan(&["one.mp4"]);
        store.save_agent_resource_plan(&plan,100).unwrap();let mut forged=plan.choices.clone();
        if let ResourceTarget::Video{page,..}=&mut forged[0].target {*page="https://other.example/secret".into();}
        assert!(matches!(store.accept_agent_resources(&plan,&forged,root.path(),"forged","fingerprint",100),Err("INVALID_ARGUMENT")));
        store.connection.lock().unwrap().execute("INSERT INTO cli_requests(request_id,fingerprint,task_id) VALUES('shared','http-add','old-task')",[]).unwrap();
        assert!(matches!(store.accept_agent_resources(&plan,&plan.choices,root.path(),"shared","resource-confirm",100),Err("REQUEST_CONFLICT")));
        assert_eq!(count(&store,"tasks"),0);
    }
    #[test]
    fn agent_resource_uncertain_submission_is_not_replayed_or_resumed() {
        let root=tempfile::tempdir().unwrap();let store=TaskStore::open_in_memory().unwrap();let plan=plan(&["one.mp4"]);
        store.save_agent_resource_plan(&plan,100).unwrap();
        let id=Uuid::parse_str(&store.accept_agent_resources(&plan,&plan.choices,root.path(),"recover","fingerprint",100).unwrap().task_ids[0]).unwrap();
        assert!(store.claim_agent_resource(id).unwrap());
        store.recover_agent_resource_intents().unwrap();
        let task=store.find(id).unwrap();assert_eq!(task.status,TaskStatus::Failed);assert_eq!(task.failure_code.as_deref(),Some("CLI_RECOVERY_REQUIRED"));
        assert!(store.agent_resource_pending().unwrap().is_empty());
        assert!(matches!(store.control_agent_resource_pending(id,"resume"),Err("CLI_RECOVERY_REQUIRED")));
        assert!(store.agent_resource_receipt("recover","fingerprint").unwrap().is_some());
    }
    #[test]
    fn agent_resource_pause_and_cancel_prevent_dispatch_claim() {
        let root=tempfile::tempdir().unwrap();let store=TaskStore::open_in_memory().unwrap();let plan=plan(&["one.mp4"]);
        store.save_agent_resource_plan(&plan,100).unwrap();
        let id=Uuid::parse_str(&store.accept_agent_resources(&plan,&plan.choices,root.path(),"controls","fingerprint",100).unwrap().task_ids[0]).unwrap();
        assert_eq!(store.control_agent_resource_pending(id,"pause").unwrap().unwrap().status,TaskStatus::Paused);
        assert!(!store.claim_agent_resource(id).unwrap());
        assert_eq!(store.control_agent_resource_pending(id,"resume").unwrap().unwrap().status,TaskStatus::Parsing);
        assert_eq!(store.control_agent_resource_pending(id,"cancel").unwrap().unwrap().status,TaskStatus::Cancelled);
        assert!(!store.claim_agent_resource(id).unwrap());
    }
    #[test]
    fn agent_resource_bt_selection_is_one_task_and_recovers_same_intent() {
        let root=tempfile::tempdir().unwrap();let store=TaskStore::open_in_memory().unwrap();
        let metadata=crate::cli_bt::BtPreparation{source:"magnet:?xt=urn:btih:fixture".into(),kind:"magnet".into(),name:"archive".into(),torrent:"fixture".into(),info_hash:"a".repeat(40),
            files:vec![crate::cli_bt::BtFile{index:1,path:"one.txt".into(),length:7},crate::cli_bt::BtFile{index:2,path:"two.txt".into(),length:9}]};
        let mut plan=plan(&[]);plan.kind="torrent".into();plan.bt=Some(metadata);
        plan.choices=(1..=2).map(|index|ResourceSelection{id:Uuid::new_v4().to_string(),name:format!("file-{index}"),kind:"file".into(),size_bytes:None,detail:None,target:ResourceTarget::BtFile{index}}).collect();
        store.save_agent_resource_plan(&plan,100).unwrap();
        let receipt=store.accept_agent_resources(&plan,&plan.choices,root.path(),"bt-request","fingerprint",100).unwrap();
        assert_eq!(receipt.task_ids.len(),1);assert_eq!(count(&store,"tasks"),1);
        let id=Uuid::parse_str(&receipt.task_ids[0]).unwrap();store.claim_agent_resource(id).unwrap();store.recover_agent_resource_intents().unwrap();
        let pending=store.agent_resource_pending().unwrap();assert_eq!(pending.len(),1);assert_eq!(pending[0].0,id);
        assert!(matches!(&pending[0].2,ResourceTarget::Bt{indices,..} if indices==&vec![1,2]));
    }
}

pub(super) fn create_tables(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS agent_resource_preparations (
        id TEXT PRIMARY KEY, expires_at INTEGER NOT NULL, plan_json TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS agent_resource_receipts (
        request_id TEXT PRIMARY KEY, preparation_id TEXT NOT NULL, task_ids_json TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS agent_resource_dispatch (
        task_id TEXT PRIMARY KEY, source TEXT NOT NULL, directory TEXT NOT NULL,
        payload_json TEXT NOT NULL, phase INTEGER NOT NULL DEFAULT 0);")?;
    Ok(())
}

impl TaskStore {
    pub(crate) fn save_agent_resource_plan(&self, plan: &ResourcePlan, now: i64) -> ResultCode<()> {
        let json = storage(serde_json::to_string(plan))?;
        let maximum = if plan.bt.is_some() { 24 * 1_048_576 } else { 1_048_576 };
        if plan.choices.len() > 1000 || json.len() > maximum { return Err("RESOURCE_LIMIT"); }
        let connection = storage(self.connection.lock())?;
        storage(connection.execute("DELETE FROM agent_resource_preparations WHERE expires_at<=?1", [now]))?;
        let count: i64 = storage(connection.query_row("SELECT COUNT(*) FROM agent_resource_preparations", [], |r| r.get(0)))?;
        if count >= 256 { return Err("BUSY"); }
        let bytes:i64=storage(connection.query_row("SELECT COALESCE(SUM(LENGTH(plan_json)),0) FROM agent_resource_preparations",[],|r|r.get(0)))?;
        if bytes.saturating_add(json.len() as i64)>128*1_048_576 {return Err("RESOURCE_LIMIT")}
        storage(connection.execute("INSERT INTO agent_resource_preparations(id,expires_at,plan_json) VALUES(?1,?2,?3)",
            params![plan.id,plan.expires_at,json]))?;
        Ok(())
    }

    pub(crate) fn agent_resource_plan(&self, id: &str, now: i64) -> ResultCode<ResourcePlan> {
        let connection = storage(self.connection.lock())?;
        let value: Option<String> = storage(connection.query_row(
            "SELECT plan_json FROM agent_resource_preparations WHERE id=?1 AND expires_at>?2",
            params![id,now], |r| r.get(0)).optional())?;
        storage(serde_json::from_str(&value.ok_or("PREPARATION_EXPIRED")?))
    }

    /// Folder handles are opaque IDs from an unexpired, host-created preview.
    pub(crate) fn agent_resource_folder(&self, selection_id: &str, now: i64) -> ResultCode<ResourceTarget> {
        let connection = storage(self.connection.lock())?;
        let mut query = storage(connection.prepare("SELECT plan_json FROM agent_resource_preparations WHERE expires_at>?1"))?;
        let rows = storage(query.query_map([now], |r| r.get::<_,String>(0)))?;
        for row in rows {
            let plan: ResourcePlan = storage(serde_json::from_str(&storage(row)?))?;
            for choice in plan.choices {
                if choice.id == selection_id && matches!(&choice.target,ResourceTarget::Cloud { folder:true,.. }) {
                    return Ok(choice.target);
                }
            }
        }
        Err("PREPARATION_EXPIRED")
    }

    /// Check receipts before preview expiry so a lost reply remains retryable.
    pub(crate) fn agent_resource_receipt(&self, request_id: &str, fingerprint: &str) -> ResultCode<Option<ResourceConfirmDto>> {
        let connection = storage(self.connection.lock())?;
        Self::resource_receipt_with(&connection, request_id, fingerprint)
    }

    fn resource_receipt_with(connection: &Connection, request_id: &str, fingerprint: &str) -> ResultCode<Option<ResourceConfirmDto>> {
        let existing: Option<String> = storage(connection.query_row(
            "SELECT fingerprint FROM cli_requests WHERE request_id=?1", [request_id], |r| r.get(0)).optional())?;
        let Some(existing) = existing else { return Ok(None); };
        if existing != fingerprint { return Err("REQUEST_CONFLICT"); }
        let receipt: Option<(String,String)> = storage(connection.query_row(
            "SELECT preparation_id,task_ids_json FROM agent_resource_receipts WHERE request_id=?1",
            [request_id], |r| Ok((r.get(0)?,r.get(1)?))).optional())?;
        let (preparation_id, ids) = receipt.ok_or("REQUEST_CONFLICT")?;
        Ok(Some(ResourceConfirmDto { preparation_id,task_ids:storage(serde_json::from_str(&ids))? }))
    }

    pub(crate) fn accept_agent_resources(&self, plan: &ResourcePlan, selected: &[ResourceSelection],
        directory: &Path, request_id: &str, fingerprint: &str, now: i64) -> ResultCode<ResourceConfirmDto> {
        if selected.is_empty() || selected.len()>100 || !directory.is_absolute() { return Err("INVALID_ARGUMENT"); }
        let mut connection = storage(self.connection.lock())?;
        let tx = storage(connection.transaction())?;
        if let Some(receipt) = Self::resource_receipt_with(&tx,request_id,fingerprint)? { return Ok(receipt); }
        let stored: Option<String> = storage(tx.query_row(
            "SELECT plan_json FROM agent_resource_preparations WHERE id=?1 AND expires_at>?2",
            params![plan.id,now], |r| r.get(0)).optional())?;
        let stored: ResourcePlan = storage(serde_json::from_str(&stored.ok_or("PREPARATION_EXPIRED")?))?;
        let count:i64=storage(tx.query_row("SELECT COUNT(*) FROM agent_resource_dispatch d JOIN tasks t ON t.id=d.task_id WHERE t.status NOT IN('completed','failed','cancelled','recycled')",[],|r|r.get(0)))?;
        if count.saturating_add(selected.len() as i64)>1000 {return Err("RESOURCE_LIMIT")}
        let mut targets=Vec::new();
        let mut selections=std::collections::HashSet::new();
        for selection in selected {
            if !selections.insert(&selection.id) {return Err("INVALID_ARGUMENT")}
            let valid = stored.choices.iter().find(|c| c.id==selection.id).ok_or("INVALID_ARGUMENT")?;
            if serde_json::to_value(valid).ok()!=serde_json::to_value(selection).ok() { return Err("INVALID_ARGUMENT"); }
            targets.push(selection.target.clone());
        }
        if stored.kind=="torrent" {
            let metadata=stored.bt.ok_or("INVALID_ARGUMENT")?;
            let indices=targets.iter().map(|target|match target {ResourceTarget::BtFile{index}=>Ok(*index),_=>Err("INVALID_ARGUMENT")}).collect::<Result<Vec<_>,_>>()?;
            if indices.iter().any(|index|!metadata.files.iter().any(|file|&file.index==index)){return Err("INVALID_ARGUMENT")}
            targets=vec![ResourceTarget::Bt{metadata,indices}];
        }
        let mut ids = Vec::new();
        for target in targets {
            let new = target.new_task()?;
            let duplicate: bool = storage(tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE source=?1 AND kind=?2 AND status!='recycled')",
                params![new.source,new.kind], |r|r.get(0)))?;
            if duplicate { return Err("DUPLICATE"); }
            let task = storage(Self::create_with_connection(&tx,new))?;
            match &target {
                ResourceTarget::Video { selector,media_id,.. } => {
                    storage(tx.execute("INSERT INTO video_task_formats(task_id,selector,media_id,engine) VALUES(?1,?2,?3,'aria2')",
                        params![task.id.to_string(),selector,media_id]))?;
                }
                ResourceTarget::Cloud { provider,account_id,.. } => {
                    storage(tx.execute("INSERT INTO cloud_download_accounts(id,provider,account_id) VALUES(?1,?2,?3)",
                        params![task.id.to_string(),provider.key(),account_id]))?;
                }
                ResourceTarget::Share {..}|ResourceTarget::Bt{..} => (),
                ResourceTarget::BtFile{..}=>return Err("INVALID_ARGUMENT"),
            }
            storage(tx.execute("INSERT INTO agent_resource_dispatch(task_id,source,directory,payload_json) VALUES(?1,?2,?3,?4)",
                params![task.id.to_string(),task.source,directory.to_string_lossy(),storage(serde_json::to_string(&target))?]))?;
            ids.push(task.id.to_string());
        }
        storage(tx.execute("INSERT INTO cli_requests(request_id,fingerprint,task_id) VALUES(?1,?2,?3)",
            params![request_id,fingerprint,ids[0]]))?;
        storage(tx.execute("INSERT INTO agent_resource_receipts(request_id,preparation_id,task_ids_json) VALUES(?1,?2,?3)",
            params![request_id,plan.id,storage(serde_json::to_string(&ids))?]))?;
        storage(tx.commit())?;
        Ok(ResourceConfirmDto {preparation_id:plan.id.clone(),task_ids:ids})
    }

    pub(crate) fn agent_resource_pending(&self) -> ResultCode<Vec<(Uuid,PathBuf,ResourceTarget)>> {
        let connection = storage(self.connection.lock())?;
        let mut query = storage(connection.prepare("SELECT d.task_id,d.directory,d.payload_json,t.status FROM agent_resource_dispatch d
            JOIN tasks t ON t.id=d.task_id WHERE d.phase=0 AND t.status IN('parsing','queued','paused','cancelled') AND t.is_private=0 ORDER BY t.created_at LIMIT 1000"))?;
        let rows = storage(query.query_map([], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))))?;
        let mut pending=Vec::new();
        for row in rows {let(id,dir,payload,status)=storage(row)?;let target:ResourceTarget=storage(serde_json::from_str(&payload))?;
            if matches!(status.as_str(),"paused"|"cancelled")&&!matches!(target,ResourceTarget::Bt{..}){continue;}
            pending.push((storage(Uuid::parse_str(&id))?,PathBuf::from(dir),target));}
        Ok(pending)
    }

    pub(crate) fn claim_agent_resource(&self, id: Uuid) -> ResultCode<bool> {
        let connection = storage(self.connection.lock())?;
        Ok(storage(connection.execute("UPDATE agent_resource_dispatch SET phase=1 WHERE task_id=?1 AND phase=0
            AND EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status IN('parsing','queued') AND is_private=0)",[id.to_string()]))?==1)
    }

    pub(crate) fn retry_agent_resource_dispatch(&self,id:Uuid)->ResultCode<()> {
        let connection=storage(self.connection.lock())?;
        storage(connection.execute("UPDATE agent_resource_dispatch SET phase=0 WHERE task_id=?1",[id.to_string()]))?;
        Ok(())
    }

    pub(crate) fn claim_agent_resource_stop(&self,id:Uuid)->ResultCode<bool> {
        let connection=storage(self.connection.lock())?;
        let payload:Option<String>=storage(connection.query_row("SELECT payload_json FROM agent_resource_dispatch WHERE task_id=?1 AND phase=0",[id.to_string()],|r|r.get(0)).optional())?;
        if !payload.is_some_and(|payload|matches!(serde_json::from_str::<ResourceTarget>(&payload),Ok(ResourceTarget::Bt{..}))){return Ok(false)}
        Ok(storage(connection.execute("UPDATE agent_resource_dispatch SET phase=1 WHERE task_id=?1 AND phase=0 AND EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status IN('paused','cancelled') AND is_private=0)",[id.to_string()]))?==1)
    }

    pub(crate) fn mark_agent_resource_stopped(&self,id:Uuid)->ResultCode<()> {
        let connection=storage(self.connection.lock())?;
        storage(connection.execute("UPDATE agent_resource_dispatch SET phase=3 WHERE task_id=?1",[id.to_string()]))?;
        Ok(())
    }

    pub(crate) fn block_agent_resource_authorization(&self,id:Uuid,code:&str)->ResultCode<()> {
        let connection=storage(self.connection.lock())?;
        storage(connection.execute("UPDATE tasks SET status='paused',failure_code=?2,error=?3,download_speed=0 WHERE id=?1",params![id.to_string(),code,crate::cli_protocol::message(code,"en")]))?;
        Ok(())
    }

    pub(crate) fn finish_agent_resource_dispatch(&self,id:Uuid) -> ResultCode<()> {
        let connection=storage(self.connection.lock())?;
        storage(connection.execute("UPDATE agent_resource_dispatch SET phase=2 WHERE task_id=?1",[id.to_string()]))?;
        Ok(())
    }

    pub(crate) fn agent_resource_directory(&self,id:Uuid) -> ResultCode<Option<PathBuf>> {
        let connection=storage(self.connection.lock())?;
        let directory:Option<String>=storage(connection.query_row("SELECT directory FROM agent_resource_dispatch WHERE task_id=?1",[id.to_string()],|r|r.get(0)).optional())?;
        Ok(directory.map(PathBuf::from))
    }

    pub(crate) fn is_agent_resource_task(&self,id:Uuid) -> Result<bool,StoreError> {
        let connection=self.connection.lock().map_err(|_|StoreError::Poisoned)?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM agent_resource_dispatch WHERE task_id=?1)",[id.to_string()],|r|r.get(0))?)
    }

    pub(crate) fn agent_resource_dispatch_pending(&self,id:Uuid)->Result<bool,StoreError> {
        let connection=self.connection.lock().map_err(|_|StoreError::Poisoned)?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM agent_resource_dispatch WHERE task_id=?1 AND phase!=2)",[id.to_string()],|r|r.get(0))?)
    }

    pub(crate) fn settle_agent_resource_dispatches(&self)->Result<(),StoreError> {
        let connection=self.connection.lock().map_err(|_|StoreError::Poisoned)?;
        connection.execute("UPDATE agent_resource_dispatch SET phase=2 WHERE phase=1 AND task_id IN(SELECT id FROM tasks WHERE aria_gid IS NOT NULL OR status IN('completed','failed','cancelled','recycled'))",[])?;
        Ok(())
    }

    pub(crate) fn is_agent_direct_share_source(&self,source:&str) -> Result<bool,StoreError> {
        let connection=self.connection.lock().map_err(|_|StoreError::Poisoned)?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM agent_resource_dispatch WHERE source=?1)",[source],|r|r.get(0))?)
    }

    /// Must run before the existing engine/cloud/video recovery routines.
    /// An uncertain non-transactional provider submission is never blindly replayed.
    pub(crate) fn recover_agent_resource_intents(&self) -> Result<(),StoreError> {
        let connection=self.connection.lock().map_err(|_|StoreError::Poisoned)?;
        // BT has a durable, caller-chosen aria2 GID. Replaying that exact intent
        // is safe even when the process died between submission and its reply.
        let retryable={
            let mut query=connection.prepare("SELECT d.task_id,d.payload_json FROM agent_resource_dispatch d JOIN tasks t ON t.id=d.task_id WHERE d.phase=1 AND t.status NOT IN('completed','recycled')")?;
            let rows=query.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
            let mut ids=Vec::new();
            for row in rows {let(id,json)=row?;if matches!(serde_json::from_str::<ResourceTarget>(&json),Ok(ResourceTarget::Bt{..})){ids.push(id)}}
            ids
        };
        for id in retryable {
            connection.execute("UPDATE agent_resource_dispatch SET phase=0 WHERE task_id=?1",[&id])?;
            connection.execute("UPDATE tasks SET status='parsing' WHERE id=?1 AND status NOT IN('paused','cancelled')",[&id])?;
        }
        connection.execute("UPDATE tasks SET status='failed',failure_code='CLI_RECOVERY_REQUIRED',
            error=?1,download_speed=0 WHERE id IN(SELECT task_id FROM agent_resource_dispatch WHERE phase=1)
            AND aria_gid IS NULL AND status NOT IN('completed','cancelled','recycled')",
            [crate::cli_protocol::message("CLI_RECOVERY_REQUIRED","en")])?;
        connection.execute("UPDATE agent_resource_dispatch SET phase=2 WHERE phase=1 AND task_id IN(
            SELECT id FROM tasks WHERE aria_gid IS NOT NULL OR status IN('completed','cancelled','recycled','failed'))",[])?;
        Ok(())
    }

    /// Serializes pending cancellation with the persisted dispatch claim.
    pub(crate) fn control_agent_resource_pending(&self,id:Uuid,action:&str) -> ResultCode<Option<DownloadTask>> {
        let connection=storage(self.connection.lock())?;
        let phase:Option<i64>=storage(connection.query_row("SELECT phase FROM agent_resource_dispatch WHERE task_id=?1",[id.to_string()],|r|r.get(0)).optional())?;
        let Some(phase)=phase else{return Ok(None)};
        let task=storage(find_with_connection(&connection,id))?;
        if task.failure_code.as_deref()==Some("CLI_RECOVERY_REQUIRED") && action=="resume" {return Err("CLI_RECOVERY_REQUIRED")}
        if phase!=0&&phase!=3 {
            let payload:String=storage(connection.query_row("SELECT payload_json FROM agent_resource_dispatch WHERE task_id=?1",[id.to_string()],|r|r.get(0)))?;
            if action!="resume"||!matches!(serde_json::from_str::<ResourceTarget>(&payload),Ok(ResourceTarget::Bt{..})){return Ok(None)}
        }
        let next=match (task.status,action) {
            (TaskStatus::Parsing|TaskStatus::Queued,"pause")=>Some("paused"),
            (TaskStatus::Paused|TaskStatus::Failed|TaskStatus::Cancelled,"resume")=>Some("parsing"),
            (TaskStatus::Parsing|TaskStatus::Queued|TaskStatus::Paused|TaskStatus::Failed,"cancel")=>Some("cancelled"),
            (_,"pause"|"resume"|"cancel")=>None,
            _=>return Err("INVALID_ARGUMENT"),
        };
        if let Some(next)=next {
            storage(connection.execute("UPDATE tasks SET status=?2,updated_at=?3,failure_code=NULL,error=NULL WHERE id=?1",params![id.to_string(),next,current_timestamp()]))?;
            if action=="resume"||phase==3&&action=="cancel" {storage(connection.execute("UPDATE agent_resource_dispatch SET phase=0 WHERE task_id=?1",[id.to_string()]))?;}
        }
        Ok(Some(storage(find_with_connection(&connection,id))?))
    }
}
