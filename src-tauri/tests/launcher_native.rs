use versadown_lib::native_menu_labels::NativeMenuLabels;
use versadown_lib::{
    launcher_menu_actions, should_apply_launcher_window_visibility, tray_menu_entries, AppSettings,
    LauncherInteractionState, LauncherVisibility,
};

#[test]
fn launcher_context_menu_contains_the_required_actions() {
    assert_eq!(
        launcher_menu_actions(),
        [
            "launcher.hide_manager",
            "launcher.new_download",
            "launcher.start_all",
            "launcher.pause_all",
            "launcher.settings",
            "launcher.quit",
        ]
    );
}

#[test]
fn tray_context_menu_matches_the_launcher_actions_when_manager_is_hidden() {
    let labels = NativeMenuLabels::fallback();
    assert_eq!(
        tray_menu_entries(&labels, false),
        [
            ("launcher.hide_manager", "Show main window"),
            ("launcher.new_download", "New download"),
            ("launcher.start_all", "Start all tasks"),
            ("launcher.pause_all", "Pause all tasks"),
            ("tray.launcher.always", "Always show"),
            ("tray.launcher.downloading", "Show only while downloading"),
            ("tray.launcher.hidden", "Turn off"),
            ("launcher.quit", "Quit VersaDown"),
        ]
    );
}

#[test]
fn background_interaction_state_records_menu_without_native_ui() {
    let state = LauncherInteractionState::new(true);
    state.record_menu();

    let snapshot = state.snapshot();
    assert!(state.is_background_test());
    assert_eq!(snapshot.menu_requests, 1);
}

#[test]
fn legacy_settings_default_to_always_showing_the_launcher() {
    let settings: AppSettings = serde_json::from_str(
        r#"{
            "debugReports": false,
            "downloadDirectory": "C:\\Downloads",
            "proxy": {
                "enabled": false,
                "protocol": "http",
                "host": "",
                "port": 7890,
                "username": "",
                "password": ""
            }
        }"#,
    )
    .expect("legacy settings should remain readable");

    assert_eq!(settings.launcher_visibility, LauncherVisibility::Always);
}

#[test]
fn application_exit_rejects_late_launcher_show_requests() {
    assert!(should_apply_launcher_window_visibility(false, true));
    assert!(should_apply_launcher_window_visibility(true, false));
    assert!(!should_apply_launcher_window_visibility(true, true));
}
