//! Startup policy for the shared desktop/Agent host. The background mode keeps
//! Tauri's event loop and the existing runtime, but creates desktop UI on demand.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, WebviewWindowBuilder};

#[derive(Clone, Debug)]
pub(crate) struct StartupOptions {
    pub agent_host: bool,
    pub background_ui_test: bool,
    pub test_single_instance: bool,
    pub data_dir: PathBuf,
}

impl StartupOptions {
    pub fn from_env() -> std::io::Result<Self> {
        let background_ui_test =
            std::env::var("VERSADOWN_BACKGROUND_UI_TEST").as_deref() == Ok("1");
        Self::from_inputs(
            is_agent_host_request(std::env::args_os().skip(1)),
            background_ui_test,
            std::env::var("VERSADOWN_TEST_SINGLE_INSTANCE").as_deref() == Ok("1"),
            std::env::var_os("VERSADOWN_TEST_DATA_DIR").map(PathBuf::from),
            crate::cli_protocol::default_discovery_path(),
        )
    }

    fn from_inputs(
        agent_host: bool,
        background_ui_test: bool,
        test_single_instance: bool,
        test_directory: Option<PathBuf>,
        discovery: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        if test_single_instance && !background_ui_test {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "test single-instance mode requires an isolated background test",
            ));
        }
        let mut data_dir = profile_directory(background_ui_test, test_directory, discovery)?;
        if test_single_instance {
            // Existing canonical directories bind both GUI and Agent launches
            // to one test identity, including equivalent path spellings.
            data_dir = data_dir.canonicalize()?;
            if !data_dir.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput, "test profile must be a directory",
                ));
            }
        }
        Ok(Self {
            agent_host,
            background_ui_test,
            test_single_instance,
            data_dir,
        })
    }

    pub fn use_single_instance(&self) -> bool {
        !self.background_ui_test || self.test_single_instance
    }

    pub fn single_instance_identifier(&self, production_identifier: &str) -> String {
        if self.background_ui_test && self.test_single_instance {
            let digest = Sha256::digest(self.data_dir.as_os_str().as_encoded_bytes());
            format!("{production_identifier}.agenttest-{digest:x}")
        } else {
            production_identifier.to_owned()
        }
    }
}

pub(crate) fn is_agent_host_request<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter().any(|arg| arg.as_ref() == "--agent-host")
}

fn profile_directory(
    background_ui_test: bool,
    test_directory: Option<PathBuf>,
    discovery: Option<PathBuf>,
) -> std::io::Result<PathBuf> {
    let directory = if background_ui_test {
        test_directory
    } else {
        discovery.and_then(|path| path.parent()?.parent().map(Path::to_path_buf))
    };
    directory.filter(|path| path.is_absolute()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "host profile unavailable")
    })
}

pub(crate) fn load_settings(data_dir: &Path) -> crate::AppSettings {
    std::fs::read(data_dir.join("settings.json"))
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default()
}

pub(crate) fn check_agent_startup(options: &StartupOptions) -> Result<(), i32> {
    if options.agent_host && !load_settings(&options.data_dir).agent_access.enabled {
        return Err(4);
    }
    Ok(())
}

pub(crate) fn configure_window_creation(
    windows: &mut [tauri::utils::config::WindowConfig],
    agent_host: bool,
) {
    if agent_host {
        for window in windows {
            window.create = false;
        }
    }
}

/// An OS lock survives neither process exit nor a crash. Keep the file itself:
/// deleting a locked file would allow another process to lock a different inode.
pub(crate) struct ProfileLease {
    _file: File,
}

impl ProfileLease {
    pub fn acquire(data_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_dir.join("agent-host.lock"))?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock.into(),
            std::fs::TryLockError::Error(error) => error,
        })?;
        Ok(Self { _file: file })
    }
}

#[derive(Default)]
struct PendingDesktop {
    ready: bool,
    // Some(None) means an ordinary show request without a navigation target.
    reveal: Option<Option<String>>,
}

pub(crate) struct RuntimeState {
    options: StartupOptions,
    desktop_enabled: AtomicBool,
    ui_initialized: AtomicBool,
    ui_initialization: Mutex<()>,
    pending: Mutex<PendingDesktop>,
}

impl RuntimeState {
    pub fn new(options: StartupOptions) -> Self {
        Self {
            desktop_enabled: AtomicBool::new(!options.agent_host),
            options,
            ui_initialized: AtomicBool::new(false),
            ui_initialization: Mutex::new(()),
            pending: Mutex::new(PendingDesktop::default()),
        }
    }

    fn defer_reveal(&self, destination: Option<&str>) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.ready {
            return false;
        }
        pending.reveal = Some(destination.map(str::to_owned));
        true
    }

    fn ready(&self) -> Option<Option<String>> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.ready = true;
        pending.reveal.take()
    }

    fn initialize_ui(&self, initialize: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let _initialization = self.ui_initialization.lock()
            .map_err(|_| "desktop initialization lock unavailable".to_owned())?;
        if self.ui_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        initialize()?;
        self.ui_initialized.store(true, Ordering::Release);
        self.desktop_enabled.store(true, Ordering::Release);
        Ok(())
    }
}

pub(crate) fn is_background_host(app: &AppHandle) -> bool {
    app.try_state::<RuntimeState>()
        .is_some_and(|state| !state.desktop_enabled.load(Ordering::Acquire))
}

pub(crate) fn defer_reveal(app: &AppHandle, destination: Option<&str>) -> bool {
    app.try_state::<RuntimeState>()
        .is_some_and(|state| state.defer_reveal(destination))
}

pub(crate) fn finish_setup(app: &AppHandle) {
    if let Some(destination) = app.state::<RuntimeState>().ready() {
        crate::launcher::reveal_manager(app, destination.as_deref());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UiInitializationThread {
    Setup,
    Worker,
}

/// Initial setup already runs on the UI thread before event dispatch begins.
pub(crate) fn ensure_desktop_ui(app: &AppHandle) -> Result<(), String> {
    ensure_desktop_ui_on(app, UiInitializationThread::Setup)
}

/// Dynamic WebView2 creation must run outside synchronous UI event handlers.
/// Only blocking workers call this entry; their initialization mutex never
/// blocks the UI thread that must service Tauri's window-creation requests.
pub(crate) fn ensure_desktop_ui_from_worker(app: &AppHandle) -> Result<(), String> {
    ensure_desktop_ui_on(app, UiInitializationThread::Worker)
}

fn on_main_thread<T: Send + 'static>(
    app: &AppHandle,
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || { let _ = sender.send(operation()); })
        .map_err(|error| error.to_string())?;
    receiver.recv().map_err(|_| "desktop UI thread unavailable".to_owned())?
}

fn ensure_launcher_for_initialization(
    app: &AppHandle,
    visibility: crate::LauncherVisibility,
    thread: UiInitializationThread,
) -> Result<Option<tauri::WebviewWindow>, String> {
    // Linux's related WebKit view is !Send and its GTK configuration is direct;
    // macOS's launcher helper also calls AppKit directly. Windows has neither
    // dependency, so both its main and launcher WebViews stay on the worker.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if thread == UiInitializationThread::Worker {
        let app_copy = app.clone();
        return on_main_thread(app, move || {
            crate::launcher_window::ensure_launcher_window(&app_copy, visibility)
        });
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = thread;
    crate::launcher_window::ensure_launcher_window(app, visibility)
}

fn ensure_desktop_ui_on(app: &AppHandle, thread: UiInitializationThread) -> Result<(), String> {
    let runtime = app.state::<RuntimeState>();
    runtime.initialize_ui(|| {
    if app.get_webview_window("main").is_none() {
        let config = app.config().app.windows.iter()
            .find(|window| window.label == "main")
            .ok_or_else(|| "main window configuration missing".to_owned())?;
        WebviewWindowBuilder::from_config(app, config)
            .map_err(|error| error.to_string())?
            .build().map_err(|error| error.to_string())?;
    }
    let visibility = app.state::<crate::commands::AppState>().settings.lock()
        .map_err(|_| "settings lock unavailable".to_owned())?.launcher_visibility;
    let launcher = ensure_launcher_for_initialization(app, visibility, thread)?;
    if let Some(main) = app.get_webview_window("main") {
        crate::webview_memory::configure_webview(&main);
        #[cfg(windows)]
        if !runtime.options.background_ui_test {
            if thread == UiInitializationThread::Setup {
                crate::window_interaction::install(&main)?;
            } else {
                let main_copy = main.clone();
                on_main_thread(app, move || crate::window_interaction::install(&main_copy))?;
            }
        }
        if !runtime.options.background_ui_test {
            main.set_min_size(Some(tauri::LogicalSize::new(960.0, 640.0)))
                .map_err(|error| error.to_string())?;
            main.show().map_err(|error| error.to_string())?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        app.set_activation_policy(tauri::ActivationPolicy::Regular)
            .map_err(|error| error.to_string())?;
        app.set_menu(crate::launcher::macos_application_menu(app)
            .map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    }
    if !runtime.options.background_ui_test {
        if app.tray_by_id(crate::launcher::TRAY_ID).is_none() {
            crate::launcher::setup_manager_tray(app)?;
        }
        if visibility == crate::LauncherVisibility::Always {
            if let Some(launcher) = launcher {
                crate::launcher_window::present_launcher(launcher)
                    .map_err(|error| error.to_string())?;
            }
        }
        let enabled = app.state::<crate::commands::AppState>().settings.lock()
            .map_err(|_| "settings lock unavailable".to_owned())?.associate_download_links;
        let data_dir = runtime.options.data_dir.clone();
        std::thread::spawn(move || {
            if let Err(error) = crate::link_association::sync(enabled, &data_dir) {
                eprintln!("Unable to update link association: {error}");
            }
        });
    }
    Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_runtime_profile_lease_excludes_duplicates_and_releases_on_drop() {
        let profile = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let first = ProfileLease::acquire(profile.path()).unwrap();
        assert_eq!(ProfileLease::acquire(profile.path()).err().unwrap().kind(), std::io::ErrorKind::WouldBlock);
        assert!(ProfileLease::acquire(other.path()).is_ok());
        drop(first);
        assert!(ProfileLease::acquire(profile.path()).is_ok());
    }

    #[test]
    fn agent_runtime_disabled_missing_or_invalid_settings_fail_closed() {
        let profile = tempfile::tempdir().unwrap();
        let mut options = StartupOptions {
            agent_host: true, background_ui_test: false, test_single_instance: false,
            data_dir: profile.path().to_owned(),
        };
        assert_eq!(check_agent_startup(&options), Err(4));
        std::fs::write(profile.path().join("settings.json"), "invalid").unwrap();
        assert_eq!(check_agent_startup(&options), Err(4));
        let mut settings = crate::AppSettings::default();
        settings.agent_access.enabled = true;
        std::fs::write(profile.path().join("settings.json"), serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(check_agent_startup(&options), Ok(()));
        settings.agent_access.enabled = false;
        std::fs::write(profile.path().join("settings.json"), serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(check_agent_startup(&options), Err(4));
        options.agent_host = false;
        assert_eq!(check_agent_startup(&options), Ok(()));
        assert!(!profile.path().join("tasks.sqlite").exists());
    }

    #[test]
    fn agent_runtime_profile_resolution_keeps_test_override_explicit() {
        let profile = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let discovery = profile.path().join("cli/connection.json");
        assert_eq!(profile_directory(false, Some(other.path().to_owned()), Some(discovery.clone())).unwrap(), profile.path());
        assert_eq!(profile_directory(true, Some(other.path().to_owned()), Some(discovery.clone())).unwrap(), other.path());
        assert!(profile_directory(true, None, Some(discovery)).is_err());
    }

    #[test]
    fn agent_runtime_background_request_stays_silent_and_desktop_waits_for_setup() {
        assert!(is_agent_host_request(["--agent-host"]));
        assert!(!is_agent_host_request(["--agent-host-other"]));
        assert!(!is_agent_host_request(["magnet:?xt=urn:btih:test"]));
        let profile = tempfile::tempdir().unwrap();
        let state = RuntimeState::new(StartupOptions {
            agent_host: true, background_ui_test: false, test_single_instance: false,
            data_dir: profile.path().to_owned(),
        });
        assert!(!state.desktop_enabled.load(Ordering::Acquire));
        assert!(state.defer_reveal(Some("downloads")));
        assert_eq!(state.ready(), Some(Some("downloads".to_owned())));
        assert!(!state.defer_reveal(None));
        assert_eq!(state.ready(), None);
    }

    #[test]
    fn agent_runtime_background_mode_disables_creation_instead_of_hiding_windows() {
        let mut windows = [
            tauri::utils::config::WindowConfig::default(),
            tauri::utils::config::WindowConfig {
                label: "launcher".into(), create: false, ..Default::default()
            },
        ];
        configure_window_creation(&mut windows, false);
        assert!(windows[0].create);
        assert!(!windows[1].create);
        configure_window_creation(&mut windows, true);
        assert!(windows.iter().all(|window| !window.create));
    }

    #[test]
    fn agent_runtime_ui_initialization_is_serial_and_retryable_after_failure() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        let profile = tempfile::tempdir().unwrap();
        let state = Arc::new(RuntimeState::new(StartupOptions {
            agent_host: true, background_ui_test: false, test_single_instance: false,
            data_dir: profile.path().to_owned(),
        }));
        assert!(state.initialize_ui(|| Err("creation failed".into())).is_err());
        assert!(!state.ui_initialized.load(Ordering::Acquire));
        assert!(!state.desktop_enabled.load(Ordering::Acquire));
        let creations = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let state = Arc::clone(&state);
            let creations = Arc::clone(&creations);
            workers.push(std::thread::spawn(move || {
                state.initialize_ui(|| {
                    creations.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    Ok(())
                }).unwrap();
            }));
        }
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(creations.load(Ordering::SeqCst), 1);
        assert!(state.desktop_enabled.load(Ordering::Acquire));
    }

    #[test]
    fn agent_runtime_test_single_instance_identity_is_bound_to_its_profile() {
        let profile = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let identifier = "com.versadown.desktop";
        let background = StartupOptions::from_inputs(
            true, true, true, Some(profile.path().to_owned()), None,
        ).unwrap();
        let desktop = StartupOptions::from_inputs(
            false, true, true, Some(profile.path().join(".")), None,
        ).unwrap();
        let unrelated = StartupOptions::from_inputs(
            true, true, true, Some(other.path().to_owned()), None,
        ).unwrap();
        assert!(background.use_single_instance());
        assert_eq!(background.data_dir, desktop.data_dir);
        assert_eq!(background.single_instance_identifier(identifier), desktop.single_instance_identifier(identifier));
        assert_ne!(background.single_instance_identifier(identifier), unrelated.single_instance_identifier(identifier));
        assert_ne!(background.single_instance_identifier(identifier), identifier);

        let production = StartupOptions::from_inputs(
            false, false, false, None, Some(profile.path().join("cli/connection.json")),
        ).unwrap();
        assert!(production.use_single_instance());
        assert_eq!(production.single_instance_identifier(identifier), identifier);
        let legacy_test = StartupOptions::from_inputs(
            false, true, false, Some(profile.path().to_owned()), None,
        ).unwrap();
        assert!(!legacy_test.use_single_instance());
        assert_eq!(legacy_test.single_instance_identifier(identifier), identifier);
    }

    #[test]
    fn agent_runtime_test_single_instance_requires_explicit_absolute_existing_profile() {
        let profile = tempfile::tempdir().unwrap();
        let discovery = Some(profile.path().join("cli/connection.json"));
        assert!(StartupOptions::from_inputs(false, false, true, Some(profile.path().to_owned()), discovery.clone()).is_err());
        assert!(StartupOptions::from_inputs(false, true, true, None, discovery.clone()).is_err());
        assert!(StartupOptions::from_inputs(false, true, true, Some(PathBuf::from("relative-test-profile")), discovery.clone()).is_err());
        assert!(StartupOptions::from_inputs(false, true, true, Some(profile.path().join("missing")), discovery).is_err());
        let file = profile.path().join("not-a-directory");
        std::fs::write(&file, "test").unwrap();
        assert!(StartupOptions::from_inputs(false, true, true, Some(file), None).is_err());
    }
}
