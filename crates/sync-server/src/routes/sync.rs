use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use quiet_sync_proto::api::{ChangesResp, ErrorBody, PushReq, PushResp, SnapshotResp};
use serde::Deserialize;

use crate::auth::{db_err_resp, err_resp, AuthUser};
use crate::db;
use crate::{ensure_primary, AppState};

#[derive(Deserialize)]
pub struct ChangesQuery {
    #[serde(default)]
    pub since: u64,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    200
}

pub async fn get_changes(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(vault_id): Path<String>,
    Query(q): Query<ChangesQuery>,
) -> Result<Json<ChangesResp>, (StatusCode, Json<ErrorBody>)> {
    let uid = auth.user_id.clone();
    let (head_seq, changes, has_more, resync_required) = state
        .db
        .run(move |conn| db::get_changes(conn, &vault_id, &uid, q.since, q.limit))
        .await
        .map_err(db_err_resp)?;
    Ok(Json(ChangesResp { head_seq, changes, has_more, resync_required }))
}

pub async fn get_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(vault_id): Path<String>,
) -> Result<Json<SnapshotResp>, (StatusCode, Json<ErrorBody>)> {
    let uid = auth.user_id.clone();
    let (head_seq, items) = state
        .db
        .run(move |conn| db::get_snapshot(conn, &vault_id, &uid))
        .await
        .map_err(db_err_resp)?;
    Ok(Json(SnapshotResp { head_seq, items }))
}

pub async fn push(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(vault_id): Path<String>,
    Json(req): Json<PushReq>,
) -> Result<Json<PushResp>, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    if req.ops.len() > 200 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "too_many_ops", "单次推送操作不能超过 200 项"));
    }
    let uid = auth.user_id.clone();
    let did = auth.device_id.clone();
    let vid = vault_id.clone();
    let ops = req.ops;
    let (head_seq, results, broadcasts) = state
        .db
        .run(move |conn| {
            let _ = db::touch_device(conn, &did);
            db::push_ops(conn, &vid, &uid, &did, ops)
        })
        .await
        .map_err(db_err_resp)?;
    state.hub.broadcast_changes(&vault_id, &auth.device_id, &broadcasts);
    Ok(Json(PushResp { head_seq, results }))
}
