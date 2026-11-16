//! Prepared resource capability: metadata first, durable acceptance before execution.
//! These internal plans never contain cookies, access codes or signed download URLs.
use crate::{cli_protocol::{Request,ResourcePrepareInput,ResourceConfirmInput,PreparedResourceDto,
    ResourceChoiceDto,ResourceConfirmDto},commands::AppState,cloud::{CloudProvider,QuarkError},
    DownloadTask,NewTask,TaskStatus};
use serde::{Deserialize,Serialize};
use serde_json::Value;
use sha2::{Digest,Sha256};
use std::{collections::HashSet,path::Path,sync::Arc,time::{Duration,SystemTime,UNIX_EPOCH}};
use tauri::{AppHandle,Manager};
use uuid::Uuid;

type ResultCode<T> = Result<T,&'static str>;
const TTL_SECONDS:i64=900;
static DISPATCH_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct ResourcePlan {
    pub id:String,
    pub expires_at:i64,
    pub kind:String,
    pub choices:Vec<ResourceSelection>,
    pub effects:Vec<String>,
    #[serde(default)]
    pub bt:Option<crate::cli_bt::BtPreparation>,
}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct ResourceSelection {
    pub id:String,
    pub name:String,
    pub kind:String,
    pub size_bytes:Option<u64>,
    pub detail:Option<String>,
    pub target:ResourceTarget,
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(tag="kind",rename_all="snake_case")]
pub(crate) enum ResourceTarget {
    Video {page:String,selector:String,media_id:String,name:String,size_bytes:Option<u64>},
    Cloud {provider:CloudProvider,account_id:String,file_id:String,name:String,size_bytes:Option<u64>,folder:bool},
    Share {source:String,name:String,size_bytes:Option<u64>},
    BtFile {index:u32},
    Bt {metadata:crate::cli_bt::BtPreparation,indices:Vec<u32>},
}
impl ResourceTarget {
    pub(crate) fn new_task(&self)->ResultCode<NewTask> {
        let (source,name,kind)=match self {
            Self::Video{page,name,..}=>(page.clone(),name.clone(),"web_video"),
            Self::Cloud{provider,file_id,name,folder:false,..}=>(format!("cloud://{}/{}",provider.key(),crate::cloud::encoded(file_id)),name.clone(),"cloud"),
            Self::Share{source,name,..}=>(source.clone(),name.clone(),"cloud"),
            Self::Bt{metadata,..}=>(metadata.source.clone(),metadata.name.clone(),metadata.kind.as_str()),
            Self::Cloud{folder:true,..}|Self::BtFile{..}=>return Err("INVALID_ARGUMENT"),
        };
        Ok(NewTask {source,name,kind:kind.into(),is_private:false})
    }
    fn size_bytes(&self)->Option<u64> {match self {
        Self::Video{size_bytes,..}|Self::Cloud{size_bytes,..}|Self::Share{size_bytes,..}=>*size_bytes,
        Self::Bt{metadata,indices}=>Some(metadata.files.iter().filter(|f|indices.contains(&f.index)).map(|f|f.length).sum()),
        Self::BtFile{..}=>None,
    }}
}
impl ResourcePlan {
    fn public(&self)->PreparedResourceDto {PreparedResourceDto {
        preparation_id:self.id.clone(),expires_at:self.expires_at,kind:self.kind.clone(),effects:self.effects.clone(),
        resources:self.choices.iter().map(|c|ResourceChoiceDto {selection_id:c.id.clone(),name:c.name.clone(),kind:c.kind.clone(),size_bytes:c.size_bytes,detail:c.detail.clone()}).collect(),
    }}
}
fn now()->i64 {SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs().min(i64::MAX as u64) as i64}
fn choice(name:String,kind:&str,size_bytes:Option<u64>,detail:Option<String>,target:ResourceTarget)->ResourceSelection {
    ResourceSelection {id:Uuid::new_v4().to_string(),name:safe_name(&name),kind:kind.into(),size_bytes,detail,target}
}
fn safe_name(name:&str)->String {crate::cloud::commands::safe_filename(&name.chars().filter(|c|!c.is_control()).take(256).collect::<String>())}
pub(crate) fn cloud_error(error:QuarkError)->&'static str {match error {
    QuarkError::NotAuthorized|QuarkError::AuthorizationExpired|QuarkError::WebAuthorizationRequired|
    QuarkError::CaptchaRequired|QuarkError::AccountMismatch|QuarkError::SessionRenewedRetryOperation|
    QuarkError::PikpakOperationVerificationRequired(_)|QuarkError::AuthorizationPending|
    QuarkError::BaiduOperationVerificationRequired=>"AUTH_REQUIRED",
    QuarkError::ProviderUnsupported=>"UNSUPPORTED",
    QuarkError::InvalidInput(ref message) if message=="agent.error.cloudWriteRequired"=>"CLOUD_WRITE_REQUIRED",
    QuarkError::DownloadEngineUnavailable=>"ENGINE_UNAVAILABLE",
    _=>"ACTION_FAILED",
}}
fn video_error(error:String)->&'static str {match error.as_str() {
    "video.error.unsupported"=>"UNSUPPORTED",
    "video.error.previewExpired"=>"PREPARATION_EXPIRED",
    "FORBIDDEN"=>"FORBIDDEN",
    _=>"ACTION_FAILED",
}}
fn validate_text(value:&str,max:usize)->ResultCode<()> {
    if value.len()>max || value.chars().any(char::is_control) {Err("INVALID_ARGUMENT")} else {Ok(())}
}
fn validate_url(value:&str)->ResultCode<()> {
    validate_text(value,8192)?;
    let url=url::Url::parse(value).map_err(|_|"INVALID_ARGUMENT")?;
    if !matches!(url.scheme(),"http"|"https") || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {return Err("INVALID_ARGUMENT")}
    Ok(())
}

pub(crate) async fn handle(app:AppHandle,request:&Request)->ResultCode<Value> {
    let state=app.state::<AppState>();
    if !state.settings.lock().map_err(|_|"INTERNAL_ERROR")?.agent_access.enabled {return Err("HOST_DISABLED")}
    match request.command.as_str() {
        "resource_prepare"=> {
            let input=request.prepare.as_ref().ok_or("INVALID_ARGUMENT")?;
            let plan=tokio::time::timeout(Duration::from_secs(30),prepare(&app,input)).await.map_err(|_|"TIMEOUT")??;
            state.store.save_agent_resource_plan(&plan,now())?;
            serde_json::to_value(plan.public()).map_err(|_|"INTERNAL_ERROR")
        }
        "resource_confirm"=> {
            let result=confirm(&app,request.confirm.as_ref().ok_or("INVALID_ARGUMENT")?).await?;
            serde_json::to_value(result).map_err(|_|"INTERNAL_ERROR")
        }
        _=>Err("INVALID_ARGUMENT"),
    }
}

async fn prepare(app:&AppHandle,input:&ResourcePrepareInput)->ResultCode<ResourcePlan> {
    validate_text(&input.input,8192)?;
    if let Some(code)=&input.access_code {validate_text(code,128)?;}
    let state=app.state::<AppState>();
    let mut bt=None;
    let choices=match input.kind.as_str() {
        "torrent"=> {
            if input.provider.is_some()||input.parent_id.is_some()||input.access_code.is_some()||input.input.is_empty(){return Err("INVALID_ARGUMENT")}
            let metadata=crate::cli_bt::prepare(app,&input.input).await?;
            let choices=metadata.files.iter().map(|file|choice(file.path.clone(),"file",Some(file.length),None,ResourceTarget::BtFile{index:file.index})).collect();
            bt=Some(metadata);
            choices
        }
        "video"=> {
            if input.provider.is_some()||input.parent_id.is_some()||input.access_code.is_some(){return Err("INVALID_ARGUMENT")}
            validate_url(&input.input)?;
            let preview=crate::web_video::prepare_agent_video(&state,&input.input).await.map_err(video_error)?;
            preview.into_iter().map(|v| {
                let name=v.name;
                choice(name.clone(),"video_format",v.size_bytes,Some(v.detail),ResourceTarget::Video {
                    page:v.page,selector:v.selector,media_id:v.media_id,name,size_bytes:v.size_bytes,
                })
            }).collect()
        }
        "cloud"=> {
            if !input.input.is_empty()||input.access_code.is_some(){return Err("INVALID_ARGUMENT")}
            let provider=input.provider.as_deref().and_then(|key|crate::cloud::ProviderRegistry::global().by_key(key)).map(|d|d.provider).filter(|p|p.is_enabled()).ok_or("INVALID_ARGUMENT")?;
            let expected=if let Some(parent)=input.parent_id.as_deref() {
                validate_text(parent,128)?;
                match state.store.agent_resource_folder(parent,now())? {
                    ResourceTarget::Cloud{provider:p,account_id,file_id,folder:true,..} if p==provider=>Some((account_id,file_id)),
                    _=>return Err("INVALID_ARGUMENT"),
                }
            }else{None};
            let (account,listing)=crate::cloud::commands::prepare_agent_cloud_directory(app.clone(),provider,expected).await.map_err(cloud_error)?;
            listing.items.into_iter().map(|file| {
                let folder=file.kind==crate::cloud::CloudFileKind::Folder;
                let name=safe_name(&file.name);
                choice(name.clone(),if folder{"folder"}else{"file"},file.size,None,ResourceTarget::Cloud {
                    provider,account_id:account.clone(),file_id:file.id,name,size_bytes:file.size,folder,
                })
            }).collect()
        }
        "share"=> {
            if input.provider.is_some()||input.parent_id.is_some(){return Err("INVALID_ARGUMENT")}
            validate_url(&input.input)?;
            let candidate=crate::parser::extract_cloud_share_candidate(&input.input,input.access_code.as_deref()).map_err(|_|"INVALID_ARGUMENT")?.ok_or("UNSUPPORTED")?;
            let confirmation=match crate::cloud::commands::inspect_cloud_share(app.clone(),candidate).await.map_err(cloud_error)? {
                crate::cloud::share::ShareConfirmationOutcome::Confirmation(c)|crate::cloud::share::ShareConfirmationOutcome::AutoSubmit{confirmation:c,..}=>c,
                crate::cloud::share::ShareConfirmationOutcome::AccessCodeRequired=>return Err("ACCESS_CODE_REQUIRED"),
            };
            let cloud=app.state::<Arc<crate::cloud::commands::CloudState>>();
            let ids=confirmation.resources.iter().map(|r|r.id.clone()).collect::<Vec<_>>();
            let selected=crate::cloud::share::selected_share_download(&cloud.share_plans,&confirmation.plan_source,&ids).map_err(cloud_error)?;
            selected.entries.into_iter().map(|entry| {
                let name=safe_name(&entry.name);
                choice(name.clone(),"file",entry.size,None,ResourceTarget::Share{source:entry.source,name,size_bytes:entry.size})
            }).collect()
        }
        _=>return Err("INVALID_ARGUMENT"),
    };
    let choices:Vec<ResourceSelection>=choices;
    if choices.len()>1000 {return Err("RESOURCE_LIMIT")}
    Ok(ResourcePlan {id:Uuid::new_v4().to_string(),expires_at:now()+TTL_SECONDS,kind:input.kind.clone(),choices,bt,
        effects:if input.kind=="share" {vec!["direct_download_only".into(),"cloud_transfer_not_authorized".into()]} else {vec!["download_after_confirmation".into()]},
    })
}

fn confirm_fingerprint(input:&ResourceConfirmInput,directory:&Path)->ResultCode<String> {
    let mut selected=input.selection_ids.clone();selected.sort();
    Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&("resource_confirm",&input.preparation_id,selected,directory.to_string_lossy())).map_err(|_|"INTERNAL_ERROR")?)))
}
async fn confirm(app:&AppHandle,input:&ResourceConfirmInput)->ResultCode<ResourceConfirmDto> {
    if input.request_id.is_empty()||input.request_id.len()>128||!input.request_id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.:".contains(&b)) {return Err("INVALID_ARGUMENT")}
    Uuid::parse_str(&input.preparation_id).map_err(|_|"INVALID_ARGUMENT")?;
    if input.selection_ids.is_empty()||input.selection_ids.len()>100 {return Err("INVALID_ARGUMENT")}
    let unique:HashSet<_>=input.selection_ids.iter().collect();
    if unique.len()!=input.selection_ids.len()||input.selection_ids.iter().any(|id|Uuid::parse_str(id).is_err()){return Err("INVALID_ARGUMENT")}
    let state=app.state::<AppState>();
    let directory=crate::agent_access::authorize_directory(&state,Path::new(&input.directory))?;
    let fingerprint=confirm_fingerprint(input,&directory)?;
    if let Some(receipt)=state.store.agent_resource_receipt(&input.request_id,&fingerprint)? {return Ok(receipt)}
    let plan=state.store.agent_resource_plan(&input.preparation_id,now())?;
    let selected=input.selection_ids.iter().map(|id|plan.choices.iter().find(|c|&c.id==id).cloned().ok_or("INVALID_ARGUMENT")).collect::<Result<Vec<_>,_>>()?;
    if plan.kind=="video"&&selected.len()!=1 {return Err("INVALID_ARGUMENT")}
    for selected in &selected {
        if !matches!(selected.target,ResourceTarget::BtFile{..}) {selected.target.new_task()?;}
        match &selected.target {
            ResourceTarget::Cloud{provider,account_id,..}=>crate::cloud::commands::require_download_account(app,*provider,account_id).map_err(cloud_error)?,
            ResourceTarget::Share{source,..}=> {crate::cloud::commands::resolve_agent_share_read_only(app.clone(),source).await.map_err(cloud_error)?;},
            _=>(),
        }
    }
    // Recheck revocation after metadata/provider work and before durable acceptance.
    crate::agent_access::authorize_directory(&state,&directory)?;
    state.store.accept_agent_resources(&plan,&selected,&directory,&input.request_id,&fingerprint,now())
}

pub(crate) async fn control_pending(app:&AppHandle,id:Uuid,action:&str)->ResultCode<Option<DownloadTask>> {
    let state=app.state::<AppState>();
    let Some(dir)=state.store.agent_resource_directory(id)? else{return Ok(None)};
    let _gate=DISPATCH_GATE.lock().await;
    state.web_video.bind_agent_application(app.clone());
    if action=="resume" {crate::agent_access::authorize_directory(&state,&dir)?;}
    state.store.control_agent_resource_pending(id,action)
}

/// Rechecked when a delayed provider/video job reaches its execution slot.
pub(crate) fn authorize_task(app:&AppHandle,id:Uuid)->ResultCode<()> {
    let state=app.state::<AppState>();
    if let Some(directory)=state.store.agent_resource_directory(id)? {
        crate::agent_access::authorize_directory(&state,&directory)?;
        for file in state.store.files(id).map_err(|_|"INTERNAL_ERROR")? {
            crate::agent_access::authorize_file(&state,Path::new(&file.path))?;
        }
    }
    Ok(())
}

/// One worker per host. Runtime state coordinates only polling; receipts are SQLite.
pub(crate) fn start(app:AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(state)=app.try_state::<AppState>() else {break};
            let _=state.store.settle_agent_resource_dispatches();
            if let Ok(pending)=state.store.agent_resource_pending() {
                for (id,directory,target) in pending {
                    let _gate=DISPATCH_GATE.lock().await;
                    let Ok(task)=state.store.find(id) else{continue};
                    let stopping=matches!(target,ResourceTarget::Bt{..})&&matches!(task.status,TaskStatus::Paused|TaskStatus::Cancelled);
                    if task.is_private||(!stopping&&!matches!(task.status,TaskStatus::Parsing|TaskStatus::Queued)){continue;}
                    if !stopping {if let Err(code)=crate::agent_access::authorize_directory(&state,&directory) {
                        if matches!(target,ResourceTarget::Bt{..}) {let _=state.store.block_agent_resource_authorization(id,code);}
                        else {let _=state.store.fail_with_code(id,Some(code),crate::cli_protocol::message(code,"en"));}
                        continue;
                    }}
                    if !state.runtime.status().available {continue;}
                    let claimed=if stopping{state.store.claim_agent_resource_stop(id)}else{state.store.claim_agent_resource(id)};
                    if !claimed.unwrap_or(false){continue;}
                    let outcome=dispatch_existing(&app,id,&directory,&target).await;
                    if let Err(code)=outcome {
                        if matches!(target,ResourceTarget::Bt{..})&&matches!(code,"ENGINE_UNAVAILABLE"|"TIMEOUT"|"UNKNOWN") {
                            let _=state.store.retry_agent_resource_dispatch(id);
                        }else{
                            let _=state.store.fail_with_code(id,Some(code),crate::cli_protocol::message(code,"en"));
                            let _=state.store.finish_agent_resource_dispatch(id);
                        }
                    }else if matches!(target,ResourceTarget::Bt{..}) {
                        if task.status==TaskStatus::Paused {let _=state.store.mark_agent_resource_stopped(id);}
                        else {let _=state.store.finish_agent_resource_dispatch(id);}
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}

async fn dispatch_existing(app:&AppHandle,id:Uuid,directory:&Path,target:&ResourceTarget)->ResultCode<()> {
    let state=app.state::<AppState>();
    let task=state.store.find(id).map_err(|_|"INTERNAL_ERROR")?;
    if task.is_private {return Err("FORBIDDEN")}
    if let ResourceTarget::Bt{metadata,indices}=target {
        let options=if matches!(task.status,TaskStatus::Paused|TaskStatus::Cancelled) {
            crate::UriSubmissionOptions{directory:Some(directory.to_path_buf()),..Default::default()}
        }else{crate::commands::download_submission_options_for_directory(&state,Some(&directory.to_string_lossy())).map_err(|_|"INVALID_ARGUMENT")?};
        crate::cli_bt::dispatch(app,id,metadata,indices,&options).await?;
        return Ok(())
    }
    crate::agent_access::authorize_directory(&state,directory)?;
    if !matches!(task.status,TaskStatus::Parsing|TaskStatus::Queued) {return Err("FORBIDDEN")}
    let output=crate::output_path::reserve_unique_output_path(&state.store,id,directory,&task.name).map_err(|_|"ACTION_FAILED")?;
    crate::agent_access::authorize_file(&state,&output)?;
    let mut record=task.clone();record.total_length=target.size_bytes();
    let record=state.store.apply_transfer_snapshot_with_files(record,&[(1,output.to_string_lossy().into_owned(),target.size_bytes().unwrap_or(0),0,true)]).map_err(|_|"INTERNAL_ERROR")?;
    match target {
        ResourceTarget::Video{..}=> {state.web_video.bind_agent_application(app.clone());state.web_video.start_agent_task(&state,record).await.map_err(video_error)?;},
        ResourceTarget::Cloud{provider,account_id,..}=> {
            crate::cloud::commands::require_download_account(app,*provider,account_id).map_err(cloud_error)?;
            let options=crate::commands::download_submission_options_for_directory(&state,Some(&directory.to_string_lossy())).map_err(|_|"INVALID_ARGUMENT")?;
            crate::cloud::commands::queue_cloud_dispatch(app.clone(),&state,record,options).await.map_err(cloud_error)?;
        }
        ResourceTarget::Share{source,..}=> {
            // Link locally for result projection; no transfer or cleanup grant is made.
            let cloud=app.state::<Arc<crate::cloud::commands::CloudState>>();
            cloud.share_plans.link_local_task(source,id).map_err(cloud_error)?;
            let options=crate::commands::download_submission_options_for_directory(&state,Some(&directory.to_string_lossy())).map_err(|_|"INVALID_ARGUMENT")?;
            crate::cloud::commands::queue_cloud_dispatch(app.clone(),&state,record,options).await.map_err(cloud_error)?;
        }
        ResourceTarget::BtFile{..}|ResourceTarget::Bt{..}=>return Err("INVALID_ARGUMENT"),
    }
    // Leave phase=dispatching until an engine binding or terminal state is durable.
    // On restart, uncertain submissions become CLI_RECOVERY_REQUIRED rather than replay.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_resource_public_preview_hides_provider_identity_and_download_internals() {
        let plan=ResourcePlan{id:Uuid::new_v4().to_string(),expires_at:123,kind:"cloud".into(),effects:vec!["download_after_confirmation".into()],bt:None,
            choices:vec![choice("sample.txt".into(),"file",Some(9),None,ResourceTarget::Cloud{provider:CloudProvider::Quark,account_id:"private-account".into(),file_id:"private-file-id".into(),name:"sample.txt".into(),size_bytes:Some(9),folder:false})]};
        let public=serde_json::to_string(&plan.public()).unwrap();
        assert!(!public.contains("private-account"));assert!(!public.contains("private-file-id"));assert!(!public.contains("cloud://"));
        assert!(public.contains("sample.txt"));assert!(public.contains("selection_id"));
    }
    #[test]
    fn agent_resource_confirmation_fingerprint_ignores_selection_order_but_binds_destination() {
        let root=tempfile::tempdir().unwrap();
        let mut input=ResourceConfirmInput{preparation_id:Uuid::new_v4().to_string(),selection_ids:vec!["one".into(),"two".into()],directory:root.path().to_string_lossy().into_owned(),request_id:"request".into()};
        let fingerprint=confirm_fingerprint(&input,root.path()).unwrap();input.selection_ids.reverse();
        assert_eq!(fingerprint,confirm_fingerprint(&input,root.path()).unwrap());
        assert_ne!(fingerprint,confirm_fingerprint(&input,&root.path().join("other")).unwrap());
    }
    #[test]
    fn agent_resource_url_and_auth_errors_are_bounded_and_stable() {
        assert!(validate_url("https://example.test/watch?v=1").is_ok());
        for value in ["file:///tmp/secret","https://user:secret@example.test/video","https://example.test/\nvideo"] {assert_eq!(validate_url(value),Err("INVALID_ARGUMENT"));}
        assert_eq!(cloud_error(QuarkError::AuthorizationExpired),"AUTH_REQUIRED");
        assert_eq!(cloud_error(QuarkError::InvalidInput("agent.error.cloudWriteRequired".into())),"CLOUD_WRITE_REQUIRED");
    }
}
