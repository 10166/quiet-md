use futures_util::{SinkExt, StreamExt};
use quiet_sync_proto::api::{ClientFrame, OnlineDevice, PushItemResult, PushOp, RemoteChange, ServerFrame};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::http::{ApiClient, ApiError};

#[derive(Debug)]
pub enum WsSignal {
    Connected { head_seq: u64, online_devices: Vec<OnlineDevice> },
    Disconnected(Option<String>),
    AuthFailed,
    Broadcast(RemoteChange),
    Presence(Vec<OnlineDevice>),
}

#[derive(Clone)]
pub struct WsConn {
    out_tx: Arc<Mutex<Option<mpsc::UnboundedSender<ClientFrame>>>>,
    pending_pushes: Arc<Mutex<HashMap<String, oneshot::Sender<(u64, Vec<PushItemResult>)>>>>,
}

impl WsConn {
    pub fn new() -> Self {
        Self {
            out_tx: Arc::new(Mutex::new(None)),
            pending_pushes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn is_connected(&self) -> bool {
        self.out_tx.lock().await.is_some()
    }

    pub async fn send_presence(&self, active_entity_id: Option<String>) {
        if let Some(tx) = self.out_tx.lock().await.as_ref() {
            let _ = tx.send(ClientFrame::Presence { active_entity_id });
        }
    }

    /// Attempt to push ops over an open WS connection with a 10s timeout. Returns `None` if WS is unavailable.
    pub async fn try_push(&self, ops: Vec<PushOp>) -> Option<anyhow::Result<(u64, Vec<PushItemResult>)>> {
        let tx = self.out_tx.lock().await.clone()?;
        let req_id = Uuid::new_v4().to_string();
        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending_pushes.lock().await.insert(req_id.clone(), resp_tx);
        if tx.send(ClientFrame::Push { req_id: req_id.clone(), ops }).is_err() {
            self.pending_pushes.lock().await.remove(&req_id);
            return None;
        }
        match tokio::time::timeout(Duration::from_secs(10), resp_rx).await {
            Ok(Ok(res)) => Some(Ok(res)),
            Ok(Err(_)) => None,
            Err(_) => {
                self.pending_pushes.lock().await.remove(&req_id);
                Some(Err(anyhow::anyhow!("WebSocket push timed out")))
            }
        }
    }

    async fn clear_active(&self) {
        *self.out_tx.lock().await = None;
        self.pending_pushes.lock().await.clear();
    }
}

impl Default for WsConn {
    fn default() -> Self {
        Self::new()
    }
}

pub fn spawn_ws_loop(
    api: ApiClient,
    ws_conn: WsConn,
    sig_tx: mpsc::UnboundedSender<WsSignal>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            if *stop_rx.borrow() {
                break;
            }
            let Some(creds) = api.credentials().await else {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                    _ = stop_rx.changed() => break,
                }
            };
            let Some(vault_id) = creds.vault_id.clone() else {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                    _ = stop_rx.changed() => break,
                }
            };

            let ticket = match api.ws_ticket(&vault_id).await {
                Ok(t) => t.ticket,
                Err(ApiError::AuthExpired) => {
                    let _ = sig_tx.send(WsSignal::AuthFailed);
                    break;
                }
                Err(e) => {
                    let _ = sig_tx.send(WsSignal::Disconnected(Some(e.to_string())));
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {},
                        _ = stop_rx.changed() => break,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };

            let Ok(ws_url) = to_ws_url(&creds.server_url, &vault_id, &ticket) else {
                let _ = sig_tx.send(WsSignal::AuthFailed);
                break;
            };
            let conn_res = connect_async(&ws_url).await;
            let Ok((stream, _)) = conn_res else {
                let err_msg = conn_res.err().map(|e| e.to_string());
                let _ = sig_tx.send(WsSignal::Disconnected(err_msg));
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = stop_rx.changed() => break,
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            };

            backoff = Duration::from_secs(1);
            let (mut write, mut read) = stream.split();
            let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientFrame>();
            *ws_conn.out_tx.lock().await = Some(out_tx.clone());

            let _ = out_tx.send(ClientFrame::Hello {
                device_id: creds.device_id.clone(),
                device_name: creds.device_name.clone(),
                platform: current_platform().into(),
                last_seq: 0,
            });

            let mut ping_interval = tokio::time::interval(Duration::from_secs(25));
            ping_interval.tick().await;

            loop {
                tokio::select! {
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            let _ = write.send(Message::Close(None)).await;
                            ws_conn.clear_active().await;
                            return;
                        }
                    }
                    _ = ping_interval.tick() => {
                        let _ = out_tx.send(ClientFrame::Ping);
                    }
                    Some(frame) = out_rx.recv() => {
                        if let Ok(txt) = serde_json::to_string(&frame)
                            && write.send(Message::Text(txt.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(txt))) => {
                                if let Ok(srv) = serde_json::from_str::<ServerFrame>(&txt) {
                                    match srv {
                                        ServerFrame::Welcome { head_seq, online_devices } => {
                                            let _ = sig_tx.send(WsSignal::Connected { head_seq, online_devices });
                                        }
                                        ServerFrame::Broadcast { item } => {
                                            let _ = sig_tx.send(WsSignal::Broadcast(item));
                                        }
                                        ServerFrame::PresenceUpdate { devices } => {
                                            let _ = sig_tx.send(WsSignal::Presence(devices));
                                        }
                                        ServerFrame::PushAck { req_id, head_seq, results } => {
                                            if let Some(tx) = ws_conn.pending_pushes.lock().await.remove(&req_id) {
                                                let _ = tx.send((head_seq, results));
                                            }
                                        }
                                        ServerFrame::Pong | ServerFrame::Changes { .. } | ServerFrame::Error { .. } => {}
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                            Some(Ok(_)) => {}
                        }
                    }
                }
            }

            ws_conn.clear_active().await;
            let _ = sig_tx.send(WsSignal::Disconnected(None));
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {},
                _ = stop_rx.changed() => break,
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    })
}

fn to_ws_url(server_url: &str, vault_id: &str, ticket: &str) -> Result<String, ApiError> {
    let base = crate::http::validate_server_url(server_url)?;
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        unreachable!("validated URL only uses https:// or http://")
    };
    Ok(format!("{ws}/v1/vaults/{vault_id}/ws?ticket={ticket}"))
}

pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}
