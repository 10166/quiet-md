use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use quiet_sync_proto::api::ErrorBody;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::auth::{
    admin_session_cookie_header, db_err_resp, err_resp, extract_cookie, is_https_request,
    validate_http_url, AdminAuth, AuthService, ADMIN_SESSION_COOKIE, ADMIN_TOKEN_TTL_SECS,
};
use crate::db::{self, NodeRole};
use crate::db_admin;
use crate::db_repl;
use crate::repl::{compute_node_storage_bytes, local_db_file_sizes};
use crate::routes::repl::{ReplRegisterReq, ReplRegisterResp};
use crate::{ensure_primary, gc, AppState};

// ---- Admin Auth ----

#[derive(Serialize)]
pub struct AdminAuthStatusResp {
    pub initialized: bool,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub node_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<NodeRole>,
    pub version: &'static str,
}

pub async fn auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminAuthStatusResp>, (StatusCode, Json<ErrorBody>)> {
    let node = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    let initialized = node.admin_password_hash.is_some();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| extract_cookie(&headers, ADMIN_SESSION_COOKIE));
    let authenticated = initialized
        && token
            .as_deref()
            .and_then(|t| state.auth.verify_admin_token(t).ok())
            .is_some_and(|c| c.ver == node.admin_token_version);
    Ok(Json(AdminAuthStatusResp {
        initialized,
        authenticated,
        node_id: authenticated.then_some(node.node_id),
        node_name: node.node_name,
        portal_url: (authenticated || !initialized).then_some(node.portal_url),
        role: authenticated.then_some(node.role),
        version: env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
pub struct AdminSetupReq {
    pub password: String,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub portal_url: Option<String>,
}

#[derive(Serialize)]
pub struct AdminLoginResp {
    pub ok: bool,
    pub token: String,
    pub expires_in: u64,
}

pub async fn auth_setup(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Json(req): Json<AdminSetupReq>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    if !state.auth.check_rate_limit("admin_setup:__global__", 15, Duration::from_secs(300)) {
        return Err(err_resp(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "初始化尝试过于频繁，请稍后再试"));
    }
    // Fast-fail before acquiring Argon2 permits if admin password is already initialized.
    let existing = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    if existing.admin_password_hash.is_some() {
        return Err(err_resp(StatusCode::CONFLICT, "conflict", "管理员密码已初始化"));
    }

    if req.password.len() < 8 || req.password.len() > 256 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "weak_password", "管理员密码长度需为 8-256 个字符"));
    }
    if req.node_name.as_deref().is_some_and(|s| s.chars().count() > 128) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "节点名称过长"));
    }
    let portal_url = match req.portal_url.as_deref() {
        Some(raw) => Some(validate_http_url(raw, true).map_err(|m| err_resp(StatusCode::BAD_REQUEST, "invalid_portal_url", m))?),
        None => None,
    };

    let _permit = state.auth.acquire_argon2_permit().await?;
    let pw = req.password.clone();
    let hash = tokio::task::spawn_blocking(move || AuthService::hash_password(&pw))
        .await
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "密码加密失败"))?
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "密码加密失败"))?;

    let node_name = req.node_name.clone();
    let ver = state
        .db
        .run(move |conn| db_admin::setup_admin_password(conn, &hash, node_name.as_deref(), portal_url.as_deref()))
        .await
        .map_err(db_err_resp)?;

    let token = state
        .auth
        .sign_admin_token(ver)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "签发管理员令牌失败"))?;
    let cookie = admin_session_cookie_header(&token, ADMIN_TOKEN_TTL_SECS, is_https_request(&req_headers));
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        headers.insert(header::SET_COOKIE, v);
    }
    Ok((
        StatusCode::CREATED,
        headers,
        Json(AdminLoginResp { ok: true, token, expires_in: ADMIN_TOKEN_TTL_SECS }),
    ))
}

#[derive(Deserialize)]
pub struct AdminLoginReq {
    pub password: String,
}

pub async fn auth_login(
    State(state): State<AppState>,
    req_headers: HeaderMap,
    Json(req): Json<AdminLoginReq>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    if !state.auth.check_rate_limit("admin_pw:__global__", 20, Duration::from_secs(300)) {
        return Err(err_resp(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "管理员登录尝试过于频繁，请稍后再试"));
    }
    let node = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    let Some(pw_hash) = node.admin_password_hash else {
        return Err(err_resp(StatusCode::CONFLICT, "admin_not_initialized", "请先初始化管理员密码"));
    };

    let _permit = state.auth.acquire_argon2_permit().await?;
    let pw = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || AuthService::verify_password(&pw, &pw_hash))
        .await
        .unwrap_or(false);
    if !ok {
        return Err(err_resp(StatusCode::UNAUTHORIZED, "invalid_credentials", "管理员密码错误"));
    }

    let token = state
        .auth
        .sign_admin_token(node.admin_token_version)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "签发管理员令牌失败"))?;
    let cookie = admin_session_cookie_header(&token, ADMIN_TOKEN_TTL_SECS, is_https_request(&req_headers));
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        headers.insert(header::SET_COOKIE, v);
    }
    Ok((
        StatusCode::OK,
        headers,
        Json(AdminLoginResp { ok: true, token, expires_in: ADMIN_TOKEN_TTL_SECS }),
    ))
}

pub async fn auth_logout(req_headers: HeaderMap) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&admin_session_cookie_header("", 0, is_https_request(&req_headers))) {
        headers.insert(header::SET_COOKIE, v);
    }
    (StatusCode::OK, headers, Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub current_password: String,
    pub new_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    _admin: AdminAuth,
    req_headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    if req.new_password.len() < 8 || req.new_password.len() > 256 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "weak_password", "新密码长度需为 8-256 个字符"));
    }
    let node = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    let Some(pw_hash) = node.admin_password_hash else {
        return Err(err_resp(StatusCode::CONFLICT, "admin_not_initialized", "管理员密码尚未初始化"));
    };

    let _permit = state.auth.acquire_argon2_permit().await?;
    let cur = req.current_password.clone();
    let ok = tokio::task::spawn_blocking(move || AuthService::verify_password(&cur, &pw_hash))
        .await
        .unwrap_or(false);
    if !ok {
        return Err(err_resp(StatusCode::UNAUTHORIZED, "invalid_credentials", "当前密码不正确"));
    }

    let new_pw = req.new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || AuthService::hash_password(&new_pw))
        .await
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "密码加密失败"))?
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "密码加密失败"))?;

    let new_ver = state
        .db
        .run(move |conn| db_admin::change_admin_password(conn, &new_hash))
        .await
        .map_err(db_err_resp)?;

    let token = state
        .auth
        .sign_admin_token(new_ver)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "签发管理员令牌失败"))?;
    let cookie = admin_session_cookie_header(&token, ADMIN_TOKEN_TTL_SECS, is_https_request(&req_headers));
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        headers.insert(header::SET_COOKIE, v);
    }
    Ok((
        StatusCode::OK,
        headers,
        Json(AdminLoginResp { ok: true, token, expires_in: ADMIN_TOKEN_TTL_SECS }),
    ))
}

// ---- Overview, Node Settings & Manual GC ----

#[derive(Serialize)]
pub struct ReplicaView {
    pub replica_id: String,
    pub name: String,
    pub portal_url: String,
    pub status: String,
    pub online: bool,
    pub applied_lsn: u64,
    pub lag_lsn: u64,
    pub blob_pending: u64,
    pub storage_bytes: u64,
    pub server_version: String,
    pub remote_addr: String,
    pub last_seen_at: u64,
    pub last_error: String,
    pub created_at: u64,
}

#[derive(Serialize)]
pub struct UpstreamView {
    pub primary_url: String,
    pub primary_id: String,
    pub primary_name: String,
    pub primary_portal_url: String,
    pub primary_epoch: String,
    pub bootstrapped: bool,
    pub applied_lsn: u64,
    pub primary_head_lsn: u64,
    pub lag_lsn: u64,
    pub blob_pending: u64,
    pub state: String,
    pub last_sync_at: u64,
    pub last_error: String,
    pub configured_at: u64,
}

#[derive(Serialize)]
pub struct NodeView {
    pub node_id: String,
    pub node_name: String,
    pub portal_url: String,
    pub role: NodeRole,
    pub repl_epoch: String,
    pub head_lsn: u64,
    pub min_retained_lsn: u64,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub storage_backend: String,
}

#[derive(Serialize)]
pub struct RuntimeSettingsView {
    pub allow_register: bool,
    pub invite_code_enabled: bool,
    pub invite_code: String,
    pub max_blob_mb: u64,
    pub tombstone_days: u64,
    pub binlog_retention_days: u64,
}

#[derive(Serialize)]
pub struct OverviewResp {
    pub node: NodeView,
    pub stats: OverviewStatsView,
    pub settings: RuntimeSettingsView,
    pub replicas: Vec<ReplicaView>,
    pub upstream: Option<UpstreamView>,
}

#[derive(Serialize)]
pub struct OverviewStatsView {
    pub user_count: u64,
    pub device_count: u64,
    pub active_device_count: u64,
    pub online_connections: usize,
    pub online_devices: usize,
    pub vault_count: u64,
    pub entity_count: u64,
    pub op_log_count: u64,
    pub entity_bytes: u64,
    pub op_log_bytes: u64,
    pub unique_blob_count: u64,
    pub unique_blob_bytes: u64,
    pub logical_blob_bytes: u64,
    pub binlog_count: u64,
    pub binlog_bytes: u64,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub total_storage_bytes: u64,
}

fn effective_register_settings(state: &AppState, node: &db::NodeConfig) -> RuntimeSettingsView {
    let allow_register = node.allow_register_override.unwrap_or(state.config.allow_register);
    let invite_code = match &node.invite_code_override {
        Some(v) => v.clone(),
        None => state.config.invite_code.clone().unwrap_or_default(),
    };
    RuntimeSettingsView {
        allow_register,
        invite_code_enabled: !invite_code.is_empty(),
        invite_code,
        max_blob_mb: state.config.max_blob_mb,
        tombstone_days: state.config.tombstone_days,
        binlog_retention_days: state.config.binlog_retention_days,
    }
}

async fn build_replication_views(
    state: &AppState,
    node: &db::NodeConfig,
    head_lsn: u64,
) -> Result<(Vec<ReplicaView>, Option<UpstreamView>), (StatusCode, Json<ErrorBody>)> {
    if node.role == NodeRole::Primary {
        let rows = state
            .db
            .run(|conn| db_repl::list_replicas(conn, false))
            .await
            .map_err(db_err_resp)?;
        let online_ids = state.repl_hub.online_replica_ids();
        let replicas = rows
            .into_iter()
            .map(|r| {
                let online = online_ids.contains(&r.replica_id);
                ReplicaView {
                    lag_lsn: head_lsn.saturating_sub(r.applied_lsn),
                    replica_id: r.replica_id,
                    name: r.name,
                    portal_url: r.portal_url,
                    status: r.status,
                    online,
                    applied_lsn: r.applied_lsn,
                    blob_pending: r.blob_pending,
                    storage_bytes: r.storage_bytes,
                    server_version: r.server_version,
                    remote_addr: r.remote_addr,
                    last_seen_at: r.last_seen_at,
                    last_error: r.last_error,
                    created_at: r.created_at,
                }
            })
            .collect();
        Ok((replicas, None))
    } else {
        let (up_opt, blob_pending) = state
            .db
            .run(|conn| Ok((db_repl::get_upstream(conn)?, db_repl::count_pending_blobs(conn)?)))
            .await
            .map_err(db_err_resp)?;
        let upstream = up_opt.map(|u| UpstreamView {
            lag_lsn: u.primary_head_lsn.saturating_sub(u.applied_lsn),
            primary_portal_url: if u.primary_portal_url.is_empty() {
                u.primary_url.clone()
            } else {
                u.primary_portal_url
            },
            primary_url: u.primary_url,
            primary_id: u.primary_id,
            primary_name: u.primary_name,
            primary_epoch: u.primary_epoch,
            bootstrapped: u.bootstrapped,
            applied_lsn: u.applied_lsn,
            primary_head_lsn: u.primary_head_lsn,
            blob_pending,
            state: u.state,
            last_sync_at: u.last_sync_at,
            last_error: u.last_error,
            configured_at: u.configured_at,
        });
        Ok((Vec::new(), upstream))
    }
}

pub async fn get_overview(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<OverviewResp>, (StatusCode, Json<ErrorBody>)> {
    let (node, db_stats) = state
        .db
        .run(|conn| Ok((db::get_node_config(conn)?, db_admin::get_overview_stats(conn)?)))
        .await
        .map_err(db_err_resp)?;
    let (online_connections, online_devices) = state.hub.connection_stats();
    let (db_bytes, wal_bytes, shm_bytes) = local_db_file_sizes(&state);
    let total_storage_bytes = db_bytes
        .saturating_add(wal_bytes)
        .saturating_add(shm_bytes)
        .saturating_add(db_stats.unique_blob_bytes);
    let (replicas, upstream) = build_replication_views(&state, &node, db_stats.head_lsn).await?;
    let settings = effective_register_settings(&state, &node);

    Ok(Json(OverviewResp {
        node: NodeView {
            node_id: node.node_id,
            node_name: node.node_name,
            portal_url: node.portal_url,
            role: node.role,
            repl_epoch: node.repl_epoch,
            head_lsn: db_stats.head_lsn,
            min_retained_lsn: node.min_retained_lsn,
            version: env!("CARGO_PKG_VERSION"),
            uptime_secs: db::unix_secs().saturating_sub(state.started_at),
            storage_backend: state.storage_backend.clone(),
        },
        stats: OverviewStatsView {
            user_count: db_stats.user_count,
            device_count: db_stats.device_count,
            active_device_count: db_stats.active_device_count,
            online_connections,
            online_devices,
            vault_count: db_stats.vault_count,
            entity_count: db_stats.entity_count,
            op_log_count: db_stats.op_log_count,
            entity_bytes: db_stats.entity_bytes,
            op_log_bytes: db_stats.op_log_bytes,
            unique_blob_count: db_stats.unique_blob_count,
            unique_blob_bytes: db_stats.unique_blob_bytes,
            logical_blob_bytes: db_stats.logical_blob_bytes,
            binlog_count: db_stats.binlog_count,
            binlog_bytes: db_stats.binlog_bytes,
            db_bytes,
            wal_bytes,
            shm_bytes,
            total_storage_bytes,
        },
        settings,
        replicas,
        upstream,
    }))
}

#[derive(Deserialize)]
pub struct UpdateNodeReq {
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub portal_url: Option<String>,
    #[serde(default)]
    pub allow_register: Option<bool>,
    #[serde(default)]
    pub invite_code: Option<String>,
}

pub async fn update_node(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(req): Json<UpdateNodeReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    if req.node_name.as_deref().is_some_and(|s| s.trim().is_empty() || s.chars().count() > 128) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "节点名称不能为空且不超过 128 个字符"));
    }
    let portal_url = match req.portal_url.as_deref() {
        Some(raw) => Some(validate_http_url(raw, true).map_err(|m| err_resp(StatusCode::BAD_REQUEST, "invalid_portal_url", m))?),
        None => None,
    };
    if req.invite_code.as_deref().is_some_and(|s| s.chars().count() > 128) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "邀请码过长"));
    }

    let node_name = req.node_name.clone();
    let invite_opt = req.invite_code.clone();
    let allow_reg = req.allow_register;
    let (updated, head_lsn) = state
        .db
        .run(move |conn| {
            let invite_arg = invite_opt.as_deref().map(|s| if s.trim().is_empty() { None } else { Some(s) });
            let cfg = db_admin::update_node_settings(conn, node_name.as_deref(), portal_url.as_deref(), allow_reg, invite_arg)?;
            let head = db::get_head_lsn(conn)?;
            Ok((cfg, head))
        })
        .await
        .map_err(db_err_resp)?;

    if updated.role == NodeRole::Primary {
        state
            .repl_hub
            .broadcast_primary_info(&updated.node_id, &updated.node_name, &updated.portal_url, head_lsn);
    } else {
        state.repl_supervisor.trigger_ack();
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "node_name": updated.node_name,
        "portal_url": updated.portal_url,
        "settings": effective_register_settings(&state, &updated),
    })))
}

pub async fn run_gc_now(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let node = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    let deleted = if node.role == NodeRole::Primary {
        gc::run_once_count(&state)
            .await
            .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "gc_failed", format!("GC 执行失败: {e}")))?
    } else {
        gc::run_backup_orphan_cleanup(&state)
            .await
            .map_err(|e| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "gc_failed", format!("孤儿块清理失败: {e}")))?
    };
    Ok(Json(serde_json::json!({
        "ok": true,
        "deleted_blobs": deleted,
        "ran_at": db::unix_secs(),
    })))
}

// ---- User, Device & Vault Management ----

#[derive(Deserialize)]
pub struct ListUsersQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default = "default_user_sort")]
    pub sort: String,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
}

fn default_user_sort() -> String {
    "storage_desc".into()
}

fn default_page_limit() -> u32 {
    50
}

#[derive(Serialize)]
pub struct ListUsersResp {
    pub total: u64,
    pub users: Vec<db_admin::AdminUserSummary>,
}

pub async fn list_users(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Query(q): Query<ListUsersQuery>,
) -> Result<Json<ListUsersResp>, (StatusCode, Json<ErrorBody>)> {
    let online_map = state.hub.online_device_ids_by_user();
    let (total, users) = state
        .db
        .run(move |conn| db_admin::list_users_admin(conn, &q.q, &q.sort, q.limit, q.offset, &online_map))
        .await
        .map_err(db_err_resp)?;
    Ok(Json(ListUsersResp { total, users }))
}

pub async fn get_user_detail(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Path(user_id): Path<String>,
) -> Result<Json<db_admin::AdminUserDetail>, (StatusCode, Json<ErrorBody>)> {
    let online = state.hub.online_device_ids_for_user(&user_id);
    let detail = state
        .db
        .run(move |conn| db_admin::get_user_detail_admin(conn, &user_id, &online))
        .await
        .map_err(db_err_resp)?;
    Ok(Json(detail))
}

pub async fn delete_user(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let uid = user_id.clone();
    let candidate_blobs = state
        .db
        .run(move |conn| db_admin::delete_user_admin(conn, &uid))
        .await
        .map_err(db_err_resp)?;

    state.hub.disconnect_user(&user_id);
    let deleted_blobs = gc::reclaim_candidate_blobs(&state, candidate_blobs).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "deleted_user_id": user_id,
        "reclaimed_blobs": deleted_blobs,
    })))
}

pub async fn revoke_user_device(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Path((user_id, device_id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let uid = user_id.clone();
    let did = device_id.clone();
    let removed = state
        .db
        .run(move |conn| db::revoke_device(conn, &uid, &did))
        .await
        .map_err(db_err_resp)?;
    if !removed {
        return Err(err_resp(StatusCode::NOT_FOUND, "not_found", "设备不存在"));
    }
    state.hub.disconnect_device(&user_id, &device_id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_vault_admin(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Path(vault_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let vid = vault_id.clone();
    let candidate_blobs = state
        .db
        .run(move |conn| db_admin::delete_vault_admin(conn, &vid))
        .await
        .map_err(db_err_resp)?;

    state.hub.disconnect_vault(&vault_id);
    let reclaimed_blobs = gc::reclaim_candidate_blobs(&state, candidate_blobs).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "deleted_vault_id": vault_id,
        "reclaimed_blobs": reclaimed_blobs,
    })))
}

// ---- Storage Breakdown ----

#[derive(Serialize)]
pub struct StorageBreakdownResp {
    pub storage_backend: String,
    pub data_dir: String,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub entity_bytes: u64,
    pub op_log_bytes: u64,
    pub binlog_count: u64,
    pub binlog_bytes: u64,
    pub unique_blob_count: u64,
    pub unique_blob_bytes: u64,
    pub logical_blob_bytes: u64,
    pub dedup_saved_bytes: u64,
    pub total_storage_bytes: u64,
    pub top_users: Vec<db_admin::AdminUserSummary>,
    pub top_vaults: Vec<db_admin::AdminVaultDetail>,
}

pub async fn get_storage(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<StorageBreakdownResp>, (StatusCode, Json<ErrorBody>)> {
    let online_map = state.hub.online_device_ids_by_user();
    let (stats, top_users, top_vaults) = state
        .db
        .run(move |conn| {
            let stats = db_admin::get_overview_stats(conn)?;
            let (_, top_users) = db_admin::list_users_admin(conn, "", "storage_desc", 20, 0, &online_map)?;
            let top_vaults = db_admin::top_vaults_by_storage(conn, 20)?;
            Ok((stats, top_users, top_vaults))
        })
        .await
        .map_err(db_err_resp)?;
    let (db_bytes, wal_bytes, shm_bytes) = local_db_file_sizes(&state);
    let total_storage_bytes = db_bytes
        .saturating_add(wal_bytes)
        .saturating_add(shm_bytes)
        .saturating_add(stats.unique_blob_bytes);
    let dedup_saved_bytes = stats.logical_blob_bytes.saturating_sub(stats.unique_blob_bytes);

    Ok(Json(StorageBreakdownResp {
        storage_backend: state.storage_backend.clone(),
        data_dir: state.config.data_dir.display().to_string(),
        db_bytes,
        wal_bytes,
        shm_bytes,
        entity_bytes: stats.entity_bytes,
        op_log_bytes: stats.op_log_bytes,
        binlog_count: stats.binlog_count,
        binlog_bytes: stats.binlog_bytes,
        unique_blob_count: stats.unique_blob_count,
        unique_blob_bytes: stats.unique_blob_bytes,
        logical_blob_bytes: stats.logical_blob_bytes,
        dedup_saved_bytes,
        total_storage_bytes,
        top_users,
        top_vaults,
    }))
}

// ---- Replication Management ----

#[derive(Serialize)]
pub struct ReplicationStatusResp {
    pub node: NodeView,
    pub replicas: Vec<ReplicaView>,
    pub upstream: Option<UpstreamView>,
    pub local_storage_bytes: u64,
}

pub async fn get_replication(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<ReplicationStatusResp>, (StatusCode, Json<ErrorBody>)> {
    let (node, head_lsn) = state
        .db
        .run(|conn| Ok((db::get_node_config(conn)?, db::get_head_lsn(conn)?)))
        .await
        .map_err(db_err_resp)?;
    let (replicas, upstream) = build_replication_views(&state, &node, head_lsn).await?;
    let local_storage_bytes = compute_node_storage_bytes(&state).await;

    Ok(Json(ReplicationStatusResp {
        node: NodeView {
            node_id: node.node_id,
            node_name: node.node_name,
            portal_url: node.portal_url,
            role: node.role,
            repl_epoch: node.repl_epoch,
            head_lsn,
            min_retained_lsn: node.min_retained_lsn,
            version: env!("CARGO_PKG_VERSION"),
            uptime_secs: db::unix_secs().saturating_sub(state.started_at),
            storage_backend: state.storage_backend.clone(),
        },
        replicas,
        upstream,
        local_storage_bytes,
    }))
}

#[derive(Deserialize)]
pub struct ConfigureBackupReq {
    pub primary_url: String,
    pub primary_admin_password: String,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub portal_url: Option<String>,
}

pub async fn configure_backup(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(req): Json<ConfigureBackupReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let primary_url = validate_http_url(&req.primary_url, false)
        .map_err(|m| err_resp(StatusCode::BAD_REQUEST, "invalid_primary_url", m))?;
    let portal_url_opt = match req.portal_url.as_deref() {
        Some(raw) => Some(validate_http_url(raw, true).map_err(|m| err_resp(StatusCode::BAD_REQUEST, "invalid_portal_url", m))?),
        None => None,
    };
    if req.node_name.as_deref().is_some_and(|s| s.trim().is_empty() || s.chars().count() > 128) {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "本机节点名称无效"));
    }

    let node = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    let replica_name = req
        .node_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&node.node_name)
        .to_string();
    let replica_portal = portal_url_opt.clone().unwrap_or_else(|| node.portal_url.clone());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "http_client", "初始化 HTTP 客户端失败"))?;

    let reg_url = format!("{primary_url}/v1/repl/register");
    let resp = client
        .post(&reg_url)
        .json(&ReplRegisterReq {
            admin_password: req.primary_admin_password,
            replica_id: node.node_id.clone(),
            replica_name: replica_name.clone(),
            portal_url: replica_portal.clone(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .send()
        .await
        .map_err(|e| err_resp(StatusCode::BAD_GATEWAY, "primary_unreachable", format!("无法连接主服务器: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.json::<ErrorBody>().await.ok();
        let (code, msg) = match err_body {
            Some(b) => (b.code, b.message),
            None => ("primary_rejected".into(), format!("主服务器拒绝配对 ({status})")),
        };
        let mapped_status = if status.is_client_error() { status } else { StatusCode::BAD_GATEWAY };
        return Err(err_resp(mapped_status, &code, msg));
    }

    let reg: ReplRegisterResp = resp
        .json()
        .await
        .map_err(|e| err_resp(StatusCode::BAD_GATEWAY, "invalid_primary_response", format!("解析主服务器响应失败: {e}")))?;

    let primary_portal = if reg.primary_portal_url.is_empty() {
        primary_url.clone()
    } else {
        reg.primary_portal_url.clone()
    };

    let purl = primary_url.clone();
    let rname_opt = req.node_name.clone();
    let rpurl_opt = portal_url_opt.clone();
    let reg_clone = reg.clone();
    let upstream = state
        .db
        .run(move |conn| {
            db_repl::configure_backup(
                conn,
                &purl,
                &reg_clone.primary_id,
                &reg_clone.primary_name,
                &primary_portal,
                &reg_clone.primary_epoch,
                &reg_clone.replica_token,
                reg_clone.head_lsn,
                rname_opt.as_deref(),
                rpurl_opt.as_deref(),
            )
        })
        .await
        .map_err(db_err_resp)?;

    // Drop any active client WS sessions and downstream replica WS sessions before entering read-only backup mode.
    state.db.set_binlog_head(0);
    state.hub.disconnect_all();
    state.repl_hub.disconnect_all();
    state.repl_supervisor.trigger_reload();

    Ok(Json(serde_json::json!({
        "ok": true,
        "role": "backup",
        "primary_id": upstream.primary_id,
        "primary_name": upstream.primary_name,
        "primary_portal_url": upstream.primary_portal_url,
    })))
}

pub async fn promote_to_primary(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let new_epoch = state
        .db
        .run(db_repl::promote_to_primary)
        .await
        .map_err(db_err_resp)?;
    state.db.set_binlog_head(0);
    state.repl_supervisor.trigger_reload();
    Ok(Json(serde_json::json!({
        "ok": true,
        "role": "primary",
        "repl_epoch": new_epoch,
    })))
}

pub async fn trigger_resync(
    State(state): State<AppState>,
    _admin: AdminAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let (node, upstream) = state
        .db
        .run(|conn| Ok((db::get_node_config(conn)?, db_repl::get_upstream(conn)?)))
        .await
        .map_err(db_err_resp)?;
    if node.role != NodeRole::Backup {
        return Err(err_resp(StatusCode::CONFLICT, "not_backup", "只有备份服务器才能触发全量重同步"));
    }
    if upstream.as_ref().is_some_and(|u| u.state == "evicted") {
        return Err(err_resp(StatusCode::CONFLICT, "replica_evicted", "当前备份服务器已被主服务器剔除，请重新配对"));
    }
    state.db.run(db_repl::mark_resync_needed).await.map_err(db_err_resp)?;
    state.repl_supervisor.trigger_reload();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn evict_replica(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Path(replica_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let rid = replica_id.clone();
    let evicted = state
        .db
        .run(move |conn| db_repl::evict_replica(conn, &rid))
        .await
        .map_err(db_err_resp)?;
    if !evicted {
        return Err(err_resp(StatusCode::NOT_FOUND, "not_found", "备份服务器不存在或已被剔除"));
    }
    state.repl_hub.evict_replica(&replica_id, "已被主服务器管理员剔除");
    Ok(Json(serde_json::json!({
        "ok": true,
        "evicted_replica_id": replica_id,
    })))
}

pub async fn purge_evicted_replica(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Path(replica_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let removed = state
        .db
        .run(move |conn| db_repl::purge_evicted_replica(conn, &replica_id))
        .await
        .map_err(db_err_resp)?;
    if !removed {
        return Err(err_resp(StatusCode::NOT_FOUND, "not_found", "未找到该已剔除记录"));
    }
    Ok(StatusCode::NO_CONTENT)
}
