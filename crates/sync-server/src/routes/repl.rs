use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use quiet_sync_proto::api::ErrorBody;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::{db_err_resp, err_resp, validate_http_url, AuthService, ReplicaAuth};
use crate::blob::validate_hash;
use crate::db::{self, NodeRole};
use crate::db_repl::{self, BinlogRecord};
use crate::repl::{EvictSignal, PrimaryCmd, PrimaryFrame, ReplicaFrame};
use crate::AppState;

const MAX_REPL_WS_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplRegisterReq {
    pub admin_password: String,
    pub replica_id: String,
    pub replica_name: String,
    #[serde(default)]
    pub portal_url: String,
    #[serde(default)]
    pub server_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplRegisterResp {
    pub primary_id: String,
    pub primary_name: String,
    pub primary_portal_url: String,
    pub primary_epoch: String,
    pub head_lsn: u64,
    pub replica_token: String,
}

fn infer_origin_from_headers(headers: &HeaderMap) -> String {
    let Some(host) = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return String::new();
    };
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"))
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "http".to_string());
    let candidate = format!("{proto}://{host}");
    validate_http_url(&candidate, true).unwrap_or_default()
}

fn extract_remote_hint(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .unwrap_or("")
        .chars()
        .take(64)
        .collect()
}

pub async fn register_replica(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ReplRegisterReq>,
) -> Result<(StatusCode, Json<ReplRegisterResp>), (StatusCode, Json<ErrorBody>)> {
    if !state.auth.check_rate_limit("admin_pw:__global__", 20, Duration::from_secs(300)) {
        return Err(err_resp(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "配对或登录尝试过于频繁，请稍后再试"));
    }
    let replica_id = req.replica_id.trim().to_string();
    let replica_name = req.replica_name.trim().to_string();
    if replica_id.is_empty() || replica_id.len() > 128 || replica_name.is_empty() || replica_name.chars().count() > 128 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "备份节点标识或名称无效"));
    }
    let portal_url = validate_http_url(&req.portal_url, true)
        .map_err(|m| err_resp(StatusCode::BAD_REQUEST, "invalid_portal_url", m))?;

    let (node, head_lsn) = state
        .db
        .run(|conn| Ok((db::get_node_config(conn)?, db::get_head_lsn(conn)?)))
        .await
        .map_err(db_err_resp)?;

    if node.role != NodeRole::Primary {
        return Err(err_resp(StatusCode::CONFLICT, "not_primary", "目标服务器本身是备份节点，无法作为主服务器"));
    }
    if node.node_id == replica_id {
        return Err(err_resp(StatusCode::BAD_REQUEST, "self_replication", "不能将服务器自身配置为自己的主服务器"));
    }
    let Some(pw_hash) = node.admin_password_hash.clone() else {
        return Err(err_resp(
            StatusCode::CONFLICT,
            "admin_not_initialized",
            "主服务器尚未设置管理员密码，请先访问主服务器 Portal 完成初始化",
        ));
    };

    let _permit = state.auth.acquire_argon2_permit().await?;
    let pw = req.admin_password.clone();
    let ok = tokio::task::spawn_blocking(move || AuthService::verify_password(&pw, &pw_hash))
        .await
        .unwrap_or(false);
    if !ok {
        return Err(err_resp(StatusCode::UNAUTHORIZED, "invalid_credentials", "主服务器管理员密码错误"));
    }

    let (replica_token, token_hash) = AuthService::generate_replica_token();
    let remote_addr = extract_remote_hint(&headers);
    let version = req.server_version.trim().chars().take(32).collect::<String>();
    let rid = replica_id.clone();
    let rname = replica_name.clone();
    let rpurl = portal_url.clone();
    state
        .db
        .run(move |conn| db_repl::register_replica(conn, &rid, &rname, &rpurl, &token_hash, &version, &remote_addr))
        .await
        .map_err(db_err_resp)?;

    let primary_portal_url = if node.portal_url.is_empty() {
        infer_origin_from_headers(&headers)
    } else {
        node.portal_url
    };

    Ok((
        StatusCode::CREATED,
        Json(ReplRegisterResp {
            primary_id: node.node_id,
            primary_name: node.node_name,
            primary_portal_url,
            primary_epoch: node.repl_epoch,
            head_lsn,
            replica_token,
        }),
    ))
}

pub async fn get_snapshot(
    State(state): State<AppState>,
    _replica: ReplicaAuth,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<ErrorBody>)> {
    let permit = tokio::time::timeout(Duration::from_secs(5), state.snapshot_permits.clone().acquire_owned())
        .await
        .map_err(|_| err_resp(StatusCode::SERVICE_UNAVAILABLE, "snapshot_busy", "快照导出并发已满，请稍后重试"))?
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "快照导出不可用"))?;

    let fallback_portal_url = infer_origin_from_headers(&headers);
    let db = state.db.clone();
    let (tx, rx) = mpsc::channel::<Result<String, std::io::Error>>(32);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut ro_conn = match db.open_readonly() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                return;
            }
        };
        if let Err(e) = db_repl::export_snapshot_stream(&mut ro_conn, &fallback_portal_url, tx.clone()) {
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let body = Body::from_stream(stream);
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "构造快照响应失败"))?;
    Ok(resp)
}

#[derive(Deserialize)]
pub struct BinlogQuery {
    #[serde(default)]
    pub since_lsn: u64,
    #[serde(default = "default_binlog_limit")]
    pub limit: u32,
    #[serde(default)]
    pub epoch: Option<String>,
}

fn default_binlog_limit() -> u32 {
    200
}

#[derive(Serialize)]
pub struct BinlogResp {
    pub epoch: String,
    pub head_lsn: u64,
    pub min_retained_lsn: u64,
    pub resync_required: bool,
    pub has_more: bool,
    pub events: Vec<BinlogRecord>,
}

pub async fn get_binlog(
    State(state): State<AppState>,
    _replica: ReplicaAuth,
    Query(q): Query<BinlogQuery>,
) -> Result<Json<BinlogResp>, (StatusCode, Json<ErrorBody>)> {
    let (cfg, head_lsn, min_retained_lsn, events, has_more, resync_required) = state
        .db
        .run(move |conn| {
            let cfg = db::get_node_config(conn)?;
            let (head, min_ret, events, has_more, resync) = db_repl::read_binlog_since(conn, q.since_lsn, q.limit)?;
            Ok((cfg, head, min_ret, events, has_more, resync))
        })
        .await
        .map_err(db_err_resp)?;

    let epoch_mismatch = q.epoch.as_deref().is_some_and(|e| !e.is_empty() && e != cfg.repl_epoch);
    let resync = resync_required || epoch_mismatch;
    Ok(Json(BinlogResp {
        epoch: cfg.repl_epoch,
        head_lsn,
        min_retained_lsn,
        resync_required: resync,
        has_more: if resync { false } else { has_more },
        events: if resync { Vec::new() } else { events },
    }))
}

pub async fn get_blob(
    State(state): State<AppState>,
    _replica: ReplicaAuth,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    if validate_hash(&hash).is_err() {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_hash", "无效的 Blob 哈希"));
    }
    let bytes = state
        .blobs
        .get(&hash)
        .await
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "blob_store", "读取附件失败"))?
        .ok_or_else(|| err_resp(StatusCode::NOT_FOUND, "not_found", "附件不存在"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

pub async fn ws_stream(
    State(state): State<AppState>,
    replica: ReplicaAuth,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, (StatusCode, Json<ErrorBody>)> {
    let fallback_portal_url = infer_origin_from_headers(&headers);
    let remote_addr = extract_remote_hint(&headers);
    let ws = ws.max_message_size(MAX_REPL_WS_BYTES).max_frame_size(MAX_REPL_WS_BYTES);
    Ok(ws.on_upgrade(move |socket| {
        handle_repl_socket(state, socket, replica.replica_id, fallback_portal_url, remote_addr)
    }))
}

async fn send_primary_frame(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    frame: &PrimaryFrame,
) -> bool {
    let Ok(json) = serde_json::to_string(frame) else { return true };
    ws_tx.send(Message::Text(json.into())).await.is_ok()
}

async fn handle_repl_socket(
    state: AppState,
    socket: WebSocket,
    replica_id: String,
    fallback_portal_url: String,
    remote_addr: String,
) {
    let conn_id = Uuid::new_v4().to_string();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (info_tx, mut info_rx) = mpsc::channel::<PrimaryCmd>(16);
    let mut evict_rx = state.repl_hub.register_conn(&replica_id, &conn_id, info_tx);

    let mut binlog_rx = state.db.subscribe_binlog();
    let mut greeted = false;
    let mut sent_lsn = 0u64;

    let idle_timeout = Duration::from_secs(45);
    let mut idle_deadline = tokio::time::Instant::now() + idle_timeout;

    loop {
        // Always prioritize eviction/close signals (even during multi-batch binlog catch-up).
        let pending_sig = evict_rx.borrow().clone();
        if let Some(sig) = pending_sig {
            match sig {
                EvictSignal::Evict(reason) => {
                    let _ = send_primary_frame(&mut ws_tx, &PrimaryFrame::Evicted { reason }).await;
                }
                EvictSignal::Close => {}
            }
            let _ = ws_tx.send(Message::Close(None)).await;
            break;
        }

        // If greeted and there are unpushed binlog events, stream batches immediately.
        if greeted {
            let latest = *binlog_rx.borrow_and_update();
            if latest > sent_lsn {
                match state.db.run(move |conn| db_repl::read_binlog_since(conn, sent_lsn, 100)).await {
                    Ok((head_lsn, _min_ret, events, has_more, resync_required)) => {
                        if resync_required {
                            let _ = send_primary_frame(
                                &mut ws_tx,
                                &PrimaryFrame::Error {
                                    code: "resync_required".into(),
                                    message: "主节点 Binlog 已裁剪，需要全量重同步".into(),
                                },
                            )
                            .await;
                            break;
                        }
                        if !events.is_empty() {
                            let last = events.last().map_or(sent_lsn, |e| e.lsn);
                            if !send_primary_frame(
                                &mut ws_tx,
                                &PrimaryFrame::BinlogBatch { head_lsn, events, has_more },
                            )
                            .await
                            {
                                break;
                            }
                            sent_lsn = last;
                            if has_more {
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("repl read_binlog_since failed: {e:?}");
                        break;
                    }
                }
            }
        }

        tokio::select! {
            biased;
            _ = evict_rx.changed() => {
                let pending_sig = evict_rx.borrow().clone();
                if let Some(sig) = pending_sig {
                    match sig {
                        EvictSignal::Evict(reason) => {
                            let _ = send_primary_frame(&mut ws_tx, &PrimaryFrame::Evicted { reason }).await;
                        }
                        EvictSignal::Close => {}
                    }
                    let _ = ws_tx.send(Message::Close(None)).await;
                    break;
                }
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::info!(replica_id, "replica stream idle timeout");
                break;
            }
            changed = binlog_rx.changed(), if greeted => {
                if changed.is_err() {
                    break;
                }
            }
            cmd = info_rx.recv() => {
                match cmd {
                    Some(PrimaryCmd::PrimaryInfo { primary_id, primary_name, primary_portal_url, head_lsn }) => {
                        let purl = if primary_portal_url.is_empty() { fallback_portal_url.clone() } else { primary_portal_url };
                        if !send_primary_frame(
                            &mut ws_tx,
                            &PrimaryFrame::PrimaryInfo { primary_id, primary_name, primary_portal_url: purl, head_lsn },
                        ).await {
                            break;
                        }
                    }
                    None => break,
                }
            }
            next = ws_rx.next() => {
                let Some(Ok(msg)) = next else { break };
                idle_deadline = tokio::time::Instant::now() + idle_timeout;
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    Message::Ping(p) => {
                        let _ = ws_tx.send(Message::Pong(p)).await;
                        continue;
                    }
                    Message::Pong(_) | Message::Binary(_) => continue,
                };
                let frame = match serde_json::from_str::<ReplicaFrame>(&text) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = send_primary_frame(&mut ws_tx, &PrimaryFrame::Error {
                            code: "bad_frame".into(),
                            message: format!("invalid replica frame: {e}"),
                        }).await;
                        continue;
                    }
                };
                match frame {
                    ReplicaFrame::Ping => {
                        if !send_primary_frame(&mut ws_tx, &PrimaryFrame::Pong).await {
                            break;
                        }
                    }
                    ReplicaFrame::Hello {
                        applied_lsn,
                        epoch,
                        name,
                        portal_url,
                        storage_bytes,
                        blob_pending,
                        server_version,
                    } => {
                        let safe_portal = validate_http_url(&portal_url, true).ok();
                        let rid = replica_id.clone();
                        let raddr = remote_addr.clone();
                        let Ok((node, head_lsn, still_active)) = state
                            .db
                            .run(move |conn| {
                                let node = db::get_node_config(conn)?;
                                let head = db::get_head_lsn(conn)?;
                                let active = db_repl::update_replica_heartbeat(
                                    conn,
                                    &rid,
                                    Some(&name),
                                    safe_portal.as_deref(),
                                    applied_lsn,
                                    blob_pending,
                                    storage_bytes,
                                    Some(&server_version),
                                    if raddr.is_empty() { None } else { Some(&raddr) },
                                )?;
                                Ok((node, head, active))
                            })
                            .await
                        else {
                            break;
                        };
                        if !still_active {
                            let _ = send_primary_frame(&mut ws_tx, &PrimaryFrame::Evicted {
                                reason: "该备份服务器已被主服务器剔除".into(),
                            }).await;
                            let _ = ws_tx.send(Message::Close(None)).await;
                            break;
                        }
                        if node.role != NodeRole::Primary {
                            let _ = send_primary_frame(&mut ws_tx, &PrimaryFrame::Error {
                                code: "not_primary".into(),
                                message: "当前节点已不是主服务器".into(),
                            }).await;
                            break;
                        }
                        let resync_required = epoch != node.repl_epoch
                            || applied_lsn < node.min_retained_lsn
                            || applied_lsn > head_lsn;
                        let purl = if node.portal_url.is_empty() { fallback_portal_url.clone() } else { node.portal_url };
                        if !send_primary_frame(
                            &mut ws_tx,
                            &PrimaryFrame::Welcome {
                                primary_id: node.node_id,
                                primary_name: node.node_name,
                                primary_portal_url: purl,
                                epoch: node.repl_epoch,
                                head_lsn,
                                min_retained_lsn: node.min_retained_lsn,
                                resync_required,
                            },
                        )
                        .await
                        {
                            break;
                        }
                        if resync_required {
                            break;
                        }
                        sent_lsn = applied_lsn;
                        greeted = true;
                        let _ = binlog_rx.borrow_and_update();
                    }
                    ReplicaFrame::Ack {
                        applied_lsn,
                        blob_pending,
                        storage_bytes,
                        name,
                        portal_url,
                        server_version,
                    } => {
                        let safe_portal = validate_http_url(&portal_url, true).ok();
                        let rid = replica_id.clone();
                        let raddr = remote_addr.clone();
                        let still_active = state
                            .db
                            .run(move |conn| {
                                db_repl::update_replica_heartbeat(
                                    conn,
                                    &rid,
                                    Some(&name),
                                    safe_portal.as_deref(),
                                    applied_lsn,
                                    blob_pending,
                                    storage_bytes,
                                    Some(&server_version),
                                    if raddr.is_empty() { None } else { Some(&raddr) },
                                )
                            })
                            .await
                            .unwrap_or(false);
                        if !still_active {
                            let _ = send_primary_frame(&mut ws_tx, &PrimaryFrame::Evicted {
                                reason: "该备份服务器已被主服务器剔除".into(),
                            }).await;
                            let _ = ws_tx.send(Message::Close(None)).await;
                            break;
                        }
                    }
                }
            }
        }
    }

    state.repl_hub.unregister_conn(&replica_id, &conn_id);
}
