use anyhow::Result;
use quiet_sync_proto::api::*;
use reqwest::StatusCode;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::credentials::{SecretStore, StoredCredentials};

#[derive(Debug)]
pub enum ApiError {
    AuthExpired,
    Network(String),
    Server { status: StatusCode, code: String, message: String },
    Other(anyhow::Error),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthExpired => write!(f, "登录状态已失效，请重新登录"),
            Self::Network(msg) => write!(f, "网络连接失败：{msg}"),
            Self::Server { message, .. } => write!(f, "{message}"),
            Self::Other(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Other(err) => Some(err.root_cause()),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

struct SessionState {
    generation: u64,
    creds: Option<StoredCredentials>,
}

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    session: Arc<RwLock<SessionState>>,
    creds_store: Arc<dyn SecretStore>,
    refresh_lock: Arc<Mutex<()>>,
}

pub fn validate_server_url(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() {
            return Err(ApiError::Server {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_url".into(),
                message: "服务器地址无效".into(),
            });
        }
        return Ok(trimmed.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        let host = if let Some(h) = host_port.strip_prefix('[') {
            h.split(']').next().unwrap_or("")
        } else {
            host_port.split(':').next().unwrap_or("")
        };
        if matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1") {
            return Ok(trimmed.to_string());
        }
        return Err(ApiError::Server {
            status: StatusCode::BAD_REQUEST,
            code: "insecure_url".into(),
            message: "远程服务器请使用 HTTPS 加密连接（仅本地回环地址允许 HTTP）".into(),
        });
    }
    Err(ApiError::Server {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_url".into(),
        message: "服务器地址必须以 https:// 开头（本地调试可使用 http://127.0.0.1）".into(),
    })
}

fn same_session_identity(a: &StoredCredentials, b: &StoredCredentials) -> bool {
    a.server_url == b.server_url
        && a.user_id == b.user_id
        && a.device_id == b.device_id
        && a.vault_id == b.vault_id
}

impl ApiClient {
    pub fn new(creds: Option<StoredCredentials>, creds_store: Arc<dyn SecretStore>) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            session: Arc::new(RwLock::new(SessionState { generation: 1, creds })),
            creds_store,
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn credentials(&self) -> Option<StoredCredentials> {
        self.session.read().await.creds.clone()
    }

    pub async fn set_credentials(&self, c: Option<StoredCredentials>) -> Result<()> {
        match &c {
            Some(v) => self.creds_store.save(v)?,
            None => self.creds_store.clear()?,
        }
        let mut s = self.session.write().await;
        let same = match (&s.creds, &c) {
            (Some(old), Some(new)) => same_session_identity(old, new),
            (None, None) => true,
            _ => false,
        };
        if !same {
            s.generation = s.generation.wrapping_add(1);
        }
        s.creds = c;
        Ok(())
    }

    pub async fn healthz(&self, server_url: &str) -> Result<(), ApiError> {
        let base = validate_server_url(server_url)?;
        let url = format!("{base}/healthz");
        let resp = self.http.get(&url).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        Ok(())
    }

    pub async fn register(&self, server_url: &str, req: &RegisterReq) -> Result<AuthResp, ApiError> {
        let base = validate_server_url(server_url)?;
        let url = format!("{base}/v1/auth/register");
        let resp = self.http.post(&url).json(req).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        resp.json().await.map_err(|e| ApiError::Network(e.to_string()))
    }

    pub async fn login(&self, server_url: &str, req: &LoginReq) -> Result<AuthResp, ApiError> {
        let base = validate_server_url(server_url)?;
        let url = format!("{base}/v1/auth/login");
        let resp = self.http.post(&url).json(req).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        resp.json().await.map_err(|e| ApiError::Network(e.to_string()))
    }

    pub async fn logout(&self) -> Result<(), ApiError> {
        let Some(c) = self.credentials().await else { return Ok(()) };
        if let Ok(base) = validate_server_url(&c.server_url) {
            let url = format!("{base}/v1/auth/logout");
            let _ = self.http.post(&url).json(&LogoutReq { refresh_token: c.refresh_token }).send().await;
        }
        Ok(())
    }

    async fn session_snapshot(&self) -> Result<(u64, String, String), ApiError> {
        let s = self.session.read().await;
        let c = s.creds.as_ref().ok_or(ApiError::AuthExpired)?;
        let base = validate_server_url(&c.server_url)?;
        Ok((s.generation, base, c.access_token.clone()))
    }

    async fn refresh_once(&self, expected_gen: u64, failed_access: &str) -> Result<(String, String), ApiError> {
        let _guard = self.refresh_lock.lock().await;
        let cur = {
            let s = self.session.read().await;
            if s.generation != expected_gen {
                return Err(ApiError::AuthExpired);
            }
            let c = s.creds.as_ref().ok_or(ApiError::AuthExpired)?;
            if c.access_token != failed_access {
                let base = validate_server_url(&c.server_url)?;
                return Ok((base, c.access_token.clone()));
            }
            c.clone()
        };
        let base = validate_server_url(&cur.server_url)?;
        let url = format!("{base}/v1/auth/refresh");
        let resp = self
            .http
            .post(&url)
            .json(&RefreshReq { refresh_token: cur.refresh_token.clone(), device_id: cur.device_id.clone() })
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            return Err(ApiError::AuthExpired);
        }
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        let auth: AuthResp = resp.json().await.map_err(|e| ApiError::Network(e.to_string()))?;

        let mut s = self.session.write().await;
        let Some(latest) = s.creds.as_ref() else {
            return Err(ApiError::AuthExpired);
        };
        if s.generation != expected_gen || !same_session_identity(latest, &cur) {
            return Err(ApiError::AuthExpired);
        }
        let mut next = latest.clone();
        next.access_token = auth.access_token.clone();
        next.refresh_token = auth.refresh_token;
        self.creds_store.save(&next).map_err(ApiError::Other)?;
        s.creds = Some(next);
        Ok((base, auth.access_token))
    }

    async fn authed_json<T: serde::de::DeserializeOwned, F>(&self, build: F) -> Result<T, ApiError>
    where
        F: Fn(&reqwest::Client, &str, &str) -> reqwest::RequestBuilder,
    {
        let (sess_gen, base, token) = self.session_snapshot().await?;
        let resp = build(&self.http, &base, &token)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = if resp.status() == StatusCode::UNAUTHORIZED {
            let (new_base, new_token) = self.refresh_once(sess_gen, &token).await?;
            build(&self.http, &new_base, &new_token)
                .send()
                .await
                .map_err(|e| ApiError::Network(e.to_string()))?
        } else {
            resp
        };
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(ApiError::AuthExpired);
        }
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        resp.json().await.map_err(|e| ApiError::Network(e.to_string()))
    }

    async fn authed_empty<F>(&self, build: F) -> Result<(), ApiError>
    where
        F: Fn(&reqwest::Client, &str, &str) -> reqwest::RequestBuilder,
    {
        let (sess_gen, base, token) = self.session_snapshot().await?;
        let resp = build(&self.http, &base, &token)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = if resp.status() == StatusCode::UNAUTHORIZED {
            let (new_base, new_token) = self.refresh_once(sess_gen, &token).await?;
            build(&self.http, &new_base, &new_token)
                .send()
                .await
                .map_err(|e| ApiError::Network(e.to_string()))?
        } else {
            resp
        };
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(ApiError::AuthExpired);
        }
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        Ok(())
    }

    pub async fn list_vaults(&self) -> Result<Vec<VaultSummary>, ApiError> {
        self.authed_json(|c, b, t| c.get(format!("{b}/v1/vaults")).bearer_auth(t)).await
    }

    pub async fn create_vault(&self, req: &CreateVaultReq) -> Result<VaultSummary, ApiError> {
        self.authed_json(|c, b, t| c.post(format!("{b}/v1/vaults")).bearer_auth(t).json(req)).await
    }

    pub async fn delete_vault(&self, vault_id: &str) -> Result<(), ApiError> {
        self.authed_empty(|c, b, t| c.delete(format!("{b}/v1/vaults/{vault_id}")).bearer_auth(t)).await
    }

    pub async fn update_e2ee_key(&self, vault_id: &str, req: &UpdateE2eeKeyReq) -> Result<(), ApiError> {
        self.authed_empty(|c, b, t| c.put(format!("{b}/v1/vaults/{vault_id}/e2ee-key")).bearer_auth(t).json(req))
            .await
    }

    pub async fn list_devices(&self) -> Result<Vec<DeviceInfo>, ApiError> {
        self.authed_json(|c, b, t| c.get(format!("{b}/v1/devices")).bearer_auth(t)).await
    }

    pub async fn get_storage_usage(&self) -> Result<StorageUsageResp, ApiError> {
        self.authed_json(|c, b, t| c.get(format!("{b}/v1/storage")).bearer_auth(t)).await
    }

    pub async fn revoke_device(&self, device_id: &str) -> Result<(), ApiError> {
        self.authed_empty(|c, b, t| c.delete(format!("{b}/v1/devices/{device_id}")).bearer_auth(t)).await
    }

    pub async fn ws_ticket(&self, vault_id: &str) -> Result<WsTicketResp, ApiError> {
        let body = serde_json::json!({ "vault_id": vault_id });
        self.authed_json(|c, b, t| c.post(format!("{b}/v1/auth/ws-ticket")).bearer_auth(t).json(&body))
            .await
    }

    pub async fn get_snapshot(&self, vault_id: &str) -> Result<SnapshotResp, ApiError> {
        self.authed_json(|c, b, t| c.get(format!("{b}/v1/vaults/{vault_id}/snapshot")).bearer_auth(t))
            .await
    }

    pub async fn get_changes(&self, vault_id: &str, since: u64, limit: u32) -> Result<ChangesResp, ApiError> {
        self.authed_json(|c, b, t| {
            c.get(format!("{b}/v1/vaults/{vault_id}/changes?since={since}&limit={limit}")).bearer_auth(t)
        })
        .await
    }

    pub async fn push(&self, vault_id: &str, req: &PushReq) -> Result<PushResp, ApiError> {
        self.authed_json(|c, b, t| c.post(format!("{b}/v1/vaults/{vault_id}/push")).bearer_auth(t).json(req))
            .await
    }

    pub async fn blobs_exists(&self, vault_id: &str, hashes: Vec<String>) -> Result<Vec<String>, ApiError> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let req = BlobsExistsReq { hashes };
        let resp: BlobsExistsResp = self
            .authed_json(|c, b, t| c.post(format!("{b}/v1/vaults/{vault_id}/blobs/exists")).bearer_auth(t).json(&req))
            .await?;
        Ok(resp.existing)
    }

    pub async fn put_blob(&self, vault_id: &str, hash: &str, bytes: Vec<u8>) -> Result<(), ApiError> {
        self.authed_empty(|c, b, t| {
            c.put(format!("{b}/v1/vaults/{vault_id}/blobs/{hash}")).bearer_auth(t).body(bytes.clone())
        })
        .await
    }

    pub async fn get_blob(&self, vault_id: &str, hash: &str) -> Result<Vec<u8>, ApiError> {
        let (sess_gen, base, token) = self.session_snapshot().await?;
        let url = format!("{base}/v1/vaults/{vault_id}/blobs/{hash}");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let resp = if resp.status() == StatusCode::UNAUTHORIZED {
            let (new_base, new_token) = self.refresh_once(sess_gen, &token).await?;
            let retry_url = format!("{new_base}/v1/vaults/{vault_id}/blobs/{hash}");
            self.http
                .get(&retry_url)
                .bearer_auth(&new_token)
                .send()
                .await
                .map_err(|e| ApiError::Network(e.to_string()))?
        } else {
            resp
        };
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(ApiError::AuthExpired);
        }
        if !resp.status().is_success() {
            return Err(parse_err(resp).await);
        }
        resp.bytes().await.map(|b| b.to_vec()).map_err(|e| ApiError::Network(e.to_string()))
    }
}

async fn parse_err(resp: reqwest::Response) -> ApiError {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if let Ok(body) = serde_json::from_str::<ErrorBody>(&text) {
        return ApiError::Server { status, code: body.code, message: body.message };
    }
    ApiError::Server {
        status,
        code: status.as_str().to_string(),
        message: if text.is_empty() { status.to_string() } else { text },
    }
}
