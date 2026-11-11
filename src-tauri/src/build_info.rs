#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
}

/// The source snapshot used to compile this process, not the current checkout.
#[tauri::command]
pub fn get_build_info() -> BuildInfo {
    let known = |value: &str| (!value.is_empty()).then(|| value.to_owned());
    BuildInfo {
        commit: known(env!("VERSADOWN_BUILD_COMMIT")),
        branch: known(env!("VERSADOWN_BUILD_BRANCH")),
        dirty: env!("VERSADOWN_BUILD_DIRTY") == "true",
    }
}
