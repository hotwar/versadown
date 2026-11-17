//! Standalone console client: no database access and no download engine.
#[path = "../cli_protocol.rs"]
mod cli_protocol;
#[path = "../cli_cloud_protocol.rs"]
mod cli_cloud_protocol;
#[path = "../cli_client.rs"]
mod cli_client;
#[path = "../cli_setup.rs"]
mod cli_setup;

use cli_protocol::{BatchItemInput, Request, ResourceConfirmInput, ResourcePrepareInput, Response, PROTOCOL_VERSION};
use cli_client::LocalClient;
use cli_cloud_protocol::CloudRequest;
use serde_json::json;
use std::{path::PathBuf, process::ExitCode, time::Duration};

struct Options {
    request: Request,
    discovery: Option<PathBuf>,
    timeout: Duration,
    lang: String,
    no_start: bool,
    input_file: Option<PathBuf>,
    cloud_file_action: Option<String>,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, ()> {
    let mut words = args.into_iter();
    let mut positional = Vec::new();
    let mut request = Request::default();
    let mut discovery = None;
    let mut timeout = None;
    let mut no_start = false;
    let mut input_file = None;
    let mut cloud_file_action = None;
    let mut provider = None;
    let mut lang = if std::env::var("LANG").unwrap_or_default().starts_with("zh") {
        "zh-CN".to_string()
    } else { "en".to_string() };
    while let Some(word) = words.next() {
        let field = match word.as_str() {
            "--json" => continue,
            "--no-start" if !no_start => { no_start = true; continue; }
            "--file" if input_file.is_none() => {
                let value = words.next().ok_or(())?;
                if value.is_empty() { return Err(()); }
                input_file = Some(PathBuf::from(value));
                continue;
            }
            "--provider" if provider.is_none() => {
                let value = words.next().ok_or(())?;
                if !bounded(&value, 32) { return Err(()); }
                provider = Some(value);
                continue;
            }
            "--help" | "-h" => { positional.push("help".into()); continue; }
            "--version" | "-V" => { positional.push("version".into()); continue; }
            "--lang" => {
                lang = words.next().ok_or(())?;
                if !matches!(lang.as_str(), "en" | "zh-CN") { return Err(()); }
                continue;
            }
            "--discovery" => {
                if discovery.is_some() { return Err(()); }
                discovery = Some(PathBuf::from(words.next().ok_or(())?));
                continue;
            }
            "--timeout" => {
                if timeout.is_some() { return Err(()); }
                let seconds: u64 = words.next().ok_or(())?.parse().map_err(|_| ())?;
                if seconds == 0 || seconds > 86400 { return Err(()); }
                timeout = Some(Duration::from_secs(seconds));
                continue;
            }
            "--limit" => {
                if request.limit.is_some() { return Err(()); }
                let value = words.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
                if !(1..=100).contains(&value) { return Err(()); }
                request.limit = Some(value);
                continue;
            }
            "--directory" => &mut request.directory,
            "--request-id" => &mut request.request_id,
            "--status" => &mut request.status,
            "--query" => &mut request.query,
            "--parent-id" => &mut request.parent_id,
            "--cursor" => &mut request.cursor,
            _ if word.starts_with('-') => return Err(()),
            _ => { positional.push(word); continue; }
        };
        if field.is_some() { return Err(()); }
        let value = words.next().ok_or(())?;
        if value.is_empty() { return Err(()); }
        *field = Some(value);
    }
    let command = positional.first().map(String::as_str).unwrap_or("help");
    match command {
        "cloud" => {
            let action = positional.get(1).map(String::as_str).ok_or(())?;
            let input = match action {
                "accounts" if positional.len() == 2 && input_file.is_none() => {
                    json!({"action":"accounts", "provider":provider})
                }
                "auth-begin" | "prepare" | "confirm" if positional.len() == 2 && input_file.is_some() => {
                    cloud_file_action = Some(action.replace('-', "_"));
                    serde_json::Value::Null
                }
                "auth-status" | "auth-cancel" if positional.len() == 3 && input_file.is_none() && bounded(&positional[2], 128) => {
                    json!({"action":action.replace('-', "_"), "authorization_id":positional[2]})
                }
                "status" | "cancel" if positional.len() == 3 && input_file.is_none() && bounded(&positional[2], 128) => {
                    json!({"action":action, "operation_id":positional[2]})
                }
                _ => return Err(()),
            };
            if action != "accounts" && provider.is_some() { return Err(()); }
            if !input.is_null() {
                let cloud: CloudRequest = serde_json::from_value(input).map_err(|_| ())?;
                if !cli_cloud_protocol::valid_request(&cloud) { return Err(()); }
                request.cloud = Some(cloud);
            }
        }
        "help" | "version" | "health" | "list" | "doctor" | "mcp-config" if positional.len() <= 1 => {}
        "batch-add" | "prepare" | "confirm" if positional.len() == 1 && input_file.is_some() => {}
        "status" | "pause" | "resume" | "cancel" | "wait" | "result" if positional.len() == 2 => {
            if positional[1].is_empty() { return Err(()); }
            request.task_id = Some(positional[1].clone());
        }
        "add" if positional.len() == 2 => {
            if !valid_direct_url(&positional[1]) { return Err(()); }
            if !request.directory.as_ref().is_some_and(|d| std::path::Path::new(d).is_absolute())
                || !request.request_id.as_ref().is_some_and(|id| valid_request_id(id)) {
                return Err(());
            }
            request.url = Some(positional[1].clone());
        }
        _ => return Err(()),
    }
    if !matches!(command, "batch-add" | "prepare" | "confirm" | "cloud") && input_file.is_some() { return Err(()); }
    if command != "cloud" && provider.is_some() { return Err(()); }
    if command != "add" && (request.directory.is_some() || request.request_id.is_some()) { return Err(()); }
    if command != "list" && (request.status.is_some() || request.query.is_some() || request.parent_id.is_some() || request.cursor.is_some() || request.limit.is_some()) { return Err(()); }
    if let Some(status) = &request.status {
        if !matches!(status.as_str(), "draft" | "queued" | "parsing" | "awaiting_user" | "downloading" | "paused" | "processing" | "completed" | "failed" | "cancelled" | "recycled") { return Err(()); }
    }
    request.command = if command == "batch-add" { "batch_add" } else { command }.to_string();
    request.lang = Some(lang.clone());
    Ok(Options { request, discovery, timeout: timeout.unwrap_or(Duration::from_secs(if command == "wait" {300} else {30})), lang, no_start, input_file, cloud_file_action })
}

fn help(lang: &str) -> Response {
    Response::success(json!({
        "description": cli_protocol::message("HELP", lang),
        "commands": ["help", "version", "doctor", "mcp-config", "health", "add URL --directory PATH --request-id ID", "batch-add --file JSON_FILE", "prepare --file JSON_FILE", "confirm --file JSON_FILE", "list [--status STATUS] [--query TEXT] [--parent-id ID] [--cursor CURSOR] [--limit 1..100]", "status ID", "pause ID", "resume ID", "cancel ID", "wait ID [--timeout SECONDS]", "result ID", "cloud accounts [--provider PROVIDER]", "cloud auth-begin --file JSON_FILE", "cloud auth-status AUTHORIZATION_ID", "cloud auth-cancel AUTHORIZATION_ID", "cloud prepare --file JSON_FILE", "cloud confirm --file JSON_FILE", "cloud status OPERATION_ID", "cloud cancel OPERATION_ID"],
        "options": ["--json", "--lang en|zh-CN", "--discovery PATH", "--no-start", "--timeout SECONDS"],
        "protocol_version": PROTOCOL_VERSION,
        "agent_guide": cli_setup::agent_guide(lang)
    }))
}

fn terminal(response: &Response, lang: &str) -> Option<Response> {
    if !response.ok { return Some(response.clone()); }
    let Some(data) = response.data.as_ref() else { return Some(Response::failure("PROTOCOL_ERROR", lang)); };
    match data.get("status").and_then(|s| s.as_str()) {
        Some("completed") if data.get("cleanup_pending").and_then(|v| v.as_bool()) != Some(true) => Some(response.clone()),
        Some("failed" | "cancelled" | "recycled") => {
            let code = if data["status"] == "failed" { "TASK_FAILED" } else { "TASK_CANCELLED" };
            let mut result = Response::failure(code, lang);
            result.data = response.data.clone();
            Some(result)
        }
        Some("completed" | "draft" | "parsing" | "awaiting_user" | "queued" | "downloading" | "processing" | "paused") => None,
        _ => Some(Response::failure("PROTOCOL_ERROR", lang)),
    }
}

async fn execute(options: &Options) -> Response {
    match options.request.command.as_str() {
        "help" => return help(&options.lang),
        "version" => return Response::success(json!({"name":"vd-cli", "version": env!("CARGO_PKG_VERSION"), "protocol_version": PROTOCOL_VERSION,
            "build_commit":cli_protocol::build_commit(), "build_dirty":cli_protocol::build_dirty()})),
        "doctor" => return cli_setup::doctor(options.discovery.clone(), &options.lang).await,
        "mcp-config" => return cli_setup::mcp_config(options.discovery.clone(), options.no_start, &options.lang),
        _ => {}
    }
    let mut request = options.request.clone();
    if let Some(file) = &options.input_file {
        let loaded = if let Some(action) = &options.cloud_file_action {
            load_cloud_request_file(&mut request, action, file).await
        } else { load_request_file(&mut request, file).await };
        if loaded.is_err() {
            return Response::failure("INVALID_ARGUMENT", &options.lang);
        }
    }
    let client = match LocalClient::new(options.discovery.clone(), &options.lang) {
        Ok(client) => client.with_auto_start(!options.no_start), Err(error) => return error,
    };
    if request.command != "wait" { return client.call(&request).await; }
    request.command = "status".into();
    loop {
        let response = client.call(&request).await;
        if let Some(result) = terminal(&response, &options.lang) { return result; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn valid_direct_url(input: &str) -> bool {
    if input.is_empty() || input.len() > 8192 { return false; }
    let Ok(url) = url::Url::parse(input) else { return false; };
    matches!(url.scheme(), "http" | "https" | "ftp") && url.host_str().is_some()
        && url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
}

fn bounded(value: &str, max: usize) -> bool { !value.trim().is_empty() && value.len() <= max }

async fn read_input_file(path: &std::path::Path) -> Result<Vec<u8>, ()> {
    use tokio::io::AsyncReadExt;
    const MAX_BYTES: u64 = 256 * 1024;
    let metadata = tokio::fs::metadata(path).await.map_err(|_| ())?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES { return Err(()); }
    let file = tokio::fs::File::open(path).await.map_err(|_| ())?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).await.map_err(|_| ())?;
    if bytes.len() > MAX_BYTES as usize { return Err(()); }
    Ok(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes).to_vec())
}

async fn load_request_file(request: &mut Request, path: &std::path::Path) -> Result<(), ()> {
    let bytes = read_input_file(path).await?;
    match request.command.as_str() {
        "batch_add" => {
            let items: Vec<BatchItemInput> = serde_json::from_slice(&bytes).map_err(|_| ())?;
            if items.is_empty() || items.len() > 20 { return Err(()); }
            let mut request_ids = std::collections::HashSet::new();
            for item in &items {
                // Keep per-item URL/directory semantics on the host so CLI and
                // MCP expose the same partial-success batch contract.
                if item.url.is_empty() || item.url.len() > 8192 || item.directory.is_empty() || item.directory.len() > 4096
                    || !valid_request_id(&item.request_id) || !request_ids.insert(item.request_id.clone()) {
                    return Err(());
                }
            }
            request.items = Some(items);
        }
        "prepare" => {
            let input: ResourcePrepareInput = serde_json::from_slice(&bytes).map_err(|_| ())?;
            if !matches!(input.kind.as_str(), "video" | "cloud" | "share" | "torrent") || input.input.len() > 8192
                || input.provider.as_deref().is_some_and(|value| !bounded(value, 32))
                || input.parent_id.as_deref().is_some_and(|value| !bounded(value, 256))
                || input.access_code.as_deref().is_some_and(|value| !bounded(value, 128)) { return Err(()); }
            request.prepare = Some(input);
        }
        "confirm" => {
            let input: ResourceConfirmInput = serde_json::from_slice(&bytes).map_err(|_| ())?;
            if !bounded(&input.preparation_id, 128) || !bounded(&input.directory, 4096)
                || !std::path::Path::new(&input.directory).is_absolute() || !valid_request_id(&input.request_id)
                || input.selection_ids.is_empty() || input.selection_ids.len() > 100
                || input.selection_ids.iter().any(|value| !bounded(value, 256))
                || input.selection_ids.iter().collect::<std::collections::HashSet<_>>().len() != input.selection_ids.len() { return Err(()); }
            request.confirm = Some(input);
        }
        _ => return Err(()),
    }
    Ok(())
}

async fn load_cloud_request_file(request: &mut Request, action: &str, path: &std::path::Path) -> Result<(), ()> {
    let bytes = read_input_file(path).await?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let object = value.as_object_mut().ok_or(())?;
    // The selected command owns the action. A file cannot silently turn a
    // preview or authorization request into a different operation.
    if object.contains_key("action") { return Err(()); }
    object.insert("action".into(), json!(action));
    let cloud: CloudRequest = serde_json::from_value(value).map_err(|_| ())?;
    if !cli_cloud_protocol::valid_request(&cloud) { return Err(()); }
    request.cloud = Some(cloud);
    Ok(())
}

async fn run(options: &Options, interrupt: impl std::future::Future<Output = std::io::Result<()>>) -> Response {
    tokio::select! {
        response = execute(options) => response,
        _ = tokio::time::sleep(options.timeout) => Response::failure("TIMEOUT", &options.lang),
        signal = interrupt => Response::failure(if signal.is_ok() { "INTERRUPTED" } else { "INTERNAL_ERROR" }, &options.lang),
    }
}

async fn interrupt_signal() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Windows inherits the parent's ignore-Ctrl-C attribute independently
        // of registered handlers. Tokio installs a handler but leaves that
        // attribute untouched. Register first, then restore signal delivery so
        // shells/process groups that ignore Ctrl-C cannot disable our contract.
        let mut signal = tokio::signal::windows::ctrl_c()?;
        unsafe { windows::Win32::System::Console::SetConsoleCtrlHandler(None, false) }
            .map_err(std::io::Error::other)?;
        signal.recv().await;
        Ok(())
    }
    #[cfg(not(windows))]
    tokio::signal::ctrl_c().await
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let lang = args.windows(2).find(|pair| pair[0] == "--lang").and_then(|pair| pair[1].to_str()).unwrap_or("en");
    let response = match args.iter().map(|v| v.to_str().map(str::to_owned)).collect::<Option<Vec<_>>>().ok_or(()).and_then(parse) {
        Err(()) => Response::failure("INVALID_ARGUMENT", lang),
        Ok(options) => run(&options, interrupt_signal()).await,
    };
    let code = cli_protocol::exit_code(&response);
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if serde_json::to_writer(&mut out, &response).is_err() || writeln!(out).is_err() { return ExitCode::from(1); }
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(args: &[&str]) -> Result<Options, ()> { parse(args.iter().map(|s| s.to_string())) }

    #[test]
    fn cli_parser_accepts_unicode_and_spaces_without_rewriting() {
        let dir = std::env::temp_dir().join("中文 下载");
        let dir = dir.to_str().unwrap();
        let o = options(&["--json", "add", "https://example.com/file", "--directory", dir, "--request-id", "request-1", "--lang", "zh-CN"]).unwrap();
        assert_eq!(o.request.directory.as_deref(), Some(dir));
        assert_eq!(o.request.lang.as_deref(), Some("zh-CN"));
        let list = options(&["list", "--parent-id", "parent", "--cursor", "opaque", "--limit", "50"]).unwrap();
        assert_eq!(list.request.parent_id.as_deref(), Some("parent"));
        assert_eq!(list.request.cursor.as_deref(), Some("opaque"));
        assert_eq!(list.request.limit, Some(50));
        assert_eq!(options(&["result", "task"]).unwrap().request.command,"result");
        assert!(options(&["health", "--no-start"]).unwrap().no_start);
        assert!(!options(&["health"]).unwrap().no_start);
        assert!(options(&["health", "--no-start", "--no-start"]).is_err());
    }

    #[test]
    fn cli_parser_rejects_ambiguous_or_unsupported_arguments() {
        for args in [vec!["wat"], vec!["pause"], vec!["status", "id", "extra"], vec!["health", "--directory", "x"], vec!["wait", "id", "--timeout", "0"], vec!["list", "--status", "secret"], vec!["add", "https://user:secret@example.com/a", "--directory", "/tmp", "--request-id", "x"]] {
            assert!(options(&args).is_err(), "{args:?}");
        }
        for args in [vec!["list","--limit","0"],vec!["list","--limit","101"],vec!["list","--limit","2","--limit","2"],vec!["result","id","--cursor","x"],vec!["health","--parent-id","x"]] {
            assert!(options(&args).is_err(),"{args:?}");
        }
    }

    #[test]
    fn setup_and_resource_commands_have_unambiguous_file_inputs() {
        assert_eq!(options(&["doctor"]).unwrap().request.command, "doctor");
        assert_eq!(options(&["mcp-config", "--no-start"]).unwrap().request.command, "mcp-config");
        for (command, wire) in [("batch-add", "batch_add"), ("prepare", "prepare"), ("confirm", "confirm")] {
            let parsed = options(&[command, "--file", "中文 input.json"]).unwrap();
            assert_eq!(parsed.request.command, wire);
            assert_eq!(parsed.input_file.unwrap(), PathBuf::from("中文 input.json"));
            assert!(options(&[command]).is_err());
            assert!(options(&[command, "--file", "one.json", "--file", "two.json"]).is_err());
        }
        for command in ["health", "doctor", "mcp-config", "list"] {
            assert!(options(&[command, "--file", "input.json"]).is_err());
        }
        let directory = std::env::temp_dir();
        assert!(options(&["add", "ftp://example.test/file.bin", "--directory", directory.to_str().unwrap(), "--request-id", "ftp-1"]).is_ok());
        for url in ["ftp://name:secret@example.test/a", "https://example.test/a#fragment"] {
            assert!(options(&["add", url, "--directory", directory.to_str().unwrap(), "--request-id", "x"]).is_err());
        }
    }

    #[test]
    fn cloud_parser_separates_authorization_queries_and_cloud_writes() {
        let request = options(&["cloud", "accounts", "--provider", "quark"]).unwrap().request;
        assert_eq!(request.command, "cloud");
        assert!(matches!(request.cloud, Some(CloudRequest::Accounts { provider: Some(ref name) }) if name == "quark"));
        for command in ["auth-begin", "prepare", "confirm"] {
            let parsed = options(&["cloud", command, "--file", "请求.json"]).unwrap();
            assert_eq!(parsed.cloud_file_action.as_deref(), Some(command.replace('-', "_").as_str()));
        }
        assert!(matches!(options(&["cloud", "auth-cancel", "auth-id"]).unwrap().request.cloud,
            Some(CloudRequest::AuthCancel { authorization_id }) if authorization_id == "auth-id"));
        assert!(matches!(options(&["cloud", "status", "operation-id"]).unwrap().request.cloud,
            Some(CloudRequest::Status { operation_id }) if operation_id == "operation-id"));
        for args in [
            vec!["cloud"], vec!["cloud", "auth-status"], vec!["cloud", "accounts", "--file", "x.json"],
            vec!["cloud", "prepare"], vec!["cloud", "cancel", "id", "extra"],
            vec!["cloud", "auth-status", "id", "--provider", "quark"], vec!["health", "--provider", "quark"],
        ] { assert!(options(&args).is_err(), "{args:?}"); }
    }

    #[tokio::test]
    async fn cloud_files_cannot_change_action_or_supply_login_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("cloud.json");
        let mut request = Request { command: "cloud".into(), ..Request::default() };
        std::fs::write(&file, r#"{"provider":"quark"}"#).unwrap();
        load_cloud_request_file(&mut request, "auth_begin", &file).await.unwrap();
        assert!(matches!(request.cloud, Some(CloudRequest::AuthBegin { .. })));
        for value in [
            json!({"provider":"quark","password":"never-forward"}),
            json!({"provider":"quark","cookie":"never-forward"}),
            json!({"action":"confirm","provider":"quark"}), json!([]),
        ] {
            std::fs::write(&file, value.to_string()).unwrap();
            assert!(load_cloud_request_file(&mut request, "auth_begin", &file).await.is_err());
        }
        std::fs::write(&file, json!({"input":{"operation":"create_share","selection_ids":["cloud-file"],
            "name":"课程","expires_in_days":7,"generate_pass_code":true}}).to_string()).unwrap();
        load_cloud_request_file(&mut request, "prepare", &file).await.unwrap();
        assert!(matches!(request.cloud, Some(CloudRequest::Prepare { .. })));
        std::fs::write(&file, json!({"input":{"operation":"create_share","selection_ids":["cloud-file"],
            "name":"课程","expires_in_days":366,"generate_pass_code":true}}).to_string()).unwrap();
        assert!(load_cloud_request_file(&mut request, "prepare", &file).await.is_err());
    }

    #[tokio::test]
    async fn batch_file_validates_envelope_before_contacting_host_but_keeps_item_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("batch.json");
        let one = json!({"url":"https://example.test/a", "directory":directory.path(), "request_id":"one"});
        let bad_semantics = json!({"url":"not-a-url", "directory":"relative", "request_id":"two"});
        std::fs::write(&file, json!([one.clone(),bad_semantics]).to_string()).unwrap();
        let mut request = Request {command:"batch_add".into(), ..Request::default()};
        load_request_file(&mut request, &file).await.unwrap();
        assert_eq!(request.items.unwrap().len(), 2, "item semantic failures belong to the host");
        for invalid in [json!([]), json!(vec![one.clone();21]), json!([one.clone(),one.clone()]),
            json!([{"url":"https://example.test/a","directory":"relative","request_id":"bad id"}]),
            json!([{"url":"https://example.test/a","directory":"relative","request_id":"one","credential":"secret"}])] {
            std::fs::write(&file, invalid.to_string()).unwrap();
            let parsed = options(&["batch-add", "--file", file.to_str().unwrap(), "--discovery", directory.path().join("missing.json").to_str().unwrap()]).unwrap();
            assert_eq!(execute(&parsed).await.error.unwrap().code, "INVALID_ARGUMENT");
        }
        std::fs::write(&file, vec![b' ';256*1024+1]).unwrap();
        assert!(read_input_file(&file).await.is_err());
        assert!(read_input_file(directory.path()).await.is_err());
        assert!(!directory.path().join("agent-start.lock").exists());
    }

    #[tokio::test]
    async fn resource_files_validate_structure_and_bound_choices_without_echoing_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("resource.json");
        std::fs::write(&file, "\u{feff}{\"kind\":\"cloud\",\"provider\":\"quark\"}").unwrap();
        let mut request = Request {command:"prepare".into(), ..Request::default()};
        load_request_file(&mut request, &file).await.unwrap();
        assert_eq!(request.prepare.unwrap().input, "");
        std::fs::write(&file, json!({"preparation_id":"prepared","selection_ids":["choice"],"directory":directory.path(),"request_id":"confirm-1"}).to_string()).unwrap();
        let mut request = Request {command:"confirm".into(), ..Request::default()};
        load_request_file(&mut request, &file).await.unwrap();
        assert_eq!(request.confirm.unwrap().selection_ids, vec!["choice"]);
        for selections in [Vec::<String>::new(), vec!["choice".into();101], vec!["choice".into();2]] {
            std::fs::write(&file, json!({"preparation_id":"prepared","selection_ids":selections,"directory":directory.path(),"request_id":"confirm-1"}).to_string()).unwrap();
            let mut request = Request {command:"confirm".into(), ..Request::default()};
            assert!(load_request_file(&mut request, &file).await.is_err());
        }
        std::fs::write(&file, json!({"kind":"share","input":"https://example.test/share","access_code":"test-private-code","unexpected":true}).to_string()).unwrap();
        let parsed = options(&["prepare", "--file", file.to_str().unwrap(), "--discovery", directory.path().join("missing.json").to_str().unwrap()]).unwrap();
        let result = execute(&parsed).await;
        assert_eq!(result.error.as_ref().unwrap().code, "INVALID_ARGUMENT");
        assert!(!serde_json::to_string(&result).unwrap().contains("test-private-code"));
    }

    #[test]
    fn cli_wait_distinguishes_success_failure_cancel_and_pending() {
        for (status, code) in [("completed", 0), ("failed", 8), ("cancelled", 8)] {
            let result = terminal(&Response::success(json!({"id":"task", "status":status})), "en").unwrap();
            assert_eq!(cli_protocol::exit_code(&result), code);
        }
        assert!(terminal(&Response::success(json!({"status":"paused"})), "en").is_none());
        assert!(terminal(&Response::success(json!({"status":"completed", "cleanup_pending":true})), "en").is_none());
        assert_eq!(cli_protocol::exit_code(&terminal(&Response::success(json!({"status":"recycled"})), "en").unwrap()), 8);
        assert_eq!(cli_protocol::exit_code(&Response::failure("TIMEOUT", "en")), 7);
        assert_eq!(cli_protocol::exit_code(&Response::failure("INTERRUPTED", "en")), 130);
    }

    #[tokio::test]
    async fn cli_wait_timeout_and_interrupt_send_no_cancel() {
        use std::sync::{Arc, Mutex};
        let commands = Arc::new(Mutex::new(Vec::new()));
        let seen = commands.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = axum::Router::new().route("/v1/call", axum::routing::post(move |axum::Json(request): axum::Json<Request>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(request.command);
                axum::Json(Response::success(json!({"id":"task", "name":"file", "kind":"file", "status":"paused", "progress":0,
                    "total_length":null,"completed_length":0,"download_speed":0,"created_at":0,"updated_at":0,
                    "completed_at":null,"cleanup_pending":false,"child_count":0,"failure":null,"required_action":null})))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("discovery.json");
        std::fs::write(&path, serde_json::to_vec(&cli_protocol::Discovery { version:1,port,token:"a".repeat(64) }).unwrap()).unwrap();
        let mut options = options(&["wait", "task", "--discovery", path.to_str().unwrap()]).unwrap();
        options.timeout = Duration::from_millis(100);
        let timed_out = run(&options, std::future::pending()).await;
        assert_eq!(cli_protocol::exit_code(&timed_out), 7);
        options.timeout = Duration::from_secs(5);
        let interrupted = run(&options, async { tokio::time::sleep(Duration::from_millis(100)).await; Ok(()) }).await;
        assert_eq!(cli_protocol::exit_code(&interrupted), 130);
        let calls = commands.lock().unwrap();
        assert!(!calls.is_empty());
        assert!(calls.iter().all(|command| command == "status"));
        server.abort();
    }
}
