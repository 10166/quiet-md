use serde::{Deserialize, Serialize};

use crate::crypto::{E2eeSetup, Envelope};
use crate::entity::EntityKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterReq {
    pub email: String,
    pub password: String,
    pub display_name: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginReq {
    pub email: String,
    pub password: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshReq {
    pub refresh_token: String,
    pub device_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutReq {
    pub refresh_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsageResp {
    pub used_bytes: u64,
    pub entity_bytes: u64,
    pub blob_bytes: u64,
    pub entity_count: u64,
    pub blob_count: u64,
    pub max_blob_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen_at: u64,
    #[serde(default)]
    pub online: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResp {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub user: UserInfo,
    pub device: DeviceInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsTicketResp {
    pub ticket: String,
    pub expires_in: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultSummary {
    pub id: String,
    pub name: String,
    pub head_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee: Option<E2eeSetup>,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateVaultReq {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee: Option<E2eeSetup>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateE2eeKeyReq {
    pub e2ee: E2eeSetup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushOp {
    pub entity_id: String,
    pub kind: EntityKind,
    pub expected_version: u64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Envelope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blob_hashes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushReq {
    pub device_id: String,
    pub ops: Vec<PushOp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteChange {
    pub seq: u64,
    pub entity_id: String,
    pub kind: EntityKind,
    pub version: u64,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<Envelope>,
    pub updated_at: u64,
    pub device_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PushItemResult {
    Accepted { entity_id: String, version: u64, seq: u64 },
    Conflict { entity_id: String, current: RemoteChange },
    MissingBlobs { entity_id: String, missing: Vec<String> },
    StaleBase { entity_id: String, head_seq: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResp {
    pub head_seq: u64,
    pub results: Vec<PushItemResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesResp {
    pub head_seq: u64,
    pub changes: Vec<RemoteChange>,
    pub has_more: bool,
    pub resync_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResp {
    pub head_seq: u64,
    pub items: Vec<RemoteChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobsExistsReq {
    pub hashes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobsExistsResp {
    pub existing: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineDevice {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_entity_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello {
        device_id: String,
        device_name: String,
        platform: String,
        last_seq: u64,
    },
    Push {
        req_id: String,
        ops: Vec<PushOp>,
    },
    Pull {
        req_id: String,
        since_seq: u64,
        limit: u32,
    },
    Presence {
        active_entity_id: Option<String>,
    },
    Ping,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome {
        head_seq: u64,
        online_devices: Vec<OnlineDevice>,
    },
    PushAck {
        req_id: String,
        head_seq: u64,
        results: Vec<PushItemResult>,
    },
    Changes {
        req_id: String,
        head_seq: u64,
        changes: Vec<RemoteChange>,
        has_more: bool,
        resync_required: bool,
    },
    Broadcast {
        item: RemoteChange,
    },
    PresenceUpdate {
        devices: Vec<OnlineDevice>,
    },
    Pong,
    Error {
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_frame_round_trip() {
        let f = ClientFrame::Push {
            req_id: "r1".into(),
            ops: vec![PushOp {
                entity_id: "d1".into(),
                kind: EntityKind::Document,
                expected_version: 0,
                deleted: false,
                envelope: Some(Envelope::plain(b"hi")),
                blob_hashes: vec![],
            }],
        };
        let j = serde_json::to_string(&f).unwrap();
        let d: ClientFrame = serde_json::from_str(&j).unwrap();
        assert_eq!(f, d);
    }
}
