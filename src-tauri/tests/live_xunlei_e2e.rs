//! Explicitly enabled destructive-in-scope live regression for the saved Xunlei session.
//!
//! The test only creates and operates on one uniquely named
//! `VersaDown-E2E-xunlei-*` root. Cleanup moves that root to the recycle
//! bin and permanently deletes it. Existing user objects are never selected.

use versadown_lib::cloud::{
    commands::{plan_cloud_directory_download, CloudState},
    CloudError, CloudFile, CloudFileKind, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport,
    CloudProvider, CloudUploadTransport, ProviderRegistry, ReqwestTransport, ResolvedCloudDownload,
    UploadHttpRequest, UploadHttpResponse,
};
use rand::RngCore;
use std::{
    collections::BTreeMap,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type LiveResult<T> = Result<T, Box<dyn Error>>;

struct AuditedTransport(ReqwestTransport);

impl AuditedTransport {
    fn new() -> Result<Self, CloudError> {
        Ok(Self(ReqwestTransport::new()?))
    }
}

fn safe_host(url: &str) -> &'static str {
    if url.contains("api-pan.xunlei.com") {
        "api-pan.xunlei.com"
    } else if url.contains("xluser-ssl.xunlei.com") {
        "xluser-ssl.xunlei.com"
    } else if url.contains("aliyuncs.com") {
        "aliyuncs.com"
    } else if url.contains("amazonaws.com") {
        "amazonaws.com"
    } else {
        "object-storage"
    }
}

fn error_kind(error: &CloudError) -> &'static str {
    match error {
        CloudError::NotAuthorized => "not-authorized",
        CloudError::AccountMismatch => "account-mismatch",
        CloudError::AuthorizationExpired => "authorization-expired",
        CloudError::WebAuthorizationRequired => "web-authorization-required",
        CloudError::CaptchaRequired => "captcha-required",
        CloudError::AuthorizationPending => "authorization-pending",
        CloudError::SourceFileMissing => "source-file-missing",
        CloudError::DownloadCheckpointInvalid => "download-checkpoint-invalid",
        CloudError::DownloadEngineUnavailable => "download-engine-unavailable",
        CloudError::DownloadSubmissionFailed => "download-submission-failed",
        CloudError::SessionRenewedRetryOperation => "session-renewed-retry-operation",
        CloudError::InvalidResponse => "invalid-response",
        CloudError::Storage => "storage",
        CloudError::Transport(_) => "transport",
        CloudError::Provider(_) => "provider",
        CloudError::InvalidInput(_) => "invalid-input",
        CloudError::UploadUnavailable => "upload-unavailable",
        CloudError::ProviderUnsupported => "provider-unsupported",
        CloudError::XunleiVerificationUnavailable(_) => "xunlei-verification-unavailable",
        CloudError::BaiduOperationVerificationRequired => "operation-verification-required",
        CloudError::BaiduUploadAuthorizationRequired(_) => "baidu-upload-authorization-required",
        CloudError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        CloudError::BaiduTransferPending => "baidu-transfer-pending",
        CloudError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
        CloudError::Mega(_) => "mega-link-error",
        CloudError::Gofile(_) => "gofile-link-error",
        CloudError::PikpakRegionalMembershipRestricted => "provider-restricted",
        CloudError::PikpakOperationVerificationRequired(_) => "operation-verification-required",
        CloudError::XunleiVerificationUnavailable(_) => "xunlei-verification-unavailable",
        CloudError::BaiduOperationVerificationRequired => "baidu-operation-verification-required",
        CloudError::BaiduDriveVerificationRequired => "baidu-operation-verification-required",
        CloudError::BaiduUploadAuthorizationRequired(_) => "baidu-upload-authorization-required",
        CloudError::BaiduUploadPrecreateRejected { .. } => "baidu-upload-precreate-rejected",
        CloudError::BaiduUploadWebSessionRequired => "baidu-upload-web-session-required",
        CloudError::XunleiNativeDownload(_) => "xunlei-native-download-error",
        CloudError::BaiduTransferPending => "baidu-transfer-pending",
        CloudError::ShareTransferDownloadUnavailable => "transfer-saved-download-unavailable",
    }
}

impl CloudHttpTransport for AuditedTransport {
    fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, CloudError> {
        let method = request.method;
        let host = safe_host(&request.url);
        match self.0.send(request) {
            Ok(response) => {
                eprintln!("REQUEST {method} {host}: HTTP {}", response.status);
                Ok(response)
            }
            Err(error) => {
                eprintln!("REQUEST {method} {host}: {}", error_kind(&error));
                Err(error)
            }
        }
    }
}

impl CloudUploadTransport for AuditedTransport {
    fn send_upload(&self, request: UploadHttpRequest) -> Result<UploadHttpResponse, CloudError> {
        let method = request.method;
        let host = safe_host(&request.url);
        match self.0.send_upload(request) {
            Ok(response) => {
                eprintln!("TRANSFER {method} {host}: HTTP {}", response.status);
                Ok(response)
            }
            Err(error) => {
                eprintln!("TRANSFER {method} {host}: {}", error_kind(&error));
                Err(error)
            }
        }
    }
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn step<T>(name: &str, operation: impl FnOnce() -> Result<T, CloudError>) -> LiveResult<T> {
    match operation() {
        Ok(value) => {
            eprintln!("STEP {name}: ok");
            Ok(value)
        }
        Err(error) => {
            eprintln!("STEP {name}: {}", error_kind(&error));
            Err(failure(format!("{name}: {}", error_kind(&error))))
        }
    }
}

fn require_live_authorization() -> LiveResult<()> {
    if std::env::var("VERSADOWN_LIVE_CLOUD").as_deref() != Ok("1") {
        return Err(failure(
            "live cloud regression requires VERSADOWN_LIVE_CLOUD=1",
        ));
    }
    Ok(())
}

fn session_root() -> LiveResult<PathBuf> {
    if let Some(data_dir) = std::env::var_os("VERSADOWN_TEST_DATA_DIR") {
        return Ok(PathBuf::from(data_dir).join("cloud-sessions"));
    }
    let app_data = std::env::var_os("APPDATA")
        .ok_or_else(|| failure("APPDATA is required for the encrypted session store"))?;
    Ok(PathBuf::from(app_data)
        .join("com.versadown.desktop")
        .join("cloud-sessions"))
}

fn unique_name() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "VersaDown-E2E-xunlei-{millis}-{:08x}",
        rand::thread_rng().next_u32()
    )
}

fn wait_named(
    state: &CloudState,
    http: &AuditedTransport,
    parent: &str,
    name: &str,
    kind: CloudFileKind,
) -> LiveResult<CloudFile> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let directory = ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, http)
            .list_directory(parent)
            .map_err(|error| failure(format!("wait for {name:?}: {}", error_kind(&error))))?;
        if let Some(file) = directory
            .items
            .into_iter()
            .find(|item| item.name == name && item.kind == kind)
        {
            return Ok(file);
        }
        if std::time::Instant::now() >= deadline {
            return Err(failure(format!(
                "{name:?} was not visible within 30 seconds"
            )));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn wait_absent(
    state: &CloudState,
    http: &AuditedTransport,
    parent: &str,
    file_id: &str,
    label: &str,
) -> LiveResult<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let directory = ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, http)
            .list_directory(parent)
            .map_err(|error| failure(format!("verify {label}: {}", error_kind(&error))))?;
        if directory.items.iter().all(|item| item.id != file_id) {
            eprintln!("STEP verify {label} absent: ok");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(failure(format!("{label} remained visible after deletion")));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn download_to(resolved: &ResolvedCloudDownload, output: &Path) -> LiveResult<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut request = client.get(&resolved.url);
    for (name, value) in &resolved.headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .map_err(|_| failure("resolved download transport failed"))?;
    if !response.status().is_success() {
        return Err(failure(format!(
            "resolved download returned HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .map_err(|_| failure("resolved download body read failed"))?
        .to_vec();
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, &bytes)?;
    Ok(bytes)
}

fn upload(
    state: &CloudState,
    http: &AuditedTransport,
    parent: &str,
    path: &Path,
) -> LiveResult<CloudFile> {
    step(
        &format!(
            "upload {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        || {
            state.upload_file(CloudProvider::Xunlei, http, parent, path)
        },
    )
}

fn trash_and_delete(
    state: &CloudState,
    http: &AuditedTransport,
    file_id: &str,
    label: &str,
) -> LiveResult<()> {
    let ids = vec![file_id.to_owned()];
    step(&format!("trash {label}"), || {
        ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, http)
            .trash(&ids)
    })?;
    step(&format!("permanently delete {label}"), || {
        ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, http)
            .delete(&ids)
    })?;
    wait_absent(
        state,
        http,
        ProviderRegistry::global()
            .get(CloudProvider::Xunlei)
            .recycle_bin_directory_id,
        file_id,
        label,
    )
}

fn cleanup_test_root(state: &CloudState, http: &AuditedTransport, root_id: &str) -> LiveResult<()> {
    let ids = vec![root_id.to_owned()];
    let recycle_id = ProviderRegistry::global()
        .get(CloudProvider::Xunlei)
        .recycle_bin_directory_id;
    let root_visible = ProviderRegistry::global()
        .driver(CloudProvider::Xunlei, &state.store, http)
        .list_directory("0")
        .map(|directory| directory.items.iter().any(|item| item.id == root_id))
        .unwrap_or(false);
    if root_visible {
        step("cleanup trash test root", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, http)
                .trash(&ids)
        })?;
    }
    let recycled = ProviderRegistry::global()
        .driver(CloudProvider::Xunlei, &state.store, http)
        .list_directory(recycle_id)?
        .items
        .iter()
        .any(|item| item.id == root_id);
    if recycled {
        step("cleanup permanently delete test root", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, http)
                .delete(&ids)
        })?;
    }
    wait_absent(state, http, "0", root_id, "test root in drive")?;
    wait_absent(state, http, recycle_id, root_id, "test root in recycle bin")
}

#[test]
#[ignore = "requires saved Xunlei authorization and mutates only VersaDown-E2E-xunlei-* test data"]
fn live_xunlei_complete_crud_and_transfer_matrix() -> LiveResult<()> {
    require_live_authorization()?;
    let state = CloudState::new(session_root()?)?;
    let http = AuditedTransport::new()?;
    let descriptor = ProviderRegistry::global().get(CloudProvider::Xunlei);
    assert!(descriptor.capabilities.list);
    assert!(descriptor.capabilities.create_directory);
    assert!(descriptor.capabilities.rename);
    assert!(descriptor.capabilities.r#move);
    assert!(descriptor.capabilities.trash);
    assert!(descriptor.capabilities.delete);
    assert!(descriptor.capabilities.download);
    assert!(descriptor.capabilities.upload);

    step("restore saved account", || {
        ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, &http)
            .account()
    })?;
    let initial_root = step("list root", || {
        ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, &http)
            .list_directory(descriptor.root_directory_id)
    })?;

    let root_name = unique_name();
    if initial_root.items.iter().any(|item| item.name == root_name) {
        return Err(failure("unique remote test root collided"));
    }
    let test_root = step("create unique test root", || {
        ProviderRegistry::global()
            .driver(CloudProvider::Xunlei, &state.store, &http)
            .create_directory(descriptor.root_directory_id, &root_name)
    })?;

    let local_root = std::env::temp_dir().join(&root_name);
    fs::create_dir_all(local_root.join("upload-tree/sub"))?;
    let single_bytes = b"VersaDown Xunlei E2E single file\n".to_vec();
    let tree_a_bytes = b"VersaDown Xunlei E2E tree root\n".to_vec();
    let mut tree_b_bytes = vec![0_u8; 32 * 1024];
    rand::thread_rng().fill_bytes(&mut tree_b_bytes);
    fs::write(local_root.join("single.txt"), &single_bytes)?;
    fs::write(local_root.join("upload-tree/root.txt"), &tree_a_bytes)?;
    fs::write(local_root.join("upload-tree/sub/nested.bin"), &tree_b_bytes)?;

    let result = (|| -> LiveResult<()> {
        let move_target = step("create move target folder", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .create_directory(&test_root.id, "move-target")
        })?;

        let rename_folder = step("create folder for rename", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .create_directory(&test_root.id, "folder-before-rename")
        })?;
        step("rename folder", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .rename(&rename_folder.id, "folder-after-rename")
        })?;
        let renamed_folder = wait_named(
            &state,
            &http,
            &test_root.id,
            "folder-after-rename",
            CloudFileKind::Folder,
        )?;

        upload(&state, &http, &test_root.id, &local_root.join("single.txt"))?;
        let single = wait_named(
            &state,
            &http,
            &test_root.id,
            "single.txt",
            CloudFileKind::File,
        )?;
        step("rename file", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .rename(&single.id, "single-renamed.txt")
        })?;
        let renamed_file = wait_named(
            &state,
            &http,
            &test_root.id,
            "single-renamed.txt",
            CloudFileKind::File,
        )?;
        step("move file", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .move_files(&[renamed_file.id.clone()], &move_target.id)
        })?;
        let moved_file = wait_named(
            &state,
            &http,
            &move_target.id,
            "single-renamed.txt",
            CloudFileKind::File,
        )?;
        let resolved = step("resolve single-file download", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .resolve_download(&moved_file.id)
        })?;
        if download_to(&resolved, &local_root.join("downloads/single-renamed.txt"))? != single_bytes
        {
            return Err(failure("single-file download bytes differ from upload"));
        }
        eprintln!("STEP download single file: ok");

        step("move folder", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .move_files(&[renamed_folder.id.clone()], &move_target.id)
        })?;
        wait_named(
            &state,
            &http,
            &move_target.id,
            "folder-after-rename",
            CloudFileKind::Folder,
        )?;
        eprintln!("STEP verify moved folder: ok");

        // Directory upload uses the production plugin primitives recursively:
        // create matching folders, then upload each ordinary file through UploadOps.
        let upload_tree = step("upload directory create root", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .create_directory(&test_root.id, "upload-tree")
        })?;
        let upload_sub = step("upload directory create child", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .create_directory(&upload_tree.id, "sub")
        })?;
        upload(
            &state,
            &http,
            &upload_tree.id,
            &local_root.join("upload-tree/root.txt"),
        )?;
        upload(
            &state,
            &http,
            &upload_sub.id,
            &local_root.join("upload-tree/sub/nested.bin"),
        )?;
        eprintln!("STEP upload directory with child: ok");

        let plan = step("plan directory download", || {
            plan_cloud_directory_download(
                CloudProvider::Xunlei,
                &upload_tree.id,
                "upload-tree",
                |directory_id| {
                    ProviderRegistry::global()
                        .driver(CloudProvider::Xunlei, &state.store, &http)
                        .list_directory(directory_id)
                },
            )
        })?;
        if plan.files.len() != 2 {
            return Err(failure(format!(
                "directory download planned {} files instead of 2",
                plan.files.len()
            )));
        }
        let expected = BTreeMap::from([
            ("upload-tree/root.txt".to_owned(), tree_a_bytes.clone()),
            (
                "upload-tree/sub/nested.bin".to_owned(),
                tree_b_bytes.clone(),
            ),
        ]);
        for file in &plan.files {
            let resolved = step("resolve directory leaf download", || {
                ProviderRegistry::global()
                    .driver(CloudProvider::Xunlei, &state.store, &http)
                    .resolve_download(&file.file_id)
            })?;
            let relative = file
                .relative_directory
                .iter()
                .chain(std::iter::once(&file.output_name))
                .collect::<PathBuf>();
            let relative_key = relative.to_string_lossy().replace('\\', "/");
            let downloaded = download_to(
                &resolved,
                &local_root.join("downloads/tree").join(&relative),
            )?;
            if expected.get(&relative_key) != Some(&downloaded) {
                return Err(failure(format!(
                    "directory leaf {relative_key:?} bytes differ"
                )));
            }
        }
        eprintln!("STEP download directory with child: ok");

        let delete_file = upload(&state, &http, &test_root.id, &local_root.join("single.txt"))?;
        let delete_file = wait_named(
            &state,
            &http,
            &test_root.id,
            &delete_file.name,
            CloudFileKind::File,
        )?;
        trash_and_delete(&state, &http, &delete_file.id, "file")?;
        eprintln!("STEP delete file: ok");

        let delete_tree = step("create recursively deleted folder", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .create_directory(&test_root.id, "delete-tree")
        })?;
        let delete_child = step("create recursively deleted child", || {
            ProviderRegistry::global()
                .driver(CloudProvider::Xunlei, &state.store, &http)
                .create_directory(&delete_tree.id, "child")
        })?;
        upload(
            &state,
            &http,
            &delete_child.id,
            &local_root.join("single.txt"),
        )?;
        trash_and_delete(&state, &http, &delete_tree.id, "directory with child")?;
        eprintln!("STEP delete directory recursively: ok");
        Ok(())
    })();

    let cleanup = cleanup_test_root(&state, &http, &test_root.id);
    let local_cleanup = fs::remove_dir_all(&local_root);
    match (result, cleanup, local_cleanup) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(test), Ok(()), Ok(())) => Err(test),
        (Ok(()), Err(cleanup), _) => Err(failure(format!("remote cleanup failed: {cleanup}"))),
        (Ok(()), Ok(()), Err(cleanup)) => Err(Box::new(cleanup)),
        (Err(test), Err(cleanup), _) => Err(failure(format!(
            "test failed ({test}); remote cleanup also failed ({cleanup})"
        ))),
        (Err(test), Ok(()), Err(cleanup)) => Err(failure(format!(
            "test failed ({test}); local cleanup also failed ({cleanup})"
        ))),
    }
}
