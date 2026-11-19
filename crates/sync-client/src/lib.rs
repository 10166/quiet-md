pub mod adapter;
pub mod credentials;
pub mod engine;
pub mod http;
pub mod scope;
pub mod state_db;
pub mod ws;

use anyhow::Result;
use base64::Engine;
use quiet_sync_proto::api::{
    CreateVaultReq, DeviceInfo, LoginReq, OnlineDevice, RegisterReq, StorageUsageResp,
    UpdateE2eeKeyReq, VaultSummary,
};
use quiet_sync_proto::crypto::VaultKeys;
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use uuid::Uuid;

pub use adapter::{
    config_entity_id, history_entity_id, parse_config_entity_id, parse_history_entity_id,
    parse_trash_entity_id, store_meta_from_sync, sync_meta_from_store, trash_entity_id, ApplyReport,
    FsLibraryAdapter, IncomingApply, LiveConflict, LocalEntity, LocalLibraryAdapter,
};
pub use credentials::{FileSecretStore, SecretStore, StoredCredentials};
pub use engine::{EngineCommand, EngineCtx, LocalChangeHint, SharedStatus, SyncEvent, SyncStatus, VaultUnlock};
pub use http::validate_server_url;
pub use scope::SyncScope;
pub use state_db::{DEFAULT_SYNC_INTERVAL_SECS, MAX_SYNC_INTERVAL_SECS};
pub use tokio;

#[derive(Clone)]
pub struct SyncHandle {
    ctx: Arc<EngineCtx>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    runtime: Option<Arc<tokio::runtime::Runtime>>,
}

impl SyncHandle {
    /// Start a dedicated background Tokio runtime (used by desktop GPUI app).
    pub fn start(sync_dir: PathBuf, adapter: Arc<dyn LocalLibraryAdapter>) -> Result<(Self, mpsc::UnboundedReceiver<SyncEvent>)> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("quiet-sync")
            .enable_all()
            .build()?;
        let rt = Arc::new(rt);
        let (handle, rx) = Self::start_on(rt.handle(), sync_dir, adapter, Some(rt.clone()))?;
        Ok((handle, rx))
    }

    /// Start on an existing Tokio runtime handle (used by async tests).
    pub fn start_on(
        rt_handle: &tokio::runtime::Handle,
        sync_dir: PathBuf,
        adapter: Arc<dyn LocalLibraryAdapter>,
        owned_rt: Option<Arc<tokio::runtime::Runtime>>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SyncEvent>)> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let db = state_db::StateDb::open(&sync_dir)?;
        let creds_store: Arc<dyn SecretStore> = Arc::new(FileSecretStore::new(&sync_dir));
        let initial_creds = creds_store.load()?;
        let api = http::ApiClient::new(initial_creds.clone(), creds_store.clone())?;
        let scope = db.scope();
        let paused = db.paused();
        let sync_interval_secs = db.sync_interval_secs();

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (sig_tx, sig_rx) = mpsc::unbounded_channel();

        let init_status = if initial_creds.is_none() {
            SyncStatus::Unconfigured
        } else if initial_creds.as_ref().is_some_and(|c| c.vault_id.is_none()) {
            SyncStatus::Unlinked
        } else if paused {
            SyncStatus::Paused
        } else {
            SyncStatus::Connecting
        };

        let ctx = Arc::new(EngineCtx {
            db,
            api,
            creds_store,
            adapter,
            ws: ws::WsConn::new(),
            events: events_tx,
            shared: SharedStatus {
                status: Arc::new(StdRwLock::new(init_status)),
                online: Arc::new(StdRwLock::new(Vec::new())),
                creds: Arc::new(StdRwLock::new(initial_creds)),
                scope: Arc::new(StdRwLock::new(scope)),
                paused: Arc::new(StdRwLock::new(paused)),
                sync_interval_secs: Arc::new(StdRwLock::new(sync_interval_secs)),
            },
            keys: AsyncMutex::new(None),
            sync_lock: AsyncMutex::new(()),
            ws_control: AsyncMutex::new(None),
            sig_tx,
        });

        let loop_ctx = ctx.clone();
        rt_handle.spawn(async move {
            engine::run_engine_loop(loop_ctx, cmd_rx, sig_rx).await;
        });

        Ok((Self { ctx, cmd_tx, runtime: owned_rt }, events_rx))
    }

    pub fn notify_changes(&self, hints: Vec<LocalChangeHint>) {
        if !hints.is_empty() {
            let _ = self.cmd_tx.send(EngineCommand::Notify(hints));
        }
    }

    pub fn trigger_sync(&self) {
        let _ = self.cmd_tx.send(EngineCommand::TriggerSync);
    }

    pub fn scan_and_sync(&self) {
        let _ = self.cmd_tx.send(EngineCommand::Notify(vec![LocalChangeHint::ScanAll]));
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(EngineCommand::SetPaused(paused));
    }

    pub fn set_scope(&self, scope: SyncScope) {
        let _ = self.cmd_tx.send(EngineCommand::SetScope(scope));
    }

    pub fn set_sync_interval_secs(&self, secs: u64) {
        let clamped = secs.min(state_db::MAX_SYNC_INTERVAL_SECS);
        *self.ctx.shared.sync_interval_secs.write().unwrap() = clamped;
        let _ = self.cmd_tx.send(EngineCommand::SetSyncInterval(clamped));
    }

    pub fn notify_user_activity(&self) {
        let _ = self.cmd_tx.send(EngineCommand::UserActivity);
    }

    pub fn set_active_doc(&self, doc_id: Option<String>) {
        let _ = self.cmd_tx.send(EngineCommand::SetActiveDoc(doc_id));
    }

    pub fn status(&self) -> SyncStatus {
        self.ctx.shared.status.read().unwrap().clone()
    }

    pub fn online_devices(&self) -> Vec<OnlineDevice> {
        self.ctx.shared.online.read().unwrap().clone()
    }

    pub fn credentials(&self) -> Option<StoredCredentials> {
        self.ctx.shared.creds.read().unwrap().clone()
    }

    pub fn scope(&self) -> SyncScope {
        *self.ctx.shared.scope.read().unwrap()
    }

    pub fn paused(&self) -> bool {
        *self.ctx.shared.paused.read().unwrap()
    }

    pub fn sync_interval_secs(&self) -> u64 {
        *self.ctx.shared.sync_interval_secs.read().unwrap()
    }

    pub fn is_linked(&self) -> bool {
        self.credentials().is_some_and(|c| c.vault_id.is_some())
    }

    pub fn is_e2ee_enabled(&self) -> bool {
        self.credentials().is_some_and(|c| c.vmk_b64.is_some() || c.e2ee_setup.is_some())
    }

    pub fn recovery_key(&self) -> Option<String> {
        let b64 = self.credentials()?.vmk_b64?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Some(VaultKeys::from_bytes(arr).to_recovery_key())
    }

    /// Run an async operation on the owned Tokio runtime (for GPUI background tasks).
    pub fn block_on_bg<F, R>(&self, f: impl FnOnce(SyncHandle) -> F + Send + 'static) -> Result<R>
    where
        F: std::future::Future<Output = Result<R>> + Send,
        R: Send,
    {
        let rt = self.runtime.as_ref().ok_or_else(|| anyhow::anyhow!("no owned runtime"))?;
        rt.block_on(f(self.clone()))
    }

    pub fn begin_shutdown(&self) -> Option<oneshot::Receiver<()>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(EngineCommand::FlushAndStop(tx)).ok().map(|()| rx)
    }

    pub fn shutdown_and_flush(&self, timeout: Duration) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(EngineCommand::FlushAndStop(tx)).is_ok()
            && let Some(rt) = &self.runtime
        {
            let _ = rt.block_on(async { tokio::time::timeout(timeout, rx).await });
        }
    }

    pub async fn shutdown_and_flush_async(&self, timeout: Duration) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(EngineCommand::FlushAndStop(tx)).is_ok() {
            let _ = tokio::time::timeout(timeout, rx).await;
        }
    }

    pub async fn check_server(&self, server_url: &str) -> Result<()> {
        Ok(self.ctx.api.healthz(server_url).await?)
    }

    pub async fn register(
        &self,
        server_url: &str,
        email: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        invite_code: Option<String>,
    ) -> Result<StoredCredentials> {
        let server_url = validate_server_url(server_url)?;
        let device_id = Uuid::new_v4().to_string();
        let dev_name = if device_name.trim().is_empty() { engine::default_device_name() } else { device_name.trim().to_string() };
        let resp = self
            .ctx
            .api
            .register(
                &server_url,
                &RegisterReq {
                    email: email.trim().to_string(),
                    password: password.to_string(),
                    display_name: display_name.trim().to_string(),
                    device_id: device_id.clone(),
                    device_name: dev_name.clone(),
                    platform: ws::current_platform().into(),
                    invite_code: invite_code.filter(|s| !s.trim().is_empty()),
                },
            )
            .await?;
        let creds = StoredCredentials {
            server_url,
            email: resp.user.email,
            user_id: resp.user.id,
            display_name: resp.user.display_name,
            device_id: resp.device.id,
            device_name: resp.device.name,
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
            vault_id: None,
            vault_name: None,
            vmk_b64: None,
            e2ee_setup: None,
        };
        self.ctx.db.reset_vault_state()?;
        *self.ctx.shared.paused.write().unwrap() = false;
        self.ctx.api.set_credentials(Some(creds.clone())).await?;
        self.ctx.reload_keys_from_creds().await;
        let _ = self.cmd_tx.send(EngineCommand::ReloadSession);
        Ok(creds)
    }

    pub async fn login(&self, server_url: &str, email: &str, password: &str, device_name: &str) -> Result<StoredCredentials> {
        let server_url = validate_server_url(server_url)?;
        let prev = self.ctx.api.credentials().await;
        let same_account = prev
            .as_ref()
            .is_some_and(|c| c.server_url == server_url && c.email.eq_ignore_ascii_case(email.trim()));
        let device_id = if same_account {
            prev.as_ref().unwrap().device_id.clone()
        } else {
            Uuid::new_v4().to_string()
        };
        let dev_name = if device_name.trim().is_empty() { engine::default_device_name() } else { device_name.trim().to_string() };
        let resp = self
            .ctx
            .api
            .login(
                &server_url,
                &LoginReq {
                    email: email.trim().to_string(),
                    password: password.to_string(),
                    device_id,
                    device_name: dev_name,
                    platform: ws::current_platform().into(),
                },
            )
            .await?;
        let same_user_session = prev
            .as_ref()
            .is_some_and(|c| c.server_url == server_url && c.user_id == resp.user.id);
        let creds = if same_user_session {
            let p = prev.unwrap();
            StoredCredentials {
                server_url,
                email: resp.user.email,
                user_id: resp.user.id,
                display_name: resp.user.display_name,
                device_id: resp.device.id,
                device_name: resp.device.name,
                access_token: resp.access_token,
                refresh_token: resp.refresh_token,
                vault_id: p.vault_id,
                vault_name: p.vault_name,
                vmk_b64: p.vmk_b64,
                e2ee_setup: p.e2ee_setup,
            }
        } else {
            self.ctx.db.reset_vault_state()?;
            *self.ctx.shared.paused.write().unwrap() = false;
            StoredCredentials {
                server_url,
                email: resp.user.email,
                user_id: resp.user.id,
                display_name: resp.user.display_name,
                device_id: resp.device.id,
                device_name: resp.device.name,
                access_token: resp.access_token,
                refresh_token: resp.refresh_token,
                vault_id: None,
                vault_name: None,
                vmk_b64: None,
                e2ee_setup: None,
            }
        };
        self.ctx.api.set_credentials(Some(creds.clone())).await?;
        self.ctx.reload_keys_from_creds().await;
        let _ = self.cmd_tx.send(EngineCommand::ReloadSession);
        Ok(creds)
    }

    pub async fn logout(&self) -> Result<()> {
        self.ctx.stop_ws().await;
        let _ = self.ctx.api.logout().await;
        self.ctx.api.set_credentials(None).await?;
        self.ctx.db.reset_vault_state()?;
        *self.ctx.shared.paused.write().unwrap() = false;
        self.ctx.reload_keys_from_creds().await;
        self.ctx.emit_status(SyncStatus::Unconfigured);
        Ok(())
    }

    pub async fn list_vaults(&self) -> Result<Vec<VaultSummary>> {
        Ok(self.ctx.api.list_vaults().await?)
    }

    pub async fn create_vault(&self, name: &str, e2ee_passphrase: Option<String>) -> Result<(VaultSummary, Option<String>)> {
        let (e2ee_setup, vmk_b64, recovery) = match e2ee_passphrase.filter(|s| !s.is_empty()) {
            Some(pass) => {
                let keys = VaultKeys::generate();
                let setup = keys.wrap_with_passphrase(&pass)?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(keys.as_bytes());
                let rec = keys.to_recovery_key();
                (Some(setup), Some(b64), Some(rec))
            }
            None => (None, None, None),
        };
        let vault = self
            .ctx
            .api
            .create_vault(&CreateVaultReq { name: name.trim().to_string(), e2ee: e2ee_setup.clone() })
            .await?;

        let mut creds = self.ctx.api.credentials().await.ok_or_else(|| anyhow::anyhow!("未登录"))?;
        creds.vault_id = Some(vault.id.clone());
        creds.vault_name = Some(vault.name.clone());
        creds.vmk_b64 = vmk_b64;
        creds.e2ee_setup = e2ee_setup;
        self.ctx.api.set_credentials(Some(creds)).await?;
        self.ctx.reload_keys_from_creds().await;
        let _ = self.ctx.db.set_paused(false);
        *self.ctx.shared.paused.write().unwrap() = false;
        engine::initial_sync(&self.ctx, &vault.id).await?;
        self.ctx.restart_ws().await;
        Ok((vault, recovery))
    }

    pub async fn link_vault(&self, vault: VaultSummary, unlock: Option<VaultUnlock>) -> Result<()> {
        let vmk_b64 = match (&vault.e2ee, unlock) {
            (Some(setup), Some(VaultUnlock::Passphrase(pass))) => {
                let keys = VaultKeys::unwrap_with_passphrase(&pass, setup)?;
                Some(base64::engine::general_purpose::STANDARD.encode(keys.as_bytes()))
            }
            (Some(setup), Some(VaultUnlock::RecoveryKey(rec))) => {
                let keys = VaultKeys::from_recovery_key(&rec)?;
                keys.verify_setup(setup)?;
                Some(base64::engine::general_purpose::STANDARD.encode(keys.as_bytes()))
            }
            (Some(_), None) => anyhow::bail!("该远程文档库已开启端到端加密，请输入同步口令或恢复密钥"),
            (None, _) => None,
        };
        let mut creds = self.ctx.api.credentials().await.ok_or_else(|| anyhow::anyhow!("未登录"))?;
        creds.vault_id = Some(vault.id.clone());
        creds.vault_name = Some(vault.name.clone());
        creds.vmk_b64 = vmk_b64;
        creds.e2ee_setup = vault.e2ee;
        self.ctx.api.set_credentials(Some(creds)).await?;
        self.ctx.reload_keys_from_creds().await;
        let _ = self.ctx.db.set_paused(false);
        *self.ctx.shared.paused.write().unwrap() = false;
        engine::initial_sync(&self.ctx, &vault.id).await?;
        self.ctx.restart_ws().await;
        Ok(())
    }

    pub async fn unlink_vault(&self) -> Result<()> {
        self.ctx.stop_ws().await;
        let Some(mut creds) = self.ctx.api.credentials().await else { return Ok(()) };
        creds.vault_id = None;
        creds.vault_name = None;
        creds.vmk_b64 = None;
        creds.e2ee_setup = None;
        self.ctx.api.set_credentials(Some(creds)).await?;
        // Retain entity_state & vault_id across unlink so re-linking preserves 3-way merge baselines.
        self.ctx.db.clear_outbox_for_unlink()?;
        *self.ctx.shared.paused.write().unwrap() = false;
        self.ctx.reload_keys_from_creds().await;
        self.ctx.emit_status(SyncStatus::Unlinked);
        Ok(())
    }

    pub async fn rotate_e2ee_passphrase(&self, new_passphrase: &str) -> Result<()> {
        let mut creds = self.ctx.api.credentials().await.ok_or_else(|| anyhow::anyhow!("未登录"))?;
        let vault_id = creds.vault_id.clone().ok_or_else(|| anyhow::anyhow!("未关联文档库"))?;
        let b64 = creds.vmk_b64.as_deref().ok_or_else(|| anyhow::anyhow!("当前文档库未开启端到端加密"))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        anyhow::ensure!(bytes.len() == 32, "invalid VMK");
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        let keys = VaultKeys::from_bytes(arr);
        let new_setup = keys.wrap_with_passphrase(new_passphrase)?;
        self.ctx.api.update_e2ee_key(&vault_id, &UpdateE2eeKeyReq { e2ee: new_setup.clone() }).await?;
        creds.e2ee_setup = Some(new_setup);
        self.ctx.api.set_credentials(Some(creds)).await?;
        Ok(())
    }

    pub async fn update_device_name(&self, name: &str) -> Result<()> {
        let Some(mut creds) = self.ctx.api.credentials().await else { return Ok(()) };
        creds.device_name = name.trim().to_string();
        self.ctx.api.set_credentials(Some(creds)).await?;
        self.ctx.reload_keys_from_creds().await;
        let _ = self.cmd_tx.send(EngineCommand::ReloadSession);
        Ok(())
    }

    pub async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.ctx.api.list_devices().await?)
    }

    pub async fn storage_usage(&self) -> Result<StorageUsageResp> {
        Ok(self.ctx.api.get_storage_usage().await?)
    }

    pub async fn revoke_device(&self, device_id: &str) -> Result<()> {
        Ok(self.ctx.api.revoke_device(device_id).await?)
    }

    pub async fn sync_now(&self) -> Result<()> {
        let _ = engine::scan_local_into_outbox(&self.ctx).await;
        engine::sync_once(&self.ctx).await
    }
}
