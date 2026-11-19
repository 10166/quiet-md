use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use quiet_sync_proto::api::{BlobsExistsReq, BlobsExistsResp, ErrorBody};

use crate::auth::{db_err_resp, err_resp, AuthUser};
use crate::blob::{sha256_hex, validate_hash};
use crate::db;
use crate::{ensure_primary, AppState};

pub async fn blobs_exists(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(vault_id): Path<String>,
    Json(req): Json<BlobsExistsReq>,
) -> Result<Json<BlobsExistsResp>, (StatusCode, Json<ErrorBody>)> {
    if req.hashes.len() > 1000 {
        return Err(err_resp(StatusCode::BAD_REQUEST, "too_many_hashes", "单次查询哈希数量不能超过 1000"));
    }
    for h in &req.hashes {
        if validate_hash(h).is_err() {
            return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_hash", "无效的 Blob 哈希"));
        }
    }
    let uid = auth.user_id.clone();
    let hashes = req.hashes;
    let existing = state
        .db
        .run(move |conn| db::filter_existing_blobs(conn, &vault_id, &uid, &hashes))
        .await
        .map_err(db_err_resp)?;
    Ok(Json(BlobsExistsResp { existing }))
}

pub async fn put_blob(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((vault_id, hash)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    ensure_primary(&state).await?;
    if validate_hash(&hash).is_err() {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_hash", "无效的 Blob 哈希"));
    }
    let max_bytes = state.config.max_blob_mb * 1024 * 1024;
    if body.len() as u64 > max_bytes {
        return Err(err_resp(StatusCode::PAYLOAD_TOO_LARGE, "blob_too_large", "附件超出服务端大小限制"));
    }
    if sha256_hex(&body) != hash {
        return Err(err_resp(StatusCode::BAD_REQUEST, "hash_mismatch", "附件校验和与声明哈希不匹配"));
    }
    let uid = auth.user_id.clone();
    let vid = vault_id.clone();
    let h_lease = hash.clone();
    let size = body.len() as u64;
    let quota_bytes = state.config.max_user_mb.map(|mb| mb.saturating_mul(1024 * 1024));
    let _gc_read = state.blob_gc_lock.read().await;
    let over_quota = state
        .db
        .run(move |conn| {
            db::verify_vault_owner(conn, &uid, &vid)?;
            if let Some(quota) = quota_bytes {
                let already = db::vault_has_blob(conn, &vid, &uid, &h_lease)?;
                if !already {
                    let usage = db::get_user_storage_usage(conn, &uid)?;
                    if usage.used_bytes().saturating_add(size) > quota {
                        return Ok(true);
                    }
                }
            }
            db::acquire_blob_lease(conn, &h_lease, db::unix_secs() + 600)?;
            Ok(false)
        })
        .await
        .map_err(db_err_resp)?;
    if over_quota {
        return Err(err_resp(StatusCode::PAYLOAD_TOO_LARGE, "quota_exceeded", "云端存储空间已超出配额限制"));
    }

    state
        .blobs
        .put(&hash, body.to_vec())
        .await
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "blob_store", "写入附件失败"))?;

    let uid = auth.user_id.clone();
    state
        .db
        .run(move |conn| db::register_vault_blob(conn, &vault_id, &uid, &hash, size))
        .await
        .map_err(db_err_resp)?;

    Ok(StatusCode::CREATED)
}

pub async fn get_blob(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((vault_id, hash)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    if validate_hash(&hash).is_err() {
        return Err(err_resp(StatusCode::BAD_REQUEST, "invalid_hash", "无效的 Blob 哈希"));
    }
    let uid = auth.user_id.clone();
    let h = hash.clone();
    let has_ref = state
        .db
        .run(move |conn| db::vault_has_blob(conn, &vault_id, &uid, &h))
        .await
        .map_err(db_err_resp)?;
    if !has_ref {
        return Err(err_resp(StatusCode::NOT_FOUND, "not_found", "附件不存在"));
    }
    let bytes = state
        .blobs
        .get(&hash)
        .await
        .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "blob_store", "读取附件失败"))?
        .ok_or_else(|| err_resp(StatusCode::NOT_FOUND, "not_found", "附件数据缺失"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}
