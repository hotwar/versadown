//! VD-owned user interaction and durable continuation of a structured Agent request.
use std::{path::Path, time::{SystemTime, UNIX_EPOCH, Duration}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, WebviewWindow, Emitter};
use tauri_plugin_dialog::DialogExt;
use crate::{commands::AppState, cli_protocol::{Request, Response},
    cli_cloud_protocol::{CloudRequest, CloudPrepareInput, CloudAuthorizationDto},
    agent_interaction_protocol::{InteractionRequest, InteractionDto},
    agent_permissions::LocalAccessGrant, cloud::CloudProvider, cli_resources::ResourceTarget};

type CodeResult<T> = Result<T, &'static str>;
const WINDOW: &str = "agent-access";
const WINDOW_DOCUMENT: &str = "index.html?window=agent-access";
pub(crate) fn now() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PendingInteraction {
    pub revision: u64,
    pub id: String, pub fingerprint: String, pub status: String, pub kind: String,
    pub request: Request, pub provider: Option<CloudProvider>, pub account_id: Option<String>,
    pub account_label: Option<String>, pub grant: LocalAccessGrant, pub needs_directory: bool,
    pub expires_at: i64, pub authorization_id: Option<String>, pub result: Option<Response>,
}
#[derive(Serialize)]
#[serde(rename_all="camelCase")]
pub(crate) struct InteractionView {
    id: String, status: String, kind: String, command: String, provider: Option<String>,
    account_label: Option<String>, download_directories: Vec<String>, upload_files: Vec<String>,
    needs_directory: bool, expires_at: i64, error_code: Option<String>,
}
impl PendingInteraction {
    fn view(&self) -> InteractionView {
        let command=match &self.request.cloud {Some(CloudRequest::Prepare{input:CloudPrepareInput::Upload{..}})=>"upload",
            Some(CloudRequest::Prepare{input:CloudPrepareInput::CreateShare{..}})=>"create_share",_=>self.request.command.as_str()};
        InteractionView { id:self.id.clone(), status:self.status.clone(),kind:self.kind.clone(),command:command.into(),
            provider:self.provider.map(|p|p.key().into()),account_label:self.account_label.clone(),
            download_directories:self.grant.download_directories.clone(),upload_files:self.grant.upload_files.clone(),
            needs_directory:self.needs_directory,expires_at:self.expires_at,
            error_code:self.result.as_ref().and_then(|r|r.error.as_ref()).map(|e|e.code.clone()) }
    }
    fn is_pending(&self) -> bool {
        matches!(self.status.as_str(), "awaiting_user" | "awaiting_login" | "resuming")
    }

    fn dto(&self) -> InteractionDto {
        InteractionDto {id:self.id.clone(),status:self.status.clone(),kind:self.kind.clone(),provider:self.provider.map(|p|p.key().into()),
            expires_at:self.expires_at,poll_interval_ms:if self.is_pending() { 1000 } else { 0 },command:self.request.command.clone(),
            cloud_action:self.request.cloud.as_ref().and_then(|r|serde_json::to_value(r).ok()?.get("action")?.as_str().map(str::to_owned)),result:self.result.clone()}
    }
}
#[derive(Default)]
struct Displayed(std::sync::Mutex<Option<String>>);

fn fingerprint(request: &Request) -> String {
    let mut request=request.clone();request.lang=None;
    format!("{:x}",Sha256::digest(serde_json::to_vec(&request).expect("request JSON")))
}
fn target_account(target: ResourceTarget) -> CodeResult<(CloudProvider,Option<String>)> {
    match target {ResourceTarget::Cloud{provider,account_id,..}=>Ok((provider,Some(account_id))),_=>Err("INVALID_ARGUMENT")}
}
fn cloud_target(app:&AppHandle,request:&Request)->CodeResult<Option<(CloudProvider,Option<String>)>> {
    let state=app.state::<AppState>();
    if let Some(input)=&request.prepare {
        if input.kind=="cloud" {
            if let Some(parent)=&input.parent_id {return target_account(state.store.agent_resource_selection(parent,now())?).map(Some)}
            let provider=crate::cloud::ProviderRegistry::global().by_key(input.provider.as_deref().ok_or("INVALID_ARGUMENT")?)
                .filter(|p|p.provider.is_enabled()).ok_or("UNSUPPORTED")?.provider;
            return Ok(Some((provider,None)))
        }
    }
    if let Some(input)=&request.confirm {
        let plan=state.store.agent_resource_plan(&input.preparation_id,now())?;
        if let Some(choice)=plan.choices.iter().find(|c|input.selection_ids.contains(&c.id)) {
            if matches!(&choice.target,ResourceTarget::Cloud{..}) {return target_account(choice.target.clone()).map(Some)}
        }
    }
    match &request.cloud {
        Some(CloudRequest::Prepare{input})=> {
            let id=match input {CloudPrepareInput::Upload{destination_id,..}=>destination_id,
                CloudPrepareInput::CreateShare{selection_ids,..}=>selection_ids.first().ok_or("INVALID_ARGUMENT")?};
            target_account(state.store.agent_resource_selection(id,now())?).map(Some)
        }
        Some(CloudRequest::Confirm{preparation_id,..})=>{
            let plan=state.store.agent_cloud_plan(preparation_id,now())?; Ok(Some((plan.provider,Some(plan.account_id))))
        }
        _=>Ok(None)
    }
}
fn local_requirements(app:&AppHandle,request:&Request)->CodeResult<(LocalAccessGrant,bool)> {
    let state=app.state::<AppState>();let mut grant=LocalAccessGrant::default();let mut missing=false;
    let mut directories=Vec::new();
    if request.command=="add" {
        crate::cli_service::direct_source(request.url.as_deref().ok_or("INVALID_ARGUMENT")?)?;
        if !crate::cli_service::valid_request_id(request.request_id.as_deref().ok_or("INVALID_ARGUMENT")?) {return Err("INVALID_ARGUMENT")}
        directories.push(request.directory.as_deref().unwrap_or(""));
    }
    if let Some(items)=&request.items {for item in items {
        // Invalid individual jobs remain per-item failures in batch_add. They
        // must neither block valid jobs nor trigger an unrelated permission UI.
        if crate::cli_service::direct_source(&item.url).is_ok(){directories.push(&item.directory);}
    }}
    if let Some(confirm)=&request.confirm {directories.push(&confirm.directory);}
    for value in directories {
        if value.trim().is_empty(){missing=true;continue}
        let checked=(||->CodeResult<()> {
        let path=Path::new(value);
        match crate::agent_access::authorize_directory(&state,path) {
            Ok(_)=>(),Err("FORBIDDEN")=>{
                crate::agent_access::validate_components(path)?;
                if !path.is_dir(){return Err("INVALID_ARGUMENT")}
                if !grant.download_directories.contains(&value.to_owned()){grant.download_directories.push(value.into());}
            },Err(code)=>return Err(code)
        }
        Ok(())
        })();
        if request.items.is_none(){checked?;}
    }
    let torrent_path = request.prepare.as_ref().filter(|input|input.kind=="torrent")
        .and_then(|input| {
            if Path::new(&input.input).is_absolute(){Some(input.input.clone())}
            else {url::Url::parse(&input.input).ok().filter(|url|url.scheme()=="file")
                .and_then(|url|url.to_file_path().ok()).and_then(|path|path.to_str().map(str::to_owned))}
        });
    let mut read_paths=Vec::new();
    if let Some(path)=&torrent_path {read_paths.push(path);}
    if let Some(CloudRequest::Prepare{input:CloudPrepareInput::Upload{paths,..}})=&request.cloud {
        if paths.is_empty()||paths.len()>20 {return Err("INVALID_ARGUMENT")}
        read_paths.extend(paths);
    }
    for value in read_paths {
            let path=Path::new(value);
            match crate::agent_access::authorize_upload_file(&state,path) {
                Ok(_)=>(),Err("FORBIDDEN")=>{
                    crate::agent_access::validate_components(path)?;
                    if path.is_dir(){return Err("UNSUPPORTED")}
                    if !path.is_file(){return Err("INVALID_ARGUMENT")}
                    grant.upload_files.push(value.clone());
                },Err(code)=>return Err(code)
            }
    }
    Ok((grant,missing))
}

async fn preflight(app:&AppHandle,request:&Request)->CodeResult<Option<PendingInteraction>> {
    let (grant,needs_directory)=local_requirements(app,request)?;
    let mut record=PendingInteraction{revision:0,id:uuid::Uuid::new_v4().to_string(),fingerprint:fingerprint(request),status:"awaiting_user".into(),kind:"local_access".into(),
        request:request.clone(),provider:None,account_id:None,account_label:None,grant,needs_directory,expires_at:now()+600,authorization_id:None,result:None};
    let mut cloud_required=false;
    if let Some((provider,expected))=cloud_target(app,request)? {
        record.provider=Some(provider);record.account_id=expected.clone();
        match crate::cloud::commands::cloud_account(app.clone(),provider).await {
            Ok(account)=> {
                if expected.as_deref().is_some_and(|id|id!=account.id){return Err("ACCOUNT_MISMATCH")}
                record.account_id=Some(account.id.clone());record.account_label=Some(account.display_name);
                if crate::agent_access::require_cloud_account(&app.state::<AppState>(),provider.key(),&account.id).is_err() {
                    record.kind="cloud_account".into();cloud_required=true;
                }
            }
            Err(failure)=>{
                let code=crate::cli_cloud::cloud_code(failure);
                if code!="AUTH_REQUIRED" {return Err(code)}
                record.kind="cloud_login".into();cloud_required=true;
            }
        }
    }
    Ok((cloud_required||needs_directory||!record.grant.download_directories.is_empty()||!record.grant.upload_files.is_empty()).then_some(record))
}

fn pending_response(record:&PendingInteraction,lang:&str)->Response {
    if let Some(result)=&record.result {
        let mut result=result.clone();
        if let Some(error)=&mut result.error {error.interaction_id=Some(record.id.clone());}
        return result
    }
    let code = match record.status.as_str() {
        "cancelled" => "TASK_CANCELLED",
        "expired" => "INTERACTION_EXPIRED",
        "interrupted" => "INTERRUPTED",
        _ if record.is_pending() => "USER_ACTION_REQUIRED",
        _ => "INTERNAL_ERROR",
    };
    let mut response = Response::failure(code, lang);
    if let Some(error) = &mut response.error {
        error.interaction_id = Some(record.id.clone());
        if record.is_pending() {
            error.retry_after_ms = Some(1000);
        }
    }
    response
}
pub(crate) async fn execute(app:AppHandle,request:Request)->Response {
    let lang=request.lang.as_deref().unwrap_or("en");
    let fingerprint=fingerprint(&request);
    // An identical request observes its receipt; polling/retries never reopen a prompt.
    match app.state::<AppState>().store.find_agent_interaction(&fingerprint) {
        Ok(Some(record))=>{
            let preparation=request.command=="prepare"||matches!(request.cloud,Some(CloudRequest::Prepare{..}));
            if preparation&&record.status=="succeeded" {
                if let Err(code)=app.state::<AppState>().store.release_agent_interaction_lookup(&record.id){return Response::failure(code,lang)}
            }else{return pending_response(&record,lang)}
        },Err(code)=>return Response::failure(code,lang),_=>()
    }
    // Confirmation owns a durable receipt lookup before plan expiry checks.
    // Let it replay an accepted operation (or accept already-authorized work)
    // before asking for permissions. Permission/auth failures have no write.
    if (request.command=="confirm"&&request.confirm.as_ref().is_some_and(|c|!c.directory.trim().is_empty()))
        || matches!(&request.cloud,Some(CloudRequest::Confirm{..})) {
        let response=crate::cli_service::execute_business(app.clone(),request.clone()).await;
        if !response.error.as_ref().is_some_and(|e|matches!(e.code.as_str(),"FORBIDDEN"|"AUTH_REQUIRED"|"CLOUD_PERMISSION_REQUIRED")) {
            return response
        }
    }
    match preflight(&app,&request).await {
        Ok(Some(record))=>{
            if let Err(code)=app.state::<AppState>().store.insert_agent_interaction(&record){return Response::failure(code,lang)}
            show_next(&app);
            pending_response(&record,lang)
        }
        Ok(None)=>{
            let result=crate::cli_service::execute_business(app.clone(),request.clone()).await;
            // Providers can discover an expired session only when listing the
            // requested folder. Only an explicit auth failure starts login;
            // network failures and uncertain writes retain their own outcome.
            if result.error.as_ref().is_some_and(|e|e.code=="AUTH_REQUIRED") {
                if let Ok(Some((provider,account_id)))=cloud_target(&app,&request) {
                    let record=PendingInteraction{revision:0,id:uuid::Uuid::new_v4().to_string(),fingerprint,
                        status:"awaiting_user".into(),kind:"cloud_login".into(),request:request.clone(),provider:Some(provider),account_id,
                        account_label:None,grant:LocalAccessGrant::default(),needs_directory:false,expires_at:now()+600,authorization_id:None,result:None};
                    if let Err(code)=app.state::<AppState>().store.insert_agent_interaction(&record){return Response::failure(code,lang)}
                    show_next(&app);return pending_response(&record,lang)
                }
            }
            result
        },
        Err(code)=>Response::failure(code,lang)
    }
}

fn trusted_origin(label: &str, url: &url::Url, dev_url: Option<&url::Url>) -> bool {
    if label != WINDOW {
        return false;
    }
    let bundled = match (url.scheme(), url.host_str()) {
        ("tauri", Some("localhost")) => Some("tauri://localhost/"),
        ("http", Some("tauri.localhost")) => Some("http://tauri.localhost/"),
        ("https", Some("tauri.localhost")) => Some("https://tauri.localhost/"),
        _ => None,
    }
    .and_then(|base| url::Url::parse(base).ok());

    bundled.as_ref().into_iter().chain(dev_url).any(|base| {
        base.join(WINDOW_DOCUMENT).is_ok_and(|expected| expected == *url)
    })
}
fn trusted_window(window: &WebviewWindow) -> CodeResult<()> {
    // Match the same configured URL that Tauri resolves for WebviewUrl::App.
    let dev_url = if tauri::is_dev() {
        window.app_handle().config().build.dev_url.as_ref()
    } else {
        None
    };
    let url = window.url().map_err(|_| "FORBIDDEN")?;
    trusted_origin(window.label(), &url, dev_url).then_some(()).ok_or("FORBIDDEN")
}
fn displayed(app:&AppHandle,id:Option<&str>)->CodeResult<PendingInteraction> {
    let active=app.state::<Displayed>().0.lock().map_err(|_|"INTERNAL_ERROR")?.clone().ok_or("NOT_FOUND")?;
    if id.is_some_and(|id|id!=active){return Err("FORBIDDEN")}
    app.state::<AppState>().store.agent_interaction(&active)
}
fn show_next(app:&AppHandle) {
    let state=app.state::<Displayed>();
    let Ok(mut active)=state.0.lock() else{return};
    if active.is_some(){return}
    let Some(record)=app.state::<AppState>().store.pending_agent_interactions().ok().and_then(|r|r.into_iter().find(|r|r.status=="awaiting_user")) else{return};
    *active=Some(record.id.clone());drop(active);
    let app=app.clone();
    tauri::async_runtime::spawn_blocking(move||{
        if let Some(window)=app.get_webview_window(WINDOW){let _=window.show();let _=window.set_focus();let _=app.emit_to(WINDOW,"agent-interaction-changed",());return}
        match tauri::WebviewWindowBuilder::new(&app,WINDOW,tauri::WebviewUrl::App(WINDOW_DOCUMENT.into()))
            .title("VD · Agent").inner_size(560.0,640.0).resizable(true).build() {
            Ok(window)=>{let closed=app.clone();window.on_window_event(move|event|{
                if matches!(event,tauri::WindowEvent::Destroyed){
                    if let Ok(mut active)=closed.state::<Displayed>().0.lock(){
                        if let Some(id)=active.take(){
                            if let Ok(mut record)=closed.state::<AppState>().store.agent_interaction(&id){
                                if record.status=="awaiting_user"{record.status="cancelled".into();record.result=Some(Response::failure("TASK_CANCELLED",record.request.lang.as_deref().unwrap_or("en")));let _=closed.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_user");}
                            }
                        }
                    }
                    show_next(&closed);
                }
            });}
            Err(_)=>{
                if let Ok(mut record)=app.state::<AppState>().store.agent_interaction(&record.id){record.status="interrupted".into();let _=app.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_user");}
                if let Ok(mut active)=app.state::<Displayed>().0.lock(){*active=None;}
            }
        }
    });
}

#[tauri::command]
pub(crate) fn agent_interaction_view(window:WebviewWindow,app:AppHandle)->Result<Option<InteractionView>,String>{
    trusted_window(&window).map_err(str::to_owned)?;
    match displayed(&app,None){Ok(record)=>Ok(Some(record.view())),Err("NOT_FOUND")=>Ok(None),Err(code)=>Err(code.into())}
}
#[tauri::command]
pub(crate) async fn agent_interaction_choose_directory(window:WebviewWindow,app:AppHandle,interaction_id:String)->Result<InteractionView,String>{
    trusted_window(&window).map_err(str::to_owned)?;
    let mut record=displayed(&app,Some(&interaction_id)).map_err(str::to_owned)?;
    if record.status!="awaiting_user"||!record.needs_directory||record.expires_at<=now(){return Err("REQUEST_CONFLICT".into())}
    let picker=app.clone();let directory=tauri::async_runtime::spawn_blocking(move||picker.dialog().file().blocking_pick_folder()).await.map_err(|_|"ACTION_FAILED")?;
    if let Some(directory)=directory {
        let directory=directory.into_path().map_err(|_|"INVALID_ARGUMENT")?.to_str().ok_or("INVALID_ARGUMENT")?.to_owned();
        if record.request.command=="add"&&record.request.directory.as_deref().is_none_or(|d|d.trim().is_empty()){record.request.directory=Some(directory.clone());}
        if let Some(items)=&mut record.request.items {for item in items {if item.directory.trim().is_empty(){item.directory=directory.clone();}}}
        if let Some(confirm)=&mut record.request.confirm {if confirm.directory.trim().is_empty(){confirm.directory=directory.clone();}}
        record.grant.download_directories.push(directory);record.needs_directory=false;
        app.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_user").map_err(str::to_owned)?;
    }
    Ok(record.view())
}

#[tauri::command]
pub(crate) async fn agent_interaction_decide(window:WebviewWindow,app:AppHandle,interaction_id:String,allow:bool)->Result<InteractionView,String>{
    trusted_window(&window).map_err(str::to_owned)?;
    let mut record=displayed(&app,Some(&interaction_id)).map_err(str::to_owned)?;
    if record.status!="awaiting_user"||record.expires_at<=now(){return Err("REQUEST_CONFLICT".into())}
    if !allow {
        record.status="cancelled".into();record.result=Some(Response::failure("TASK_CANCELLED",record.request.lang.as_deref().unwrap_or("en")));
        app.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_user").map_err(str::to_owned)?;
        return Ok(record.view())
    }
    if record.needs_directory{return Err("INVALID_ARGUMENT".into())}
    record.grant=record.grant.validated().map_err(str::to_owned)?;
    record.status="resuming".into();
    app.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_user").map_err(str::to_owned)?;
    let result=record.view();
    tauri::async_runtime::spawn(async move {approve_and_continue(app,record).await;});
    Ok(result)
}
fn finish(app:&AppHandle,mut record:PendingInteraction,response:Response,previous:&str){
    record.status=if response.ok{"succeeded"}else{"failed"}.into();record.result=Some(response);
    let _=app.state::<AppState>().store.update_agent_interaction(&mut record,previous);
}
fn authorization_result(response: Response, lang: &str) -> Result<CloudAuthorizationDto, Response> {
    if !response.ok { return Err(response); }
    response.data.and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| Response::failure("PROTOCOL_ERROR", lang))
}

async fn approve_and_continue(app:AppHandle,mut record:PendingInteraction){
    let lang=record.request.lang.as_deref().unwrap_or("en").to_owned();
    let mut login_attempted=false;
    loop {
    if record.kind=="cloud_login" {
        login_attempted=true;
        let provider=record.provider.expect("cloud interaction provider");
        let response=crate::cli_cloud_auth::execute(app.clone(),&CloudRequest::AuthBegin{provider:provider.key().into(),account_ref:record.account_id.as_deref().map(|id|crate::cli_cloud_auth::account_ref(provider,id))},&lang).await;
        let dto = match authorization_result(response, &lang) {
            Ok(dto) => dto,
            Err(failure) => { finish(&app, record, failure, "resuming"); return; }
        };
        record.status="awaiting_login".into();record.authorization_id=Some(dto.authorization_id);
        if app.state::<AppState>().store.update_agent_interaction(&mut record,"resuming").is_err(){return}
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !app.state::<AppState>().store.agent_interaction(&record.id).is_ok_and(|r|r.status=="awaiting_login"){return}
            let response=crate::cli_cloud_auth::execute(app.clone(),&CloudRequest::AuthStatus{authorization_id:record.authorization_id.clone().unwrap()},&lang).await;
            let dto = match authorization_result(response, &lang) {
                Ok(dto) => dto,
                Err(failure) => { finish(&app, record, failure, "awaiting_login"); return; }
            };
            if dto.status=="pending"{continue}
            if dto.status!="succeeded"{
                if matches!(dto.status.as_str(),"closed"|"cancelled"|"expired") {
                    record.status=if dto.status=="expired"{"expired"}else{"cancelled"}.into();
                    record.result=None;
                    let _=app.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_login");
                } else {
                    let failure = match dto.error {
                        Some(error) => Response { ok: false, data: None, error: Some(error) },
                        None => Response::failure("PROTOCOL_ERROR", &lang),
                    };
                    finish(&app, record, failure, "awaiting_login");
                }
                return
            }
            record.status="resuming".into();
            if app.state::<AppState>().store.update_agent_interaction(&mut record,"awaiting_login").is_err(){return}
            break
        }
    }
    if matches!(record.kind.as_str(),"cloud_account"|"cloud_login") {
        let provider=record.provider.unwrap();
        let account = match crate::cloud::commands::cloud_account(app.clone(), provider).await {
            Ok(account) => account,
            Err(error) => {
                finish(&app, record, Response::failure(crate::cli_cloud::cloud_code(error), &lang), "resuming");
                return;
            }
        };
        if record.account_id.as_deref().is_some_and(|id|id!=account.id){finish(&app,record,Response::failure("ACCOUNT_MISMATCH",&lang),"resuming");return}
        if let Err(code)=crate::agent_access::approve_cloud_account(&app,provider.key(),&account.id,Some(account.display_name)) {finish(&app,record,Response::failure(code,&lang),"resuming");return}
    }
    if let Err(code)=app.state::<AppState>().store.save_agent_task_grant(&record.id,&record.fingerprint,&record.grant){finish(&app,record,Response::failure(code,&lang),"resuming");return}
    let response=crate::agent_permissions::with_scope(record.id.clone(),crate::cli_service::execute_business(app.clone(),record.request.clone())).await;
    if !login_attempted&&record.provider.is_some()&&response.error.as_ref().is_some_and(|e|e.code=="AUTH_REQUIRED") {
        // A provider may expire the session during the approved operation.
        // Keep the original request and local grant while VD completes login.
        record.kind="cloud_login".into();
        if app.state::<AppState>().store.update_agent_interaction(&mut record,"resuming").is_err(){return}
        continue
    }
    finish(&app,record,response,"resuming");
    return
    }
}

pub(crate) async fn interaction(app:&AppHandle,input:&InteractionRequest,lang:&str)->Response {
    if uuid::Uuid::parse_str(input.id()).is_err(){return Response::failure("INVALID_ARGUMENT",lang)}
    let mut record=match app.state::<AppState>().store.agent_interaction(input.id()){Ok(r)=>r,Err(c)=>return Response::failure(c,lang)};
    match input {
        InteractionRequest::Status{..}=>(),
        InteractionRequest::Cancel{..} if matches!(record.status.as_str(),"awaiting_user"|"awaiting_login")=>{
            let previous=record.status.clone();record.status="cancelled".into();record.result=Some(Response::failure("TASK_CANCELLED",lang));
            if let Err(c)=app.state::<AppState>().store.update_agent_interaction(&mut record,&previous){return Response::failure(c,lang)}
            if let Some(id)=&record.authorization_id{let _=crate::cli_cloud_auth::execute(app.clone(),&CloudRequest::AuthCancel{authorization_id:id.clone()},lang).await;}
        }
        InteractionRequest::Resume{..} if matches!(record.status.as_str(),"cancelled"|"expired"|"interrupted")
            || (record.status=="failed"&&(record.request.command=="prepare"||matches!(&record.request.cloud,Some(CloudRequest::Prepare{..}))))=>{
            // A crash during execution may have accepted a write. The same original
            // request ID is retained; host receipts decide whether it is replayable.
            let previous=record.status.clone();record.status="awaiting_user".into();record.result=None;record.expires_at=now()+600;
            if let Err(c)=app.state::<AppState>().store.update_agent_interaction(&mut record,&previous){return Response::failure(c,lang)}
            if let Ok(mut active)=app.state::<Displayed>().0.lock(){if active.as_deref()==Some(&record.id){*active=None;}}
            show_next(app);
        }
        _=>()
    }
    Response::success(serde_json::to_value(record.dto()).expect("interaction DTO"))
}
pub(crate) fn register(app:&AppHandle){
    app.manage(Displayed::default());
    if let Ok(records)=app.state::<AppState>().store.pending_agent_interactions(){for mut record in records{let previous=record.status.clone();record.status="interrupted".into();let _=app.state::<AppState>().store.update_agent_interaction(&mut record,&previous);}}
    let app=app.clone();tauri::async_runtime::spawn(async move{loop{
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Ok(records)=app.state::<AppState>().store.pending_agent_interactions(){for mut record in records{
            if record.expires_at<=now()&&matches!(record.status.as_str(),"awaiting_user"|"awaiting_login"){
                let previous=record.status.clone();record.status="expired".into();
                if app.state::<AppState>().store.update_agent_interaction(&mut record,&previous).is_ok(){
                    if let Some(id)=record.authorization_id {let _=crate::cli_cloud_auth::execute(app.clone(),&CloudRequest::AuthCancel{authorization_id:id},"en").await;}
                }
            }
        }}
    }});
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record()->PendingInteraction {
        let request=Request{command:"add".into(),url:Some("https://example.test/file.zip".into()),request_id:Some("original-request".into()),..Default::default()};
        PendingInteraction{revision:0,id:uuid::Uuid::new_v4().to_string(),fingerprint:fingerprint(&request),request,status:"awaiting_user".into(),kind:"local_access".into(),provider:None,account_id:None,account_label:None,grant:LocalAccessGrant::default(),needs_directory:true,expires_at:now()+600,authorization_id:None,result:None}
    }
    #[test]
    fn agent_interaction_approval_is_bound_to_vd_window_and_local_document(){
        let local=url::Url::parse("http://tauri.localhost/index.html?window=agent-access").unwrap();
        assert!(trusted_origin(WINDOW, &local, None));
        assert!(trusted_origin(WINDOW, &url::Url::parse("tauri://localhost/index.html?window=agent-access").unwrap(), None));
        assert!(!trusted_origin("main", &local, None));
        assert!(!trusted_origin("cloud-auth-baidu", &local, None));
        assert!(!trusted_origin(WINDOW, &url::Url::parse("https://example.test/index.html?window=agent-access").unwrap(), None));
        assert!(!trusted_origin(WINDOW, &url::Url::parse("http://tauri.localhost/another.html").unwrap(), None));
    }
    #[test]
    fn agent_interaction_uses_the_configured_dev_document() {
        let dev_url = url::Url::parse("http://127.0.0.1:53167/").unwrap();
        let page = dev_url.join(WINDOW_DOCUMENT).unwrap();
        assert!(trusted_origin(WINDOW, &page, Some(&dev_url)));
        assert!(!trusted_origin(WINDOW, &page, None));
        assert!(!trusted_origin("main", &page, Some(&dev_url)));
        assert!(!trusted_origin(WINDOW, &dev_url.join("index.html").unwrap(), Some(&dev_url)));

        let other_port = url::Url::parse("http://127.0.0.1:53168/").unwrap();
        assert!(!trusted_origin(WINDOW, &other_port.join(WINDOW_DOCUMENT).unwrap(), Some(&dev_url)));
    }
    #[test]
    fn agent_interaction_retries_keep_receipt_and_do_not_reopen_denial(){
        let store=crate::TaskStore::open_in_memory().unwrap();let mut record=record();
        store.insert_agent_interaction(&record).unwrap();
        assert_eq!(store.insert_agent_interaction(&record),Err("BUSY"));
        let pending=pending_response(&record,"en");
        assert_eq!(pending.error.unwrap().interaction_id.as_deref(),Some(record.id.as_str()));
        record.status="cancelled".into();record.result=Some(Response::failure("TASK_CANCELLED","en"));
        store.update_agent_interaction(&mut record,"awaiting_user").unwrap();
        let replay=store.find_agent_interaction(&record.fingerprint).unwrap().unwrap();
        assert_eq!(pending_response(&replay,"en").error.unwrap().code,"TASK_CANCELLED");
        assert!(store.pending_agent_interactions().unwrap().is_empty());
        assert_eq!(store.update_agent_interaction(&mut record,"awaiting_user"),Err("REQUEST_CONFLICT"));
    }
    #[test]
    fn agent_interaction_terminal_login_receipts_stop_polling() {
        let mut record = record();
        for (status, code) in [("cancelled", "TASK_CANCELLED"), ("expired", "INTERACTION_EXPIRED"), ("interrupted", "INTERRUPTED")] {
            record.status = status.into();
            let error = pending_response(&record, "en").error.unwrap();
            assert_eq!(error.code, code);
            assert!(error.retry_after_ms.is_none());
            assert_eq!(record.dto().poll_interval_ms, 0);
        }
        assert_eq!(authorization_result(Response::failure("BUSY", "en"), "en").unwrap_err().error.unwrap().code, "BUSY");
        assert_eq!(authorization_result(Response::success(serde_json::Value::Null), "en").unwrap_err().error.unwrap().code, "PROTOCOL_ERROR");
    }

    #[test]
    fn agent_interaction_concurrent_approve_cancel_has_one_winner(){
        let store=std::sync::Arc::new(crate::TaskStore::open_in_memory().unwrap());let record=record();
        store.insert_agent_interaction(&record).unwrap();
        let outcomes=std::thread::scope(|scope|{
            let a=scope.spawn(||{let mut r=record.clone();r.status="resuming".into();store.update_agent_interaction(&mut r,"awaiting_user")});
            let b=scope.spawn(||{let mut r=record.clone();r.status="cancelled".into();store.update_agent_interaction(&mut r,"awaiting_user")});
            vec![a.join().unwrap(),b.join().unwrap()]
        });
        assert_eq!(outcomes.iter().filter(|r|r.is_ok()).count(),1);
    }
    #[test]
    fn agent_interaction_restart_keeps_original_request_and_typed_result(){
        let root=tempfile::tempdir().unwrap();let db=root.path().join("tasks.sqlite");let mut record=record();
        {let store=crate::TaskStore::open_path(&db).unwrap();store.insert_agent_interaction(&record).unwrap();
            record.status="resuming".into();record.request.directory=Some(root.path().to_string_lossy().into_owned());
            store.update_agent_interaction(&mut record,"awaiting_user").unwrap();}
        let store=crate::TaskStore::open_path(&db).unwrap();let mut restored=store.pending_agent_interactions().unwrap().remove(0);
        assert_eq!(restored.request.request_id.as_deref(),Some("original-request"));
        assert_eq!(restored.request.directory,record.request.directory);
        restored.status="interrupted".into();store.update_agent_interaction(&mut restored,"resuming").unwrap();
        assert!(store.pending_agent_interactions().unwrap().is_empty());
        let dto=serde_json::to_string(&restored.dto()).unwrap();assert!(!dto.contains("example.test"));assert!(!dto.contains("original-request"));
    }
    #[test]
    fn agent_interaction_completed_previews_release_lookup_but_keep_status_receipt(){
        let store=crate::TaskStore::open_in_memory().unwrap();let mut old=record();
        store.insert_agent_interaction(&old).unwrap();old.status="succeeded".into();
        store.update_agent_interaction(&mut old,"awaiting_user").unwrap();
        store.release_agent_interaction_lookup(&old.id).unwrap();
        assert!(store.find_agent_interaction(&old.fingerprint).unwrap().is_none());
        assert_eq!(store.agent_interaction(&old.id).unwrap().status,"succeeded");
        let mut next=old.clone();next.revision=0;next.id=uuid::Uuid::new_v4().to_string();next.status="awaiting_user".into();
        store.insert_agent_interaction(&next).unwrap();
        assert_eq!(store.find_agent_interaction(&old.fingerprint).unwrap().unwrap().id,next.id);
    }
    #[test]
    fn agent_interaction_old_login_cannot_replace_a_resumed_login(){
        let store=crate::TaskStore::open_in_memory().unwrap();let mut record=record();
        store.insert_agent_interaction(&record).unwrap();
        record.status="awaiting_login".into();record.authorization_id=Some("old-login".into());
        store.update_agent_interaction(&mut record,"awaiting_user").unwrap();let mut stale=record.clone();
        record.status="cancelled".into();store.update_agent_interaction(&mut record,"awaiting_login").unwrap();
        record.status="awaiting_login".into();record.authorization_id=Some("new-login".into());
        store.update_agent_interaction(&mut record,"cancelled").unwrap();
        stale.status="cancelled".into();
        assert_eq!(store.update_agent_interaction(&mut stale,"awaiting_login"),Err("REQUEST_CONFLICT"));
        assert_eq!(store.agent_interaction(&record.id).unwrap().authorization_id.as_deref(),Some("new-login"));
    }
}
