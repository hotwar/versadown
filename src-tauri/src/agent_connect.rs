//! Desktop entry for copying the same MCP configuration produced by vd-cli.
use serde::Serialize;
use std::path::{Path, PathBuf};

const DEV_PROFILE: &str = "VERSADOWN_AGENT_DEV_PROFILE";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConnectionInfo {
    pub executable_path: String,
    pub executable_exists: bool,
    pub config_json: String,
    pub installation_prompt: String,
}

fn connection_info_at(
    executable: &Path,
    dev_discovery: Option<PathBuf>,
    lang: &str,
) -> Result<AgentConnectionInfo, String> {
    let dev_profile = dev_discovery.is_some();
    let setup = crate::cli_setup::mcp_setup_at(executable, dev_discovery, None, false, lang)
        .map_err(str::to_owned)?;
    let mut installation_prompt = setup.installation_prompt;
    if dev_profile {
        installation_prompt.push_str(if lang == "zh-CN" {
            "\n\n这是 VD 开发实例的配置。请保留 --discovery 参数，并在调用时保持开发实例运行。"
        } else {
            "\n\nThis configuration points to a VD development instance. Keep the --discovery argument and leave the development instance running while using these tools."
        });
    }
    Ok(AgentConnectionInfo {
        executable_path: setup.executable_path,
        executable_exists: setup.executable_exists,
        config_json: setup.config_json,
        installation_prompt,
    })
}

#[tauri::command]
pub fn get_agent_connection_info(
    app: tauri::AppHandle,
    lang: Option<String>,
) -> Result<AgentConnectionInfo, String> {
    let lang = if lang.as_deref() == Some("zh-CN") { "zh-CN" } else { "en" };
    let executable = std::env::current_exe().map_err(|_| "INTERNAL_ERROR".to_owned())?;
    // The workspace launcher marks an isolated profile. The host owns its
    // discovery path, so an external Agent can contact the same running VD
    // without inheriting APPDATA or another process's environment.
    let dev_discovery = if std::env::var(DEV_PROFILE).as_deref() == Ok("1") {
        Some(crate::cli_host::current_discovery_path(&app).ok_or("INTERNAL_ERROR")?)
    } else {
        None
    };
    connection_info_at(&executable, dev_discovery, lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_setup_uses_current_bundle_and_reports_missing_companion() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("VD 中文 space");
        let executable = base.join(if cfg!(windows) { "versadown.exe" } else { "versadown" });
        let info = connection_info_at(&executable, None, "zh-CN").unwrap();
        assert!(!info.executable_exists);
        assert!(!base.exists());
        let config: serde_json::Value = serde_json::from_str(&info.config_json).unwrap();
        assert_eq!(config["mcpServers"]["versadown"]["command"], info.executable_path);
        assert_eq!(config["mcpServers"]["versadown"]["args"], serde_json::json!(["--lang", "zh-CN"]));
        assert!(Path::new(&info.executable_path).is_absolute());
        assert!(info.installation_prompt.contains(&info.config_json));
        std::fs::create_dir(&base).unwrap();
        std::fs::write(&info.executable_path, b"fixture only").unwrap();
        let ready = connection_info_at(&executable, None, "en").unwrap();
        assert!(ready.executable_exists);
        assert_eq!(ready.executable_path, info.executable_path);
        let serialized = serde_json::to_value(&ready).unwrap();
        assert_eq!(serialized["executableExists"], true);
        assert_eq!(serialized["configJson"], ready.config_json);
        assert!(serialized.get("executable_exists").is_none());
    }

    #[test]
    fn dev_setup_exports_only_the_active_profile_path() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join(if cfg!(windows) { "versadown.exe" } else { "versadown" });
        let discovery = temp.path().join("资料 with spaces").join("cli").join("connection.json");
        std::fs::create_dir_all(discovery.parent().unwrap()).unwrap();
        std::fs::write(&discovery, r#"{"port":1,"token":"must-not-be-copied"}"#).unwrap();
        let info = connection_info_at(&executable, Some(discovery.clone()), "zh-CN").unwrap();
        let config: serde_json::Value = serde_json::from_str(&info.config_json).unwrap();
        assert_eq!(config["mcpServers"]["versadown"]["args"],
            serde_json::json!(["--lang", "zh-CN", "--discovery", discovery]));
        assert_eq!(config["mcpServers"]["versadown"]["command"], info.executable_path);
        assert!(info.installation_prompt.contains("保持开发实例运行"));
        assert!(!info.config_json.contains("must-not-be-copied"));
        assert!(!info.installation_prompt.contains("must-not-be-copied"));
    }
}
