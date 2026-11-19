use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use quiet_sync_proto::api::{ClientFrame, ErrorBody, ServerFrame};
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use crate::auth::{db_err_resp, err_resp};
use crate::db::{self, DbError};
use crate::hub::{try_send_frame, WsOutbound, MAX_WS_QUEUE_MSGS};
use crate::{ensure_primary, AppState};

const MAX_WS_FRAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
pub struct WsQuery {
    pub ticket: Option<String>,
}

pub async fn ws_upgrade(
    State(state): State<AppState>,
    Path(vault_id): Path<String>,
    Query(q): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let (user_id, device_id, session_exp) = if let Some(ticket) = q.ticket.as_deref() {
        let t = state
            .auth
            .consume_ws_ticket(ticket, &vault_id)
            .ok_or_else(|| err_resp(StatusCode::UNAUTHORIZED, "invalid_ticket", "WebSocket 门票无效或已过期"))?;
        (t.user_id, t.device_id, t.session_exp)
    } else if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        let claims = state
            .auth
            .verify_access_token(bearer)
            .map_err(|_| err_resp(StatusCode::UNAUTHORIZED, "token_invalid", "访问令牌无效或已过期"))?;
        (claims.sub, claims.did, claims.exp)
    } else {
        return Err(err_resp(StatusCode::UNAUTHORIZED, "unauthorized", "缺少 WebSocket 鉴权凭据"));
    };

    let uid = user_id.clone();
    let did = device_id.clone();
    let vid = vault_id.clone();
    let (head_seq, device_row) = state
        .db
        .run(move |conn| {
            if !db::is_device_active(conn, &uid, &did)? {
                return Err(db::DbError::Forbidden("设备已注销"));
            }
            let (head, _) = db::verify_vault_owner(conn, &uid, &vid)?;
            let dev = db::get_device(conn, &did)?.ok_or(db::DbError::Forbidden("设备不存在"))?;
            let _ = db::touch_device(conn, &did);
            Ok((head, dev))
        })
        .await
        .map_err(|e| match e {
            db::DbError::Forbidden(m) => err_resp(StatusCode::UNAUTHORIZED, "device_revoked", m),
            other => db_err_resp(other),
        })?;

    let ws = ws.max_message_size(MAX_WS_FRAME_BYTES).max_frame_size(MAX_WS_FRAME_BYTES);
    Ok(ws.on_upgrade(move |socket| {
        handle_socket(state, socket, vault_id, user_id, device_id, device_row.name, device_row.platform, head_seq, session_exp)
    }))
}

#[allow(clippy::too_many_arguments)]
async fn handle_socket(
    state: AppState,
    socket: WebSocket,
    vault_id: String,
    user_id: String,
    device_id: String,
    device_name: String,
    platform: String,
    head_seq: u64,
    session_exp: u64,
) {
    let conn_id = Uuid::new_v4().to_string();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOutbound>(MAX_WS_QUEUE_MSGS);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let (writer_done_tx, mut writer_done_rx) = oneshot::channel::<()>();

    let online_devices = state.hub.join(
        &vault_id,
        &conn_id,
        &user_id,
        &device_id,
        &device_name,
        &platform,
        out_tx.clone(),
        queued_bytes.clone(),
        cancel_tx.clone(),
    );
    let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Welcome { head_seq, online_devices });

    let writer_bytes = queued_bytes.clone();
    let mut writer_cancel_rx = cancel_tx.subscribe();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel_rx.changed() => {
                    if *writer_cancel_rx.borrow() {
                        let _ = ws_tx.send(Message::Close(None)).await;
                        break;
                    }
                }
                msg = out_rx.recv() => {
                    match msg {
                        Some(WsOutbound::Text { json, bytes }) => {
                            writer_bytes.fetch_sub(bytes, Ordering::AcqRel);
                            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                        Some(WsOutbound::Close) | None => {
                            let _ = ws_tx.send(Message::Close(None)).await;
                            break;
                        }
                    }
                }
            }
        }
        let _ = writer_done_tx.send(());
    });

    let secs_left = session_exp.saturating_sub(db::unix_secs());
    let exp_sleep = tokio::time::sleep(Duration::from_secs(secs_left));
    tokio::pin!(exp_sleep);

    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    break;
                }
            }
            _ = &mut exp_sleep => {
                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                    code: "token_expired".into(),
                    message: "WebSocket 会话已过期".into(),
                });
                break;
            }
            _ = &mut writer_done_rx => {
                break;
            }
            next = ws_rx.next() => {
                let Some(Ok(msg)) = next else { break };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => continue,
                };
                if text.len() > MAX_WS_FRAME_BYTES {
                    let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                        code: "frame_too_large".into(),
                        message: "WebSocket 帧超出大小限制".into(),
                    });
                    break;
                }
                let frame = match serde_json::from_str::<ClientFrame>(&text) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                            code: "bad_frame".into(),
                            message: format!("invalid frame: {e}"),
                        });
                        continue;
                    }
                };
                match frame {
                    ClientFrame::Ping => {
                        let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Pong);
                    }
                    ClientFrame::Hello { .. } => {
                        let uid = user_id.clone();
                        let did = device_id.clone();
                        let vid = vault_id.clone();
                        match state.db.run(move |conn| {
                            if !db::is_device_active(conn, &uid, &did)? {
                                return Err(DbError::Forbidden("设备已注销"));
                            }
                            db::verify_vault_owner(conn, &uid, &vid)
                        }).await {
                            Ok((head, _)) => {
                                let online_devices = state.hub.join(
                                    &vault_id,
                                    &conn_id,
                                    &user_id,
                                    &device_id,
                                    &device_name,
                                    &platform,
                                    out_tx.clone(),
                                    queued_bytes.clone(),
                                    cancel_tx.clone(),
                                );
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Welcome { head_seq: head, online_devices });
                            }
                            Err(DbError::Forbidden(_)) => break,
                            Err(_) => {}
                        }
                    }
                    ClientFrame::Presence { active_entity_id } => {
                        state.hub.set_presence(&vault_id, &conn_id, active_entity_id);
                    }
                    ClientFrame::Pull { req_id, since_seq, limit } => {
                        let uid = user_id.clone();
                        let did = device_id.clone();
                        let vid = vault_id.clone();
                        match state.db.run(move |conn| {
                            if !db::is_device_active(conn, &uid, &did)? {
                                return Err(DbError::Forbidden("当前设备已被移除或已注销"));
                            }
                            db::get_changes(conn, &vid, &uid, since_seq, limit)
                        }).await {
                            Ok((head_seq, changes, has_more, resync_required)) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Changes {
                                    req_id,
                                    head_seq,
                                    changes,
                                    has_more,
                                    resync_required,
                                });
                            }
                            Err(DbError::ReadOnly(msg)) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                    code: "read_only_replica".into(),
                                    message: msg.into(),
                                });
                                break;
                            }
                            Err(DbError::Forbidden(msg)) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                    code: "device_revoked".into(),
                                    message: msg.into(),
                                });
                                break;
                            }
                            Err(e) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                    code: "pull_failed".into(),
                                    message: format!("{e:?}"),
                                });
                            }
                        }
                    }
                    ClientFrame::Push { req_id, ops } => {
                        if ops.len() > 200 {
                            let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                code: "too_many_ops".into(),
                                message: "单次推送操作不能超过 200 项".into(),
                            });
                            continue;
                        }
                        let uid = user_id.clone();
                        let did = device_id.clone();
                        let vid = vault_id.clone();
                        match state
                            .db
                            .run(move |conn| {
                                let _ = db::touch_device(conn, &did);
                                db::push_ops(conn, &vid, &uid, &did, ops)
                            })
                            .await
                        {
                            Ok((head_seq, results, broadcasts)) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::PushAck { req_id, head_seq, results });
                                state.hub.broadcast_changes(&vault_id, &device_id, &broadcasts);
                            }
                            Err(DbError::ReadOnly(msg)) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                    code: "read_only_replica".into(),
                                    message: msg.into(),
                                });
                                break;
                            }
                            Err(DbError::Forbidden(msg)) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                    code: "device_revoked".into(),
                                    message: msg.into(),
                                });
                                break;
                            }
                            Err(e) => {
                                let _ = try_send_frame(&out_tx, &queued_bytes, &cancel_tx, &ServerFrame::Error {
                                    code: "push_failed".into(),
                                    message: format!("{e:?}"),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = cancel_tx.send(true);
    state.hub.leave(&vault_id, &conn_id);
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_millis(200), writer).await;
}
