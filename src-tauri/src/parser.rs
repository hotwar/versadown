use base64::{engine::general_purpose::STANDARD, Engine as _};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use url::Url;

use crate::cloud::CloudProvider;

#[derive(Clone, Eq, PartialEq)]
pub struct CloudShareCandidate {
    pub provider: CloudProvider,
    pub url: String,
    pub access_code: Option<String>,
}

impl fmt::Debug for CloudShareCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CloudShareCandidate")
            .field("provider", &self.provider)
            .field("url", &"<redacted>")
            .field("access_code", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ShareInputError {
    #[error("一次只能解析一个网盘分享链接")]
    MultipleLinks,
    #[error("访问码格式无效")]
    InvalidAccessCode,
    #[error("网盘分享链接格式无效")]
    InvalidShare,
}

impl CloudShareCandidate {
    fn from_candidate(candidate: Candidate) -> Option<Self> {
        let url = Url::parse(&candidate.uri).ok()?;
        let provider = cloud_provider_for_url(&url)?;
        valid_share_id(&url, provider)?;
        let access_code = access_code_from_query(&url);
        Some(Self {
            provider,
            // Parsing a URL is needed for structural checks only. Serializing
            // it would rewrite a query signature or access code.
            url: candidate.uri,
            access_code,
        })
    }

    fn with_access_code(mut self, access_code: Option<&str>) -> Result<Self, ShareInputError> {
        self.access_code = access_code.map(normalize_access_code).transpose()?;
        Ok(self)
    }
}

pub fn extract_cloud_share_candidate(
    input: &str,
    entered_access_code: Option<&str>,
) -> Result<Option<CloudShareCandidate>, ShareInputError> {
    let candidates = extract_candidate_spans(input);
    let has_invalid_share = candidates.iter().any(|candidate| {
        let Ok(url) = Url::parse(&candidate.source) else {
            return false;
        };
        registered_share_provider(&url).is_some_and(|provider| {
            cloud_provider_for_url(&url) != Some(provider)
                || valid_share_id(&url, provider).is_none()
        })
    });
    let mut values = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(mut value) =
            CloudShareCandidate::from_candidate(classify_input(&candidate.source))
        else {
            continue;
        };
        value.access_code = value.access_code.or_else(|| {
            candidate_text_access_code(input, &candidates, index).map(|(_, code)| code.to_owned())
        });
        if !values.contains(&value) {
            values.push(value);
        }
    }
    match values.as_slice() {
        [] if has_invalid_share => Err(ShareInputError::InvalidShare),
        [] => Ok(None),
        [value] => Ok(Some(value.clone().with_access_code(
            entered_access_code.or(value.access_code.as_deref()),
        )?)),
        _ => Err(ShareInputError::MultipleLinks),
    }
}

fn cloud_provider_for_url(url: &Url) -> Option<CloudProvider> {
    crate::cloud::ProviderRegistry::global()
        .for_share_url(url)
        .map(|descriptor| descriptor.provider)
}

pub(crate) fn registered_share_provider(url: &Url) -> Option<CloudProvider> {
    crate::cloud::ProviderRegistry::global()
        .all()
        .iter()
        .find(|descriptor| {
            matches!(url.scheme(), "http" | "https")
                && url
                    .host_str()
                    .is_some_and(|host| descriptor.share_hosts.contains(&host))
        })
        .map(|descriptor| descriptor.provider)
}

fn valid_share_id(url: &Url, provider: CloudProvider) -> Option<&str> {
    let descriptor = crate::cloud::ProviderRegistry::global().get(provider);
    if !descriptor.accepts_share_url(url)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    descriptor
        .share_patterns
        .iter()
        .find_map(|prefix| url.path().strip_prefix(prefix))
        .filter(|id| {
            !id.is_empty()
                && !id.contains('/')
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
}

fn access_code_from_query(url: &Url) -> Option<String> {
    url.query_pairs()
        .find_map(|(name, value)| is_access_code_parameter(&name).then(|| value.into_owned()))
}

fn is_access_code_parameter(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "pwd" | "passcode")
}

fn access_code_from_text(input: &str) -> Option<(&str, &str)> {
    let pattern = Regex::new(r"(?i)(?:提取码|访问码|access\s*code)\s*[:：]\s*([a-z0-9]+)")
        .expect("static access code pattern");
    let captures = pattern.captures(input)?;
    Some((captures.get(0)?.as_str(), captures.get(1)?.as_str()))
}

fn normalize_access_code(value: &str) -> Result<String, ShareInputError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(ShareInputError::InvalidAccessCode);
    }
    Ok(value.to_string())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    Direct,
    WebVideo,
    Ed2k,
    Ftp,
    Hls,
    Dash,
    Magnet,
    Torrent,
    Thunder,
    CloudShare,
    QuarkShare,
    PikPakShare,
    Text,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub uri: String,
    pub auto_start: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedCandidate {
    pub kind: CandidateKind,
    pub source: String,
    pub auto_start: bool,
}

impl ValidatedCandidate {
    pub fn as_candidate(&self) -> Candidate {
        Candidate {
            kind: self.kind.clone(),
            uri: self.source.clone(),
            auto_start: self.auto_start,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InputValidationError {
    #[error("未发现受支持的有效下载链接")]
    Unsupported,
    #[error("下载链接格式无效")]
    InvalidUrl,
    #[error("磁链格式无效")]
    InvalidMagnet,
    #[error("ED2K 链接格式无效")]
    InvalidEd2k,
    #[error("FTP 链接格式无效")]
    InvalidFtp,
    #[error("迅雷链接格式无效")]
    InvalidThunder,
    #[error("网盘分享链接格式无效")]
    InvalidCloudShare,
    #[error("种子文件路径格式无效")]
    InvalidTorrent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedInput {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_probe_video: bool,
    pub input: String,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<CandidateKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ScannedInput {
    fn valid(candidate: ValidatedCandidate) -> Self {
        Self {
            can_probe_video: false,
            input: candidate.source,
            valid: true,
            kind: Some(candidate.kind),
            message: None,
        }
    }

    fn invalid(input: String, error: InputValidationError) -> Self {
        Self {
            can_probe_video: false,
            input,
            valid: false,
            kind: None,
            message: Some(error.to_string()),
        }
    }
}

pub fn classify_input(input: &str) -> Candidate {
    let uri = input.trim().to_string();
    validate_download_input(&uri)
        .map(|candidate| candidate.as_candidate())
        .unwrap_or(Candidate {
            kind: CandidateKind::Text,
            uri,
            auto_start: false,
        })
}

pub fn extract_candidates(text: &str) -> Vec<Candidate> {
    extract_raw_candidates(text)
        .into_iter()
        .map(|value| classify_input(&value))
        .collect()
}

pub fn scan_download_inputs(text: &str) -> Vec<ScannedInput> {
    let values = extract_candidate_spans(text);
    if values.is_empty() {
        let input = text.trim().to_string();
        return match validate_download_input(&input) {
            Ok(candidate) => vec![ScannedInput::valid(candidate)],
            Err(error) => vec![ScannedInput::invalid(input, error)],
        };
    }
    values
        .iter()
        .enumerate()
        .map(
            |(index, value)| match validate_download_input(&value.source) {
                Ok(candidate) => {
                    let mut scanned = ScannedInput::valid(candidate);
                    // Batch preparation consumes only `input`, so preserve a code
                    // from this share's copy text instead of dropping it here.
                    if let Some(share) =
                        CloudShareCandidate::from_candidate(classify_input(&value.source))
                    {
                        if share.access_code.is_none() {
                            if let Some((copy_text, _)) =
                                candidate_text_access_code(text, &values, index)
                            {
                                scanned.input.push(' ');
                                scanned.input.push_str(copy_text);
                            }
                        }
                    }
                    scanned
                }
                Err(error) => ScannedInput::invalid(value.source.clone(), error),
            },
        )
        .collect()
}

pub fn validate_download_input(input: &str) -> Result<ValidatedCandidate, InputValidationError> {
    let source = input.trim();
    if source.is_empty() {
        return Err(InputValidationError::Unsupported);
    }
    let lower = source.to_ascii_lowercase();
    if lower.starts_with("magnet:") {
        validate_magnet(source)?;
        return Ok(validated(CandidateKind::Magnet, source.to_string()));
    }
    if lower.starts_with("ed2k:") {
        if !valid_ed2k_file_link(source) {
            return Err(InputValidationError::InvalidEd2k);
        }
        return Ok(validated(CandidateKind::Ed2k, source.to_string()));
    }
    if lower.starts_with("thunder://") {
        let target = decode_thunder_target(source)?;
        let target_kind = validate_download_input(&target)?.kind;
        if matches!(
            target_kind,
            CandidateKind::Torrent
                | CandidateKind::Thunder
                | CandidateKind::WebVideo
                | CandidateKind::CloudShare
                | CandidateKind::QuarkShare
                | CandidateKind::PikPakShare
        ) {
            return Err(InputValidationError::InvalidThunder);
        }
        return Ok(validated(CandidateKind::Thunder, source.to_string()));
    }
    if lower.starts_with("ftp:") {
        return validate_network_url(source, "ftp")
            .map(|source| validated(CandidateKind::Ftp, source));
    }
    if lower.starts_with("http:") || lower.starts_with("https:") {
        let url = Url::parse(source).map_err(|_| InputValidationError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(InputValidationError::InvalidUrl);
        }
        if registered_share_provider(&url).is_some() {
            let provider =
                cloud_provider_for_url(&url).ok_or(InputValidationError::InvalidCloudShare)?;
            valid_share_id(&url, provider).ok_or(InputValidationError::InvalidCloudShare)?;
            let kind = match provider {
                CloudProvider::Quark => CandidateKind::QuarkShare,
                CloudProvider::Pikpak => CandidateKind::PikPakShare,
                _ => CandidateKind::CloudShare,
            };
            return Ok(validated(kind, source.to_string()));
        }
        if let Some(page) = crate::web_video::playback_url(&url) {
            return Ok(validated(CandidateKind::WebVideo, page.url.into()));
        }
        let normalized = normalize_queryless_unicode_path(source, &url);
        let path = url.path().to_ascii_lowercase();
        // Remote URLs, including URLs ending in `.torrent`, are ordinary
        // downloads. Only a local torrent file explicitly provided to the app
        // enters the BT metadata/selection flow.
        let kind = if path.ends_with(".m3u8") {
            CandidateKind::Hls
        } else if path.ends_with(".mpd") {
            CandidateKind::Dash
        } else {
            CandidateKind::Direct
        };
        return Ok(validated(kind, normalized));
    }
    if lower.ends_with(".torrent") {
        validate_local_torrent(source)?;
        return Ok(validated(CandidateKind::Torrent, source.to_string()));
    }
    Err(InputValidationError::Unsupported)
}

pub fn validate_task_source(kind: &str, source: &str) -> Result<(), InputValidationError> {
    if kind == "cloud" {
        let valid = if source.starts_with("cloud-share://") {
            crate::cloud::share::parse_stable_cloud_share_source(source).is_ok()
        } else {
            crate::cloud::parse_stable_cloud_source(source).is_ok()
        };
        return valid
            .then_some(())
            .ok_or(InputValidationError::InvalidCloudShare);
    }
    let candidate = validate_download_input(source)?;
    let valid_kind = match kind {
        "file" => candidate.kind == CandidateKind::Direct,
        "web_video" => candidate.kind == CandidateKind::WebVideo
            || (candidate.kind == CandidateKind::Direct
                && Url::parse(source).ok().as_ref().and_then(crate::web_video::manual_page).is_some()),
        "ed2k" => candidate.kind == CandidateKind::Ed2k,
        "ftp" => candidate.kind == CandidateKind::Ftp,
        "hls" => candidate.kind == CandidateKind::Hls,
        "dash" => candidate.kind == CandidateKind::Dash,
        "magnet" => candidate.kind == CandidateKind::Magnet,
        "torrent" => candidate.kind == CandidateKind::Torrent,
        _ => false,
    };
    valid_kind
        .then_some(())
        .ok_or(InputValidationError::Unsupported)
}

fn validated(kind: CandidateKind, source: String) -> ValidatedCandidate {
    let auto_start = matches!(
        kind,
        CandidateKind::Direct
            | CandidateKind::Ed2k
            | CandidateKind::Ftp
            | CandidateKind::Hls
            | CandidateKind::Dash
            | CandidateKind::Thunder
    );
    ValidatedCandidate {
        kind,
        source,
        auto_start,
    }
}

fn extract_raw_candidates(text: &str) -> Vec<String> {
    extract_candidate_spans(text)
        .into_iter()
        .map(|value| value.source)
        .collect()
}

struct CandidateSpan {
    source: String,
    start: usize,
    end: usize,
}

fn candidate_text_access_code<'a>(
    text: &'a str,
    candidates: &[CandidateSpan],
    index: usize,
) -> Option<(&'a str, &'a str)> {
    let start = candidates[index].end;
    let end = candidates
        .get(index + 1)
        .map_or(text.len(), |value| value.start);
    access_code_from_text(&text[start..end]).or_else(|| {
        // A single share also supports copy text putting its code before the
        // URL. With several links only their trailing text may supply a code.
        (candidates.len() == 1)
            .then(|| access_code_from_text(text))
            .flatten()
    })
}

fn extract_candidate_spans(text: &str) -> Vec<CandidateSpan> {
    let pattern = Regex::new(
        r#"(?i)(ed2k:[^\s<>；]*?\|/|magnet:[^\s<>；]*|thunder://[^\s<>；]*|ftp:[^\s<>；]+|https?://[^\s<>；]+|file://[^\s<>；]+|"/[^"\r\n]+\.torrent"|'/[^'\r\n]+\.torrent'|[a-z]:\\[^\r\n<>]*?\.torrent|/[^\s<>；"']+\.torrent)"#,
    )
    .expect("static candidate pattern");
    let masked = mask_markdown_link_labels(text);
    let mut values = Vec::new();
    let mut offset = 0;
    for line in masked.split_inclusive('\n') {
        if let Some(paths) = local_torrent_paths_in_line(line) {
            for source in paths {
                let start = offset + line.find(&source).unwrap_or(0);
                values.push(CandidateSpan {
                    end: start + source.len(),
                    source,
                    start,
                });
            }
        } else {
            for found in pattern
                .find_iter(line)
                .filter(|value| !is_url_like_path_start(line, value))
            {
                let raw = found.as_str();
                let quoted = raw.starts_with("\"/") || raw.starts_with("'/");
                let start = offset + found.start() + usize::from(quoted);
                let candidate = if quoted { &raw[1..raw.len() - 1] } else { raw };
                let source = trim_extracted_candidate(candidate);
                values.push(CandidateSpan {
                    end: start + source.len(),
                    source,
                    start,
                });
            }
        }
        offset += line.len();
    }
    values
}

/// Hide Markdown labels and delimiters while keeping byte offsets and the
/// destination URL intact. A URL used as both label and target is one link.
fn mask_markdown_link_labels(text: &str) -> String {
    let mut masked = text.as_bytes().to_vec();
    for (separator, _) in text.match_indices("](") {
        let Some(label_start) = text[..separator].rfind('[') else {
            continue;
        };
        if text[label_start..separator].contains(['\r', '\n']) {
            continue;
        }
        let token_start = text[..label_start]
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        if text[token_start..label_start].contains("://") {
            // Brackets inside a raw URL's query are data, not Markdown.
            continue;
        }
        let target_start = separator + 2;
        let mut depth = 1;
        let mut target_end = None;
        for (index, character) in text[target_start..].char_indices() {
            match character {
                '\r' | '\n' => break,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        target_end = Some(target_start + index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(target_end) = target_end else {
            continue;
        };
        let target = &text[target_start..target_end];
        if target.chars().any(char::is_whitespace)
            || !Url::parse(target).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        {
            continue;
        }
        masked[label_start..target_start].fill(b' ');
        masked[target_end] = b' ';
    }
    String::from_utf8(masked).expect("only complete Markdown labels were masked")
}

fn ends_with_torrent(value: &str) -> bool {
    value.to_ascii_lowercase().ends_with(".torrent")
}

/// A `/` directly after a host-like character (`example.com/x.torrent`) is a
/// URL path, not the start of a local absolute path.
fn is_url_like_path_start(line: &str, match_: &regex::Match<'_>) -> bool {
    match_.as_str().starts_with('/')
        && line[..match_.start()]
            .chars()
            .next_back()
            .is_some_and(|previous| previous.is_ascii_alphanumeric() || "-.:/".contains(previous))
}

/// Splits a line of absolute unix paths at each whitespace-preceded `/`.
fn split_at_path_starts(line: &str) -> Vec<&str> {
    let starts: Vec<usize> = line
        .char_indices()
        .filter(|(index, character)| {
            *character == '/'
                && line[..*index]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(index, _)| index)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(position, start)| {
            let end = starts.get(position + 1).copied().unwrap_or(line.len());
            line[*start..end].trim()
        })
        .collect()
}

/// Native drops and staged torrents are absolute paths that may contain
/// spaces. A whole line that exists, or has no second path start, is a single
/// candidate; several `/...torrent` fragments are split. `None` defers to the
/// mixed-text scanner.
fn local_torrent_paths_in_line(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    let source = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    let absolute = source.starts_with('/') || std::path::Path::new(source).is_absolute();
    if !absolute || !ends_with_torrent(source) {
        return None;
    }
    if std::path::Path::new(source).exists() || !source.starts_with('/') {
        return Some(vec![source.to_string()]);
    }
    let parts = split_at_path_starts(source);
    match parts.as_slice() {
        [_] => Some(vec![source.to_string()]),
        _ if parts.iter().all(|part| ends_with_torrent(part)) => {
            Some(parts.iter().map(|part| part.to_string()).collect())
        }
        _ => None,
    }
}

fn trim_extracted_candidate(value: &str) -> String {
    // Cloud share IDs are ASCII. Some official copy text places the access
    // code label directly after the ID without whitespace (notably UC).
    // Only split a structurally valid, queryless share prefix; never rewrite
    // an arbitrary URL's signed query or a Unicode file path.
    let marker = Regex::new(r"(?i)(?:提取码|访问码|access\s*code)\s*[:：]")
        .expect("static access code label pattern");
    let value = marker
        .find(value)
        .and_then(|marker| {
            let prefix = &value[..marker.start()];
            let url = Url::parse(prefix).ok()?;
            let provider = cloud_provider_for_url(&url)?;
            (url.query().is_none()
                && url.fragment().is_none()
                && valid_share_id(&url, provider).is_some())
            .then_some(prefix)
        })
        .unwrap_or(value);
    let has_signed_component = Url::parse(value).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https" | "ftp")
            && (url.query().is_some() || url.fragment().is_some())
    });
    if has_signed_component {
        value.to_string()
    } else {
        value
            .trim_end_matches(|character: char| ".,;；，。)】]}".contains(character))
            .to_string()
    }
}

fn validate_magnet(source: &str) -> Result<(), InputValidationError> {
    let query = source
        .get(..8)
        .filter(|prefix| prefix.eq_ignore_ascii_case("magnet:?"))
        .and_then(|_| source.get(8..))
        .ok_or(InputValidationError::InvalidMagnet)?;
    let valid = query.split('&').any(|part| {
        let Some((name, value)) = part.split_once('=') else {
            return false;
        };
        name.eq_ignore_ascii_case("xt")
            && value
                .get(..9)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("urn:btih:"))
            && value.get(9..).is_some_and(|hash| {
                (hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    || (hash.len() == 32
                        && hash.bytes().all(|byte| {
                            byte.is_ascii_alphabetic() || (b'2'..=b'7').contains(&byte)
                        }))
            })
    });
    valid
        .then_some(())
        .ok_or(InputValidationError::InvalidMagnet)
}

pub(crate) fn decode_thunder_target(source: &str) -> Result<String, InputValidationError> {
    let encoded = source
        .get("thunder://".len()..)
        .ok_or(InputValidationError::InvalidThunder)?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| InputValidationError::InvalidThunder)?;
    let decoded = String::from_utf8(decoded).map_err(|_| InputValidationError::InvalidThunder)?;
    decoded
        .strip_prefix("AA")
        .and_then(|value| value.strip_suffix("ZZ"))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(InputValidationError::InvalidThunder)
}

fn validate_network_url(source: &str, scheme: &str) -> Result<String, InputValidationError> {
    if scheme == "ftp" {
        crate::parse_ftp_url(source).map_err(|_| InputValidationError::InvalidFtp)?;
    }
    let url = Url::parse(source).map_err(|_| InputValidationError::InvalidFtp)?;
    if url.scheme() != scheme || url.host_str().is_none() {
        return Err(InputValidationError::InvalidFtp);
    }
    Ok(normalize_queryless_unicode_path(source, &url))
}

fn normalize_queryless_unicode_path(source: &str, url: &Url) -> String {
    if url.query().is_some() || url.fragment().is_some() {
        return source.to_string();
    }
    let Some(authority_start) = source.find("://").map(|index| index + 3) else {
        return source.to_string();
    };
    let authority_end = source[authority_start..]
        .find('/')
        .map(|index| authority_start + index)
        .unwrap_or(source.len());
    if !source[..authority_end].is_ascii() || source[authority_end..].is_ascii() {
        return source.to_string();
    }
    url.to_string()
}

fn validate_local_torrent(source: &str) -> Result<(), InputValidationError> {
    let path = if source.to_ascii_lowercase().starts_with("file:") {
        Url::parse(source)
            .ok()
            .and_then(|url| url.to_file_path().ok())
    } else {
        Some(std::path::PathBuf::from(source))
    }
    .ok_or(InputValidationError::InvalidTorrent)?;
    path.is_absolute()
        .then_some(())
        .ok_or(InputValidationError::InvalidTorrent)
}

fn valid_ed2k_file_link(value: &str) -> bool {
    let parts = value.split('|').collect::<Vec<_>>();
    parts.len() >= 6
        && parts[0].eq_ignore_ascii_case("ed2k://")
        && parts[1].eq_ignore_ascii_case("file")
        && !parts[2].trim().is_empty()
        && parts[3].parse::<u64>().is_ok()
        && parts[4].len() == 32
        && parts[4].bytes().all(|byte| byte.is_ascii_hexdigit())
        && parts.last() == Some(&"/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::CloudProvider;

    #[test]
    fn cloud_share_candidate_debug_keeps_provider_but_redacts_url_and_access_code() {
        let candidate = CloudShareCandidate {
            provider: CloudProvider::Baidu,
            url: "https://pan.baidu.com/s/private-share-secret".into(),
            access_code: Some("FAKE-CODE-SECRET".into()),
        };

        assert_eq!(candidate.clone(), candidate);
        let debug = format!("{candidate:?}");
        assert!(debug.contains("Baidu"));
        for secret in ["private-share-secret", "FAKE-CODE-SECRET", "pan.baidu.com"] {
            assert!(
                !debug.contains(secret),
                "debug output leaked {secret}: {debug}"
            );
        }
    }

    #[test]
    fn recognizes_all_provider_hosts_without_rewriting_the_access_code_query() {
        for (input, provider) in [
            ("https://pan.quark.cn/s/a?pwd=1234", CloudProvider::Quark),
            (
                "https://mypikpak.com/s/a 提取码: 1234",
                CloudProvider::Pikpak,
            ),
            ("https://pan.baidu.com/s/a?pwd=1234", CloudProvider::Baidu),
            ("https://www.alipan.com/s/a?pwd=1234", CloudProvider::Aliyun),
            ("https://pan.xunlei.com/s/a?pwd=1234", CloudProvider::Xunlei),
            ("https://drive.uc.cn/s/a?pwd=1234", CloudProvider::Uc),
        ] {
            let value = extract_cloud_share_candidate(input, None).unwrap().unwrap();
            assert_eq!(value.provider, provider);
            assert_eq!(value.access_code.as_deref(), Some("1234"));
            assert_eq!(value.url, input.split_whitespace().next().unwrap());
        }
    }

    fn share_copy_samples() -> Vec<(
        &'static str,
        CloudProvider,
        &'static str,
        Option<&'static str>,
    )> {
        vec![
            ("通过百度网盘分享的文件：video.mp4\n链接：https://pan.baidu.com/s/1sample_Baidu?pwd=6164\n【提供下载、保存到网盘】", CloudProvider::Baidu, "https://pan.baidu.com/s/1sample_Baidu?pwd=6164", Some("6164")),
            ("迅雷网盘\nhttps://pan.xunlei.com/s/sample_XunleiOne\n提取码：5zpw 复制这段内容后打开 App", CloudProvider::Xunlei, "https://pan.xunlei.com/s/sample_XunleiOne", Some("5zpw")),
            ("链接：https://pan.xunlei.com/s/sample_XunleiTwo?pwd=xwh8# 复制这段内容后打开 App", CloudProvider::Xunlei, "https://pan.xunlei.com/s/sample_XunleiTwo?pwd=xwh8#", Some("xwh8")),
            ("阿里\n图书.pdf\nhttps://www.alipan.com/s/sampleAli\n【仅支持保存到网盘】", CloudProvider::Aliyun, "https://www.alipan.com/s/sampleAli", None),
            ("来自UC网盘分享文件：视频。链接：https://drive.uc.cn/s/sampleUcOne提取码：LycE\n【提供下载、保存到网盘】", CloudProvider::Uc, "https://drive.uc.cn/s/sampleUcOne", Some("LycE")),
            ("来自UC网盘分享文件：不要密码\n链接：https://drive.uc.cn/s/sampleUcTwo?public=1", CloudProvider::Uc, "https://drive.uc.cn/s/sampleUcTwo?public=1", None),
            ("我用夸克网盘给你分享了文件。\n链接：https://pan.quark.cn/s/sampleQuarkOne\n提取码：pDcd", CloudProvider::Quark, "https://pan.quark.cn/s/sampleQuarkOne", Some("pDcd")),
            ("我用夸克网盘给你分享了文件。\n链接：https://pan.quark.cn/s/sampleQuarkTwo", CloudProvider::Quark, "https://pan.quark.cn/s/sampleQuarkTwo", None),
        ]
    }

    #[test]
    fn share_copy_samples_round_trip_through_batch_scan_without_losing_codes() {
        let samples = share_copy_samples();
        let input = samples
            .iter()
            .map(|sample| sample.0)
            .collect::<Vec<_>>()
            .join("\n\n");
        let scanned = scan_download_inputs(&input);
        assert_eq!(scanned.len(), samples.len());
        for (item, (original, provider, url, code)) in scanned.iter().zip(samples) {
            assert!(item.valid, "{original}");
            for input in [original, item.input.as_str()] {
                let candidate = extract_cloud_share_candidate(input, None).unwrap().unwrap();
                assert_eq!(candidate.provider, provider);
                assert_eq!(candidate.url, url);
                assert_eq!(candidate.access_code.as_deref(), code);
            }
        }
    }

    #[test]
    fn markdown_share_link_uses_destination_once_and_preserves_query_bytes() {
        let url = "https://pan.baidu.com/s/sample?pwd=%36%31%36%34&signature=a%2Fb+z.";
        let input = format!("链接：[{url}]({url})");
        let scanned = scan_download_inputs(&input);
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].valid);
        let value = extract_cloud_share_candidate(&input, None)
            .unwrap()
            .unwrap();
        assert_eq!(value.url, url);
        assert_eq!(value.access_code.as_deref(), Some("6164"));
        assert_eq!(extract_candidates(&input)[0].uri, url);
    }

    #[test]
    fn markdown_batch_keeps_distinct_shares_and_their_own_access_codes() {
        let input = "[https://pan.quark.cn/s/one](https://pan.quark.cn/s/one) 提取码：a1B2\n[第二项](https://drive.uc.cn/s/two) 提取码：c3D4";
        let scanned = scan_download_inputs(input);
        assert_eq!(scanned.len(), 2);
        for (item, code) in scanned.iter().zip(["a1B2", "c3D4"]) {
            assert!(item.valid);
            let value = extract_cloud_share_candidate(&item.input, None)
                .unwrap()
                .unwrap();
            assert_eq!(value.access_code.as_deref(), Some(code));
        }
        assert_eq!(
            extract_cloud_share_candidate(input, None),
            Err(ShareInputError::MultipleLinks)
        );
    }

    #[test]
    fn sharing_text_code_never_leaks_into_a_preceding_share() {
        let input = "https://www.alipan.com/s/noCode\nhttps://pan.quark.cn/s/withCode\n提取码：Ab12\nhttps://drive.uc.cn/s/noCode?public=1";
        let scanned = scan_download_inputs(input);
        let codes: Vec<_> = scanned
            .iter()
            .map(|item| {
                extract_cloud_share_candidate(&item.input, None)
                    .unwrap()
                    .unwrap()
                    .access_code
            })
            .collect();
        assert_eq!(codes, [None, Some("Ab12".into()), None]);
    }

    #[test]
    fn scan_preserves_the_copied_access_code_label() {
        for label in ["访问码: aB12", "access code: aB12"] {
            let input = format!("{label}\nhttps://pan.baidu.com/s/example");
            let scanned = scan_download_inputs(&input);
            assert_eq!(
                scanned[0].input,
                format!("https://pan.baidu.com/s/example {label}")
            );
            assert_eq!(
                extract_cloud_share_candidate(&scanned[0].input, None)
                    .unwrap()
                    .unwrap()
                    .access_code
                    .as_deref(),
                Some("aB12")
            );
        }
    }

    #[test]
    fn scan_does_not_interpret_a_signed_query_as_markdown() {
        let url =
            "https://example.test/a.zip?signature=[label](https://example.test/value)&other=abc'";
        let scanned = scan_download_inputs(url);
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].valid);
        assert_eq!(scanned[0].input, url);
    }

    #[test]
    fn query_access_code_is_decoded_once_and_explicit_entry_takes_precedence() {
        let input =
            "https://pan.baidu.com/s/sample?pwd=%36%31%36%34&signature=%2F+abc 提取码：other";
        let candidate = extract_cloud_share_candidate(input, None).unwrap().unwrap();
        assert_eq!(candidate.access_code.as_deref(), Some("6164"));
        assert_eq!(candidate.url, input.split_whitespace().next().unwrap());
        assert_eq!(
            extract_cloud_share_candidate(input, Some("Edit12"))
                .unwrap()
                .unwrap()
                .access_code
                .as_deref(),
            Some("Edit12")
        );
        for encoded in ["%2536", "%0d%0a", "%2B", "%FF"] {
            assert_eq!(
                extract_cloud_share_candidate(
                    &format!("https://pan.baidu.com/s/sample?pwd={encoded}"),
                    None
                ),
                Err(ShareInputError::InvalidAccessCode)
            );
        }
    }

    #[test]
    fn rejects_decommissioned_tianyi_share_links_instead_of_treating_them_as_direct_urls() {
        let source = "https://cloud.189.cn/t/retired-share?pwd=1234";

        assert_eq!(
            validate_download_input(source),
            Err(InputValidationError::InvalidCloudShare)
        );
        assert_eq!(
            extract_cloud_share_candidate(source, None),
            Err(ShareInputError::InvalidShare)
        );
        assert!(crate::cloud::ProviderRegistry::global()
            .catalog()
            .iter()
            .all(|descriptor| descriptor.provider != CloudProvider::Tianyi));
    }

    #[test]
    fn rejects_multiple_share_urls() {
        assert!(matches!(
            extract_cloud_share_candidate(
                "https://pan.baidu.com/s/a https://pan.xunlei.com/s/b",
                None
            ),
            Err(ShareInputError::MultipleLinks)
        ));
    }

    #[test]
    fn classifies_a_short_btih_as_text_instead_of_a_magnet() {
        assert_eq!(
            classify_input("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef0123").kind,
            CandidateKind::Text
        );
    }

    #[test]
    fn rejects_a_registered_share_host_without_a_share_id() {
        assert!(matches!(
            extract_cloud_share_candidate("https://pan.baidu.com/s/", None),
            Err(ShareInputError::InvalidShare)
        ));
    }

    #[test]
    fn never_treats_a_registered_cloud_share_as_a_direct_file_url() {
        let source = "https://pan.baidu.com/s/share_123?pwd=preserved";

        assert_eq!(
            validate_download_input(source).unwrap().kind,
            CandidateKind::CloudShare
        );
        assert!(matches!(
            validate_task_source("file", source),
            Err(InputValidationError::Unsupported)
        ));
    }

    #[test]
    fn scan_keeps_invalid_candidates_in_order_without_creating_a_fallback_link() {
        let values =
            scan_download_inputs("magnet:?xt=urn:btih:short https://example.test/release.iso");

        assert_eq!(values.len(), 2);
        assert_eq!(values[0].message.as_deref(), Some("磁链格式无效"));
        assert!(!values[0].valid);
        assert!(values[1].valid);
        assert_eq!(values[1].kind, Some(CandidateKind::Direct));
    }

    #[test]
    fn scan_recognizes_a_local_torrent_inside_mixed_text() {
        #[cfg(windows)]
        let values = scan_download_inputs(r"复制 C:\Downloads\release.torrent 完成");
        #[cfg(not(windows))]
        let values = scan_download_inputs("复制 /Users/example/Downloads/release.torrent 完成");

        assert_eq!(values.len(), 1);
        assert!(values[0].valid);
        assert_eq!(values[0].kind, Some(CandidateKind::Torrent));
        #[cfg(windows)]
        assert_eq!(values[0].input, r"C:\Downloads\release.torrent");
        #[cfg(not(windows))]
        assert_eq!(values[0].input, "/Users/example/Downloads/release.torrent");
    }

    #[test]
    #[cfg(unix)]
    fn scan_preserves_quoted_local_torrent_paths_with_spaces_and_unicode() {
        let values =
            scan_download_inputs("下载 \"/Users/example/下载文件/My release.torrent\" 完成");
        assert_eq!(values.len(), 1);
        assert!(values[0].valid);
        assert_eq!(
            values[0].input,
            "/Users/example/下载文件/My release.torrent"
        );
    }

    #[test]
    fn local_path_quote_handling_does_not_strip_a_signed_url_apostrophe() {
        let source = "https://example.test/archive.zip?signature=abc'";
        let values = scan_download_inputs(source);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].input, source);
    }

    #[test]
    #[cfg(unix)]
    fn scan_preserves_unquoted_native_and_staged_paths_with_spaces() {
        let input = "/Users/test/Library/Application Support/com.versadown.desktop/dropped-torrents/id/My 文件.torrent\n/Users/test/下载/Other file.torrent";
        let values = scan_download_inputs(input);
        assert_eq!(values.len(), 2);
        for (value, line) in values.iter().zip(input.lines()) {
            assert!(value.valid);
            assert_eq!(value.input, line);
        }
    }

    #[test]
    #[cfg(unix)]
    fn scan_splits_multiple_absolute_torrent_paths_on_one_line() {
        let values = scan_download_inputs("/Users/me/a.torrent /Users/me/b.torrent");
        let inputs: Vec<_> = values.iter().map(|value| value.input.as_str()).collect();
        assert_eq!(inputs, ["/Users/me/a.torrent", "/Users/me/b.torrent"]);
        assert!(values.iter().all(|value| value.valid));
    }

    #[test]
    #[cfg(unix)]
    fn scan_splits_paths_with_spaces_and_unicode_on_one_line() {
        let values = scan_download_inputs("/Users/me/测试 file.torrent /Users/me/My dir/b.torrent");
        let inputs: Vec<_> = values.iter().map(|value| value.input.as_str()).collect();
        assert_eq!(
            inputs,
            ["/Users/me/测试 file.torrent", "/Users/me/My dir/b.torrent"]
        );
    }

    #[test]
    #[cfg(unix)]
    fn scan_keeps_a_single_path_with_spaces_and_chinese_whole() {
        let line = "/Users/me/Application Support/测试 file.torrent";
        let values = scan_download_inputs(line);
        assert_eq!(values.len(), 1);
        assert!(values[0].valid);
        assert_eq!(values[0].input, line);
    }

    #[test]
    fn scan_ignores_url_like_text_ending_in_torrent() {
        let values = scan_download_inputs("see example.com/files/x.torrent");
        assert!(values.iter().all(|value| value.input != "/files/x.torrent"));
        let values = scan_download_inputs("see sub-host.example.org/a/b/x.torrent here");
        assert!(values.iter().all(|value| !value.input.starts_with('/')));
    }

    #[test]
    fn scan_keeps_windows_torrent_paths_with_spaces() {
        let values = scan_download_inputs(r"C:\Users\x\a b.torrent");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].input, r"C:\Users\x\a b.torrent");
    }

    #[test]
    fn scan_leaves_magnet_and_http_links_unaffected_in_multiline_input() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";
        let input =
            format!("{magnet}\nhttps://example.test/dir/x.torrent\nhttps://example.test/a.zip");
        let values = scan_download_inputs(&input);
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].input, magnet);
        assert_eq!(values[1].input, "https://example.test/dir/x.torrent");
        assert!(values.iter().all(|value| value.valid));
    }

    #[test]
    #[cfg(unix)]
    fn scan_handles_multiline_mix_of_paths() {
        let values = scan_download_inputs("/a/x.torrent /b/y.torrent\n/c/with space.torrent");
        let inputs: Vec<_> = values.iter().map(|value| value.input.as_str()).collect();
        assert_eq!(
            inputs,
            ["/a/x.torrent", "/b/y.torrent", "/c/with space.torrent"]
        );
    }

    #[test]
    fn treats_remote_torrent_urls_as_ordinary_downloads() {
        assert_eq!(
            validate_download_input("https://example.test/release.torrent?token=temporary")
                .unwrap()
                .kind,
            CandidateKind::Direct
        );
        assert_eq!(
            validate_download_input("ftp://example.test/release.torrent")
                .unwrap()
                .kind,
            CandidateKind::Ftp
        );
    }

    #[test]
    fn scan_never_strips_a_signed_url_query_value() {
        let input = "https://example.test/release.iso?signature=ends-in-a-period.";
        let values = scan_download_inputs(input);

        assert_eq!(values[0].input, input);
        assert!(values[0].valid);
    }

    #[test]
    fn only_normalizes_a_queryless_unicode_url_path() {
        assert_eq!(
            validate_download_input("https://example.test/目录/file.iso")
                .unwrap()
                .source,
            "https://example.test/%E7%9B%AE%E5%BD%95/file.iso"
        );
        assert_eq!(
            validate_download_input("https://example.test/目录/file.iso?signature=未改写")
                .unwrap()
                .source,
            "https://example.test/目录/file.iso?signature=未改写"
        );
    }
}
