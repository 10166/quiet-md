pub mod auth;
pub mod blob;
pub mod config;
pub mod db;
pub mod db_admin;
pub mod db_repl;
pub mod gc;
pub mod hub;
pub mod portal;
pub mod repl;
pub mod routes;

use anyhow::Result;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use quiet_sync_proto::api::ErrorBody;
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::auth::{db_err_resp, err_resp, AuthService};
use crate::blob::{DynBlobStore, FsBlobStore, S3BlobStore};
use crate::config::ServerConfig;
use crate::db::{Db, NodeRole};
use crate::hub::VaultHub;
use crate::repl::{ReplHub, ReplSupervisor};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub db: Db,
    pub auth: AuthService,
    pub blobs: DynBlobStore,
    pub hub: VaultHub,
    pub blob_gc_lock: Arc<tokio::sync::RwLock<()>>,
    pub repl_hub: ReplHub,
    pub repl_supervisor: ReplSupervisor,
    pub snapshot_permits: Arc<tokio::sync::Semaphore>,
    pub started_at: u64,
    pub storage_backend: String,
}

impl FromRef<AppState> for AuthService {
    fn from_ref(s: &AppState) -> Self {
        s.auth.clone()
    }
}

impl FromRef<AppState> for Db {
    fn from_ref(s: &AppState) -> Self {
        s.db.clone()
    }
}

pub async fn ensure_primary(state: &AppState) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    let role = state
        .db
        .run(|conn| db::get_node_config(conn).map(|c| c.role))
        .await
        .map_err(db_err_resp)?;
    if role != NodeRole::Primary {
        return Err(err_resp(
            StatusCode::SERVICE_UNAVAILABLE,
            "read_only_replica",
            "当前服务器处于只读备份模式，请连接主服务器执行写入操作",
        ));
    }
    Ok(())
}

pub fn create_state(cfg: ServerConfig) -> Result<AppState> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    repl::cleanup_stale_temp_files(&cfg.data_dir);
    let jwt_secret = cfg.resolve_jwt_secret()?;
    let db = Db::open(&cfg.data_dir.join("sync.db"))?;
    let auth = AuthService::new(jwt_secret);

    if cfg.node_name.is_some() || cfg.public_url.is_some() || cfg.admin_password.as_deref().is_some_and(|s| !s.is_empty()) {
        let mut conn = rusqlite::Connection::open(db.path())?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        let existing_node = db::get_node_config(&conn).map_err(|e| anyhow::anyhow!("read node_config: {e:?}"))?;
        let (admin_hash, bump_ver) = match cfg.admin_password.as_deref() {
            Some(pw) if !pw.is_empty() => {
                let unchanged = existing_node
                    .admin_password_hash
                    .as_deref()
                    .is_some_and(|h| AuthService::verify_password(pw, h));
                if unchanged {
                    (None, false)
                } else {
                    (Some(AuthService::hash_password(pw)?), existing_node.admin_password_hash.is_some())
                }
            }
            _ => (None, false),
        };
        db_admin::apply_startup_overrides(
            &mut conn,
            cfg.node_name.as_deref(),
            cfg.public_url.as_deref(),
            admin_hash.as_deref(),
            bump_ver,
        )
        .map_err(|e| anyhow::anyhow!("apply startup node overrides: {e:?}"))?;
    }

    let (blobs, storage_backend): (DynBlobStore, String) =
        if let (Some(endpoint), Some(bucket), Some(ak), Some(sk)) =
            (&cfg.s3_endpoint, &cfg.s3_bucket, &cfg.s3_access_key, &cfg.s3_secret_key)
        {
            (
                Arc::new(S3BlobStore::new(
                    endpoint.clone(),
                    bucket.clone(),
                    cfg.s3_region.clone(),
                    ak.clone(),
                    sk.clone(),
                    cfg.s3_prefix.clone(),
                )),
                "s3".to_string(),
            )
        } else {
            (Arc::new(FsBlobStore::new(cfg.data_dir.join("blobs"))?), "local_fs".to_string())
        };
    Ok(AppState {
        config: Arc::new(cfg),
        db,
        auth,
        blobs,
        hub: VaultHub::new(),
        blob_gc_lock: Arc::new(tokio::sync::RwLock::new(())),
        repl_hub: ReplHub::new(),
        repl_supervisor: ReplSupervisor::new(),
        snapshot_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        started_at: db::unix_secs(),
        storage_backend,
    })
}

pub fn router(state: AppState) -> Router {
    let max_blob = (state.config.max_blob_mb as usize).saturating_mul(1024 * 1024).max(1024 * 1024);
    let blob_router = Router::new()
        .route("/v1/vaults/{vault_id}/blobs/{hash}", put(routes::blobs::put_blob).get(routes::blobs::get_blob))
        .layer(DefaultBodyLimit::max(max_blob + 1024));

    Router::new()
        .route("/", get(portal::redirect_root))
        .route("/portal", get(portal::serve_portal))
        .route("/healthz", get(healthz))
        // Sync client API
        .route("/v1/auth/register", post(routes::auth::register))
        .route("/v1/auth/login", post(routes::auth::login))
        .route("/v1/auth/refresh", post(routes::auth::refresh))
        .route("/v1/auth/logout", post(routes::auth::logout))
        .route("/v1/auth/ws-ticket", post(routes::auth::issue_ws_ticket))
        .route("/v1/auth/me", get(routes::auth::me))
        .route("/v1/storage", get(routes::auth::storage_usage))
        .route("/v1/devices", get(routes::auth::list_devices))
        .route("/v1/devices/{device_id}", delete(routes::auth::revoke_device))
        .route("/v1/vaults", get(routes::vaults::list_vaults).post(routes::vaults::create_vault))
        .route("/v1/vaults/{vault_id}", delete(routes::vaults::delete_vault))
        .route("/v1/vaults/{vault_id}/e2ee-key", put(routes::vaults::update_e2ee_key))
        .route("/v1/vaults/{vault_id}/changes", get(routes::sync::get_changes))
        .route("/v1/vaults/{vault_id}/snapshot", get(routes::sync::get_snapshot))
        .route("/v1/vaults/{vault_id}/push", post(routes::sync::push))
        .route("/v1/vaults/{vault_id}/blobs/exists", post(routes::blobs::blobs_exists))
        .route("/v1/vaults/{vault_id}/ws", get(routes::ws::ws_upgrade))
        // Primary-Backup replication API
        .route("/v1/repl/register", post(routes::repl::register_replica))
        .route("/v1/repl/snapshot", get(routes::repl::get_snapshot))
        .route("/v1/repl/binlog", get(routes::repl::get_binlog))
        .route("/v1/repl/blobs/{hash}", get(routes::repl::get_blob))
        .route("/v1/repl/stream", get(routes::repl::ws_stream))
        // Admin Portal API
        .route("/api/admin/auth/status", get(routes::admin::auth_status))
        .route("/api/admin/auth/setup", post(routes::admin::auth_setup))
        .route("/api/admin/auth/login", post(routes::admin::auth_login))
        .route("/api/admin/auth/logout", post(routes::admin::auth_logout))
        .route("/api/admin/auth/change-password", post(routes::admin::change_password))
        .route("/api/admin/overview", get(routes::admin::get_overview))
        .route("/api/admin/node", put(routes::admin::update_node))
        .route("/api/admin/gc", post(routes::admin::run_gc_now))
        .route("/api/admin/users", get(routes::admin::list_users))
        .route("/api/admin/users/{user_id}", get(routes::admin::get_user_detail).delete(routes::admin::delete_user))
        .route("/api/admin/users/{user_id}/devices/{device_id}", delete(routes::admin::revoke_user_device))
        .route("/api/admin/vaults/{vault_id}", delete(routes::admin::delete_vault_admin))
        .route("/api/admin/storage", get(routes::admin::get_storage))
        .route("/api/admin/replication", get(routes::admin::get_replication))
        .route("/api/admin/replication/configure-backup", post(routes::admin::configure_backup))
        .route("/api/admin/replication/promote", post(routes::admin::promote_to_primary))
        .route("/api/admin/replication/resync", post(routes::admin::trigger_resync))
        .route("/api/admin/replication/replicas/{replica_id}", delete(routes::admin::evict_replica))
        .route("/api/admin/replication/replicas/{replica_id}/purge", delete(routes::admin::purge_evicted_replica))
        .merge(blob_router)
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(json!({ "ok": true, "service": "quiet-sync-server", "version": env!("CARGO_PKG_VERSION") })))
}

/// Start an ephemeral server on `127.0.0.1:0` for integration tests.
pub async fn spawn_test_server(data_dir: PathBuf) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = create_state(ServerConfig::for_test(data_dir))?;
    state.repl_supervisor.spawn(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((addr, handle))
}
