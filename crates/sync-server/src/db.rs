use anyhow::{Context, Result};
use base64::Engine;
use quiet_sync_proto::api::{
    DeviceInfo, PushItemResult, PushOp, RemoteChange, VaultSummary,
};
use quiet_sync_proto::crypto::{E2eeSetup, Envelope};
use quiet_sync_proto::entity::EntityKind;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use uuid::Uuid;

pub fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Primary,
    Backup,
}

impl NodeRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Backup => "backup",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "backup" => Self::Backup,
            _ => Self::Primary,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: String,
    pub node_name: String,
    pub portal_url: String,
    pub role: NodeRole,
    pub repl_epoch: String,
    pub min_retained_lsn: u64,
    pub admin_password_hash: Option<String>,
    pub admin_token_version: u64,
    pub allow_register_override: Option<bool>,
    pub invite_code_override: Option<String>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplPushItem {
    pub seq: u64,
    pub entity_id: String,
    pub kind: String,
    pub version: u64,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_b64: Option<String>,
    pub updated_at: u64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blob_hashes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReplEvent {
    UserUpsert {
        id: String,
        email: String,
        display_name: String,
        password_hash: String,
        created_at: u64,
    },
    UserDelete {
        user_id: String,
    },
    DeviceUpsert {
        id: String,
        user_id: String,
        name: String,
        platform: String,
        refresh_hash: String,
        refresh_expires_at: u64,
        last_seen_at: u64,
        revoked: bool,
    },
    VaultUpsert {
        id: String,
        owner_id: String,
        name: String,
        head_seq: u64,
        e2ee_enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        e2ee_meta: Option<String>,
        min_retained_seq: u64,
        created_at: u64,
    },
    VaultDelete {
        vault_id: String,
    },
    VaultBlobRegister {
        vault_id: String,
        blob_hash: String,
        size: u64,
        created_at: u64,
    },
    PushBatch {
        vault_id: String,
        head_seq: u64,
        device_id: String,
        device_last_seen_at: u64,
        items: Vec<ReplPushItem>,
    },
    GcCompact {
        tombstone_cutoff: u64,
        upload_grace_cutoff: u64,
    },
}

impl ReplEvent {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::UserUpsert { .. } => "user_upsert",
            Self::UserDelete { .. } => "user_delete",
            Self::DeviceUpsert { .. } => "device_upsert",
            Self::VaultUpsert { .. } => "vault_upsert",
            Self::VaultDelete { .. } => "vault_delete",
            Self::VaultBlobRegister { .. } => "vault_blob_register",
            Self::PushBatch { .. } => "push_batch",
            Self::GcCompact { .. } => "gc_compact",
        }
    }
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
    binlog_head: Arc<watch::Sender<u64>>,
}

#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub password_hash: String,
    pub created_at: u64,
}

#[derive(Clone, Debug)]
pub struct DeviceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub platform: String,
    pub refresh_hash: String,
    pub refresh_expires_at: u64,
    pub last_seen_at: u64,
    pub revoked: bool,
}

#[derive(Debug)]
pub enum DbError {
    BadRequest(&'static str),
    NotFound(&'static str),
    Forbidden(&'static str),
    Conflict(&'static str),
    ReadOnly(&'static str),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for DbError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let conn = Connection::open(path).context("open sqlite database")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL UNIQUE COLLATE NOCASE,
                display_name TEXT NOT NULL,
                password_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS devices (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                platform TEXT NOT NULL,
                refresh_hash TEXT NOT NULL,
                refresh_expires_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_devices_user ON devices(user_id);
            CREATE TABLE IF NOT EXISTS vaults (
                id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                head_seq INTEGER NOT NULL DEFAULT 0,
                e2ee_enabled INTEGER NOT NULL DEFAULT 0,
                e2ee_meta TEXT,
                min_retained_seq INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_vaults_owner ON vaults(owner_id);
            CREATE TABLE IF NOT EXISTS entities (
                vault_id TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                entity_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                version INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0,
                envelope BLOB,
                updated_at INTEGER NOT NULL,
                device_id TEXT NOT NULL,
                PRIMARY KEY (vault_id, entity_id)
            );
            CREATE INDEX IF NOT EXISTS idx_entities_vault_seq ON entities(vault_id, seq);
            CREATE TABLE IF NOT EXISTS op_log (
                vault_id TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                entity_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                version INTEGER NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0,
                envelope BLOB,
                updated_at INTEGER NOT NULL,
                device_id TEXT NOT NULL,
                PRIMARY KEY (vault_id, seq)
            );
            CREATE TABLE IF NOT EXISTS vault_blobs (
                vault_id TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                blob_hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (vault_id, blob_hash)
            );
            CREATE TABLE IF NOT EXISTS entity_blobs (
                vault_id TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                entity_id TEXT NOT NULL,
                blob_hash TEXT NOT NULL,
                PRIMARY KEY (vault_id, entity_id, blob_hash)
            );
            CREATE INDEX IF NOT EXISTS idx_entity_blobs_hash ON entity_blobs(blob_hash);
            CREATE TABLE IF NOT EXISTS op_log_blobs (
                vault_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                blob_hash TEXT NOT NULL,
                PRIMARY KEY (vault_id, seq, blob_hash),
                FOREIGN KEY (vault_id, seq) REFERENCES op_log(vault_id, seq) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_op_log_blobs_hash ON op_log_blobs(blob_hash);
            CREATE TABLE IF NOT EXISTS blob_leases (
                blob_hash TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS node_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                node_id TEXT NOT NULL,
                node_name TEXT NOT NULL,
                portal_url TEXT NOT NULL DEFAULT '',
                role TEXT NOT NULL DEFAULT 'primary',
                repl_epoch TEXT NOT NULL,
                min_retained_lsn INTEGER NOT NULL DEFAULT 0,
                admin_password_hash TEXT,
                admin_token_version INTEGER NOT NULL DEFAULT 1,
                allow_register_override INTEGER,
                invite_code_override TEXT,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS repl_binlog (
                lsn INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                op TEXT NOT NULL,
                payload BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_repl_binlog_ts ON repl_binlog(ts);
            CREATE TABLE IF NOT EXISTS repl_replicas (
                replica_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                portal_url TEXT NOT NULL DEFAULT '',
                token_hash TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                applied_lsn INTEGER NOT NULL DEFAULT 0,
                blob_pending INTEGER NOT NULL DEFAULT 0,
                storage_bytes INTEGER NOT NULL DEFAULT 0,
                server_version TEXT NOT NULL DEFAULT '',
                remote_addr TEXT NOT NULL DEFAULT '',
                last_seen_at INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS repl_upstream (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                primary_url TEXT NOT NULL,
                primary_id TEXT NOT NULL,
                primary_name TEXT NOT NULL,
                primary_portal_url TEXT NOT NULL DEFAULT '',
                primary_epoch TEXT NOT NULL,
                replica_token TEXT NOT NULL,
                bootstrapped INTEGER NOT NULL DEFAULT 0,
                applied_lsn INTEGER NOT NULL DEFAULT 0,
                primary_head_lsn INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'idle',
                last_sync_at INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                configured_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS repl_pending_blobs (
                blob_hash TEXT PRIMARY KEY,
                size INTEGER NOT NULL DEFAULT 0,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );",
        )?;
        let now = unix_secs() as i64;
        let has_node: Option<i64> = conn
            .query_row("SELECT 1 FROM node_config WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        if has_node.is_none() {
            let node_id = Uuid::new_v4().to_string();
            let epoch = Uuid::new_v4().to_string();
            let default_name = default_node_name();
            conn.execute(
                "INSERT INTO node_config (id, node_id, node_name, portal_url, role, repl_epoch, min_retained_lsn, admin_token_version, updated_at)
                 VALUES (1, ?1, ?2, '', 'primary', ?3, 0, 1, ?4)",
                params![node_id, default_name, epoch, now],
            )?;
        }
        let head_lsn: i64 = conn
            .query_row(
                "SELECT MAX(COALESCE((SELECT MAX(lsn) FROM repl_binlog), 0), (SELECT min_retained_lsn FROM node_config WHERE id = 1))",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let (binlog_tx, _) = watch::channel(head_lsn.max(0) as u64);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
            binlog_head: Arc::new(binlog_tx),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn open_readonly(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open readonly sqlite connection")?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        Ok(conn)
    }

    pub fn subscribe_binlog(&self) -> watch::Receiver<u64> {
        self.binlog_head.subscribe()
    }

    pub fn current_binlog_head(&self) -> u64 {
        *self.binlog_head.borrow()
    }

    pub fn set_binlog_head(&self, lsn: u64) {
        self.binlog_head.send_replace(lsn);
    }

    pub async fn run<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&mut Connection) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        let binlog_head = self.binlog_head.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| DbError::Internal(anyhow::anyhow!("db mutex poisoned")))?;
            let before = guard.total_changes();
            let res = f(&mut guard);
            if res.is_ok()
                && guard.total_changes() != before
                && let Ok(latest) = guard.query_row(
                    "SELECT MAX(COALESCE((SELECT MAX(lsn) FROM repl_binlog), 0), (SELECT min_retained_lsn FROM node_config WHERE id = 1))",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            {
                let latest = latest.max(0) as u64;
                binlog_head.send_if_modified(|cur| {
                    if latest > *cur {
                        *cur = latest;
                        true
                    } else {
                        false
                    }
                });
            }
            res
        })
        .await
        .map_err(|e| DbError::Internal(e.into()))?
    }
}

fn default_node_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "quiet-sync-server".to_string())
}

pub fn get_node_config(conn: &Connection) -> Result<NodeConfig, DbError> {
    conn.query_row(
        "SELECT node_id, node_name, portal_url, role, repl_epoch, min_retained_lsn,
                admin_password_hash, admin_token_version, allow_register_override,
                invite_code_override, updated_at
         FROM node_config WHERE id = 1",
        [],
        |r| {
            let allow_ov: Option<i64> = r.get(8)?;
            Ok(NodeConfig {
                node_id: r.get(0)?,
                node_name: r.get(1)?,
                portal_url: r.get(2)?,
                role: NodeRole::parse(&r.get::<_, String>(3)?),
                repl_epoch: r.get(4)?,
                min_retained_lsn: r.get::<_, i64>(5)? as u64,
                admin_password_hash: r.get(6)?,
                admin_token_version: r.get::<_, i64>(7)? as u64,
                allow_register_override: allow_ov.map(|v| v != 0),
                invite_code_override: r.get(9)?,
                updated_at: r.get::<_, i64>(10)? as u64,
            })
        },
    )
    .map_err(DbError::from)
}

pub fn get_head_lsn(conn: &Connection) -> Result<u64, DbError> {
    let lsn: i64 = conn.query_row(
        "SELECT MAX(COALESCE((SELECT MAX(lsn) FROM repl_binlog), 0), (SELECT min_retained_lsn FROM node_config WHERE id = 1))",
        [],
        |r| r.get(0),
    )?;
    Ok(lsn.max(0) as u64)
}

pub fn ensure_primary_tx(conn: &Connection) -> Result<(), DbError> {
    let role: String = conn.query_row("SELECT role FROM node_config WHERE id = 1", [], |r| r.get(0))?;
    if NodeRole::parse(&role) != NodeRole::Primary {
        return Err(DbError::ReadOnly("当前服务器处于只读备份模式，请连接主服务器执行写入操作"));
    }
    Ok(())
}

pub fn append_binlog(conn: &Connection, event: &ReplEvent) -> Result<u64, DbError> {
    ensure_primary_tx(conn)?;
    let payload = serde_json::to_vec(event).map_err(|e| DbError::Internal(e.into()))?;
    conn.execute(
        "INSERT INTO repl_binlog (ts, op, payload) VALUES (?1, ?2, ?3)",
        params![unix_secs() as i64, event.op_name(), payload],
    )?;
    Ok(conn.last_insert_rowid() as u64)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UserStorageUsage {
    pub entity_count: u64,
    pub entity_bytes: u64,
    pub blob_count: u64,
    pub blob_bytes: u64,
}

impl UserStorageUsage {
    pub fn used_bytes(self) -> u64 {
        self.entity_bytes.saturating_add(self.blob_bytes)
    }
}

pub fn get_user_storage_usage(conn: &Connection, user_id: &str) -> Result<UserStorageUsage, DbError> {
    conn.query_row(
        "SELECT
            (SELECT COUNT(*) FROM entities e JOIN vaults v ON v.id = e.vault_id WHERE v.owner_id = ?1 AND e.deleted = 0),
            COALESCE((SELECT SUM(LENGTH(e.envelope)) FROM entities e JOIN vaults v ON v.id = e.vault_id WHERE v.owner_id = ?1 AND e.deleted = 0), 0),
            (SELECT COUNT(*) FROM vault_blobs vb JOIN vaults v ON v.id = vb.vault_id WHERE v.owner_id = ?1),
            COALESCE((SELECT SUM(vb.size) FROM vault_blobs vb JOIN vaults v ON v.id = vb.vault_id WHERE v.owner_id = ?1), 0)",
        params![user_id],
        |r| {
            Ok(UserStorageUsage {
                entity_count: r.get::<_, i64>(0)? as u64,
                entity_bytes: r.get::<_, i64>(1)? as u64,
                blob_count: r.get::<_, i64>(2)? as u64,
                blob_bytes: r.get::<_, i64>(3)? as u64,
            })
        },
    )
    .map_err(DbError::from)
}

pub fn create_user(conn: &mut Connection, id: &str, email: &str, display_name: &str, password_hash: &str) -> Result<UserRow, DbError> {
    let now = unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists: Option<String> = tx
        .query_row("SELECT id FROM users WHERE email = ?1", params![email], |r| r.get(0))
        .optional()?;
    if exists.is_some() {
        return Err(DbError::Conflict("该邮箱已注册"));
    }
    tx.execute(
        "INSERT INTO users (id, email, display_name, password_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, email, display_name, password_hash, now as i64],
    )?;
    append_binlog(
        &tx,
        &ReplEvent::UserUpsert {
            id: id.into(),
            email: email.into(),
            display_name: display_name.into(),
            password_hash: password_hash.into(),
            created_at: now,
        },
    )?;
    tx.commit()?;
    Ok(UserRow {
        id: id.into(),
        email: email.into(),
        display_name: display_name.into(),
        password_hash: password_hash.into(),
        created_at: now,
    })
}

pub fn find_user_by_email(conn: &Connection, email: &str) -> Result<Option<UserRow>, DbError> {
    Ok(conn
        .query_row(
            "SELECT id, email, display_name, password_hash, created_at FROM users WHERE email = ?1",
            params![email],
            |r| {
                Ok(UserRow {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    display_name: r.get(2)?,
                    password_hash: r.get(3)?,
                    created_at: r.get::<_, i64>(4)? as u64,
                })
            },
        )
        .optional()?)
}

pub fn find_user_by_id(conn: &Connection, id: &str) -> Result<Option<UserRow>, DbError> {
    Ok(conn
        .query_row(
            "SELECT id, email, display_name, password_hash, created_at FROM users WHERE id = ?1",
            params![id],
            |r| {
                Ok(UserRow {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    display_name: r.get(2)?,
                    password_hash: r.get(3)?,
                    created_at: r.get::<_, i64>(4)? as u64,
                })
            },
        )
        .optional()?)
}

pub fn upsert_device(
    conn: &mut Connection,
    device_id: &str,
    user_id: &str,
    name: &str,
    platform: &str,
    refresh_hash: &str,
    refresh_expires_at: u64,
) -> Result<DeviceInfo, DbError> {
    let now = unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Never let another user claim an existing device_id.
    if let Some(existing_owner) = tx
        .query_row("SELECT user_id FROM devices WHERE id = ?1", params![device_id], |r| r.get::<_, String>(0))
        .optional()?
        && existing_owner != user_id
    {
        return Err(DbError::Forbidden("设备标识与当前账号不匹配"));
    }
    tx.execute(
        "INSERT INTO devices (id, user_id, name, platform, refresh_hash, refresh_expires_at, last_seen_at, revoked)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
         ON CONFLICT(id) DO UPDATE SET
           name = excluded.name,
           platform = excluded.platform,
           refresh_hash = excluded.refresh_hash,
           refresh_expires_at = excluded.refresh_expires_at,
           last_seen_at = excluded.last_seen_at,
           revoked = 0",
        params![device_id, user_id, name, platform, refresh_hash, refresh_expires_at as i64, now as i64],
    )?;
    append_binlog(
        &tx,
        &ReplEvent::DeviceUpsert {
            id: device_id.into(),
            user_id: user_id.into(),
            name: name.into(),
            platform: platform.into(),
            refresh_hash: refresh_hash.into(),
            refresh_expires_at,
            last_seen_at: now,
            revoked: false,
        },
    )?;
    tx.commit()?;
    Ok(DeviceInfo {
        id: device_id.into(),
        name: name.into(),
        platform: platform.into(),
        last_seen_at: now,
        online: false,
    })
}

pub fn get_device(conn: &Connection, device_id: &str) -> Result<Option<DeviceRow>, DbError> {
    Ok(conn
        .query_row(
            "SELECT id, user_id, name, platform, refresh_hash, refresh_expires_at, last_seen_at, revoked FROM devices WHERE id = ?1",
            params![device_id],
            |r| {
                Ok(DeviceRow {
                    id: r.get(0)?,
                    user_id: r.get(1)?,
                    name: r.get(2)?,
                    platform: r.get(3)?,
                    refresh_hash: r.get(4)?,
                    refresh_expires_at: r.get::<_, i64>(5)? as u64,
                    last_seen_at: r.get::<_, i64>(6)? as u64,
                    revoked: r.get::<_, i64>(7)? != 0,
                })
            },
        )
        .optional()?)
}

pub fn is_device_active(conn: &Connection, user_id: &str, device_id: &str) -> Result<bool, DbError> {
    let row: Option<(String, i64)> = conn
        .query_row("SELECT user_id, revoked FROM devices WHERE id = ?1", params![device_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(matches!(row, Some((uid, 0)) if uid == user_id))
}

pub fn rotate_refresh(conn: &mut Connection, device_id: &str, new_hash: &str, new_exp: u64) -> Result<(), DbError> {
    let now = unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE devices SET refresh_hash = ?1, refresh_expires_at = ?2, last_seen_at = ?3 WHERE id = ?4",
        params![new_hash, new_exp as i64, now as i64, device_id],
    )?;
    if let Some(dev) = get_device(&tx, device_id)? {
        append_binlog(
            &tx,
            &ReplEvent::DeviceUpsert {
                id: dev.id,
                user_id: dev.user_id,
                name: dev.name,
                platform: dev.platform,
                refresh_hash: dev.refresh_hash,
                refresh_expires_at: dev.refresh_expires_at,
                last_seen_at: dev.last_seen_at,
                revoked: dev.revoked,
            },
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn touch_device(conn: &Connection, device_id: &str) -> Result<(), DbError> {
    conn.execute("UPDATE devices SET last_seen_at = ?1 WHERE id = ?2", params![unix_secs() as i64, device_id])?;
    Ok(())
}

pub fn list_devices(conn: &Connection, user_id: &str) -> Result<Vec<DeviceInfo>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, platform, last_seen_at FROM devices WHERE user_id = ?1 AND revoked = 0 ORDER BY last_seen_at DESC",
    )?;
    let rows = stmt.query_map(params![user_id], |r| {
        Ok(DeviceInfo {
            id: r.get(0)?,
            name: r.get(1)?,
            platform: r.get(2)?,
            last_seen_at: r.get::<_, i64>(3)? as u64,
            online: false,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn revoke_device(conn: &mut Connection, user_id: &str, device_id: &str) -> Result<bool, DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let n = tx.execute(
        "UPDATE devices SET revoked = 1, refresh_hash = '' WHERE id = ?1 AND user_id = ?2",
        params![device_id, user_id],
    )?;
    if n > 0
        && let Some(dev) = get_device(&tx, device_id)?
    {
        append_binlog(
            &tx,
            &ReplEvent::DeviceUpsert {
                id: dev.id,
                user_id: dev.user_id,
                name: dev.name,
                platform: dev.platform,
                refresh_hash: dev.refresh_hash,
                refresh_expires_at: dev.refresh_expires_at,
                last_seen_at: dev.last_seen_at,
                revoked: dev.revoked,
            },
        )?;
    }
    tx.commit()?;
    Ok(n > 0)
}

pub fn revoke_by_refresh_hash(conn: &mut Connection, refresh_hash: &str) -> Result<Option<(String, String)>, DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT user_id, id FROM devices WHERE refresh_hash = ?1 AND revoked = 0",
            params![refresh_hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    tx.execute("UPDATE devices SET revoked = 1, refresh_hash = '' WHERE refresh_hash = ?1", params![refresh_hash])?;
    if let Some((_, ref did)) = row
        && let Some(dev) = get_device(&tx, did)?
    {
        append_binlog(
            &tx,
            &ReplEvent::DeviceUpsert {
                id: dev.id,
                user_id: dev.user_id,
                name: dev.name,
                platform: dev.platform,
                refresh_hash: dev.refresh_hash,
                refresh_expires_at: dev.refresh_expires_at,
                last_seen_at: dev.last_seen_at,
                revoked: dev.revoked,
            },
        )?;
    }
    tx.commit()?;
    Ok(row)
}

pub fn list_vaults(conn: &Connection, user_id: &str) -> Result<Vec<VaultSummary>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, head_seq, e2ee_meta, created_at FROM vaults WHERE owner_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![user_id], |r| {
        let e2ee_json: Option<String> = r.get(3)?;
        let e2ee = e2ee_json.and_then(|s| serde_json::from_str::<E2eeSetup>(&s).ok());
        Ok(VaultSummary {
            id: r.get(0)?,
            name: r.get(1)?,
            head_seq: r.get::<_, i64>(2)? as u64,
            e2ee,
            created_at: r.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn create_vault(conn: &mut Connection, id: &str, owner_id: &str, name: &str, e2ee: Option<&E2eeSetup>) -> Result<VaultSummary, DbError> {
    let now = unix_secs();
    let e2ee_json = e2ee.map(serde_json::to_string).transpose().map_err(|e| DbError::Internal(e.into()))?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO vaults (id, owner_id, name, head_seq, e2ee_enabled, e2ee_meta, min_retained_seq, created_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?5, 0, ?6)",
        params![id, owner_id, name, i32::from(e2ee.is_some()), e2ee_json, now as i64],
    )?;
    append_binlog(
        &tx,
        &ReplEvent::VaultUpsert {
            id: id.into(),
            owner_id: owner_id.into(),
            name: name.into(),
            head_seq: 0,
            e2ee_enabled: e2ee.is_some(),
            e2ee_meta: e2ee_json,
            min_retained_seq: 0,
            created_at: now,
        },
    )?;
    tx.commit()?;
    Ok(VaultSummary {
        id: id.into(),
        name: name.into(),
        head_seq: 0,
        e2ee: e2ee.cloned(),
        created_at: now,
    })
}

pub fn delete_vault(conn: &mut Connection, owner_id: &str, vault_id: &str) -> Result<bool, DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let n = tx.execute("DELETE FROM vaults WHERE id = ?1 AND owner_id = ?2", params![vault_id, owner_id])?;
    if n > 0 {
        append_binlog(&tx, &ReplEvent::VaultDelete { vault_id: vault_id.into() })?;
    }
    tx.commit()?;
    Ok(n > 0)
}

pub fn update_vault_e2ee_key(conn: &mut Connection, owner_id: &str, vault_id: &str, e2ee: &E2eeSetup) -> Result<(), DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(i64, i64, String, i64, i64)> = tx
        .query_row(
            "SELECT e2ee_enabled, head_seq, name, min_retained_seq, created_at FROM vaults WHERE id = ?1 AND owner_id = ?2",
            params![vault_id, owner_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (_, head_seq, name, min_retained_seq, created_at) = match row {
        None => return Err(DbError::NotFound("文档库不存在")),
        Some((0, seq, _, _, _)) if seq > 0 => return Err(DbError::Conflict("已有同步记录的明文文档库不能直接启用端到端加密")),
        Some(v) => v,
    };
    let json = serde_json::to_string(e2ee).map_err(|e| DbError::Internal(e.into()))?;
    tx.execute(
        "UPDATE vaults SET e2ee_enabled = 1, e2ee_meta = ?1 WHERE id = ?2 AND owner_id = ?3",
        params![json, vault_id, owner_id],
    )?;
    append_binlog(
        &tx,
        &ReplEvent::VaultUpsert {
            id: vault_id.into(),
            owner_id: owner_id.into(),
            name,
            head_seq: head_seq as u64,
            e2ee_enabled: true,
            e2ee_meta: Some(json),
            min_retained_seq: min_retained_seq as u64,
            created_at: created_at as u64,
        },
    )?;
    tx.commit()?;
    Ok(())
}

pub fn verify_vault_owner(conn: &Connection, owner_id: &str, vault_id: &str) -> Result<(u64, u64), DbError> {
    let row: Option<(String, i64, i64)> = conn
        .query_row(
            "SELECT owner_id, head_seq, min_retained_seq FROM vaults WHERE id = ?1",
            params![vault_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        None => Err(DbError::NotFound("文档库不存在")),
        Some((owner, _, _)) if owner != owner_id => Err(DbError::Forbidden("无权访问该文档库")),
        Some((_, head, min_seq)) => Ok((head as u64, min_seq as u64)),
    }
}

pub fn acquire_blob_lease(conn: &mut Connection, blob_hash: &str, expires_at: u64) -> Result<(), DbError> {
    ensure_primary_tx(conn)?;
    conn.execute(
        "INSERT INTO blob_leases (blob_hash, expires_at) VALUES (?1, ?2)
         ON CONFLICT(blob_hash) DO UPDATE SET expires_at = MAX(expires_at, excluded.expires_at)",
        params![blob_hash, expires_at as i64],
    )?;
    Ok(())
}

pub fn register_vault_blob(conn: &mut Connection, vault_id: &str, owner_id: &str, blob_hash: &str, size: u64) -> Result<(), DbError> {
    let now = unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    verify_vault_owner(&tx, owner_id, vault_id)?;
    tx.execute(
        "INSERT INTO vault_blobs (vault_id, blob_hash, size, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(vault_id, blob_hash) DO UPDATE SET size = excluded.size, created_at = excluded.created_at",
        params![vault_id, blob_hash, size as i64, now as i64],
    )?;
    tx.execute("DELETE FROM blob_leases WHERE blob_hash = ?1", params![blob_hash])?;
    append_binlog(
        &tx,
        &ReplEvent::VaultBlobRegister {
            vault_id: vault_id.into(),
            blob_hash: blob_hash.into(),
            size,
            created_at: now,
        },
    )?;
    tx.commit()?;
    Ok(())
}

pub fn is_blob_referenced_anywhere(conn: &Connection, blob_hash: &str, now: u64) -> Result<bool, DbError> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 WHERE
               EXISTS (SELECT 1 FROM vault_blobs WHERE blob_hash = ?1)
               OR EXISTS (SELECT 1 FROM entity_blobs WHERE blob_hash = ?1)
               OR EXISTS (SELECT 1 FROM op_log_blobs WHERE blob_hash = ?1)
               OR EXISTS (SELECT 1 FROM blob_leases WHERE blob_hash = ?1 AND expires_at >= ?2)",
            params![blob_hash, now as i64],
            |r| r.get(0),
        )
        .optional()?;
    Ok(hit.is_some())
}

pub fn vault_has_blob(conn: &Connection, vault_id: &str, owner_id: &str, blob_hash: &str) -> Result<bool, DbError> {
    verify_vault_owner(conn, owner_id, vault_id)?;
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM vault_blobs WHERE vault_id = ?1 AND blob_hash = ?2",
            params![vault_id, blob_hash],
            |r| r.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

pub fn filter_existing_blobs(conn: &Connection, vault_id: &str, owner_id: &str, hashes: &[String]) -> Result<Vec<String>, DbError> {
    verify_vault_owner(conn, owner_id, vault_id)?;
    let mut out = Vec::new();
    let mut stmt = conn.prepare("SELECT 1 FROM vault_blobs WHERE vault_id = ?1 AND blob_hash = ?2")?;
    for h in hashes {
        if stmt.query_row(params![vault_id, h], |r| r.get::<_, i64>(0)).optional()?.is_some() {
            out.push(h.clone());
        }
    }
    Ok(out)
}

pub fn push_ops(
    conn: &mut Connection,
    vault_id: &str,
    owner_id: &str,
    device_id: &str,
    ops: Vec<PushOp>,
) -> Result<(u64, Vec<PushItemResult>, Vec<RemoteChange>), DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !is_device_active(&tx, owner_id, device_id)? {
        return Err(DbError::Forbidden("当前设备已被移除或已注销"));
    }
    let row: Option<(String, i64, i64)> = tx
        .query_row("SELECT owner_id, head_seq, e2ee_enabled FROM vaults WHERE id = ?1", params![vault_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let (mut head_seq, e2ee_enabled) = match row {
        None => return Err(DbError::NotFound("文档库不存在")),
        Some((owner, _, _)) if owner != owner_id => return Err(DbError::Forbidden("无权访问该文档库")),
        Some((_, seq, e2ee)) => (seq as u64, e2ee != 0),
    };

    let now = unix_secs();
    let mut results = Vec::with_capacity(ops.len());
    let mut broadcasts = Vec::new();
    let mut repl_items = Vec::new();

    for op in ops {
        if op.entity_id.is_empty() || op.entity_id.len() > 256 {
            return Err(DbError::BadRequest("无效的实体标识"));
        }
        if op.blob_hashes.len() > 64 || op.blob_hashes.iter().any(|h| crate::blob::validate_hash(h).is_err()) {
            return Err(DbError::BadRequest("无效的 Blob 哈希列表"));
        }
        if e2ee_enabled {
            if !matches!(op.envelope, Some(Envelope::Encrypted { .. })) {
                return Err(DbError::BadRequest("加密文档库只接受加密信封"));
            }
        } else if (!op.deleted && !matches!(op.envelope, Some(Envelope::Plain { .. })))
            || matches!(op.envelope, Some(Envelope::Encrypted { .. }))
        {
            return Err(DbError::BadRequest("明文文档库不能接受加密信封或空数据"));
        }

        let mut missing = Vec::new();
        for h in &op.blob_hashes {
            let ok: Option<i64> = tx
                .query_row("SELECT 1 FROM vault_blobs WHERE vault_id = ?1 AND blob_hash = ?2", params![vault_id, h], |r| r.get(0))
                .optional()?;
            if ok.is_none() {
                missing.push(h.clone());
            }
        }
        if !missing.is_empty() {
            results.push(PushItemResult::MissingBlobs { entity_id: op.entity_id, missing });
            continue;
        }

        let existing: Option<(String, i64, i64, i64, Option<Vec<u8>>, i64, String)> = tx
            .query_row(
                "SELECT kind, version, seq, deleted, envelope, updated_at, device_id FROM entities WHERE vault_id = ?1 AND entity_id = ?2",
                params![vault_id, &op.entity_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .optional()?;

        let cur_ver = existing.as_ref().map_or(0u64, |r| r.1 as u64);
        if op.expected_version != cur_ver {
            if let Some((k_str, ver, seq, del, env_blob, upd, dev)) = existing {
                let kind = EntityKind::parse(&k_str).unwrap_or(op.kind);
                let envelope = env_blob.and_then(|b| serde_json::from_slice::<Envelope>(&b).ok());
                results.push(PushItemResult::Conflict {
                    entity_id: op.entity_id.clone(),
                    current: RemoteChange {
                        seq: seq as u64,
                        entity_id: op.entity_id,
                        kind,
                        version: ver as u64,
                        deleted: del != 0,
                        envelope,
                        updated_at: upd as u64,
                        device_id: dev,
                    },
                });
            } else {
                results.push(PushItemResult::StaleBase {
                    entity_id: op.entity_id,
                    head_seq,
                });
            }
            continue;
        }

        head_seq += 1;
        let next_ver = cur_ver + 1;
        let env_bytes = match &op.envelope {
            Some(env) => Some(serde_json::to_vec(env).map_err(|e| DbError::Internal(e.into()))?),
            None => None,
        };
        let kind_str = op.kind.as_str();
        let del_int = i32::from(op.deleted);

        tx.execute(
            "INSERT INTO entities (vault_id, entity_id, kind, version, seq, deleted, envelope, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(vault_id, entity_id) DO UPDATE SET
               kind = excluded.kind,
               version = excluded.version,
               seq = excluded.seq,
               deleted = excluded.deleted,
               envelope = excluded.envelope,
               updated_at = excluded.updated_at,
               device_id = excluded.device_id",
            params![vault_id, &op.entity_id, kind_str, next_ver as i64, head_seq as i64, del_int, env_bytes, now as i64, device_id],
        )?;

        tx.execute(
            "INSERT INTO op_log (vault_id, seq, entity_id, kind, version, deleted, envelope, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![vault_id, head_seq as i64, &op.entity_id, kind_str, next_ver as i64, del_int, env_bytes, now as i64, device_id],
        )?;

        tx.execute(
            "DELETE FROM entity_blobs WHERE vault_id = ?1 AND entity_id = ?2",
            params![vault_id, &op.entity_id],
        )?;
        if !op.deleted {
            for h in &op.blob_hashes {
                tx.execute(
                    "INSERT OR IGNORE INTO entity_blobs (vault_id, entity_id, blob_hash) VALUES (?1, ?2, ?3)",
                    params![vault_id, &op.entity_id, h],
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO op_log_blobs (vault_id, seq, blob_hash) VALUES (?1, ?2, ?3)",
                    params![vault_id, head_seq as i64, h],
                )?;
            }
        }

        results.push(PushItemResult::Accepted {
            entity_id: op.entity_id.clone(),
            version: next_ver,
            seq: head_seq,
        });
        repl_items.push(ReplPushItem {
            seq: head_seq,
            entity_id: op.entity_id.clone(),
            kind: kind_str.to_string(),
            version: next_ver,
            deleted: op.deleted,
            envelope_b64: env_bytes.as_ref().map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            updated_at: now,
            device_id: device_id.to_string(),
            blob_hashes: if op.deleted { Vec::new() } else { op.blob_hashes },
        });
        broadcasts.push(RemoteChange {
            seq: head_seq,
            entity_id: op.entity_id,
            kind: op.kind,
            version: next_ver,
            deleted: op.deleted,
            envelope: op.envelope,
            updated_at: now,
            device_id: device_id.to_string(),
        });
    }

    tx.execute("UPDATE vaults SET head_seq = ?1 WHERE id = ?2", params![head_seq as i64, vault_id])?;
    if !repl_items.is_empty() {
        append_binlog(
            &tx,
            &ReplEvent::PushBatch {
                vault_id: vault_id.to_string(),
                head_seq,
                device_id: device_id.to_string(),
                device_last_seen_at: now,
                items: repl_items,
            },
        )?;
    }
    tx.commit()?;
    Ok((head_seq, results, broadcasts))
}

pub fn get_changes(
    conn: &Connection,
    vault_id: &str,
    owner_id: &str,
    since_seq: u64,
    limit: u32,
) -> Result<(u64, Vec<RemoteChange>, bool, bool), DbError> {
    let (head_seq, min_retained) = verify_vault_owner(conn, owner_id, vault_id)?;
    if since_seq < min_retained {
        return Ok((head_seq, Vec::new(), false, true));
    }
    let cap = limit.clamp(1, 500) as i64;
    let mut stmt = conn.prepare(
        "SELECT seq, entity_id, kind, version, deleted, envelope, updated_at, device_id
         FROM op_log WHERE vault_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
    )?;
    let mut changes = Vec::new();
    let rows = stmt.query_map(params![vault_id, since_seq as i64, cap + 1], |r| {
        let env_blob: Option<Vec<u8>> = r.get(5)?;
        let kind_str: String = r.get(2)?;
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, String>(1)?,
            kind_str,
            r.get::<_, i64>(3)? as u64,
            r.get::<_, i64>(4)? != 0,
            env_blob,
            r.get::<_, i64>(6)? as u64,
            r.get::<_, String>(7)?,
        ))
    })?;
    for row in rows {
        let (seq, entity_id, kind_str, version, deleted, env_blob, updated_at, device_id) = row?;
        let Some(kind) = EntityKind::parse(&kind_str) else { continue };
        let envelope = env_blob.and_then(|b| serde_json::from_slice::<Envelope>(&b).ok());
        changes.push(RemoteChange { seq, entity_id, kind, version, deleted, envelope, updated_at, device_id });
    }
    let has_more = changes.len() > cap as usize;
    if has_more {
        changes.truncate(cap as usize);
    }
    Ok((head_seq, changes, has_more, false))
}

pub fn get_snapshot(conn: &Connection, vault_id: &str, owner_id: &str) -> Result<(u64, Vec<RemoteChange>), DbError> {
    let (head_seq, _) = verify_vault_owner(conn, owner_id, vault_id)?;
    let mut stmt = conn.prepare(
        "SELECT seq, entity_id, kind, version, deleted, envelope, updated_at, device_id
         FROM entities WHERE vault_id = ?1 AND deleted = 0 ORDER BY seq ASC",
    )?;
    let mut items = Vec::new();
    let rows = stmt.query_map(params![vault_id], |r| {
        let env_blob: Option<Vec<u8>> = r.get(5)?;
        let kind_str: String = r.get(2)?;
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, String>(1)?,
            kind_str,
            r.get::<_, i64>(3)? as u64,
            r.get::<_, i64>(4)? != 0,
            env_blob,
            r.get::<_, i64>(6)? as u64,
            r.get::<_, String>(7)?,
        ))
    })?;
    for row in rows {
        let (seq, entity_id, kind_str, version, deleted, env_blob, updated_at, device_id) = row?;
        let Some(kind) = EntityKind::parse(&kind_str) else { continue };
        let envelope = env_blob.and_then(|b| serde_json::from_slice::<Envelope>(&b).ok());
        items.push(RemoteChange { seq, entity_id, kind, version, deleted, envelope, updated_at, device_id });
    }
    Ok((head_seq, items))
}

/// Execute tombstone/op_log/vault_blob pruning inside an existing transaction.
/// Returns `(pruned_any, unlinked_hashes)`.
pub fn prune_old_tombstones_tx(
    tx: &rusqlite::Transaction<'_>,
    tombstone_cutoff: u64,
    upload_grace_cutoff: u64,
    now: u64,
) -> Result<(bool, HashSet<String>), DbError> {
    // Record the highest compacted seq per vault BEFORE deleting op_log / tombstone rows so
    // `min_retained_seq` monotonically tracks gaps even when older live seqs still exist.
    let mut compacted_watermarks: Vec<(String, i64)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT vault_id, MAX(seq) FROM op_log
             WHERE updated_at < ?1 AND NOT EXISTS (
                 SELECT 1 FROM entities e
                 WHERE e.vault_id = op_log.vault_id AND e.entity_id = op_log.entity_id AND e.seq = op_log.seq AND e.deleted = 0
             )
             GROUP BY vault_id",
        )?;
        for row in stmt.query_map(params![tombstone_cutoff as i64], |r| Ok((r.get(0)?, r.get(1)?)))? {
            compacted_watermarks.push(row?);
        }
    }
    {
        let mut stmt = tx.prepare(
            "SELECT vault_id, MAX(seq) FROM entities WHERE deleted = 1 AND updated_at < ?1 GROUP BY vault_id",
        )?;
        for row in stmt.query_map(params![tombstone_cutoff as i64], |r| Ok((r.get(0)?, r.get(1)?)))? {
            compacted_watermarks.push(row?);
        }
    }

    // Compact op_log entries that are already superseded by a later seq for the same entity and older than cutoff.
    let del_oplog = tx.execute(
        "DELETE FROM op_log WHERE updated_at < ?1 AND NOT EXISTS (
            SELECT 1 FROM entities e WHERE e.vault_id = op_log.vault_id AND e.entity_id = op_log.entity_id AND e.seq = op_log.seq AND e.deleted = 0
        )",
        params![tombstone_cutoff as i64],
    )?;
    let del_ent = tx.execute("DELETE FROM entities WHERE deleted = 1 AND updated_at < ?1", params![tombstone_cutoff as i64])?;
    for (vid, max_seq) in compacted_watermarks {
        tx.execute(
            "UPDATE vaults SET min_retained_seq = MAX(min_retained_seq, ?1) WHERE id = ?2",
            params![max_seq, vid],
        )?;
    }

    tx.execute("DELETE FROM blob_leases WHERE expires_at < ?1", params![now as i64])?;

    let mut unlinked_hashes: HashSet<String> = HashSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT blob_hash FROM vault_blobs vb
             WHERE vb.created_at <= ?1
               AND NOT EXISTS (SELECT 1 FROM entity_blobs eb WHERE eb.vault_id = vb.vault_id AND eb.blob_hash = vb.blob_hash)
               AND NOT EXISTS (SELECT 1 FROM op_log_blobs ob WHERE ob.vault_id = vb.vault_id AND ob.blob_hash = vb.blob_hash)",
        )?;
        for r in stmt.query_map(params![upload_grace_cutoff as i64], |r| r.get::<_, String>(0))? {
            unlinked_hashes.insert(r?);
        }
    }
    let del_vb = tx.execute(
        "DELETE FROM vault_blobs
         WHERE created_at <= ?1
           AND NOT EXISTS (SELECT 1 FROM entity_blobs eb WHERE eb.vault_id = vault_blobs.vault_id AND eb.blob_hash = vault_blobs.blob_hash)
           AND NOT EXISTS (SELECT 1 FROM op_log_blobs ob WHERE ob.vault_id = vault_blobs.vault_id AND ob.blob_hash = vault_blobs.blob_hash)",
        params![upload_grace_cutoff as i64],
    )?;

    let pruned_any = del_oplog > 0 || del_ent > 0 || del_vb > 0;
    Ok((pruned_any, unlinked_hashes))
}

/// Remove expired tombstones, reclaim unreferenced vault blobs, and return candidate orphan blob hashes.
pub fn prune_old_tombstones_and_find_orphan_blobs(
    conn: &mut Connection,
    tombstone_cutoff: u64,
    upload_grace_cutoff: u64,
    all_stored_hashes: &[String],
) -> Result<Vec<String>, DbError> {
    let now = unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (pruned_any, unlinked_hashes) = prune_old_tombstones_tx(&tx, tombstone_cutoff, upload_grace_cutoff, now)?;
    if pruned_any {
        append_binlog(&tx, &ReplEvent::GcCompact { tombstone_cutoff, upload_grace_cutoff })?;
    }

    let mut live_hashes = HashSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT blob_hash FROM vault_blobs
             UNION
             SELECT blob_hash FROM entity_blobs
             UNION
             SELECT blob_hash FROM op_log_blobs
             UNION
             SELECT blob_hash FROM blob_leases WHERE expires_at >= ?1",
        )?;
        let rows = stmt.query_map(params![now as i64], |r| r.get::<_, String>(0))?;
        for h in rows {
            live_hashes.insert(h?);
        }
    }
    tx.commit()?;

    let mut candidates: HashSet<String> = all_stored_hashes.iter().cloned().collect();
    candidates.extend(unlinked_hashes);
    Ok(candidates.into_iter().filter(|h| !live_hashes.contains(h.as_str())).collect())
}
