use serde::Serialize;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupHealth {
    pub credentials_unavailable: bool,
}

#[tauri::command]
pub fn startup_health(health: tauri::State<'_, StartupHealth>) -> StartupHealth {
    health.inner().clone()
}
