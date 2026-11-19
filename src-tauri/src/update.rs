use serde::{Deserialize, Serialize};
use std::{io::Read, time::Duration};

pub const LATEST_RELEASE_URL: &str = "https://www.versadown.com/releases/latest.json";
pub const DOWNLOAD_PAGE_URL: &str = "https://www.versadown.com/#gate";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_REDIRECTS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdate {
    pub version: String,
    pub notes: String,
    pub download_url: String,
}

#[derive(Deserialize)]
struct ReleaseManifest {
    version: String,
    #[serde(default)]
    notes: String,
}

pub fn fetch_app_update(current_version: &str) -> Result<Option<AppUpdate>, String> {
    let client = update_client(current_version)
        .build()
        .map_err(|error| format!("Unable to create update client: {error}"))?;
    fetch_app_update_from(&client, LATEST_RELEASE_URL, current_version)
}

fn update_client(current_version: &str) -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .user_agent(format!("VersaDown/{current_version}"))
}

fn fetch_app_update_from(
    client: &reqwest::blocking::Client,
    url: &str,
    current_version: &str,
) -> Result<Option<AppUpdate>, String> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .map_err(|error| format!("Unable to fetch release manifest: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Unexpected release response: {}", response.status()));
    }
    let mut body = Vec::new();
    response
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("Unable to read release manifest: {error}"))?;
    if body.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("Release manifest exceeds size limit".into());
    }
    let manifest = serde_json::from_slice::<ReleaseManifest>(&body)
        .map_err(|error| format!("Invalid release manifest: {error}"))?;
    app_update_from_manifest(current_version, manifest)
}

fn app_update_from_manifest(
    current_version: &str,
    manifest: ReleaseManifest,
) -> Result<Option<AppUpdate>, String> {
    let current =
        parse_version(current_version).ok_or_else(|| "Invalid current version".to_string())?;
    let latest =
        parse_version(&manifest.version).ok_or_else(|| "Invalid release version".to_string())?;
    if latest <= current {
        return Ok(None);
    }
    Ok(Some(AppUpdate {
        version: format!("{}.{}.{}", latest[0], latest[1], latest[2]),
        notes: manifest.notes.trim().into(),
        download_url: DOWNLOAD_PAGE_URL.into(),
    }))
}

fn parse_version(value: &str) -> Option<[u64; 3]> {
    let value = value
        .trim()
        .strip_prefix('v')
        .or_else(|| value.trim().strip_prefix('V'))
        .unwrap_or(value.trim());
    let mut parts = value.split('.');
    // Only stable releases are offered in this channel. Reject suffixes,
    // signed numbers and leading zeroes instead of silently changing priority.
    let parse_part = |part: Option<&str>| {
        let part = part?;
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        part.parse::<u64>().ok()
    };
    let parsed = [
        parse_part(parts.next())?,
        parse_part(parts.next())?,
        parse_part(parts.next())?,
    ];
    if parts.next().is_some() {
        None
    } else {
        Some(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};

    fn serve_manifest(
        status_and_headers: &str,
        body: String,
        delay: Duration,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/latest.json", listener.local_addr().unwrap());
        let status_and_headers = status_and_headers.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "request closed before its headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 8192);
            }
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 {status_and_headers}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            // The client intentionally closes early for timeout/oversize tests.
            let _ = stream.write_all(response.as_bytes());
            String::from_utf8(request).unwrap()
        });
        (url, server)
    }

    fn loopback_client() -> reqwest::blocking::ClientBuilder {
        update_client("0.3.0").https_only(false).no_proxy()
    }

    #[test]
    fn returns_an_update_only_for_a_newer_semantic_version() {
        let update = app_update_from_manifest(
            "0.3.9",
            ReleaseManifest {
                version: "0.3.10".into(),
                notes: "改进更新体验".into(),
            },
        )
        .unwrap()
        .unwrap();

        assert_eq!(update.version, "0.3.10");
        assert_eq!(update.notes, "改进更新体验");
        assert_eq!(update.download_url, DOWNLOAD_PAGE_URL);
        assert!(app_update_from_manifest(
            "0.3.0",
            ReleaseManifest {
                version: "0.3.0".into(),
                notes: String::new()
            }
        )
        .unwrap()
        .is_none());
        assert!(app_update_from_manifest(
            "0.3.0",
            ReleaseManifest {
                version: "0.2.9".into(),
                notes: String::new()
            }
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn rejects_invalid_and_non_stable_versions() {
        for version in [
            "next",
            "0.3",
            "0.3.1.0",
            "0.3.1-beta.1",
            "0.3.1+build.1",
            "0.03.1",
            "+0.3.1",
            "0.-3.1",
            "0.3.",
            "0.3.18446744073709551616",
        ] {
            assert!(
                app_update_from_manifest(
                    "0.3.0",
                    ReleaseManifest {
                        version: version.into(),
                        notes: String::new(),
                    },
                )
                .is_err(),
                "accepted invalid release: {version}"
            );
        }
        assert!(app_update_from_manifest(
            "invalid",
            ReleaseManifest {
                version: "0.3.1".into(),
                notes: String::new(),
            },
        )
        .is_err());
    }

    #[test]
    fn normalizes_version_labels_and_leaves_empty_notes_for_localization() {
        let update = app_update_from_manifest(
            "v0.3.0",
            ReleaseManifest {
                version: " V0.3.1 ".into(),
                notes: "  ".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(update.version, "0.3.1");
        assert_eq!(update.notes, "");
    }

    #[test]
    fn requests_fresh_json_and_accepts_optional_notes() {
        let (url, server) =
            serve_manifest("200 OK", r#"{"version":"0.3.1"}"#.into(), Duration::ZERO);
        let client = loopback_client().build().unwrap();
        let result = fetch_app_update_from(&client, &url, "0.3.0");
        let request = server.join().unwrap().to_ascii_lowercase();
        let update = result.unwrap().unwrap();
        assert_eq!(update.version, "0.3.1");
        assert_eq!(update.notes, "");
        assert_eq!(update.download_url, "https://www.versadown.com/#gate");
        assert!(request.starts_with("get /latest.json http/1.1\r\n"));
        assert!(request.contains("\r\naccept: application/json\r\n"));
        assert!(request.contains("\r\ncache-control: no-cache\r\n"));
        assert!(request.contains("\r\nuser-agent: versadown/0.3.0\r\n"));
    }

    #[test]
    fn follows_a_redirect_to_a_manifest_on_another_origin() {
        let (target, target_server) =
            serve_manifest("200 OK", r#"{"version":"0.3.1"}"#.into(), Duration::ZERO);
        let (url, redirect_server) = serve_manifest(
            &format!("301 Moved Permanently\r\nLocation: {target}"),
            String::new(),
            Duration::ZERO,
        );
        let client = loopback_client().build().unwrap();
        let update = fetch_app_update_from(&client, &url, "0.3.0")
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "0.3.1");
        assert_eq!(update.download_url, DOWNLOAD_PAGE_URL);
        redirect_server.join().unwrap();
        target_server.join().unwrap();
    }

    #[test]
    fn rejects_http_errors_redirects_without_location_and_invalid_json() {
        let client = loopback_client().build().unwrap();
        for (status, body, expected_error) in [
            ("404 Not Found", "", "Unexpected release response: 404"),
            (
                "302 Found",
                r#"{"version":"0.3.1"}"#,
                "Unexpected release response: 302",
            ),
            (
                "200 OK",
                "<html>Not a manifest</html>",
                "Invalid release manifest",
            ),
            (
                "200 OK",
                r#"{"notes":"missing version"}"#,
                "Invalid release manifest",
            ),
        ] {
            let (url, server) = serve_manifest(status, body.into(), Duration::ZERO);
            let result = fetch_app_update_from(&client, &url, "0.3.0");
            server.join().unwrap();
            assert!(result.unwrap_err().starts_with(expected_error));
        }
    }

    #[test]
    fn rejects_oversized_manifests() {
        let body = format!(
            r#"{{"version":"0.3.1","notes":"{}"}}"#,
            "x".repeat(MAX_MANIFEST_BYTES as usize)
        );
        let (url, server) = serve_manifest("200 OK", body, Duration::ZERO);
        let client = loopback_client().build().unwrap();
        let result = fetch_app_update_from(&client, &url, "0.3.0");
        server.join().unwrap();
        assert_eq!(result.unwrap_err(), "Release manifest exceeds size limit");
    }

    #[test]
    fn reports_timeouts_as_failure_instead_of_no_update() {
        let (url, server) = serve_manifest(
            "200 OK",
            r#"{"version":"0.3.1"}"#.into(),
            Duration::from_millis(250),
        );
        let client = loopback_client()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let result = fetch_app_update_from(&client, &url, "0.3.0");
        server.join().unwrap();
        assert!(result
            .unwrap_err()
            .starts_with("Unable to fetch release manifest"));
    }
}
