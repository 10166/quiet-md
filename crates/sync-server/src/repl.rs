use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::blob::sha256_hex;
use crate::db::{self, NodeConfig, NodeRole};
use crate::db_admin;
use crate::db_repl::{self, BinlogRecord, UpstreamRow};
use crate::{gc, AppState};

// ---- Wire Protocol Frames ----

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplicaFrame {
    Hello {
        applied_lsn: u64,
        epoch: String,
        name: String,
        portal_url: String,
        storage_bytes: u64,
        blob_pending: u64,
        server_version: String,
    },
    Ack {
        applied_lsn: u64,
        blob_pending: u64,
        storage_bytes: u64,
        name: String,
        portal_url: String,
        server_version: String,
    },
    Ping,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrimaryFrame {
    Welcome {
        primary_id: String,
        primary_name: String,
        primary_portal_url: String,
        epoch: String,
        head_lsn: u64,
        min_retained_lsn: u64,
        resync_required: bool,
    },
    BinlogBatch {
        head_lsn: u64,
        events: Vec<BinlogRecord>,
        has_more: bool,
    },
    PrimaryInfo {
        primary_id: String,
        primary_name: String,
        primary_portal_url: String,
        head_lsn: u64,
    },
    Evicted {
        reason: String,
    },
    Pong,
    Error {
        code: String,
        message: String,
    },
}

// ---- Primary-side Live Replica Hub ----

#[derive(Clone, Debug)]
pub enum PrimaryCmd {
    PrimaryInfo {
        primary_id: String,
        primary_name: String,
        primary_portal_url: String,
        head_lsn: u64,
    },
}

#[derive(Clone, Debug)]
pub enum EvictSignal {
    Evict(String),
    Close,
}

struct ConnectedReplica {
    conn_id: String,
    info_tx: mpsc::Sender<PrimaryCmd>,
    evict_tx: watch::Sender<Option<EvictSignal>>,
}

#[derive(Clone, Default)]
pub struct ReplHub {
    connected: Arc<RwLock<HashMap<String, ConnectedReplica>>>,
}

impl ReplHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_conn(
        &self,
        replica_id: &str,
        conn_id: &str,
        info_tx: mpsc::Sender<PrimaryCmd>,
    ) -> watch::Receiver<Option<EvictSignal>> {
        let (evict_tx, evict_rx) = watch::channel(None);
        let mut guard = self.connected.write().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = guard.insert(
            replica_id.to_string(),
            ConnectedReplica { conn_id: conn_id.to_string(), info_tx, evict_tx },
        ) {
            let _ = old.evict_tx.send(Some(EvictSignal::Close));
        }
        evict_rx
    }

    pub fn unregister_conn(&self, replica_id: &str, conn_id: &str) {
        let mut guard = self.connected.write().unwrap_or_else(|e| e.into_inner());
        if guard.get(replica_id).is_some_and(|c| c.conn_id == conn_id) {
            guard.remove(replica_id);
        }
    }

    pub fn evict_replica(&self, replica_id: &str, reason: &str) {
        let mut guard = self.connected.write().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = guard.remove(replica_id) {
            let _ = conn.evict_tx.send(Some(EvictSignal::Evict(reason.to_string())));
        }
    }

    pub fn broadcast_primary_info(&self, primary_id: &str, primary_name: &str, primary_portal_url: &str, head_lsn: u64) {
        let guard = self.connected.read().unwrap_or_else(|e| e.into_inner());
        let cmd = PrimaryCmd::PrimaryInfo {
            primary_id: primary_id.to_string(),
            primary_name: primary_name.to_string(),
            primary_portal_url: primary_portal_url.to_string(),
            head_lsn,
        };
        for conn in guard.values() {
            let _ = conn.info_tx.try_send(cmd.clone());
        }
    }

    pub fn online_replica_ids(&self) -> HashSet<String> {
        self.connected.read().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect()
    }

    pub fn disconnect_all(&self) {
        let mut guard = self.connected.write().unwrap_or_else(|e| e.into_inner());
        for conn in guard.values() {
            let _ = conn.evict_tx.send(Some(EvictSignal::Close));
        }
        guard.clear();
    }
}

// ---- Backup-side Outbound Replication Supervisor ----

#[derive(Clone)]
pub struct ReplSupervisor {
    reload_tx: Arc<watch::Sender<u64>>,
    blob_notify: Arc<Notify>,
    ack_notify: Arc<Notify>,
    shutdown_tx: Arc<watch::Sender<bool>>,
}

impl Default for ReplSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplSupervisor {
    pub fn new() -> Self {
        let (reload_tx, _) = watch::channel(0u64);
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            reload_tx: Arc::new(reload_tx),
            blob_notify: Arc::new(Notify::new()),
            ack_notify: Arc::new(Notify::new()),
            shutdown_tx: Arc::new(shutdown_tx),
        }
    }

    /// Wake the backup worker to reload configuration (e.g. after configure-backup / promote / resync).
    pub fn trigger_reload(&self) {
        self.reload_tx.send_modify(|v| *v = v.wrapping_add(1));
        self.blob_notify.notify_one();
    }

    /// Wake the active backup WS stream to send an immediate Ack (e.g. after portal_url or name change).
    pub fn trigger_ack(&self) {
        self.ack_notify.notify_one();
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        self.trigger_reload();
    }

    pub fn spawn(&self, state: AppState) {
        let reload_rx = self.reload_tx.subscribe();
        let shutdown_rx = self.shutdown_tx.subscribe();
        let blob_notify = self.blob_notify.clone();
        let ack_notify = self.ack_notify.clone();
        let st_worker = state.clone();
        tokio::spawn(async move {
            repl_worker_loop(st_worker, reload_rx, shutdown_rx, blob_notify, ack_notify).await;
        });

        let reload_rx2 = self.reload_tx.subscribe();
        let shutdown_rx2 = self.shutdown_tx.subscribe();
        let blob_notify2 = self.blob_notify.clone();
        let ack_notify2 = self.ack_notify.clone();
        tokio::spawn(async move {
            blob_sync_loop(state, reload_rx2, shutdown_rx2, blob_notify2, ack_notify2).await;
        });
    }
}

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn cleanup_stale_temp_files(data_dir: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(data_dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if let Some(name) = p.file_name().and_then(|s| s.to_str())
            && name.starts_with("repl_snapshot_")
            && name.ends_with(".ndjson.tmp")
        {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn build_streaming_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn truncate_err(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= 512 {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn is_eviction_http_error(status_code: u16, body: &str) -> bool {
    (status_code == 403 && body.contains("replica_evicted"))
        || (status_code == 401 && (body.contains("unknown_replica") || body.contains("invalid_replica_token")))
}

async fn read_capped_error_text(mut resp: reqwest::Response) -> String {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let remaining = 2048usize.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let take = remaining.min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
        if buf.len() >= 2048 {
            break;
        }
    }
    truncate_err(&String::from_utf8_lossy(&buf))
}

pub fn local_db_file_sizes(state: &AppState) -> (u64, u64, u64) {
    let db_path = state.db.path();
    let db_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal_path = format!("{}-wal", db_path.display());
    let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let shm_path = format!("{}-shm", db_path.display());
    let shm_bytes = std::fs::metadata(&shm_path).map(|m| m.len()).unwrap_or(0);
    (db_bytes, wal_bytes, shm_bytes)
}

pub async fn compute_node_storage_bytes(state: &AppState) -> u64 {
    let (db_bytes, wal_bytes, shm_bytes) = local_db_file_sizes(state);
    let blob_bytes = state
        .db
        .run(|conn| db_admin::get_overview_stats(conn).map(|s| s.unique_blob_bytes))
        .await
        .unwrap_or(0);
    db_bytes.saturating_add(wal_bytes).saturating_add(shm_bytes).saturating_add(blob_bytes)
}

fn to_ws_url(http_base: &str, path: &str) -> anyhow::Result<String> {
    let base = http_base.trim().trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("unsupported primary_url scheme");
    };
    Ok(format!("{ws_base}{path}"))
}

enum BootstrapErr {
    Evicted(String),
    Cancelled,
    Transient(String),
}

async fn repl_worker_loop(
    state: AppState,
    mut reload_rx: watch::Receiver<u64>,
    mut shutdown_rx: watch::Receiver<bool>,
    blob_notify: Arc<Notify>,
    ack_notify: Arc<Notify>,
) {
    let client = build_streaming_http_client();
    let mut backoff_secs = 1u64;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        // Mark current reload counter as observed before reading DB config.
        let _ = reload_rx.borrow_and_update();

        let Ok((node, upstream_opt)) = state
            .db
            .run(|conn| Ok((db::get_node_config(conn)?, db_repl::get_upstream(conn)?)))
            .await
        else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };

        let Some(upstream) = upstream_opt.filter(|_| node.role == NodeRole::Backup) else {
            tokio::select! {
                _ = reload_rx.changed() => {}
                _ = shutdown_rx.changed() => {}
            }
            backoff_secs = 1;
            continue;
        };

        if upstream.state == "evicted" {
            tokio::select! {
                _ = reload_rx.changed() => {}
                _ = shutdown_rx.changed() => {}
            }
            backoff_secs = 1;
            continue;
        }

        // Step 1: Full WAL-consistent snapshot bootstrap when not yet bootstrapped (or after resync_required).
        if !upstream.bootstrapped {
            let _ = state
                .db
                .run(|conn| db_repl::update_upstream_state(conn, "bootstrapping", Some(""), None, None, None, false))
                .await;
            let boot_res = tokio::select! {
                res = bootstrap_snapshot(&state, &client, &node, &upstream) => res,
                _ = reload_rx.changed() => Err(BootstrapErr::Cancelled),
                _ = shutdown_rx.changed() => Err(BootstrapErr::Cancelled),
            };
            match boot_res {
                Ok((snapshot_lsn, pending)) => {
                    tracing::info!(snapshot_lsn, pending_blobs = pending, "backup snapshot bootstrap completed");
                    backoff_secs = 1;
                    blob_notify.notify_one();
                    let st_gc = state.clone();
                    tokio::spawn(async move {
                        let _ = gc::run_backup_orphan_cleanup(&st_gc).await;
                    });
                }
                Err(BootstrapErr::Cancelled) => {
                    backoff_secs = 1;
                    continue;
                }
                Err(BootstrapErr::Evicted(reason)) => {
                    tracing::warn!("backup evicted during snapshot bootstrap: {reason}");
                    let r = truncate_err(&reason);
                    let _ = state
                        .db
                        .run(move |conn| db_repl::update_upstream_state(conn, "evicted", Some(&r), None, None, None, false))
                        .await;
                    continue;
                }
                Err(BootstrapErr::Transient(err)) => {
                    let e = truncate_err(&err);
                    tracing::warn!("backup snapshot bootstrap failed (retry in {backoff_secs}s): {e}");
                    let e2 = e.clone();
                    let _ = state
                        .db
                        .run(move |conn| db_repl::update_upstream_state(conn, "reconnecting", Some(&e2), None, None, None, false))
                        .await;
                    sleep_or_wake(Duration::from_secs(backoff_secs), &mut reload_rx, &mut shutdown_rx).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }
            }
        }

        // Reload fresh upstream/node state before opening WS stream.
        let Ok((node, Some(upstream))) = state
            .db
            .run(|conn| Ok((db::get_node_config(conn)?, db_repl::get_upstream(conn)?)))
            .await
        else {
            continue;
        };
        if node.role != NodeRole::Backup || upstream.state == "evicted" || !upstream.bootstrapped {
            continue;
        }

        // Step 2: Outbound persistent WebSocket binlog stream.
        match run_ws_session(&state, &node, &upstream, &mut reload_rx, &mut shutdown_rx, &blob_notify, &ack_notify).await {
            WsExit::Reload => {
                backoff_secs = 1;
            }
            WsExit::ResyncRequired => {
                let _ = state.db.run(db_repl::mark_resync_needed).await;
                backoff_secs = 1;
            }
            WsExit::Evicted(reason) => {
                let r = truncate_err(&reason);
                tracing::warn!("backup evicted by primary: {r}");
                let _ = state
                    .db
                    .run(move |conn| db_repl::update_upstream_state(conn, "evicted", Some(&r), None, None, None, false))
                    .await;
                backoff_secs = 1;
            }
            WsExit::Disconnected(err) => {
                let e = truncate_err(&err);
                tracing::warn!("backup replication stream disconnected (retry in {backoff_secs}s): {e}");
                let e2 = e.clone();
                let _ = state
                    .db
                    .run(move |conn| db_repl::update_upstream_state(conn, "reconnecting", Some(&e2), None, None, None, false))
                    .await;
                sleep_or_wake(Duration::from_secs(backoff_secs), &mut reload_rx, &mut shutdown_rx).await;
                backoff_secs = (backoff_secs * 2).min(30);
            }
        }
    }
}

async fn sleep_or_wake(dur: Duration, reload_rx: &mut watch::Receiver<u64>, shutdown_rx: &mut watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::time::sleep(dur) => {}
        _ = reload_rx.changed() => {}
        _ = shutdown_rx.changed() => {}
    }
}

async fn bootstrap_snapshot(
    state: &AppState,
    client: &reqwest::Client,
    node: &NodeConfig,
    upstream: &UpstreamRow,
) -> Result<(u64, u64), BootstrapErr> {
    let url = format!("{}/v1/repl/snapshot", upstream.primary_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header("x-replica-id", &node.node_id)
        .bearer_auth(&upstream.replica_token)
        .send()
        .await
        .map_err(|e| BootstrapErr::Transient(format!("连接主服务器失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = read_capped_error_text(resp).await;
        if is_eviction_http_error(status.as_u16(), &body) {
            return Err(BootstrapErr::Evicted("该备份服务器已被主服务器剔除或凭据已失效".into()));
        }
        return Err(BootstrapErr::Transient(format!("主服务器快照请求失败 ({status}): {body}")));
    }

    let tmp_path = state.config.data_dir.join(format!("repl_snapshot_{}.ndjson.tmp", Uuid::new_v4()));
    let _tmp_guard = TempFileGuard(tmp_path.clone());
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| BootstrapErr::Transient(format!("创建临时快照文件失败: {e}")))?;
    let mut resp = resp;
    while let Some(bytes) = resp
        .chunk()
        .await
        .map_err(|e| BootstrapErr::Transient(format!("下载快照中断: {e}")))?
    {
        file.write_all(&bytes)
            .await
            .map_err(|e| BootstrapErr::Transient(format!("写入临时快照失败: {e}")))?;
    }
    let _ = file.flush().await;
    drop(file);

    let p_apply = tmp_path.clone();
    let expected_pid = upstream.primary_id.clone();
    let expected_cfg_at = upstream.configured_at;
    let res = state
        .db
        .run(move |conn| db_repl::apply_snapshot_ndjson(conn, &p_apply, &expected_pid, expected_cfg_at))
        .await;

    res.map_err(|e| BootstrapErr::Transient(format!("应用快照失败: {e:?}")))
}

enum WsExit {
    Reload,
    ResyncRequired,
    Evicted(String),
    Disconnected(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_ws_session(
    state: &AppState,
    node: &NodeConfig,
    upstream: &UpstreamRow,
    reload_rx: &mut watch::Receiver<u64>,
    shutdown_rx: &mut watch::Receiver<bool>,
    blob_notify: &Arc<Notify>,
    ack_notify: &Arc<Notify>,
) -> WsExit {
    let ws_url = match to_ws_url(&upstream.primary_url, "/v1/repl/stream") {
        Ok(u) => u,
        Err(e) => return WsExit::Disconnected(e.to_string()),
    };
    let mut req = match ws_url.into_client_request() {
        Ok(r) => r,
        Err(e) => return WsExit::Disconnected(format!("构造 WS 请求失败: {e}")),
    };
    if let Ok(v) = HeaderValue::from_str(&node.node_id) {
        req.headers_mut().insert("x-replica-id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", upstream.replica_token)) {
        req.headers_mut().insert("authorization", v);
    }

    let (ws_stream, _) = match tokio_tungstenite::connect_async(req).await {
        Ok(ok) => ok,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let code = resp.status().as_u16();
            let body_str = resp
                .body()
                .as_deref()
                .map(|b| truncate_err(&String::from_utf8_lossy(b)))
                .unwrap_or_default();
            if is_eviction_http_error(code, &body_str) {
                return WsExit::Evicted("该备份服务器已被主服务器剔除或凭据已失效".into());
            }
            return WsExit::Disconnected(format!("主服务器拒绝同步连接 ({code}): {body_str}"));
        }
        Err(e) => return WsExit::Disconnected(format!("连接主服务器流失败: {e}")),
    };

    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let blob_pending = state.db.run(|conn| db_repl::count_pending_blobs(conn)).await.unwrap_or(0);
    let storage_bytes = compute_node_storage_bytes(state).await;

    let hello = ReplicaFrame::Hello {
        applied_lsn: upstream.applied_lsn,
        epoch: upstream.primary_epoch.clone(),
        name: node.node_name.clone(),
        portal_url: node.portal_url.clone(),
        storage_bytes,
        blob_pending,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if let Ok(txt) = serde_json::to_string(&hello)
        && ws_tx.send(Message::Text(txt.into())).await.is_err()
    {
        return WsExit::Disconnected("发送 Hello 握手失败".into());
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                let _ = ws_tx.send(Message::Close(None)).await;
                return WsExit::Reload;
            }
            _ = reload_rx.changed() => {
                let _ = ws_tx.send(Message::Close(None)).await;
                return WsExit::Reload;
            }
            _ = ack_notify.notified() => {
                if let Err(e) = send_ack(state, &mut ws_tx).await {
                    return WsExit::Disconnected(e);
                }
            }
            _ = heartbeat.tick() => {
                if let Err(e) = send_ack(state, &mut ws_tx).await {
                    return WsExit::Disconnected(e);
                }
            }
            next = ws_rx.next() => {
                let Some(msg_res) = next else {
                    return WsExit::Disconnected("主服务器关闭了同步连接".into());
                };
                let msg = match msg_res {
                    Ok(m) => m,
                    Err(e) => return WsExit::Disconnected(format!("读取同步流异常: {e}")),
                };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => return WsExit::Disconnected("主服务器断开同步连接".into()),
                    Message::Ping(p) => {
                        let _ = ws_tx.send(Message::Pong(p)).await;
                        continue;
                    }
                    Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => continue,
                };
                let frame = match serde_json::from_str::<PrimaryFrame>(&text) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("invalid primary frame: {e}");
                        continue;
                    }
                };
                match frame {
                    PrimaryFrame::Welcome {
                        primary_name,
                        primary_portal_url,
                        epoch,
                        head_lsn,
                        resync_required,
                        ..
                    } => {
                        let epoch_mismatch = epoch != upstream.primary_epoch;
                        let pname = primary_name.clone();
                        let purl = primary_portal_url.clone();
                        let _ = state
                            .db
                            .run(move |conn| {
                                db_repl::update_upstream_state(
                                    conn,
                                    "streaming",
                                    Some(""),
                                    Some(head_lsn),
                                    Some(&pname),
                                    if purl.is_empty() { None } else { Some(&purl) },
                                    true,
                                )
                            })
                            .await;
                        if resync_required || epoch_mismatch {
                            let _ = ws_tx.send(Message::Close(None)).await;
                            return WsExit::ResyncRequired;
                        }
                    }
                    PrimaryFrame::BinlogBatch { head_lsn, events, .. } => {
                        let res = state
                            .db
                            .run(move |conn| db_repl::apply_binlog_batch(conn, &events, head_lsn))
                            .await;
                        match res {
                            Ok((_applied, has_new_blobs, gc_triggered)) => {
                                if has_new_blobs {
                                    blob_notify.notify_one();
                                }
                                if gc_triggered {
                                    let st_gc = state.clone();
                                    tokio::spawn(async move {
                                        let _ = gc::run_backup_orphan_cleanup(&st_gc).await;
                                    });
                                }
                                if let Err(e) = send_ack(state, &mut ws_tx).await {
                                    return WsExit::Disconnected(e);
                                }
                            }
                            Err(e) => {
                                return WsExit::Disconnected(format!("重放 Binlog 失败: {e:?}"));
                            }
                        }
                    }
                    PrimaryFrame::PrimaryInfo { primary_name, primary_portal_url, head_lsn, .. } => {
                        let _ = state
                            .db
                            .run(move |conn| {
                                db_repl::update_upstream_state(
                                    conn,
                                    "streaming",
                                    None,
                                    Some(head_lsn),
                                    Some(&primary_name),
                                    if primary_portal_url.is_empty() { None } else { Some(&primary_portal_url) },
                                    false,
                                )
                            })
                            .await;
                    }
                    PrimaryFrame::Evicted { reason } => {
                        let _ = ws_tx.send(Message::Close(None)).await;
                        return WsExit::Evicted(reason);
                    }
                    PrimaryFrame::Error { code, message } => {
                        if code == "resync_required" {
                            let _ = ws_tx.send(Message::Close(None)).await;
                            return WsExit::ResyncRequired;
                        }
                        if code == "replica_evicted" {
                            let _ = ws_tx.send(Message::Close(None)).await;
                            return WsExit::Evicted(message);
                        }
                        return WsExit::Disconnected(format!("主服务器错误 ({code}): {message}"));
                    }
                    PrimaryFrame::Pong => {}
                }
            }
        }
    }
}

async fn send_ack<S>(state: &AppState, ws_tx: &mut S) -> Result<(), String>
where
    S: SinkExt<Message> + Unpin,
{
    let Ok((node, Some(up), blob_pending)) = state
        .db
        .run(|conn| {
            Ok((
                db::get_node_config(conn)?,
                db_repl::get_upstream(conn)?,
                db_repl::count_pending_blobs(conn)?,
            ))
        })
        .await
    else {
        return Ok(());
    };
    let storage_bytes = compute_node_storage_bytes(state).await;
    let frame = ReplicaFrame::Ack {
        applied_lsn: up.applied_lsn,
        blob_pending,
        storage_bytes,
        name: node.node_name,
        portal_url: node.portal_url,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let txt = serde_json::to_string(&frame).map_err(|e| e.to_string())?;
    ws_tx.send(Message::Text(txt.into())).await.map_err(|_| "发送心跳 Ack 失败".to_string())
}

async fn blob_sync_loop(
    state: AppState,
    mut reload_rx: watch::Receiver<u64>,
    mut shutdown_rx: watch::Receiver<bool>,
    blob_notify: Arc<Notify>,
    ack_notify: Arc<Notify>,
) {
    let client = build_streaming_http_client();

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let _ = reload_rx.borrow_and_update();

        let Ok((node, upstream_opt, pending)) = state
            .db
            .run(|conn| {
                let node = db::get_node_config(conn)?;
                let up = db_repl::get_upstream(conn)?;
                let pending = if node.role == NodeRole::Backup && up.as_ref().is_some_and(|u| u.bootstrapped) {
                    db_repl::list_pending_blobs(conn, 16)?
                } else {
                    Vec::new()
                };
                Ok((node, up, pending))
            })
            .await
        else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };

        let Some(upstream) =
            upstream_opt.filter(|u| node.role == NodeRole::Backup && u.bootstrapped && u.state != "evicted")
        else {
            tokio::select! {
                _ = blob_notify.notified() => {}
                _ = reload_rx.changed() => {}
                _ = shutdown_rx.changed() => {}
            }
            continue;
        };

        if pending.is_empty() {
            tokio::select! {
                _ = blob_notify.notified() => {}
                _ = reload_rx.changed() => {}
                _ = shutdown_rx.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(15)) => {}
            }
            continue;
        }

        let mut progressed = false;
        let mut evicted = false;
        let mut results = futures_util::stream::iter(pending.into_iter().map(|hash| {
            let st = state.clone();
            let cli = client.clone();
            let rid = node.node_id.clone();
            let purl = upstream.primary_url.clone();
            let tok = upstream.replica_token.clone();
            async move { fetch_one_blob(&st, &cli, &rid, &purl, &tok, &hash).await }
        }))
        .buffer_unordered(4);

        while let Some(res) = results.next().await {
            match res {
                BlobFetchOutcome::Done => progressed = true,
                BlobFetchOutcome::Evicted => evicted = true,
                BlobFetchOutcome::RetryLater => {}
            }
        }

        if evicted {
            let _ = state
                .db
                .run(|conn| db_repl::update_upstream_state(conn, "evicted", Some("该备份服务器已被主服务器剔除或凭据已失效"), None, None, None, false))
                .await;
            state.repl_supervisor.trigger_reload();
            continue;
        }
        if progressed {
            ack_notify.notify_one();
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = reload_rx.changed() => {}
            _ = shutdown_rx.changed() => {}
        }
    }
}

enum BlobFetchOutcome {
    Done,
    Evicted,
    RetryLater,
}

async fn fetch_one_blob(
    state: &AppState,
    client: &reqwest::Client,
    replica_id: &str,
    primary_url: &str,
    replica_token: &str,
    hash: &str,
) -> BlobFetchOutcome {
    if state.blobs.exists(hash).await.unwrap_or(false) {
        let h = hash.to_string();
        let _ = state.db.run(move |conn| db_repl::mark_pending_blob_done(conn, &h)).await;
        return BlobFetchOutcome::Done;
    }

    let url = format!("{}/v1/repl/blobs/{hash}", primary_url.trim_end_matches('/'));
    let resp = match client
        .get(&url)
        .header("x-replica-id", replica_id)
        .bearer_auth(replica_token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let h = hash.to_string();
            let msg = truncate_err(&e.to_string());
            let _ = state.db.run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, &msg)).await;
            return BlobFetchOutcome::RetryLater;
        }
    };

    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 401 {
        let body = read_capped_error_text(resp).await;
        if is_eviction_http_error(status.as_u16(), &body) {
            return BlobFetchOutcome::Evicted;
        }
        let h = hash.to_string();
        let _ = state.db.run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, &body)).await;
        return BlobFetchOutcome::RetryLater;
    }
    if status.as_u16() == 404 {
        let h = hash.to_string();
        if state
            .db
            .run(move |conn| db_repl::drop_if_unreferenced_pending_blob(conn, &h))
            .await
            .unwrap_or(false)
        {
            return BlobFetchOutcome::Done;
        }
        let h2 = hash.to_string();
        let _ = state.db.run(move |conn| db_repl::mark_pending_blob_failed(conn, &h2, "primary blob 404")).await;
        return BlobFetchOutcome::RetryLater;
    }
    if !status.is_success() {
        let h = hash.to_string();
        let msg = format!("HTTP {status}");
        let _ = state.db.run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, &msg)).await;
        return BlobFetchOutcome::RetryLater;
    }

    let max_bytes = (state.config.max_blob_mb as usize).saturating_mul(1024 * 1024).max(1024 * 1024);
    let mut bytes = Vec::new();
    let mut resp = resp;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > max_bytes {
                    let h = hash.to_string();
                    let _ = state
                        .db
                        .run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, "blob exceeds max_blob_mb limit"))
                        .await;
                    return BlobFetchOutcome::RetryLater;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => {
                let h = hash.to_string();
                let msg = truncate_err(&e.to_string());
                let _ = state.db.run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, &msg)).await;
                return BlobFetchOutcome::RetryLater;
            }
        }
    }

    if sha256_hex(&bytes) != hash {
        let h = hash.to_string();
        let _ = state
            .db
            .run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, "sha256 mismatch"))
            .await;
        return BlobFetchOutcome::RetryLater;
    }

    let _gc_read = state.blob_gc_lock.read().await;
    if let Err(e) = state.blobs.put(hash, bytes).await {
        let h = hash.to_string();
        let msg = truncate_err(&e.to_string());
        let _ = state.db.run(move |conn| db_repl::mark_pending_blob_failed(conn, &h, &msg)).await;
        return BlobFetchOutcome::RetryLater;
    }
    let h = hash.to_string();
    let _ = state.db.run(move |conn| db_repl::mark_pending_blob_done(conn, &h)).await;
    BlobFetchOutcome::Done
}
