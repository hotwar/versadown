//! Standalone console client: no database access and no download engine.
#[path = "../cli_protocol.rs"]
mod cli_protocol;
#[path = "../cli_client.rs"]
mod cli_client;

use cli_protocol::{Request, Response, PROTOCOL_VERSION};
use cli_client::LocalClient;
use serde_json::json;
use std::{path::PathBuf, process::ExitCode, time::Duration};

struct Options {
    request: Request,
    discovery: Option<PathBuf>,
    timeout: Duration,
    lang: String,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, ()> {
    let mut words = args.into_iter();
    let mut positional = Vec::new();
    let mut request = Request::default();
    let mut discovery = None;
    let mut timeout = None;
    let mut lang = if std::env::var("LANG").unwrap_or_default().starts_with("zh") {
        "zh-CN".to_string()
    } else { "en".to_string() };
    while let Some(word) = words.next() {
        let field = match word.as_str() {
            "--json" => continue,
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
        "help" | "version" | "health" | "list" if positional.len() <= 1 => {}
        "status" | "pause" | "resume" | "cancel" | "wait" | "result" if positional.len() == 2 => {
            if positional[1].is_empty() { return Err(()); }
            request.task_id = Some(positional[1].clone());
        }
        "add" if positional.len() == 2 => {
            let url = url::Url::parse(&positional[1]).map_err(|_| ())?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
                || !url.username().is_empty() || url.password().is_some() { return Err(()); }
            if !request.directory.as_ref().is_some_and(|d| std::path::Path::new(d).is_absolute())
                || !request.request_id.as_ref().is_some_and(|id| id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))) {
                return Err(());
            }
            request.url = Some(positional[1].clone());
        }
        _ => return Err(()),
    }
    if command != "add" && (request.directory.is_some() || request.request_id.is_some()) { return Err(()); }
    if command != "list" && (request.status.is_some() || request.query.is_some() || request.parent_id.is_some() || request.cursor.is_some() || request.limit.is_some()) { return Err(()); }
    if let Some(status) = &request.status {
        if !matches!(status.as_str(), "draft" | "queued" | "parsing" | "awaiting_user" | "downloading" | "paused" | "processing" | "completed" | "failed" | "cancelled" | "recycled") { return Err(()); }
    }
    request.command = command.to_string();
    request.lang = Some(lang.clone());
    Ok(Options { request, discovery, timeout: timeout.unwrap_or(Duration::from_secs(if command == "wait" {300} else {30})), lang })
}

fn help(lang: &str) -> Response {
    Response::success(json!({
        "description": cli_protocol::message("HELP", lang),
        "commands": ["help", "version", "health", "add URL --directory PATH --request-id ID", "list [--status STATUS] [--query TEXT] [--parent-id ID] [--cursor CURSOR] [--limit 1..100]", "status ID", "pause ID", "resume ID", "cancel ID", "wait ID [--timeout SECONDS]", "result ID"],
        "options": ["--json", "--lang en|zh-CN", "--discovery PATH", "--timeout SECONDS"],
        "protocol_version": PROTOCOL_VERSION
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
        "version" => return Response::success(json!({"name":"vd-cli", "version": env!("CARGO_PKG_VERSION"), "protocol_version": PROTOCOL_VERSION})),
        _ => {}
    }
    let client = match LocalClient::new(options.discovery.clone(), &options.lang) {
        Ok(client) => client, Err(error) => return error,
    };
    if options.request.command != "wait" { return client.call(&options.request).await; }
    let mut request = options.request.clone();
    request.command = "status".into();
    loop {
        let response = client.call(&request).await;
        if let Some(result) = terminal(&response, &options.lang) { return result; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
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
