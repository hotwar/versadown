//! The upload pipeline: encryption and MAC against the independent vectors,
//! the pieces actually sent, and the node that gets created.

use super::*;
use crate::cloud::mega::account::manage_vectors as vectors;
use crate::cloud::mega::account::seal::attribute_key;
use crate::cloud::mega::crypto::{
    chunk_ranges, condensed_mac, decode_base64url, decrypt_ecb, decrypt_node_attributes,
    verify_file_mac, FileKey,
};
use crate::cloud::{
    upload_cancelled, CloudUploadTransport, UploadBody, UploadControl, UploadFileSnapshot,
    UploadHttpRequest, UploadHttpResponse,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use zeroize::Zeroizing;

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|i| ((i * 31 + 7) & 0xFF) as u8).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_all(body: &UploadBody) -> Vec<u8> {
    let mut out = Vec::new();
    body.reader(UploadControl::default())
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn ctr_body(snapshot: &UploadFileSnapshot, offset: u64, length: u64) -> UploadBody {
    UploadBody::AesCtr {
        body: Box::new(snapshot.slice(offset, length).unwrap()),
        key: Zeroizing::new(vectors::UPLOAD_AES_KEY),
        nonce: vectors::UPLOAD_NONCE,
        offset,
    }
}

// ------------------------------------------------------ vectors and pieces ---

#[test]
fn pieces_follow_megas_mac_chunk_rule() {
    assert_eq!(chunk_ranges(0).collect::<Vec<_>>(), vec![(0, 0)]);
    assert_eq!(chunk_ranges(1).collect::<Vec<_>>(), vec![(0, 1)]);
    assert_eq!(
        chunk_ranges(100_000).collect::<Vec<_>>(),
        vec![(0, 100_000)]
    );
    let k = 1024u64;
    let pieces: Vec<_> = chunk_ranges(3_000_000).collect();
    let starts: Vec<u64> = pieces.iter().map(|piece| piece.0).collect();
    assert_eq!(
        starts,
        vec![0, 128 * k, 384 * k, 768 * k, 1280 * k, 1920 * k, 2688 * k]
    );
    assert_eq!(pieces.last().unwrap(), &(2_752_512, 247_488));
    // 128 KiB growing by 128 KiB up to 1 MiB, which then repeats; no gaps.
    let big: Vec<_> = chunk_ranges(20 * 1024 * 1024 + 5).collect();
    assert_eq!(big[7].1, 1024 * k);
    assert!(big[8..big.len() - 1]
        .iter()
        .all(|piece| piece.1 == 1024 * k));
    let mut expected = 0;
    for (offset, length) in &big {
        assert_eq!(*offset, expected);
        expected += length;
    }
    assert_eq!(expected, 20 * 1024 * 1024 + 5);
}

#[test]
fn streamed_ciphertext_and_macs_equal_the_independent_vectors() {
    let dir = tempfile::tempdir().unwrap();
    for (length, ciphertext_sha, meta_mac, _, _) in vectors::UPLOAD_CASES {
        let plain = pattern(*length);
        let path = dir.path().join(format!("plain-{length}.bin"));
        std::fs::write(&path, &plain).unwrap();
        let snapshot = UploadFileSnapshot::capture(&path).unwrap();

        // Piece by piece, exactly as they are POSTed.
        let mut ciphertext = Vec::new();
        for (offset, piece) in chunk_ranges(*length as u64) {
            ciphertext.extend(read_all(&ctr_body(&snapshot, offset, piece)));
        }
        assert_eq!(ciphertext.len(), *length);
        assert_eq!(
            &hex(&Sha256::digest(&ciphertext)),
            ciphertext_sha,
            "length {length}"
        );

        let key = FileKey {
            aes_key: Zeroizing::new(vectors::UPLOAD_AES_KEY),
            nonce: vectors::UPLOAD_NONCE,
            meta_mac: [0; 8],
        };
        let mac = condensed_mac(&key, plain.as_slice(), *length as u64).unwrap();
        assert_eq!(mac, *meta_mac, "length {length}");
    }
}

#[test]
fn a_piece_in_the_middle_is_encrypted_at_its_own_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.bin");
    std::fs::write(&path, pattern(3_000_000)).unwrap();
    let snapshot = UploadFileSnapshot::capture(&path).unwrap();
    let piece = read_all(&ctr_body(&snapshot, 393_216, 100_000));
    assert_eq!(
        hex(&Sha256::digest(&piece)),
        vectors::UPLOAD_OFFSET_CT_SHA256
    );
}

// ---------------------------------------------------------------- the flow ---

struct Posted {
    url: String,
    body: Vec<u8>,
}

/// An upload server that answers like MEGA's: nothing for a middle piece and
/// the completion token for the last one.
struct FakeUploadServer {
    total: u64,
    token: &'static str,
    /// Scripted answers for the first posts (status, body), then normal ones.
    scripted: Mutex<Vec<(u16, String)>>,
    posts: Mutex<Vec<Posted>>,
}

impl FakeUploadServer {
    fn new(total: u64) -> Self {
        Self {
            total,
            token: "UPLOADTOKEN-completion_handle0123",
            scripted: Mutex::new(Vec::new()),
            posts: Mutex::new(Vec::new()),
        }
    }

    fn scripted(self, answers: Vec<(u16, &str)>) -> Self {
        *self.scripted.lock().unwrap() = answers
            .into_iter()
            .map(|(status, body)| (status, body.to_owned()))
            .collect();
        self
    }
}

impl CloudUploadTransport for FakeUploadServer {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, QuarkError> {
        assert_eq!(request.method, "POST");
        let body = read_all(&request.body);
        assert_eq!(body.len() as u64, request.body.len());
        let offset: u64 = request.url.rsplit('/').next().unwrap().parse().unwrap();
        let reply = {
            let mut scripted = self.scripted.lock().unwrap();
            if scripted.is_empty() {
                None
            } else {
                Some(scripted.remove(0))
            }
        };
        self.posts.lock().unwrap().push(Posted {
            url: request.url,
            body: body.clone(),
        });
        let (status, text) = reply.unwrap_or_else(|| {
            let last = offset + body.len() as u64 == self.total;
            (
                200,
                if last {
                    self.token.to_owned()
                } else {
                    String::new()
                },
            )
        });
        Ok(UploadHttpResponse {
            status,
            headers: Default::default(),
            body: text,
        })
    }
}

fn upload_server(created: Value) -> FakeMega {
    server(v2_account(), move |command| {
        match command["a"].as_str().unwrap() {
            "f" => ok(serde_json::from_str(LISTING).unwrap()),
            "u" => ok(json!({"p": "https://gfs302n.userstorage.mega.co.nz/ul/UPLOADURL"})),
            "p" => ok(created.clone()),
            _ => code(-2),
        }
    })
}

fn write_file(name: &str, plain: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, plain).unwrap();
    (dir, path)
}

fn decrypt_ciphertext(key: &FileKey, posts: &[Posted]) -> Vec<u8> {
    let transform = crate::cloud::download_transform::DownloadTransform::aes_ctr_path_range(
        *key.aes_key,
        key.nonce,
        0,
    );
    let mut plain = Vec::new();
    for post in posts {
        let offset: u64 = post.url.rsplit('/').next().unwrap().parse().unwrap();
        assert_eq!(
            offset as usize,
            plain.len(),
            "pieces are posted in order without gaps"
        );
        let mut piece = post.body.clone();
        transform.decryptor(offset).apply(&mut piece);
        plain.extend(piece);
    }
    plain
}

#[test]
fn a_multi_piece_upload_posts_ciphertext_and_creates_a_node_with_a_matching_key() {
    let plain = pattern(3_000_000);
    let (_dir, path) = write_file("big é.bin", &plain);
    let http = upload_server(
        json!({"f": [{"h": "NEWFILE1", "p": "ROOTNODE", "t": 0, "s": 3_000_000, "ts": 1700000777}]}),
    );
    let upload = FakeUploadServer::new(plain.len() as u64);
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);

    let created = client.upload_file(&upload, "0", &path).unwrap();
    assert_eq!(created.id, "NEWFILE1");
    assert_eq!(created.parent_id, "0");
    assert_eq!(created.name, "big é.bin");
    assert_eq!(created.kind, CloudFileKind::File);
    assert_eq!(created.size, Some(3_000_000));
    assert_eq!(created.modified_at.as_deref(), Some("1700000777"));

    // `u` asked for the file size; its URL is the base of every POST.
    let asked = http.commands("u");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].command["s"], 3_000_000);
    let posts = upload.posts.lock().unwrap();
    let expected: Vec<(u64, u64)> = chunk_ranges(3_000_000).collect();
    assert_eq!(posts.len(), expected.len());
    for (post, (offset, length)) in posts.iter().zip(&expected) {
        assert_eq!(
            post.url,
            format!("https://gfs302n.userstorage.mega.co.nz/ul/UPLOADURL/{offset}")
        );
        assert_eq!(post.body.len() as u64, *length);
    }

    // The node: token from the last reply, key wrapped with the master key.
    let node = &http.commands("p")[0].command;
    assert_eq!(node["t"], "ROOTNODE");
    let node = &node["n"][0];
    assert_eq!(node["h"], "UPLOADTOKEN-completion_handle0123");
    assert_eq!(node["t"], 0);
    let wrapped = decode_base64url(node["k"].as_str().unwrap()).unwrap();
    let packed = Zeroizing::new(decrypt_ecb(&MASTER_KEY, &wrapped).unwrap());
    let key = FileKey::from_bytes(&packed).unwrap();
    let attributes = decrypt_node_attributes(&key.aes_key, node["a"].as_str().unwrap()).unwrap();
    assert_eq!(attributes["n"], "big é.bin");
    assert_eq!(attribute_key(&packed).unwrap(), key.aes_key);

    // What was posted decrypts to the file, whose MAC is the one in the key.
    assert_eq!(decrypt_ciphertext(&key, &posts), plain);
    assert!(verify_file_mac(&key, plain.as_slice(), plain.len() as u64).unwrap());
    // Nothing identifying the key or plaintext went to the API in the clear.
    let traffic = http.all_traffic();
    assert!(!traffic.contains(&crate::cloud::mega::crypto::encode_base64url(&packed)));
    // The next read must see the new file.
    client.list("0").unwrap();
    assert_eq!(http.commands("f").len(), 2);
}

#[test]
fn a_small_and_an_empty_file_upload_in_one_piece() {
    for length in [0usize, 100_000] {
        let plain = pattern(length);
        let (_dir, path) = write_file("small.bin", &plain);
        let http = upload_server(json!({"f": [{"h": "NEWFILE1", "t": 0}]}));
        let upload = FakeUploadServer::new(length as u64);
        let store = signed_in_store();
        let cache = ListingCache::default();
        let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
        client.upload_file(&upload, "DIRAAAAA", &path).unwrap();
        let posts = upload.posts.lock().unwrap();
        assert_eq!(posts.len(), 1, "length {length}");
        assert!(posts[0].url.ends_with("/UPLOADURL/0"));
        assert_eq!(http.commands("p")[0].command["t"], "DIRAAAAA");
        if length == 0 {
            assert!(posts[0].body.is_empty());
        }
    }
}

#[test]
fn a_busy_server_is_retried_per_piece_without_resending_finished_ones() {
    let plain = pattern(400_000);
    let (_dir, path) = write_file("retry.bin", &plain);
    let http = upload_server(json!({"f": [{"h": "NEWFILE1", "t": 0}]}));
    // Piece 1 ok, piece 2 answers "-3" twice and then goes through.
    let upload = FakeUploadServer::new(plain.len() as u64).scripted(vec![
        (200, ""),
        (200, "-3"),
        (200, "-3"),
    ]);
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    client.upload_file(&upload, "0", &path).unwrap();
    let posts = upload.posts.lock().unwrap();
    let offsets: Vec<&str> = posts
        .iter()
        .map(|post| post.url.rsplit('/').next().unwrap())
        .collect();
    // 400 000 bytes = 128 KiB + 256 KiB + rest: 3 pieces, the 2nd tried 3 times.
    assert_eq!(offsets, vec!["0", "131072", "131072", "131072", "393216"]);
}

#[test]
fn upload_failures_map_to_the_projects_errors_and_never_create_a_node() {
    let plain = pattern(150_000);
    let (_dir, path) = write_file("fail.bin", &plain);
    let cases: [(Vec<(u16, &str)>, &str); 4] = [
        (vec![(200, "-17")], "cloud.error.mega.storageFull"),
        (vec![(200, "-99")], "MEGA 错误码 -99"),
        (vec![(403, "")], "MEGA 上传服务返回 HTTP 403"),
        (
            vec![(200, "-3"), (200, "-3"), (200, "-3"), (200, "-3")],
            "MEGA 上传服务暂时繁忙，请稍后重试",
        ),
    ];
    for (answers, expected) in cases {
        let http = upload_server(json!({"f": [{"h": "NEWFILE1", "t": 0}]}));
        let upload = FakeUploadServer::new(plain.len() as u64).scripted(answers);
        let store = signed_in_store();
        let cache = ListingCache::default();
        let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
        let error = client.upload_file(&upload, "0", &path).unwrap_err();
        let text = error_text(error);
        assert!(
            text == expected || text.ends_with(expected),
            "{text} vs {expected}"
        );
        assert!(http.commands("p").is_empty(), "{expected}");
    }
}

#[test]
fn a_garbage_completion_token_is_not_trusted() {
    let plain = pattern(10);
    let (_dir, path) = write_file("token.bin", &plain);
    for token in ["", "has space", "<html>"] {
        let http = upload_server(json!({"f": [{"h": "NEWFILE1", "t": 0}]}));
        let upload = FakeUploadServer::new(10).scripted(vec![(200, token)]);
        let store = signed_in_store();
        let cache = ListingCache::default();
        let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
        assert!(
            matches!(
                client.upload_file(&upload, "0", &path),
                Err(QuarkError::InvalidResponse)
            ),
            "{token:?}"
        );
        assert!(http.commands("p").is_empty());
    }
}

#[test]
fn storage_that_is_full_is_reported_before_any_byte_is_sent() {
    let (_dir, path) = write_file("full.bin", &pattern(10));
    let http = server(v2_account(), |command| {
        match command["a"].as_str().unwrap() {
            "f" => ok(serde_json::from_str(LISTING).unwrap()),
            "u" => code(-17),
            _ => code(-2),
        }
    });
    let upload = FakeUploadServer::new(10);
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let error = client.upload_file(&upload, "0", &path).unwrap_err();
    assert_eq!(error_text(error), "cloud.error.mega.storageFull");
    assert!(upload.posts.lock().unwrap().is_empty());
}

#[test]
fn a_cancelled_upload_stops_without_creating_a_node() {
    struct CancelAfterFirstPiece(FakeUploadServer);
    impl CloudUploadTransport for CancelAfterFirstPiece {
        fn send_upload(
            &self,
            request: UploadHttpRequest,
        ) -> Result<UploadHttpResponse, QuarkError> {
            if self.0.posts.lock().unwrap().is_empty() {
                return self.0.send_upload(request);
            }
            Err(upload_cancelled())
        }
    }
    let plain = pattern(400_000);
    let (_dir, path) = write_file("cancel.bin", &plain);
    let http = upload_server(json!({"f": [{"h": "NEWFILE1", "t": 0}]}));
    let upload = CancelAfterFirstPiece(FakeUploadServer::new(plain.len() as u64));
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let error = client.upload_file(&upload, "0", &path).unwrap_err();
    assert!(crate::cloud::is_upload_cancelled(&error));
    assert_eq!(upload.0.posts.lock().unwrap().len(), 1);
    assert!(http.commands("p").is_empty());
}

#[test]
fn bad_names_targets_and_files_are_refused_before_talking_to_mega() {
    let http = upload_server(json!({"f": [{"h": "NEWFILE1", "t": 0}]}));
    let upload = FakeUploadServer::new(0);
    let store = signed_in_store();
    let cache = ListingCache::default();
    let client = MegaAccountClient::new(&http, &store).with_cache(&cache);
    let (_dir, path) = write_file("ok.bin", b"x");
    assert!(matches!(
        client.upload_file(&upload, "__versa_down_mega_recycle_bin__", &path),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(matches!(
        client.upload_file(&upload, "FILEBBBB", &path),
        Err(QuarkError::SourceFileMissing)
    ));
    assert!(matches!(
        client.upload_file(&upload, "0", Path::new("/nonexistent/dir/file.bin")),
        Err(QuarkError::InvalidInput(_))
    ));
    assert!(http.commands("u").is_empty());
    assert!(http.commands("p").is_empty());
    assert!(upload.posts.lock().unwrap().is_empty());
}
