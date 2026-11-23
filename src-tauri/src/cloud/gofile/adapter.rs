//! Share adapter of Gofile. A share page is never inspected here: it is opened
//! in the embedded browser window and the page's own file request is captured
//! (`capture`). Resolving a captured file replays exactly that request, with
//! the visitor cookie the browser held, as an ordinary download.

use super::capture::{download_headers, is_file_request};
use super::failure::GofileFailure;
use crate::cloud::share::{
    CloudShareResolvedEntry, ShareInspect, ShareInspection, ShareResolution, ShareResolve,
};
use crate::cloud::{
    CloudHttpRequest, CloudHttpTransport, CloudProvider, QuarkError, ResolvedCloudDownload,
};
use std::collections::BTreeMap;

pub struct GofileShareAdapter<'a> {
    http: &'a dyn CloudHttpTransport,
}

impl<'a> GofileShareAdapter<'a> {
    pub fn new(http: &'a dyn CloudHttpTransport) -> Self {
        Self { http }
    }
}

impl ShareInspect for GofileShareAdapter<'_> {
    fn inspect(
        &self,
        _: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        Err(QuarkError::Gofile(GofileFailure::OpenSharePage))
    }
}

impl ShareResolve for GofileShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        if entry.provider != CloudProvider::Gofile || entry.plan.provider != CloudProvider::Gofile {
            return Err(QuarkError::InvalidResponse);
        }
        let context = &entry.plan.provider_context;
        let url = context["url"]
            .as_str()
            .and_then(|raw| url::Url::parse(raw).ok())
            .filter(is_file_request)
            .ok_or(QuarkError::InvalidResponse)?;
        let headers = download_headers(context)?;
        // One byte tells whether the captured request still works before the
        // download engine starts: a refused one is an expired link, not a
        // transfer failure.
        let mut probe_headers: BTreeMap<String, String> = headers.iter().cloned().collect();
        probe_headers.insert("Range".into(), "bytes=0-0".into());
        let detail = self.http.send_detailed(CloudHttpRequest {
            method: "GET",
            url: url.as_str().to_owned(),
            headers: probe_headers,
            body: None,
        })?;
        if matches!(detail.response.status, 401 | 403 | 404 | 410) {
            return Err(QuarkError::Gofile(GofileFailure::LinkExpired));
        }
        Ok(ShareResolution::Direct(ResolvedCloudDownload {
            url: url.into(),
            name: entry.entry.name.clone(),
            headers,
            transform: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::gofile::CapturedDownload;
    use crate::cloud::{CloudHttpDetail, CloudHttpResponse};
    use std::sync::Mutex;

    struct Probe {
        status: u16,
        sent: Mutex<Vec<CloudHttpRequest>>,
    }

    impl CloudHttpTransport for Probe {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            unreachable!("the probe uses send_detailed")
        }
        fn send_detailed(&self, request: CloudHttpRequest) -> Result<CloudHttpDetail, QuarkError> {
            self.sent.lock().unwrap().push(request);
            Ok(CloudHttpDetail {
                response: CloudHttpResponse {
                    status: self.status,
                    ..Default::default()
                },
                ..Default::default()
            })
        }
    }

    fn resolved_entry() -> CloudShareResolvedEntry {
        let plan = CapturedDownload {
            share_page: "https://gofile.io/d/AbC123".into(),
            url: url::Url::parse("https://store1.gofile.io/download/web/id/report.pdf").unwrap(),
            suggested_name: None,
            cookie: Some("accountToken=guest".into()),
            user_agent: Some("UA/1".into()),
        }
        .into_plan()
        .unwrap();
        CloudShareResolvedEntry {
            provider: CloudProvider::Gofile,
            entry: plan.entries[0].clone(),
            plan,
        }
    }

    #[test]
    fn a_working_capture_resolves_to_the_same_request_with_its_visitor_headers() {
        let http = Probe {
            status: 206,
            sent: Mutex::default(),
        };
        let ShareResolution::Direct(download) = GofileShareAdapter::new(&http)
            .resolve(&resolved_entry())
            .unwrap()
        else {
            panic!("a captured download is direct");
        };
        assert_eq!(
            download.url,
            "https://store1.gofile.io/download/web/id/report.pdf"
        );
        assert_eq!(download.name, "report.pdf");
        assert!(download
            .headers
            .contains(&("Cookie".into(), "accountToken=guest".into())));
        assert!(download
            .headers
            .contains(&("User-Agent".into(), "UA/1".into())));
        let probe = &http.sent.lock().unwrap()[0];
        assert_eq!(probe.headers["Range"], "bytes=0-0");
        assert_eq!(probe.headers["Cookie"], "accountToken=guest");
    }

    #[test]
    fn a_refused_capture_is_an_expired_link_not_a_transfer_failure() {
        for status in [401, 403, 404, 410] {
            let http = Probe {
                status,
                sent: Mutex::default(),
            };
            let error = GofileShareAdapter::new(&http)
                .resolve(&resolved_entry())
                .err()
                .unwrap();
            assert!(
                matches!(error, QuarkError::Gofile(GofileFailure::LinkExpired)),
                "{status}"
            );
        }
    }

    #[test]
    fn a_tampered_plan_cannot_redirect_the_request() {
        let http = Probe {
            status: 200,
            sent: Mutex::default(),
        };
        let mut entry = resolved_entry();
        entry.plan.provider_context["url"] =
            serde_json::json!("https://evil.test/download/web/id/x");
        assert!(GofileShareAdapter::new(&http).resolve(&entry).is_err());
        assert!(http.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_share_page_is_never_inspected_through_the_adapter() {
        let http = Probe {
            status: 200,
            sent: Mutex::default(),
        };
        let candidate =
            crate::parser::extract_cloud_share_candidate("https://gofile.io/d/AbC123", None)
                .unwrap()
                .unwrap();
        assert!(matches!(
            GofileShareAdapter::new(&http).inspect(&candidate),
            Err(QuarkError::Gofile(GofileFailure::OpenSharePage))
        ));
        assert!(http.sent.lock().unwrap().is_empty());
    }
}
