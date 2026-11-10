use std::{path::PathBuf, time::Instant};

use uuid::Uuid;

pub(super) const NATIVE_GID_PREFIX: &str = "xunlei-native:";
pub(super) const NATIVE_DOWNLOADING: i32 = 3;

/// A task ID issued by a native SDK is valid only for the engine process that
/// issued it. Persist both values so an SDK restart cannot accidentally
/// control a newly reused numeric task ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativeTaskHandle {
    pub(super) engine_session: Uuid,
    pub(super) task_id: u32,
}

impl NativeTaskHandle {
    pub(super) fn new(engine_session: Uuid, task_id: u32) -> Option<Self> {
        (task_id != 0).then_some(Self {
            engine_session,
            task_id,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NativeEngineSession(Uuid);

impl NativeEngineSession {
    pub(super) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[cfg(test)]
    fn new_for_tests(id: Uuid) -> Self {
        Self(id)
    }

    pub(super) fn allocate_handle(self, task_id: u32) -> Option<NativeTaskHandle> {
        NativeTaskHandle::new(self.0, task_id)
    }

    pub(super) fn owns(self, handle: NativeTaskHandle) -> bool {
        self.0 == handle.engine_session
    }
}

/// A task being retried deliberately becomes unbound before it is resolved
/// again. This forbids pause/cancel from being directed at an old SDK number
/// while a new signed URL and acceleration token are being acquired.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NativeTaskBinding {
    handle: Option<NativeTaskHandle>,
}

impl NativeTaskBinding {
    #[cfg(test)]
    fn from_handle(handle: NativeTaskHandle) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    #[cfg(test)]
    fn begin_resume(&mut self) -> Option<NativeTaskHandle> {
        self.handle.take()
    }

    #[cfg(test)]
    fn bind_after_submit(&mut self, handle: NativeTaskHandle) {
        self.handle = Some(handle);
    }

    #[cfg(test)]
    fn handle(self) -> Option<NativeTaskHandle> {
        self.handle
    }

    #[cfg(test)]
    fn can_control(self, handle: NativeTaskHandle, session: &NativeEngineSession) -> bool {
        self.handle == Some(handle) && session.owns(handle)
    }
}

pub(super) fn native_gid(handle: NativeTaskHandle) -> String {
    format!(
        "{NATIVE_GID_PREFIX}{}:{}",
        handle.engine_session, handle.task_id
    )
}

pub(super) fn parse_native_gid(gid: &str) -> Option<NativeTaskHandle> {
    let value = gid.strip_prefix(NATIVE_GID_PREFIX)?;
    let (session, task_id) = value.split_once(':')?;
    if task_id.contains(':') {
        return None;
    }
    NativeTaskHandle::new(Uuid::parse_str(session).ok()?, task_id.parse().ok()?)
}

/// Submission data is intentionally not `Debug`: it includes a signed URL,
/// request headers, and short-lived account acceleration credentials.
#[allow(dead_code)]
pub(super) struct NativeSubmission {
    pub(super) url: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) output_path: PathBuf,
    pub(super) output_name: String,
    pub(super) user_id: String,
    pub(super) peer_id: String,
    pub(super) acceleration_token: Option<String>,
    pub(super) cid: Option<String>,
    pub(super) gcid: Option<String>,
    pub(super) total_length: Option<u64>,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeEngineError {
    #[cfg(target_os = "macos")]
    UnsupportedClientVersion,
    Unavailable,
    Incompatible,
    WorkerStopped,
    TaskUnknown,
    OperationFailed,
}

#[allow(dead_code)]
pub(super) trait NativeEngine {
    fn submit(&mut self, request: NativeSubmission) -> Result<NativeTaskHandle, NativeEngineError>;
    fn poll(&mut self, handle: NativeTaskHandle) -> Result<NativeProgress, NativeEngineError>;
    fn control(
        &mut self,
        handle: NativeTaskHandle,
        action: crate::DownloadAction,
    ) -> Result<(), NativeEngineError>;
    fn release(&mut self, handle: NativeTaskHandle) -> Result<(), NativeEngineError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct NativeProgress {
    pub(super) completed: u64,
    pub(super) total: u64,
    pub(super) status: i32,
    pub(super) error_code: i32,
    pub(super) connections: u32,
    pub(super) received: u64,
    pub(super) peer_received: u64,
}

impl NativeProgress {
    /// The Windows proxy's packed `XL_QueryTaskInfo` response is decoded at
    /// its platform adapter boundary. Linux reports already-normalized values
    /// through the worker protocol rather than sharing this layout.
    pub(super) fn from_windows_query_info(info: &[u8; 924]) -> Self {
        let peer_received = read_u64(info, 60).saturating_add(read_u64(info, 68));
        Self {
            status: read_i32(info, 4),
            error_code: read_i32(info, 8),
            total: read_u64(info, 12),
            completed: read_u64(info, 20),
            connections: read_u32(info, 48),
            received: read_u64(info, 52).saturating_add(peer_received),
            peer_received,
        }
    }
}

pub(super) struct NativeTransferMeter {
    sampled_at: Instant,
    received: u64,
    peer_received: u64,
    pub(super) last_data_at: Instant,
    has_baseline: bool,
    svip_confirmed: bool,
}

impl NativeTransferMeter {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            sampled_at: now,
            received: 0,
            peer_received: 0,
            last_data_at: now,
            has_baseline: false,
            svip_confirmed: false,
        }
    }

    pub(super) fn sample(
        &mut self,
        progress: NativeProgress,
        has_vip_token: bool,
        now: Instant,
    ) -> (u64, bool) {
        if !self.has_baseline {
            self.received = progress.received;
            self.peer_received = progress.peer_received;
            self.sampled_at = now;
            self.has_baseline = true;
            return (0, false);
        }
        let bytes = progress.received.saturating_sub(self.received);
        let elapsed = now.duration_since(self.sampled_at).as_secs_f64();
        let speed = if elapsed > 0.0 && progress.status == NATIVE_DOWNLOADING {
            (bytes as f64 / elapsed) as u64
        } else {
            0
        };
        if bytes > 0 {
            self.last_data_at = now;
        }
        if has_vip_token && progress.peer_received > self.peer_received && bytes > 0 {
            self.svip_confirmed = true;
        } else if !has_vip_token {
            self.svip_confirmed = false;
        }
        self.received = progress.received;
        self.peer_received = progress.peer_received;
        self.sampled_at = now;
        (speed, self.svip_confirmed)
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap_or_default())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::{
        native_gid, parse_native_gid, NativeEngineSession, NativeTaskBinding, NativeTaskHandle,
    };
    use uuid::Uuid;

    #[test]
    fn session_qualified_gid_round_trips_only_the_owning_handle() {
        let session = Uuid::parse_str("4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1").unwrap();
        let handle = NativeTaskHandle::new(session, 42).unwrap();

        assert_eq!(
            native_gid(handle),
            "xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42"
        );
        assert_eq!(parse_native_gid(&native_gid(handle)), Some(handle));
    }

    #[test]
    fn native_gid_rejects_legacy_malformed_and_zero_task_ids() {
        for gid in [
            "xunlei-native:42",
            "xunlei-native:not-a-uuid:42",
            "xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:0",
            "xunlei-native:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:not-a-number",
            "aria2:4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1:42",
        ] {
            assert_eq!(parse_native_gid(gid), None, "{gid}");
        }
    }

    #[test]
    fn a_handle_from_another_engine_session_is_not_owned() {
        let owner = NativeEngineSession::new_for_tests(
            Uuid::parse_str("4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1").unwrap(),
        );
        let other = NativeEngineSession::new_for_tests(
            Uuid::parse_str("ea01d137-cc8a-4770-a2b7-6ef8c5d12af5").unwrap(),
        );
        let handle = owner.allocate_handle(7).unwrap();

        assert!(owner.owns(handle));
        assert!(!other.owns(handle));
    }

    #[test]
    fn beginning_resume_drops_the_old_handle_before_a_new_submit() {
        let session = NativeEngineSession::new_for_tests(
            Uuid::parse_str("4aa8c3e7-f41d-4d2e-bbb0-21a9f87000f1").unwrap(),
        );
        let old_handle = session.allocate_handle(7).unwrap();
        let mut binding = NativeTaskBinding::from_handle(old_handle);

        assert_eq!(binding.begin_resume(), Some(old_handle));
        assert_eq!(binding.handle(), None);
        assert!(!binding.can_control(old_handle, &session));

        let replacement = session.allocate_handle(8).unwrap();
        binding.bind_after_submit(replacement);
        assert_eq!(binding.handle(), Some(replacement));
        assert!(binding.can_control(replacement, &session));
    }
}
