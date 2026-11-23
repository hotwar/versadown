//! What the embedded Gofile window may do, and what a captured file request
//! becomes.
//!
//! The window runs Gofile's own page. Two decisions are made here, purely on
//! the request address: which navigations the window may follow, and which
//! requests are the final file download that the app takes over. Nothing in
//! this module knows how the page obtains its download address.

use super::failure::GofileFailure;
use super::link::code_of_page;
use crate::cloud::share::{CloudShareEntry, CloudSharePlan, CloudShareRootKind};
use crate::cloud::{CloudProvider, QuarkError};
use serde_json::json;
use url::Url;

/// Sent with the download so it looks like the page's own request.
pub(super) const REFERER: &str = "https://gofile.io/";

/// Hosts that are Gofile pages or APIs, never file servers.
const NON_FILE_LABELS: [&str; 3] = ["www", "api", "gofile"];

fn is_gofile_host(host: &str) -> bool {
    host == "gofile.io" || host.ends_with(".gofile.io")
}

fn is_plain_https(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.port_or_known_default(), Some(443))
}

/// Whether the window may navigate (its main frame) to `url`. External links
/// are refused, not opened. Cloudflare's challenge page is the one foreign
/// origin Gofile's own page depends on.
pub(crate) fn is_page_navigation_allowed(url: &Url) -> bool {
    if url.as_str() == "about:blank" {
        return true;
    }
    is_plain_https(url)
        && url
            .host_str()
            .is_some_and(|host| is_gofile_host(host) || host == "challenges.cloudflare.com")
}

/// Whether `url` is a file download from a Gofile file server: a strict
/// subdomain of `gofile.io` that is not the website or the API, under
/// `/download/`, naming a file. The decision is made on the host the page
/// actually requested, so server names may change freely.
pub(crate) fn is_file_request(url: &Url) -> bool {
    if !is_plain_https(url) {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let Some(subdomain) = host.strip_suffix(".gofile.io") else {
        return false;
    };
    if subdomain.is_empty() || NON_FILE_LABELS.contains(&subdomain) {
        return false;
    }
    let Some(rest) = url.path().strip_prefix("/download/") else {
        return false;
    };
    // `/download/<...>/<file name>`: at least a name below the prefix.
    rest.split('/')
        .filter(|segment| !segment.is_empty())
        .count()
        >= 2
        && !url.path().ends_with('/')
}

fn header_safe(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

/// One file request taken over from the Gofile window.
pub(crate) struct CapturedDownload {
    /// The canonical share page the window was opened for.
    pub share_page: String,
    pub url: Url,
    /// File name suggested by the browser's download event, if any.
    pub suggested_name: Option<String>,
    /// `name=value; ...` of the visitor cookies the browser would send.
    pub cookie: Option<String>,
    pub user_agent: Option<String>,
}

impl CapturedDownload {
    /// The name shown and used for the file: the last path segment of the
    /// request (Gofile puts the file name there), else the browser's.
    pub fn file_name(&self) -> String {
        let from_url = self
            .url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .map(|segment| {
                percent_encoding::percent_decode_str(segment)
                    .decode_utf8_lossy()
                    .trim()
                    .to_owned()
            })
            .filter(|name| !name.is_empty());
        let name = from_url
            .or_else(|| {
                self.suggested_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "gofile-download".to_owned());
        crate::cloud::commands::safe_filename(&name)
    }

    /// A one-file share plan whose encrypted context carries everything the
    /// download needs. The task source derived from it holds only hashes.
    pub fn into_plan(&self) -> Result<CloudSharePlan, QuarkError> {
        if !is_file_request(&self.url) || code_of_page(&self.share_page).is_none() {
            return Err(QuarkError::InvalidInput("Gofile 下载请求无效".into()));
        }
        let entry_id = self.url.path().to_owned();
        // One plan per file: each carries its own visitor credentials, so a
        // later capture of the same file refreshes exactly that file.
        let share_id = format!("gofile{}{entry_id}", self.share_page);
        let entry =
            CloudShareEntry::new(&share_id, entry_id.clone(), self.file_name(), vec![], None);
        let context = json!({
            "share_page": self.share_page,
            "url": self.url.as_str(),
            "cookie": self.cookie,
            "user_agent": self.user_agent,
        });
        let mut plan =
            CloudSharePlan::new(CloudProvider::Gofile, share_id, None, vec![entry], context);
        plan.root_kind = CloudShareRootKind::File { entry_id };
        Ok(plan)
    }
}

/// The request headers of a captured download, rebuilt from a stored plan
/// context. Anything that could split a header is refused.
pub(super) fn download_headers(
    context: &serde_json::Value,
) -> Result<Vec<(String, String)>, QuarkError> {
    let mut headers = vec![("Referer".to_owned(), REFERER.to_owned())];
    for (key, name) in [("cookie", "Cookie"), ("user_agent", "User-Agent")] {
        if let Some(value) = context[key].as_str() {
            if !header_safe(value) {
                return Err(QuarkError::Gofile(GofileFailure::LinkExpired));
            }
            headers.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn only_gofile_file_server_downloads_are_taken_over() {
        for accepted in [
            "https://store-eu-par-3.gofile.io/download/web/0f3a-uuid/report.pdf",
            "https://store1.gofile.io/download/abc123/a%20b.zip?x=1",
            "https://anything-new.gofile.io/download/web/id/file.bin",
        ] {
            assert!(is_file_request(&url(accepted)), "{accepted}");
        }
        for rejected in [
            // Website, API, and lookalike hosts.
            "https://gofile.io/download/web/id/file.bin",
            "https://www.gofile.io/download/web/id/file.bin",
            "https://api.gofile.io/download/web/id/file.bin",
            "https://store1.gofile.io.evil.test/download/web/id/file.bin",
            "https://store1.evilgofile.io/download/web/id/file.bin",
            "https://evil.test/download/web/id/file.bin",
            // Not a file download path.
            "https://store1.gofile.io/d/abc123",
            "https://store1.gofile.io/download/",
            "https://store1.gofile.io/download/onlyone",
            "https://store1.gofile.io/download/web/id/",
            "https://store1.gofile.io/contents/abc",
            // Scheme, port, userinfo.
            "http://store1.gofile.io/download/web/id/file.bin",
            "https://store1.gofile.io:8443/download/web/id/file.bin",
            "https://user:pw@store1.gofile.io/download/web/id/file.bin",
        ] {
            assert!(!is_file_request(&url(rejected)), "{rejected}");
        }
    }

    /// The request the share page really makes when "Download" is clicked
    /// (observed in a normal browser visit, ids replaced): the page's script
    /// clicks an `<a href>` without a `download` attribute, so the window sees
    /// a main-frame navigation to the file server, which answers with
    /// `Content-Disposition: attachment`, a name-only last segment and no
    /// query string.
    #[test]
    fn the_observed_download_request_of_a_share_page_is_taken_over() {
        let observed = url(
            "https://store-na-phx-3.gofile.io/download/web/00000000-0000-4000-8000-000000000000/sample-file",
        );
        assert!(is_file_request(&observed));
        let download = CapturedDownload {
            share_page: "https://gofile.io/d/AbC123".into(),
            url: observed,
            suggested_name: None,
            cookie: Some("accountToken=guest-secret".into()),
            user_agent: None,
        };
        assert_eq!(download.file_name(), "sample-file");
        assert!(download.into_plan().is_ok());
    }

    #[test]
    fn navigation_stays_on_gofile_and_its_challenge_page() {
        for allowed in [
            "https://gofile.io/d/abc",
            "https://www.gofile.io/",
            "https://store1.gofile.io/download/web/id/file.bin",
            "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/x",
            "about:blank",
        ] {
            assert!(is_page_navigation_allowed(&url(allowed)), "{allowed}");
        }
        for refused in [
            "https://example.com/",
            "https://gofile.io.evil.test/d/abc",
            "https://evilgofile.io/",
            "http://gofile.io/d/abc",
            "https://gofile.io:444/d/abc",
            "https://user@gofile.io/d/abc",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hi",
        ] {
            assert!(!is_page_navigation_allowed(&url(refused)), "{refused}");
        }
    }

    fn capture(raw: &str) -> CapturedDownload {
        CapturedDownload {
            share_page: "https://gofile.io/d/AbC123".into(),
            url: url(raw),
            suggested_name: None,
            cookie: Some("accountToken=guest-secret; other=1".into()),
            user_agent: Some("Mozilla/5.0 test".into()),
        }
    }

    #[test]
    fn the_file_name_comes_from_the_request_then_the_browser() {
        let mut download =
            capture("https://store1.gofile.io/download/web/id/My%20File%20%281%29.zip");
        assert_eq!(download.file_name(), "My File (1).zip");
        download.url = url("https://store1.gofile.io/download/web/id/..%2F..%2Fx.bin");
        // An encoded traversal stays one harmless file name.
        assert!(!download.file_name().contains(['/', '\\']));
        download.url = url("https://store1.gofile.io/download/web/id/..");
        assert_eq!(download.file_name(), "gofile-download");
    }

    #[test]
    fn a_captured_request_becomes_a_one_file_plan_with_credentials_only_in_context() {
        let download = capture("https://store1.gofile.io/download/web/id/report.pdf?t=SECRETTOKEN");
        let plan = download.into_plan().unwrap();
        assert_eq!(plan.provider, CloudProvider::Gofile);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].name, "report.pdf");
        let entry_id = plan.entries[0].entry_id.clone();
        assert!(matches!(
            &plan.root_kind,
            CloudShareRootKind::File { entry_id: root } if *root == entry_id
        ));
        assert_eq!(
            plan.provider_context["cookie"],
            "accountToken=guest-secret; other=1"
        );
        assert_eq!(
            plan.provider_context["share_page"],
            "https://gofile.io/d/AbC123"
        );
        // The durable task source holds hashes only.
        let source = plan.source_for(&entry_id);
        for secret in [
            "SECRETTOKEN",
            "guest-secret",
            "AbC123",
            "report.pdf",
            "gofile.io",
        ] {
            assert!(!source.contains(secret), "{secret} leaked into {source}");
        }
        // The same file captured again refreshes the same task identity.
        let again = capture("https://store9.gofile.io/download/web/id/report.pdf?t=NEW")
            .into_plan()
            .unwrap();
        assert_eq!(again.source_for(&again.entries[0].entry_id), source);
    }

    #[test]
    fn requests_that_are_not_gofile_downloads_never_become_plans() {
        let mut download = capture("https://evil.test/download/web/id/report.pdf");
        assert!(download.into_plan().is_err());
        download.url = url("https://store1.gofile.io/download/web/id/report.pdf");
        download.share_page = "https://evil.test/d/abc".into();
        assert!(download.into_plan().is_err());
    }

    #[test]
    fn headers_carry_the_visitor_cookie_and_refuse_header_splitting() {
        let context = json!({"cookie": "a=b", "user_agent": "UA/1"});
        let headers = download_headers(&context).unwrap();
        assert!(headers.contains(&("Cookie".into(), "a=b".into())));
        assert!(headers.contains(&("User-Agent".into(), "UA/1".into())));
        assert!(headers.contains(&("Referer".into(), REFERER.into())));
        for bad in ["a=b\r\nX-Evil: 1", "a=b\n", ""] {
            assert!(download_headers(&json!({"cookie": bad})).is_err());
        }
        // No cookie and no user agent is fine: only the referer remains.
        let bare = download_headers(&json!({"cookie": null, "user_agent": null})).unwrap();
        assert_eq!(bare.len(), 1);
    }
}
