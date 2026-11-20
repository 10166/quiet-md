//! Built-in local listen API & MCP Streamable HTTP server.
//! Disabled by default (zero threads, zero open ports). On WASM builds this
//! module compiles to no-op stubs.

#[cfg(not(target_family = "wasm"))]
pub mod bridge;
#[cfg(not(target_family = "wasm"))]
pub mod config;
#[cfg(not(target_family = "wasm"))]
pub mod http;
#[cfg(not(target_family = "wasm"))]
pub mod mcp;

use gpui::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerStatus {
    Stopped,
    Running { port: u16 },
    Error(String),
}

pub fn init(cx: &mut App) { imp::init(cx); }
pub fn shutdown(cx: &mut App) { imp::shutdown(cx); }

#[cfg(not(target_family = "wasm"))]
pub use imp::{apply_config, config, regenerate_token, status};

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    pub fn init(_: &mut App) {}
    pub fn shutdown(_: &mut App) {}
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::bridge::{handle_request, BridgeReq, GpuiBackend};
    use super::config::{self, LocalApiConfig};
    use super::http::{self, AuthState};
    use super::mcp::McpServer;
    use super::ServerStatus;
    use gpui::*;
    use quiet_sync_client::tokio;
    use quiet_sync_client::tokio::net::TcpListener;
    use quiet_sync_client::tokio::sync::{mpsc, watch};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, RwLock};
    use std::thread::JoinHandle;

    struct RunningServer {
        port: u16,
        shutdown_tx: watch::Sender<bool>,
        thread: Option<JoinHandle<()>>,
        _pump_task: Task<()>,
    }

    impl RunningServer {
        fn stop(mut self) {
            let _ = self.shutdown_tx.send(true);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    struct LocalApiManager {
        cfg: LocalApiConfig,
        status: ServerStatus,
        auth: Arc<RwLock<AuthState>>,
        read_only: Arc<AtomicBool>,
        snapshot_before_edit: Arc<AtomicBool>,
        running: Option<RunningServer>,
    }

    impl Global for LocalApiManager {}

    pub fn init(cx: &mut App) {
        let cfg = config::load();
        let auth = Arc::new(RwLock::new(AuthState { token: cfg.token.clone(), port: cfg.port }));
        let read_only = Arc::new(AtomicBool::new(cfg.read_only));
        let snapshot_before_edit = Arc::new(AtomicBool::new(cfg.snapshot_before_edit));
        cx.set_global(LocalApiManager {
            cfg: cfg.clone(),
            status: ServerStatus::Stopped,
            auth,
            read_only,
            snapshot_before_edit,
            running: None,
        });
        if cfg.enabled {
            start_server(cx);
        }
    }

    pub fn config(cx: &App) -> LocalApiConfig {
        cx.try_global::<LocalApiManager>().map_or_else(config::load, |m| m.cfg.clone())
    }

    pub fn status(cx: &App) -> ServerStatus {
        cx.try_global::<LocalApiManager>().map_or(ServerStatus::Stopped, |m| m.status.clone())
    }

    pub fn apply_config(mut next: LocalApiConfig, cx: &mut App) -> anyhow::Result<()> {
        if next.port < 1024 {
            anyhow::bail!("Port must be between 1024 and 65535");
        }
        if next.enabled {
            next.ensure_token();
        }
        config::save(&next)?;

        if !cx.has_global::<LocalApiManager>() {
            return Ok(());
        }
        let (need_restart, was_running_port) = {
            let mgr = cx.global_mut::<LocalApiManager>();
            let was_running_port = mgr.running.as_ref().map(|r| r.port);
            let need_restart = next.enabled && was_running_port != Some(next.port);
            let need_stop = !next.enabled || need_restart;

            mgr.cfg = next.clone();
            mgr.read_only.store(next.read_only, Ordering::Release);
            mgr.snapshot_before_edit.store(next.snapshot_before_edit, Ordering::Release);
            if let Ok(mut g) = mgr.auth.write() {
                g.token = next.token.clone();
                g.port = next.port;
            }

            if need_stop {
                if let Some(srv) = mgr.running.take() {
                    srv.stop();
                }
                mgr.status = ServerStatus::Stopped;
            }
            (need_restart, was_running_port)
        };
        if next.enabled && (need_restart || was_running_port.is_none()) {
            start_server(cx);
        }
        notify_windows(cx);
        Ok(())
    }

    pub fn regenerate_token(cx: &mut App) -> anyhow::Result<String> {
        let mut cfg = config(cx);
        cfg.token = config::generate_token();
        let token = cfg.token.clone();
        apply_config(cfg, cx)?;
        Ok(token)
    }

    pub fn shutdown(cx: &mut App) {
        if cx.has_global::<LocalApiManager>() {
            let mgr = cx.global_mut::<LocalApiManager>();
            if let Some(srv) = mgr.running.take() {
                srv.stop();
            }
            mgr.status = ServerStatus::Stopped;
        }
    }

    fn notify_windows(cx: &mut App) {
        for w in cx.windows() {
            if let Some(h) = w.downcast::<crate::app::Workspace>() {
                let _ = h.update(cx, |_, _, cx| cx.notify());
            }
        }
    }

    fn start_server(cx: &mut App) {
        let (port, auth, read_only, snapshot_before_edit) = {
            let Some(mgr) = cx.try_global::<LocalApiManager>() else { return };
            (mgr.cfg.port, mgr.auth.clone(), mgr.read_only.clone(), mgr.snapshot_before_edit.clone())
        };

        // Bind synchronously on loopback first so bind errors surface immediately in Settings UI.
        let std_listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => {
                if let Err(e) = l.set_nonblocking(true) {
                    if cx.has_global::<LocalApiManager>() {
                        cx.global_mut::<LocalApiManager>().status = ServerStatus::Error(e.to_string());
                    }
                    return;
                }
                l
            }
            Err(e) => {
                if cx.has_global::<LocalApiManager>() {
                    cx.global_mut::<LocalApiManager>().status = ServerStatus::Error(e.to_string());
                }
                return;
            }
        };

        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<BridgeReq>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let backend = Arc::new(GpuiBackend::new(req_tx, read_only, snapshot_before_edit));
        let mcp = Arc::new(McpServer::new(backend));

        let pump_task = cx.spawn(async move |cx| {
            loop {
                let req = std::future::poll_fn(|tcx| req_rx.poll_recv(tcx)).await;
                let Some(req) = req else { break };
                cx.update(|cx| handle_request(req, cx));
            }
        });

        let thread = std::thread::Builder::new()
            .name("quiet-local-api".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("local_api tokio runtime failed: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    let listener = match TcpListener::from_std(std_listener) {
                        Ok(l) => l,
                        Err(e) => {
                            log::error!("local_api TcpListener conversion failed: {e}");
                            return;
                        }
                    };
                    http::serve(listener, mcp, auth, shutdown_rx).await;
                });
            })
            .ok();

        if cx.has_global::<LocalApiManager>() {
            let mgr = cx.global_mut::<LocalApiManager>();
            if let Some(t) = thread {
                mgr.running = Some(RunningServer {
                    port,
                    shutdown_tx,
                    thread: Some(t),
                    _pump_task: pump_task,
                });
                mgr.status = ServerStatus::Running { port };
            } else {
                mgr.status = ServerStatus::Error("failed to spawn local API server thread".into());
            }
        }
    }
}
