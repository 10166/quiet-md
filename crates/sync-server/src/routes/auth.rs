use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use quiet_sync_proto::api::{
    AuthResp, DeviceInfo, ErrorBody, LoginReq, LogoutReq, RefreshReq, RegisterReq, StorageUsageResp,
    UserInfo, WsTicketResp,
};
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

use crate::auth::{
    ct_eq, db_err_resp, err_resp, hash_token, AuthService, AuthUser, ACCESS_TTL_SECS, REFRESH_TTL_SECS,
};
use crate::db;
use crate::{ensure_primary, AppState};

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Result<(StatusCode, Json<AuthResp>), (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let node = state.db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
    let allow_register = node.allow_register_override.unwrap_or(state.config.allow_register);
    if !allow_register {
        return Err(err_resp(StatusCode::FORBIDDEN, "register_disabled", "服务端未开放注册"));
    }
    let effective_invite: Option<String> = match node.invite_code_override {
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v),
        None => state.config.invite_code.clone(),
    };
    if let Some(expected) = effective_invite.as_deref()
        && !req.invite_code.as_deref().is_some_and(|got| ct_eq(got.as_bytes(), expected.as_bytes()))
    {
        return Err(err_resp(StatusCode::FORBIDDEN, "invalid_invite", "邀请码无效"));
    }
    let email = req.email.trim().to_lowercase();
    if !email.contains('@') || email.len() < 4 || email.len() > 254 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_email", "邮箱格式或长度不正确"));
    }
    if req.password.len() < 6 || req.password.len() > 256 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "weak_password", "密码长度需为 6-256 个字符"));
    }
    if req.display_name.chars().count() > 128 || req.device_name.chars().count() > 128 || req.platform.chars().count() > 32 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "名称或平台字段超出长度限制"));
    }
    if !state.auth.check_rate_limit("reg:__global__", 120, Duration::from_secs(600))
        || !state.auth.check_rate_limit(&format!("reg:{email}"), 10, Duration::from_secs(600))
    {
        return Err(err_resp(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "请求过于频繁，请稍后再试"));
    }

    let _permit = state.auth.acquire_argon2_permit().await?;
    let password = req.password.clone();
    let pw_hash = tokio::task::spawn_blocking(move || AuthService::hash_password(&password))
        .await
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "密码加密失败"))?
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "密码加密失败"))?;

    let user_id = Uuid::new_v4().to_string();
    let display_name = if req.display_name.trim().is_empty() {
        email.split('@').next().unwrap_or("User").to_string()
    } else {
        req.display_name.trim().to_string()
    };
    let (refresh_token, refresh_hash) = AuthService::generate_refresh_token();
    let refresh_exp = db::unix_secs() + REFRESH_TTL_SECS;
    let device_id = sanitize_device_id(&req.device_id);
    let device_name = req.device_name.trim().to_string();
    let platform = req.platform.trim().to_string();

    let (user_row, device_info) = state
        .db
        .run(move |conn| {
            let user = db::create_user(conn, &user_id, &email, &display_name, &pw_hash)?;
            let dev = db::upsert_device(conn, &device_id, &user.id, &device_name, &platform, &refresh_hash, refresh_exp)?;
            Ok((user, dev))
        })
        .await
        .map_err(db_err_resp)?;

    let access_token = state
        .auth
        .sign_access_token(&user_row.id, &device_info.id)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "签发令牌失败"))?;

    Ok((
        StatusCode::CREATED,
        Json(AuthResp {
            access_token,
            refresh_token,
            expires_in: ACCESS_TTL_SECS,
            user: UserInfo {
                id: user_row.id,
                email: user_row.email,
                display_name: user_row.display_name,
                created_at: user_row.created_at,
            },
            device: device_info,
        }),
    ))
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<AuthResp>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let email = req.email.trim().to_lowercase();
    if email.is_empty() || email.len() > 254 || req.password.len() > 256 || req.device_name.chars().count() > 128 || req.platform.chars().count() > 32 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_input", "登录参数无效或过长"));
    }
    if !state.auth.check_rate_limit(&format!("login:{email}"), 15, Duration::from_secs(300)) {
        return Err(err_resp(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "登录尝试过于频繁，请稍后再试"));
    }

    let email_q = email.clone();
    let user_row = state
        .db
        .run(move |conn| db::find_user_by_email(conn, &email_q))
        .await
        .map_err(db_err_resp)?
        .ok_or_else(|| err_resp(StatusCode::UNAUTHORIZED, "invalid_credentials", "邮箱或密码错误"))?;

    let _permit = state.auth.acquire_argon2_permit().await?;
    let password = req.password.clone();
    let phc = user_row.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || AuthService::verify_password(&password, &phc))
        .await
        .unwrap_or(false);
    if !ok {
        return Err(err_resp(StatusCode::UNAUTHORIZED, "invalid_credentials", "邮箱或密码错误"));
    }

    let (refresh_token, refresh_hash) = AuthService::generate_refresh_token();
    let refresh_exp = db::unix_secs() + REFRESH_TTL_SECS;
    let uid = user_row.id.clone();
    let device_id = sanitize_device_id(&req.device_id);
    let device_name = req.device_name.trim().to_string();
    let platform = req.platform.trim().to_string();

    let device_info = state
        .db
        .run(move |conn| db::upsert_device(conn, &device_id, &uid, &device_name, &platform, &refresh_hash, refresh_exp))
        .await
        .map_err(db_err_resp)?;

    let access_token = state
        .auth
        .sign_access_token(&user_row.id, &device_info.id)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "签发令牌失败"))?;

    Ok(Json(AuthResp {
        access_token,
        refresh_token,
        expires_in: ACCESS_TTL_SECS,
        user: UserInfo {
            id: user_row.id,
            email: user_row.email,
            display_name: user_row.display_name,
            created_at: user_row.created_at,
        },
        device: device_info,
    }))
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshReq>,
) -> Result<Json<AuthResp>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let device_id = req.device_id.clone();
    let provided_hash = hash_token(&req.refresh_token);
    let (new_refresh, new_hash) = AuthService::generate_refresh_token();
    let new_exp = db::unix_secs() + REFRESH_TTL_SECS;

    let (user_row, device_info) = state
        .db
        .run(move |conn| {
            let Some(dev) = db::get_device(conn, &device_id)? else {
                return Err(db::DbError::Forbidden("设备未注册"));
            };
            if dev.revoked
                || dev.refresh_hash.is_empty()
                || !ct_eq(dev.refresh_hash.as_bytes(), provided_hash.as_bytes())
                || db::unix_secs() > dev.refresh_expires_at
            {
                return Err(db::DbError::Forbidden("登录凭据已失效，请重新登录"));
            }
            let Some(user) = db::find_user_by_id(conn, &dev.user_id)? else {
                return Err(db::DbError::NotFound("账号不存在"));
            };
            db::rotate_refresh(conn, &dev.id, &new_hash, new_exp)?;
            let info = DeviceInfo {
                id: dev.id,
                name: dev.name,
                platform: dev.platform,
                last_seen_at: db::unix_secs(),
                online: false,
            };
            Ok((user, info))
        })
        .await
        .map_err(|e| match e {
            db::DbError::Forbidden(m) => err_resp(StatusCode::UNAUTHORIZED, "refresh_invalid", m),
            other => db_err_resp(other),
        })?;

    let access_token = state
        .auth
        .sign_access_token(&user_row.id, &device_info.id)
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "签发令牌失败"))?;

    Ok(Json(AuthResp {
        access_token,
        refresh_token: new_refresh,
        expires_in: ACCESS_TTL_SECS,
        user: UserInfo {
            id: user_row.id,
            email: user_row.email,
            display_name: user_row.display_name,
            created_at: user_row.created_at,
        },
        device: device_info,
    }))
}

pub async fn logout(
    State(state): State<AppState>,
    Json(req): Json<LogoutReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let h = hash_token(&req.refresh_token);
    if let Some((uid, did)) = state
        .db
        .run(move |conn| db::revoke_by_refresh_hash(conn, &h))
        .await
        .map_err(db_err_resp)?
    {
        state.hub.disconnect_device(&uid, &did);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<UserInfo>, (StatusCode, Json<ErrorBody>)> {
    let uid = auth.user_id.clone();
    let user = state
        .db
        .run(move |conn| db::find_user_by_id(conn, &uid))
        .await
        .map_err(db_err_resp)?
        .ok_or_else(|| err_resp(StatusCode::NOT_FOUND, "not_found", "账号不存在"))?;
    Ok(Json(UserInfo {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        created_at: user.created_at,
    }))
}

pub async fn storage_usage(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<StorageUsageResp>, (StatusCode, Json<ErrorBody>)> {
    let uid = auth.user_id.clone();
    let usage = state
        .db
        .run(move |conn| db::get_user_storage_usage(conn, &uid))
        .await
        .map_err(db_err_resp)?;
    let max_blob_bytes = state.config.max_blob_mb.saturating_mul(1024 * 1024);
    let quota_bytes = state.config.max_user_mb.map(|mb| mb.saturating_mul(1024 * 1024));
    Ok(Json(StorageUsageResp {
        used_bytes: usage.used_bytes(),
        entity_bytes: usage.entity_bytes,
        blob_bytes: usage.blob_bytes,
        entity_count: usage.entity_count,
        blob_count: usage.blob_count,
        max_blob_bytes,
        quota_bytes,
    }))
}

pub async fn list_devices(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<DeviceInfo>>, (StatusCode, Json<ErrorBody>)> {
    let uid = auth.user_id.clone();
    let mut list = state
        .db
        .run(move |conn| db::list_devices(conn, &uid))
        .await
        .map_err(db_err_resp)?;
    let online = state.hub.online_device_ids_for_user(&auth.user_id);
    for d in &mut list {
        d.online = online.contains(&d.id);
    }
    Ok(Json(list))
}

pub async fn revoke_device(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(device_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let uid = auth.user_id.clone();
    let did = device_id.clone();
    let removed = state
        .db
        .run(move |conn| db::revoke_device(conn, &uid, &did))
        .await
        .map_err(db_err_resp)?;
    if !removed {
        return Err(err_resp(StatusCode::NOT_FOUND, "not_found", "设备不存在"));
    }
    state.hub.disconnect_device(&auth.user_id, &device_id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct WsTicketReq {
    pub vault_id: String,
}

pub async fn issue_ws_ticket(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<WsTicketReq>,
) -> Result<Json<WsTicketResp>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let uid = auth.user_id.clone();
    let vid = req.vault_id.clone();
    state
        .db
        .run(move |conn| db::verify_vault_owner(conn, &uid, &vid))
        .await
        .map_err(db_err_resp)?;
    let ticket = state.auth.issue_ws_ticket(&auth.user_id, &auth.device_id, &req.vault_id, auth.exp);
    Ok(Json(WsTicketResp { ticket, expires_in: crate::auth::WS_TICKET_TTL_SECS }))
}

fn sanitize_device_id(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        Uuid::new_v4().to_string()
    } else {
        trimmed.to_string()
    }
}
