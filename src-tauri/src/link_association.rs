//! Makes VersaDown the system handler for magnet, thunder and ed2k links, so a
//! link clicked in a browser starts (or reaches) VersaDown with the link as its
//! argument.
//!
//! The launch target that was registered is recorded in the data directory.
//! Startup only registers again when that record is missing or the program
//! moved, so a handler the user picked later is not taken back on every launch;
//! turning the setting off removes only VersaDown's own registration.

use std::path::Path;

pub const SCHEMES: [&str; 3] = ["magnet", "thunder", "ed2k"];

/// The record of version 1 meant a hidden handler file; version 2 uses the
/// installed desktop entry when there is one, so it registers again.
const MARKER_FILE: &str = "link-association-v2";

/// Startup and a settings change can both sync; the marker must follow one at a time.
static SYNC: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn sync(enabled: bool, data_dir: &Path) -> Result<(), String> {
    let _guard = SYNC.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    sync_with(
        enabled,
        data_dir,
        launch_target,
        &platform::register,
        &platform::unregister,
    )
}

type Registration<'a> = &'a dyn Fn(&str) -> Result<(), String>;

fn sync_with(
    enabled: bool,
    data_dir: &Path,
    target: impl FnOnce() -> Result<String, String>,
    register: Registration,
    unregister: Registration,
) -> Result<(), String> {
    let marker = data_dir.join(MARKER_FILE);
    let recorded = std::fs::read_to_string(&marker).ok();
    if enabled {
        let target = target()?;
        if recorded.as_deref() == Some(target.as_str()) {
            return Ok(());
        }
        register(&target)?;
        std::fs::write(&marker, &target).map_err(|error| format!("无法记录链接关联：{error}"))
    } else if let Some(recorded) = recorded {
        unregister(&recorded)?;
        std::fs::remove_file(&marker).map_err(|error| format!("无法清除链接关联记录：{error}"))
    } else {
        Ok(())
    }
}

/// The program the system should start for a link. An AppImage runs from a
/// temporary mount, so the image itself is registered instead.
fn launch_target() -> Result<String, String> {
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|value| !value.is_empty()) {
        return Ok(appimage.to_string_lossy().into_owned());
    }
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("无法确定程序路径：{error}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::SCHEMES;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Written for a program that has no installed desktop entry (an AppImage
    /// or a development build).
    pub(super) const HANDLER_FILE: &str = "versadown-handler.desktop";
    /// The desktop entry the deb and rpm install. It declares the schemes, so
    /// links open VersaDown as soon as the package is installed.
    pub(super) const PACKAGED_ENTRY: &str = "VersaDown.desktop";

    type Run<'a> = &'a dyn Fn(&str, &[String]) -> Result<(), String>;

    pub(super) fn register(target: &str) -> Result<(), String> {
        register_in(
            &data_home()?,
            &config_home()?,
            target,
            packaged_entry(target),
            &run,
        )
    }

    pub(super) fn unregister(target: &str) -> Result<(), String> {
        unregister_in(&data_home()?, &config_home()?, packaged_entry(target), &run)
    }

    pub(super) fn register_in(
        data_home: &Path,
        config_home: &Path,
        target: &str,
        packaged: Option<&str>,
        run: Run,
    ) -> Result<(), String> {
        let applications = data_home.join("applications");
        let handler = match packaged {
            Some(entry) => {
                // Undo an earlier opt-out, and drop the hidden handler an
                // unpackaged build may have left behind.
                edit_mimeapps(config_home, |content| {
                    edit_section(content, "Removed Associations", &|handlers| {
                        without(handlers, &[entry])
                    })
                })?;
                remove_handler_file(&applications)?;
                entry
            }
            None => {
                std::fs::create_dir_all(&applications)
                    .map_err(|error| format!("无法创建应用目录：{error}"))?;
                std::fs::write(applications.join(HANDLER_FILE), handler_entry(target))
                    .map_err(|error| format!("无法写入链接关联：{error}"))?;
                HANDLER_FILE
            }
        };
        // Only refreshes the cache that some desktops read; xdg-mime below is
        // what sets the default, so a missing tool is not an error.
        let _ = run(
            "update-desktop-database",
            &[applications.to_string_lossy().into_owned()],
        );
        let mut arguments = vec!["default".to_string(), handler.to_string()];
        arguments.extend(mime_types());
        run("xdg-mime", &arguments).map_err(|error| format!("无法设置默认程序：{error}"))
    }

    pub(super) fn unregister_in(
        data_home: &Path,
        config_home: &Path,
        packaged: Option<&str>,
        run: Run,
    ) -> Result<(), String> {
        let ours: Vec<&str> = [Some(HANDLER_FILE), packaged].into_iter().flatten().collect();
        edit_mimeapps(config_home, |content| {
            let content = edit_section(content, "Default Applications", &|handlers| {
                without(handlers, &ours)
            });
            // The installed entry still declares the schemes; GIO, which
            // browsers and dde-open use, skips it once it is listed here.
            match packaged {
                Some(entry) => edit_section(&content, "Removed Associations", &|handlers| {
                    let mut handlers = without(handlers, &[entry]);
                    handlers.push(entry.to_string());
                    handlers
                }),
                None => content,
            }
        })?;
        let applications = data_home.join("applications");
        remove_handler_file(&applications)?;
        let _ = run(
            "update-desktop-database",
            &[applications.to_string_lossy().into_owned()],
        );
        Ok(())
    }

    /// The installed desktop entry, when it starts this same program.
    fn packaged_entry(target: &str) -> Option<&'static str> {
        let program = Path::new(target).file_name()?;
        system_data_dirs().into_iter().find_map(|dir| {
            let content =
                std::fs::read_to_string(dir.join("applications").join(PACKAGED_ENTRY)).ok()?;
            let exec = content.lines().find_map(|line| line.strip_prefix("Exec="))?;
            let command = exec.split_whitespace().next()?.trim_matches('"');
            (Path::new(command).file_name()? == program).then_some(PACKAGED_ENTRY)
        })
    }

    fn remove_handler_file(applications: &Path) -> Result<(), String> {
        match std::fs::remove_file(applications.join(HANDLER_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("无法删除链接关联：{error}")),
        }
    }

    fn without(handlers: Vec<String>, removed: &[&str]) -> Vec<String> {
        handlers
            .into_iter()
            .filter(|handler| !removed.contains(&handler.as_str()))
            .collect()
    }

    fn edit_mimeapps(config_home: &Path, edit: impl FnOnce(&str) -> String) -> Result<(), String> {
        let mimeapps = config_home.join("mimeapps.list");
        let content = std::fs::read_to_string(&mimeapps).unwrap_or_default();
        let updated = edit(&content);
        if updated == content {
            return Ok(());
        }
        std::fs::create_dir_all(config_home)
            .and_then(|()| std::fs::write(&mimeapps, updated))
            .map_err(|error| format!("无法更新默认程序列表：{error}"))
    }

    fn mime_types() -> impl Iterator<Item = String> {
        SCHEMES
            .iter()
            .map(|scheme| format!("x-scheme-handler/{scheme}"))
    }

    pub(super) fn handler_entry(target: &str) -> String {
        let mime_types: String = mime_types().map(|mime| format!("{mime};")).collect();
        format!(
            "[Desktop Entry]\nType=Application\nName=VersaDown\nExec={} %u\nTerminal=false\nNoDisplay=true\nMimeType={mime_types}\n",
            quote_exec(target)
        )
    }

    /// Quotes a path for a desktop entry's Exec key. Inside quotes `"`, `` ` ``,
    /// `$` and `\` take a backslash, `%` is doubled, and the string escaping of
    /// the file then doubles every backslash.
    fn quote_exec(path: &str) -> String {
        let mut quoted = String::with_capacity(path.len() + 2);
        quoted.push('"');
        for character in path.chars() {
            match character {
                '"' | '`' | '$' => quoted.push_str("\\\\"),
                '\\' => quoted.push_str("\\\\\\"),
                '%' => quoted.push('%'),
                _ => {}
            }
            quoted.push(character);
        }
        quoted.push('"');
        quoted
    }

    /// Rewrites the handler lists of VersaDown's schemes in one section of a
    /// mimeapps.list, adding the section or a key when needed; every other line
    /// is kept as it was.
    pub(super) fn edit_section(
        content: &str,
        section: &str,
        change: &dyn Fn(Vec<String>) -> Vec<String>,
    ) -> String {
        let header = format!("[{section}]");
        let keys: Vec<String> = mime_types().collect();
        let mut seen = vec![false; keys.len()];
        let mut lines: Vec<String> = Vec::new();
        let mut in_section = false;
        let mut found_section = false;
        let flush = |lines: &mut Vec<String>, seen: &[bool]| {
            let at = lines.len()
                - lines
                    .iter()
                    .rev()
                    .take_while(|line| line.trim().is_empty())
                    .count();
            let added: Vec<String> = keys
                .iter()
                .zip(seen)
                .filter(|(_, seen)| !**seen)
                .filter_map(|(key, _)| handler_line(key, change(Vec::new())))
                .collect();
            lines.splice(at..at, added);
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                if in_section {
                    flush(&mut lines, &seen);
                }
                in_section = trimmed == header;
                found_section |= in_section;
                lines.push(line.to_string());
                continue;
            }
            let key_index = line
                .split_once('=')
                .filter(|_| in_section)
                .and_then(|(key, _)| keys.iter().position(|ours| ours == key.trim()));
            let Some(index) = key_index else {
                lines.push(line.to_string());
                continue;
            };
            seen[index] = true;
            let value = line.split_once('=').map(|(_, value)| value).unwrap_or("");
            let handlers: Vec<String> = value
                .split(';')
                .map(str::trim)
                .filter(|handler| !handler.is_empty())
                .map(str::to_string)
                .collect();
            let changed = change(handlers.clone());
            if changed == handlers {
                lines.push(line.to_string());
            } else if let Some(line) = handler_line(&keys[index], changed) {
                lines.push(line);
            }
        }
        if in_section {
            flush(&mut lines, &seen);
        } else if !found_section {
            let added: Vec<String> = keys
                .iter()
                .filter_map(|key| handler_line(key, change(Vec::new())))
                .collect();
            if !added.is_empty() {
                if lines.last().is_some_and(|line| !line.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(header);
                lines.extend(added);
            }
        }
        let mut updated = lines.join("\n");
        if !updated.is_empty() && (content.ends_with('\n') || content.is_empty()) {
            updated.push('\n');
        }
        updated
    }

    fn handler_line(key: &str, handlers: Vec<String>) -> Option<String> {
        (!handlers.is_empty()).then(|| format!("{key}={};", handlers.join(";")))
    }

    fn run(program: &str, arguments: &[String]) -> Result<(), String> {
        let status = Command::new(program)
            .args(arguments)
            .status()
            .map_err(|error| format!("{program}：{error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{program} 退出码 {status}"))
        }
    }

    fn data_home() -> Result<PathBuf, String> {
        xdg_dir("XDG_DATA_HOME", ".local/share")
    }

    fn config_home() -> Result<PathBuf, String> {
        xdg_dir("XDG_CONFIG_HOME", ".config")
    }

    fn system_data_dirs() -> Vec<PathBuf> {
        std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string())
            .split(':')
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .collect()
    }

    fn xdg_dir(variable: &str, fallback: &str) -> Result<PathBuf, String> {
        if let Some(path) = std::env::var_os(variable)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            return Ok(path);
        }
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(fallback))
            .ok_or_else(|| "无法确定用户目录".to_string())
    }
}

#[cfg(windows)]
mod platform {
    use super::SCHEMES;
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    pub(super) fn register(target: &str) -> Result<(), String> {
        let classes = RegKey::predef(HKEY_CURRENT_USER);
        for scheme in SCHEMES {
            let base = format!("Software\\Classes\\{scheme}");
            let write = || -> std::io::Result<()> {
                let (key, _) = classes.create_subkey(&base)?;
                key.set_value("", &format!("URL:{scheme}"))?;
                key.set_value("URL Protocol", &"")?;
                let (icon, _) = classes.create_subkey(format!("{base}\\DefaultIcon"))?;
                icon.set_value("", &format!("\"{target}\",0"))?;
                let (command, _) = classes.create_subkey(format!("{base}\\shell\\open\\command"))?;
                command.set_value("", &open_command(target))
            };
            write().map_err(|error| format!("无法设置默认程序：{error}"))?;
        }
        Ok(())
    }

    /// Removes a scheme only while it still opens VersaDown; another program
    /// that took the scheme over keeps it, and machine-wide keys are untouched.
    pub(super) fn unregister(target: &str) -> Result<(), String> {
        let classes = RegKey::predef(HKEY_CURRENT_USER);
        for scheme in SCHEMES {
            let base = format!("Software\\Classes\\{scheme}");
            let registered: Option<String> = classes
                .open_subkey(format!("{base}\\shell\\open\\command"))
                .and_then(|key| key.get_value(""))
                .ok();
            if registered.as_deref() == Some(open_command(target).as_str()) {
                classes
                    .delete_subkey_all(&base)
                    .map_err(|error| format!("无法取消链接关联：{error}"))?;
            }
        }
        Ok(())
    }

    fn open_command(target: &str) -> String {
        format!("\"{target}\" \"%1\"")
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
mod platform {
    pub(super) fn register(_target: &str) -> Result<(), String> {
        Ok(())
    }

    pub(super) fn unregister(_target: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod sync_tests {
    use super::sync_with;
    use std::cell::RefCell;

    struct Calls(RefCell<Vec<String>>);

    impl Calls {
        fn new() -> Self {
            Self(RefCell::new(Vec::new()))
        }

        fn sync(&self, enabled: bool, dir: &std::path::Path, target: &str) {
            let register = |target: &str| {
                self.0.borrow_mut().push(format!("register {target}"));
                Ok(())
            };
            let unregister = |target: &str| {
                self.0.borrow_mut().push(format!("unregister {target}"));
                Ok(())
            };
            let target = target.to_string();
            sync_with(enabled, dir, || Ok(target), &register, &unregister).unwrap();
        }

        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.borrow_mut())
        }
    }

    #[test]
    fn registers_once_and_again_only_when_the_program_moves() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Calls::new();

        calls.sync(true, dir.path(), "/opt/a/versa-down");
        calls.sync(true, dir.path(), "/opt/a/versa-down");
        calls.sync(true, dir.path(), "/opt/b/versa-down");

        assert_eq!(
            calls.take(),
            ["register /opt/a/versa-down", "register /opt/b/versa-down"]
        );
    }

    #[test]
    fn turning_off_removes_the_recorded_registration_once() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Calls::new();
        calls.sync(true, dir.path(), "/opt/a/versa-down");
        calls.take();

        calls.sync(false, dir.path(), "/opt/b/versa-down");
        calls.sync(false, dir.path(), "/opt/b/versa-down");
        calls.sync(true, dir.path(), "/opt/b/versa-down");

        assert_eq!(
            calls.take(),
            ["unregister /opt/a/versa-down", "register /opt/b/versa-down"]
        );
    }

    #[test]
    fn a_failed_registration_is_retried_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let fail = |_: &str| Err("xdg-mime 退出码 1".to_string());
        let ok = |_: &str| Ok(());

        assert!(sync_with(true, dir.path(), || Ok("/opt/a".into()), &fail, &ok).is_err());

        let calls = Calls::new();
        calls.sync(true, dir.path(), "/opt/a");
        assert_eq!(calls.take(), ["register /opt/a"]);
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::platform::*;
    use std::cell::RefCell;
    use std::path::Path;

    fn ok(_: &str, _: &[String]) -> Result<(), String> {
        Ok(())
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn handler_entry_quotes_the_program_and_lists_every_scheme() {
        let entry = handler_entry("/opt/Versa Down/50%/a\"b$c\\d");

        assert!(entry.contains(r#"Exec="/opt/Versa Down/50%%/a\\"b\\$c\\\\d" %u"#));
        assert!(entry.contains(
            "MimeType=x-scheme-handler/magnet;x-scheme-handler/thunder;x-scheme-handler/ed2k;\n"
        ));
        assert!(entry.contains("NoDisplay=true"));
    }

    #[test]
    fn an_unpackaged_build_writes_a_handler_and_makes_it_the_default() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let calls = RefCell::new(Vec::new());
        let run = |program: &str, arguments: &[String]| {
            calls
                .borrow_mut()
                .push(format!("{program} {}", arguments.join(" ")));
            Ok(())
        };

        register_in(data.path(), config.path(), "/tmp/VersaDown.AppImage", None, &run).unwrap();

        let entry = read(&data.path().join("applications").join(HANDLER_FILE));
        assert!(entry.contains("Exec=\"/tmp/VersaDown.AppImage\" %u"));
        assert_eq!(
            calls.borrow().last().unwrap(),
            "xdg-mime default versadown-handler.desktop x-scheme-handler/magnet x-scheme-handler/thunder x-scheme-handler/ed2k"
        );
    }

    #[test]
    fn a_package_uses_its_desktop_entry_and_removes_an_old_handler_and_opt_out() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join("applications")).unwrap();
        std::fs::write(data.path().join("applications").join(HANDLER_FILE), "x").unwrap();
        std::fs::write(
            config.path().join("mimeapps.list"),
            "[Removed Associations]\nx-scheme-handler/magnet=VersaDown.desktop;\nx-scheme-handler/ed2k=amule.desktop;VersaDown.desktop;\n",
        )
        .unwrap();
        let calls = RefCell::new(Vec::new());
        let run = |program: &str, arguments: &[String]| {
            calls
                .borrow_mut()
                .push(format!("{program} {}", arguments.join(" ")));
            Ok(())
        };

        register_in(
            data.path(),
            config.path(),
            "/usr/bin/versadown",
            Some(PACKAGED_ENTRY),
            &run,
        )
        .unwrap();

        assert!(!data.path().join("applications").join(HANDLER_FILE).exists());
        assert_eq!(
            read(&config.path().join("mimeapps.list")),
            "[Removed Associations]\nx-scheme-handler/ed2k=amule.desktop;\n"
        );
        assert_eq!(
            calls.borrow().last().unwrap(),
            "xdg-mime default VersaDown.desktop x-scheme-handler/magnet x-scheme-handler/thunder x-scheme-handler/ed2k"
        );
    }

    #[test]
    fn register_reports_a_failed_default() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let run = |program: &str, _: &[String]| {
            if program == "xdg-mime" {
                Err("xdg-mime 退出码 1".to_string())
            } else {
                Ok(())
            }
        };

        assert!(register_in(data.path(), config.path(), "/usr/bin/versadown", None, &run).is_err());
    }

    #[test]
    fn unregister_removes_only_versadown_from_the_defaults() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join("applications")).unwrap();
        std::fs::write(data.path().join("applications").join(HANDLER_FILE), "x").unwrap();
        std::fs::write(
            config.path().join("mimeapps.list"),
            "[Default Applications]\nx-scheme-handler/magnet=versadown-handler.desktop\nx-scheme-handler/ed2k=amule.desktop;versadown-handler.desktop;\nx-scheme-handler/thunder=thunder.desktop\ntext/html=firefox.desktop\n\n[Added Associations]\nx-scheme-handler/magnet=versadown-handler.desktop;\n",
        )
        .unwrap();

        unregister_in(data.path(), config.path(), None, &ok).unwrap();

        assert_eq!(
            read(&config.path().join("mimeapps.list")),
            "[Default Applications]\nx-scheme-handler/ed2k=amule.desktop;\nx-scheme-handler/thunder=thunder.desktop\ntext/html=firefox.desktop\n\n[Added Associations]\nx-scheme-handler/magnet=versadown-handler.desktop;\n"
        );
        assert!(!data.path().join("applications").join(HANDLER_FILE).exists());
    }

    #[test]
    fn unregistering_a_package_opts_its_desktop_entry_out_of_every_scheme() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("mimeapps.list"),
            "[Default Applications]\nx-scheme-handler/magnet=VersaDown.desktop\n\n[Removed Associations]\nx-scheme-handler/ed2k=amule.desktop;\n\n[Added Associations]\ntext/plain=gedit.desktop;\n",
        )
        .unwrap();

        unregister_in(data.path(), config.path(), Some(PACKAGED_ENTRY), &ok).unwrap();

        assert_eq!(
            read(&config.path().join("mimeapps.list")),
            "[Default Applications]\n\n[Removed Associations]\nx-scheme-handler/ed2k=amule.desktop;VersaDown.desktop;\nx-scheme-handler/magnet=VersaDown.desktop;\nx-scheme-handler/thunder=VersaDown.desktop;\n\n[Added Associations]\ntext/plain=gedit.desktop;\n"
        );
    }

    #[test]
    fn unregistering_a_package_creates_the_opt_out_section_when_missing() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();

        unregister_in(data.path(), config.path(), Some(PACKAGED_ENTRY), &ok).unwrap();

        assert_eq!(
            read(&config.path().join("mimeapps.list")),
            "[Removed Associations]\nx-scheme-handler/magnet=VersaDown.desktop;\nx-scheme-handler/thunder=VersaDown.desktop;\nx-scheme-handler/ed2k=VersaDown.desktop;\n"
        );
    }

    #[test]
    fn unregister_without_a_registration_does_nothing() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();

        unregister_in(data.path(), config.path(), None, &ok).unwrap();

        assert!(!config.path().join("mimeapps.list").exists());
    }
}
