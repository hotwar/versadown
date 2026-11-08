use super::{CloudHttpTransport, QuarkError, XunleiClient, AUTH};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct XunleiMembership {
    pub account_id: String,
    pub kind: MembershipKind,
    pub level: Option<u16>,
    /// Official user_vas=2 / vas_type, not the file certificate's vip_type.
    pub native_vip_type: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MembershipKind {
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

fn parse_membership(value: &Value, account_id: &str) -> Result<XunleiMembership, QuarkError> {
    let rows = value["vip_info"]
        .as_array()
        .ok_or(QuarkError::InvalidResponse)?;
    let active = |row: &&Value| number(&row["is_vip"]) == Some(1);
    let download = rows
        .iter()
        .filter(active)
        .filter(|row| number(&row["user_vas"]) == Some(2))
        .max_by_key(|row| number(&row["vas_type"]).unwrap_or(0));
    let cloud = rows
        .iter()
        .filter(active)
        .find(|row| number(&row["user_vas"]) == Some(306));
    let (kind, row, native_vip_type) = if let Some(row) = download {
        let vip_type = number(&row["vas_type"]).ok_or(QuarkError::InvalidResponse)?;
        let kind = match vip_type {
            5 => MembershipKind::Svip,
            3 => MembershipKind::Platinum,
            _ => MembershipKind::Vip,
        };
        (kind, Some(row), vip_type)
    } else if let Some(row) = cloud {
        (MembershipKind::Cloud, Some(row), 0)
    } else {
        (MembershipKind::None, None, 0)
    };
    Ok(XunleiMembership {
        account_id: account_id.into(),
        kind,
        level: row
            .and_then(|row| number(&row["level"]))
            .filter(|level| *level > 0),
        native_vip_type,
    })
}

impl<T: CloudHttpTransport> XunleiClient<'_, T> {
    pub fn membership(&self) -> Result<XunleiMembership, QuarkError> {
        let value = self.request("GET", format!("{AUTH}/v1/user/me"), None)?;
        let session = self.session();
        let account_id = value
            .get("sub")
            .or_else(|| value.get("id"))
            .or_else(|| value.get("user_id"))
            .and_then(Value::as_str)
            .unwrap_or(&session.user_id);
        if account_id.is_empty() || (!session.user_id.is_empty() && account_id != session.user_id) {
            return Err(QuarkError::AccountMismatch);
        }
        parse_membership(&value, account_id)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn native_membership_markers(&self) -> Vec<(String, String)> {
        // Optional account metadata must not break an otherwise valid download.
        self.membership()
            .ok()
            .map(|membership| {
                vec![(
                    crate::xunlei_native::VIP_TYPE_MARKER_HEADER.into(),
                    membership.native_vip_type.to_string(),
                )]
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn uses_official_download_membership_type_and_level() {
        let result = parse_membership(
            &json!({"vip_info":[
                {"user_vas":"306","vas_type":"5","is_vip":"1","level":"9"},
                {"user_vas":"2","vas_type":"5","is_vip":"1","level":"1"}
            ]}),
            "account",
        )
        .unwrap();
        assert_eq!(result.kind, MembershipKind::Svip);
        assert_eq!(result.level, Some(1));
        assert_eq!(result.native_vip_type, 5);
    }
    #[test]
    fn expired_or_other_product_membership_does_not_grant_download_membership() {
        let result = parse_membership(
            &json!({"vip_info":[
                {"user_vas":2,"vas_type":5,"is_vip":0,"level":8},
                {"user_vas":313,"vas_type":5,"is_vip":1,"level":3}
            ]}),
            "account",
        )
        .unwrap();
        assert_eq!(result.kind, MembershipKind::None);
        assert_eq!(result.native_vip_type, 0);
        assert!(parse_membership(&json!({}), "account").is_err());
    }
}
