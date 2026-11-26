//! Text for the native tray menu. The renderer's i18n catalogs are the only
//! source of translations: it pushes the current labels on startup and on every
//! language change, and the last push is cached so the next launch builds the
//! tray in the same language before any renderer has loaded.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CACHE_FILE: &str = "native-menu-labels.json";

/// Every native label, by stable key. The renderer sends exactly these keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeMenuLabels {
    pub tooltip: String,
    pub hide_manager: String,
    pub show_manager: String,
    pub new_download: String,
    pub start_all: String,
    pub pause_all: String,
    pub launcher_visibility: String,
    pub launcher_always: String,
    pub launcher_downloading: String,
    pub launcher_hidden: String,
    pub quit: String,
}

impl NativeMenuLabels {
    /// Used only until the renderer has pushed labels for the first time
    /// (first launch): the interface language lives in the renderer's
    /// preferences, so the native side cannot know it and falls back to English.
    pub fn fallback() -> Self {
        Self {
            tooltip: "VersaDown Universal Downloader".into(),
            hide_manager: "Hide main window".into(),
            show_manager: "Show main window".into(),
            new_download: "New download".into(),
            start_all: "Start all tasks".into(),
            pause_all: "Pause all tasks".into(),
            launcher_visibility: "Floating launcher".into(),
            launcher_always: "Always show".into(),
            launcher_downloading: "Show only while downloading".into(),
            launcher_hidden: "Turn off".into(),
            quit: "Quit VersaDown".into(),
        }
    }

    pub fn is_complete(&self) -> bool {
        [
            &self.tooltip,
            &self.hide_manager,
            &self.show_manager,
            &self.new_download,
            &self.start_all,
            &self.pause_all,
            &self.launcher_visibility,
            &self.launcher_always,
            &self.launcher_downloading,
            &self.launcher_hidden,
            &self.quit,
        ]
        .iter()
        .all(|label| !label.trim().is_empty())
    }

    pub fn manager_item(&self, manager_visible: bool) -> &str {
        if manager_visible {
            &self.hide_manager
        } else {
            &self.show_manager
        }
    }
}

fn read_cache(path: &Path) -> Option<NativeMenuLabels> {
    let labels: NativeMenuLabels = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    labels.is_complete().then_some(labels)
}

fn write_cache(path: &Path, labels: &NativeMenuLabels) -> std::io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(labels).map_err(std::io::Error::other)?)?;
    std::fs::rename(temporary, path)
}

/// The labels the tray currently shows, plus where the last push is cached.
pub struct NativeMenuLabelStore {
    cache_path: PathBuf,
    labels: Mutex<NativeMenuLabels>,
}

impl NativeMenuLabelStore {
    pub fn open(data_dir: &Path) -> Self {
        let cache_path = data_dir.join(CACHE_FILE);
        let labels = read_cache(&cache_path).unwrap_or_else(NativeMenuLabels::fallback);
        Self { cache_path, labels: Mutex::new(labels) }
    }

    pub fn current(&self) -> NativeMenuLabels {
        self.labels.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    /// Stores a push and returns whether it differs from what was shown.
    pub fn update(&self, next: NativeMenuLabels) -> Result<bool, String> {
        if !next.is_complete() {
            return Err("原生菜单文案不能为空".to_string());
        }
        let mut current = self.labels.lock().unwrap_or_else(|error| error.into_inner());
        if *current == next {
            return Ok(false);
        }
        write_cache(&self.cache_path, &next)
            .map_err(|error| format!("无法保存原生菜单文案：{error}"))?;
        *current = next;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chinese() -> NativeMenuLabels {
        NativeMenuLabels {
            tooltip: "万能下载 VersaDown".into(),
            hide_manager: "隐藏主界面".into(),
            show_manager: "显示主界面".into(),
            new_download: "新建下载".into(),
            start_all: "开始全部任务".into(),
            pause_all: "暂停全部任务".into(),
            launcher_visibility: "悬浮球显示".into(),
            launcher_always: "总是显示".into(),
            launcher_downloading: "仅下载时显示".into(),
            launcher_hidden: "关闭显示".into(),
            quit: "退出万能下载".into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("versadown-labels-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_manager_item_label_follows_the_visibility() {
        let labels = chinese();
        assert_eq!(labels.manager_item(true), "隐藏主界面");
        assert_eq!(labels.manager_item(false), "显示主界面");
    }

    #[test]
    fn without_a_cache_the_store_falls_back_to_english() {
        let dir = scratch("fallback");
        let store = NativeMenuLabelStore::open(&dir);
        assert_eq!(store.current(), NativeMenuLabels::fallback());
        assert!(store.current().is_complete());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_push_is_cached_and_used_by_the_next_launch() {
        let dir = scratch("cache");
        let store = NativeMenuLabelStore::open(&dir);
        assert_eq!(store.update(chinese()), Ok(true));
        assert_eq!(store.update(chinese()), Ok(false));
        assert_eq!(store.current(), chinese());
        assert_eq!(NativeMenuLabelStore::open(&dir).current(), chinese());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_corrupt_or_incomplete_cache_is_ignored() {
        let dir = scratch("corrupt");
        std::fs::write(dir.join(CACHE_FILE), b"{not json").unwrap();
        assert_eq!(NativeMenuLabelStore::open(&dir).current(), NativeMenuLabels::fallback());
        let mut incomplete = chinese();
        incomplete.quit = " ".into();
        std::fs::write(dir.join(CACHE_FILE), serde_json::to_vec(&incomplete).unwrap()).unwrap();
        assert_eq!(NativeMenuLabelStore::open(&dir).current(), NativeMenuLabels::fallback());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_labels_are_rejected_and_not_cached() {
        let dir = scratch("reject");
        let store = NativeMenuLabelStore::open(&dir);
        let mut empty = chinese();
        empty.new_download = String::new();
        assert!(store.update(empty).is_err());
        assert_eq!(store.current(), NativeMenuLabels::fallback());
        assert!(!dir.join(CACHE_FILE).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_wire_format_uses_the_renderer_keys_and_rejects_unknown_ones() {
        let json = serde_json::to_value(chinese()).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "hideManager", "launcherAlways", "launcherDownloading", "launcherHidden",
                "launcherVisibility", "newDownload", "pauseAll", "quit", "showManager",
                "startAll", "tooltip",
            ]
        );
        let mut extra = json;
        extra["surprise"] = "x".into();
        assert!(serde_json::from_value::<NativeMenuLabels>(extra).is_err());
    }
}
