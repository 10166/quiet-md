use anyhow::Result;
use base64::Engine;
use fdl_core::docxml;
use fdl_core::merge::{merge_boards, merge_documents, merge_json_object, merge_markdown, merge_meta};
use fdl_core::store::{is_safe_doc_ext, is_safe_entity_id};
use fdl_core::whiteboard::Board;
use quiet_sync_proto::api::*;
use quiet_sync_proto::crypto::{Envelope, VaultKeys};
use quiet_sync_proto::entity::{
    sha256_hex, ContentBody, EntityKind, EntityPayload, SyncMeta, INLINE_THRESHOLD,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::adapter::{
    config_entity_id, history_entity_id, is_valid_config_key, local_entity_content_hash,
    parse_config_entity_id, parse_history_entity_id, parse_trash_entity_id, trash_entity_id,
    ApplyReport, IncomingApply, LocalEntity, LocalLibraryAdapter, LOCAL_ONLY_PREF_KEYS,
    LOCAL_ONLY_SETTINGS_KEYS,
};
use crate::credentials::{SecretStore, StoredCredentials};
use crate::http::{ApiClient, ApiError};
use crate::scope::SyncScope;
use crate::state_db::{encode_base, now_ms, BaseSnapshot, EntityStateRow, StateDb};
use crate::ws::{current_platform, spawn_ws_loop, WsConn, WsSignal};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Unconfigured,
    LoggedOut,
    Unlinked,
    Connecting,
    Synced { last_sync_ms: u64 },
    Pending { pending: usize },
    Syncing { pending: usize },
    Offline { pending: usize },
    Paused,
    AuthExpired,
    Error(String),
}

#[derive(Clone, Debug)]
pub enum LocalChangeHint {
    Upsert { kind: EntityKind, entity_id: String },
    Delete { kind: EntityKind, entity_id: String },
    ScanAll,
}

#[derive(Clone, Debug)]
pub enum SyncEvent {
    Status(SyncStatus),
    Presence(Vec<OnlineDevice>),
    Applied(ApplyReport),
    ConflictNotice {
        doc_id: String,
        conflict_doc_id: String,
        title: String,
        device_name: String,
    },
    AuthChanged,
}

#[derive(Clone, Debug)]
pub enum VaultUnlock {
    Passphrase(String),
    RecoveryKey(String),
}

pub enum EngineCommand {
    Notify(Vec<LocalChangeHint>),
    TriggerSync,
    SetPaused(bool),
    SetScope(SyncScope),
    SetSyncInterval(u64),
    UserActivity,
    SetActiveDoc(Option<String>),
    ReloadSession,
    FlushAndStop(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct SharedStatus {
    pub status: Arc<StdRwLock<SyncStatus>>,
    pub online: Arc<StdRwLock<Vec<OnlineDevice>>>,
    pub creds: Arc<StdRwLock<Option<StoredCredentials>>>,
    pub scope: Arc<StdRwLock<SyncScope>>,
    pub paused: Arc<StdRwLock<bool>>,
    pub sync_interval_secs: Arc<StdRwLock<u64>>,
}

pub struct EngineCtx {
    pub db: StateDb,
    pub api: ApiClient,
    pub creds_store: Arc<dyn SecretStore>,
    pub adapter: Arc<dyn LocalLibraryAdapter>,
    pub ws: WsConn,
    pub events: mpsc::UnboundedSender<SyncEvent>,
    pub shared: SharedStatus,
    pub keys: AsyncMutex<Option<VaultKeys>>,
    pub sync_lock: AsyncMutex<()>,
    pub ws_control: AsyncMutex<Option<(tokio::sync::watch::Sender<bool>, tokio::task::JoinHandle<()>)>>,
    pub sig_tx: mpsc::UnboundedSender<WsSignal>,
}

impl EngineCtx {
    pub fn emit_status(&self, st: SyncStatus) {
        *self.shared.status.write().unwrap() = st.clone();
        let _ = self.events.send(SyncEvent::Status(st));
    }

    pub async fn reload_keys_from_creds(&self) {
        let c = self.api.credentials().await;
        *self.shared.creds.write().unwrap() = c.clone();
        let loaded = c.and_then(|c| c.vmk_b64).and_then(|b64| {
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(VaultKeys::from_bytes(arr))
            } else {
                None
            }
        });
        *self.keys.lock().await = loaded;
        let _ = self.events.send(SyncEvent::AuthChanged);
    }

    pub async fn restart_ws(&self) {
        let mut guard = self.ws_control.lock().await;
        if let Some((stop_tx, handle)) = guard.take() {
            let _ = stop_tx.send(true);
            handle.abort();
        }
        let creds = self.api.credentials().await;
        let paused = self.db.paused();
        if let Some(c) = creds
            && c.vault_id.is_some()
            && !paused
        {
            self.emit_status(SyncStatus::Connecting);
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let handle = spawn_ws_loop(self.api.clone(), self.ws.clone(), self.sig_tx.clone(), stop_rx);
            *guard = Some((stop_tx, handle));
        } else if self.api.credentials().await.is_none() {
            self.emit_status(SyncStatus::LoggedOut);
        } else if self.api.credentials().await.is_some_and(|c| c.vault_id.is_none()) {
            self.emit_status(SyncStatus::Unlinked);
        } else if paused {
            self.emit_status(SyncStatus::Paused);
        }
    }

    pub async fn stop_ws(&self) {
        let mut guard = self.ws_control.lock().await;
        if let Some((stop_tx, handle)) = guard.take() {
            let _ = stop_tx.send(true);
            handle.abort();
        }
        *self.shared.online.write().unwrap() = Vec::new();
        let _ = self.events.send(SyncEvent::Presence(Vec::new()));
    }
}

fn effective_keys<'a>(creds: &StoredCredentials, keys: Option<&'a VaultKeys>) -> Result<Option<&'a VaultKeys>> {
    let e2ee_expected = creds.e2ee_setup.is_some() || creds.vmk_b64.is_some();
    if e2ee_expected {
        let k = keys.ok_or_else(|| anyhow::anyhow!("加密文档库密钥未解锁，已拒绝明文操作"))?;
        Ok(Some(k))
    } else {
        Ok(None)
    }
}

fn asset_wire_id(keys: Option<&VaultKeys>, rel_path: &str) -> String {
    match keys {
        Some(k) => k.asset_entity_id(rel_path),
        None => rel_path.to_string(),
    }
}

fn resolve_asset_rel_for_id(db: &StateDb, entity_id: &str) -> Option<String> {
    if let Some(rel) = db.get_asset_rel(entity_id) {
        return Some(rel);
    }
    if entity_id.starts_with(".assets/") || entity_id.starts_with("assets/") {
        return Some(entity_id.to_string());
    }
    if let Ok(Some(st)) = db.get_entity(entity_id)
        && let Ok(Some(snap)) = st.decode_base()
        && let EntityPayload::Asset { rel_path, .. } = snap.payload
    {
        return Some(rel_path);
    }
    None
}

fn scoped_pending_count(ctx: &EngineCtx) -> usize {
    ctx.db.outbox_count_for_scope(ctx.db.scope())
}

fn current_sync_interval(ctx: &EngineCtx) -> u64 {
    *ctx.shared.sync_interval_secs.read().unwrap()
}

fn update_status_after_sync(
    ctx: &EngineCtx,
    ws_connected: bool,
    retry_attempt: &mut u32,
    retry_deadline: &mut Option<tokio::time::Instant>,
) {
    if ctx.db.paused() {
        *retry_deadline = None;
        *retry_attempt = 0;
        ctx.emit_status(SyncStatus::Paused);
        return;
    }
    let pending = scoped_pending_count(ctx);
    if pending == 0 {
        *retry_deadline = None;
        *retry_attempt = 0;
        ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
    } else {
        let secs = (2u64 << (*retry_attempt).min(5)).min(60);
        *retry_attempt = retry_attempt.saturating_add(1);
        *retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
        if ws_connected {
            ctx.emit_status(SyncStatus::Syncing { pending });
        } else {
            ctx.emit_status(SyncStatus::Offline { pending });
        }
    }
}

pub async fn run_engine_loop(
    ctx: Arc<EngineCtx>,
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    mut sig_rx: mpsc::UnboundedReceiver<WsSignal>,
) {
    let mut debounce_deadline: Option<tokio::time::Instant> = None;
    let mut max_deadline: Option<tokio::time::Instant> = None;
    let mut retry_deadline: Option<tokio::time::Instant> = None;
    let mut retry_attempt: u32 = 0;

    ctx.reload_keys_from_creds().await;
    if ctx.api.credentials().await.is_some_and(|c| c.vault_id.is_some()) && !ctx.db.paused() {
        ctx.restart_ws().await;
        let _ = scan_local_into_outbox(&ctx).await;
        let _ = sync_once(&ctx).await;
        let pending = scoped_pending_count(&ctx);
        if pending > 0 {
            retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
        }
    } else if ctx.api.credentials().await.is_some() {
        ctx.emit_status(if ctx.db.paused() { SyncStatus::Paused } else { SyncStatus::Unlinked });
    } else {
        ctx.emit_status(SyncStatus::Unconfigured);
    }

    loop {
        let flush_due = match (debounce_deadline, max_deadline) {
            (Some(d), Some(m)) => Some(d.min(m)),
            (Some(d), None) | (None, Some(d)) => Some(d),
            (None, None) => None,
        };
        let next_tick = match (flush_due, retry_deadline) {
            (Some(a), Some(b)) => Some((a.min(b), a <= b)),
            (Some(a), None) => Some((a, true)),
            (None, Some(b)) => Some((b, false)),
            (None, None) => None,
        };

        tokio::select! {
            _ = async {
                match next_tick {
                    Some((t, _)) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let is_debounce = next_tick.is_some_and(|(_, deb)| deb);
                if is_debounce {
                    debounce_deadline = None;
                    max_deadline = None;
                } else {
                    retry_deadline = None;
                }
                if !ctx.db.paused() {
                    if is_debounce && scoped_pending_count(&ctx) == 0 {
                        continue;
                    }
                    let res = if is_debounce { flush_outbox(&ctx).await } else { sync_once(&ctx).await };
                    let connected = ctx.ws.is_connected().await;
                    match res {
                        Ok(()) => update_status_after_sync(&ctx, connected, &mut retry_attempt, &mut retry_deadline),
                        Err(_) if scoped_pending_count(&ctx) > 0 => {
                            let secs = (2u64 << retry_attempt.min(5)).min(60);
                            retry_attempt = retry_attempt.saturating_add(1);
                            retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(secs));
                        }
                        Err(_) => {}
                    }
                }
            }
            Some(sig) = sig_rx.recv() => {
                match sig {
                    WsSignal::Connected { head_seq, online_devices } => {
                        *ctx.shared.online.write().unwrap() = online_devices.clone();
                        let _ = ctx.events.send(SyncEvent::Presence(online_devices));
                        let local_seq = ctx.db.head_seq();
                        let pending = scoped_pending_count(&ctx);
                        if debounce_deadline.is_some() {
                            if head_seq > local_seq {
                                let _ = pull_only(&ctx).await;
                            }
                            let left = scoped_pending_count(&ctx);
                            if left > 0 {
                                ctx.emit_status(SyncStatus::Pending { pending: left });
                            } else {
                                debounce_deadline = None;
                                max_deadline = None;
                                ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
                            }
                        } else if head_seq > local_seq || pending > 0 {
                            if sync_once(&ctx).await.is_ok() {
                                debounce_deadline = None;
                                max_deadline = None;
                                update_status_after_sync(&ctx, true, &mut retry_attempt, &mut retry_deadline);
                            } else if scoped_pending_count(&ctx) > 0 {
                                retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            }
                        } else {
                            retry_attempt = 0;
                            retry_deadline = None;
                            ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
                        }
                    }
                    WsSignal::Disconnected(err) => {
                        *ctx.shared.online.write().unwrap() = Vec::new();
                        let _ = ctx.events.send(SyncEvent::Presence(Vec::new()));
                        if !ctx.db.paused() && ctx.api.credentials().await.is_some_and(|c| c.vault_id.is_some()) {
                            let pending = scoped_pending_count(&ctx);
                            if pending > 0 && retry_deadline.is_none() && debounce_deadline.is_none() {
                                retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                            }
                            match err {
                                Some(_e) if pending == 0 => ctx.emit_status(SyncStatus::Offline { pending: 0 }),
                                _ => ctx.emit_status(SyncStatus::Offline { pending }),
                            }
                        }
                    }
                    WsSignal::AuthFailed => {
                        ctx.stop_ws().await;
                        retry_deadline = None;
                        ctx.emit_status(SyncStatus::AuthExpired);
                    }
                    WsSignal::Presence(devices) => {
                        *ctx.shared.online.write().unwrap() = devices.clone();
                        let _ = ctx.events.send(SyncEvent::Presence(devices));
                    }
                    WsSignal::Broadcast(item) => {
                        if !ctx.db.paused() {
                            let flush_pending = debounce_deadline.is_none();
                            if let Err(e) = handle_broadcast(&ctx, item, flush_pending).await {
                                log::warn!("sync broadcast error: {e:#}");
                                if flush_pending {
                                    let _ = sync_once(&ctx).await;
                                } else {
                                    let _ = pull_only(&ctx).await;
                                }
                            }
                            if flush_pending && scoped_pending_count(&ctx) > 0 && retry_deadline.is_none() {
                                retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            }
                        }
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    EngineCommand::Notify(hints) => {
                        let Some(creds) = ctx.api.credentials().await else { continue };
                        if creds.vault_id.is_none() {
                            continue;
                        }
                        let keys_guard = ctx.keys.lock().await;
                        let Ok(keys_opt) = effective_keys(&creds, keys_guard.as_ref()) else {
                            continue;
                        };
                        let mut need_scan = false;
                        let scope = ctx.db.scope();
                        let mut maps = Vec::new();
                        for h in hints {
                            match h {
                                LocalChangeHint::Upsert { kind, entity_id } if scope.allows(kind) => {
                                    let wire_id = if kind == EntityKind::Asset {
                                        let w = asset_wire_id(keys_opt, &entity_id);
                                        maps.push((w.clone(), Some(entity_id)));
                                        w
                                    } else {
                                        entity_id
                                    };
                                    let _ = ctx.db.enqueue_outbox(&wire_id, kind, false);
                                }
                                LocalChangeHint::Delete { kind, entity_id } if scope.allows(kind) => {
                                    let wire_id = if kind == EntityKind::Asset {
                                        let w = asset_wire_id(keys_opt, &entity_id);
                                        maps.push((w.clone(), Some(entity_id)));
                                        w
                                    } else {
                                        entity_id
                                    };
                                    let _ = ctx.db.enqueue_outbox(&wire_id, kind, true);
                                }
                                LocalChangeHint::ScanAll => need_scan = true,
                                _ => {}
                            }
                        }
                        drop(keys_guard);
                        if !maps.is_empty() {
                            let _ = ctx.db.commit_remote_batch(&[], &[], &[], &maps, &[], None);
                        }
                        if need_scan {
                            let _ = scan_local_into_outbox(&ctx).await;
                        }
                        let pending = scoped_pending_count(&ctx);
                        if pending > 0 && !ctx.db.paused() {
                            let now = tokio::time::Instant::now();
                            let interval = current_sync_interval(&ctx);
                            if interval == 0 {
                                debounce_deadline = Some(now + Duration::from_millis(300));
                                if max_deadline.is_none() {
                                    max_deadline = Some(now + Duration::from_millis(1500));
                                }
                            } else {
                                debounce_deadline = Some(now + Duration::from_secs(interval));
                                max_deadline = None;
                                if ctx.ws.is_connected().await {
                                    ctx.emit_status(SyncStatus::Pending { pending });
                                } else {
                                    ctx.emit_status(SyncStatus::Offline { pending });
                                }
                            }
                        }
                    }
                    EngineCommand::TriggerSync => {
                        if !ctx.db.paused() {
                            debounce_deadline = None;
                            max_deadline = None;
                            retry_deadline = None;
                            let _ = scan_local_into_outbox(&ctx).await;
                            let _ = sync_once(&ctx).await;
                            if scoped_pending_count(&ctx) > 0 {
                                retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            }
                        }
                    }
                    EngineCommand::SetPaused(paused) => {
                        let _ = ctx.db.set_paused(paused);
                        *ctx.shared.paused.write().unwrap() = paused;
                        if paused {
                            debounce_deadline = None;
                            max_deadline = None;
                            retry_deadline = None;
                            ctx.stop_ws().await;
                            ctx.emit_status(SyncStatus::Paused);
                        } else {
                            retry_attempt = 0;
                            ctx.restart_ws().await;
                            let _ = scan_local_into_outbox(&ctx).await;
                            let _ = sync_once(&ctx).await;
                            if scoped_pending_count(&ctx) > 0 {
                                retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            }
                        }
                    }
                    EngineCommand::SetScope(scope) => {
                        let old_scope = ctx.db.scope();
                        let _ = ctx.db.set_scope(scope);
                        *ctx.shared.scope.write().unwrap() = scope;
                        if !ctx.db.paused() && ctx.api.credentials().await.is_some_and(|c| c.vault_id.is_some()) {
                            let _ = reconcile_newly_enabled_scope(&ctx, old_scope, scope).await;
                            let _ = scan_local_into_outbox(&ctx).await;
                            let _ = sync_once(&ctx).await;
                            if scoped_pending_count(&ctx) > 0 {
                                retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            }
                        }
                    }
                    EngineCommand::SetSyncInterval(secs) => {
                        let clamped = secs.min(crate::state_db::MAX_SYNC_INTERVAL_SECS);
                        let _ = ctx.db.set_sync_interval_secs(clamped);
                        *ctx.shared.sync_interval_secs.write().unwrap() = clamped;
                        if let Some(old) = debounce_deadline
                            && !ctx.db.paused()
                        {
                            let now = tokio::time::Instant::now();
                            if clamped == 0 {
                                debounce_deadline = Some(now + Duration::from_millis(300));
                                max_deadline = Some(now + Duration::from_millis(1500));
                            } else {
                                debounce_deadline = Some(old.min(now + Duration::from_secs(clamped)));
                                max_deadline = None;
                            }
                        }
                    }
                    EngineCommand::UserActivity => {
                        let interval = current_sync_interval(&ctx);
                        if interval > 0 && debounce_deadline.is_some() && !ctx.db.paused() {
                            debounce_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(interval));
                            max_deadline = None;
                        }
                    }
                    EngineCommand::SetActiveDoc(doc_id) => {
                        ctx.ws.send_presence(doc_id).await;
                    }
                    EngineCommand::ReloadSession => {
                        ctx.reload_keys_from_creds().await;
                        retry_attempt = 0;
                        retry_deadline = None;
                        if ctx.api.credentials().await.is_some_and(|c| c.vault_id.is_some()) {
                            if !ctx.db.paused() {
                                ctx.restart_ws().await;
                                let _ = scan_local_into_outbox(&ctx).await;
                                let _ = sync_once(&ctx).await;
                                if scoped_pending_count(&ctx) > 0 {
                                    retry_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                                }
                            } else {
                                ctx.emit_status(SyncStatus::Paused);
                            }
                        } else {
                            ctx.stop_ws().await;
                        }
                    }
                    EngineCommand::FlushAndStop(done) => {
                        if !ctx.db.paused() && ctx.api.credentials().await.is_some_and(|c| c.vault_id.is_some()) {
                            let _ = tokio::time::timeout(Duration::from_millis(800), flush_outbox(&ctx)).await;
                        }
                        ctx.stop_ws().await;
                        let _ = done.send(());
                        break;
                    }
                }
            }
            else => break,
        }
    }
}

pub async fn sync_once(ctx: &Arc<EngineCtx>) -> Result<()> {
    let _guard = ctx.sync_lock.lock().await;
    let Some(creds) = ctx.api.credentials().await else {
        ctx.emit_status(SyncStatus::LoggedOut);
        return Ok(());
    };
    let Some(vault_id) = creds.vault_id.clone() else {
        ctx.emit_status(SyncStatus::Unlinked);
        return Ok(());
    };
    if ctx.db.paused() {
        ctx.emit_status(SyncStatus::Paused);
        return Ok(());
    }

    let pending = scoped_pending_count(ctx);
    ctx.emit_status(SyncStatus::Syncing { pending });

    let result: Result<()> = async {
        pull_remote_changes(ctx, &vault_id, &creds.device_name).await?;
        flush_outbox_inner(ctx, &vault_id, &creds.device_id, &creds.device_name).await?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            if ctx.db.paused() {
                ctx.emit_status(SyncStatus::Paused);
            } else {
                let left = scoped_pending_count(ctx);
                if left == 0 {
                    ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
                } else {
                    ctx.emit_status(SyncStatus::Syncing { pending: left });
                }
            }
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            let left = scoped_pending_count(ctx);
            if e.downcast_ref::<ApiError>().is_some_and(|ae| matches!(ae, ApiError::AuthExpired)) {
                ctx.stop_ws().await;
                ctx.emit_status(SyncStatus::AuthExpired);
            } else if e.downcast_ref::<ApiError>().is_some_and(|ae| matches!(ae, ApiError::Network(_))) {
                ctx.emit_status(SyncStatus::Offline { pending: left });
            } else {
                ctx.emit_status(SyncStatus::Error(msg));
            }
            Err(e)
        }
    }
}

async fn pull_only(ctx: &Arc<EngineCtx>) -> Result<()> {
    let _guard = ctx.sync_lock.lock().await;
    let Some(creds) = ctx.api.credentials().await else { return Ok(()) };
    let Some(vault_id) = creds.vault_id.clone() else { return Ok(()) };
    if ctx.db.paused() {
        return Ok(());
    }
    pull_remote_changes(ctx, &vault_id, &creds.device_name).await
}

async fn handle_broadcast(ctx: &Arc<EngineCtx>, item: RemoteChange, flush_pending: bool) -> Result<()> {
    let _guard = ctx.sync_lock.lock().await;
    let Some(creds) = ctx.api.credentials().await else { return Ok(()) };
    let Some(vault_id) = creds.vault_id.clone() else { return Ok(()) };
    if ctx.db.paused() {
        return Ok(());
    }

    let cur_seq = ctx.db.head_seq();
    if item.seq <= cur_seq {
        return Ok(());
    }
    if item.seq > cur_seq + 1 {
        // Missed intermediate sequence(s): do a full paginated pull.
        pull_remote_changes(ctx, &vault_id, &creds.device_name).await?;
    } else {
        let seq = item.seq;
        apply_remote_batch_with_head(ctx, &vault_id, &creds.device_name, vec![item], Some(seq)).await?;
    }
    if flush_pending && scoped_pending_count(ctx) > 0 && !ctx.db.paused() {
        flush_outbox_inner(ctx, &vault_id, &creds.device_id, &creds.device_name).await?;
    }
    if ctx.db.paused() {
        ctx.emit_status(SyncStatus::Paused);
    } else {
        let left = scoped_pending_count(ctx);
        if left == 0 {
            ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
        } else if flush_pending {
            ctx.emit_status(SyncStatus::Syncing { pending: left });
        } else {
            ctx.emit_status(SyncStatus::Pending { pending: left });
        }
    }
    Ok(())
}

async fn flush_outbox(ctx: &Arc<EngineCtx>) -> Result<()> {
    let _guard = ctx.sync_lock.lock().await;
    let Some(creds) = ctx.api.credentials().await else { return Ok(()) };
    let Some(vault_id) = creds.vault_id.clone() else { return Ok(()) };
    if ctx.db.paused() {
        ctx.emit_status(SyncStatus::Paused);
        return Ok(());
    }
    let pending = scoped_pending_count(ctx);
    if pending == 0 {
        return Ok(());
    }
    ctx.emit_status(SyncStatus::Syncing { pending });
    match flush_outbox_inner(ctx, &vault_id, &creds.device_id, &creds.device_name).await {
        Ok(()) => {
            if ctx.db.paused() {
                ctx.emit_status(SyncStatus::Paused);
            } else {
                let left = scoped_pending_count(ctx);
                if left == 0 {
                    ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
                } else {
                    ctx.emit_status(SyncStatus::Syncing { pending: left });
                }
            }
            Ok(())
        }
        Err(e) => {
            let left = scoped_pending_count(ctx);
            if e.downcast_ref::<ApiError>().is_some_and(|ae| matches!(ae, ApiError::AuthExpired)) {
                ctx.stop_ws().await;
                ctx.emit_status(SyncStatus::AuthExpired);
            } else if e.downcast_ref::<ApiError>().is_some_and(|ae| matches!(ae, ApiError::Network(_))) {
                ctx.emit_status(SyncStatus::Offline { pending: left });
            } else {
                ctx.emit_status(SyncStatus::Error(e.to_string()));
            }
            Err(e)
        }
    }
}

pub async fn initial_sync(ctx: &Arc<EngineCtx>, vault_id: &str) -> Result<()> {
    let _guard = ctx.sync_lock.lock().await;
    let Some(creds) = ctx.api.credentials().await else {
        anyhow::bail!("未登录");
    };
    ctx.emit_status(SyncStatus::Syncing { pending: 0 });
    let same_vault = ctx.db.vault_id().as_deref() == Some(vault_id);
    if !same_vault {
        ctx.db.reset_vault_state()?;
    }
    ctx.db.set_vault_id(vault_id)?;

    let snapshot = ctx.api.get_snapshot(vault_id).await?;
    let keys = ctx.keys.lock().await.clone();
    let keys_opt = effective_keys(&creds, keys.as_ref())?;
    let e2ee_expected = keys_opt.is_some();
    let scope = ctx.db.scope();

    let mut remote_present_ids = HashSet::new();
    let mut decoded_remote = Vec::new();
    for item in snapshot.items {
        remote_present_ids.insert(item.entity_id.clone());
        if !scope.allows(item.kind) {
            continue;
        }
        if let Some(d) = decode_remote_item(ctx, vault_id, keys_opt, e2ee_expected, item).await? {
            decoded_remote.push(d);
        }
    }

    // Read local inventory to match & deduplicate initial library items.
    let mut inv = ctx.adapter.read_inventory().await?;
    let mut local_folder_by_rel: HashMap<String, String> = inv.folders.iter().map(|f| (f.rel.clone(), f.id.clone())).collect();

    // Build folder rel map from decoded remote folders so we can compute remote doc relative paths.
    let mut remote_folders_by_id: HashMap<String, (String, Option<String>)> = HashMap::new();
    for d in &decoded_remote {
        if let Some(EntityPayload::Folder { name, parent_id, .. }) = &d.payload {
            remote_folders_by_id.insert(d.change.entity_id.clone(), (name.clone(), parent_id.clone()));
        }
    }
    let remote_rel_of = |name: &str, parent_id: Option<&str>| -> String {
        let mut parts = vec![fdl_core::store::file_name_for(name)];
        let mut cur = parent_id;
        let mut guard = 0usize;
        while let Some(pid) = cur
            && guard < 64
        {
            guard += 1;
            if let Some((fname, fparent)) = remote_folders_by_id.get(pid) {
                parts.push(fdl_core::store::file_name_for(fname));
                cur = fparent.as_deref();
            } else {
                break;
            }
        }
        parts.reverse();
        parts.join("/")
    };

    let mut adopt_clean_ids = HashSet::new();

    // Match folders first (only for entities whose UUID does not already exist locally).
    let local_folder_ids: HashSet<String> = inv.folders.iter().map(|f| f.id.clone()).collect();
    for d in &decoded_remote {
        if let Some(EntityPayload::Folder { name, parent_id, .. }) = &d.payload
            && !local_folder_ids.contains(&d.change.entity_id)
        {
            let r_rel = remote_rel_of(name, parent_id.as_deref());
            if let Some(local_id) = local_folder_by_rel.remove(&r_rel) {
                ctx.adapter.adopt_id(&local_id, &d.change.entity_id).await?;
                adopt_clean_ids.insert(d.change.entity_id.clone());
            }
        }
    }
    inv = ctx.adapter.read_inventory().await?;
    let mut local_doc_by_rel: HashMap<String, String> = inv.docs.iter().map(|d| (d.rel.clone(), d.id.clone())).collect();
    let local_doc_ids: HashSet<String> = inv.docs.iter().map(|d| d.id.clone()).collect();

    for d in &decoded_remote {
        if let Some(EntityPayload::Document { name, parent_id, ext, .. }) = &d.payload {
            if local_doc_ids.contains(&d.change.entity_id) {
                continue;
            }
            let stem = name.strip_suffix(&format!(".{ext}")).unwrap_or(name);
            let mut r_rel = remote_rel_of(stem, parent_id.as_deref());
            r_rel.push('.');
            r_rel.push_str(ext);
            if let Some(local_id) = local_doc_by_rel.remove(&r_rel)
                && let Some(LocalEntity::Document { bytes: local_bytes, .. }) = ctx.adapter.read_entity(EntityKind::Document, &local_id).await?
            {
                let remote_bytes = d.raw_bytes.as_deref().unwrap_or(&[]);
                if sha256_hex(&local_bytes) != sha256_hex(remote_bytes) {
                    let _ = ctx
                        .adapter
                        .create_conflict_copy(&local_id, ext, &local_bytes, &format!("{} 本机初始", creds.device_name))
                        .await?;
                }
                ctx.adapter.adopt_id(&local_id, &d.change.entity_id).await?;
                adopt_clean_ids.insert(d.change.entity_id.clone());
            }
        }
    }

    // Apply all remote items via full merge/TOCTOU pipeline (preserving same-UUID offline edits on relink).
    apply_decoded_remote_batch(
        ctx,
        vault_id,
        &creds.device_name,
        keys_opt,
        decoded_remote,
        Some(snapshot.head_seq),
        &adopt_clean_ids,
    )
    .await?;

    if same_vault {
        reconcile_missing_from_snapshot(ctx, &creds.device_name, keys_opt, scope, &remote_present_ids, None).await?;
    }

    // Scan local library for everything not yet matched and push!
    scan_local_into_outbox_locked(ctx).await?;
    if !ctx.db.paused() {
        flush_outbox_inner(ctx, vault_id, &creds.device_id, &creds.device_name).await?;
    }
    if ctx.db.paused() {
        ctx.emit_status(SyncStatus::Paused);
    } else {
        let left = scoped_pending_count(ctx);
        if left == 0 {
            ctx.emit_status(SyncStatus::Synced { last_sync_ms: now_ms() });
        } else {
            ctx.emit_status(SyncStatus::Syncing { pending: left });
        }
    }
    Ok(())
}

async fn reconcile_newly_enabled_scope(ctx: &Arc<EngineCtx>, old: SyncScope, new: SyncScope) -> Result<()> {
    let mut enabled_kinds = HashSet::new();
    if !old.trash && new.trash {
        enabled_kinds.insert(EntityKind::TrashItem);
    }
    if !old.history && new.history {
        enabled_kinds.insert(EntityKind::HistorySnapshot);
    }
    if !old.settings && new.settings {
        enabled_kinds.insert(EntityKind::Config);
    }
    if enabled_kinds.is_empty() {
        return Ok(());
    }
    let _guard = ctx.sync_lock.lock().await;
    let Some(creds) = ctx.api.credentials().await else { return Ok(()) };
    let Some(vault_id) = creds.vault_id.clone() else { return Ok(()) };
    let snapshot = ctx.api.get_snapshot(&vault_id).await?;
    let keys = ctx.keys.lock().await.clone();
    let keys_opt = effective_keys(&creds, keys.as_ref())?;
    let mut remote_present = HashSet::new();
    let mut filtered = Vec::new();
    for item in snapshot.items {
        if enabled_kinds.contains(&item.kind) {
            remote_present.insert(item.entity_id.clone());
            filtered.push(item);
        }
    }
    apply_remote_batch_with_head(ctx, &vault_id, &creds.device_name, filtered, None).await?;
    reconcile_missing_from_snapshot(ctx, &creds.device_name, keys_opt, new, &remote_present, Some(&enabled_kinds)).await?;
    Ok(())
}

pub(crate) async fn scan_local_into_outbox(ctx: &Arc<EngineCtx>) -> Result<()> {
    let _guard = ctx.sync_lock.lock().await;
    scan_local_into_outbox_locked(ctx).await
}

async fn scan_local_into_outbox_locked(ctx: &Arc<EngineCtx>) -> Result<()> {
    let Some(creds) = ctx.api.credentials().await else { return Ok(()) };
    if creds.vault_id.is_none() {
        return Ok(());
    }
    let keys = ctx.keys.lock().await.clone();
    let keys_opt = effective_keys(&creds, keys.as_ref())?;

    let inv = ctx.adapter.read_inventory().await?;
    let states = ctx.db.all_entities()?;
    let scope = ctx.db.scope();

    let mut live_ids: HashSet<(EntityKind, String)> = HashSet::new();
    let mut asset_maps: Vec<(String, Option<String>)> = Vec::new();

    for f in &inv.folders {
        live_ids.insert((EntityKind::Folder, f.id.clone()));
        let le = LocalEntity::Folder {
            id: f.id.clone(),
            name: f.name.clone(),
            parent_id: f.parent_id.clone(),
            meta: crate::adapter::sync_meta_from_store(&f.meta),
        };
        let hash = local_entity_content_hash(&le);
        if states.get(&f.id).is_none_or(|st| st.deleted || st.base_hash != hash) {
            ctx.db.enqueue_outbox(&f.id, EntityKind::Folder, false)?;
        }
    }

    for rel in &inv.assets {
        let wire_id = asset_wire_id(keys_opt, rel);
        asset_maps.push((wire_id.clone(), Some(rel.clone())));
        live_ids.insert((EntityKind::Asset, wire_id.clone()));
        if let Some(le) = ctx.adapter.read_entity(EntityKind::Asset, rel).await? {
            let hash = local_entity_content_hash(&le);
            if states.get(&wire_id).is_none_or(|st| st.deleted || st.base_hash != hash) {
                ctx.db.enqueue_outbox(&wire_id, EntityKind::Asset, false)?;
            }
        }
    }

    for d in &inv.docs {
        live_ids.insert((EntityKind::Document, d.id.clone()));
        if let Some(le) = ctx.adapter.read_entity(EntityKind::Document, &d.id).await? {
            let hash = local_entity_content_hash(&le);
            if states.get(&d.id).is_none_or(|st| st.deleted || st.base_hash != hash) {
                ctx.db.enqueue_outbox(&d.id, EntityKind::Document, false)?;
            }
        }
    }

    if scope.trash {
        for t in &inv.trash {
            let eid = trash_entity_id(&t.id);
            live_ids.insert((EntityKind::TrashItem, eid.clone()));
            if let Some(le) = ctx.adapter.read_entity(EntityKind::TrashItem, &eid).await? {
                let hash = local_entity_content_hash(&le);
                if states.get(&eid).is_none_or(|st| st.deleted || st.base_hash != hash) {
                    ctx.db.enqueue_outbox(&eid, EntityKind::TrashItem, false)?;
                }
            }
        }
    }

    if scope.history {
        for h in &inv.history {
            let eid = history_entity_id(&h.doc_id, &h.name);
            live_ids.insert((EntityKind::HistorySnapshot, eid.clone()));
            if let Some(le) = ctx.adapter.read_entity(EntityKind::HistorySnapshot, &eid).await? {
                let hash = local_entity_content_hash(&le);
                if states.get(&eid).is_none_or(|st| st.deleted || st.base_hash != hash) {
                    ctx.db.enqueue_outbox(&eid, EntityKind::HistorySnapshot, false)?;
                }
            }
        }
    }

    if scope.settings {
        let mut cfg_keys = vec!["preferences".to_string(), "shortcuts".to_string(), "library_settings".to_string()];
        for t in &inv.themes {
            cfg_keys.push(format!("theme:{t}"));
        }
        for key in cfg_keys {
            let eid = config_entity_id(&key);
            if let Some(le) = ctx.adapter.read_entity(EntityKind::Config, &eid).await? {
                live_ids.insert((EntityKind::Config, eid.clone()));
                let hash = local_entity_content_hash(&le);
                if states.get(&eid).is_none_or(|st| st.deleted || st.base_hash != hash) {
                    ctx.db.enqueue_outbox(&eid, EntityKind::Config, false)?;
                }
            }
        }
    }

    if !asset_maps.is_empty() {
        ctx.db.commit_remote_batch(&[], &[], &[], &asset_maps, &[], None)?;
    }

    // Anything previously tracked (and not deleted) that disappeared locally needs deletion pushed.
    for (eid, st) in &states {
        if st.deleted || !scope.allows(st.kind) {
            continue;
        }
        if !live_ids.contains(&(st.kind, eid.clone())) {
            ctx.db.enqueue_outbox(eid, st.kind, true)?;
        }
    }

    Ok(())
}

async fn pull_remote_changes(ctx: &Arc<EngineCtx>, vault_id: &str, device_name: &str) -> Result<()> {
    loop {
        let since = ctx.db.head_seq();
        let resp = ctx.api.get_changes(vault_id, since, 200).await?;
        if resp.resync_required {
            let snap = ctx.api.get_snapshot(vault_id).await?;
            let remote_present: HashSet<String> = snap.items.iter().map(|i| i.entity_id.clone()).collect();
            let creds = ctx.api.credentials().await;
            let keys = ctx.keys.lock().await.clone();
            let keys_opt = match creds.as_ref() {
                Some(c) => effective_keys(c, keys.as_ref())?,
                None => None,
            };
            let scope = ctx.db.scope();
            apply_remote_batch_with_head(ctx, vault_id, device_name, snap.items, Some(snap.head_seq)).await?;
            reconcile_missing_from_snapshot(ctx, device_name, keys_opt, scope, &remote_present, None).await?;
            break;
        }
        if resp.changes.is_empty() {
            if resp.head_seq > since {
                ctx.db.set_head_seq(resp.head_seq)?;
            }
            break;
        }
        let max_seq = resp.changes.last().map_or(since, |c| c.seq);
        apply_remote_batch_with_head(ctx, vault_id, device_name, resp.changes, Some(max_seq)).await?;
        if !resp.has_more {
            break;
        }
    }
    Ok(())
}

struct DecodedRemote {
    change: RemoteChange,
    payload: Option<EntityPayload>,
    raw_bytes: Option<Vec<u8>>,
}

fn is_safe_asset_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > 1024 || rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return false;
    }
    if !(rel.starts_with(".assets/") || rel.starts_with("assets/")) {
        return false;
    }
    rel.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

async fn decode_remote_item(
    ctx: &Arc<EngineCtx>,
    vault_id: &str,
    keys: Option<&VaultKeys>,
    e2ee_expected: bool,
    change: RemoteChange,
) -> Result<Option<DecodedRemote>> {
    match change.kind {
        EntityKind::Folder | EntityKind::Document => {
            anyhow::ensure!(is_safe_entity_id(&change.entity_id), "无效的远端实体 ID: {}", change.entity_id);
        }
        EntityKind::TrashItem => {
            let tid = parse_trash_entity_id(&change.entity_id)
                .ok_or_else(|| anyhow::anyhow!("无效的远端回收站 ID: {}", change.entity_id))?;
            anyhow::ensure!(is_safe_entity_id(tid), "非法的回收站目标 ID: {tid}");
        }
        EntityKind::HistorySnapshot => {
            anyhow::ensure!(
                parse_history_entity_id(&change.entity_id).is_some(),
                "无效的远端历史快照 ID: {}",
                change.entity_id
            );
        }
        EntityKind::Config => {
            anyhow::ensure!(
                parse_config_entity_id(&change.entity_id).is_some(),
                "无效的远端配置 ID: {}",
                change.entity_id
            );
        }
        EntityKind::Asset => {
            if e2ee_expected {
                let valid = change
                    .entity_id
                    .strip_prefix("asset:")
                    .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()));
                anyhow::ensure!(valid, "无效的加密附件 ID: {}", change.entity_id);
            } else {
                anyhow::ensure!(is_safe_asset_rel(&change.entity_id), "非法的远端附件路径: {}", change.entity_id);
            }
        }
    }

    let encoded = if e2ee_expected {
        let k = keys.ok_or_else(|| anyhow::anyhow!("加密文档库密钥未解锁，已拒绝远端条目"))?;
        let env = change
            .envelope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("加密文档库缺少认证信封: {}", change.entity_id))?;
        if change.deleted {
            k.verify_tombstone(vault_id, &change.entity_id, change.kind, change.version, env)?;
            return Ok(Some(DecodedRemote { change, payload: None, raw_bytes: None }));
        }
        k.decrypt_entity(vault_id, &change.entity_id, change.kind, change.version, env)?
    } else {
        if change.deleted || change.envelope.is_none() {
            return Ok(Some(DecodedRemote { change, payload: None, raw_bytes: None }));
        }
        change.envelope.as_ref().unwrap().decode_plain()?
    };

    let payload = EntityPayload::decode_bytes(&encoded)?;
    anyhow::ensure!(
        payload.kind() == change.kind,
        "远端负载类型与外层声明不匹配: {}",
        change.entity_id
    );
    match &payload {
        EntityPayload::Document { ext, .. } => {
            anyhow::ensure!(is_safe_doc_ext(ext), "非法的文档扩展名: {ext}");
        }
        EntityPayload::TrashItem { ext, .. } => {
            anyhow::ensure!(ext.is_empty() || is_safe_doc_ext(ext), "非法的回收站扩展名: {ext}");
        }
        EntityPayload::Asset { rel_path, .. } => {
            anyhow::ensure!(is_safe_asset_rel(rel_path), "非法的附件相对路径: {rel_path}");
            anyhow::ensure!(
                asset_wire_id(keys, rel_path) == change.entity_id,
                "附件实体 ID 与负载路径不匹配: {}",
                change.entity_id
            );
        }
        EntityPayload::HistorySnapshot { doc_id, name, .. } => {
            anyhow::ensure!(
                history_entity_id(doc_id, name) == change.entity_id,
                "历史快照 ID 与负载不匹配: {}",
                change.entity_id
            );
        }
        EntityPayload::Config { key, .. } => {
            anyhow::ensure!(
                is_valid_config_key(key) && config_entity_id(key) == change.entity_id,
                "配置 ID 与键名不匹配: {}",
                change.entity_id
            );
        }
        EntityPayload::Folder { .. } => {}
    }

    let raw_bytes = match &payload {
        EntityPayload::Document { content, .. }
        | EntityPayload::HistorySnapshot { content, .. }
        | EntityPayload::Config { content, .. } => Some(resolve_content_bytes(ctx, vault_id, keys, content).await?),
        EntityPayload::TrashItem { content: Some(content), .. } => Some(resolve_content_bytes(ctx, vault_id, keys, content).await?),
        EntityPayload::Asset { blob_hash, raw_sha256, .. } => {
            let raw = fetch_and_decrypt_blob(ctx, vault_id, keys, blob_hash, raw_sha256).await?;
            Some(raw)
        }
        EntityPayload::Folder { .. } | EntityPayload::TrashItem { content: None, .. } => None,
    };
    Ok(Some(DecodedRemote { change, payload: Some(payload), raw_bytes }))
}

async fn resolve_content_bytes(ctx: &Arc<EngineCtx>, vault_id: &str, keys: Option<&VaultKeys>, body: &ContentBody) -> Result<Vec<u8>> {
    match body {
        ContentBody::Inline { .. } => Ok(body.decode_inline()?.unwrap_or_default()),
        ContentBody::Blob { hash, raw_sha256, .. } => fetch_and_decrypt_blob(ctx, vault_id, keys, hash, raw_sha256).await,
    }
}

async fn fetch_and_decrypt_blob(ctx: &Arc<EngineCtx>, vault_id: &str, keys: Option<&VaultKeys>, blob_hash: &str, raw_sha256: &str) -> Result<Vec<u8>> {
    let wire = ctx.api.get_blob(vault_id, blob_hash).await?;
    let raw = match keys {
        Some(k) => k.decrypt_blob(vault_id, &wire)?,
        None => wire,
    };
    anyhow::ensure!(sha256_hex(&raw) == raw_sha256, "downloaded blob checksum mismatch");
    Ok(raw)
}

fn non_folder_apply_order(k: EntityKind, deleted: bool) -> u8 {
    match (k, deleted) {
        (EntityKind::Asset, false) => 0,
        (EntityKind::Document, false) => 1,
        (EntityKind::TrashItem, false) => 2,
        (EntityKind::HistorySnapshot, false) => 3,
        (EntityKind::Config, false) => 4,
        (EntityKind::Config, true) => 5,
        (EntityKind::HistorySnapshot, true) => 6,
        (EntityKind::TrashItem, true) => 7,
        (EntityKind::Document, true) => 8,
        (EntityKind::Asset, true) => 9,
        (EntityKind::Folder, false) => 0,
        (EntityKind::Folder, true) => 10,
    }
}

fn sort_decoded_topologically(items: Vec<DecodedRemote>, db: &StateDb) -> Vec<DecodedRemote> {
    let mut active_folders = Vec::new();
    let mut non_folders = Vec::new();
    let mut deleted_folders = Vec::new();

    for d in items {
        if d.change.kind == EntityKind::Folder {
            if d.change.deleted {
                deleted_folders.push(d);
            } else {
                active_folders.push(d);
            }
        } else {
            non_folders.push(d);
        }
    }

    // Topological sort active folders: parents before children (tie-break by seq asc).
    let af_sorted = if active_folders.len() <= 1 {
        active_folders
    } else {
        let mut id_to_idx = HashMap::with_capacity(active_folders.len());
        for (i, d) in active_folders.iter().enumerate() {
            id_to_idx.insert(d.change.entity_id.clone(), i);
        }
        let mut in_deg = vec![0usize; active_folders.len()];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); active_folders.len()];
        for (i, d) in active_folders.iter().enumerate() {
            if let Some(EntityPayload::Folder { parent_id: Some(pid), .. }) = &d.payload
                && let Some(&p_idx) = id_to_idx.get(pid)
                && p_idx != i
            {
                in_deg[i] += 1;
                children[p_idx].push(i);
            }
        }
        let mut heap = std::collections::BinaryHeap::new();
        for (i, &deg) in in_deg.iter().enumerate() {
            if deg == 0 {
                heap.push(std::cmp::Reverse((active_folders[i].change.seq, i)));
            }
        }
        let mut order = Vec::with_capacity(active_folders.len());
        let mut seen = vec![false; active_folders.len()];
        while let Some(std::cmp::Reverse((_, u))) = heap.pop() {
            if seen[u] {
                continue;
            }
            seen[u] = true;
            order.push(u);
            for &v in &children[u] {
                in_deg[v] -= 1;
                if in_deg[v] == 0 {
                    heap.push(std::cmp::Reverse((active_folders[v].change.seq, v)));
                }
            }
        }
        let mut rem: Vec<usize> = (0..active_folders.len()).filter(|i| !seen[*i]).collect();
        rem.sort_by_key(|&i| active_folders[i].change.seq);
        order.extend(rem);
        let mut slots: Vec<Option<DecodedRemote>> = active_folders.into_iter().map(Some).collect();
        order.into_iter().filter_map(|i| slots[i].take()).collect()
    };

    non_folders.sort_by_key(|d| (non_folder_apply_order(d.change.kind, d.change.deleted), d.change.seq));

    // Topological sort deleted folders: children before parents using local StateDb parent info.
    let df_sorted = if deleted_folders.len() <= 1 {
        deleted_folders
    } else {
        let mut id_to_idx = HashMap::with_capacity(deleted_folders.len());
        for (i, d) in deleted_folders.iter().enumerate() {
            id_to_idx.insert(d.change.entity_id.clone(), i);
        }
        let mut in_deg = vec![0usize; deleted_folders.len()];
        let mut parents_waiting: Vec<Vec<usize>> = vec![Vec::new(); deleted_folders.len()];
        for (i, d) in deleted_folders.iter().enumerate() {
            let pid_opt = db
                .get_entity(&d.change.entity_id)
                .ok()
                .flatten()
                .and_then(|st| st.decode_base().ok().flatten())
                .and_then(|b| match b.payload {
                    EntityPayload::Folder { parent_id, .. } => parent_id,
                    _ => None,
                });
            if let Some(pid) = pid_opt
                && let Some(&p_idx) = id_to_idx.get(&pid)
                && p_idx != i
            {
                in_deg[p_idx] += 1;
                parents_waiting[i].push(p_idx);
            }
        }
        let mut heap = std::collections::BinaryHeap::new();
        for (i, &deg) in in_deg.iter().enumerate() {
            if deg == 0 {
                heap.push(std::cmp::Reverse((std::cmp::Reverse(deleted_folders[i].change.seq), i)));
            }
        }
        let mut order = Vec::with_capacity(deleted_folders.len());
        let mut seen = vec![false; deleted_folders.len()];
        while let Some(std::cmp::Reverse((_, u))) = heap.pop() {
            if seen[u] {
                continue;
            }
            seen[u] = true;
            order.push(u);
            for &p in &parents_waiting[u] {
                in_deg[p] -= 1;
                if in_deg[p] == 0 {
                    heap.push(std::cmp::Reverse((std::cmp::Reverse(deleted_folders[p].change.seq), p)));
                }
            }
        }
        let mut rem: Vec<usize> = (0..deleted_folders.len()).filter(|i| !seen[*i]).collect();
        rem.sort_by_key(|&i| std::cmp::Reverse(deleted_folders[i].change.seq));
        order.extend(rem);
        let mut slots: Vec<Option<DecodedRemote>> = deleted_folders.into_iter().map(Some).collect();
        order.into_iter().filter_map(|i| slots[i].take()).collect()
    };

    let mut out = Vec::with_capacity(af_sorted.len() + non_folders.len() + df_sorted.len());
    out.extend(af_sorted);
    out.extend(non_folders);
    out.extend(df_sorted);
    out
}

fn remote_local_entity_equiv(change: &RemoteChange, payload: Option<&EntityPayload>, raw_bytes: Option<&[u8]>) -> Option<LocalEntity> {
    let p = payload?;
    Some(match p {
        EntityPayload::Folder { name, parent_id, meta } => LocalEntity::Folder {
            id: change.entity_id.clone(),
            name: name.clone(),
            parent_id: parent_id.clone(),
            meta: meta.clone(),
        },
        EntityPayload::Document { name, parent_id, ext, meta, .. } => {
            let bytes = raw_bytes.unwrap_or_default().to_vec();
            let asset_refs = fdl_core::store::Store::doc_asset_refs_from_bytes(ext, &bytes);
            LocalEntity::Document {
                id: change.entity_id.clone(),
                file_name: name.clone(),
                parent_id: parent_id.clone(),
                ext: ext.clone(),
                meta: meta.clone(),
                bytes,
                asset_refs,
            }
        }
        EntityPayload::Asset { rel_path, mime, .. } => LocalEntity::Asset {
            rel_path: rel_path.clone(),
            bytes: raw_bytes.unwrap_or_default().to_vec(),
            mime: mime.clone(),
        },
        EntityPayload::TrashItem { orig_rel, folder, ext, deleted, root, meta, .. } => LocalEntity::TrashItem {
            id: parse_trash_entity_id(&change.entity_id)?.to_string(),
            orig_rel: orig_rel.clone(),
            folder: folder.clone(),
            ext: ext.clone(),
            deleted: *deleted,
            root: root.clone(),
            meta: meta.clone(),
            bytes: raw_bytes.map(|b| b.to_vec()),
        },
        EntityPayload::HistorySnapshot { doc_id, name, trashed, label, .. } => LocalEntity::HistorySnapshot {
            doc_id: doc_id.clone(),
            name: name.clone(),
            trashed: *trashed,
            label: label.clone(),
            bytes: raw_bytes.unwrap_or_default().to_vec(),
        },
        EntityPayload::Config { key, .. } => LocalEntity::Config {
            key: key.clone(),
            bytes: raw_bytes.unwrap_or_default().to_vec(),
        },
    })
}

async fn apply_remote_batch_with_head(
    ctx: &Arc<EngineCtx>,
    vault_id: &str,
    device_name: &str,
    changes: Vec<RemoteChange>,
    new_head_seq: Option<u64>,
) -> Result<()> {
    let creds = ctx.api.credentials().await;
    let keys = ctx.keys.lock().await.clone();
    let keys_opt = match creds.as_ref() {
        Some(c) => effective_keys(c, keys.as_ref())?,
        None => keys.as_ref(),
    };
    let e2ee_expected = creds.as_ref().is_some_and(|c| c.e2ee_setup.is_some() || c.vmk_b64.is_some()) || keys_opt.is_some();
    let scope = ctx.db.scope();

    // Coalesce multiple versions/entries per entity_id, keeping max (version, seq).
    let mut latest: HashMap<String, RemoteChange> = HashMap::new();
    for c in changes {
        if !scope.allows(c.kind) {
            continue;
        }
        match latest.entry(c.entity_id.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(c);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let cur = e.get();
                if (c.version, c.seq) > (cur.version, cur.seq) {
                    e.insert(c);
                }
            }
        }
    }

    let mut decoded = Vec::new();
    for (_, c) in latest {
        // Monotonic entity version guard: never regress to already-applied versions.
        if let Some(st) = ctx.db.get_entity(&c.entity_id)?
            && c.version <= st.server_version
        {
            continue;
        }
        if let Some(d) = decode_remote_item(ctx, vault_id, keys_opt, e2ee_expected, c).await? {
            decoded.push(d);
        }
    }

    let adopt_clean = HashSet::new();
    apply_decoded_remote_batch(ctx, vault_id, device_name, keys_opt, decoded, new_head_seq, &adopt_clean).await
}

async fn is_doc_locally_dirty(ctx: &Arc<EngineCtx>, doc_id: &str, outbox_ids: &HashSet<String>) -> Result<bool> {
    if outbox_ids.contains(doc_id) {
        return Ok(true);
    }
    let Some(local) = ctx.adapter.read_entity(EntityKind::Document, doc_id).await? else {
        return Ok(false);
    };
    let lh = local_entity_content_hash(&local);
    Ok(match ctx.db.get_entity(doc_id)? {
        Some(st) => st.deleted || st.base_hash != lh,
        None => true,
    })
}

async fn apply_decoded_remote_batch(
    ctx: &Arc<EngineCtx>,
    _vault_id: &str,
    device_name: &str,
    keys_opt: Option<&VaultKeys>,
    decoded: Vec<DecodedRemote>,
    new_head_seq: Option<u64>,
    adopt_clean_ids: &HashSet<String>,
) -> Result<()> {
    if decoded.is_empty() {
        if let Some(seq) = new_head_seq {
            ctx.db.commit_remote_batch(&[], &[], &[], &[], &[], Some(seq))?;
        }
        return Ok(());
    }

    struct PlannedCommit {
        entity_id: String,
        upsert: Option<EntityStateRow>,
        remove_outbox: bool,
        enqueue_outbox: Vec<(String, EntityKind, bool)>,
        asset_maps: Vec<(String, Option<String>)>,
        blob_mark: Option<(String, String, u64)>,
        conflict_notice: Option<SyncEvent>,
        applied_folder_id: Option<String>,
    }

    let scope = ctx.db.scope();
    let mut pending = sort_decoded_topologically(decoded, &ctx.db);
    let mut all_applied_folders = HashSet::new();
    let mut just_applied_ids = HashSet::new();

    for attempt in 0..3 {
        let outbox_ids: HashSet<String> = ctx.db.list_outbox()?.into_iter().map(|o| o.entity_id).collect();
        let mut cleanly_deleted_docs: HashSet<String> = HashSet::new();
        let mut resurrected_doc_ids: HashSet<String> = HashSet::new();
        let mut asset_writes: Vec<(String, Vec<u8>)> = Vec::new();
        let mut applies: Vec<IncomingApply> = Vec::new();
        let mut planned: Vec<PlannedCommit> = Vec::new();

        for d in &pending {
            let eid = d.change.entity_id.clone();
            let kind = d.change.kind;
            let prev = ctx.db.get_entity(&eid)?;

            let remote_equiv = remote_local_entity_equiv(&d.change, d.payload.as_ref(), d.raw_bytes.as_deref());
            let remote_hash = remote_equiv.as_ref().map(local_entity_content_hash).unwrap_or_default();
            let remote_base_blob = d
                .payload
                .as_ref()
                .map(|p| encode_base(&BaseSnapshot { payload: p.clone(), raw_bytes: d.raw_bytes.clone() }))
                .transpose()?;

            if kind == EntityKind::Asset {
                if d.change.deleted {
                    let mut asset_maps = vec![(eid.clone(), None)];
                    if let Some(rel) = resolve_asset_rel_for_id(&ctx.db, &eid)
                        && is_safe_asset_rel(&rel)
                    {
                        applies.push(IncomingApply::AssetDelete { rel_path: rel });
                    } else {
                        asset_maps.clear();
                    }
                    planned.push(PlannedCommit {
                        entity_id: eid,
                        upsert: Some(EntityStateRow {
                            entity_id: d.change.entity_id.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: String::new(),
                            base_blob: None,
                            deleted: true,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: true,
                        enqueue_outbox: Vec::new(),
                        asset_maps,
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                } else if let Some(EntityPayload::Asset { rel_path, blob_hash, size, .. }) = &d.payload {
                    let bytes = d.raw_bytes.as_deref().unwrap_or(&[]).to_vec();
                    asset_writes.push((rel_path.clone(), bytes));
                    planned.push(PlannedCommit {
                        entity_id: eid.clone(),
                        upsert: Some(EntityStateRow {
                            entity_id: eid.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: remote_hash,
                            base_blob: remote_base_blob,
                            deleted: false,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: true,
                        enqueue_outbox: Vec::new(),
                        asset_maps: vec![(eid, Some(rel_path.clone()))],
                        blob_mark: Some((blob_hash.clone(), rel_path.clone(), *size)),
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                }
                continue;
            }

            if kind == EntityKind::TrashItem
                && !d.change.deleted
                && let Some(tid) = parse_trash_entity_id(&eid)
            {
                let tid_owned = tid.to_string();
                if resurrected_doc_ids.contains(&tid_owned)
                    || (!cleanly_deleted_docs.contains(&tid_owned) && is_doc_locally_dirty(ctx, &tid_owned, &outbox_ids).await?)
                {
                    planned.push(PlannedCommit {
                        entity_id: eid.clone(),
                        upsert: Some(EntityStateRow {
                            entity_id: eid.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: String::new(),
                            base_blob: None,
                            deleted: true,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: false,
                        enqueue_outbox: vec![
                            (eid, EntityKind::TrashItem, true),
                            (tid_owned, EntityKind::Document, false),
                        ],
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                    continue;
                }
            }

            let local_now = ctx.adapter.read_entity(kind, &eid).await?;
            let local_hash = local_now.as_ref().map(local_entity_content_hash);

            let force_clean = adopt_clean_ids.contains(&eid);
            let is_dirty = if force_clean {
                false
            } else {
                match (&prev, &local_hash) {
                    (None, None) => false,
                    (None, Some(lh)) => !d.change.deleted && *lh != remote_hash,
                    (Some(st), None) => !st.deleted,
                    (Some(st), Some(lh)) => st.deleted || *lh != st.base_hash,
                }
            };

            if !is_dirty {
                if kind == EntityKind::Document && d.change.deleted {
                    cleanly_deleted_docs.insert(eid.clone());
                }
                let base_bytes = prev.as_ref().and_then(|s| s.decode_base().ok().flatten()).and_then(|b| b.raw_bytes);
                if let Some(app) = to_incoming_apply(
                    &d.change,
                    d.payload.as_ref(),
                    d.raw_bytes.as_deref(),
                    base_bytes,
                    device_name,
                    !scope.trash,
                    Some(local_hash),
                ) {
                    applies.push(app);
                }
                planned.push(PlannedCommit {
                    entity_id: eid.clone(),
                    upsert: Some(EntityStateRow {
                        entity_id: eid.clone(),
                        kind,
                        server_version: d.change.version,
                        base_hash: remote_hash,
                        base_blob: remote_base_blob,
                        deleted: d.change.deleted,
                        updated_at: d.change.updated_at,
                    }),
                    remove_outbox: true,
                    enqueue_outbox: Vec::new(),
                    asset_maps: Vec::new(),
                    blob_mark: None,
                    conflict_notice: None,
                    applied_folder_id: (kind == EntityKind::Folder && !d.change.deleted).then_some(eid),
                });
                continue;
            }

            // Concurrent modification detected against local disk! Perform 3-way merge.
            let base_snap = prev.as_ref().and_then(|s| s.decode_base().ok().flatten());
            match (kind, local_now, d.payload.as_ref()) {
                (
                    EntityKind::Document,
                    Some(LocalEntity::Document { file_name: l_name, parent_id: l_parent, ext: l_ext, meta: l_meta, bytes: l_bytes, .. }),
                    Some(EntityPayload::Document { name: r_name, parent_id: r_parent, ext: r_ext, meta: r_meta, .. }),
                ) => {
                    let r_bytes = d.raw_bytes.as_deref().unwrap_or(&[]);
                    if l_bytes == r_bytes && l_name == *r_name && l_parent == *r_parent && l_meta == *r_meta {
                        planned.push(PlannedCommit {
                            entity_id: eid,
                            upsert: Some(EntityStateRow {
                                entity_id: d.change.entity_id.clone(),
                                kind,
                                server_version: d.change.version,
                                base_hash: remote_hash,
                                base_blob: remote_base_blob,
                                deleted: false,
                                updated_at: d.change.updated_at,
                            }),
                            remove_outbox: true,
                            enqueue_outbox: Vec::new(),
                            asset_maps: Vec::new(),
                            blob_mark: None,
                            conflict_notice: None,
                            applied_folder_id: None,
                        });
                        continue;
                    }
                    if base_snap.is_none() && l_bytes == r_bytes && l_name == *r_name && l_parent == *r_parent {
                        if let Some(app) = to_incoming_apply_doc(
                            &eid,
                            r_name.clone(),
                            r_parent.clone(),
                            r_ext.clone(),
                            r_meta.clone(),
                            r_bytes.to_vec(),
                            device_name,
                            Some(local_hash),
                        ) {
                            applies.push(app);
                        }
                        planned.push(PlannedCommit {
                            entity_id: eid,
                            upsert: Some(EntityStateRow {
                                entity_id: d.change.entity_id.clone(),
                                kind,
                                server_version: d.change.version,
                                base_hash: remote_hash,
                                base_blob: remote_base_blob,
                                deleted: false,
                                updated_at: d.change.updated_at,
                            }),
                            remove_outbox: true,
                            enqueue_outbox: Vec::new(),
                            asset_maps: Vec::new(),
                            blob_mark: None,
                            conflict_notice: None,
                            applied_folder_id: None,
                        });
                        continue;
                    }

                    let merged = merge_doc_three_way(
                        base_snap.as_ref(),
                        (&l_name, l_parent.as_deref(), &l_ext, &l_meta, &l_bytes),
                        (r_name, r_parent.as_deref(), r_ext, r_meta, r_bytes),
                    );

                    match merged {
                        Some((m_name, m_parent, m_ext, m_meta, m_bytes)) => {
                            if let Some(app) = to_incoming_apply_doc(
                                &eid,
                                m_name.clone(),
                                m_parent.clone(),
                                m_ext.clone(),
                                m_meta.clone(),
                                m_bytes.clone(),
                                device_name,
                                Some(local_hash),
                            ) {
                                applies.push(app);
                            }
                            let matches_remote = m_bytes == r_bytes && m_name == *r_name && m_parent == *r_parent && m_meta == *r_meta;
                            planned.push(PlannedCommit {
                                entity_id: eid.clone(),
                                upsert: Some(EntityStateRow {
                                    entity_id: eid.clone(),
                                    kind,
                                    server_version: d.change.version,
                                    base_hash: remote_hash,
                                    base_blob: remote_base_blob,
                                    deleted: false,
                                    updated_at: d.change.updated_at,
                                }),
                                remove_outbox: matches_remote,
                                enqueue_outbox: if matches_remote {
                                    Vec::new()
                                } else {
                                    vec![(eid, EntityKind::Document, false)]
                                },
                                asset_maps: Vec::new(),
                                blob_mark: None,
                                conflict_notice: None,
                                applied_folder_id: None,
                            });
                        }
                        None => {
                            // Same-block conflict: keep local as primary, write remote to conflict copy.
                            let conflict_id = ctx.adapter.create_conflict_copy(&eid, r_ext, r_bytes, device_name).await?;
                            let title = l_name.strip_suffix(&format!(".{l_ext}")).unwrap_or(&l_name).to_string();
                            planned.push(PlannedCommit {
                                entity_id: eid.clone(),
                                upsert: Some(EntityStateRow {
                                    entity_id: eid.clone(),
                                    kind,
                                    server_version: d.change.version,
                                    base_hash: remote_hash,
                                    base_blob: remote_base_blob,
                                    deleted: false,
                                    updated_at: d.change.updated_at,
                                }),
                                remove_outbox: false,
                                enqueue_outbox: vec![
                                    (conflict_id.clone(), EntityKind::Document, false),
                                    (eid.clone(), EntityKind::Document, false),
                                ],
                                asset_maps: Vec::new(),
                                blob_mark: None,
                                conflict_notice: Some(SyncEvent::ConflictNotice {
                                    doc_id: eid,
                                    conflict_doc_id: conflict_id,
                                    title,
                                    device_name: device_name.to_string(),
                                }),
                                applied_folder_id: None,
                            });
                        }
                    }
                }
                (EntityKind::Document, Some(_), None) => {
                    // Local modified while remote deleted: local edit wins!
                    resurrected_doc_ids.insert(eid.clone());
                    planned.push(PlannedCommit {
                        entity_id: eid.clone(),
                        upsert: Some(EntityStateRow {
                            entity_id: eid.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: String::new(),
                            base_blob: None,
                            deleted: true,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: false,
                        enqueue_outbox: vec![(eid, EntityKind::Document, false)],
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                }
                (EntityKind::Document, None, Some(_)) => {
                    // Local deleted while remote edited: remote edit wins!
                    if let Some(app) = to_incoming_apply(
                        &d.change,
                        d.payload.as_ref(),
                        d.raw_bytes.as_deref(),
                        None,
                        device_name,
                        !scope.trash,
                        Some(None),
                    ) {
                        applies.push(app);
                    }
                    planned.push(PlannedCommit {
                        entity_id: eid,
                        upsert: Some(EntityStateRow {
                            entity_id: d.change.entity_id.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: remote_hash,
                            base_blob: remote_base_blob,
                            deleted: false,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: true,
                        enqueue_outbox: Vec::new(),
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                }
                (
                    EntityKind::Folder,
                    Some(LocalEntity::Folder { name: l_name, parent_id: l_parent, meta: l_meta, .. }),
                    Some(EntityPayload::Folder { name: r_name, parent_id: r_parent, meta: r_meta }),
                ) => {
                    let (b_name, b_parent, b_meta) = match base_snap.as_ref().map(|s| &s.payload) {
                        Some(EntityPayload::Folder { name, parent_id, meta }) => (name.clone(), parent_id.clone(), meta.clone()),
                        _ => (r_name.clone(), r_parent.clone(), r_meta.clone()),
                    };
                    let m_name = if l_name == b_name { r_name.clone() } else { l_name };
                    let m_parent = if l_parent == b_parent { r_parent.clone() } else { l_parent };
                    let m_meta = merge_meta(
                        &crate::adapter::store_meta_from_sync(&b_meta),
                        &crate::adapter::store_meta_from_sync(&l_meta),
                        &crate::adapter::store_meta_from_sync(r_meta),
                    );
                    let m_sync_meta = crate::adapter::sync_meta_from_store(&m_meta);
                    applies.push(IncomingApply::Folder {
                        id: eid.clone(),
                        name: m_name.clone(),
                        parent_id: m_parent.clone(),
                        meta: m_sync_meta.clone(),
                        deleted: false,
                        expected_local_hash: Some(local_hash),
                    });
                    let matches_remote = m_name == *r_name && m_parent == *r_parent && m_sync_meta == *r_meta;
                    planned.push(PlannedCommit {
                        entity_id: eid.clone(),
                        upsert: Some(EntityStateRow {
                            entity_id: eid.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: remote_hash,
                            base_blob: remote_base_blob,
                            deleted: false,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: matches_remote,
                        enqueue_outbox: if matches_remote {
                            Vec::new()
                        } else {
                            vec![(eid.clone(), EntityKind::Folder, false)]
                        },
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: Some(eid),
                    });
                }
                (EntityKind::Folder, Some(_), None) => {
                    // Local folder modified/renamed while remote deleted: keep local folder & resurrect.
                    planned.push(PlannedCommit {
                        entity_id: eid.clone(),
                        upsert: Some(EntityStateRow {
                            entity_id: eid.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: String::new(),
                            base_blob: None,
                            deleted: true,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: false,
                        enqueue_outbox: vec![(eid, EntityKind::Folder, false)],
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                }
                (
                    EntityKind::Config,
                    Some(LocalEntity::Config { key: l_key, bytes: l_bytes }),
                    Some(EntityPayload::Config { key: r_key, .. }),
                ) => {
                    let r_bytes = d.raw_bytes.as_deref().unwrap_or(&[]);
                    if (r_key == "preferences" || r_key == "shortcuts" || r_key == "library_settings")
                        && let (Ok(lv), Ok(rv)) = (
                            serde_json::from_slice::<serde_json::Value>(&l_bytes),
                            serde_json::from_slice::<serde_json::Value>(r_bytes),
                        )
                    {
                        let bv = base_snap
                            .as_ref()
                            .and_then(|s| s.raw_bytes.as_deref())
                            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
                            .unwrap_or_else(|| serde_json::json!({}));
                        let keep = if r_key == "preferences" {
                            LOCAL_ONLY_PREF_KEYS
                        } else if r_key == "library_settings" {
                            LOCAL_ONLY_SETTINGS_KEYS
                        } else {
                            &[]
                        };
                        let merged = merge_json_object(&bv, &lv, &rv, keep);
                        let m_bytes = serde_json::to_vec_pretty(&merged)?;
                        applies.push(IncomingApply::Config {
                            key: l_key,
                            bytes: Some(m_bytes.clone()),
                            expected_local_hash: Some(local_hash),
                        });
                        let matches_remote = m_bytes == r_bytes;
                        planned.push(PlannedCommit {
                            entity_id: eid.clone(),
                            upsert: Some(EntityStateRow {
                                entity_id: eid.clone(),
                                kind,
                                server_version: d.change.version,
                                base_hash: remote_hash,
                                base_blob: remote_base_blob,
                                deleted: false,
                                updated_at: d.change.updated_at,
                            }),
                            remove_outbox: matches_remote,
                            enqueue_outbox: if matches_remote {
                                Vec::new()
                            } else {
                                vec![(eid, EntityKind::Config, false)]
                            },
                            asset_maps: Vec::new(),
                            blob_mark: None,
                            conflict_notice: None,
                            applied_folder_id: None,
                        });
                        continue;
                    }
                    planned.push(PlannedCommit {
                        entity_id: eid.clone(),
                        upsert: Some(EntityStateRow {
                            entity_id: eid.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: remote_hash,
                            base_blob: remote_base_blob,
                            deleted: d.change.deleted,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: false,
                        enqueue_outbox: vec![(eid, EntityKind::Config, false)],
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: None,
                    });
                }
                _ => {
                    if let Some(app) = to_incoming_apply(
                        &d.change,
                        d.payload.as_ref(),
                        d.raw_bytes.as_deref(),
                        None,
                        device_name,
                        !scope.trash,
                        Some(local_hash),
                    ) {
                        applies.push(app);
                    }
                    planned.push(PlannedCommit {
                        entity_id: eid,
                        upsert: Some(EntityStateRow {
                            entity_id: d.change.entity_id.clone(),
                            kind,
                            server_version: d.change.version,
                            base_hash: remote_hash,
                            base_blob: remote_base_blob,
                            deleted: d.change.deleted,
                            updated_at: d.change.updated_at,
                        }),
                        remove_outbox: true,
                        enqueue_outbox: Vec::new(),
                        asset_maps: Vec::new(),
                        blob_mark: None,
                        conflict_notice: None,
                        applied_folder_id: (kind == EntityKind::Folder && !d.change.deleted).then(|| d.change.entity_id.clone()),
                    });
                }
            }
        }

        // Apply disk changes FIRST before touching SQLite entity_state / outbox / head_seq.
        for (rel_path, bytes) in asset_writes {
            ctx.adapter.write_asset(&rel_path, &bytes).await?;
        }
        let report = if !applies.is_empty() {
            ctx.adapter.apply_remote(applies).await?
        } else {
            ApplyReport::default()
        };

        let raced_ids: HashSet<String> = report.skipped_raced_ids.iter().map(|(_, id)| id.clone()).collect();
        let mut upserts = Vec::new();
        let mut remove_outbox = Vec::new();
        let mut enqueue_outbox = Vec::new();
        let mut asset_maps = Vec::new();
        let mut blob_marks = Vec::new();
        let mut notices = Vec::new();

        for p in planned {
            if raced_ids.contains(&p.entity_id) {
                continue;
            }
            just_applied_ids.insert(p.entity_id.clone());
            if let Some(fid) = p.applied_folder_id {
                all_applied_folders.insert(fid);
            }
            if let Some(u) = p.upsert {
                upserts.push(u);
            }
            if p.remove_outbox {
                remove_outbox.push(p.entity_id.clone());
            }
            enqueue_outbox.extend(p.enqueue_outbox);
            asset_maps.extend(p.asset_maps);
            if let Some(bm) = p.blob_mark {
                blob_marks.push(bm);
            }
            if let Some(ev) = p.conflict_notice {
                notices.push(ev);
            }
        }

        for c in &report.live_conflicts {
            enqueue_outbox.push((c.conflict_doc_id.clone(), EntityKind::Document, false));
            enqueue_outbox.push((c.doc_id.clone(), EntityKind::Document, false));
            notices.push(SyncEvent::ConflictNotice {
                doc_id: c.doc_id.clone(),
                conflict_doc_id: c.conflict_doc_id.clone(),
                title: c.title.clone(),
                device_name: c.device_name.clone(),
            });
        }
        for (doc_id, _, _) in &report.merged_back_docs {
            enqueue_outbox.push((doc_id.clone(), EntityKind::Document, false));
        }
        for rel in &report.requeue_assets {
            let wire_id = asset_wire_id(keys_opt, rel);
            asset_maps.push((wire_id.clone(), Some(rel.clone())));
            enqueue_outbox.push((wire_id, EntityKind::Asset, false));
        }

        let commit_head = if raced_ids.is_empty() { new_head_seq } else { None };
        ctx.db.commit_remote_batch(
            &upserts,
            &remove_outbox,
            &enqueue_outbox,
            &asset_maps,
            &blob_marks,
            commit_head,
        )?;

        for ev in notices {
            let _ = ctx.events.send(ev);
        }
        if report.tree_changed
            || report.trash_changed
            || !report.updated_docs.is_empty()
            || !report.renamed_docs.is_empty()
            || !report.meta_docs.is_empty()
            || !report.deleted_docs.is_empty()
            || !report.history_docs.is_empty()
            || !report.config_keys.is_empty()
        {
            let _ = ctx.events.send(SyncEvent::Applied(report));
        }

        if raced_ids.is_empty() {
            break;
        }
        if attempt == 2 {
            anyhow::bail!("并发本地修改冲突过多，已推迟远端批次应用");
        }
        pending.retain(|d| raced_ids.contains(&d.change.entity_id));
    }

    if !all_applied_folders.is_empty() {
        repair_orphaned_children(ctx, device_name, &all_applied_folders, &just_applied_ids).await?;
    }

    Ok(())
}

async fn repair_orphaned_children(
    ctx: &Arc<EngineCtx>,
    device_name: &str,
    applied_folders: &HashSet<String>,
    just_applied_ids: &HashSet<String>,
) -> Result<()> {
    let states = ctx.db.all_entities()?;
    let outbox_ids: HashSet<String> = ctx.db.list_outbox()?.into_iter().map(|o| o.entity_id).collect();
    let mut repairs = Vec::new();
    let mut repair_ids = Vec::new();

    for (eid, st) in states {
        if st.deleted
            || just_applied_ids.contains(&eid)
            || outbox_ids.contains(&eid)
            || !matches!(st.kind, EntityKind::Folder | EntityKind::Document)
        {
            continue;
        }
        let Some(base) = st.decode_base().ok().flatten() else {
            continue;
        };
        let desired_parent = match &base.payload {
            EntityPayload::Folder { parent_id, .. } | EntityPayload::Document { parent_id, .. } => parent_id.clone(),
            _ => None,
        };
        let Some(ref pid) = desired_parent else { continue };
        if !applied_folders.contains(pid) {
            continue;
        }
        let Some(local) = ctx.adapter.read_entity(st.kind, &eid).await? else {
            continue;
        };
        let local_hash = local_entity_content_hash(&local);
        match local {
            LocalEntity::Folder { name, parent_id, meta, .. } if parent_id.as_deref() != Some(pid.as_str()) && parent_id.is_none() => {
                repairs.push(IncomingApply::Folder {
                    id: eid.clone(),
                    name,
                    parent_id: Some(pid.clone()),
                    meta,
                    deleted: false,
                    expected_local_hash: Some(Some(local_hash)),
                });
                repair_ids.push(eid);
            }
            LocalEntity::Document { file_name, parent_id, ext, meta, bytes, .. } if parent_id.as_deref() != Some(pid.as_str()) && parent_id.is_none() => {
                repairs.push(IncomingApply::Document {
                    id: eid.clone(),
                    file_name,
                    parent_id: Some(pid.clone()),
                    ext,
                    meta,
                    bytes,
                    deleted: false,
                    preserve_in_local_trash: true,
                    expected_local_hash: Some(Some(local_hash)),
                    base_bytes: None,
                    device_name: device_name.to_string(),
                });
                repair_ids.push(eid);
            }
            _ => {}
        }
    }

    if !repairs.is_empty() {
        let report = ctx.adapter.apply_remote(repairs).await?;
        let raced: HashSet<String> = report.skipped_raced_ids.iter().map(|(_, id)| id.clone()).collect();
        if report.tree_changed && !repair_ids.iter().all(|id| raced.contains(id)) {
            let _ = ctx.events.send(SyncEvent::Applied(report));
        }
    }
    Ok(())
}

async fn reconcile_missing_from_snapshot(
    ctx: &Arc<EngineCtx>,
    device_name: &str,
    keys_opt: Option<&VaultKeys>,
    scope: SyncScope,
    remote_present: &HashSet<String>,
    only_kinds: Option<&HashSet<EntityKind>>,
) -> Result<()> {
    let states = ctx.db.all_entities()?;
    let mut missing: Vec<(String, EntityStateRow)> = states
        .into_iter()
        .filter(|(eid, st)| {
            !st.deleted
                && scope.allows(st.kind)
                && only_kinds.is_none_or(|ks| ks.contains(&st.kind))
                && !remote_present.contains(eid)
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    // Delete leaf entities before parent folders (same reverse order).
    missing.sort_by_key(|(eid, st)| {
        let k = match st.kind {
            EntityKind::Config => 0u8,
            EntityKind::HistorySnapshot => 1,
            EntityKind::TrashItem => 2,
            EntityKind::Document => 3,
            EntityKind::Asset => 4,
            EntityKind::Folder => 5,
        };
        (k, eid.clone())
    });

    let mut applies = Vec::new();
    let mut upserts = Vec::new();
    let mut remove_outbox = Vec::new();
    let mut enqueue_outbox = Vec::new();
    let mut asset_maps = Vec::new();
    let mut candidate_ids = Vec::new();
    let now = now_ms() / 1000;

    for (eid, st) in missing {
        if st.kind == EntityKind::Asset {
            let rel_opt = resolve_asset_rel_for_id(&ctx.db, &eid);
            let local_now = if let Some(ref rel) = rel_opt {
                ctx.adapter.read_entity(EntityKind::Asset, rel).await?
            } else {
                None
            };
            if let Some(le) = local_now {
                if local_entity_content_hash(&le) != st.base_hash {
                    // Modified offline while missing from server snapshot: keep & re-upload.
                    upserts.push(EntityStateRow {
                        entity_id: eid.clone(),
                        kind: EntityKind::Asset,
                        server_version: 0,
                        base_hash: String::new(),
                        base_blob: None,
                        deleted: true,
                        updated_at: now,
                    });
                    enqueue_outbox.push((eid.clone(), EntityKind::Asset, false));
                    if let Some(rel) = rel_opt {
                        asset_maps.push((eid, Some(rel)));
                    }
                    continue;
                }
                if let Some(rel) = rel_opt && is_safe_asset_rel(&rel) {
                    applies.push(IncomingApply::AssetDelete { rel_path: rel });
                }
            }
            candidate_ids.push(eid.clone());
            asset_maps.push((eid.clone(), None));
            upserts.push(EntityStateRow {
                entity_id: eid.clone(),
                kind: EntityKind::Asset,
                server_version: 0,
                base_hash: String::new(),
                base_blob: None,
                deleted: true,
                updated_at: now,
            });
            remove_outbox.push(eid);
            continue;
        }

        let local_now = ctx.adapter.read_entity(st.kind, &eid).await?;
        if let Some(ref le) = local_now {
            let lh = local_entity_content_hash(le);
            if lh != st.base_hash {
                // Edited offline while server compacted tombstone: keep local & enqueue re-upload!
                upserts.push(EntityStateRow {
                    entity_id: eid.clone(),
                    kind: st.kind,
                    server_version: 0,
                    base_hash: String::new(),
                    base_blob: None,
                    deleted: true,
                    updated_at: now,
                });
                enqueue_outbox.push((eid, st.kind, false));
                continue;
            }
            let expected_hash = Some(Some(lh));
            match st.kind {
                EntityKind::Folder => applies.push(IncomingApply::Folder {
                    id: eid.clone(),
                    name: String::new(),
                    parent_id: None,
                    meta: SyncMeta::default(),
                    deleted: true,
                    expected_local_hash: expected_hash,
                }),
                EntityKind::Document => applies.push(IncomingApply::Document {
                    id: eid.clone(),
                    file_name: String::new(),
                    parent_id: None,
                    ext: "xml".into(),
                    meta: SyncMeta::default(),
                    bytes: Vec::new(),
                    deleted: true,
                    preserve_in_local_trash: !scope.trash,
                    expected_local_hash: expected_hash,
                    base_bytes: None,
                    device_name: device_name.into(),
                }),
                EntityKind::TrashItem => {
                    if let Some(tid) = parse_trash_entity_id(&eid) {
                        applies.push(IncomingApply::TrashItem {
                            id: tid.to_string(),
                            orig_rel: String::new(),
                            folder: None,
                            ext: String::new(),
                            deleted: 0,
                            root: None,
                            meta: SyncMeta::default(),
                            bytes: None,
                            purged: true,
                        });
                    }
                }
                EntityKind::HistorySnapshot => {
                    if let Some((doc_id, name)) = parse_history_entity_id(&eid) {
                        applies.push(IncomingApply::HistorySnapshot {
                            doc_id: doc_id.to_string(),
                            name: name.to_string(),
                            trashed: false,
                            label: None,
                            bytes: None,
                        });
                    }
                }
                EntityKind::Config => {
                    if let Some(key) = parse_config_entity_id(&eid) {
                        applies.push(IncomingApply::Config {
                            key: key.to_string(),
                            bytes: None,
                            expected_local_hash: expected_hash,
                        });
                    }
                }
                EntityKind::Asset => {}
            }
        }
        candidate_ids.push(eid.clone());
        upserts.push(EntityStateRow {
            entity_id: eid.clone(),
            kind: st.kind,
            server_version: 0,
            base_hash: String::new(),
            base_blob: None,
            deleted: true,
            updated_at: now,
        });
        remove_outbox.push(eid);
    }

    let report = if !applies.is_empty() {
        ctx.adapter.apply_remote(applies).await?
    } else {
        ApplyReport::default()
    };
    let raced_ids: HashSet<String> = report.skipped_raced_ids.iter().map(|(_, id)| id.clone()).collect();
    upserts.retain(|u| !raced_ids.contains(&u.entity_id));
    remove_outbox.retain(|id| !raced_ids.contains(id));

    for rel in &report.requeue_assets {
        let wire_id = asset_wire_id(keys_opt, rel);
        asset_maps.push((wire_id.clone(), Some(rel.clone())));
        enqueue_outbox.push((wire_id, EntityKind::Asset, false));
    }

    let _ = candidate_ids;
    ctx.db.commit_remote_batch(&upserts, &remove_outbox, &enqueue_outbox, &asset_maps, &[], None)?;

    if report.tree_changed
        || report.trash_changed
        || !report.deleted_docs.is_empty()
        || !report.history_docs.is_empty()
        || !report.config_keys.is_empty()
    {
        let _ = ctx.events.send(SyncEvent::Applied(report));
    }
    Ok(())
}

fn merge_doc_three_way(
    base: Option<&BaseSnapshot>,
    local: (&str, Option<&str>, &str, &SyncMeta, &[u8]),
    remote: (&str, Option<&str>, &str, &SyncMeta, &[u8]),
) -> Option<(String, Option<String>, String, SyncMeta, Vec<u8>)> {
    let base_snap = base?;
    let EntityPayload::Document { name: b_name, parent_id: b_parent, ext: b_ext, meta: b_meta, .. } = &base_snap.payload else {
        return None;
    };
    let b_bytes = base_snap.raw_bytes.as_deref()?;
    if local.2 != remote.2 || local.2 != b_ext.as_str() {
        return None;
    }
    let ext = local.2;
    let m_name = if local.0 == b_name.as_str() { remote.0.to_string() } else { local.0.to_string() };
    let m_parent = if local.1 == b_parent.as_deref() { remote.1.map(str::to_string) } else { local.1.map(str::to_string) };
    let m_meta = crate::adapter::sync_meta_from_store(&merge_meta(
        &crate::adapter::store_meta_from_sync(b_meta),
        &crate::adapter::store_meta_from_sync(local.3),
        &crate::adapter::store_meta_from_sync(remote.3),
    ));

    let m_bytes = match ext {
        "excalidraw" => {
            let b_str = std::str::from_utf8(b_bytes).ok()?;
            let l_str = std::str::from_utf8(local.4).ok()?;
            let r_str = std::str::from_utf8(remote.4).ok()?;
            let bb = Board::from_file(b_str).ok()?;
            let lb = Board::from_file(l_str).ok()?;
            let rb = Board::from_file(r_str).ok()?;
            let res = merge_boards(&bb, &lb, &rb);
            if res.has_conflicts() {
                return None;
            }
            res.value.file_json().into_bytes()
        }
        "md" => {
            let b_str = std::str::from_utf8(b_bytes).ok()?;
            let l_str = std::str::from_utf8(local.4).ok()?;
            let r_str = std::str::from_utf8(remote.4).ok()?;
            let title = m_name.strip_suffix(".md").unwrap_or(&m_name);
            let res = merge_markdown(b_str, l_str, r_str, title);
            if res.has_conflicts() {
                return None;
            }
            res.value.into_bytes()
        }
        _ => {
            let b_str = std::str::from_utf8(b_bytes).ok()?;
            let l_str = std::str::from_utf8(local.4).ok()?;
            let r_str = std::str::from_utf8(remote.4).ok()?;
            let bd = docxml::parse(b_str).ok()?;
            let ld = docxml::parse(l_str).ok()?;
            let rd = docxml::parse(r_str).ok()?;
            let res = merge_documents(&bd, &ld, &rd);
            if res.has_conflicts() {
                return None;
            }
            docxml::serialize(&res.value).into_bytes()
        }
    };

    Some((m_name, m_parent, ext.to_string(), m_meta, m_bytes))
}

#[allow(clippy::too_many_arguments)]
fn to_incoming_apply(
    change: &RemoteChange,
    payload: Option<&EntityPayload>,
    raw_bytes: Option<&[u8]>,
    base_bytes: Option<Vec<u8>>,
    device_name: &str,
    preserve_in_local_trash: bool,
    expected_local_hash: Option<Option<String>>,
) -> Option<IncomingApply> {
    if change.deleted || payload.is_none() {
        return match change.kind {
            EntityKind::Folder => Some(IncomingApply::Folder {
                id: change.entity_id.clone(),
                name: String::new(),
                parent_id: None,
                meta: SyncMeta::default(),
                deleted: true,
                expected_local_hash,
            }),
            EntityKind::Document => Some(IncomingApply::Document {
                id: change.entity_id.clone(),
                file_name: String::new(),
                parent_id: None,
                ext: "xml".into(),
                meta: SyncMeta::default(),
                bytes: Vec::new(),
                deleted: true,
                preserve_in_local_trash,
                expected_local_hash,
                base_bytes,
                device_name: device_name.into(),
            }),
            EntityKind::TrashItem => Some(IncomingApply::TrashItem {
                id: parse_trash_entity_id(&change.entity_id)?.to_string(),
                orig_rel: String::new(),
                folder: None,
                ext: String::new(),
                deleted: 0,
                root: None,
                meta: SyncMeta::default(),
                bytes: None,
                purged: true,
            }),
            EntityKind::HistorySnapshot => {
                let (doc_id, name) = parse_history_entity_id(&change.entity_id)?;
                Some(IncomingApply::HistorySnapshot {
                    doc_id: doc_id.to_string(),
                    name: name.to_string(),
                    trashed: false,
                    label: None,
                    bytes: None,
                })
            }
            EntityKind::Config => Some(IncomingApply::Config {
                key: parse_config_entity_id(&change.entity_id)?.to_string(),
                bytes: None,
                expected_local_hash,
            }),
            EntityKind::Asset => None,
        };
    }

    match payload.unwrap() {
        EntityPayload::Folder { name, parent_id, meta } => Some(IncomingApply::Folder {
            id: change.entity_id.clone(),
            name: name.clone(),
            parent_id: parent_id.clone(),
            meta: meta.clone(),
            deleted: false,
            expected_local_hash,
        }),
        EntityPayload::Document { name, parent_id, ext, meta, .. } => Some(IncomingApply::Document {
            id: change.entity_id.clone(),
            file_name: name.clone(),
            parent_id: parent_id.clone(),
            ext: ext.clone(),
            meta: meta.clone(),
            bytes: raw_bytes.unwrap_or_default().to_vec(),
            deleted: false,
            preserve_in_local_trash,
            expected_local_hash,
            base_bytes,
            device_name: device_name.into(),
        }),
        EntityPayload::TrashItem { orig_rel, folder, ext, deleted, root, meta, .. } => Some(IncomingApply::TrashItem {
            id: parse_trash_entity_id(&change.entity_id)?.to_string(),
            orig_rel: orig_rel.clone(),
            folder: folder.clone(),
            ext: ext.clone(),
            deleted: *deleted,
            root: root.clone(),
            meta: meta.clone(),
            bytes: raw_bytes.map(|b| b.to_vec()),
            purged: false,
        }),
        EntityPayload::HistorySnapshot { doc_id, name, trashed, label, .. } => Some(IncomingApply::HistorySnapshot {
            doc_id: doc_id.clone(),
            name: name.clone(),
            trashed: *trashed,
            label: label.clone(),
            bytes: raw_bytes.map(|b| b.to_vec()),
        }),
        EntityPayload::Config { key, .. } => Some(IncomingApply::Config {
            key: key.clone(),
            bytes: raw_bytes.map(|b| b.to_vec()),
            expected_local_hash,
        }),
        EntityPayload::Asset { .. } => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn to_incoming_apply_doc(
    id: &str,
    file_name: String,
    parent_id: Option<String>,
    ext: String,
    meta: SyncMeta,
    bytes: Vec<u8>,
    device_name: &str,
    expected_local_hash: Option<Option<String>>,
) -> Option<IncomingApply> {
    Some(IncomingApply::Document {
        id: id.to_string(),
        file_name,
        parent_id,
        ext,
        meta,
        bytes,
        deleted: false,
        preserve_in_local_trash: true,
        expected_local_hash,
        base_bytes: None,
        device_name: device_name.into(),
    })
}

fn outbox_sort_order(kind: EntityKind, deleted: bool) -> u8 {
    match (kind, deleted) {
        (EntityKind::Folder, false) => 0,
        (EntityKind::Asset, false) => 1,
        (EntityKind::Document, false) => 2,
        (EntityKind::TrashItem, false) => 3,
        (EntityKind::HistorySnapshot, false) => 4,
        (EntityKind::Config, false) => 5,
        (EntityKind::Config, true) => 6,
        (EntityKind::HistorySnapshot, true) => 7,
        (EntityKind::TrashItem, true) => 8,
        (EntityKind::Document, true) => 9,
        (EntityKind::Asset, true) => 10,
        (EntityKind::Folder, true) => 11,
    }
}

async fn flush_outbox_inner(ctx: &Arc<EngineCtx>, vault_id: &str, device_id: &str, device_name: &str) -> Result<()> {
    let Some(creds) = ctx.api.credentials().await else { return Ok(()) };
    let keys = ctx.keys.lock().await.clone();
    let keys_opt = effective_keys(&creds, keys.as_ref())?;
    let mut stalled = 0u32;

    loop {
        if ctx.db.paused() {
            break;
        }
        let scope = ctx.db.scope();
        let mut items = ctx.db.list_outbox_for_scope(scope)?;
        if items.is_empty() {
            break;
        }
        items.sort_by_key(|o| outbox_sort_order(o.kind, o.deleted));
        let batch = items.into_iter().take(50).collect::<Vec<_>>();

        struct Prepared {
            entity_id: String,
            kind: EntityKind,
            deleted: bool,
            op: PushOp,
            local_hash: String,
            base_blob: Option<Vec<u8>>,
            blobs: HashMap<String, Vec<u8>>,
            asset_rel: Option<String>,
            asset_size: u64,
        }

        let mut prepared = Vec::new();
        let mut made_local_progress = false;

        for item in batch {
            if ctx.db.paused() || !ctx.db.scope().allows(item.kind) {
                break;
            }
            let prev = ctx.db.get_entity(&item.entity_id)?;
            let expected_version = prev.as_ref().map_or(0, |s| s.server_version);
            let next_version = expected_version + 1;

            let local = if item.deleted {
                None
            } else if item.kind == EntityKind::Asset {
                match resolve_asset_rel_for_id(&ctx.db, &item.entity_id) {
                    Some(rel) => ctx.adapter.read_entity(EntityKind::Asset, &rel).await?,
                    None => None,
                }
            } else {
                ctx.adapter.read_entity(item.kind, &item.entity_id).await?
            };

            let Some(local) = local else {
                // Entity does not exist locally: push deletion if server knows it; else drop from outbox.
                if prev.as_ref().is_none_or(|s| s.deleted) {
                    let _ = ctx.db.remove_outbox(&item.entity_id);
                    made_local_progress = true;
                    continue;
                }
                let envelope = match keys_opt {
                    Some(k) => Some(k.encrypt_tombstone(vault_id, &item.entity_id, item.kind, next_version)?),
                    None => None,
                };
                prepared.push(Prepared {
                    entity_id: item.entity_id.clone(),
                    kind: item.kind,
                    deleted: true,
                    op: PushOp {
                        entity_id: item.entity_id,
                        kind: item.kind,
                        expected_version,
                        deleted: true,
                        envelope,
                        blob_hashes: Vec::new(),
                    },
                    local_hash: String::new(),
                    base_blob: None,
                    blobs: HashMap::new(),
                    asset_rel: None,
                    asset_size: 0,
                });
                continue;
            };

            let local_hash = local_entity_content_hash(&local);
            if prev.as_ref().is_some_and(|st| !st.deleted && st.base_hash == local_hash) {
                let _ = ctx.db.remove_outbox(&item.entity_id);
                made_local_progress = true;
                continue;
            }

            // Ensure any assets referenced by this document are queued/pushed as well.
            if let LocalEntity::Document { asset_refs, .. } = &local {
                let mut asset_maps = Vec::new();
                let mut to_enqueue = Vec::new();
                for rel in asset_refs {
                    let wire_id = asset_wire_id(keys_opt, rel);
                    asset_maps.push((wire_id.clone(), Some(rel.clone())));
                    if ctx.db.get_entity(&wire_id)?.is_none_or(|st| st.deleted) {
                        to_enqueue.push((wire_id, EntityKind::Asset, false));
                    }
                }
                if !asset_maps.is_empty() || !to_enqueue.is_empty() {
                    let _ = ctx.db.commit_remote_batch(&[], &[], &to_enqueue, &asset_maps, &[], None);
                }
            }

            let mut blobs = HashMap::new();
            let mut asset_rel = None;
            let mut asset_size = 0u64;
            let (payload, raw_bytes_for_base) = match &local {
                LocalEntity::Folder { name, parent_id, meta, .. } => (
                    EntityPayload::Folder { name: name.clone(), parent_id: parent_id.clone(), meta: meta.clone() },
                    None,
                ),
                LocalEntity::Document { file_name, parent_id, ext, meta, bytes, .. } => {
                    let body = make_content_body(vault_id, keys_opt, bytes, &mut blobs)?;
                    (
                        EntityPayload::Document {
                            name: file_name.clone(),
                            parent_id: parent_id.clone(),
                            ext: ext.clone(),
                            meta: meta.clone(),
                            content: body,
                        },
                        Some(bytes.clone()),
                    )
                }
                LocalEntity::Asset { rel_path, bytes, mime } => {
                    let (hash, wire) = encode_blob_bytes(vault_id, keys_opt, bytes)?;
                    asset_rel = Some(rel_path.clone());
                    asset_size = wire.len() as u64;
                    blobs.insert(hash.clone(), wire);
                    (
                        EntityPayload::Asset {
                            rel_path: rel_path.clone(),
                            blob_hash: hash,
                            raw_sha256: sha256_hex(bytes),
                            size: bytes.len() as u64,
                            mime: mime.clone(),
                        },
                        None,
                    )
                }
                LocalEntity::TrashItem { orig_rel, folder, ext, deleted, root, meta, bytes, .. } => {
                    let content = bytes
                        .as_ref()
                        .map(|b| make_content_body(vault_id, keys_opt, b, &mut blobs))
                        .transpose()?;
                    (
                        EntityPayload::TrashItem {
                            orig_rel: orig_rel.clone(),
                            folder: folder.clone(),
                            ext: ext.clone(),
                            deleted: *deleted,
                            root: root.clone(),
                            meta: meta.clone(),
                            content,
                        },
                        bytes.clone(),
                    )
                }
                LocalEntity::HistorySnapshot { doc_id, name, trashed, label, bytes } => {
                    let body = make_content_body(vault_id, keys_opt, bytes, &mut blobs)?;
                    (
                        EntityPayload::HistorySnapshot {
                            doc_id: doc_id.clone(),
                            name: name.clone(),
                            trashed: *trashed,
                            label: label.clone(),
                            content: body,
                        },
                        Some(bytes.clone()),
                    )
                }
                LocalEntity::Config { key, bytes } => {
                    let body = make_content_body(vault_id, keys_opt, bytes, &mut blobs)?;
                    (EntityPayload::Config { key: key.clone(), content: body }, Some(bytes.clone()))
                }
            };

            let encoded = payload.encode_bytes()?;
            let blob_hashes = payload.referenced_blobs();
            let envelope = match keys_opt {
                Some(k) => k.encrypt_entity(vault_id, &item.entity_id, item.kind, next_version, &encoded)?,
                None => Envelope::plain(&encoded),
            };
            let base_blob = Some(encode_base(&BaseSnapshot { payload, raw_bytes: raw_bytes_for_base })?);

            prepared.push(Prepared {
                entity_id: item.entity_id.clone(),
                kind: item.kind,
                deleted: false,
                op: PushOp {
                    entity_id: item.entity_id,
                    kind: item.kind,
                    expected_version,
                    deleted: false,
                    envelope: Some(envelope),
                    blob_hashes,
                },
                local_hash,
                base_blob,
                blobs,
                asset_rel,
                asset_size,
            });
        }

        if prepared.is_empty() {
            if made_local_progress {
                stalled = 0;
                continue;
            }
            break;
        }
        if ctx.db.paused() {
            break;
        }

        // Pre-upload any referenced blobs not yet recorded in synced_blobs.
        let mut all_blobs: HashMap<String, (Vec<u8>, String)> = HashMap::new();
        for p in &prepared {
            for (h, bytes) in &p.blobs {
                if !ctx.db.is_blob_synced(h) {
                    all_blobs.insert(h.clone(), (bytes.clone(), p.asset_rel.clone().unwrap_or_default()));
                }
            }
        }
        if !all_blobs.is_empty() {
            let hashes: Vec<String> = all_blobs.keys().cloned().collect();
            let existing: HashSet<String> = ctx.api.blobs_exists(vault_id, hashes).await?.into_iter().collect();
            for (h, (bytes, rel)) in &all_blobs {
                if !existing.contains(h) {
                    ctx.api.put_blob(vault_id, h, bytes.clone()).await?;
                }
                let _ = ctx.db.mark_blob_synced(h, rel, bytes.len() as u64);
            }
        }

        let ops: Vec<PushOp> = prepared.iter().map(|p| p.op.clone()).collect();
        let total_json_len: usize = ops.iter().map(|o| serde_json::to_vec(o).map_or(0, |v| v.len())).sum();
        let (head_seq, results) = if total_json_len <= 200 * 1024
            && let Some(ws_res) = ctx.ws.try_push(ops.clone()).await
        {
            match ws_res {
                Ok(r) => r,
                Err(_) => {
                    let resp = ctx.api.push(vault_id, &PushReq { device_id: device_id.into(), ops }).await?;
                    (resp.head_seq, resp.results)
                }
            }
        } else {
            let resp = ctx.api.push(vault_id, &PushReq { device_id: device_id.into(), ops }).await?;
            (resp.head_seq, resp.results)
        };

        let mut prep_map: HashMap<String, Prepared> = prepared.into_iter().map(|p| (p.entity_id.clone(), p)).collect();
        let mut conflicts = Vec::new();
        let mut all_accepted = true;
        let mut made_progress = made_local_progress;

        for res in results {
            match res {
                PushItemResult::Accepted { entity_id, version, seq } => {
                    if let Some(p) = prep_map.remove(&entity_id) {
                        let asset_maps = if p.deleted && p.kind == EntityKind::Asset {
                            vec![(entity_id.clone(), None)]
                        } else if let Some(rel) = &p.asset_rel {
                            vec![(entity_id.clone(), Some(rel.clone()))]
                        } else {
                            Vec::new()
                        };
                        let blob_marks: Vec<(String, String, u64)> = if let Some(rel) = &p.asset_rel {
                            p.blobs.keys().map(|h| (h.clone(), rel.clone(), p.asset_size)).collect()
                        } else {
                            Vec::new()
                        };
                        let row = EntityStateRow {
                            entity_id: entity_id.clone(),
                            kind: p.kind,
                            server_version: version,
                            base_hash: p.local_hash,
                            base_blob: p.base_blob,
                            deleted: p.deleted,
                            updated_at: now_ms() / 1000,
                        };
                        let new_seq = (seq == ctx.db.head_seq() + 1).then_some(seq);
                        ctx.db.commit_remote_batch(
                            &[row],
                            &[entity_id],
                            &[],
                            &asset_maps,
                            &blob_marks,
                            new_seq,
                        )?;
                        made_progress = true;
                    }
                }
                PushItemResult::MissingBlobs { entity_id, missing } => {
                    all_accepted = false;
                    for h in &missing {
                        let _ = ctx.db.mark_blob_unsynced(h);
                    }
                    if let Some(p) = prep_map.get(&entity_id) {
                        for h in missing {
                            if let Some(bytes) = p.blobs.get(&h) {
                                ctx.api.put_blob(vault_id, &h, bytes.clone()).await?;
                                let _ = ctx.db.mark_blob_synced(&h, p.asset_rel.as_deref().unwrap_or(""), bytes.len() as u64);
                                made_progress = true;
                            }
                        }
                    }
                    let _ = ctx.db.bump_outbox_retry(&entity_id);
                }
                PushItemResult::StaleBase { entity_id, .. } => {
                    all_accepted = false;
                    if let Some(p) = prep_map.get(&entity_id) {
                        ctx.db.upsert_entity(&EntityStateRow {
                            entity_id,
                            kind: p.kind,
                            server_version: 0,
                            base_hash: String::new(),
                            base_blob: None,
                            deleted: true,
                            updated_at: now_ms() / 1000,
                        })?;
                        made_progress = true;
                    }
                }
                PushItemResult::Conflict { current, .. } => {
                    all_accepted = false;
                    conflicts.push(current);
                    made_progress = true;
                }
            }
        }

        if !conflicts.is_empty() {
            apply_remote_batch_with_head(ctx, vault_id, device_name, conflicts, None).await?;
            let _ = pull_remote_changes(ctx, vault_id, device_name).await;
            stalled = 0;
            continue;
        }
        if all_accepted && head_seq > ctx.db.head_seq() && scoped_pending_count(ctx) == 0 {
            let _ = pull_remote_changes(ctx, vault_id, device_name).await;
        }
        if made_progress {
            stalled = 0;
        } else {
            stalled += 1;
            if stalled >= 3 {
                break;
            }
        }
    }
    Ok(())
}

fn make_content_body(vault_id: &str, keys: Option<&VaultKeys>, raw: &[u8], blobs: &mut HashMap<String, Vec<u8>>) -> Result<ContentBody> {
    if raw.len() <= INLINE_THRESHOLD {
        return Ok(ContentBody::inline_from_bytes(raw));
    }
    let (hash, wire) = encode_blob_bytes(vault_id, keys, raw)?;
    blobs.insert(hash.clone(), wire);
    Ok(ContentBody::Blob {
        hash,
        raw_sha256: sha256_hex(raw),
        raw_size: raw.len() as u64,
    })
}

fn encode_blob_bytes(vault_id: &str, keys: Option<&VaultKeys>, raw: &[u8]) -> Result<(String, Vec<u8>)> {
    match keys {
        Some(k) => k.encrypt_blob(vault_id, raw),
        None => Ok((sha256_hex(raw), raw.to_vec())),
    }
}

pub fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("Quiet.md ({})", current_platform()))
}

pub fn sync_dir_for_root(sync_dir: PathBuf) -> PathBuf {
    sync_dir
}
