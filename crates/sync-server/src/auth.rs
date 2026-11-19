use anyhow::{Context, Result};
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use axum::Json;
use base64::Engine;
use hmac::{Hmac, Mac};
use quiet_sync_proto::api::ErrorBody;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::db::{self, Db, DbError, NodeRole};
use crate::db_repl;

pub const ACCESS_TTL_SECS: u64 = 15 * 60;
pub const REFRESH_TTL_SECS: u64 = 30 * 24 * 3600;
pub const WS_TICKET_TTL_SECS: u64 = 30;
pub const ADMIN_TOKEN_TTL_SECS: u64 = 12 * 3600;
pub const ADMIN_SESSION_COOKIE: &str = "quiet_admin_session";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub did: String,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdminClaims {
    pub typ: String,
    pub ver: u64,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Clone)]
pub struct AuthService {
    jwt_secret: Arc<Vec<u8>>,
    ws_tickets: Arc<Mutex<HashMap<String, WsTicket>>>,
    rate_limits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    argon2_permits: Arc<Semaphore>,
}

#[derive(Clone, Debug)]
pub struct WsTicket {
    pub user_id: String,
    pub device_id: String,
    pub vault_id: String,
    pub expires_at: u64,
    pub session_exp: u64,
}

impl AuthService {
    pub fn new(jwt_secret: Vec<u8>) -> Self {
        Self {
            jwt_secret: Arc::new(jwt_secret),
            ws_tickets: Arc::new(Mutex::new(HashMap::new())),
            rate_limits: Arc::new(Mutex::new(HashMap::new())),
            argon2_permits: Arc::new(Semaphore::new(4)),
        }
    }

    pub async fn acquire_argon2_permit(&self) -> std::result::Result<OwnedSemaphorePermit, (StatusCode, Json<ErrorBody>)> {
        tokio::time::timeout(Duration::from_secs(2), self.argon2_permits.clone().acquire_owned())
            .await
            .map_err(|_| err_resp(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "认证服务繁忙，请稍后再试"))?
            .map_err(|_| err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "认证服务不可用"))
    }

    pub fn hash_password(password: &str) -> Result<String> {
        let mut salt_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt_bytes);
        let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("salt encode: {e}"))?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("password hash: {e}"))?;
        Ok(hash.to_string())
    }

    pub fn verify_password(password: &str, phc: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(phc) else { return false };
        Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
    }

    pub fn sign_access_token(&self, user_id: &str, device_id: &str) -> Result<String> {
        let now = db::unix_secs();
        let claims = Claims {
            sub: user_id.to_string(),
            did: device_id.to_string(),
            iat: now,
            exp: now + ACCESS_TTL_SECS,
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header_b64 = b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload_b64 = b64.encode(serde_json::to_vec(&claims)?);
        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.jwt_secret).context("hmac key")?;
        mac.update(signing_input.as_bytes());
        let sig_b64 = b64.encode(mac.finalize().into_bytes());
        Ok(format!("{signing_input}.{sig_b64}"))
    }

    pub fn verify_access_token(&self, token: &str) -> Result<Claims> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!("invalid jwt format");
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let sig = b64.decode(s).context("invalid jwt signature b64")?;
        let signing_input = format!("{h}.{p}");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.jwt_secret).context("hmac key")?;
        mac.update(signing_input.as_bytes());
        let expected = mac.finalize().into_bytes();
        anyhow::ensure!(ct_eq(&sig, &expected), "invalid jwt signature");

        let header_bytes = b64.decode(h).context("invalid jwt header")?;
        let header_val: serde_json::Value = serde_json::from_slice(&header_bytes)?;
        anyhow::ensure!(header_val.get("alg").and_then(|v| v.as_str()) == Some("HS256"), "unsupported jwt alg");

        let payload_bytes = b64.decode(p).context("invalid jwt payload")?;
        let claims: Claims = serde_json::from_slice(&payload_bytes)?;
        anyhow::ensure!(db::unix_secs() <= claims.exp, "access token expired");
        Ok(claims)
    }

    pub fn generate_refresh_token() -> (String, String) {
        let mut raw = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut raw);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let hash = hash_token(&token);
        (token, hash)
    }

    pub fn generate_replica_token() -> (String, String) {
        let mut raw = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut raw);
        let token = format!("qmr_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw));
        let hash = hash_token(&token);
        (token, hash)
    }

    pub fn sign_admin_token(&self, token_version: u64) -> Result<String> {
        let now = db::unix_secs();
        let claims = AdminClaims {
            typ: "admin".to_string(),
            ver: token_version,
            iat: now,
            exp: now + ADMIN_TOKEN_TTL_SECS,
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header_b64 = b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload_b64 = b64.encode(serde_json::to_vec(&claims)?);
        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.jwt_secret).context("hmac key")?;
        mac.update(signing_input.as_bytes());
        let sig_b64 = b64.encode(mac.finalize().into_bytes());
        Ok(format!("{signing_input}.{sig_b64}"))
    }

    pub fn verify_admin_token(&self, token: &str) -> Result<AdminClaims> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!("invalid admin jwt format");
        };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let sig = b64.decode(s).context("invalid admin jwt signature b64")?;
        let signing_input = format!("{h}.{p}");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.jwt_secret).context("hmac key")?;
        mac.update(signing_input.as_bytes());
        let expected = mac.finalize().into_bytes();
        anyhow::ensure!(ct_eq(&sig, &expected), "invalid admin jwt signature");

        let header_bytes = b64.decode(h).context("invalid admin jwt header")?;
        let header_val: serde_json::Value = serde_json::from_slice(&header_bytes)?;
        anyhow::ensure!(header_val.get("alg").and_then(|v| v.as_str()) == Some("HS256"), "unsupported jwt alg");

        let payload_bytes = b64.decode(p).context("invalid admin jwt payload")?;
        let claims: AdminClaims = serde_json::from_slice(&payload_bytes)?;
        anyhow::ensure!(claims.typ == "admin", "not an admin token");
        anyhow::ensure!(db::unix_secs() <= claims.exp, "admin token expired");
        Ok(claims)
    }

    pub fn issue_ws_ticket(&self, user_id: &str, device_id: &str, vault_id: &str, session_exp: u64) -> String {
        let mut raw = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut raw);
        let ticket = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        let now = db::unix_secs();
        let expires_at = now + WS_TICKET_TTL_SECS;
        let mut guard = self.ws_tickets.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, v| v.expires_at >= now);
        guard.insert(
            ticket.clone(),
            WsTicket { user_id: user_id.into(), device_id: device_id.into(), vault_id: vault_id.into(), expires_at, session_exp },
        );
        ticket
    }

    pub fn consume_ws_ticket(&self, ticket: &str, vault_id: &str) -> Option<WsTicket> {
        let mut guard = self.ws_tickets.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.remove(ticket)?;
        if entry.expires_at >= db::unix_secs() && entry.vault_id == vault_id {
            Some(entry)
        } else {
            None
        }
    }

    pub fn prune_ws_tickets(&self) {
        let now = db::unix_secs();
        let mut guard = self.ws_tickets.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, v| v.expires_at >= now);
    }

    pub fn check_rate_limit(&self, key: &str, max_attempts: usize, window: Duration) -> bool {
        const MAX_KEYS: usize = 10_000;
        let now = Instant::now();
        let mut guard = self.rate_limits.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() > 2_048 {
            let gc_window = Duration::from_secs(900);
            guard.retain(|_, hits| {
                hits.retain(|t| now.duration_since(*t) < gc_window);
                !hits.is_empty()
            });
        }
        if !guard.contains_key(key) && guard.len() >= MAX_KEYS
            && let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, hits)| hits.last().copied().unwrap_or(now))
                .map(|(k, _)| k.clone())
        {
            guard.remove(&oldest_key);
        }
        let hits = guard.entry(key.to_string()).or_default();
        hits.retain(|t| now.duration_since(*t) < window);
        if hits.len() >= max_attempts {
            return false;
        }
        hits.push(now);
        true
    }
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn extract_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for val in headers.get_all(header::COOKIE) {
        let Ok(s) = val.to_str() else { continue };
        for part in s.split(';') {
            let trimmed = part.trim();
            if let Some((k, v)) = trimmed.split_once('=')
                && k.trim() == name
            {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

pub fn is_https_request(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

pub fn admin_session_cookie_header(token: &str, max_age: u64, secure: bool) -> String {
    if secure {
        format!("{ADMIN_SESSION_COOKIE}={token}; Path=/api/admin; HttpOnly; SameSite=Strict; Secure; Max-Age={max_age}")
    } else {
        format!("{ADMIN_SESSION_COOKIE}={token}; Path=/api/admin; HttpOnly; SameSite=Strict; Max-Age={max_age}")
    }
}

pub fn validate_http_url(raw: &str, allow_empty: bool) -> Result<String, &'static str> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return if allow_empty { Ok(String::new()) } else { Err("服务器地址不能为空") };
    }
    if trimmed.len() > 512 || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("服务器地址格式无效");
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| "服务器地址格式无效")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("服务器地址必须以 http:// 或 https:// 开头");
    }
    if parsed.host_str().is_none() {
        return Err("服务器地址缺少主机名");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("服务器地址不能包含账号凭据");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("服务器地址不能包含查询参数或锚点");
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

#[derive(Clone, Debug)]
pub struct AdminAuth {
    pub token_version: u64,
}

impl<S> FromRequestParts<S> for AdminAuth
where
    S: Send + Sync,
    AuthService: FromRef<S>,
    Db: FromRef<S>,
{
    type Rejection = (StatusCode, Json<ErrorBody>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let (token, from_cookie) = if let Some(bearer) = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        {
            (bearer.to_string(), false)
        } else if let Some(cookie_tok) = extract_cookie(&parts.headers, ADMIN_SESSION_COOKIE) {
            (cookie_tok, true)
        } else {
            return Err(err_resp(StatusCode::UNAUTHORIZED, "admin_unauthorized", "请先登录管理员后台"));
        };

        if from_cookie && !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
            let csrf_ok = parts
                .headers
                .get("x-quiet-portal")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == "1");
            if !csrf_ok {
                return Err(err_resp(StatusCode::FORBIDDEN, "csrf_required", "缺少管理后台安全校验头"));
            }
        }

        let auth = AuthService::from_ref(state);
        let claims = auth
            .verify_admin_token(&token)
            .map_err(|_| err_resp(StatusCode::UNAUTHORIZED, "admin_token_invalid", "管理员会话已过期，请重新登录"))?;
        let db = Db::from_ref(state);
        let node = db.run(|conn| db::get_node_config(conn)).await.map_err(db_err_resp)?;
        if node.admin_password_hash.is_none() || node.admin_token_version != claims.ver {
            return Err(err_resp(StatusCode::UNAUTHORIZED, "admin_token_revoked", "管理员会话已失效，请重新登录"));
        }
        Ok(AdminAuth { token_version: claims.ver })
    }
}

#[derive(Clone, Debug)]
pub struct ReplicaAuth {
    pub replica_id: String,
    pub name: String,
    pub portal_url: String,
}

impl<S> FromRequestParts<S> for ReplicaAuth
where
    S: Send + Sync,
    Db: FromRef<S>,
{
    type Rejection = (StatusCode, Json<ErrorBody>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let replica_id = parts
            .headers
            .get("x-replica-id")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(|| err_resp(StatusCode::UNAUTHORIZED, "missing_replica_id", "缺少备份节点标识"))?
            .to_string();
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| err_resp(StatusCode::UNAUTHORIZED, "unauthorized", "缺少复制鉴权令牌"))?;
        let provided_hash = hash_token(token);

        let db = Db::from_ref(state);
        let rid = replica_id.clone();
        let (role, row) = db
            .run(move |conn| {
                let node = db::get_node_config(conn)?;
                let row = db_repl::get_replica(conn, &rid)?;
                Ok((node.role, row))
            })
            .await
            .map_err(db_err_resp)?;

        if role != NodeRole::Primary {
            return Err(err_resp(StatusCode::CONFLICT, "not_primary", "当前节点处于备份模式，不能作为上游主服务器"));
        }
        let Some(row) = row else {
            return Err(err_resp(StatusCode::UNAUTHORIZED, "unknown_replica", "备份节点未注册"));
        };
        if !ct_eq(provided_hash.as_bytes(), row.token_hash.as_bytes()) {
            return Err(err_resp(StatusCode::UNAUTHORIZED, "invalid_replica_token", "备份节点令牌无效"));
        }
        if row.status == "evicted" {
            return Err(err_resp(StatusCode::FORBIDDEN, "replica_evicted", "该备份服务器已被主服务器剔除"));
        }
        Ok(ReplicaAuth {
            replica_id: row.replica_id,
            name: row.name,
            portal_url: row.portal_url,
        })
    }
}

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub user_id: String,
    pub device_id: String,
    pub exp: u64,
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    AuthService: FromRef<S>,
    Db: FromRef<S>,
{
    type Rejection = (StatusCode, Json<ErrorBody>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth_hdr = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| err_resp(StatusCode::UNAUTHORIZED, "unauthorized", "缺少访问令牌"))?;
        let auth = AuthService::from_ref(state);
        let claims = auth
            .verify_access_token(auth_hdr)
            .map_err(|_| err_resp(StatusCode::UNAUTHORIZED, "token_invalid", "访问令牌无效或已过期"))?;
        let db = Db::from_ref(state);
        let uid = claims.sub.clone();
        let did = claims.did.clone();
        let active = db
            .run(move |conn| db::is_device_active(conn, &uid, &did))
            .await
            .map_err(db_err_resp)?;
        if !active {
            return Err(err_resp(StatusCode::UNAUTHORIZED, "device_revoked", "当前设备已被移除或已注销"));
        }
        Ok(AuthUser { user_id: claims.sub, device_id: claims.did, exp: claims.exp })
    }
}

pub fn err_resp(status: StatusCode, code: &str, message: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (status, Json(ErrorBody { code: code.into(), message: message.into() }))
}

pub fn db_err_resp(err: DbError) -> (StatusCode, Json<ErrorBody>) {
    match err {
        DbError::BadRequest(m) => err_resp(StatusCode::BAD_REQUEST, "bad_request", m),
        DbError::NotFound(m) => err_resp(StatusCode::NOT_FOUND, "not_found", m),
        DbError::Forbidden(m) => err_resp(StatusCode::FORBIDDEN, "forbidden", m),
        DbError::Conflict(m) => err_resp(StatusCode::CONFLICT, "conflict", m),
        DbError::ReadOnly(m) => err_resp(StatusCode::SERVICE_UNAVAILABLE, "read_only_replica", m),
        DbError::Internal(e) => {
            tracing::error!("database error: {e:#}");
            err_resp(StatusCode::INTERNAL_SERVER_ERROR, "internal", "服务器内部错误")
        }
    }
}
