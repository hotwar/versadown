//! The CLI boundary uses the same queue projection and action router as the GUI.
//! Only this explicit projection is serialized: internal sources, engine errors,
//! account identifiers and file metadata never cross the local API boundary.
use serde::{Deserialize, Serialize};
use base64::Engine;
use std::path::Path;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

use crate::{
    cli_protocol::{Request, Response, ApiError, TaskDto, TaskListDto, TaskResultDto,
        ArtifactDto, HealthDto, CapabilitiesDto, LimitsDto, BatchItemInput, BatchItemResult,
        BatchAddDto, PROTOCOL_VERSION},
    commands::{self, AppState},
    models::{NewTask, QueueTaskPayload},
    parser::CandidateKind,
    store::CliCreateOutcome,
    TaskStatus, TaskStore,
};

type ApiResult<T> = Result<T, &'static str>;

pub(crate) fn public_task(task: &QueueTaskPayload) -> TaskDto {
    TaskDto {
        id: task.id.clone(),
        // Some old tasks use their entire input as the display name. Never
        // expose such a name: query strings and userinfo can contain secrets.
        name: (!task.name.contains("://") && !task.name.contains('?'))
            .then(|| task.name.chars().take(256).collect()),
        kind: task.kind.clone(),
        status: task.status.as_db().to_owned(),
        progress: task.progress,
        total_length: task.total_length,
        completed_length: task.completed_length,
        download_speed: task.download_speed,
        created_at: task.created_at,
        updated_at: task.updated_at,
        completed_at: task.completed_at,
        cleanup_pending: pending_cleanup(task),
        child_count: task.children.iter().filter(|child| !contains_private(child)).count().min(u32::MAX as usize) as u32,
        // Never pass engine/provider error text or unconstrained failure codes.
        failure: (task.status == TaskStatus::Failed).then(|| ApiError::new(
            if task.reauthorization_required { "AUTH_REQUIRED" }
            else if task.failure_code.as_deref() == Some("CLI_RECOVERY_REQUIRED") { "CLI_RECOVERY_REQUIRED" }
            else { "TASK_FAILED" }, "en")),
        required_action: if task.reauthorization_required { Some("open_desktop_login".into()) }
            else if task.status == TaskStatus::AwaitingUser { Some("inspect_desktop".into()) } else { None },
    }
}

fn pending_cleanup(task: &QueueTaskPayload) -> bool {
    task.cleanup_pending || task.children.iter().any(pending_cleanup)
}

fn contains_private(task: &QueueTaskPayload) -> bool {
    task.is_private || task.children.iter().any(contains_private)
}

fn find_public<'a>(tasks: &'a [QueueTaskPayload], id: &str) -> Option<&'a QueueTaskPayload> {
    for task in tasks.iter().filter(|task| !contains_private(task)) {
        if task.id == id {
            return Some(task);
        }
        if let Some(found) = find_public(&task.children, id) {
            return Some(found);
        }
    }
    None
}

#[derive(Serialize, Deserialize)]
struct PageCursor { created_at: i64, id: String, filter: String }

fn projected_list(tasks: &[QueueTaskPayload], request: &Request) -> ApiResult<Value> {
    let source = if let Some(parent) = &request.parent_id {
        &find_public(tasks, parent).ok_or("NOT_FOUND")?.children
    } else { tasks };
    let filter = format!("{:x}", Sha256::digest(serde_json::to_vec(
        &(&request.parent_id, &request.status, &request.query)).map_err(|_| "INTERNAL_ERROR")?));
    let after = request.cursor.as_ref().map(|cursor| -> ApiResult<PageCursor> {
        if cursor.len() > 1024 { return Err("INVALID_ARGUMENT"); }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor).map_err(|_| "INVALID_ARGUMENT")?;
        let cursor: PageCursor = serde_json::from_slice(&bytes).map_err(|_| "INVALID_ARGUMENT")?;
        if cursor.filter != filter { return Err("INVALID_ARGUMENT"); }
        Ok(cursor)
    }).transpose()?;
    let limit = request.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) { return Err("INVALID_ARGUMENT"); }
    let query = request.query.as_deref().map(str::to_lowercase);
    let mut rows: Vec<_> = source.iter().filter(|task| !contains_private(task))
        .filter(|task| request.status.as_deref().is_none_or(|s| task.status.as_db() == s))
        .filter(|task| after.as_ref().is_none_or(|c| (task.created_at, &task.id) > (c.created_at, &c.id)))
        .map(public_task)
        .filter(|task| query.as_ref().is_none_or(|q| task.name.as_ref().is_some_and(|n| n.to_lowercase().contains(q))))
        .collect();
    rows.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if more { rows.last().map(|last| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(
            &PageCursor { created_at: last.created_at, id: last.id.clone(), filter }).expect("cursor JSON"))
    }) } else { None };
    serde_json::to_value(TaskListDto { tasks: rows, next_cursor }).map_err(|_| "INTERNAL_ERROR")
}

fn visit_files<'a>(task: &'a QueueTaskPayload, files: &mut Vec<&'a crate::aria2::TransferFile>) {
    files.extend(task.files.iter().filter(|file| file.selected));
    for child in &task.children { visit_files(child, files); }
}

fn authorized_outputs(state: &AppState, task: &QueueTaskPayload) -> ApiResult<()> {
    authorized_outputs_with(&state.store, task, |path, directory| {
        if directory { crate::agent_access::authorize_directory(state, path).map(|_| ()) }
        else { crate::agent_access::authorize_file(state, path).map(|_| ()) }
    })
}

fn authorized_outputs_with(
    store: &TaskStore,
    task: &QueueTaskPayload,
    authorize: impl Fn(&Path, bool) -> ApiResult<()>,
) -> ApiResult<()> {
    // Intermediate queue folders are presentation-only nodes, never an
    // independently authorized execution scope or a result resource.
    if task.id.starts_with("group:") { return Err("UNSUPPORTED"); }
    let id = uuid::Uuid::parse_str(&task.id).map_err(|_| "INVALID_ARGUMENT")?;
    let group = store.cloud_directory_group(id).map_err(|_| "INTERNAL_ERROR")?;
    if let Some(group) = &group {
        // A partly dispatched directory can have only some child paths in
        // task_files. Its scheduler will still materialize the remaining
        // children, so checking the existing files alone is insufficient.
        authorize(&group.output_directory, true)?;
    }
    let mut files = Vec::new();
    visit_files(task, &mut files);
    for file in &files { authorize(Path::new(&file.path), false)?; }
    // A not-yet-dispatched ordinary task may have no engine file list yet.
    if files.is_empty() && group.is_none() {
        // cli_dispatch provides the durable destination for accepted work.
        let directory = store.cli_intent_directory(id).map_err(|_| "INTERNAL_ERROR")?
            .or(store.agent_resource_directory(id)?)
            .ok_or("FORBIDDEN")?;
        authorize(&directory, true)?;
    }
    Ok(())
}

fn task_result(state: &AppState, tasks: &[QueueTaskPayload], id: &str) -> ApiResult<Value> {
    let task = find_public(tasks, id).ok_or("NOT_FOUND")?;
    match task.status {
        TaskStatus::Failed => return Err("TASK_FAILED"),
        TaskStatus::Cancelled | TaskStatus::Recycled => return Err("TASK_CANCELLED"),
        TaskStatus::Completed if !pending_cleanup(task) => (),
        _ => return Err("TASK_NOT_READY"),
    }
    authorized_outputs(state, task)?;
    let mut files = Vec::new();
    visit_files(task, &mut files);
    if files.len() > 100 { return Err("UNSUPPORTED"); }
    let mut outputs = std::collections::BTreeMap::new();
    for file in files {
        let path = crate::agent_access::authorize_file(state, Path::new(&file.path))?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Some(metadata),
            Ok(_) => return Err("FORBIDDEN"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("ACTION_FAILED"),
        };
        let display = path.to_str().ok_or("FORBIDDEN")?.to_owned();
        outputs.insert(display.clone(), ArtifactDto { path: display,
            size_bytes: metadata.as_ref().map_or(file.length, |m| m.len()), exists: metadata.is_some(), sha256: None });
    }
    serde_json::to_value(TaskResultDto { task_id: id.to_owned(), status: task.status.as_db().into(),
        files: outputs.into_values().collect() }).map_err(|_| "INTERNAL_ERROR")
}

fn queue(store: &TaskStore) -> ApiResult<Vec<QueueTaskPayload>> {
    // Read the full tree solely to detect private descendants before a group
    // action. Filtering and serialization below do not depend on GUI unlock.
    commands::list_queue_tasks_for_scope(store, None).map_err(|_| "INTERNAL_ERROR")
}

fn status(store: &TaskStore, id: &str, _action: bool) -> ApiResult<Value> {
    let tasks = queue(store)?;
    projected_status(&tasks, id)
}

fn projected_status(tasks: &[QueueTaskPayload], id: &str) -> ApiResult<Value> {
    let task = find_public(&tasks, id).ok_or("NOT_FOUND")?;
    if contains_private(task) {
        return Err("NOT_FOUND");
    }
    serde_json::to_value(public_task(task)).map_err(|_| "INTERNAL_ERROR")
}

async fn host_queue(app: &AppHandle) -> ApiResult<Vec<QueueTaskPayload>> {
    let state = app.state::<AppState>();
    let cloud = app.try_state::<std::sync::Arc<crate::cloud::commands::CloudState>>()
        .map(|cloud| std::sync::Arc::clone(cloud.inner()));
    commands::list_queue_tasks_in_background(std::sync::Arc::clone(&state.store), cloud, None, true)
        .await.map_err(|_| "INTERNAL_ERROR")
}

async fn host_status(app: &AppHandle, id: &str) -> ApiResult<Value> {
    projected_status(&host_queue(app).await?, id)
}

async fn control_public<F, Fut>(store: &TaskStore, id: &str, action: &str, dispatch: F) -> ApiResult<Value>
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    status(store, id, true)?;
    dispatch(id.to_owned(), action.to_owned()).await.map_err(|_| "ACTION_FAILED")?;
    status(store, id, false)
}

fn direct_source(source: &str) -> ApiResult<String> {
    let candidate = crate::parser::validate_download_input(source).map_err(|_| "INVALID_ARGUMENT")?;
    if !matches!(candidate.kind, CandidateKind::Direct | CandidateKind::Ftp) {
        return Err("UNSUPPORTED");
    }
    let parsed = url::Url::parse(&candidate.source).map_err(|_| "INVALID_ARGUMENT")?;
    if !matches!(parsed.scheme(), "http" | "https" | "ftp") || parsed.host_str().is_none()
        || !parsed.username().is_empty() || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err("INVALID_ARGUMENT");
    }
    Ok(candidate.source)
}

fn fingerprint(source: &str, directory: &str) -> String {
    // Length-delimited JSON avoids collisions at argument boundaries.
    let parameters = serde_json::to_vec(&(source, directory)).expect("string tuple is JSON");
    format!("{:x}", Sha256::digest(parameters))
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn validate_batch(items: &[BatchItemInput]) -> ApiResult<()> {
    if items.is_empty() || items.len() > 20 { return Err("INVALID_ARGUMENT"); }
    let mut ids = std::collections::HashSet::new();
    for item in items {
        // Validate every echoed identity before the first transaction. In
        // particular, an invalid ID must not be a channel for returning URLs.
        if !valid_request_id(&item.request_id) || !ids.insert(&item.request_id)
            || item.url.is_empty() || item.url.len() > 8192
            || item.directory.is_empty() || item.directory.len() > 4096
        {
            return Err("INVALID_ARGUMENT");
        }
    }
    Ok(())
}

async fn batch_add_with<F, Fut>(items: &[BatchItemInput], lang: &str, mut submit: F) -> ApiResult<BatchAddDto>
where
    F: FnMut(BatchItemInput) -> Fut,
    Fut: std::future::Future<Output = ApiResult<TaskDto>>,
{
    validate_batch(items)?;
    let mut results = Vec::with_capacity(items.len());
    let mut accepted = 0;
    for item in items {
        let result = match submit(item.clone()).await {
            Ok(task) => {
                accepted += 1;
                BatchItemResult { request_id: item.request_id.clone(), ok: true, data: Some(task), error: None }
            },
            Err(code) => BatchItemResult { request_id: item.request_id.clone(), ok: false,
                data: None, error: Some(ApiError::new(code, lang)) },
        };
        results.push(result);
    }
    Ok(BatchAddDto { failed: items.len() as u32 - accepted, accepted, items: results })
}

fn create_direct_intent(store: &TaskStore, source: &str, directory: &str, id: &str,
    options: &crate::UriSubmissionOptions) -> ApiResult<crate::DownloadTask>
{
    let mut new_task = NewTask::direct(source);
    new_task.kind = if url::Url::parse(source).is_ok_and(|url| url.scheme() == "ftp") { "ftp" } else { "file" }.into();
    new_task.name = url::Url::parse(source).ok()
        .and_then(|url| url.path_segments()?.next_back().map(str::to_owned))
        .filter(|name| !name.is_empty()).unwrap_or_else(|| "download".into());
    match store.create_cli_task_with_intent(id, &fingerprint(source, directory), new_task, options)
        .map_err(|error| match error {
            crate::store::StoreError::NotFound(_) => "NOT_FOUND",
            crate::store::StoreError::UnsupportedCliOptions => "UNSUPPORTED",
            _ => "INTERNAL_ERROR",
        })?
    {
        CliCreateOutcome::Conflict => Err("REQUEST_CONFLICT"),
        CliCreateOutcome::Duplicate => Err("DUPLICATE"),
        CliCreateOutcome::Busy => Err("BUSY"),
        CliCreateOutcome::Replayed(task) | CliCreateOutcome::Created(task) => Ok(task),
    }
}

async fn add_one(app: &AppHandle, input: BatchItemInput) -> ApiResult<TaskDto> {
    let state = app.state::<AppState>();
    if !valid_request_id(&input.request_id) { return Err("INVALID_ARGUMENT"); }
    if input.url.len() > 8192 || input.directory.len() > 4096 { return Err("INVALID_ARGUMENT"); }
    let source = direct_source(&input.url)?;
    let directory = Path::new(&input.directory);
    if input.directory.trim().is_empty() || !directory.is_absolute() { return Err("INVALID_ARGUMENT"); }
    let authorized_directory = crate::agent_access::authorize_directory(&state, directory)?;
    let directory = authorized_directory.to_str().ok_or("INVALID_ARGUMENT")?;
    let options = commands::download_submission_options_for_directory(&state, Some(directory))
        .map_err(|_| "INVALID_ARGUMENT")?;
    let directory = options.directory.as_deref().and_then(Path::to_str).ok_or("INVALID_ARGUMENT")?;
    crate::agent_access::authorize_directory(&state, Path::new(directory))?;
    let task = create_direct_intent(&state.store, &source, directory, &input.request_id, &options)?;
    app.state::<std::sync::Arc<crate::cli_dispatch::CliDispatcher>>().notify();
    let _ = commands::emit_task_update_for_task(app, &state.store, None, task.clone());
    serde_json::from_value(host_status(app, &task.id.to_string()).await?).map_err(|_| "INTERNAL_ERROR")
}

fn validate_request(request: &Request) -> ApiResult<()> {
    let task_command = matches!(request.command.as_str(), "status" | "result" | "pause" | "resume" | "cancel");
    if task_command != request.task_id.is_some()
        || (request.command != "add" && (request.url.is_some() || request.directory.is_some() || request.request_id.is_some()))
        || (request.command == "batch_add") != request.items.is_some()
        || (request.command == "prepare") != request.prepare.is_some()
        || (request.command == "confirm") != request.confirm.is_some()
        || (request.command == "cloud") != request.cloud.is_some()
        || (request.command != "list" && (request.status.is_some() || request.query.is_some()
            || request.parent_id.is_some() || request.cursor.is_some() || request.limit.is_some()))
    {
        return Err("INVALID_ARGUMENT");
    }
    if let Some(items) = &request.items { validate_batch(items)?; }
    if let Some(id) = &request.task_id {
        if id.len() > 256 || (uuid::Uuid::parse_str(id).is_err() && !id.starts_with("group:")) { return Err("INVALID_ARGUMENT"); }
    }
    if let Some(status) = &request.status {
        TaskStatus::from_db(status).ok_or("INVALID_ARGUMENT")?;
    }
    if let Some(query) = &request.query {
        if query.len() > 256 { return Err("INVALID_ARGUMENT"); }
    }
    if request.parent_id.as_ref().is_some_and(|id| id.len() > 256)
        || request.lang.as_deref().is_some_and(|lang| !matches!(lang, "en" | "zh-CN")) { return Err("INVALID_ARGUMENT"); }
    Ok(())
}

pub(crate) async fn execute(app: AppHandle, request: Request) -> Response {
    let lang = request.lang.as_deref().unwrap_or("en");
    if let Err(code)=validate_request(&request){return Response::failure(code,lang)}
    if !crate::agent_access::is_enabled(&app.state::<AppState>()){return Response::failure("HOST_DISABLED",lang)}
    if let Some(cloud)=&request.cloud{return crate::cli_cloud::execute(app.clone(),cloud,lang).await}
    match execute_inner(&app, &request).await {
        Ok(value) => Response::success(value),
        Err(code) => {
            let mut response = Response::failure(code, lang);
            if request.command == "result" && matches!(code, "TASK_NOT_READY" | "TASK_FAILED" | "TASK_CANCELLED") {
                if let (Some(id), Ok(tasks)) = (&request.task_id, host_queue(&app).await) {
                    if let Some(task) = find_public(&tasks, id) {
                        response.data = serde_json::to_value(TaskResultDto { task_id: id.clone(),
                            status: task.status.as_db().into(), files: Vec::new() }).ok();
                    }
                }
            }
            response
        },
    }
}

async fn execute_inner(app: &AppHandle, request: &Request) -> ApiResult<Value> {
    validate_request(request)?;
    let state = app.state::<AppState>();
    if !crate::agent_access::is_enabled(&state) { return Err("HOST_DISABLED"); }
    match request.command.as_str() {
        "health" => serde_json::to_value(HealthDto {
            version: env!("CARGO_PKG_VERSION").into(), protocol_version: PROTOCOL_VERSION,
            build_commit: crate::cli_protocol::build_commit(), build_dirty: crate::cli_protocol::build_dirty(),
            instance_id: crate::cli_host::instance_id(app), engine_available: state.runtime.status().available,
            capabilities: CapabilitiesDto { new_download_protocols: vec!["http".into(), "https".into(), "ftp".into()],
                prepared_resource_kinds: Some(["video", "cloud", "share", "torrent"].map(str::to_owned).to_vec()),
                task_types: ["file", "cloud", "web_video", "directory", "magnet", "torrent", "ftp"].map(str::to_owned).to_vec(),
                operations: ["health", "add", "batch_add", "prepare", "confirm", "list", "status", "pause", "resume", "cancel", "result", "cloud"].map(str::to_owned).to_vec(),
                private_tasks: false },
            limits: LimitsDto { list_default_limit: 50, list_max_limit: 100, poll_interval_ms: 1000,
                max_response_bytes: 8 * 1024 * 1024, max_pending_tasks: 1000 },
        }).map_err(|_| "INTERNAL_ERROR"),
        "list" => {
            let tasks = host_queue(app).await?;
            projected_list(&tasks, request)
        }
        "status" => host_status(app, request.task_id.as_deref().ok_or("INVALID_ARGUMENT")?).await,
        "result" => task_result(&state, &host_queue(app).await?, request.task_id.as_deref().ok_or("INVALID_ARGUMENT")?),
        "pause" | "resume" | "cancel" => {
            let id = request.task_id.as_deref().ok_or("INVALID_ARGUMENT")?;
            let tasks = host_queue(app).await?;
            let task = find_public(&tasks, id).ok_or("NOT_FOUND")?;
            if request.command == "resume" { authorized_outputs(&state, task)?; }
            if (request.command == "pause" && task.status == TaskStatus::Paused)
                || (request.command == "cancel" && matches!(task.status, TaskStatus::Cancelled | TaskStatus::Recycled))
                || (request.command == "resume" && matches!(task.status, TaskStatus::Queued | TaskStatus::Downloading)) {
                return serde_json::to_value(public_task(task)).map_err(|_| "INTERNAL_ERROR");
            }
            // Completed files are never deleted by an automation cancel.
            if matches!(task.status, TaskStatus::Completed | TaskStatus::Recycled) { return Err("ACTION_FAILED"); }
            if crate::cli_resources::control_pending(app, uuid::Uuid::parse_str(id).map_err(|_| "INVALID_ARGUMENT")?, &request.command).await?.is_some() {
                return host_status(app, id).await;
            }
            control_public(&state.store, id, &request.command, |id, action| async move {
                commands::dispatch_task_action(id, action, app.clone()).await.map(|_| ())
            }).await?;
            host_status(app, id).await
        }
        "add" => {
            let task = add_one(app, BatchItemInput { url: request.url.clone().ok_or("INVALID_ARGUMENT")?,
                directory: request.directory.clone().ok_or("INVALID_ARGUMENT")?,
                request_id: request.request_id.clone().ok_or("INVALID_ARGUMENT")? }).await?;
            serde_json::to_value(task).map_err(|_| "INTERNAL_ERROR")
        }
        "batch_add" => serde_json::to_value(batch_add_with(request.items.as_deref().ok_or("INVALID_ARGUMENT")?,
            request.lang.as_deref().unwrap_or("en"), |item| add_one(app, item)).await?).map_err(|_| "INTERNAL_ERROR"),
        "prepare" | "confirm" => crate::cli_resources::handle(app.clone(), request).await,
        _ => Err("INVALID_ARGUMENT"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_partial_directory_requires_root_authority_even_with_authorized_existing_files() {
        use crate::{agent_access::AgentAccessSettings, cloud::CloudProvider, models::{NewCloudDirectoryGroup, NewCloudDirectoryGroupItem}};
        let directory = tempfile::tempdir().unwrap();
        let allowed_child = directory.path().join("Collection").join("allowed");
        std::fs::create_dir_all(&allowed_child).unwrap();
        let store = TaskStore::open_in_memory().unwrap();
        let group = store.create_cloud_directory_group(
            NewCloudDirectoryGroup::new(CloudProvider::Quark, "remote", "Collection", directory.path()),
            &[
                NewCloudDirectoryGroupItem::new("first", &["allowed"], "one.bin", Some(100)),
                NewCloudDirectoryGroupItem::new("pending", &["outside"], "two.bin", Some(100)),
            ],
        ).unwrap();
        let task = store.create(NewTask::direct("https://example.test/one.bin")).unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap().into_iter()
            .find(|item| item.remote_file_id == "first").unwrap();
        store.link_cloud_directory_group_item_task(group.id, item.id, task.id).unwrap();
        store.upsert_files(task.id, &[(1, allowed_child.join("one.bin").to_string_lossy().into_owned(), 100, 20, true)]).unwrap();
        let root = queue(&store).unwrap().into_iter().find(|task| task.id == group.id.to_string()).unwrap();
        let mut files = Vec::new();
        visit_files(&root, &mut files);
        assert_eq!(files.len(), 1, "the second file has not been dispatched yet");
        let child_policy = AgentAccessSettings { enabled: true, allowed_directories: vec![allowed_child.to_string_lossy().into_owned()], ..Default::default() }.validated().unwrap();
        assert!(child_policy.authorize_file(Path::new(&files[0].path)).is_ok());
        let check = |policy: &AgentAccessSettings| authorized_outputs_with(&store, &root, |path, directory| {
            if directory { policy.authorize_directory(path).map(|_| ()) }
            else { policy.authorize_file(path).map(|_| ()) }
        });
        assert_eq!(check(&child_policy), Err("FORBIDDEN"));
        let root_policy = AgentAccessSettings { enabled: true, allowed_directories: vec![directory.path().to_string_lossy().into_owned()], ..Default::default() }.validated().unwrap();
        assert_eq!(check(&root_policy), Ok(()));
    }

    #[test]
    fn cli_intermediate_folder_cannot_be_used_as_an_independent_output_scope() {
        let store = TaskStore::open_in_memory().unwrap();
        store.create(NewTask::direct("https://example.test/one.bin")).unwrap();
        let mut folder = queue(&store).unwrap().remove(0);
        folder.id = format!("group:{}:dir:allowed", uuid::Uuid::new_v4());
        assert_eq!(authorized_outputs_with(&store, &folder, |_, _| Ok(())), Err("UNSUPPORTED"));
    }

    #[test]
    fn cli_pages_are_bounded_filter_bound_and_do_not_expand_children() {
        let store = TaskStore::open_in_memory().unwrap();
        for n in 0..5 { store.create(NewTask::direct(format!("https://example.test/{n}.bin"))).unwrap(); }
        let mut tasks = queue(&store).unwrap();
        let mut child = tasks[0].clone();
        child.id = uuid::Uuid::new_v4().to_string();
        tasks[0].children = vec![child.clone()];
        let mut request = Request { command: "list".into(), limit: Some(2), ..Default::default() };
        let first = projected_list(&tasks, &request).unwrap();
        assert_eq!(first["tasks"].as_array().unwrap().len(), 2);
        assert!(!first.to_string().contains("children"));
        let mut seen: std::collections::HashSet<String> = first["tasks"].as_array().unwrap().iter()
            .map(|t| t["id"].as_str().unwrap().into()).collect();
        request.cursor = first["next_cursor"].as_str().map(str::to_owned);
        loop {
            let page = projected_list(&tasks, &request).unwrap();
            for row in page["tasks"].as_array().unwrap() { assert!(seen.insert(row["id"].as_str().unwrap().into())); }
            request.cursor = page["next_cursor"].as_str().map(str::to_owned);
            if request.cursor.is_none() { break; }
        }
        assert_eq!(seen.len(), 5);
        request.cursor = first["next_cursor"].as_str().map(str::to_owned);
        request.status = Some("paused".into());
        assert_eq!(projected_list(&tasks, &request), Err("INVALID_ARGUMENT"));
        request.cursor = None; request.status = None; request.parent_id = Some(tasks[0].id.clone());
        let children = projected_list(&tasks, &request).unwrap();
        assert_eq!(children["tasks"][0]["id"], child.id);
        tasks[0].children[0].is_private = true;
        assert_eq!(projected_list(&tasks, &request), Err("NOT_FOUND"));
        request.parent_id = None; request.limit = Some(101);
        assert_eq!(projected_list(&tasks, &request), Err("INVALID_ARGUMENT"));
    }

    #[test]
    fn cli_failed_task_projection_exposes_safe_next_action_not_engine_error() {
        let store = TaskStore::open_in_memory().unwrap();
        store.create(NewTask::direct("https://example.test/file")).unwrap();
        let mut task = queue(&store).unwrap().remove(0);
        task.status = TaskStatus::Failed;
        task.error = Some("Cookie=secret; signed_url=https://private.test/?token=password".into());
        task.failure_code = Some("arbitrary-secret-code".into());
        task.reauthorization_required = true;
        let projected = serde_json::to_value(public_task(&task)).unwrap();
        assert_eq!(projected["failure"]["code"], "AUTH_REQUIRED");
        assert_eq!(projected["required_action"], "open_desktop_login");
        for secret in ["Cookie", "secret", "password", "private.test"] { assert!(!projected.to_string().contains(secret)); }
    }

    #[test]
    fn cli_direct_input_accepts_anonymous_ftp_and_http_files_without_url_credentials() {
        assert!(direct_source("https://example.test/file.bin?signature=secret").is_ok());
        assert!(direct_source("ftp://example.test/pub/archive.zip").is_ok());
        for source in ["ftp://name:secret@example.test/file", "ftp://name@example.test/file", "ftp://example.test/pub/", "ftps://example.test/file", "https://example.test/list.m3u8", "https://example.test/a.mpd", "https://www.youtube.com/watch?v=abcdefghijk", "https://pan.baidu.com/s/123", "magnet:?xt=urn:btih:0123456789012345678901234567890123456789", "https://name:secret@example.test/file", "https://example.test/file#secret"] {
            assert!(direct_source(source).is_err(), "accepted {source}");
        }
    }

    fn accept_batch_fixture(store: &TaskStore, item: BatchItemInput) -> ApiResult<TaskDto> {
        let source = direct_source(&item.url)?;
        let options = crate::UriSubmissionOptions { directory: Some(item.directory.clone().into()), ..Default::default() };
        let task = create_direct_intent(store, &source, &item.directory, &item.request_id, &options)?;
        serde_json::from_value(status(store, &task.id.to_string(), false)?).map_err(|_| "INTERNAL_ERROR")
    }

    #[tokio::test]
    async fn cli_batch_partial_failure_replays_successes_and_retries_only_unaccepted_items() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("tasks.db");
        let destination = directory.path().to_string_lossy().into_owned();
        let mut items = vec![
            BatchItemInput { url: "https://example.test/one.bin?token=secret".into(), directory: destination.clone(), request_id: "one".into() },
            BatchItemInput { url: "ftp://name:secret@example.test/two.bin".into(), directory: destination.clone(), request_id: "two".into() },
            BatchItemInput { url: "https://example.test/one.bin?token=secret".into(), directory: destination.clone(), request_id: "duplicate".into() },
        ];
        let original_id;
        {
            let store = TaskStore::open_path(&database).unwrap();
            let first = batch_add_with(&items, "en", |item| std::future::ready(accept_batch_fixture(&store, item))).await.unwrap();
            assert_eq!((first.accepted, first.failed), (1, 2));
            assert_eq!(first.items.iter().map(|item| item.request_id.as_str()).collect::<Vec<_>>(), vec!["one", "two", "duplicate"]);
            assert_eq!(first.items[1].error.as_ref().unwrap().code, "INVALID_ARGUMENT");
            assert_eq!(first.items[2].error.as_ref().unwrap().code, "DUPLICATE");
            let json = serde_json::to_string(&first).unwrap();
            assert!(!json.contains("secret") && !json.contains("example.test"));
            original_id = first.items[0].data.as_ref().unwrap().id.clone();
        }
        let store = TaskStore::open_path(&database).unwrap();
        items[1].url = "ftp://example.test/two.bin".into();
        items[2].url = "https://example.test/three.bin".into();
        let retry = batch_add_with(&items, "en", |item| std::future::ready(accept_batch_fixture(&store, item))).await.unwrap();
        assert_eq!((retry.accepted, retry.failed), (3, 0));
        assert_eq!(retry.items[0].data.as_ref().unwrap().id, original_id);
        assert_eq!(retry.items[1].data.as_ref().unwrap().kind, "ftp");
        assert_eq!(store.list().unwrap().len(), 3);
        let changed_options = crate::UriSubmissionOptions { directory: Some(directory.path().into()),
            proxy: Some("http://proxy.example.test:8080".into()), ..Default::default() };
        let ftp_replay = create_direct_intent(&store, &items[1].url, &destination, "two", &changed_options).unwrap();
        assert_eq!(ftp_replay.id.to_string(), retry.items[1].data.as_ref().unwrap().id);
        assert_eq!(create_direct_intent(&store, "ftp://example.test/new.bin", &destination, "new", &changed_options).unwrap_err(), "UNSUPPORTED");
        items[0].url = "https://example.test/different.bin".into();
        let conflict = batch_add_with(&items, "en", |item| std::future::ready(accept_batch_fixture(&store, item))).await.unwrap();
        assert_eq!(conflict.items[0].error.as_ref().unwrap().code, "REQUEST_CONFLICT");
        assert_eq!(store.list().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn cli_batch_invalid_envelope_never_invokes_submission() {
        let item = BatchItemInput { url: "https://example.test/file".into(), directory: "ignored".into(), request_id: "one".into() };
        let mut invalid_id = item.clone();
        invalid_id.request_id = "https://secret.test/?token=secret".into();
        let mut long_url = item.clone();
        long_url.request_id = "two".into();
        long_url.url = "a".repeat(8193);
        let mut empty_directory = item.clone();
        empty_directory.request_id = "two".into();
        empty_directory.directory.clear();
        for items in [Vec::new(), vec![item.clone(), item.clone()], vec![item.clone(), invalid_id],
            vec![item.clone(), long_url], vec![item.clone(), empty_directory], vec![item; 21]] {
            let result = batch_add_with(&items, "en", |_| {
                panic!("invalid batch reached submission");
                #[allow(unreachable_code)]
                std::future::ready(Err("INTERNAL_ERROR"))
            }).await;
            assert_eq!(result.unwrap_err(), "INVALID_ARGUMENT");
        }
        let misplaced = Request { command: "health".into(), items: Some(Vec::new()), ..Default::default() };
        assert_eq!(validate_request(&misplaced), Err("INVALID_ARGUMENT"));
        let missing = Request { command: "batch_add".into(), ..Default::default() };
        assert_eq!(validate_request(&missing), Err("INVALID_ARGUMENT"));
    }

    #[test]
    fn cli_projection_hides_private_tasks_and_secrets() {
        let store = TaskStore::open_in_memory().unwrap();
        let public = store.create(NewTask::direct("https://name:password@example.test/file?token=secret")).unwrap();
        let mut private = NewTask::direct("https://example.test/private");
        private.is_private = true;
        let private = store.create(private).unwrap();
        assert_eq!(status(&store, &private.id.to_string(), false), Err("NOT_FOUND"));
        let projected = status(&store, &public.id.to_string(), false).unwrap().to_string();
        for secret in ["password", "token", "secret", "aria_gid", "source", "files"] {
            assert!(!projected.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn cli_idempotency_is_atomic_and_distinct_from_source_duplicates() {
        let store = std::sync::Arc::new(TaskStore::open_in_memory().unwrap());
        let threads: Vec<_> = (0..8).map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.create_cli_task("request", "fingerprint", NewTask::direct("https://example.test/file")).unwrap())
        }).collect();
        let mut created = 0;
        let mut ids = std::collections::HashSet::new();
        for thread in threads {
            match thread.join().unwrap() {
                CliCreateOutcome::Created(task) => { created += 1; ids.insert(task.id); }
                CliCreateOutcome::Replayed(task) => { ids.insert(task.id); }
                _ => panic!("unexpected receipt"),
            }
        }
        assert_eq!(created, 1);
        assert_eq!(ids.len(), 1);
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(matches!(store.create_cli_task("request", "other", NewTask::direct("https://example.test/file")).unwrap(), CliCreateOutcome::Conflict));
        assert!(matches!(store.create_cli_task("different", "fingerprint", NewTask::direct("https://example.test/file")).unwrap(), CliCreateOutcome::Duplicate));
    }

    #[test]
    fn cli_request_receipt_survives_reopen_and_response_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.sqlite");
        let id = {
            let store = TaskStore::open_path(&path).unwrap();
            match store.create_cli_task("retry", "hash", NewTask::direct("https://example.test/file")).unwrap() {
                CliCreateOutcome::Created(task) => task.id,
                _ => panic!("expected creation"),
            }
        };
        let store = TaskStore::open_path(&path).unwrap();
        match store.create_cli_task("retry", "hash", NewTask::direct("https://example.test/file")).unwrap() {
            CliCreateOutcome::Replayed(task) => assert_eq!(task.id, id),
            _ => panic!("expected replay"),
        }
        assert_eq!(store.list().unwrap().len(), 1);
        assert_ne!(fingerprint("a", "bc"), fingerprint("ab", "c"));
    }

    #[test]
    fn cli_directory_privacy_covers_roots_and_linked_children() {
        use crate::{cloud::CloudProvider, models::{NewCloudDirectoryGroup, NewCloudDirectoryGroupItem}};
        let store = TaskStore::open_in_memory().unwrap();
        let group = store.create_cloud_directory_group(
            NewCloudDirectoryGroup::new(CloudProvider::Quark, "remote", "Collection", "C:/downloads"),
            &[NewCloudDirectoryGroupItem::new("leaf", &[] as &[&str], "file.bin", Some(100))],
        ).unwrap();
        let task = store.create(NewTask::direct("https://example.test/file.bin")).unwrap();
        let item = store.cloud_directory_group_items(group.id).unwrap().remove(0);
        store.link_cloud_directory_group_item_task(group.id, item.id, task.id).unwrap();
        assert!(status(&store, &group.id.to_string(), true).is_ok());
        assert!(status(&store, &task.id.to_string(), true).is_ok());
        store.set_cloud_directory_group_private(group.id, true).unwrap();
        assert_eq!(status(&store, &group.id.to_string(), true), Err("NOT_FOUND"));
        assert_eq!(status(&store, &task.id.to_string(), false), Err("NOT_FOUND"));
        assert_eq!(status(&store, &task.id.to_string(), true), Err("NOT_FOUND"));
    }

    #[test]
    fn cli_private_descendants_block_directory_control() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = store.create(NewTask::direct("https://example.test/file.bin")).unwrap();
        let mut root = queue(&store).unwrap().remove(0);
        let mut child = root.clone();
        child.id = uuid::Uuid::new_v4().to_string();
        child.is_private = true;
        root.children.push(child.clone());
        assert!(contains_private(&root));
        assert!(find_public(&[root.clone()], &child.id).is_none());
        assert_eq!(public_task(&root).child_count, 0);
        assert_eq!(root.id, task.id.to_string());
    }

    #[test]
    fn cli_directory_wait_includes_child_cleanup_work() {
        let store = TaskStore::open_in_memory().unwrap();
        store.create(NewTask::direct("https://example.test/file.bin")).unwrap();
        let mut root = queue(&store).unwrap().remove(0);
        root.status = TaskStatus::Completed;
        let mut child = root.clone();
        child.cleanup_pending = true;
        root.children.push(child);
        assert!(public_task(&root).cleanup_pending);
        root.children[0].cleanup_pending = false;
        assert!(!public_task(&root).cleanup_pending);
    }

    #[test]
    fn cli_replayed_task_never_bypasses_new_privacy_setting() {
        let store = TaskStore::open_in_memory().unwrap();
        let task = match store.create_cli_task("one", "hash", NewTask::direct("https://example.test/file.bin")).unwrap() {
            CliCreateOutcome::Created(task) => task,
            _ => panic!("expected creation"),
        };
        store.set_private(task.id, true).unwrap();
        let replay = store.create_cli_task("one", "hash", NewTask::direct("https://example.test/file.bin")).unwrap();
        let CliCreateOutcome::Replayed(task) = replay else { panic!("expected replay") };
        assert_eq!(status(&store, &task.id.to_string(), false), Err("NOT_FOUND"));
    }

    #[tokio::test]
    async fn cli_controls_delegate_all_task_kinds_after_privacy_authorization() {
        use crate::{cloud::CloudProvider, models::NewCloudDirectoryGroup};
        let store = TaskStore::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for kind in ["file", "web_video", "cloud", "ftp", "magnet"] {
            let mut new_task = NewTask::direct(format!("https://example.test/{kind}"));
            new_task.kind = kind.into();
            ids.push(store.create(new_task).unwrap().id.to_string());
        }
        let group = store.create_cloud_directory_group(NewCloudDirectoryGroup::new(
            CloudProvider::Quark, "root", "folder", "C:/downloads",
        ), &[]).unwrap();
        ids.push(group.id.to_string());
        for id in ids {
            for action in ["pause", "resume", "cancel"] {
                let expected_id = id.clone();
                let result = control_public(&store, &id, action, |routed_id, routed_action| async move {
                    assert_eq!(routed_id, expected_id);
                    assert_eq!(routed_action, action);
                    Ok(())
                }).await.unwrap();
                assert_eq!(result["id"], id);
            }
        }
        let private = store.create(NewTask { source: "https://example.test/private".into(), name: "private".into(), kind: "file".into(), is_private: true }).unwrap();
        for action in ["pause", "resume", "cancel"] {
            let result = control_public(&store, &private.id.to_string(), action, |_, _| async {
                panic!("private task reached the shared action router");
            }).await;
            assert_eq!(result, Err("NOT_FOUND"));
        }
        store.set_cloud_directory_group_private(group.id, true).unwrap();
        let result = control_public(&store, &group.id.to_string(), "cancel", |_, _| async {
            panic!("private directory reached the shared action router");
        }).await;
        assert_eq!(result, Err("NOT_FOUND"));
    }
}
