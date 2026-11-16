//! Bounded, side-effect-free torrent inspection for Agent prepare/confirm.
//! The exact inspected metadata is journaled; a later file replacement or a
//! metadata service's display-only listing cannot change the confirmed files.
use std::{collections::{BTreeMap, HashSet}, path::{Path, PathBuf}, time::Duration};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 100;
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct BtFile {
    pub index: u32,
    /// Relative to the dedicated task output directory, including torrent name.
    pub path: String,
    pub length: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct BtPreparation {
    pub source: String,
    pub kind: String,
    pub name: String,
    pub torrent: String,
    pub info_hash: String,
    pub files: Vec<BtFile>,
}

pub(crate) async fn prepare(app: &AppHandle, input: &str) -> Result<BtPreparation> {
    if input.is_empty() || input.len() > 8192 { return Err("INVALID_ARGUMENT"); }
    let state = app.state::<crate::commands::AppState>();
    let candidate = crate::parser::validate_download_input(input).map_err(|_| "INVALID_ARGUMENT")?;
    let (source, kind, torrent, expected_hash) = match candidate.kind {
        crate::parser::CandidateKind::Magnet => {
            let settings = state.settings.lock().map_err(|_| "INTERNAL_ERROR")?.magnet_metadata.clone();
            let expected = magnet_hash(&candidate.source)?;
            let metadata = tokio::time::timeout(Duration::from_secs(20),
                crate::resolve_magnet_torrent(&settings, &candidate.source)).await
                .map_err(|_| "METADATA_UNAVAILABLE")?.map_err(|_| "METADATA_UNAVAILABLE")?
                .ok_or("METADATA_UNAVAILABLE")?;
            (candidate.source, "magnet", metadata.torrent, Some(expected))
        },
        crate::parser::CandidateKind::Torrent => {
            let path = if Path::new(&candidate.source).is_absolute() { PathBuf::from(&candidate.source) }
                else { url::Url::parse(&candidate.source).ok().and_then(|url| url.to_file_path().ok()).ok_or("INVALID_ARGUMENT")? };
            let path = crate::agent_access::authorize_file(&state, &path)?;
            let source = path.to_str().ok_or("INVALID_ARGUMENT")?.to_owned();
            let torrent = crate::load_torrent_base64(&source).await.map_err(|_| "INVALID_ARGUMENT")?;
            crate::agent_access::authorize_file(&state, &path)?;
            (source, "torrent", torrent, None)
        },
        _ => return Err("UNSUPPORTED"),
    };
    if !crate::agent_access::is_enabled(&state) { return Err("HOST_DISABLED"); }
    let mut preparation = inspect(&torrent)?;
    if expected_hash.is_some_and(|expected| expected != preparation.info_hash) { return Err("INVALID_ARGUMENT"); }
    preparation.source = source;
    preparation.kind = kind.into();
    Ok(preparation)
}

pub(crate) async fn dispatch(app: &AppHandle, task_id: Uuid, metadata: &BtPreparation,
    indices: &[u32], options: &crate::UriSubmissionOptions) -> Result<crate::DownloadTask>
{
    let state = app.state::<crate::commands::AppState>();
    let root = options.directory.as_ref().ok_or("INVALID_ARGUMENT")?;
    crate::agent_access::authorize_directory(&state, root)?;
    let service = state.runtime.service().await.map_err(|_| "ENGINE_UNAVAILABLE")?;
    service.dispatch_cli_torrent(task_id, metadata, indices, options, &|path| {
        crate::agent_access::authorize_file(&state, path).is_ok()
    }).await.map_err(|error| match error {
        crate::DownloadServiceError::InvalidUriOption("authorization") => "FORBIDDEN",
        crate::DownloadServiceError::InvalidNativeCheckpoint => "CLI_RECOVERY_REQUIRED",
        crate::DownloadServiceError::InvalidUriOption(_) | crate::DownloadServiceError::Input(_) => "INVALID_ARGUMENT",
        _ => "ENGINE_UNAVAILABLE",
    })
}

pub(crate) fn output_directory(task_id: Uuid, parent: &Path) -> PathBuf {
    parent.join(format!("vd-{task_id}"))
}

pub(crate) fn inspect(torrent: &str) -> Result<BtPreparation> {
    if torrent.len() > (MAX_BYTES + 2) / 3 * 4 { return Err("UNSUPPORTED"); }
    let bytes = STANDARD.decode(torrent).map_err(|_| "INVALID_ARGUMENT")?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES { return Err("INVALID_ARGUMENT"); }
    let mut parser = Parser { bytes: &bytes, position: 0, nodes: 0 };
    let root = parser.node(0)?;
    if parser.position != bytes.len() { return Err("INVALID_ARGUMENT"); }
    let info = root.field(b"info")?;
    if info.optional(b"meta version")?.is_some() { return Err("UNSUPPORTED"); }
    let name = component(info.field(b"name")?.bytes()?)?;
    if let Some(utf8) = info.optional(b"name.utf-8")? {
        if component(utf8.bytes()?)? != name { return Err("UNSUPPORTED"); }
    }
    let piece_length = info.field(b"piece length")?.positive()?;
    let pieces = info.field(b"pieces")?.bytes()?;
    if pieces.is_empty() || pieces.len() % 20 != 0 { return Err("INVALID_ARGUMENT"); }
    let mut files = Vec::new();
    if let Some(list) = info.optional(b"files")? {
        if info.optional(b"length")?.is_some() { return Err("INVALID_ARGUMENT"); }
        let list = list.list()?;
        if list.is_empty() || list.len() > MAX_FILES { return Err("UNSUPPORTED"); }
        for entry in list {
            if entry.optional(b"symlink path")?.is_some()
                || entry.optional(b"attr")?.is_some_and(|attr| attr.bytes().map_or(true, |bytes| !bytes.is_empty())) {
                return Err("UNSUPPORTED");
            }
            let parts = entry.field(b"path")?.list()?;
            if parts.is_empty() || parts.len() > 16 { return Err("INVALID_ARGUMENT"); }
            let parts = parts.iter().map(|part| component(part.bytes()?)).collect::<Result<Vec<_>>>()?;
            if let Some(utf8) = entry.optional(b"path.utf-8")? {
                let alternate = utf8.list()?.iter().map(|part| component(part.bytes()?)).collect::<Result<Vec<_>>>()?;
                if alternate != parts { return Err("UNSUPPORTED"); }
            }
            let path = format!("{name}/{}", parts.join("/"));
            if path.len() > 1024 { return Err("UNSUPPORTED"); }
            files.push(BtFile { index: files.len() as u32 + 1, path, length: entry.field(b"length")?.nonnegative()? });
        }
    } else {
        if info.optional(b"symlink path")?.is_some() || info.optional(b"attr")?.is_some() { return Err("UNSUPPORTED"); }
        files.push(BtFile { index: 1, path: name.clone(), length: info.field(b"length")?.nonnegative()? });
    }
    let mut paths = HashSet::new();
    let mut total = 0_u64;
    for file in &files {
        let key = file.path.to_lowercase();
        if !paths.insert(key) { return Err("INVALID_ARGUMENT"); }
        total = total.checked_add(file.length).ok_or("UNSUPPORTED")?;
    }
    for path in &paths {
        let parts: Vec<_> = path.split('/').collect();
        for end in 1..parts.len() {
            if paths.contains(&parts[..end].join("/")) { return Err("INVALID_ARGUMENT"); }
        }
    }
    if total == 0 || total.div_ceil(piece_length) != (pieces.len() / 20) as u64 { return Err("INVALID_ARGUMENT"); }
    let info_hash = format!("{:x}", Sha1::digest(&bytes[info.start..info.end]));
    Ok(BtPreparation { source: String::new(), kind: "torrent".into(), name,
        torrent: torrent.into(), info_hash, files })
}

fn component(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "UNSUPPORTED")?;
    if text.is_empty() || text.len() > 255 || text == "." || text == ".."
        || text.ends_with(['.', ' ']) || text.chars().any(|c| c.is_control() || "/\\:<>\"|?*".contains(c)) {
        return Err("INVALID_ARGUMENT");
    }
    let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9')) { return Err("INVALID_ARGUMENT"); }
    Ok(text.into())
}

fn magnet_hash(source: &str) -> Result<String> {
    let url = url::Url::parse(source).map_err(|_| "INVALID_ARGUMENT")?;
    let values = url.query_pairs().filter(|(key, _)| key == "xt")
        .filter_map(|(_, value)| value.get(..9).filter(|prefix| prefix.eq_ignore_ascii_case("urn:btih:")).map(|_| value[9..].to_owned()))
        .collect::<Vec<_>>();
    if values.len() != 1 { return Err("INVALID_ARGUMENT"); }
    let value = &values[0];
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) { return Ok(value.to_ascii_lowercase()); }
    if value.len() != 32 { return Err("INVALID_ARGUMENT"); }
    let mut output = Vec::with_capacity(20);
    let (mut bits, mut buffer) = (0_u32, 0_u32);
    for b in value.bytes() {
        let b = b.to_ascii_uppercase();
        let digit = match b { b'A'..=b'Z' => b - b'A', b'2'..=b'7' => b - b'2' + 26, _ => return Err("INVALID_ARGUMENT") };
        buffer = (buffer << 5) | u32::from(digit);
        bits += 5;
        if bits >= 8 { bits -= 8; output.push((buffer >> bits) as u8); }
    }
    Ok(output.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct Node<'a> { start: usize, end: usize, value: Value<'a> }
enum Value<'a> { Integer(i64), Bytes(&'a [u8]), List(Vec<Node<'a>>), Dict(BTreeMap<&'a [u8], Node<'a>>) }
impl<'a> Node<'a> {
    fn optional(&self, key: &[u8]) -> Result<Option<&Node<'a>>> {
        match &self.value { Value::Dict(values) => Ok(values.get(key)), _ => Err("INVALID_ARGUMENT") }
    }
    fn field(&self, key: &[u8]) -> Result<&Node<'a>> { self.optional(key)?.ok_or("INVALID_ARGUMENT") }
    fn bytes(&self) -> Result<&'a [u8]> { match self.value { Value::Bytes(bytes) => Ok(bytes), _ => Err("INVALID_ARGUMENT") } }
    fn list(&self) -> Result<&[Node<'a>]> { match &self.value { Value::List(values) => Ok(values), _ => Err("INVALID_ARGUMENT") } }
    fn nonnegative(&self) -> Result<u64> { match self.value { Value::Integer(value) if value >= 0 => Ok(value as u64), _ => Err("INVALID_ARGUMENT") } }
    fn positive(&self) -> Result<u64> { let value = self.nonnegative()?; if value == 0 { Err("INVALID_ARGUMENT") } else { Ok(value) } }
}
struct Parser<'a> { bytes: &'a [u8], position: usize, nodes: usize }
impl<'a> Parser<'a> {
    fn node(&mut self, depth: usize) -> Result<Node<'a>> {
        self.nodes += 1;
        if depth > 32 || self.nodes > 10_000 { return Err("UNSUPPORTED"); }
        let start = self.position;
        let value = match self.bytes.get(self.position).copied().ok_or("INVALID_ARGUMENT")? {
            b'i' => {
                self.position += 1;
                let end = self.bytes[self.position..].iter().position(|b| *b == b'e').ok_or("INVALID_ARGUMENT")? + self.position;
                let text = std::str::from_utf8(&self.bytes[self.position..end]).map_err(|_| "INVALID_ARGUMENT")?;
                if text.is_empty() || text.starts_with('+') || text == "-0"
                    || (text.starts_with('0') && text.len() > 1) || text.starts_with("-0") { return Err("INVALID_ARGUMENT"); }
                let value = text.parse().map_err(|_| "INVALID_ARGUMENT")?;
                self.position = end + 1;
                Value::Integer(value)
            },
            b'l' | b'd' => {
                let dictionary = self.bytes[self.position] == b'd';
                self.position += 1;
                let mut list = Vec::new();
                let mut map = BTreeMap::new();
                let mut previous: Option<&[u8]> = None;
                while self.bytes.get(self.position).copied().ok_or("INVALID_ARGUMENT")? != b'e' {
                    if dictionary {
                        let key = self.node(depth + 1)?.bytes()?;
                        if previous.is_some_and(|previous| previous >= key) { return Err("INVALID_ARGUMENT"); }
                        previous = Some(key);
                        map.insert(key, self.node(depth + 1)?);
                    } else { list.push(self.node(depth + 1)?); }
                }
                self.position += 1;
                if dictionary { Value::Dict(map) } else { Value::List(list) }
            },
            b'0'..=b'9' => {
                let colon = self.bytes[self.position..].iter().position(|b| *b == b':').ok_or("INVALID_ARGUMENT")? + self.position;
                let text = std::str::from_utf8(&self.bytes[self.position..colon]).map_err(|_| "INVALID_ARGUMENT")?;
                if text.len() > 8 || (text.starts_with('0') && text.len() > 1) { return Err("INVALID_ARGUMENT"); }
                let length: usize = text.parse().map_err(|_| "INVALID_ARGUMENT")?;
                let end = (colon + 1).checked_add(length).filter(|end| *end <= self.bytes.len()).ok_or("INVALID_ARGUMENT")?;
                self.position = end;
                Value::Bytes(&self.bytes[colon + 1..end])
            },
            _ => return Err("INVALID_ARGUMENT"),
        };
        Ok(Node { start, end: self.position, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn single(name: &str) -> String {
        STANDARD.encode(format!("d4:infod6:lengthi1e4:name{}:{name}12:piece lengthi1e6:pieces20:abcdefghijklmnopqrstee", name.len()))
    }
    #[test]
    fn cli_bt_inspects_exact_metadata_and_rejects_unsafe_components() {
        let parsed = inspect(&single("safe.bin")).unwrap();
        assert_eq!(parsed.files.len(), 1);
        assert_eq!((parsed.files[0].index, parsed.files[0].length), (1, 1));
        assert_eq!(parsed.files[0].path, "safe.bin");
        assert_eq!(parsed.info_hash.len(), 40);
        for name in ["../escape", "..", "C:escape", "CON.txt", "a\\b", "trailing.", "a\0b", "a "] {
            assert!(inspect(&single(name)).is_err(), "accepted {name:?}");
        }
    }
    #[test]
    fn cli_bt_rejects_duplicate_keys_trailing_bytes_and_wrong_piece_counts() {
        for bytes in [
            "d4:infod6:lengthi1e4:name1:a4:name1:b12:piece lengthi1e6:pieces20:abcdefghijklmnopqrstee",
            "d4:infod6:lengthi2e4:name1:a12:piece lengthi1e6:pieces20:abcdefghijklmnopqrstee",
            "d4:infod6:lengthi1e4:name1:a12:piece lengthi1e6:pieces20:abcdefghijklmnopqrsteeextra",
        ] { assert!(inspect(&STANDARD.encode(bytes)).is_err()); }
        let deep = format!("{}{}", "l".repeat(40), "e".repeat(40));
        assert_eq!(inspect(&STANDARD.encode(deep)).unwrap_err(), "UNSUPPORTED");
    }
    #[test]
    fn cli_bt_normalizes_hashes_without_accepting_ambiguous_magnets() {
        assert_eq!(magnet_hash("magnet:?xt=urn:btih:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap(), "0".repeat(40));
        let hash = "0123456789ABCDEF0123456789ABCDEF01234567";
        assert_eq!(magnet_hash(&format!("magnet:?xt=urn:btih:{hash}")).unwrap(), hash.to_lowercase());
        assert!(magnet_hash(&format!("magnet:?xt=urn:btih:{hash}&xt=urn:btih:{hash}")).is_err());
    }
}
