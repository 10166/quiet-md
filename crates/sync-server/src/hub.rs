use quiet_sync_proto::api::{OnlineDevice, RemoteChange, ServerFrame};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, watch};

pub const MAX_WS_QUEUE_MSGS: usize = 128;
pub const MAX_WS_QUEUE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum WsOutbound {
    Text { json: String, bytes: usize },
    Close,
}

pub fn try_send_frame(
    tx: &mpsc::Sender<WsOutbound>,
    queued_bytes: &AtomicUsize,
    cancel: &watch::Sender<bool>,
    frame: &ServerFrame,
) -> bool {
    let Ok(json) = serde_json::to_string(frame) else {
        return true;
    };
    try_send_text(tx, queued_bytes, cancel, json)
}

fn try_send_text(
    tx: &mpsc::Sender<WsOutbound>,
    queued_bytes: &AtomicUsize,
    cancel: &watch::Sender<bool>,
    json: String,
) -> bool {
    let bytes = json.len();
    let cur = queued_bytes.load(Ordering::Acquire);
    if cur.saturating_add(bytes) > MAX_WS_QUEUE_BYTES {
        let _ = cancel.send(true);
        let _ = tx.try_send(WsOutbound::Close);
        return false;
    }
    queued_bytes.fetch_add(bytes, Ordering::AcqRel);
    match tx.try_send(WsOutbound::Text { json, bytes }) {
        Ok(()) => true,
        Err(_) => {
            queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
            let _ = cancel.send(true);
            false
        }
    }
}

struct PeerConn {
    user_id: String,
    device_id: String,
    device_name: String,
    platform: String,
    active_entity_id: Option<String>,
    tx: mpsc::Sender<WsOutbound>,
    queued_bytes: Arc<AtomicUsize>,
    cancel: watch::Sender<bool>,
}

impl PeerConn {
    fn send_json(&self, json: String) -> bool {
        try_send_text(&self.tx, &self.queued_bytes, &self.cancel, json)
    }

    fn close_now(&self) {
        let _ = self.cancel.send(true);
        let _ = self.tx.try_send(WsOutbound::Close);
    }
}

#[derive(Clone, Default)]
pub struct VaultHub {
    vaults: Arc<RwLock<HashMap<String, HashMap<String, PeerConn>>>>,
}

impl VaultHub {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn join(
        &self,
        vault_id: &str,
        conn_id: &str,
        user_id: &str,
        device_id: &str,
        device_name: &str,
        platform: &str,
        tx: mpsc::Sender<WsOutbound>,
        queued_bytes: Arc<AtomicUsize>,
        cancel: watch::Sender<bool>,
    ) -> Vec<OnlineDevice> {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        let peers = guard.entry(vault_id.to_string()).or_default();
        peers.insert(
            conn_id.to_string(),
            PeerConn {
                user_id: user_id.to_string(),
                device_id: device_id.to_string(),
                device_name: device_name.to_string(),
                platform: platform.to_string(),
                active_entity_id: None,
                tx,
                queued_bytes,
                cancel,
            },
        );
        let devices = collect_online_devices(peers);
        if let Ok(json) = serde_json::to_string(&ServerFrame::PresenceUpdate { devices: devices.clone() }) {
            peers.retain(|cid, peer| cid == conn_id || peer.send_json(json.clone()));
        }
        collect_online_devices(peers)
    }

    pub fn leave(&self, vault_id: &str, conn_id: &str) {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        let Some(peers) = guard.get_mut(vault_id) else { return };
        peers.remove(conn_id);
        if peers.is_empty() {
            guard.remove(vault_id);
            return;
        }
        let devices = collect_online_devices(peers);
        if let Ok(json) = serde_json::to_string(&ServerFrame::PresenceUpdate { devices }) {
            peers.retain(|_, peer| peer.send_json(json.clone()));
        }
        if peers.is_empty() {
            guard.remove(vault_id);
        }
    }

    pub fn set_presence(&self, vault_id: &str, conn_id: &str, active_entity_id: Option<String>) {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        let Some(peers) = guard.get_mut(vault_id) else { return };
        let Some(peer) = peers.get_mut(conn_id) else { return };
        peer.active_entity_id = active_entity_id;
        let devices = collect_online_devices(peers);
        if let Ok(json) = serde_json::to_string(&ServerFrame::PresenceUpdate { devices }) {
            peers.retain(|_, peer| peer.send_json(json.clone()));
        }
    }

    pub fn broadcast_changes(&self, vault_id: &str, origin_device_id: &str, items: &[RemoteChange]) {
        if items.is_empty() {
            return;
        }
        let encoded: Vec<String> = items
            .iter()
            .filter_map(|item| serde_json::to_string(&ServerFrame::Broadcast { item: item.clone() }).ok())
            .collect();
        if encoded.is_empty() {
            return;
        }
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        let Some(peers) = guard.get_mut(vault_id) else { return };
        let mut dropped = false;
        peers.retain(|_, peer| {
            if peer.device_id == origin_device_id {
                return true;
            }
            for json in &encoded {
                if !peer.send_json(json.clone()) {
                    dropped = true;
                    return false;
                }
            }
            true
        });
        if peers.is_empty() {
            guard.remove(vault_id);
        } else if dropped {
            let devices = collect_online_devices(peers);
            if let Ok(json) = serde_json::to_string(&ServerFrame::PresenceUpdate { devices }) {
                peers.retain(|_, peer| peer.send_json(json.clone()));
            }
        }
    }

    pub fn disconnect_device(&self, user_id: &str, device_id: &str) {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        let mut empty_vaults = Vec::new();
        for (vid, peers) in guard.iter_mut() {
            let mut removed = false;
            peers.retain(|_, peer| {
                if peer.user_id == user_id && peer.device_id == device_id {
                    peer.close_now();
                    removed = true;
                    false
                } else {
                    true
                }
            });
            if peers.is_empty() {
                empty_vaults.push(vid.clone());
            } else if removed {
                let devices = collect_online_devices(peers);
                if let Ok(json) = serde_json::to_string(&ServerFrame::PresenceUpdate { devices }) {
                    peers.retain(|_, peer| peer.send_json(json.clone()));
                }
                if peers.is_empty() {
                    empty_vaults.push(vid.clone());
                }
            }
        }
        for vid in empty_vaults {
            guard.remove(&vid);
        }
    }

    pub fn online_device_ids_for_user(&self, user_id: &str) -> HashSet<String> {
        let guard = self.vaults.read().unwrap_or_else(|e| e.into_inner());
        let mut out = HashSet::new();
        for peers in guard.values() {
            for peer in peers.values() {
                if peer.user_id == user_id {
                    out.insert(peer.device_id.clone());
                }
            }
        }
        out
    }

    pub fn disconnect_user(&self, user_id: &str) {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        let mut empty_vaults = Vec::new();
        for (vid, peers) in guard.iter_mut() {
            let mut removed = false;
            peers.retain(|_, peer| {
                if peer.user_id == user_id {
                    peer.close_now();
                    removed = true;
                    false
                } else {
                    true
                }
            });
            if peers.is_empty() {
                empty_vaults.push(vid.clone());
            } else if removed {
                let devices = collect_online_devices(peers);
                if let Ok(json) = serde_json::to_string(&ServerFrame::PresenceUpdate { devices }) {
                    peers.retain(|_, peer| peer.send_json(json.clone()));
                }
                if peers.is_empty() {
                    empty_vaults.push(vid.clone());
                }
            }
        }
        for vid in empty_vaults {
            guard.remove(&vid);
        }
    }

    pub fn disconnect_vault(&self, vault_id: &str) {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        if let Some(peers) = guard.remove(vault_id) {
            for peer in peers.values() {
                peer.close_now();
            }
        }
    }

    pub fn disconnect_all(&self) {
        let mut guard = self.vaults.write().unwrap_or_else(|e| e.into_inner());
        for peers in guard.values() {
            for peer in peers.values() {
                peer.close_now();
            }
        }
        guard.clear();
    }

    pub fn connection_stats(&self) -> (usize, usize) {
        let guard = self.vaults.read().unwrap_or_else(|e| e.into_inner());
        let mut conns = 0usize;
        let mut devices = HashSet::new();
        for peers in guard.values() {
            conns += peers.len();
            for p in peers.values() {
                devices.insert(p.device_id.clone());
            }
        }
        (conns, devices.len())
    }

    pub fn online_device_ids_by_user(&self) -> HashMap<String, HashSet<String>> {
        let guard = self.vaults.read().unwrap_or_else(|e| e.into_inner());
        let mut out: HashMap<String, HashSet<String>> = HashMap::new();
        for peers in guard.values() {
            for p in peers.values() {
                out.entry(p.user_id.clone()).or_default().insert(p.device_id.clone());
            }
        }
        out
    }
}

fn collect_online_devices(peers: &HashMap<String, PeerConn>) -> Vec<OnlineDevice> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for p in peers.values() {
        if seen.insert(p.device_id.clone()) {
            out.push(OnlineDevice {
                device_id: p.device_id.clone(),
                device_name: p.device_name.clone(),
                platform: p.platform.clone(),
                active_entity_id: p.active_entity_id.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    out
}
