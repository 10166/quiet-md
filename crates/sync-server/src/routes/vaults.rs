use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use quiet_sync_proto::api::{CreateVaultReq, ErrorBody, UpdateE2eeKeyReq, VaultSummary};
use uuid::Uuid;

use crate::auth::{db_err_resp, err_resp, AuthUser};
use crate::db;
use crate::{ensure_primary, AppState};

pub async fn list_vaults(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<VaultSummary>>, (StatusCode, Json<ErrorBody>)> {
    let uid = auth.user_id.clone();
    let list = state
        .db
        .run(move |conn| db::list_vaults(conn, &uid))
        .await
        .map_err(db_err_resp)?;
    Ok(Json(list))
}

pub async fn create_vault(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateVaultReq>,
) -> Result<(StatusCode, Json<VaultSummary>), (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let name = req.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 128 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_name", "文档库名称不能为空且不能超过 128 个字符"));
    }
    let id = Uuid::new_v4().to_string();
    let uid = auth.user_id.clone();
    let e2ee = req.e2ee.clone();
    let vault = state
        .db
        .run(move |conn| db::create_vault(conn, &id, &uid, &name, e2ee.as_ref()))
        .await
        .map_err(db_err_resp)?;
    Ok((StatusCode::CREATED, Json(vault)))
}

pub async fn delete_vault(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(vault_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let uid = auth.user_id.clone();
    let deleted = state
        .db
        .run(move |conn| db::delete_vault(conn, &uid, &vault_id))
        .await
        .map_err(db_err_resp)?;
    if !deleted {
        return Err(err_resp(StatusCode::NOT_FOUND, "not_found", "文档库不存在"));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update_e2ee_key(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(vault_id): Path<String>,
    Json(req): Json<UpdateE2eeKeyReq>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    let uid = auth.user_id.clone();
    let e2ee = req.e2ee;
    state
        .db
        .run(move |conn| db::update_vault_e2ee_key(conn, &uid, &vault_id, &e2ee))
        .await
        .map_err(db_err_resp)?;
    Ok(StatusCode::NO_CONTENT)
}
