//! UC Drive's authenticated web protocol.
//!
//! UC issues browser-cookie sessions rather than a stable public OAuth token.
//! This adapter therefore retains only the minimal cookie pairs obtained from
//! the official `drive.uc.cn` web authorization flow.  Signed download URLs
//! are never persisted. Server-rotated session cookies are retained so a
//! directory read and the following download resolve use the same session.
use super::{
    types::required, CloudAccount, CloudAuthMethod, CloudDirectory, CloudDriveUploadProfile,
    CloudFile, CloudFileKind, CloudHttpRequest, CloudHttpResponse, CloudHttpTransport,
    CloudProvider, CloudShare, CloudShareOptions, CloudSharePassCodeCharacterSet,
    CloudSharePassCodeConstraint, CloudStorageCapacity, CloudUploadTransport, QuarkError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{cell::RefCell, collections::BTreeMap, fmt, path::Path};

const API: &str = "https://pc-api.uc.cn/1/clouddrive";
const WEB: &str = "https://drive.uc.cn";
pub(crate) const UC_CARD_DISPLAY_NAME: &str = "UC网盘";
const UC_SHARE_TASK_MAX_POLLS: usize = 60;
const UC_SHARE_TRANSIENT_RETRIES: usize = 2;
const UC_SHARE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
pub const UC_DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) uc-cloud-drive/2.5.20 Chrome/100.0.4896.160 Electron/18.3.5.4-b478491100 Safari/537.36 Channel/pckk_other_ch";
// The download CDN rejects the legacy desktop-client profile used by UC's
// JSON API. Use the current browser profile only for the signed file request.
const UC_DOWNLOAD_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";
const UC_UPLOAD_PROFILE: CloudDriveUploadProfile = CloudDriveUploadProfile {
    object_host: "pds.uc.cn",
    object_store_user_agent: "aliyun-sdk-js/6.6.1",
};
pub(crate) const UC_RECYCLE_BIN_ID: &str = "__versa_down_uc_recycle_bin__";
const UC_RECYCLE_BIN_NAME: &str = "回收站";

pub const UC_SHARE_PASS_CODE_CONSTRAINT: CloudSharePassCodeConstraint =
    CloudSharePassCodeConstraint {
        required: false,
        min_length: 4,
        max_length: 4,
        character_set: CloudSharePassCodeCharacterSet::AsciiAlphanumeric,
        hint: "提取码必须为 4 位字母或数字",
    };
pub(crate) const UC_WEB_PROFILE_JS: &str = include_str!("uc-web-profile.js");

#[derive(Clone, Deserialize, Serialize)]
pub struct UcSession {
    pub cookie: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Stable UC user id obtained from the authenticated member response.
    /// `__uid` is intentionally not stored here: UC rotates that browser
    /// session marker and it cannot safely bind an interrupted download.
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub auth_method: CloudAuthMethod,
}

impl fmt::Debug for UcSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UcSession")
            .field("cookie", &"[redacted]")
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

impl UcSession {
    pub fn cookie(cookie: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            display_name: None,
            account_id: None,
            auth_method: CloudAuthMethod::Web,
        }
    }
}

/// Convert official-WebView cookie records into the minimum restorable UC
/// session.  Cookie attributes, trackers, and non-UC cookies are excluded.
pub fn parse_uc_web_session(cookies: &[String]) -> Result<UcSession, QuarkError> {
    const SESSION_COOKIE_NAMES: &[&str] = &[
        "__uid",
        "__pus",
        "__puus",
        "_UP_A4A_11_",
        "__kp",
        "__kps",
        "__ktd",
        "b-user-id",
        // UC issues this cookie from drive.uc.cn alongside the normal login
        // session. Directory reads work without it, but the server rejects
        // share creation when it is absent.
        "UDRIVE_TRANSFER_SESS",
        // Share-download signing requires this browser-issued anti-CSRF
        // value even though directory listing does not.
        "csrf_token",
    ];
    let mut pairs: Vec<(String, String)> = Vec::new();
    for raw in cookies {
        let pair = raw.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if value.is_empty()
            || !SESSION_COOKIE_NAMES.contains(&name)
            || name.chars().any(char::is_control)
            || value.contains(['\r', '\n'])
        {
            continue;
        }
        if let Some(existing) = pairs.iter_mut().find(|(current, _)| current == name) {
            existing.1 = value.to_owned();
        } else {
            pairs.push((name.to_owned(), value.to_owned()));
        }
    }
    if !pairs.iter().any(|(name, _)| name == "__puus") {
        return Err(QuarkError::AuthorizationPending);
    }
    Ok(UcSession::cookie(
        pairs
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

/// The official UC page keeps the visible nickname in its in-memory store.
/// The page script deliberately returns only this presentation field; browser
/// cookies and any other store state stay in the WebView.
pub fn parse_uc_web_authorization(
    cookies: &[String],
    browser_profile: &Value,
) -> Result<UcSession, QuarkError> {
    let mut session = parse_uc_web_session(cookies)?;
    session.display_name = browser_profile
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| name.chars().count() <= 256 && !name.chars().any(char::is_control))
        .map(str::to_owned);
    Ok(session)
}

#[derive(Clone, Debug)]
pub struct UcDownload {
    pub url: String,
    pub name: String,
    pub headers: BTreeMap<String, String>,
}

pub struct UcClient<'a, T: CloudHttpTransport + ?Sized> {
    session: RefCell<UcSession>,
    http: &'a T,
}

impl<'a, T: CloudHttpTransport + ?Sized> UcClient<'a, T> {
    pub fn new(session: UcSession, http: &'a T) -> Self {
        Self {
            session: RefCell::new(session),
            http,
        }
    }

    pub fn session(&self) -> UcSession {
        self.session.borrow().clone()
    }

    fn cookie(&self) -> Result<String, QuarkError> {
        let cookie = self.session.borrow().cookie.clone();
        let has_session = cookie
            .split(';')
            .filter_map(cookie_pair)
            .any(|(name, _)| name == "__puus");
        if has_session {
            Ok(cookie)
        } else {
            Err(QuarkError::AuthorizationExpired)
        }
    }

    fn headers(&self) -> Result<BTreeMap<String, String>, QuarkError> {
        Ok(BTreeMap::from([
            ("Accept".into(), "application/json, text/plain, */*".into()),
            ("Accept-Language".into(), "zh-CN,zh;q=0.9".into()),
            ("Cookie".into(), self.cookie()?),
            (
                "Content-Type".into(),
                "application/json;charset=UTF-8".into(),
            ),
            ("Origin".into(), WEB.into()),
            ("Referer".into(), format!("{WEB}/")),
            ("User-Agent".into(), UC_DESKTOP_USER_AGENT.into()),
        ]))
    }

    fn request(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        let response = self.send(method, path, query, body)?;
        uc_json(&response)?;
        Ok(response)
    }

    fn send(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<CloudHttpResponse, QuarkError> {
        let query = if query.is_empty() {
            "pr=UCBrowser&fr=pc".into()
        } else {
            format!("{query}&fr=pc&pr=UCBrowser")
        };
        let (response, rotated) = self.send_once(method, path, &query, body.clone())?;
        if rotated && (method == "GET" || (method == "POST" && path == "file/download")) {
            return self
                .send_once(method, path, &query, body)
                .map(|(response, _)| response);
        }
        Ok(response)
    }

    fn send_once(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<(CloudHttpResponse, bool), QuarkError> {
        let original_cookie = self.cookie()?;
        let mut headers = self.headers()?;
        if matches!(method, "POST" | "PUT" | "PATCH" | "DELETE") {
            if let Some(csrf) = cookie_value(&original_cookie, "csrf_token") {
                headers.insert("x-csrf-token".into(), csrf);
            }
        }
        let response = self.http.send(CloudHttpRequest {
            method,
            url: format!("{API}/{path}?{query}"),
            headers,
            body: body.map(|value| value.to_string()),
        })?;
        let refreshed_cookie = cookie_for_download(&original_cookie, &response.set_cookies);
        let session_rotated =
            cookie_value(&original_cookie, "__puus") != cookie_value(&refreshed_cookie, "__puus");
        if refreshed_cookie != original_cookie {
            self.session.borrow_mut().cookie = refreshed_cookie;
        }
        Ok((response, session_rotated))
    }

    fn request_json(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        let response = self.request(method, path, query, body)?;
        uc_json(&response)
    }

    /// The create call has already committed before the returned task is
    /// polled. Retrying only these follow-up reads avoids duplicate shares
    /// while allowing a transient TLS/socket failure to recover.
    fn personal_share_follow_up(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        for attempt in 0..=UC_SHARE_TRANSIENT_RETRIES {
            match self.request_json(method, path, query, body.clone()) {
                Err(QuarkError::Transport(_)) if attempt < UC_SHARE_TRANSIENT_RETRIES => {
                    std::thread::sleep(UC_SHARE_POLL_INTERVAL);
                }
                result => return result,
            }
        }
        unreachable!("retry loop always returns its final request result")
    }

    /// Browser `__uid` is sufficient to render an old session card, but it is
    /// not a durable account identity.  Never use it to decide whether a
    /// paused download belongs to a newly restored session.
    fn legacy_session_id(&self) -> Result<String, QuarkError> {
        let cookie = self.cookie()?;
        Ok(cookie_value(&cookie, "__uid")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("uc-{:x}", md5::compute(&cookie))))
    }

    fn persisted_account_id(&self) -> Option<String> {
        self.session
            .borrow()
            .account_id
            .clone()
            .map(|id| id.trim().to_owned())
            .filter(|id| {
                !id.is_empty() && id.chars().count() <= 256 && !id.chars().any(char::is_control)
            })
    }

    /// UC's `b-user-id` cookie is a user identifier issued alongside the web
    /// session. Unlike `__uid`, it is not the `AAR/...` browser-session
    /// marker that changes when a user signs in again.
    fn browser_account_id(&self) -> Result<Option<String>, QuarkError> {
        Ok(cookie_value(&self.cookie()?, "b-user-id").filter(|id| {
            !id.is_empty()
                && id.chars().count() <= 256
                && !id.chars().any(char::is_control)
                && !id.starts_with("AAR/")
        }))
    }

    fn account_with_id(&self, id: String) -> CloudAccount {
        CloudAccount {
            provider: CloudProvider::Uc,
            display_name: self
                .saved_display_name(&id)
                .unwrap_or_else(|| UC_CARD_DISPLAY_NAME.to_owned()),
            id,
            auth_method: self.session.borrow().auth_method,
        }
    }

    fn has_share_session_cookie(&self) -> Result<bool, QuarkError> {
        Ok(cookie_value(&self.cookie()?, "UDRIVE_TRANSFER_SESS")
            .is_some_and(|value| !value.is_empty()))
    }

    fn saved_display_name(&self, account_id: &str) -> Option<String> {
        self.session
            .borrow()
            .display_name
            .clone()
            .filter(|value| !value.is_empty())
            // `__uid` is an opaque, browser-issued session identifier. It
            // was written as a card label by older builds, but is never a
            // user-visible UC account name.
            .filter(|value| value != account_id)
            .filter(|value| value != UC_CARD_DISPLAY_NAME)
    }

    /// Build the account shown from a browser-issued UC session. The official
    /// page's visible nickname is retained with the encrypted session, so the
    /// card can be restored without replaying opaque browser context.
    pub fn web_account(&self) -> Result<CloudAccount, QuarkError> {
        let id = self
            .persisted_account_id()
            .unwrap_or(self.legacy_session_id()?);
        Ok(self.account_with_id(id))
    }

    /// Return the durable UC user identity. The live browser cookie/member
    /// response must take precedence over persisted metadata: UC can switch
    /// accounts in the same browser profile, leaving an otherwise valid
    /// encrypted session whose saved account id belongs to the previous user.
    /// Neither live source accepts the rotating `AAR/...` marker.
    pub fn verified_account_id(&self) -> Result<Option<String>, QuarkError> {
        let browser_account_id = self.browser_account_id()?;
        let response = self.request_json(
            "GET",
            "member",
            "fetch_subscribe=true&_ch=home&fetch_identity=true",
            None,
        );
        match response {
            Ok(value) => {
                let member_account_id = value["data"]
                    .get("user_id")
                    .and_then(string_or_number)
                    .filter(|id| {
                        !id.is_empty()
                            && id.chars().count() <= 256
                            && !id.chars().any(char::is_control)
                            && !id.starts_with("AAR/")
                    });
                Ok(member_account_id.or(browser_account_id))
            }
            Err(_) if browser_account_id.is_some() => Ok(browser_account_id),
            Err(error) => Err(error),
        }
    }

    /// A UC browser session restores the account captured at authorization.
    /// The rotating `__uid` is a rendering-only fallback for old sessions;
    /// new bindings use `verified_account_id` above.
    pub fn account(&self) -> Result<CloudAccount, QuarkError> {
        let id = self
            .verified_account_id()?
            .unwrap_or(self.legacy_session_id()?);
        Ok(self.account_with_id(id))
    }

    pub fn storage_capacity(&self) -> Result<CloudStorageCapacity, QuarkError> {
        let value = self.request_json(
            "GET",
            "member",
            "fetch_subscribe=true&_ch=home&fetch_identity=true",
            None,
        )?;
        CloudStorageCapacity::from_json(
            &value["data"]["total_capacity"],
            &value["data"]["use_capacity"],
            false,
        )
    }

    fn stable_account_id(&self) -> Result<Option<String>, QuarkError> {
        if let Some(id) = self.persisted_account_id() {
            return Ok(Some(id));
        }
        // Share-transfer ownership is an existing compatibility contract. Its
        // legacy fallback stays local to those plans; personal-download
        // bindings use `verified_account_id` and never rely on this marker.
        if let Some(id) = cookie_value(&self.cookie()?, "__uid").filter(|id| !id.is_empty()) {
            return Ok(Some(id));
        }
        self.verified_account_id()
    }

    pub fn directory(&self, parent: &str) -> Result<CloudDirectory, QuarkError> {
        if parent.is_empty() || parent.contains(['?', '#', '\r', '\n']) {
            return Err(QuarkError::InvalidInput("目录 ID 无效".into()));
        }
        let mut items = Vec::new();
        for page in 1..=10_000 {
            let value = self.request_json(
                "GET",
                "file/sort",
                &format!(
                    "pdir_fid={}&_page={page}&_size=200&_sort=file_name%3Aasc",
                    encoded(parent)
                ),
                None,
            )?;
            let files = value
                .pointer("/data/list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = files.len();
            items.extend(
                files
                    .iter()
                    .map(|file| parse_file(file, parent))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let total = value.pointer("/metadata/_total").and_then(Value::as_u64);
            if count < 200 || total.is_some_and(|total| items.len() as u64 >= total) {
                if parent == "0" {
                    items.push(CloudFile {
                        id: UC_RECYCLE_BIN_ID.into(),
                        parent_id: "0".into(),
                        name: UC_RECYCLE_BIN_NAME.into(),
                        kind: "folder".into(),
                        size: None,
                        modified_at: None,
                    });
                }
                return Ok(CloudDirectory {
                    id: parent.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<CloudFile, QuarkError> {
        validate_name(name)?;
        let response = self.request(
            "POST",
            "file",
            "",
            Some(json!({"pdir_fid":parent,"file_name":name,"dir_path":"","dir_init_lock":false})),
        )?;
        let value = uc_json(&response)?;
        Ok(CloudFile {
            id: required(value.get("data").ok_or(QuarkError::InvalidResponse)?, "fid")?,
            parent_id: parent.into(),
            name: name.into(),
            kind: "folder".into(),
            size: None,
            modified_at: None,
        })
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<CloudHttpResponse, QuarkError> {
        validate_id(id)?;
        validate_name(name)?;
        self.request(
            "POST",
            "file/rename",
            "",
            Some(json!({"fid":id,"file_name":name})),
        )
    }

    pub fn move_files(
        &self,
        ids: &[String],
        parent: &str,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.file_operation("file/move", ids, parent)
    }

    pub fn copy_files(
        &self,
        ids: &[String],
        parent: &str,
    ) -> Result<CloudHttpResponse, QuarkError> {
        self.file_operation("file/copy", ids, parent)
    }

    fn file_operation(
        &self,
        path: &str,
        ids: &[String],
        parent: &str,
    ) -> Result<CloudHttpResponse, QuarkError> {
        validate_ids(ids)?;
        validate_id(parent)?;
        self.request(
            "POST",
            path,
            "",
            Some(json!({"filelist":ids,"to_pdir_fid":parent,"action_type":1})),
        )
    }

    pub fn trash(&self, ids: &[String]) -> Result<CloudHttpResponse, QuarkError> {
        validate_ids(ids)?;
        self.request(
            "POST",
            "file/delete",
            "",
            Some(json!({"action_type":2,"filelist":ids,"exclude_fids":[]})),
        )
    }

    /// Uploads one regular local file using UC's authenticated CloudDrive
    /// endpoints. The shared transfer code never takes ownership of UC's
    /// browser cookie or request headers.
    pub fn upload_file<U: CloudUploadTransport>(
        &self,
        parent: &str,
        path: &Path,
        upload_http: &U,
    ) -> Result<CloudFile, QuarkError> {
        if parent == UC_RECYCLE_BIN_ID {
            return Err(QuarkError::InvalidInput(
                "回收站不能作为上传目标目录".into(),
            ));
        }
        super::upload::upload_cloud_drive_file(
            upload_http,
            parent,
            path,
            UC_UPLOAD_PROFILE,
            |payload| self.request_json("POST", "file/upload/pre", "", Some(payload)),
            |payload| self.request_json("POST", "file/update/hash", "", Some(payload)),
            |payload| self.request_json("POST", "file/upload/auth", "", Some(payload)),
            |payload| self.request_json("POST", "file/upload/finish", "", Some(payload)),
        )
    }

    pub fn list_recycle_bin(&self) -> Result<CloudDirectory, QuarkError> {
        let mut items = Vec::new();
        for page in 1..=10_000 {
            let value = self.request_json(
                "GET",
                "file/recycle/list",
                &format!("_page={page}&_size=200"),
                None,
            )?;
            let files = value
                .pointer("/data/list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = files.len();
            items.extend(
                files
                    .iter()
                    .map(uc_recycle_file)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let total = value.pointer("/metadata/_total").and_then(Value::as_u64);
            if count < 200 || total.is_some_and(|total| items.len() as u64 >= total) {
                return Ok(CloudDirectory {
                    id: UC_RECYCLE_BIN_ID.into(),
                    items,
                });
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn restore_recycle_entries(&self, record_ids: &[String]) -> Result<(), QuarkError> {
        validate_recycle_record_ids(record_ids)?;
        self.request(
            "POST",
            "file/recycle/restore",
            "",
            Some(json!({"select_mode":2,"record_list":record_ids})),
        )?;
        Ok(())
    }

    pub fn delete_recycle_entries(&self, record_ids: &[String]) -> Result<(), QuarkError> {
        validate_recycle_record_ids(record_ids)?;
        self.request(
            "POST",
            "file/recycle/remove",
            "",
            Some(json!({"select_mode":2,"record_list":record_ids})),
        )?;
        Ok(())
    }

    pub fn create_share(
        &self,
        file_ids: &[String],
        options: &CloudShareOptions,
    ) -> Result<CloudShare, QuarkError> {
        if file_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个文件或目录".into()));
        }
        if options.generate_pass_code {
            return Err(QuarkError::InvalidInput(
                "UC 网盘需要设置自定义提取码".into(),
            ));
        }
        let pass_code = options
            .pass_code
            .as_deref()
            .filter(|code| !code.trim().is_empty());
        let mut body = json!({
            "fid_list": file_ids,
            "title": options.name,
            "url_type": if pass_code.is_some() { 2 } else { 1 },
            "expired_type": uc_share_expiry_type(options.expires_in_days)?,
            "dl_limit": -1,
        });
        if let Some(pass_code) = pass_code {
            body["passcode"] = json!(pass_code);
        }
        let response = self.send("POST", "share", "", Some(body))?;
        let created = match uc_json(&response) {
            Err(QuarkError::Provider(_))
                if uc_response_code(&response) == Some(41026)
                    && !self.has_share_session_cookie()? =>
            {
                // Legacy sessions were persisted before UC's share-scoped
                // browser cookie was retained. Reopen the official page so
                // the next captured session can make this sensitive request.
                return Err(QuarkError::WebAuthorizationRequired);
            }
            result => result?,
        };
        let created_data = created.get("data").ok_or(QuarkError::InvalidResponse)?;
        let mut share_id = uc_share_task_value(created_data, "share_id");
        let task_id = uc_share_task_value(created_data, "task_id");
        if share_id.is_none() {
            let task_id = task_id.ok_or(QuarkError::InvalidResponse)?;
            for retry in 0..UC_SHARE_TASK_MAX_POLLS {
                let task = self.personal_share_follow_up(
                    "GET",
                    "task",
                    &format!("task_id={}&retry_index={retry}", encoded(&task_id)),
                    None,
                )?;
                if let Some(id) = task
                    .pointer("/data/share_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    share_id = Some(id.to_owned());
                    break;
                }
                match task.pointer("/data/status").and_then(Value::as_i64) {
                    Some(0 | 1) | None => std::thread::sleep(UC_SHARE_POLL_INTERVAL),
                    _ => return Err(QuarkError::Provider("UC 分享创建任务失败".into())),
                }
            }
        }
        let share_id = share_id
            .ok_or_else(|| QuarkError::Provider("UC 分享链接仍在生成，请稍后重试".into()))?;
        let details = self.personal_share_follow_up(
            "POST",
            "share/password",
            "",
            Some(json!({"share_id": &share_id})),
        )?;
        let mut share = uc_share_from_value(
            details.get("data").unwrap_or(&details),
            Some(file_ids),
            pass_code,
        )?;
        // The password-detail response can expose its own `pwd_id` but not the
        // created share ID. Keep the task's `share_id`, since UC's management
        // list and delete endpoints both operate on that identifier.
        share.id = share_id;
        Ok(share)
    }

    pub fn list_shares(&self) -> Result<Vec<CloudShare>, QuarkError> {
        let mut shares = Vec::new();
        for page in 1..=10_000 {
            let value = self.request_json("GET", "share/mypage/detail", &format!("_page={page}&_size=200&_order_field=created_at&_order_type=desc&_fetch_total=1"), None)?;
            let list = value
                .pointer("/data/list")
                .and_then(Value::as_array)
                .ok_or(QuarkError::InvalidResponse)?;
            let count = list.len();
            shares.extend(
                list.iter()
                    .map(|share| uc_share_from_value(share, None, None))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if count < 200 {
                return Ok(shares);
            }
        }
        Err(QuarkError::InvalidResponse)
    }

    pub fn cancel_shares(&self, share_ids: &[String]) -> Result<(), QuarkError> {
        let selected_share_ids = share_ids
            .iter()
            .map(String::as_str)
            .filter(|share_id| !share_id.is_empty())
            .collect::<Vec<_>>();
        if selected_share_ids.is_empty() {
            return Err(QuarkError::InvalidInput("请选择至少一个分享".into()));
        }
        let response = self.send(
            "POST",
            "share/delete",
            "",
            Some(json!({"share_ids": selected_share_ids})),
        )?;
        let code = serde_json::from_str::<Value>(&response.body)
            .ok()
            .and_then(|value| value.get("code").and_then(Value::as_i64));
        // A share may already be stopped while UC's list endpoint is still
        // converging. In that case the follow-up list is the authoritative
        // cancellation result.
        if !(response.status == 200 && code == Some(41012)) {
            uc_json(&response)?;
        }
        for attempt in 0..UC_SHARE_TASK_MAX_POLLS {
            let shares = self.list_shares()?;
            if !shares
                .iter()
                .any(|share| selected_share_ids.contains(&share.id.as_str()))
            {
                return Ok(());
            }
            if attempt + 1 < UC_SHARE_TASK_MAX_POLLS {
                std::thread::sleep(UC_SHARE_POLL_INTERVAL);
            }
        }
        Err(QuarkError::Provider(
            "UC 分享取消状态尚未同步，请稍后刷新".into(),
        ))
    }

    /// Resolve an immediate, signed URL.  It is intentionally returned rather
    /// than stored; callers must create a stable `cloud://uc/<fid>` task source.
    pub fn download(&self, id: &str) -> Result<UcDownload, QuarkError> {
        validate_id(id)?;
        let response = self.send(
            "POST",
            "file/download",
            "entry=ft",
            Some(json!({"fids":[id]})),
        )?;
        let value = uc_json(&response)?;
        let file = value
            .pointer("/data/0")
            .ok_or(QuarkError::InvalidResponse)?;
        let url = https_download_url(&required(file, "download_url")?)?;
        let cookie = cookie_for_download(&self.cookie()?, &response.set_cookies);
        let mut headers = self.headers()?;
        // This is a GET to UC's file CDN rather than a JSON API call. Sending
        // the API Content-Type makes UC reject an otherwise valid signed URL.
        headers.remove("Content-Type");
        headers.insert("User-Agent".into(), UC_DOWNLOAD_USER_AGENT.into());
        headers.insert("Cookie".into(), cookie.clone());
        // UC validates the browser anti-CSRF token on the signed download
        // request too. Keep the value that was actually sent to the CDN: a
        // download-signing response is allowed to rotate the cookie first.
        if let Some(csrf) = cookie_value(&cookie, "csrf_token") {
            headers.insert("x-csrf-token".into(), csrf);
        }
        Ok(UcDownload {
            url,
            name: file
                .get("file_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(id)
                .to_owned(),
            headers,
        })
    }
}

fn uc_share_expiry_type(expires_in_days: Option<u16>) -> Result<i64, QuarkError> {
    match expires_in_days {
        None => Ok(1),
        Some(1) => Ok(2),
        Some(7) => Ok(3),
        Some(30) => Ok(4),
        Some(60) => Ok(5),
        Some(180) => Ok(7),
        Some(_) => Err(QuarkError::InvalidInput(
            "UC 网盘仅支持永久、1 天、7 天、30 天、60 天或 180 天有效期".into(),
        )),
    }
}

fn uc_share_task_value(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .or_else(|| value.pointer(&format!("/task_resp/data/{field}")))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn uc_share_from_value(
    value: &Value,
    fallback_file_ids: Option<&[String]>,
    pass_code: Option<&str>,
) -> Result<CloudShare, QuarkError> {
    let id = value
        .get("share_id")
        .or_else(|| value.get("pwd_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(QuarkError::InvalidResponse)?;
    let source_file_ids = value
        .get("fid_list")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .or_else(|| fallback_file_ids.map(|ids| ids.to_vec()))
        .unwrap_or_default();
    let folder = value.get("file_type").and_then(Value::as_i64) == Some(0)
        || value.get("dir").and_then(Value::as_bool) == Some(true);
    Ok(CloudShare {
        id: id.into(),
        url: value
            .get("share_url")
            .or_else(|| value.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("https://drive.uc.cn/s/{id}")),
        name: value
            .get("title")
            .or_else(|| value.get("file_name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("分享文件")
            .into(),
        kind: if folder {
            CloudFileKind::Folder
        } else {
            CloudFileKind::File
        },
        source_file_ids,
        pass_code: value
            .get("passcode")
            .or_else(|| value.get("share_pwd"))
            .and_then(Value::as_str)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .or_else(|| pass_code.map(str::to_owned)),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at: value
            .get("expired_at")
            .or_else(|| value.get("expire_at"))
            .and_then(Value::as_str)
            .filter(|time| !time.is_empty())
            .map(str::to_owned),
    })
}

fn uc_json(response: &CloudHttpResponse) -> Result<Value, QuarkError> {
    if response.status == 401 {
        return Err(QuarkError::AuthorizationExpired);
    }
    let value: Value =
        serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(0);
    let status = value.get("status").and_then(Value::as_i64).unwrap_or(200);
    let message = value
        .get("message")
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if code == 401 || status == 401 || message.to_ascii_lowercase().contains("login") {
        return Err(QuarkError::AuthorizationExpired);
    }
    let normalized_message = message.to_ascii_lowercase();
    if code == 32011 || normalized_message.contains("not real name") || message.contains("未实名")
    {
        return Err(QuarkError::Provider(
            "UC 网盘账号未实名，请先在 UC 官方页面完成实名认证后重试".into(),
        ));
    }
    if ["captcha", "verify", "risk", "安全验证"]
        .iter()
        .any(|needle| normalized_message.contains(&needle.to_ascii_lowercase()))
    {
        return Err(QuarkError::WebAuthorizationRequired);
    }
    if code == 21001 && normalized_message.contains("file not found") {
        return Err(QuarkError::SourceFileMissing);
    }
    if !(200..300).contains(&response.status) || code != 0 || !matches!(status, 0 | 200) {
        return Err(QuarkError::Provider(uc_rejection_message(
            response.status,
            code,
            status,
        )));
    }
    Ok(value)
}

fn uc_response_code(response: &CloudHttpResponse) -> Option<i64> {
    serde_json::from_str::<Value>(&response.body)
        .ok()?
        .get("code")?
        .as_i64()
}

fn uc_rejection_message(http_status: u16, code: i64, status: i64) -> String {
    let reason = if code != 0 {
        format!("错误码 {code}")
    } else if !matches!(status, 0 | 200) {
        format!("状态码 {status}")
    } else {
        format!("HTTP {http_status}")
    };
    format!("UC 网盘拒绝请求（{reason}）")
}

fn parse_file(file: &Value, parent: &str) -> Result<CloudFile, QuarkError> {
    Ok(CloudFile {
        id: required(file, "fid")?,
        parent_id: parent.into(),
        name: required(file, "file_name")?,
        kind: if file
            .get("dir")
            .and_then(Value::as_bool)
            .unwrap_or(file.get("file_type").and_then(Value::as_i64) == Some(0))
        {
            "folder"
        } else {
            "file"
        }
        .into(),
        size: file.get("size").and_then(Value::as_u64),
        modified_at: file.get("updated_at").and_then(string_or_number),
    })
}

fn uc_recycle_file(file: &Value) -> Result<CloudFile, QuarkError> {
    Ok(CloudFile {
        id: required(file, "record_id")?,
        parent_id: UC_RECYCLE_BIN_ID.into(),
        name: required(file, "file_name")?,
        kind: if file
            .get("dir")
            .and_then(Value::as_bool)
            .unwrap_or(file.get("file_type").and_then(Value::as_i64) == Some(0))
        {
            "folder"
        } else {
            "file"
        }
        .into(),
        size: file.get("size").and_then(Value::as_u64),
        modified_at: file.get("updated_at").and_then(string_or_number),
    })
}

fn encoded(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn string_or_number(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn validate_name(name: &str) -> Result<(), QuarkError> {
    if name.is_empty() || name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        return Err(QuarkError::InvalidInput("文件名无效".into()));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), QuarkError> {
    if id.is_empty() || id.contains(['?', '#', '\r', '\n']) {
        return Err(QuarkError::InvalidInput("文件 ID 无效".into()));
    }
    Ok(())
}

fn validate_ids(ids: &[String]) -> Result<(), QuarkError> {
    if ids.is_empty() {
        return Err(QuarkError::InvalidInput("请选择至少一个文件".into()));
    }
    ids.iter().try_for_each(|id| validate_id(id))
}

fn validate_recycle_record_ids(record_ids: &[String]) -> Result<(), QuarkError> {
    if record_ids.is_empty()
        || record_ids.iter().any(|record_id| {
            record_id.is_empty()
                || record_id.len() > 512
                || record_id.contains(['?', '#', '\r', '\n'])
                || record_id.chars().any(char::is_control)
        })
    {
        return Err(QuarkError::InvalidInput("回收站记录 ID 无效".into()));
    }
    Ok(())
}

fn cookie_pair(value: &str) -> Option<(&str, &str)> {
    let pair = value.trim();
    let (name, value) = pair.split_once('=')?;
    (!name.is_empty()
        && !value.is_empty()
        && !pair.contains(['\r', '\n'])
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)))
    .then_some((name, value))
}

fn cookie_value(cookie: &str, wanted: &str) -> Option<String> {
    cookie
        .split(';')
        .filter_map(cookie_pair)
        .find_map(|(name, value)| (name == wanted).then(|| value.to_owned()))
}

fn cookie_for_download(web_cookie: &str, response_cookies: &[String]) -> String {
    let mut cookies: Vec<(String, String)> = web_cookie
        .split(';')
        .filter_map(cookie_pair)
        .map(|(name, value)| (name.to_owned(), format!("{name}={value}")))
        .collect();
    for response_cookie in response_cookies {
        let pair = response_cookie.split(';').next().unwrap_or("");
        if let Some((name, value)) = cookie_pair(pair) {
            cookies.retain(|(current, _)| current != name);
            cookies.push((name.to_owned(), format!("{name}={value}")));
        }
    }
    cookies
        .into_iter()
        .map(|(_, pair)| pair)
        .collect::<Vec<_>>()
        .join("; ")
}

fn https_download_url(value: &str) -> Result<String, QuarkError> {
    let url = url::Url::parse(value).map_err(|_| QuarkError::InvalidResponse)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(QuarkError::InvalidResponse);
    }
    Ok(url.into())
}

use super::share::{
    CloudShareAdapter, CloudShareEntry, CloudSharePlan, CloudSharePlanStore,
    CloudShareResolvedEntry, CloudTransferCapacity, RecordedTransferDownload,
    RecordedTransferResolver, ShareInspect, ShareInspection, ShareOperationResult, ShareResolution,
    ShareResolve, ShareTransfer, TransferRequest,
};

pub struct UcShareAdapter<'a> {
    pub credentials: &'a dyn super::CredentialStore,
    pub http: &'a dyn CloudHttpTransport,
}
struct UcShareSession<'a> {
    http: &'a dyn CloudHttpTransport,
    headers: BTreeMap<String, String>,
    account_cookie_hash: String,
    context_changed: bool,
}
impl UcShareAdapter<'_> {
    fn stored_session(&self) -> Result<UcSession, QuarkError> {
        let bytes = self
            .credentials
            .load("uc")?
            .ok_or(QuarkError::NotAuthorized)?;
        let session: UcSession = serde_json::from_slice(&bytes).map_err(|_| QuarkError::Storage)?;
        if session.cookie.is_empty() {
            return Err(QuarkError::NotAuthorized);
        }
        Ok(session)
    }
    fn session(&self, context: Option<&Value>) -> Result<UcShareSession<'_>, QuarkError> {
        let session = self.stored_session()?;
        // Use the same authenticated cookie snapshot for signing and downloading.
        let valid = cookie_value(&session.cookie, "__puus").is_some();
        if !valid {
            return Err(QuarkError::AuthorizationExpired);
        }
        let mut headers = BTreeMap::from([
            ("Cookie".into(), session.cookie.clone()),
            ("User-Agent".into(), UC_DESKTOP_USER_AGENT.into()),
            ("Origin".into(), WEB.into()),
            ("Referer".into(), format!("{WEB}/")),
        ]);
        if let Some(csrf) = cookie_value(&session.cookie, "csrf_token") {
            headers.insert("x-csrf-token".into(), csrf);
        }
        let account_cookie_hash = format!("{:x}", md5::compute(&session.cookie));
        if let Some(context) = context.filter(|context| {
            context
                .get("account_cookie_hash")
                .map(|value| value.as_str() == Some(account_cookie_hash.as_str()))
                .unwrap_or_else(|| {
                    context
                        .pointer("/headers/Cookie")
                        .and_then(Value::as_str)
                        .is_some_and(|cookie| {
                            cookie_value(cookie, "__pus") == cookie_value(&session.cookie, "__pus")
                        })
                })
        }) {
            let saved: BTreeMap<String, String> =
                serde_json::from_value(context["headers"].clone())
                    .map_err(|_| QuarkError::InvalidResponse)?;
            let cookie = saved.get("Cookie").ok_or(QuarkError::InvalidResponse)?;
            if cookie.chars().any(char::is_control) {
                return Err(QuarkError::InvalidResponse);
            }
            // Restore the provider-issued snapshot, including explicit removals.
            // Never reintroduce account-cookie values absent from this context.
            headers.insert("Cookie".into(), cookie.clone());
            headers.remove("x-csrf-token");
            if let Some(csrf) = saved.get("x-csrf-token") {
                if csrf.is_empty() || csrf.chars().any(char::is_control) {
                    return Err(QuarkError::InvalidResponse);
                }
                headers.insert("x-csrf-token".into(), csrf.clone());
            }
        }
        Ok(UcShareSession {
            http: self.http,
            headers,
            account_cookie_hash,
            context_changed: false,
        })
    }
}
impl UcShareSession<'_> {
    fn context(&self, mut context: Value) -> Result<Value, QuarkError> {
        context["headers"] =
            serde_json::to_value(&self.headers).map_err(|_| QuarkError::InvalidResponse)?;
        context["account_cookie_hash"] = self.account_cookie_hash.clone().into();
        Ok(context)
    }
    fn updated<T>(
        &self,
        result: Result<T, QuarkError>,
        context: Value,
    ) -> Result<ShareOperationResult<T>, QuarkError> {
        Ok(ShareOperationResult {
            result,
            updated_context: if self.context_changed {
                Some(self.context(context)?)
            } else {
                None
            },
        })
    }
    fn request(
        &mut self,
        method: &'static str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, QuarkError> {
        if matches!(
            path,
            "share/sharepage/download" | "share/sharepage/save" | "file/download"
        ) {
            self.require_csrf()?;
        }
        let response = self.http.send(CloudHttpRequest {
            method,
            url: format!(
                "{API}/{path}{}pr=UCBrowser&fr=pc",
                if path.contains('?') { "&" } else { "?" }
            ),
            headers: self.headers.clone(),
            body: body.map(|v| v.to_string()),
        })?;
        if response.status == 401 {
            return Err(QuarkError::AuthorizationExpired);
        }
        if response.status == 412 {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        let value: Value =
            serde_json::from_str(&response.body).map_err(|_| QuarkError::InvalidResponse)?;
        let code = value["code"].as_i64().ok_or(QuarkError::InvalidResponse)?;
        for key in ["error_code", "status"] {
            if value.get(key).is_some_and(|v| v.as_i64().is_none()) {
                return Err(QuarkError::InvalidResponse);
            }
        }
        if value
            .get("error_code")
            .is_some_and(|v| v.as_i64() != Some(code))
        {
            return Err(QuarkError::InvalidResponse);
        }
        if value.get("error").is_some_and(|v| v.as_str().is_none()) {
            return Err(QuarkError::InvalidResponse);
        }
        if matches!(code, 9 | 4031) {
            return Err(QuarkError::WebAuthorizationRequired);
        }
        if code == 41008
            || (matches!(
                value["message"].as_str(),
                Some(
                    "提取码错误"
                        | "提取码不正确"
                        | "请输入提取码"
                        | "提取码不能为空"
                        | "需要提取码"
                )
            ) && code != 0)
        {
            return Err(QuarkError::InvalidInput("UC 分享访问码错误或缺失".into()));
        }
        if code == 41012 {
            return Err(QuarkError::Provider("UC 分享已失效或已停止分享".into()));
        }
        uc_json(&response)?;
        if value
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        {
            return Err(QuarkError::Provider("UC 分享请求失败".into()));
        }
        self.context_changed = true;
        self.merge_response_cookies(&response.set_cookies);
        // A newly returned JSON token wins over Set-Cookie, including deletion.
        if let Some(csrf) = value.pointer("/data/csrf_token") {
            let csrf = csrf
                .as_str()
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .ok_or(QuarkError::InvalidResponse)?;
            self.headers.insert("x-csrf-token".into(), csrf.into());
        }
        Ok(value)
    }
    fn merge_response_cookies(&mut self, issued: &[String]) {
        let mut cookies: Vec<(String, String)> = self.headers["Cookie"]
            .split(';')
            .filter_map(cookie_pair)
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        for raw in issued {
            let mut parts = raw.split(';');
            let Some((name, value)) = parts.next().unwrap_or("").trim().split_once('=') else {
                continue;
            };
            if name.is_empty()
                || value.chars().any(char::is_control)
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                continue;
            }
            let mut max_age = None;
            let mut expires = None;
            for attribute in parts {
                let Some((key, value)) = attribute.trim().split_once('=') else {
                    continue;
                };
                if key.eq_ignore_ascii_case("Max-Age") {
                    max_age = value.trim().parse::<i64>().ok();
                }
                if key.eq_ignore_ascii_case("Expires") {
                    expires = httpdate::parse_http_date(value.trim()).ok();
                }
            }
            let cleared = value.is_empty()
                || max_age.map(|age| age <= 0).unwrap_or_else(|| {
                    expires.is_some_and(|expiry| expiry <= std::time::SystemTime::now())
                });
            cookies.retain(|(current, _)| current != name);
            if !cleared {
                cookies.push((name.into(), value.into()));
            }
            if name == "csrf_token" {
                if cleared {
                    self.headers.remove("x-csrf-token");
                } else {
                    self.headers.insert("x-csrf-token".into(), value.into());
                }
            }
        }
        self.headers.insert(
            "Cookie".into(),
            cookies
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    fn token(&mut self, id: &str, code: Option<&str>) -> Result<String, QuarkError> {
        let value = self.request(
            "POST",
            "share/sharepage/token",
            Some(json!({"pwd_id":id,"passcode":code.unwrap_or("")})),
        )?;
        required(&value["data"], "stoken")
    }
    fn require_csrf(&self) -> Result<(), QuarkError> {
        if self
            .headers
            .get("x-csrf-token")
            .is_some_and(|s| !s.is_empty() && !s.chars().any(char::is_control))
        {
            Ok(())
        } else {
            Err(QuarkError::WebAuthorizationRequired)
        }
    }
}
impl ShareInspect for UcShareAdapter<'_> {
    fn inspect(
        &self,
        candidate: &crate::parser::CloudShareCandidate,
    ) -> Result<ShareInspection, QuarkError> {
        let id = super::share::share_id(candidate, CloudProvider::Uc)?;
        let mut session = self.session(None)?;
        let token = match session.token(&id, candidate.access_code.as_deref()) {
            Err(QuarkError::InvalidInput(_)) if candidate.access_code.is_none() => {
                return Ok(ShareInspection::AccessCodeRequired)
            }
            result => result?,
        };
        let mut context = json!({"stoken":token,"files":{}});
        let mut queue = std::collections::VecDeque::from([("0".to_owned(), Vec::<String>::new())]);
        let mut seen = std::collections::HashSet::new();
        let mut entries = vec![];
        let mut requests = 0;
        while let Some((parent, directory)) = queue.pop_front() {
            let mut count = 0u64;
            for page in 1..=10_000 {
                requests += 1;
                if requests > 10_000 || directory.len() > 128 {
                    return Err(QuarkError::InvalidResponse);
                }
                let value=session.request("GET",&format!("share/sharepage/detail?pwd_id={}&stoken={}&pdir_fid={}&_page={page}&_size=200&_fetch_total=1",encoded(&id),encoded(&token),encoded(&parent)),None)?;
                let files = value["data"]["list"]
                    .as_array()
                    .ok_or(QuarkError::InvalidResponse)?;
                count += files.len() as u64;
                for raw in files {
                    let file = parse_file(raw, &parent)?;
                    if !seen.insert(file.id.clone()) {
                        return Err(QuarkError::InvalidResponse);
                    }
                    if file.kind == "folder" {
                        let mut path = directory.clone();
                        path.push(file.name);
                        queue.push_back((file.id, path));
                    } else {
                        context["files"][&file.id] =
                            json!({"token":required(raw,"share_fid_token")?,"parent":parent});
                        entries.push(CloudShareEntry::new(
                            &id,
                            file.id,
                            file.name,
                            directory.clone(),
                            file.size,
                        ));
                    }
                }
                let more = value
                    .pointer("/metadata/_total")
                    .and_then(Value::as_u64)
                    .map(|total| count < total)
                    .unwrap_or(files.len() == 200);
                if !more {
                    break;
                }
                if files.is_empty() || page == 10_000 {
                    return Err(QuarkError::InvalidResponse);
                }
            }
        }
        // This internal context is serializable only through the encrypted plan store.
        context = session.context(context)?;
        Ok(ShareInspection::Plan(CloudSharePlan::new(
            CloudProvider::Uc,
            id,
            candidate.access_code.clone(),
            entries,
            context,
        )))
    }
}
impl ShareResolve for UcShareAdapter<'_> {
    fn resolve(&self, entry: &CloudShareResolvedEntry) -> Result<ShareResolution, QuarkError> {
        resolve_uc_share(self.credentials, self.http, entry)
    }
    fn resolve_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
    ) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
        resolve_uc_share_with_context(self.credentials, self.http, entry)
    }
}
impl ShareTransfer for UcShareAdapter<'_> {
    fn transfer_capacity(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<CloudTransferCapacity, QuarkError> {
        UcClient::new(self.stored_session()?, self.http).storage_capacity()
    }

    fn transfer_owner_fingerprint(
        &self,
        _: &CloudShareResolvedEntry,
    ) -> Result<Option<String>, QuarkError> {
        UcClient::new(self.stored_session()?, self.http)
            .stable_account_id()?
            .map(|id| super::share::stable_owner_fingerprint(CloudProvider::Uc, &id))
            .transpose()
    }
    fn transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
        self.transfer_with_context(entry, store)?.result
    }
    fn transfer_with_context(
        &self,
        entry: &CloudShareResolvedEntry,
        store: &CloudSharePlanStore,
    ) -> Result<ShareOperationResult<super::commands::ResolvedCloudDownload>, QuarkError> {
        if entry.provider != CloudProvider::Uc || entry.plan.provider != CloudProvider::Uc {
            return Err(QuarkError::InvalidResponse);
        }
        let owner = self.transfer_owner_fingerprint(entry)?;
        let mut session = self.session(Some(&entry.plan.provider_context))?;
        let mut context = entry.plan.provider_context.clone();
        let result = (|| {
            let token = session.token(&entry.plan.canonical_share_id, entry.access_code())?;
            context["stoken"] = token.clone().into();
            session.require_csrf()?;
            let file = &entry.plan.provider_context["files"][&entry.entry.entry_id];
            let value=session.request("POST","share/sharepage/save",Some(json!({"fid_list":[entry.entry.entry_id],"fid_token_list":[required(file,"token")?],"to_pdir_fid":"0","pwd_id":entry.plan.canonical_share_id,"stoken":token,"pdir_fid":required(file,"parent")?,"scene":"link","pdir_save_all":false,"exclude_fids":[]})))?;
            let task = required(&value["data"], "task_id")?;
            for retry in 0..20 {
                let value = session.request(
                    "GET",
                    &format!("task?task_id={}&retry_index={retry}", encoded(&task)),
                    None,
                )?;
                match value["data"]["status"].as_i64() {
                    Some(2) => {
                        let ids = value
                            .pointer("/data/save_as/save_as_top_fids")
                            .and_then(Value::as_array)
                            .filter(|v| v.len() == 1)
                            .ok_or(QuarkError::InvalidResponse)?;
                        let id = ids[0]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or(QuarkError::InvalidResponse)?;
                        if let Some(owner) = &owner {
                            store.record_transfer_with_owner(
                                &entry.plan.source_for(&entry.entry.entry_id),
                                id,
                                owner,
                            )?;
                        } else {
                            store.record_transfer(
                                &entry.plan.source_for(&entry.entry.entry_id),
                                id,
                            )?;
                        }
                        let headers = session.headers.clone();
                        let value =
                            session.request("POST", "file/download", Some(json!({"fids":[id]})))?;
                        let download = uc_share_download(&value, id, &entry.entry.name, headers)?;
                        return Ok(download);
                    }
                    Some(0 | 1) => std::thread::sleep(std::time::Duration::from_millis(250)),
                    _ => return Err(QuarkError::Provider("UC 分享转存失败".into())),
                }
            }
            Err(QuarkError::Provider("UC 分享转存尚未完成".into()))
        })();
        session.updated(result, context)
    }
}
impl RecordedTransferResolver for UcShareAdapter<'_> {
    fn refresh_recorded_transfer(
        &self,
        entry: &CloudShareResolvedEntry,
        created_file_id: &str,
    ) -> Result<ShareOperationResult<RecordedTransferDownload>, QuarkError> {
        if entry.provider != CloudProvider::Uc
            || entry.plan.provider != CloudProvider::Uc
            || created_file_id.is_empty()
        {
            return Err(QuarkError::InvalidResponse);
        }
        let mut session = self.session(Some(&entry.plan.provider_context))?;
        let context = entry.plan.provider_context.clone();
        let result = session
            .request(
                "POST",
                "file/download",
                Some(json!({"fids":[created_file_id]})),
            )
            .and_then(|value| {
                uc_share_download(
                    &value,
                    created_file_id,
                    &entry.entry.name,
                    session.headers.clone(),
                )
                .map(RecordedTransferDownload::Available)
            });
        session.updated(result, context)
    }
}
impl CloudShareAdapter for UcShareAdapter<'_> {
    fn transfer_capability(&self) -> Option<&dyn ShareTransfer> {
        Some(self)
    }
    fn recorded_capability(&self) -> Option<&dyn RecordedTransferResolver> {
        Some(self)
    }
}
fn uc_share_download(
    value: &Value,
    id: &str,
    name: &str,
    headers: BTreeMap<String, String>,
) -> Result<super::commands::ResolvedCloudDownload, QuarkError> {
    let files = value["data"]
        .as_array()
        .filter(|v| v.len() == 1)
        .ok_or(QuarkError::InvalidResponse)?;
    let file = &files[0];
    if file["fid"].as_str() != Some(id) {
        return Err(QuarkError::InvalidResponse);
    }
    let url = required(file, "download_url")?;
    super::share::validate_direct_url(&url)?;
    Ok(super::commands::ResolvedCloudDownload {
        url,
        name: name.into(),
        headers: headers.into_iter().collect(),
    })
}
pub fn resolve_uc_share(
    credentials: &dyn super::CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareResolution, QuarkError> {
    resolve_uc_share_with_context(credentials, http, entry)?.result
}
fn resolve_uc_share_with_context(
    credentials: &dyn super::CredentialStore,
    http: &dyn CloudHttpTransport,
    entry: &CloudShareResolvedEntry,
) -> Result<ShareOperationResult<ShareResolution>, QuarkError> {
    if entry.provider != CloudProvider::Uc || entry.plan.provider != CloudProvider::Uc {
        return Err(QuarkError::InvalidResponse);
    }
    let adapter = UcShareAdapter { credentials, http };
    let mut session = adapter.session(Some(&entry.plan.provider_context))?;
    let mut context = entry.plan.provider_context.clone();
    let result = (|| {
        let token = session.token(&entry.plan.canonical_share_id, entry.access_code())?;
        context["stoken"] = token.clone().into();
        session.require_csrf()?;
        let headers = session.headers.clone();
        let value=session.request("POST","share/sharepage/download",Some(json!({"pwd_id":entry.plan.canonical_share_id,"stoken":token,"fids":[entry.entry.entry_id],"fids_token":[required(&entry.plan.provider_context["files"][&entry.entry.entry_id],"token")?]})))?;
        if value["data"].as_array().is_some_and(|v| v.is_empty()) {
            return Ok(ShareResolution::TransferRequired(TransferRequest {
                source: entry.plan.source_for(&entry.entry.entry_id),
            }));
        }
        Ok(ShareResolution::Direct(uc_share_download(
            &value,
            &entry.entry.entry_id,
            &entry.entry.name,
            headers,
        )?))
    })();
    session.updated(result, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::share::{
        test_support::{entry, Http},
        *,
    };
    use crate::cloud::{CredentialStore, MemoryCredentialStore};

    fn share_credentials() -> MemoryCredentialStore {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "uc",
                &serde_json::to_vec(&UcSession::cookie(
                    "__uid=account-7; __pus=identity; __puus=session; csrf_token=csrf-secret",
                ))
                .unwrap(),
            )
            .unwrap();
        credentials
    }
    fn share_credentials_with_server_session_only() -> MemoryCredentialStore {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "uc",
                &serde_json::to_vec(&UcSession::cookie("__puus=session; csrf_token=csrf-secret"))
                    .unwrap(),
            )
            .unwrap();
        credentials
    }

    #[test]
    fn uc_personal_download_reports_a_missing_server_file() {
        let http = Http::new(vec![json!({
            "code": 21001,
            "status": 404,
            "message": "file not found [stale-file]",
        })]);
        let error = UcClient::new(UcSession::cookie("__puus=session"), &http)
            .download("stale-file")
            .unwrap_err();

        assert!(matches!(error, QuarkError::SourceFileMissing));
    }

    #[test]
    fn uc_personal_download_replays_the_rotated_csrf_cookie_to_the_cdn() {
        let http = ShareReplies::new(vec![(
            json!({"code":0,"data":[{"fid":"file-1","file_name":"fixture.txt","download_url":"https://cdn.example/fixture"}]}),
            vec![
                "csrf_token=fresh-csrf; Path=/; Secure",
                "UC_DOWNLOAD_SESSION=issued; Path=/; Secure",
            ],
        )]);
        let direct = UcClient::new(
            UcSession::cookie("__puus=session; csrf_token=stale-csrf"),
            &http,
        )
        .download("file-1")
        .unwrap();

        assert!(direct
            .headers
            .get("Cookie")
            .is_some_and(|cookie| cookie.contains("csrf_token=fresh-csrf")));
        assert!(direct
            .headers
            .get("Cookie")
            .is_some_and(|cookie| cookie.contains("UC_DOWNLOAD_SESSION=issued")));
        assert_eq!(
            direct.headers.get("x-csrf-token").map(String::as_str),
            Some("fresh-csrf")
        );
        assert_eq!(
            direct.headers.get("User-Agent").map(String::as_str),
            Some(UC_DOWNLOAD_USER_AGENT)
        );
        assert!(!direct.headers.contains_key("Content-Type"));
    }

    #[test]
    fn uc_safe_read_retries_once_with_the_rotated_server_session() {
        let http = ShareReplies::new(vec![
            (
                json!({"code":0,"status":200,"data":{"list":[]},"metadata":{"_total":0}}),
                vec!["__puus=fresh-session; Path=/; Secure"],
            ),
            (
                json!({"code":0,"status":200,"data":{"list":[{"fid":"file-1","file_name":"fixture.txt","dir":false}]},"metadata":{"_total":1}}),
                vec![],
            ),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=stale-session"), &http);

        let directory = client.directory("0").unwrap();

        assert_eq!(directory.items[0].id, "file-1");
        assert!(client.session().cookie.contains("__puus=fresh-session"));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].headers["Cookie"].contains("__puus=stale-session"));
        assert!(requests[1].headers["Cookie"].contains("__puus=fresh-session"));
    }

    #[test]
    fn uc_download_retries_a_false_missing_response_after_session_rotation() {
        let http = ShareReplies::new(vec![
            (
                json!({"code":21001,"status":404,"message":"file not found"}),
                vec!["__puus=fresh-session; Path=/; Secure"],
            ),
            (
                json!({"code":0,"status":200,"data":[{"fid":"file-1","file_name":"fixture.txt","download_url":"https://cdn.example/fixture"}]}),
                vec![],
            ),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=stale-session"), &http);

        let download = client.download("file-1").unwrap();

        assert_eq!(download.name, "fixture.txt");
        assert!(client.session().cookie.contains("__puus=fresh-session"));
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].headers["Cookie"].contains("__puus=stale-session"));
        assert!(requests[1].headers["Cookie"].contains("__puus=fresh-session"));
    }

    #[test]
    fn uc_web_session_keeps_the_csrf_token_needed_for_share_download_signing() {
        // Break caught: accepting an official web session for directory reads
        // while dropping its csrf cookie makes a subsequently parsed share
        // fail only at the signed-download endpoint.
        let session = parse_uc_web_session(&[
            "__uid=account-7".into(),
            "__puus=web-session".into(),
            "csrf_token=web-csrf".into(),
        ])
        .expect("the browser session should be accepted");
        let credentials = MemoryCredentialStore::default();
        credentials
            .save("uc", &serde_json::to_vec(&session).unwrap())
            .unwrap();
        let adapter = UcShareAdapter {
            credentials: &credentials,
            http: &Http::new(vec![]),
        };

        let signing = adapter.session(None).expect("a share signing session");

        assert_eq!(
            signing.headers.get("x-csrf-token").map(String::as_str),
            Some("web-csrf")
        );
    }

    #[test]
    fn uc_web_session_retains_the_share_scoped_browser_cookies() {
        let session = parse_uc_web_session(&[
            "__puus=web-session".into(),
            "__ktd=browser-state".into(),
            "b-user-id=account-id".into(),
            "UDRIVE_TRANSFER_SESS=share-session".into(),
            "analytics=never-persisted".into(),
        ])
        .expect("the browser session should be accepted");

        assert!(session.cookie.contains("__ktd=browser-state"));
        assert!(session.cookie.contains("b-user-id=account-id"));
        assert!(session
            .cookie
            .contains("UDRIVE_TRANSFER_SESS=share-session"));
        assert!(!session.cookie.contains("analytics="));
    }

    #[test]
    fn uc_legacy_session_share_rejection_requests_web_reauthorization() {
        let http = Http::new(vec![json!({
            "code": 41026,
            "message": "request rejected",
        })]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        let error = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: None,
                },
            )
            .unwrap_err();

        assert!(matches!(error, QuarkError::WebAuthorizationRequired));
    }

    #[test]
    fn uc_web_account_never_displays_an_opaque_cookie_identifier() {
        // Break caught: __uid is an opaque browser-session identifier, not a
        // user-visible UC account name. Older builds persisted it as the card
        // label when the profile endpoint returned no nickname.
        let mut session = UcSession::cookie("__uid=AAR/opaque-id; __puus=session");
        session.display_name = Some("AAR/opaque-id".into());
        let http = Http::new(vec![]);
        let client = UcClient::new(session, &http);

        let account = client.web_account().expect("a UC card account");

        assert_eq!(account.id, "AAR/opaque-id");
        assert_eq!(account.display_name, "UC网盘");
    }

    #[test]
    fn uc_account_restores_the_nickname_captured_from_the_browser() {
        let mut session = UcSession::cookie("__uid=AAR/opaque-id; __puus=session");
        session.display_name = Some("云端小橙".into());
        let http = Http::new(vec![json!({
            "code": 0,
            "data": {"user_id": "uc-user-42"}
        })]);
        let client = UcClient::new(session, &http);

        let account = client
            .account()
            .expect("the saved card profile should load");

        assert_eq!(account.id, "uc-user-42");
        assert_eq!(account.display_name, "云端小橙");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.contains("/member?"));
    }

    #[test]
    fn uc_web_account_uses_persisted_member_identity_without_replaying_a_profile_request() {
        let mut session = UcSession::cookie("__uid=AAR/rotated-session; __puus=session");
        session.account_id = Some("uc-user-42".into());
        session.display_name = Some("云端小橙".into());
        let http = Http::new(vec![]);
        let client = UcClient::new(session, &http);

        let account = client.web_account().expect("a restored UC card account");

        assert_eq!(account.id, "uc-user-42");
        assert_eq!(account.display_name, "云端小橙");
        assert!(http.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn uc_account_uses_b_user_id_when_the_member_profile_omits_an_identity() {
        let session =
            UcSession::cookie("__uid=AAR/rotated-session; b-user-id=uc-user-42; __puus=session");
        let http = Http::new(vec![json!({"code": 0, "data": {}})]);
        let client = UcClient::new(session, &http);

        let account = client.account().expect("a stable UC account identity");

        assert_eq!(account.id, "uc-user-42");
        assert_eq!(account.display_name, "UC网盘");
    }

    #[test]
    fn uc_live_browser_identity_overrides_a_persisted_previous_account() {
        let mut session =
            UcSession::cookie("__uid=AAR/current-session; b-user-id=current-user; __puus=session");
        session.account_id = Some("previous-user".into());
        let http = Http::new(vec![json!({"code": 0, "data": {}})]);
        let client = UcClient::new(session, &http);

        let account_id = client
            .verified_account_id()
            .expect("the live UC identity should be readable");

        assert_eq!(account_id.as_deref(), Some("current-user"));
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn uc_persisted_identity_is_not_accepted_as_a_live_download_owner() {
        let mut session = UcSession::cookie("__uid=AAR/current-session; __puus=session");
        session.account_id = Some("previous-user".into());
        let http = Http::new(vec![json!({"code": 0, "data": {}})]);
        let client = UcClient::new(session, &http);

        assert_eq!(client.verified_account_id().unwrap(), None);
    }

    #[test]
    fn uc_personal_share_retries_a_transient_task_poll_transport_failure() {
        struct RetryHttp {
            replies:
                std::sync::Mutex<std::collections::VecDeque<Result<CloudHttpResponse, QuarkError>>>,
            requests: std::sync::Mutex<Vec<CloudHttpRequest>>,
        }
        impl CloudHttpTransport for RetryHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.requests.lock().unwrap().push(request);
                self.replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request")
            }
        }
        let response = |body: Value| CloudHttpResponse {
            status: 200,
            body: body.to_string(),
            set_cookies: vec![],
        };
        let http = RetryHttp {
            replies: std::sync::Mutex::new(
                vec![
                    Ok(response(json!({"code": 0, "data": {"task_id": "task-1"}}))),
                    Err(QuarkError::Transport("HTTPS 请求失败".into())),
                    Ok(response(json!({"code": 0, "data": {"status": 2, "share_id": "share-1"}}))),
                    Ok(response(json!({"code": 0, "data": {"share_id": "share-1", "share_url": "https://drive.uc.cn/s/share-1", "title": "报告.pdf", "file_type": 1}}))),
                ]
                .into(),
            ),
            requests: std::sync::Mutex::new(vec![]),
        };
        let client = UcClient::new(
            UcSession::cookie("__puus=session; UDRIVE_TRANSFER_SESS=share-session"),
            &http,
        );

        let share = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: None,
                },
            )
            .unwrap();

        assert_eq!(share.id, "share-1");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].url.contains("/task?task_id=task-1"));
        assert!(requests[2].url.contains("/task?task_id=task-1"));
    }

    #[test]
    fn uc_personal_share_retries_a_transient_share_link_lookup_transport_failure() {
        struct RetryHttp {
            replies:
                std::sync::Mutex<std::collections::VecDeque<Result<CloudHttpResponse, QuarkError>>>,
        }
        impl CloudHttpTransport for RetryHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                self.replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request")
            }
        }
        let response = |body: Value| CloudHttpResponse {
            status: 200,
            body: body.to_string(),
            set_cookies: vec![],
        };
        let http = RetryHttp {
            replies: std::sync::Mutex::new(
                vec![
                    Ok(response(json!({"code": 0, "data": {"task_id": "task-1"}}))),
                    Ok(response(json!({"code": 0, "data": {"status": 2, "share_id": "share-1"}}))),
                    Err(QuarkError::Transport("HTTPS 请求失败".into())),
                    Ok(response(json!({"code": 0, "data": {"share_id": "share-1", "share_url": "https://drive.uc.cn/s/share-1", "title": "报告.pdf", "file_type": 1}}))),
                ]
                .into(),
            ),
        };
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        let share = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: None,
                },
            )
            .unwrap();

        assert_eq!(share.url, "https://drive.uc.cn/s/share-1");
    }

    #[test]
    fn uc_personal_share_keeps_the_task_share_id_when_password_details_only_expose_pwd_id() {
        // The create task's `share_id` is the opaque ID required by the list
        // and delete APIs. `share/password` can return only a different
        // password-record ID (`pwd_id`), which must not replace it.
        let http = Http::new(vec![
            json!({"code": 0, "data": {"share_id": "share-1"}}),
            json!({"code": 0, "data": {"pwd_id": "pwd-1", "share_url": "https://drive.uc.cn/s/share-1", "title": "报告.pdf", "file_type": 1}}),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        let share = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: None,
                },
            )
            .unwrap();

        assert_eq!(share.id, "share-1");
        assert_eq!(share.url, "https://drive.uc.cn/s/share-1");
    }

    #[test]
    fn uc_personal_share_cancel_waits_for_the_server_list_to_exclude_the_target() {
        // Break caught: acknowledging `share/delete` before `mypage/detail`
        // reflects it lets the UI faithfully reload an obsolete server list.
        let http = Http::new(vec![
            json!({"code": 0, "data": null}),
            json!({"code": 0, "data": {"list": [{"share_id": "share-1", "title": "报告.pdf", "file_type": 1}]}}),
            json!({"code": 0, "data": {"list": []}}),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        client.cancel_shares(&["share-1".into()]).unwrap();

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].url.contains("/share/delete?"));
        assert!(requests[1].url.contains("/share/mypage/detail?"));
        assert!(requests[2].url.contains("/share/mypage/detail?"));
    }

    #[test]
    fn uc_personal_share_cancel_confirms_server_removal_when_delete_reports_it_is_already_stopped()
    {
        // Break caught: UC can return 41012 for an already-stopped share while
        // its list endpoint still has not converged. The list is authoritative.
        let http = Http::new(vec![
            json!({"code": 41012, "message": "分享已失效或已停止分享"}),
            json!({"code": 0, "data": {"list": [{"share_id": "share-1", "title": "报告.pdf", "file_type": 1}]}}),
            json!({"code": 0, "data": {"list": []}}),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        client.cancel_shares(&["share-1".into()]).unwrap();

        assert_eq!(http.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn uc_personal_share_cancel_submits_all_selected_share_ids_in_one_request() {
        // UC's delete endpoint accepts `share_ids`, not a singular `share_id`.
        // The latter returns a misleading success response but does not cancel
        // the selected share on the server.
        let http = Http::new(vec![
            json!({"code": 0, "data": null}),
            json!({"code": 0, "data": {"list": [
                {"share_id": "share-1", "title": "报告 1.pdf", "file_type": 1},
                {"share_id": "share-2", "title": "报告 2.pdf", "file_type": 1}
            ]}}),
            json!({"code": 0, "data": {"list": []}}),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        client
            .cancel_shares(&["share-1".into(), "share-2".into()])
            .unwrap();

        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].body.as_deref().unwrap()).unwrap(),
            json!({"share_ids": ["share-1", "share-2"]}),
        );
    }

    #[test]
    fn uc_personal_share_uses_uc_expiry_enum_and_omits_unset_fields() {
        let http = Http::new(vec![
            json!({"code": 0, "data": {"task_id": "task-1"}}),
            json!({"code": 0, "data": {"status": 2, "share_id": "share-1"}}),
            json!({"code": 0, "data": {"share_id": "share-1", "share_url": "https://drive.uc.cn/s/share-1", "title": "报告.pdf", "file_type": 1}}),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: Some(7),
                },
            )
            .unwrap();

        let requests = http.requests.lock().unwrap();
        let body: Value = serde_json::from_str(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "fid_list": ["file-1"],
                "title": "报告.pdf",
                "url_type": 1,
                "expired_type": 3,
                "dl_limit": -1,
            })
        );
        assert_eq!(
            requests[0].headers.get("Accept").map(String::as_str),
            Some("application/json, text/plain, */*")
        );
    }

    #[test]
    fn uc_personal_share_request_uses_uc_web_client_profile() {
        let http = Http::new(vec![json!({
            "code": 41026,
            "message": "request rejected",
        })]);
        let client = UcClient::new(
            UcSession::cookie("__puus=session; UDRIVE_TRANSFER_SESS=share-session"),
            &http,
        );

        let error = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: None,
                },
            )
            .unwrap_err();

        assert!(matches!(error, QuarkError::Provider(_)));
        let request = &http.requests.lock().unwrap()[0];
        assert!(request.url.ends_with("/share?pr=UCBrowser&fr=pc"));
        assert_eq!(
            request.headers.get("Accept-Language").map(String::as_str),
            Some("zh-CN,zh;q=0.9")
        );
        assert_eq!(
            request.headers.get("Content-Type").map(String::as_str),
            Some("application/json;charset=UTF-8")
        );
        assert_eq!(
            request.headers.get("Referer").map(String::as_str),
            Some("https://drive.uc.cn/")
        );
    }

    #[test]
    fn uc_personal_share_sends_the_browser_csrf_token_for_its_create_request() {
        let http = Http::new(vec![json!({
            "code": 41026,
            "message": "request rejected",
        })]);
        let client = UcClient::new(
            UcSession::cookie("__puus=session; csrf_token=csrf-for-share"),
            &http,
        );

        let _ = client.create_share(
            &["file-1".into()],
            &CloudShareOptions {
                name: "报告.pdf".into(),
                pass_code: None,
                generate_pass_code: false,
                expires_in_days: None,
            },
        );

        assert_eq!(
            http.requests.lock().unwrap()[0]
                .headers
                .get("x-csrf-token")
                .map(String::as_str),
            Some("csrf-for-share")
        );
    }

    #[test]
    fn uc_personal_share_accepts_task_id_nested_in_task_response() {
        let http = Http::new(vec![
            json!({"code": 0, "data": {"task_resp": {"data": {"task_id": "task-1"}}}}),
            json!({"code": 0, "data": {"status": 2, "share_id": "share-1"}}),
            json!({"code": 0, "data": {"share_id": "share-1", "share_url": "https://drive.uc.cn/s/share-1", "title": "报告.pdf", "file_type": 1}}),
        ]);
        let client = UcClient::new(UcSession::cookie("__puus=session"), &http);

        let share = client
            .create_share(
                &["file-1".into()],
                &CloudShareOptions {
                    name: "报告.pdf".into(),
                    pass_code: None,
                    generate_pass_code: false,
                    expires_in_days: None,
                },
            )
            .unwrap();

        assert_eq!(share.id, "share-1");
    }

    #[test]
    fn uc_rejection_reports_the_provider_code_and_message() {
        let error = uc_json(&CloudHttpResponse {
            status: 200,
            body: json!({"code": 41010, "message": "有效期不受支持"}).to_string(),
            set_cookies: vec![],
        })
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "网盘请求失败：UC 网盘拒绝请求（错误码 41010）"
        );
    }

    #[test]
    fn uc_share_expiry_types_match_uc_server_enums() {
        for (days, expected) in [
            (None, 1),
            (Some(1), 2),
            (Some(7), 3),
            (Some(30), 4),
            (Some(60), 5),
            (Some(180), 7),
        ] {
            assert_eq!(uc_share_expiry_type(days).unwrap(), expected);
        }
        assert!(matches!(
            uc_share_expiry_type(Some(2)),
            Err(QuarkError::InvalidInput(_))
        ));
    }

    fn share_entry() -> CloudShareResolvedEntry {
        entry(CloudSharePlan::new(
            CloudProvider::Uc,
            "share123".into(),
            Some("a123".into()),
            vec![CloudShareEntry::new(
                "share123",
                "f1".into(),
                "file.txt".into(),
                vec![],
                Some(7),
            )],
            json!({"stoken":"old-token","files":{"f1":{"token":"file-token","parent":"0"}}}),
        ))
    }

    #[test]
    fn uc_share_transfer_capacity_uses_member_bytes() {
        let credentials = share_credentials();
        let http = Http::new(vec![json!({
            "code": 0, "status": 200,
            "data": {"total_capacity": 1000, "use_capacity": "250"}
        })]);
        let adapter = UcShareAdapter {
            credentials: &credentials,
            http: &http,
        };
        assert_eq!(
            adapter.transfer_capacity(&share_entry()).unwrap(),
            CloudTransferCapacity::Limited {
                total_bytes: 1000,
                used_bytes: 250
            }
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.contains("/member?fetch_subscribe=true"));
        assert!(!requests[0].url.contains("sharepage/save"));
        drop(requests);

        let invalid_uc = Http::new(vec![json!({
            "code": 0, "status": 200,
            "data": {"total_capacity": "unknown", "use_capacity": 1}
        })]);
        assert!(UcShareAdapter {
            credentials: &credentials,
            http: &invalid_uc
        }
        .transfer_capacity(&share_entry())
        .is_err());
    }

    #[test]
    fn uc_share_access_code_and_cancelled_share_are_user_visible() {
        let credentials = share_credentials();
        let missing_code = Http::new(vec![json!({"code":41008,"message":"需要提取码"})]);
        let mut without_code = crate::parser::CloudShareCandidate {
            provider: CloudProvider::Uc,
            url: "https://drive.uc.cn/s/share123".into(),
            access_code: Some("placeholder".into()),
        };
        without_code.access_code = None;
        assert!(matches!(
            provider_share_adapter(CloudProvider::Uc, &credentials, &missing_code)
                .unwrap()
                .inspect(&without_code),
            Ok(ShareInspection::AccessCodeRequired)
        ));

        let cancelled = Http::new(vec![json!({"code":41012,"message":"好友已取消了分享"})]);
        let Err(error) = provider_share_adapter(CloudProvider::Uc, &credentials, &cancelled)
            .unwrap()
            .inspect(&without_code)
        else {
            panic!("a cancelled share must be rejected");
        };
        assert!(error.to_string().contains("UC 分享已失效或已停止分享"));
    }

    #[test]
    fn uc_share_server_session_cookie_alone_inspects_and_resolves_direct_download() {
        let credentials = share_credentials_with_server_session_only();
        let token = json!({"code":0,"data":{"stoken":"fresh"}});
        let http = Http::new(vec![
            token.clone(),
            json!({"code":0,"data":{"list":[{"fid":"f1","file_name":"one","dir":false,"share_fid_token":"file-token"}]},"metadata":{"_total":1}}),
            token,
            json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/file"}]}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let candidate = crate::parser::CloudShareCandidate {
            provider: CloudProvider::Uc,
            url: "https://drive.uc.cn/s/share123".into(),
            access_code: Some("a123".into()),
        };

        let ShareInspection::Plan(plan) = adapter.inspect(&candidate).unwrap() else {
            panic!("share inspection must produce a plan")
        };
        let selected = entry(plan);
        let ShareResolution::Direct(download) = adapter.resolve(&selected).unwrap() else {
            panic!("share must resolve its original direct download")
        };

        assert_eq!(download.url, "https://cdn.example/file");
        for request in http.requests.lock().unwrap().iter() {
            assert!(request.headers["Cookie"].contains("__puus=session"));
            assert!(!request.headers["Cookie"].contains("__pus="));
        }
    }
    #[test]
    fn uc_recorded_transfer_refreshes_personal_file_without_share_save() {
        let credentials = share_credentials();
        let http = Http::new(vec![json!({
            "code": 0,
            "data": [{"fid":"personal-id","download_url":"https://cdn.example/personal"}]
        })]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();

        let refreshed = adapter
            .refresh_recorded_transfer(&share_entry(), "personal-id")
            .unwrap();

        let Ok(RecordedTransferDownload::Available(download)) = refreshed.result else {
            panic!("a personal direct link is required")
        };
        assert_eq!(download.url, "https://cdn.example/personal");
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.contains("file/download"));
        assert!(!requests
            .iter()
            .any(|request| request.url.contains("share/sharepage/save")));
    }
    #[test]
    fn uc_transfer_owner_uses_uid_not_rotating_session_cookie() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "uc",
                &serde_json::to_vec(&UcSession::cookie(
                    "__uid=account-7; __puus=before; csrf_token=csrf",
                ))
                .unwrap(),
            )
            .unwrap();
        let http = Http::new(vec![]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();

        let before = adapter.transfer_owner_fingerprint(&share_entry()).unwrap();
        credentials
            .save(
                "uc",
                &serde_json::to_vec(&UcSession::cookie(
                    "__uid=account-7; __puus=renewed; csrf_token=csrf",
                ))
                .unwrap(),
            )
            .unwrap();
        let after = adapter.transfer_owner_fingerprint(&share_entry()).unwrap();

        assert_eq!(before, after);
    }
    #[test]
    fn uc_transfer_owner_without_uid_or_authenticated_account_id_is_unavailable() {
        let credentials = share_credentials_with_server_session_only();
        let http = Http::new(vec![json!({"code":0,"data":{}})]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();

        assert_eq!(
            adapter.transfer_owner_fingerprint(&share_entry()).unwrap(),
            None
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.contains("/member?"));
    }
    struct ShareReplies {
        requests: std::sync::Mutex<Vec<CloudHttpRequest>>,
        replies: std::sync::Mutex<std::collections::VecDeque<CloudHttpResponse>>,
    }
    impl ShareReplies {
        fn new(replies: Vec<(Value, Vec<&str>)>) -> Self {
            Self {
                requests: std::sync::Mutex::new(vec![]),
                replies: std::sync::Mutex::new(
                    replies
                        .into_iter()
                        .map(|(value, cookies)| CloudHttpResponse {
                            status: 200,
                            body: value.to_string(),
                            set_cookies: cookies.into_iter().map(str::to_owned).collect(),
                        })
                        .collect(),
                ),
            }
        }
    }
    impl CloudHttpTransport for ShareReplies {
        fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(QuarkError::InvalidResponse)
        }
    }
    #[test]
    fn uc_share_inspection_cookie_survives_resolve_and_transfer_without_reissue() {
        let credentials = share_credentials();
        let token = json!({"code":0,"data":{"stoken":"fresh"}});
        let http = ShareReplies::new(vec![
            (token.clone(), vec!["SESSION=issued; Path=/; Secure"]),
            (
                json!({"code":0,"data":{"list":[{"fid":"f1","file_name":"one","dir":false,"share_fid_token":"file-token"}]},"metadata":{"_total":1}}),
                vec![],
            ),
            (token.clone(), vec![]),
            (
                json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/f"}]}),
                vec![],
            ),
            (token, vec![]),
            (json!({"code":0,"data":{"task_id":"t"}}), vec![]),
            (
                json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal-id"]}}}),
                vec![],
            ),
            (
                json!({"code":0,"data":[{"fid":"personal-id","download_url":"https://cdn.example/personal"}]}),
                vec![],
            ),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let candidate = crate::parser::CloudShareCandidate {
            provider: CloudProvider::Uc,
            url: "https://drive.uc.cn/s/share123".into(),
            access_code: Some("a123".into()),
        };
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate).unwrap() else {
            panic!()
        };
        let directory = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
        );
        store.upsert(&plan).unwrap();
        let selected = store.resolve_entry(&plan.source_for("f1")).unwrap();
        let ShareResolution::Direct(direct) = adapter.resolve(&selected).unwrap() else {
            panic!()
        };
        let cookie = direct
            .headers
            .iter()
            .find(|(key, _)| key == "Cookie")
            .unwrap();
        assert!(cookie.1.contains("SESSION=issued"));
        let transferred = adapter.transfer(&selected, &store).unwrap();
        assert!(transferred
            .headers
            .iter()
            .any(|(key, value)| key == "Cookie" && value.contains("SESSION=issued")));
        for request in http.requests.lock().unwrap().iter().skip(1) {
            assert!(request.headers["Cookie"].contains("SESSION=issued"));
        }
    }
    #[test]
    fn uc_share_resolve_cookie_update_survives_reopening_the_dpapi_plan() {
        let credentials = share_credentials();
        let token = json!({"code":0,"data":{"stoken":"fresh"}});
        let http = ShareReplies::new(vec![
            (token.clone(), vec!["SESSION=resolve-issued; Path=/"]),
            (
                json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/first"}]}),
                vec![],
            ),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let source = share_entry().plan.source_for("f1");
        {
            let store = CloudSharePlanStore::new(
                crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
            );
            store.upsert(&share_entry().plan).unwrap();
            let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
            resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();
        }
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
        );
        let http = ShareReplies::new(vec![
            (token, vec![]),
            (
                json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/second"}]}),
                vec![],
            ),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let direct = resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();
        assert!(direct
            .headers
            .iter()
            .any(|(key, value)| key == "Cookie" && value.contains("SESSION=resolve-issued")));
        assert!(
            http.requests.lock().unwrap()[0].headers["Cookie"].contains("SESSION=resolve-issued")
        );
    }
    #[test]
    fn uc_share_cleared_or_expired_csrf_blocks_download_and_save() {
        let credentials = share_credentials();
        for cookie in [
            "csrf_token=; Max-Age=0; Path=/",
            "csrf_token=still-present; Max-Age=-1; Path=/",
            "csrf_token=still-present; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path=/",
        ] {
            for transfer in [false, true] {
                let http = ShareReplies::new(vec![(
                    json!({"code":0,"data":{"stoken":"fresh"}}),
                    vec![cookie],
                )]);
                let adapter =
                    provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
                let directory = tempfile::tempdir().unwrap();
                let store = CloudSharePlanStore::new(
                    crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
                );
                let selected = share_entry();
                store.upsert(&selected.plan).unwrap();
                let result = if transfer {
                    adapter.transfer(&selected, &store).map(|_| ())
                } else {
                    adapter.resolve(&selected).map(|_| ())
                };
                assert!(matches!(result, Err(QuarkError::WebAuthorizationRequired)));
                assert_eq!(http.requests.lock().unwrap().len(), 1);
            }
        }
    }
    #[test]
    fn uc_share_revoked_csrf_is_persisted_on_error_and_never_resurrected() {
        let credentials = share_credentials();
        for during_transfer in [false, true] {
            let token = json!({"code":0,"data":{"stoken":"fresh"}});
            let mut replies = vec![];
            if during_transfer {
                replies.extend([
                    (token.clone(), vec![]),
                    (json!({"code":0,"data":[]}), vec![]),
                ]);
            }
            replies.push((token.clone(), vec!["csrf_token=; Max-Age=0; Path=/"]));
            let http = ShareReplies::new(replies);
            let directory = tempfile::tempdir().unwrap();
            let store = CloudSharePlanStore::new(
                crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
            );
            let mut selected = share_entry();
            selected.plan.provider_context["headers"] = json!({"Cookie":"__pus=identity; __puus=session; csrf_token=csrf-secret","x-csrf-token":"csrf-secret"});
            store.upsert(&selected.plan).unwrap();
            let source = selected.plan.source_for("f1");
            let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
            assert!(matches!(
                resolve_current_share_download(&store, adapter.as_ref(), &source),
                Err(QuarkError::WebAuthorizationRequired)
            ));
            let updated = store.resolve_entry(&source).unwrap();
            assert!(updated.plan.provider_context["headers"]
                .get("x-csrf-token")
                .is_none());
            assert!(!updated.plan.provider_context["headers"]["Cookie"]
                .as_str()
                .unwrap()
                .contains("csrf_token="));
            assert!(!http
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.url.contains("share/sharepage/save")));
            assert_eq!(
                http.requests.lock().unwrap().len(),
                if during_transfer { 3 } else { 1 }
            );
            drop(store);
            let reopened = CloudSharePlanStore::new(
                crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
            );
            let second = ShareReplies::new(vec![(token, vec![])]);
            let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &second).unwrap();
            assert!(matches!(
                resolve_current_share_download(&reopened, adapter.as_ref(), &source),
                Err(QuarkError::WebAuthorizationRequired)
            ));
            let requests = second.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(!requests[0].headers.contains_key("x-csrf-token"));
            assert!(!requests[0].headers["Cookie"].contains("csrf_token="));
        }
    }
    #[test]
    fn uc_share_json_csrf_replaces_same_response_cookie_deletion() {
        let credentials = share_credentials();
        let http = ShareReplies::new(vec![
            (
                json!({"code":0,"data":{"stoken":"fresh","csrf_token":"json-replacement"}}),
                vec!["csrf_token=; Max-Age=0; Path=/"],
            ),
            (
                json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/f"}]}),
                vec![],
            ),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let ShareResolution::Direct(direct) = adapter.resolve(&share_entry()).unwrap() else {
            panic!()
        };
        assert!(direct
            .headers
            .contains(&("x-csrf-token".into(), "json-replacement".into())));
        assert!(!http.requests.lock().unwrap()[1].headers["Cookie"].contains("csrf_token="));
    }
    #[test]
    fn uc_share_transfer_receives_resolve_context_and_persists_its_cookie_rotation() {
        let credentials = share_credentials();
        let token = json!({"code":0,"data":{"stoken":"fresh"}});
        let http = ShareReplies::new(vec![
            (token.clone(), vec!["SESSION=resolve-issued; Path=/"]),
            (json!({"code":0,"data":[]}), vec![]),
            (token, vec![]),
            (
                json!({"code":0,"data":{"task_id":"t"}}),
                vec!["SESSION=transfer-issued; Path=/"],
            ),
            (
                json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal-id"]}}}),
                vec![],
            ),
            (
                json!({"code":0,"data":[{"fid":"personal-id","download_url":"https://cdn.example/personal"}]}),
                vec![],
            ),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
        );
        let selected = share_entry();
        store.upsert(&selected.plan).unwrap();
        let source = selected.plan.source_for("f1");
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let direct = resolve_current_share_download(&store, adapter.as_ref(), &source).unwrap();
        assert!(direct
            .headers
            .iter()
            .any(|(key, value)| key == "Cookie" && value.contains("SESSION=transfer-issued")));
        assert!(
            http.requests.lock().unwrap()[2].headers["Cookie"].contains("SESSION=resolve-issued")
        );
        let updated = store.resolve_entry(&source).unwrap();
        assert!(updated.plan.provider_context["headers"]["Cookie"]
            .as_str()
            .unwrap()
            .contains("SESSION=transfer-issued"));
        assert_eq!(updated.plan.transfers[0].created_file_id, "personal-id");
    }
    #[test]
    fn uc_share_direct_url_keeps_identity_headers_out_of_source() {
        let credentials = share_credentials();
        let http = Http::new(vec![
            json!({"code":0,"data":{"stoken":"fresh-token"}}),
            json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/file?signature=secret"}]}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http)
            .expect("UC share adapter registered");
        let entry = share_entry();
        let ShareResolution::Direct(direct) = adapter.resolve(&entry).unwrap() else {
            panic!("direct URL expected")
        };
        for (name, value) in [
            (
                "Cookie",
                "__uid=account-7; __pus=identity; __puus=session; csrf_token=csrf-secret; SESSION=issued",
            ),
            ("x-csrf-token", "csrf-secret"),
            ("User-Agent", UC_DESKTOP_USER_AGENT),
            ("Origin", WEB),
            ("Referer", "https://drive.uc.cn/"),
        ] {
            assert!(direct
                .headers
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case(name) && v == value));
        }
        assert_eq!(direct.url, "https://cdn.example/file?signature=secret");
        for secret in [
            "cookie",
            "csrf-secret",
            "fresh-token",
            "signature",
            "share123",
        ] {
            assert!(!entry.plan.source_for("f1").contains(secret));
        }
    }
    #[test]
    fn uc_share_pages_recursively_and_retains_only_protected_token_context() {
        let credentials = share_credentials();
        let http = Http::new(vec![
            json!({"code":0,"data":{"stoken":"fresh-token"}}),
            json!({"code":0,"data":{"list":[{"fid":"d","file_name":"dir","dir":true}]},"metadata":{"_total":2}}),
            json!({"code":0,"data":{"list":[{"fid":"f1","file_name":"one","dir":false,"share_fid_token":"f-token"}]},"metadata":{"_total":2}}),
            json!({"code":0,"data":{"list":[{"fid":"f2","file_name":"two","dir":false,"share_fid_token":"nested-token"}]},"metadata":{"_total":1}}),
        ]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let candidate = crate::parser::CloudShareCandidate {
            provider: CloudProvider::Uc,
            url: "https://drive.uc.cn/s/share123".into(),
            access_code: Some("a123".into()),
        };
        let ShareInspection::Plan(plan) = adapter.inspect(&candidate).unwrap() else {
            panic!()
        };
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[1].relative_directory, vec!["dir"]);
        assert_eq!(
            plan.provider_context["files"]["f2"]["token"],
            "nested-token"
        );
        let requests = http.requests.lock().unwrap();
        assert!(requests[2].url.contains("_page=2"));
        assert!(requests[3].url.contains("pdir_fid=d"));
        let body: Value = serde_json::from_str(requests[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["passcode"], "a123");
    }
    #[test]
    fn uc_share_errors_missing_csrf_and_no_login_never_transfer_or_retry() {
        let empty = MemoryCredentialStore::default();
        let http = Http::new(vec![]);
        let adapter = provider_share_adapter(CloudProvider::Uc, &empty, &http).unwrap();
        assert!(matches!(
            adapter.resolve(&share_entry()),
            Err(QuarkError::NotAuthorized)
        ));
        assert!(http.requests.lock().unwrap().is_empty());
        let credentials = share_credentials();
        for value in [
            json!({"code":401}),
            json!({"code":4031,"message":"risk verification"}),
            json!({"code":1,"message":"expired share secret"}),
            json!({"code":"0","data":[]}),
            json!({"code":0,"error_code":4031}),
        ] {
            let http = Http::new(vec![
                json!({"code":0,"data":{"stoken":"fresh"}}),
                value.clone(),
            ]);
            let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
            let error = adapter.resolve(&share_entry()).err().unwrap();
            if value["code"] == 4031 {
                assert!(matches!(error, QuarkError::WebAuthorizationRequired));
            }
            assert!(!error.to_string().contains("expired share secret"));
            assert_eq!(http.requests.lock().unwrap().len(), 2);
        }
        let no_csrf = MemoryCredentialStore::default();
        no_csrf
            .save(
                "uc",
                &serde_json::to_vec(&UcSession::cookie("__uid=account-7; __pus=i; __puus=s"))
                    .unwrap(),
            )
            .unwrap();
        let http = Http::new(vec![json!({"code":0,"data":{"stoken":"fresh"}})]);
        assert!(matches!(
            provider_share_adapter(CloudProvider::Uc, &no_csrf, &http)
                .unwrap()
                .resolve(&share_entry()),
            Err(QuarkError::WebAuthorizationRequired)
        ));
    }
    #[test]
    fn uc_share_issued_csrf_is_used_and_no_direct_journals_exact_transfer() {
        let credentials = MemoryCredentialStore::default();
        credentials
            .save(
                "uc",
                &serde_json::to_vec(&UcSession::cookie("__uid=account-7; __pus=i; __puus=s"))
                    .unwrap(),
            )
            .unwrap();
        let token = json!({"code":0,"data":{"stoken":"fresh","csrf_token":"issued-csrf"}});
        let http = Http::new(vec![
            token.clone(),
            json!({"code":0,"data":[]}),
            token,
            json!({"code":0,"data":{"task_id":"t"}}),
            json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal-id"]}}}),
            json!({"code":401}),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
        );
        let entry = share_entry();
        store.upsert(&entry.plan).unwrap();
        let source = entry.plan.source_for("f1");
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        assert!(matches!(
            resolve_current_share_download(&store, adapter.as_ref(), &source),
            Err(QuarkError::AuthorizationExpired)
        ));
        assert_eq!(
            store.resolve_entry(&source).unwrap().plan.transfers[0].created_file_id,
            "personal-id"
        );
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests[1].headers["x-csrf-token"], "issued-csrf");
        assert!(requests[5].body.as_ref().unwrap().contains("personal-id"));
    }

    struct DirectoryTransport;

    impl CloudHttpTransport for DirectoryTransport {
        fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
            Ok(CloudHttpResponse {
                status: 200,
                body: r#"{
                    "code": 0,
                    "status": 200,
                    "data": {
                        "list": [{
                            "fid": "file-1",
                            "file_name": "normal-file.txt",
                            "file_type": 1,
                            "risk_type": 0
                        }]
                    },
                    "metadata": {"_total": 1}
                }"#
                .into(),
                ..Default::default()
            })
        }
    }

    #[test]
    fn directory_accepts_successful_file_metadata_that_includes_risk_type() {
        let directory = UcClient::new(
            UcSession::cookie("__pus=identity; __puus=session"),
            &DirectoryTransport,
        )
        .directory("0")
        .expect("a successful UC directory response is not a web-verification response");

        assert_eq!(directory.id, "0");
        assert_eq!(directory.items.len(), 2);
        assert!(directory.items.iter().any(|item| item.id == "file-1"));
        assert!(directory
            .items
            .iter()
            .any(|item| item.id == UC_RECYCLE_BIN_ID));
    }

    #[test]
    fn uc_share_newer_json_csrf_survives_followup_responses_without_csrf_cookies() {
        let credentials = share_credentials();
        let http = Http::new(vec![
            json!({"code":0,"data":{"stoken":"fresh","csrf_token":"newer-csrf"}}),
            json!({"code":0,"data":{"task_id":"t"}}),
            json!({"code":0,"data":{"status":2,"save_as":{"save_as_top_fids":["personal-id"]}}}),
            json!({"code":0,"data":[{"fid":"personal-id","download_url":"https://cdn.example/personal"}]}),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let store = CloudSharePlanStore::new(
            crate::cloud::SystemCredentialStore::new(directory.path().into()).unwrap(),
        );
        let entry = share_entry();
        store.upsert(&entry.plan).unwrap();
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let direct = adapter.transfer(&entry, &store).unwrap();
        assert!(direct
            .headers
            .contains(&("x-csrf-token".into(), "newer-csrf".into())));
        assert_eq!(
            http.requests.lock().unwrap()[3].headers["x-csrf-token"],
            "newer-csrf"
        );
    }
    #[test]
    fn uc_share_set_cookie_csrf_rotates_before_signing_and_keeps_download_snapshot() {
        struct CookieHttp(std::sync::Mutex<Vec<CloudHttpRequest>>);
        impl CloudHttpTransport for CookieHttp {
            fn send(&self, request: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                let mut requests = self.0.lock().unwrap();
                requests.push(request);
                let first = requests.len() == 1;
                Ok(CloudHttpResponse{status:200,body:if first {json!({"code":0,"data":{"stoken":"fresh"}})}else{json!({"code":0,"data":[{"fid":"f1","download_url":"https://cdn.example/f"}]})}.to_string(),set_cookies:vec![if first{"csrf_token=issued-cookie; Path=/"}else{"csrf_token=after-signing; Path=/"}.into()]})
            }
        }
        let credentials = share_credentials();
        let http = CookieHttp(std::sync::Mutex::new(vec![]));
        let adapter = provider_share_adapter(CloudProvider::Uc, &credentials, &http).unwrap();
        let ShareResolution::Direct(direct) = adapter.resolve(&share_entry()).unwrap() else {
            panic!()
        };
        assert!(direct
            .headers
            .contains(&("x-csrf-token".into(), "issued-cookie".into())));
        assert_eq!(
            http.0.lock().unwrap()[1].headers["x-csrf-token"],
            "issued-cookie"
        );
        assert!(!direct
            .headers
            .iter()
            .any(|(_, value)| value.contains("after-signing")));
    }
    #[test]
    fn uc_share_http_challenge_without_json_preserves_authorization_error() {
        struct StatusHttp(u16);
        impl CloudHttpTransport for StatusHttp {
            fn send(&self, _: CloudHttpRequest) -> Result<CloudHttpResponse, QuarkError> {
                Ok(CloudHttpResponse {
                    status: self.0,
                    body: "<html>secret</html>".into(),
                    ..Default::default()
                })
            }
        }
        let credentials = share_credentials();
        for status in [401, 412] {
            let http = StatusHttp(status);
            let result = provider_share_adapter(CloudProvider::Uc, &credentials, &http)
                .unwrap()
                .resolve(&share_entry());
            assert!(if status == 401 {
                matches!(result, Err(QuarkError::AuthorizationExpired))
            } else {
                matches!(result, Err(QuarkError::WebAuthorizationRequired))
            });
        }
    }
}
