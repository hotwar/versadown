//! Recognising a Gofile share page.

use url::Url;

fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The share code of `https://gofile.io/d/<code>` (also `www.`), nothing else.
fn share_code(url: &Url) -> Option<&str> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(url.host_str(), Some("gofile.io" | "www.gofile.io"))
    {
        return None;
    }
    let code = url.path().strip_prefix("/d/")?;
    valid_code(code).then_some(code)
}

/// The canonical page to open for a pasted share link: no query, no
/// fragment, no `www.`, always https.
pub(crate) fn share_page_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let code = share_code(&url)?;
    Url::parse(&format!("https://gofile.io/d/{code}")).ok()
}

/// The share code of an already canonical page address.
pub(super) fn code_of_page(page: &str) -> Option<String> {
    let url = Url::parse(page).ok()?;
    share_code(&url).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_pages_are_recognised_and_canonicalised() {
        for (raw, expected) in [
            ("https://gofile.io/d/AbC123", "https://gofile.io/d/AbC123"),
            (
                "http://www.gofile.io/d/AbC123?x=1#y",
                "https://gofile.io/d/AbC123",
            ),
            ("  https://gofile.io/d/a-b_c  ", "https://gofile.io/d/a-b_c"),
        ] {
            assert_eq!(share_page_url(raw).unwrap().as_str(), expected, "{raw}");
        }
    }

    #[test]
    fn lookalikes_and_other_pages_are_not_share_pages() {
        for raw in [
            "https://gofile.io/",
            "https://gofile.io/d/",
            "https://gofile.io/d/a/b",
            "https://gofile.io/download/abc",
            "https://gofile.io:8443/d/abc",
            "https://user@gofile.io/d/abc",
            "https://api.gofile.io/d/abc",
            "https://store1.gofile.io/d/abc",
            "https://gofile.io.evil.test/d/abc",
            "https://evilgofile.io/d/abc",
            "ftp://gofile.io/d/abc",
            "https://gofile.io/d/a%2Fb",
            "not a url",
        ] {
            assert!(share_page_url(raw).is_none(), "{raw}");
        }
    }
}
