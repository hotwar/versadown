//! Agent cloud operations. The host owns scopes, provider sessions and receipts.
use crate::cloud::BaiduUploadAuthStage;
use crate::{cli_cloud_protocol::*,cli_protocol::{ApiError,Response},cli_resources::{ResourceTarget,ResourcePlan,ResourceSelection},commands::AppState,cloud::{CloudProvider,CloudShareOptions,QuarkError}};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use std::{collections::HashSet,path::{Path,PathBuf},sync::Arc,time::{Duration,SystemTime,UNIX_EPOCH}};
use tauri::{AppHandle,Manager};
use uuid::Uuid;
type CodeResult<T>=Result<T,&'static str>;
pub(crate) fn now()->i64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64}

#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct FileSnapshot {pub path:PathBuf,pub name:String,pub size:u64,pub modified:u128}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct CloudPlan {
    pub id:String,pub expires_at:i64,pub operation:String,pub provider:CloudProvider,pub account_id:String,
    pub targets:Vec<ResourceTarget>,pub sources:Vec<FileSnapshot>,pub share_options:Option<CloudShareOptions>,
}

pub(crate) fn cloud_code(e:QuarkError)->&'static str {
    match &e {
        QuarkError::InvalidInput(s) if s=="agent.error.forbidden"=>"CLOUD_PERMISSION_REQUIRED",
        QuarkError::InvalidInput(s) if s=="agent.error.fileConflict"=>"FILE_CONFLICT",
        QuarkError::InvalidInput(_)=>"INVALID_ARGUMENT",
        QuarkError::SourceFileMissing=>"SOURCE_CHANGED",
        QuarkError::BaiduUploadAuthorizationRequired(stage) => match stage {
            BaiduUploadAuthStage::Token => "CLOUD_UPLOAD_SESSION_REQUIRED",
            BaiduUploadAuthStage::Precreate => "CLOUD_UPLOAD_VERIFICATION_REQUIRED",
            BaiduUploadAuthStage::Commit => "CLOUD_RESULT_UNCERTAIN",
        },
        _=>crate::cli_resources::cloud_error(e),
    }
}
fn target_fields(target:&ResourceTarget)->CodeResult<(CloudProvider,&str,&str,&[String],bool)> {
    match target {ResourceTarget::Cloud{provider,account_id,file_id,directory_chain,folder,..}=>Ok((*provider,account_id,file_id,directory_chain,*folder)),_=>Err("INVALID_ARGUMENT")}
}
pub(crate) fn scope(app:&AppHandle,target:&ResourceTarget)->CodeResult<()> {
    let (provider,account,_,_,_)=target_fields(target)?;
    crate::agent_access::require_cloud_account(&app.state::<AppState>(),provider.key(),account)
}
pub(crate) async fn validate_target(app:&AppHandle,target:&ResourceTarget)->CodeResult<()> {
    scope(app,target)?;
    let (provider,account,id,chain,folder)=target_fields(target)?;
    let account=account.to_owned();let id=id.to_owned();let chain=chain.to_vec();
    crate::cloud::commands::run_session_work(app.clone(),provider,crate::cloud::SessionWorkKind::Read,move|s,h|{
        crate::cloud::commands::validate_agent_cloud_path(provider,&s.store,h,&account,&chain,&id,folder)
    }).await.map_err(cloud_code)?;
    scope(app,target)
}
fn snapshot(path:&Path)->CodeResult<FileSnapshot>{
    let meta=std::fs::symlink_metadata(path).map_err(|_|"SOURCE_CHANGED")?;
    // Directory uploads need their own bounded manifest and conflict policy.
    if !meta.is_file()||meta.file_type().is_symlink(){return Err("UNSUPPORTED")}
    let name=path.file_name().and_then(|s|s.to_str()).ok_or("INVALID_ARGUMENT")?.to_owned();
    let modified=meta.modified().map_err(|_|"SOURCE_CHANGED")?.duration_since(UNIX_EPOCH).map_err(|_|"SOURCE_CHANGED")?.as_nanos();
    Ok(FileSnapshot{path:path.into(),name,size:meta.len(),modified})
}
fn validate_source(app:&AppHandle,permission_scope:&str,file:&FileSnapshot)->CodeResult<()> {
    let path=crate::agent_permissions::authorize_upload_file_in_scope(&app.state::<AppState>(),permission_scope,&file.path)?;
    let current=snapshot(&path)?;
    if path!=file.path||current.size!=file.size||current.modified!=file.modified {Err("SOURCE_CHANGED")}else{Ok(())}
}
fn allowed_plan(app:&AppHandle,plan:&CloudPlan)->CodeResult<()> {
    for target in &plan.targets {scope(app,target)?;}
    let state=app.state::<AppState>();
    for file in &plan.sources {crate::agent_permissions::authorize_upload_path_in_scope(&state,&plan.id,&file.path)?;}
    Ok(())
}
fn preview(plan:&CloudPlan)->CloudPreparedDto {
    CloudPreparedDto {preparation_id:plan.id.clone(),operation:plan.operation.clone(),provider:plan.provider.key().into(),
        account_ref:crate::cli_cloud_auth::account_ref(plan.provider,&plan.account_id),expires_at:plan.expires_at,
        items:if plan.operation=="upload" {plan.sources.iter().map(|s|CloudOperationItemDto{name:s.name.clone(),size_bytes:Some(s.size)}).collect()}
        else{plan.targets.iter().filter_map(|t|match t{ResourceTarget::Cloud{name,size_bytes,..}=>Some(CloudOperationItemDto{name:name.clone(),size_bytes:*size_bytes}),_=>None}).collect()},
        effects:vec![if plan.operation=="upload"{"upload_local_files_without_overwrite"}else{"create_share_for_existing_cloud_files"}.into()]}
}
async fn prepare(app:&AppHandle,input:&CloudPrepareInput)->CodeResult<CloudPreparedDto>{
    let state=app.state::<AppState>();
    let mut plan=CloudPlan{id:Uuid::new_v4().to_string(),expires_at:now()+900,operation:String::new(),provider:CloudProvider::Quark,account_id:String::new(),targets:vec![],sources:vec![],share_options:None};
    match input {
        CloudPrepareInput::Upload{destination_id,paths}=>{
            plan.operation="upload".into();
            let target=state.store.agent_resource_folder(destination_id,now())?;
            let (provider,account,_,_,_)=target_fields(&target)?;
            plan.provider=provider;plan.account_id=account.into();
            if !crate::cloud::ProviderRegistry::global().get(provider).capabilities.upload{return Err("UNSUPPORTED")}
            let mut names=HashSet::new();
            for path in paths {
                let canonical=crate::agent_permissions::authorize_upload_path(&state,Path::new(path))?;
                let file=snapshot(&canonical)?;
                if !names.insert(file.name.to_lowercase()){return Err("FILE_CONFLICT")}
                plan.sources.push(file);
            }
            validate_target(app,&target).await?;
            let (_,_,id,_,_)=target_fields(&target)?;
            let listing=crate::cloud::commands::cloud_list_directory(app.clone(),provider,id.to_owned()).await.map_err(cloud_code)?;
            if listing.items.iter().any(|f|names.contains(&f.name.to_lowercase())){return Err("FILE_CONFLICT")}
            plan.targets.push(target);
        }
        CloudPrepareInput::CreateShare{selection_ids,name,expires_in_days,pass_code,generate_pass_code}=>{
            plan.operation="create_share".into();
            for selection in selection_ids {
                let target=state.store.agent_resource_selection(selection,now())?;
                let (provider,account,_,_,_)=target_fields(&target)?;
                if plan.targets.is_empty(){plan.provider=provider;plan.account_id=account.into();}
                if plan.provider!=provider||plan.account_id!=account{return Err("ACCOUNT_MISMATCH")}
                if !crate::cloud::ProviderRegistry::global().get(provider).capabilities.share_create{return Err("UNSUPPORTED")}
                validate_target(app,&target).await?;
                plan.targets.push(target);
            }
            let options=CloudShareOptions{name:name.clone(),pass_code:pass_code.clone(),generate_pass_code:*generate_pass_code,expires_in_days:if *expires_in_days==0{None}else{Some(*expires_in_days)}};
            crate::cloud::commands::validate_share_options(plan.provider,&options).map_err(cloud_code)?;
            plan.share_options=Some(options);
        }
    }
    // Carry the one-request local approval beyond the prepare request and into
    // confirmation/background execution. It never becomes a saved setting.
    crate::agent_permissions::inherit_scope(&state,&plan.id)?;
    allowed_plan(app,&plan)?;
    state.store.save_agent_cloud_plan(&plan,now())?;
    Ok(preview(&plan))
}
fn fingerprint(preparation_id:&str)->String{format!("{:x}",Sha256::digest(format!("agent_cloud_confirm:{preparation_id}")))}
fn operation_dto(plan:&CloudPlan,request_id:&str)->CloudOperationDto{
    CloudOperationDto{id:Uuid::new_v4().to_string(),request_id:request_id.into(),operation:plan.operation.clone(),provider:plan.provider.key().into(),account_ref:crate::cli_cloud_auth::account_ref(plan.provider,&plan.account_id),status:"queued".into(),completed_bytes:0,total_bytes:if plan.operation=="upload"{Some(plan.sources.iter().map(|f|f.size).sum())}else{None},supported_actions:vec!["cancel".into()],files:vec![],share:None,failures:vec![],error:None}
}
async fn confirm(app:&AppHandle,id:&str,request_id:&str)->CodeResult<CloudOperationDto>{
    let state=app.state::<AppState>();let fingerprint=fingerprint(id);
    if let Some((plan,dto))=state.store.agent_cloud_receipt(request_id,&fingerprint)?{allowed_plan(app,&plan)?;return Ok(dto)}
    let plan=state.store.agent_cloud_plan(id,now())?;
    for target in &plan.targets{validate_target(app,target).await?;}
    for source in &plan.sources{validate_source(app,&plan.id,source)?;}
    allowed_plan(app,&plan)?;
    // One prepared write belongs to one request. Retrying that request is
    // allowed; a new request cannot reuse its task-specific local approval.
    state.store.claim_agent_grant(&plan.id,request_id)?;
    state.store.accept_agent_cloud(&plan,&operation_dto(&plan,request_id),&fingerprint)
}
fn status(app:&AppHandle,id:&str)->CodeResult<CloudOperationDto>{let (plan,dto)=app.state::<AppState>().store.agent_cloud_operation(id)?;allowed_plan(app,&plan)?;Ok(dto)}
fn cancel(app:&AppHandle,id:&str)->CodeResult<CloudOperationDto>{
    let state=app.state::<AppState>();let (plan,mut dto)=state.store.agent_cloud_operation(id)?;allowed_plan(app,&plan)?;
    if matches!(dto.status.as_str(),"succeeded"|"partial"|"failed"|"cancelled"|"needs_review"){return Ok(dto)}
    let old=dto.status.clone();
    if old=="queued"{dto.status="cancelled".into();dto.supported_actions.clear();}
    else if plan.operation=="upload"{dto.status="cancelling".into();}
    else{return Err("UNSUPPORTED")}
    if !state.store.update_agent_cloud(&dto,&[old.as_str()])?{return status(app,id)}
    if dto.status=="cancelling"{let _=crate::cloud::commands::cloud_control_upload(plan.provider,id.into(),"cancel".into());}
    Ok(dto)
}
pub(crate) async fn execute(app:AppHandle,request:&CloudRequest,lang:&str)->Response{
    if !crate::cli_cloud_protocol::valid_request(request){return Response::failure("INVALID_ARGUMENT",lang)}
    if matches!(request,CloudRequest::Accounts{..}|CloudRequest::AuthBegin{..}|CloudRequest::AuthStatus{..}|CloudRequest::AuthCancel{..}){return crate::cli_cloud_auth::execute(app,request,lang).await}
    let result:CodeResult<serde_json::Value>=match request{
        CloudRequest::Prepare{input}=>match tokio::time::timeout(Duration::from_secs(25),prepare(&app,input)).await{Ok(r)=>r.and_then(|d|serde_json::to_value(d).map_err(|_|"INTERNAL_ERROR")),Err(_)=>Err("TIMEOUT")},
        CloudRequest::Confirm{preparation_id,request_id}=>confirm(&app,preparation_id,request_id).await.and_then(|d|serde_json::to_value(d).map_err(|_|"INTERNAL_ERROR")),
        CloudRequest::Status{operation_id}=>status(&app,operation_id).and_then(|d|serde_json::to_value(d).map_err(|_|"INTERNAL_ERROR")),
        CloudRequest::Cancel{operation_id}=>cancel(&app,operation_id).and_then(|d|serde_json::to_value(d).map_err(|_|"INTERNAL_ERROR")),
        _=>Err("INVALID_ARGUMENT"),
    };
    match result{Ok(v)=>Response::success(v),Err(code)=>Response::failure(code,lang)}
}

fn operation_cancelled(app:&AppHandle,id:&str)->bool{app.state::<AppState>().store.agent_cloud_operation(id).map(|(_,d)|matches!(d.status.as_str(),"cancelling"|"cancelled")).unwrap_or(true)}
async fn output_file(app:&AppHandle,plan:&CloudPlan,file:crate::cloud::CloudFile)->CodeResult<CloudFileResultDto>{
    let (_,_,parent,chain,_)=target_fields(&plan.targets[0])?;let mut chain=chain.to_vec();if chain.last().map(String::as_str)!=Some(parent){chain.push(parent.into());}
    if file.parent_id!=parent {
        let (_,listing)=crate::cloud::commands::prepare_agent_cloud_directory(app.clone(),plan.provider,Some((plan.account_id.clone(),parent.into()))).await.map_err(cloud_code)?;
        if listing.items.iter().any(|item|item.id==file.parent_id&&item.kind==crate::cloud::CloudFileKind::Folder){chain.push(file.parent_id.clone());}
        else{return Err("CLOUD_RESULT_UNCERTAIN")}
    }
    let id=Uuid::new_v4().to_string();let result=CloudFileResultDto{selection_id:id.clone(),name:file.name.clone(),size_bytes:file.size,kind:file.kind.as_str().into()};
    let target=ResourceTarget::Cloud{provider:plan.provider,account_id:plan.account_id.clone(),file_id:file.id,name:file.name.clone(),size_bytes:file.size,folder:file.kind==crate::cloud::CloudFileKind::Folder,directory_chain:chain};
    validate_target(app,&target).await?;
    app.state::<AppState>().store.save_agent_resource_plan(&ResourcePlan{id:Uuid::new_v4().to_string(),expires_at:now()+900,kind:"cloud".into(),bt:None,effects:vec![],choices:vec![ResourceSelection{id,name:file.name,kind:file.kind.as_str().into(),size_bytes:file.size,detail:None,target}]},now())?;
    Ok(result)
}
async fn perform(app:&AppHandle,plan:&CloudPlan,dto:&mut CloudOperationDto)->CodeResult<()> {
    allowed_plan(app,plan)?;
    if plan.operation=="create_share"{
        let checked_app=app.clone();let owned=plan.clone();
        let share=crate::cloud::commands::run_session_work(app.clone(),plan.provider,crate::cloud::SessionWorkKind::Mutation,move|s,h|{
            allowed_plan(&checked_app,&owned).map_err(|_|QuarkError::InvalidInput("agent.error.forbidden".into()))?;
            let mut ids=Vec::new();
            for target in &owned.targets {let (_,account,id,chain,folder)=target_fields(target).map_err(|_|QuarkError::Storage)?;
                crate::cloud::commands::validate_agent_cloud_path(owned.provider,&s.store,h,account,chain,id,folder)?;ids.push(id.to_owned());}
            allowed_plan(&checked_app,&owned).map_err(|_|QuarkError::InvalidInput("agent.error.forbidden".into()))?;
            crate::cloud::ProviderRegistry::global().driver(owned.provider,&s.store,h).create_share(&ids,owned.share_options.as_ref().ok_or(QuarkError::Storage)?)
        }).await.map_err(cloud_code)?;
        // The actual provider result is authoritative; never invent an expiry.
        dto.share=Some(CloudShareResultDto{url:share.url,pass_code:share.pass_code,expires_at:share.expires_at});
    }else{
        let (_,_,parent,chain,_)=target_fields(&plan.targets[0])?;
        for (index,file) in plan.sources.iter().enumerate(){
            if operation_cancelled(app,&dto.id){dto.status="cancelled".into();break;}
            if let Err(code)=allowed_plan(app,plan).and_then(|_|validate_source(app,&plan.id,file)) {
                dto.failures.push(CloudItemErrorDto{index,name:file.name.clone(),error:ApiError::new(code,"en")});break;
            }
            let progress_app=app.clone();let id=dto.id.clone();let done=dto.completed_bytes;let provider=plan.provider;let progress_plan=plan.clone();
            let progress=Arc::new(move |event:crate::cloud::CloudUploadProgress|{
                if operation_cancelled(&progress_app,&id)||allowed_plan(&progress_app,&progress_plan).is_err(){let _=crate::cloud::commands::cloud_control_upload(provider,id.clone(),"cancel".into());}
                if let Ok((_,mut current))=progress_app.state::<AppState>().store.agent_cloud_operation(&id) {
                    if current.status=="running" {current.completed_bytes=done.saturating_add(event.uploaded_bytes).min(current.total_bytes.unwrap_or(u64::MAX));let _=progress_app.state::<AppState>().store.update_agent_cloud(&current,&["running"]);}
                }
            });
            let result=crate::cloud::commands::upload_agent_file(app.clone(),plan.provider,plan.account_id.clone(),chain.to_vec(),parent.into(),file.clone(),dto.id.clone(),plan.id.clone(),progress).await;
            match result {
                Ok(uploaded)=>{dto.files.push(output_file(app,plan,uploaded).await.map_err(|_|"CLOUD_RESULT_UNCERTAIN")?);dto.completed_bytes=dto.completed_bytes.saturating_add(file.size);},
                Err(e)=>{
                    use crate::cloud::commands::AgentUploadError;
                    let code=match e {
                        AgentUploadError::Provider(error) if crate::cloud::is_upload_cancelled(&error)=>{dto.status="cancelled".into();break;},
                        AgentUploadError::Provider(error)=>cloud_code(error),
                        AgentUploadError::ResultUncertain=>"CLOUD_RESULT_UNCERTAIN",
                        AgentUploadError::SourceChanged=>"SOURCE_CHANGED",
                    };
                    dto.failures.push(CloudItemErrorDto{index,name:file.name.clone(),error:ApiError::new(code,"en")});
                    match code {
                        "CLOUD_RESULT_UNCERTAIN" | "ACTION_FAILED" => {
                            dto.status = "needs_review".into();
                            break;
                        }
                        "CLOUD_UPLOAD_SESSION_REQUIRED" | "CLOUD_UPLOAD_VERIFICATION_REQUIRED"
                        | "CLOUD_OPERATION_VERIFICATION_REQUIRED" | "CLOUD_AUTHORIZATION_PENDING"
                        | "AUTH_REQUIRED" | "ACCOUNT_MISMATCH" => break,
                        _ => {}
                    }
                }
            }
            app.state::<AppState>().store.update_agent_cloud(dto,&["running"])?;
        }
    }
    if dto.status=="running"{dto.status=if dto.failures.is_empty(){"succeeded"}else if dto.files.is_empty(){"failed"}else{"partial"}.into();}
    Ok(())
}
pub(crate) fn start(app:AppHandle){tauri::async_runtime::spawn(async move{
    let slots=Arc::new(tokio::sync::Semaphore::new(2));
    loop {
        if let Ok(ids)=app.state::<AppState>().store.agent_cloud_pending(){for id in ids{
            let Ok(permit)=slots.clone().try_acquire_owned() else{break};
            let Ok((plan,mut dto))=app.state::<AppState>().store.agent_cloud_operation(&id) else{continue};
            dto.status="running".into();if plan.operation=="create_share"{dto.supported_actions.clear();}
            if !app.state::<AppState>().store.update_agent_cloud(&dto,&["queued"]).unwrap_or(false){continue;}
            let work_app=app.clone();
            tauri::async_runtime::spawn(async move{let _permit=permit;
                if let Err(code)=perform(&work_app,&plan,&mut dto).await{dto.status=if matches!(code,"ACTION_FAILED"|"CLOUD_RESULT_UNCERTAIN"|"INTERNAL_ERROR"){"needs_review"}else if dto.files.is_empty(){"failed"}else{"partial"}.into();dto.error=Some(ApiError::new(code,"en"));}
                dto.supported_actions.clear();let _=work_app.state::<AppState>().store.update_agent_cloud(&dto,&["running","cancelling"]);
            });
        }}
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
});}

#[cfg(test)]
mod tests{
    use super::*;
    #[test]fn cloud_upload_snapshot_detects_changes_and_rejects_directories(){let dir=tempfile::tempdir().unwrap();assert!(matches!(snapshot(dir.path()),Err("UNSUPPORTED")));let p=dir.path().join("sample.txt");std::fs::write(&p,b"one").unwrap();let a=snapshot(&p).unwrap();std::fs::write(&p,b"longer").unwrap();let b=snapshot(&p).unwrap();assert_ne!(a.size,b.size);}
    #[test]fn cloud_write_errors_do_not_turn_account_changes_or_uncertain_mutations_into_login(){assert_eq!(cloud_code(QuarkError::AccountMismatch),"ACCOUNT_MISMATCH");assert_eq!(cloud_code(QuarkError::SessionRenewedRetryOperation),"CLOUD_RESULT_UNCERTAIN");assert_eq!(cloud_code(QuarkError::InvalidInput("agent.error.fileConflict".into())),"FILE_CONFLICT");}

    #[test]
    fn baidu_upload_errors_preserve_verification_and_uncertain_commit() {
        for (stage, code, next_action) in [
            (BaiduUploadAuthStage::Token, "CLOUD_UPLOAD_SESSION_REQUIRED", "verify_cloud_upload_in_browser"),
            (BaiduUploadAuthStage::Precreate, "CLOUD_UPLOAD_VERIFICATION_REQUIRED", "verify_cloud_upload_in_browser"),
            (BaiduUploadAuthStage::Commit, "CLOUD_RESULT_UNCERTAIN", "inspect_desktop"),
        ] {
            let actual = cloud_code(QuarkError::BaiduUploadAuthorizationRequired(stage));
            assert_eq!(actual, code);
            let error = ApiError::new(actual, "en");
            assert_eq!(error.next_action.as_deref(), Some(next_action));
            assert!(!error.retryable);
        }
    }
}
