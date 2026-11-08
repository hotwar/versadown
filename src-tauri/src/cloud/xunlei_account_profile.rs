use super::{CloudHttpTransport, QuarkError, XunleiClient, AUTH};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XunleiAccountProfile {
    account_id: String,
    nickname: Option<String>,
    membership: Option<Membership>,
}

#[derive(Debug, Serialize)]
struct Membership {
    kind: MembershipKind,
    level: Option<u16>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum MembershipKind {
    None,
    Vip,
    Platinum,
    Svip,
    Cloud,
}

fn number(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|v| v.try_into().ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn membership(profile: &Value) -> Option<Membership> {
    let rows = profile["vip_info"].as_array()?;
    let active = |product| {
        rows.iter().find(|row| {
            number(&row["user_vas"]) == Some(product) && number(&row["is_vip"]) == Some(1)
        })
    };
    let (kind, row) = if let Some(row) = active(2) {
        let kind = match number(&row["vas_type"]) {
            Some(5) => MembershipKind::Svip,
            Some(3) => MembershipKind::Platinum,
            _ => MembershipKind::Vip,
        };
        (kind, Some(row))
    } else if let Some(row) = active(306) {
        (MembershipKind::Cloud, Some(row))
    } else {
        (MembershipKind::None, None)
    };
    Some(Membership {
        kind,
        level: row.and_then(|r| number(&r["level"])).filter(|n| *n > 0),
    })
}

impl<T: CloudHttpTransport> XunleiClient<'_, T> {
    pub fn account_card_profile(&self) -> Result<XunleiAccountProfile, QuarkError> {
        let profile = self.request("GET", format!("{AUTH}/v1/user/me"), None)?;
        let account_id = self.session().user_id;
        if account_id.is_empty() {
            return Err(QuarkError::InvalidResponse);
        }
        if let Some(id) = ["sub", "id", "user_id"]
            .iter()
            .find_map(|key| profile.get(key))
        {
            let matches = id.as_str().is_some_and(|id| id == account_id)
                || id.as_u64().is_some_and(|id| id.to_string() == account_id);
            if !matches {
                return Err(QuarkError::AccountMismatch);
            }
        }
        let nickname = ["name", "nickname"]
            .iter()
            .filter_map(|key| profile[key].as_str().map(str::trim))
            .find(|name| !name.is_empty())
            .map(str::to_owned);
        Ok(XunleiAccountProfile {
            account_id,
            nickname,
            membership: membership(&profile),
        })
    }
}
