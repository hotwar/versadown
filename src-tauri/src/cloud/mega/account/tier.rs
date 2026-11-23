//! Whether the signed-in account is a paid (Pro) one, cached per session.
//!
//! Only a paid account has a transfer quota of its own: a free account's quota
//! is IP-based even when signed in (the SDK parses the free quota as IP
//! buckets, `tah`, and `mxfer`/`caxfer` only for paid plans; measured against
//! the live API with a free account, a download made with the session raised
//! the IP buckets and left the account counters at zero). Public-link
//! downloads therefore carry the session only for a paid account.
//!
//! The level comes from `uq` with `pro: 1`: the web client reads
//! `account.type = res.utype` from that reply, and the SDK derives the same
//! from the plans in it (`CommandGetUserQuota::processPlans`). `utype` is `0`
//! for a free account; any other value is a paid plan. A transfer allowance
//! (`mxfer`) also marks a paid plan.

use super::api::AccountApi;
use super::session::MegaSession;
use crate::cloud::{CloudHttpTransport, QuarkError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A plan can be bought or end while the app runs; recheck now and then.
const TIER_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_ENTRIES: usize = 8;

/// Session fingerprint -> (is paid, when it was learned).
type Tiers = HashMap<[u8; 32], (bool, Instant)>;

static TIERS: Mutex<Option<Tiers>> = Mutex::new(None);

/// Cache key: a hash, so the session id itself is not kept a second time.
fn fingerprint(sid: &str) -> [u8; 32] {
    Sha256::digest(sid.as_bytes()).into()
}

/// The plan level of a `uq` reply, when it states one.
fn paid_in(quota: &Value) -> Option<bool> {
    let level = quota.get("utype")?.as_i64()?;
    let allowance = quota.get("mxfer").and_then(Value::as_u64).unwrap_or(0);
    Some(level > 0 || allowance > 0)
}

fn cached(sid: &str) -> Option<bool> {
    let tiers = TIERS.lock().ok()?;
    let (paid, at) = tiers.as_ref()?.get(&fingerprint(sid))?;
    (at.elapsed() < TIER_TTL).then_some(*paid)
}

fn store(sid: &str, paid: bool) {
    if let Ok(mut tiers) = TIERS.lock() {
        let tiers = tiers.get_or_insert_with(HashMap::new);
        if tiers.len() >= MAX_ENTRIES {
            tiers.clear();
        }
        tiers.insert(fingerprint(sid), (paid, Instant::now()));
    }
}

/// Keeps the cache current whenever a `uq` reply is read anyway (the storage
/// capacity refresh). Replies that carry no plan level change nothing.
pub(super) fn remember(sid: &str, quota: &Value) {
    if let Some(paid) = paid_in(quota) {
        store(sid, paid);
    }
}

/// Whether the session's account is a paid one: from the cache while fresh,
/// otherwise from one `uq` request.
pub fn session_is_paid(
    http: &dyn CloudHttpTransport,
    session: &MegaSession,
) -> Result<bool, QuarkError> {
    if let Some(paid) = cached(session.sid()) {
        return Ok(paid);
    }
    let quota = AccountApi::with_session(http, session.sid()).quota()?;
    let paid = paid_in(&quota).ok_or(QuarkError::InvalidResponse)?;
    store(session.sid(), paid);
    Ok(paid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_plan_level_decides() {
        assert_eq!(paid_in(&json!({"utype": 0, "tah": [1, 2]})), Some(false));
        assert_eq!(paid_in(&json!({"utype": 4})), Some(true));
        assert_eq!(
            paid_in(&json!({"utype": 0, "mxfer": 1_000_000})),
            Some(true)
        );
        assert_eq!(paid_in(&json!({"mstrg": 1, "cstrg": 0})), None);
    }

    #[test]
    fn remembered_levels_are_per_session_and_replies_without_a_level_change_nothing() {
        remember("TIER-TEST-A", &json!({"utype": 3}));
        remember("TIER-TEST-B", &json!({"utype": 0}));
        remember("TIER-TEST-B", &json!({"mstrg": 1}));
        assert_eq!(cached("TIER-TEST-A"), Some(true));
        assert_eq!(cached("TIER-TEST-B"), Some(false));
        assert_eq!(cached("TIER-TEST-UNKNOWN"), None);
    }
}
