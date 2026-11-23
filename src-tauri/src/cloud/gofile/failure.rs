//! User-actionable Gofile failures. Their wire text is a stable localization
//! key (never provider text) so the UI can translate it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GofileFailure {
    /// The captured file address or its visitor cookie no longer works; the
    /// share page has to be opened again to get a fresh one.
    LinkExpired,
    /// A Gofile share is read in the embedded browser window, never through
    /// an adapter.
    OpenSharePage,
}

impl GofileFailure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::LinkExpired => "gofile_link_expired",
            Self::OpenSharePage => "gofile_open_share_page",
        }
    }

    /// Localization key; it is also the wire message.
    pub fn message(&self) -> &'static str {
        match self {
            Self::LinkExpired => "cloud.error.gofile.linkExpired",
            Self::OpenSharePage => "cloud.error.gofile.openSharePage",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_stable_localization_keys() {
        assert_eq!(
            GofileFailure::LinkExpired.message(),
            "cloud.error.gofile.linkExpired"
        );
        assert_eq!(GofileFailure::LinkExpired.code(), "gofile_link_expired");
    }
}
