//! Links handed to VersaDown by the system: a magnet, thunder or ed2k link
//! clicked in a browser arrives as a program argument, either of this process
//! or of a second launch that the single-instance plugin forwards here.
//!
//! Links wait in a queue until the manager window takes them, because a link
//! that starts the app arrives before the window can listen for events.

use std::ffi::OsStr;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

const PREFIXES: [&str; 3] = ["magnet:?", "thunder://", "ed2k://"];

#[derive(Default)]
pub struct PendingExternalLinks(Mutex<Vec<String>>);

impl PendingExternalLinks {
    fn push(&self, links: Vec<String>) {
        if let Ok(mut pending) = self.0.lock() {
            pending.extend(links);
        }
    }

    fn take(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }
}

pub fn links_from_args<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .filter_map(|arg| link_from_arg(&arg.as_ref().to_string_lossy()))
        .collect()
}

fn link_from_arg(arg: &str) -> Option<String> {
    let link = arg.trim();
    let lower = link.to_ascii_lowercase();
    if !PREFIXES.iter().any(|prefix| lower.starts_with(prefix)) {
        return None;
    }
    // Some browsers percent-encode the `|` separators of an ed2k link, which the
    // parser expects literally.
    if lower.starts_with("ed2k://") && !link.contains('|') && lower.contains("%7c") {
        return Some(
            percent_encoding::percent_decode_str(link)
                .decode_utf8_lossy()
                .into_owned(),
        );
    }
    Some(link.to_string())
}

/// Queues the links, brings the manager forward and tells it to take them.
pub fn deliver(app: &AppHandle, links: Vec<String>) {
    if links.is_empty() {
        return;
    }
    app.state::<PendingExternalLinks>().push(links);
    crate::launcher::reveal_manager(app, Some("downloads"));
    let _ = app.emit_to("main", "external-links-available", ());
}

#[tauri::command]
pub fn take_external_links(pending: State<'_, PendingExternalLinks>) -> Vec<String> {
    pending.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_download_links_from_the_arguments() {
        let links = links_from_args([
            "/usr/bin/versa-down",
            "--flag",
            " magnet:?xt=urn:btih:614a23274663874107b501d36c5b10fd75fba919&dn=a%20b ",
            "THUNDER://QUFodHRwOi8vZXhhbXBsZS5jb20vYS56aXBaWg==",
            "ed2k://|file|a.mkv|1024|0123456789ABCDEF0123456789ABCDEF|/",
            "https://example.com/a.zip",
            "flashget://abc",
        ]);

        assert_eq!(
            links,
            [
                "magnet:?xt=urn:btih:614a23274663874107b501d36c5b10fd75fba919&dn=a%20b",
                "THUNDER://QUFodHRwOi8vZXhhbXBsZS5jb20vYS56aXBaWg==",
                "ed2k://|file|a.mkv|1024|0123456789ABCDEF0123456789ABCDEF|/",
            ]
        );
    }

    #[test]
    fn decodes_an_ed2k_link_with_encoded_separators() {
        assert_eq!(
            links_from_args(["ed2k://%7Cfile%7C%E6%B5%8B%E8%AF%95.mkv%7C1024%7C0123456789ABCDEF0123456789ABCDEF%7C/"]),
            ["ed2k://|file|测试.mkv|1024|0123456789ABCDEF0123456789ABCDEF|/"]
        );
    }

    #[test]
    fn pending_links_are_taken_once() {
        let pending = PendingExternalLinks::default();
        pending.push(vec!["magnet:?xt=a".into()]);
        pending.push(vec!["ed2k://|file|b|1|x|/".into()]);

        assert_eq!(pending.take(), ["magnet:?xt=a", "ed2k://|file|b|1|x|/"]);
        assert!(pending.take().is_empty());
    }
}
