use clap::Parser;
use quiet_sync_server::config::ServerConfig;
use quiet_sync_server::{create_state, gc, router};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("quiet_sync_server=info,tower_http=info")),
        )
        .init();

    let cfg = ServerConfig::parse();
    let bind = cfg.bind.clone();
    let state = create_state(cfg)?;
    gc::spawn_gc(state.clone());
    state.repl_supervisor.spawn(state.clone());
    let supervisor = state.repl_supervisor.clone();
    let hub = state.hub.clone();
    let repl_hub = state.repl_hub.clone();

    let listener = TcpListener::bind(&bind).await?;
    tracing::info!("quiet-sync-server listening on http://{bind} (portal: http://{bind}/portal)");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            supervisor.shutdown();
            hub.disconnect_all();
            repl_hub.disconnect_all();
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down quiet-sync-server");
}
