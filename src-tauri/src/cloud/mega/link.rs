//! Parsing of public MEGA links: `https://mega.nz/file/<ID>#<KEY>` and
//! `https://mega.nz/folder/<ID>#<KEY>`.
//!
//! The fragment is the decryption key. It must never be logged, formatted into
//! an error, or sent to any server.

use super::crypto::decode_base64url;
use super::failure::MegaFailure;
use crate::cloud::QuarkError;
use zeroize::Zeroizing;

const FILE_KEY_BYTES: usize = 32;
const FOLDER_KEY_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MegaLinkKind {
    File,
    Folder,
}

pub struct MegaLink {
    pub kind: MegaLinkKind,
    /// Node handle of the shared file or folder.
    pub id: String,
    /// 32 bytes for a file link, 16 bytes for a folder link.
    pub key: Zeroizing<Vec<u8>>,
}

// A derived Debug would print the key.
impl std::fmt::Debug for MegaLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MegaLink")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

fn incomplete() -> QuarkError {
    QuarkError::Mega(MegaFailure::LinkIncomplete)
}

fn valid_handle(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Parse a candidate URL that the share parser has already routed to MEGA.
pub fn parse_mega_link(raw: &str) -> Result<MegaLink, QuarkError> {
    let url = url::Url::parse(raw).map_err(|_| incomplete())?;
    let path = url.path();
    let (kind, id) = if let Some(id) = path.strip_prefix("/file/") {
        (MegaLinkKind::File, id)
    } else if let Some(id) = path.strip_prefix("/folder/") {
        (MegaLinkKind::Folder, id)
    } else {
        return Err(incomplete());
    };
    if !valid_handle(id) {
        return Err(incomplete());
    }
    let fragment = url
        .fragment()
        .filter(|key| !key.is_empty())
        .ok_or_else(incomplete)?;
    // `/folder/<ID>#<KEY>/file/<ID2>` links one file inside a folder; that
    // form is not supported yet and must not silently download the folder.
    if fragment.contains('/') {
        return Err(QuarkError::Mega(MegaFailure::LinkUnsupported));
    }
    let key = decode_base64url(fragment).ok_or_else(incomplete)?;
    let expected = match kind {
        MegaLinkKind::File => FILE_KEY_BYTES,
        MegaLinkKind::Folder => FOLDER_KEY_BYTES,
    };
    if key.len() != expected {
        return Err(incomplete());
    }
    Ok(MegaLink {
        kind,
        id: id.to_owned(),
        key: Zeroizing::new(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 32 and 16 bytes of 0xAB, base64url without padding.
    const FILE_KEY: &str = "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s";
    const FOLDER_KEY: &str = "q6urq6urq6urq6urq6urqw";

    #[test]
    fn parses_file_and_folder_links() {
        let file = parse_mega_link(&format!("https://mega.nz/file/AbC_-123#{FILE_KEY}")).unwrap();
        assert_eq!(file.kind, MegaLinkKind::File);
        assert_eq!(file.id, "AbC_-123");
        assert_eq!(file.key.as_slice(), &[0xAB; 32]);
        let folder = parse_mega_link(&format!("https://mega.nz/folder/Zz9#{FOLDER_KEY}")).unwrap();
        assert_eq!(folder.kind, MegaLinkKind::Folder);
        assert_eq!(folder.key.as_slice(), &[0xAB; 16]);
    }

    #[test]
    fn missing_or_malformed_keys_are_incomplete() {
        for raw in [
            "https://mega.nz/file/AbC".to_owned(),
            "https://mega.nz/file/AbC#".to_owned(),
            "https://mega.nz/file/AbC#not-a-key".to_owned(),
            // A folder key on a file link (and vice versa) has the wrong size.
            format!("https://mega.nz/file/AbC#{FOLDER_KEY}"),
            format!("https://mega.nz/folder/AbC#{FILE_KEY}"),
        ] {
            assert!(
                matches!(
                    parse_mega_link(&raw),
                    Err(QuarkError::Mega(MegaFailure::LinkIncomplete))
                ),
                "{raw}"
            );
        }
    }

    #[test]
    fn single_file_inside_folder_links_are_unsupported_not_silently_widened() {
        let raw = format!("https://mega.nz/folder/AbC#{FOLDER_KEY}/file/Def");
        assert!(matches!(
            parse_mega_link(&raw),
            Err(QuarkError::Mega(MegaFailure::LinkUnsupported))
        ));
    }

    #[test]
    fn debug_output_hides_the_key() {
        let link = parse_mega_link(&format!("https://mega.nz/file/AbC#{FILE_KEY}")).unwrap();
        assert!(!format!("{link:?}").contains(FILE_KEY));
    }
}
