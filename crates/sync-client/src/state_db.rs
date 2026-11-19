use anyhow::{Context, Result};
use quiet_sync_proto::entity::{compress_bytes, decompress_bytes, EntityKind, EntityPayload};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::scope::SyncScope;

pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 30;
pub const MAX_SYNC_INTERVAL_SECS: u64 = 3600;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseSnapshot {
    pub payload: EntityPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct EntityStateRow {
    pub entity_id: String,
    pub kind: EntityKind,
    pub server_version: u64,
    pub base_hash: String,
    pub base_blob: Option<Vec<u8>>,
    pub deleted: bool,
    pub updated_at: u64,
}

impl EntityStateRow {
    pub fn decode_base(&self) -> Result<Option<BaseSnapshot>> {
        let Some(blob) = &self.base_blob else { return Ok(None) };
        let json = decompress_bytes(blob)?;
        Ok(Some(serde_json::from_slice(&json)?))
    }
}

pub fn encode_base(snap: &BaseSnapshot) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(snap)?;
    Ok(compress_bytes(&json))
}

#[derive(Clone, Debug)]
pub struct OutboxItem {
    pub entity_id: String,
    pub kind: EntityKind,
    pub deleted: bool,
    pub queued_at: u64,
    pub retry_count: u32,
}

#[derive(Clone)]
pub struct StateDb {
    conn: Arc<Mutex<Connection>>,
}

impl StateDb {
    pub fn open(sync_dir: &Path) -> Result<Self> {
        crate::credentials::ensure_private_dir(sync_dir)?;
        let path = sync_dir.join("state.db");
        let conn = Connection::open(&path).context("open sync client state db")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS entity_state (
                 entity_id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 server_version INTEGER NOT NULL,
                 base_hash TEXT NOT NULL,
                 base_blob BLOB,
                 deleted INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS outbox (
                 entity_id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 deleted INTEGER NOT NULL DEFAULT 0,
                 queued_at INTEGER NOT NULL,
                 retry_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS synced_blobs (
                 blob_hash TEXT PRIMARY KEY,
                 rel_path TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 synced_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS asset_map (
                 entity_id TEXT PRIMARY KEY,
                 rel_path TEXT NOT NULL
             );",
        )?;
        crate::credentials::harden_private_file(&path)?;
        for suffix in ["-wal", "-shm"] {
            let aux = Path::new(&format!("{}{suffix}", path.display())).to_path_buf();
            if aux.exists() {
                crate::credentials::harden_private_file(&aux)?;
            }
        }
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn head_seq(&self) -> u64 {
        self.get_meta("head_seq").and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn set_head_seq(&self, seq: u64) -> Result<()> {
        self.set_meta("head_seq", &seq.to_string())
    }

    pub fn vault_id(&self) -> Option<String> {
        self.get_meta("vault_id").filter(|s| !s.is_empty())
    }

    pub fn set_vault_id(&self, id: &str) -> Result<()> {
        self.set_meta("vault_id", id)
    }

    pub fn paused(&self) -> bool {
        self.get_meta("paused").as_deref() == Some("1")
    }

    pub fn set_paused(&self, paused: bool) -> Result<()> {
        self.set_meta("paused", if paused { "1" } else { "0" })
    }

    pub fn scope(&self) -> SyncScope {
        let read_bool = |k: &str| self.get_meta(k).map(|v| v != "0").unwrap_or(true);
        SyncScope {
            trash: read_bool("scope_trash"),
            history: read_bool("scope_history"),
            settings: read_bool("scope_settings"),
        }
    }

    pub fn set_scope(&self, scope: SyncScope) -> Result<()> {
        self.set_meta("scope_trash", if scope.trash { "1" } else { "0" })?;
        self.set_meta("scope_history", if scope.history { "1" } else { "0" })?;
        self.set_meta("scope_settings", if scope.settings { "1" } else { "0" })?;
        Ok(())
    }

    pub fn sync_interval_secs(&self) -> u64 {
        self.get_meta("sync_interval_secs")
            .and_then(|s| s.parse::<u64>().ok())
            .map(|s| s.min(MAX_SYNC_INTERVAL_SECS))
            .unwrap_or(DEFAULT_SYNC_INTERVAL_SECS)
    }

    pub fn set_sync_interval_secs(&self, secs: u64) -> Result<()> {
        self.set_meta("sync_interval_secs", &secs.min(MAX_SYNC_INTERVAL_SECS).to_string())
    }

    pub fn get_entity(&self, entity_id: &str) -> Result<Option<EntityStateRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT entity_id, kind, server_version, base_hash, base_blob, deleted, updated_at FROM entity_state WHERE entity_id = ?1",
                params![entity_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<Vec<u8>>>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, i64>(6)? as u64,
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(entity_id, kind_str, server_version, base_hash, base_blob, deleted, updated_at)| {
            Some(EntityStateRow {
                entity_id,
                kind: EntityKind::parse(&kind_str)?,
                server_version,
                base_hash,
                base_blob,
                deleted,
                updated_at,
            })
        }))
    }

    pub fn all_entities(&self) -> Result<HashMap<String, EntityStateRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT entity_id, kind, server_version, base_hash, base_blob, deleted, updated_at FROM entity_state",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, String>(3)?,
                r.get::<_, Option<Vec<u8>>>(4)?,
                r.get::<_, i64>(5)? != 0,
                r.get::<_, i64>(6)? as u64,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (entity_id, kind_str, server_version, base_hash, base_blob, deleted, updated_at) = row?;
            if let Some(kind) = EntityKind::parse(&kind_str) {
                out.insert(
                    entity_id.clone(),
                    EntityStateRow { entity_id, kind, server_version, base_hash, base_blob, deleted, updated_at },
                );
            }
        }
        Ok(out)
    }

    pub fn upsert_entity(&self, row: &EntityStateRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO entity_state (entity_id, kind, server_version, base_hash, base_blob, deleted, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(entity_id) DO UPDATE SET
               kind = excluded.kind,
               server_version = excluded.server_version,
               base_hash = excluded.base_hash,
               base_blob = excluded.base_blob,
               deleted = excluded.deleted,
               updated_at = excluded.updated_at",
            params![
                row.entity_id,
                row.kind.as_str(),
                row.server_version as i64,
                row.base_hash,
                row.base_blob,
                i32::from(row.deleted),
                row.updated_at as i64
            ],
        )?;
        Ok(())
    }

    pub fn enqueue_outbox(&self, entity_id: &str, kind: EntityKind, deleted: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO outbox (entity_id, kind, deleted, queued_at, retry_count)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(entity_id) DO UPDATE SET
               kind = excluded.kind,
               deleted = excluded.deleted,
               queued_at = excluded.queued_at",
            params![entity_id, kind.as_str(), i32::from(deleted), now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn list_outbox(&self) -> Result<Vec<OutboxItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT entity_id, kind, deleted, queued_at, retry_count FROM outbox ORDER BY queued_at ASC")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, i64>(4)? as u32,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (entity_id, kind_str, deleted, queued_at, retry_count) = row?;
            if let Some(kind) = EntityKind::parse(&kind_str) {
                out.push(OutboxItem { entity_id, kind, deleted, queued_at, retry_count });
            }
        }
        Ok(out)
    }

    pub fn list_outbox_for_scope(&self, scope: SyncScope) -> Result<Vec<OutboxItem>> {
        let items = self.list_outbox()?;
        Ok(items.into_iter().filter(|i| scope.allows(i.kind)).collect())
    }

    pub fn outbox_count(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn outbox_count_for_scope(&self, scope: SyncScope) -> usize {
        self.list_outbox_for_scope(scope).map_or(0, |v| v.len())
    }

    pub fn remove_outbox(&self, entity_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM outbox WHERE entity_id = ?1", params![entity_id])?;
        Ok(())
    }

    pub fn bump_outbox_retry(&self, entity_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE outbox SET retry_count = retry_count + 1 WHERE entity_id = ?1", params![entity_id])?;
        Ok(())
    }

    pub fn is_blob_synced(&self, hash: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT 1 FROM synced_blobs WHERE blob_hash = ?1", params![hash], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn mark_blob_synced(&self, hash: &str, rel_path: &str, size: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO synced_blobs (blob_hash, rel_path, size, synced_at) VALUES (?1, ?2, ?3, ?4)",
            params![hash, rel_path, size as i64, now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn mark_blob_unsynced(&self, hash: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM synced_blobs WHERE blob_hash = ?1", params![hash])?;
        Ok(())
    }

    pub fn get_asset_rel(&self, entity_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT rel_path FROM asset_map WHERE entity_id = ?1", params![entity_id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn all_asset_maps(&self) -> Result<HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT entity_id, rel_path FROM asset_map")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (eid, rel) = row?;
            out.insert(eid, rel);
        }
        Ok(out)
    }

    pub fn commit_remote_batch(
        &self,
        upserts: &[EntityStateRow],
        remove_outbox: &[String],
        enqueue_outbox: &[(String, EntityKind, bool)],
        asset_maps: &[(String, Option<String>)],
        blob_marks: &[(String, String, u64)],
        new_head_seq: Option<u64>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for row in upserts {
            tx.execute(
                "INSERT INTO entity_state (entity_id, kind, server_version, base_hash, base_blob, deleted, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(entity_id) DO UPDATE SET
                   kind = excluded.kind,
                   server_version = excluded.server_version,
                   base_hash = excluded.base_hash,
                   base_blob = excluded.base_blob,
                   deleted = excluded.deleted,
                   updated_at = excluded.updated_at",
                params![
                    row.entity_id,
                    row.kind.as_str(),
                    row.server_version as i64,
                    row.base_hash,
                    row.base_blob,
                    i32::from(row.deleted),
                    row.updated_at as i64
                ],
            )?;
        }
        for eid in remove_outbox {
            tx.execute("DELETE FROM outbox WHERE entity_id = ?1", params![eid])?;
        }
        let now = now_ms() as i64;
        for (eid, kind, deleted) in enqueue_outbox {
            tx.execute(
                "INSERT INTO outbox (entity_id, kind, deleted, queued_at, retry_count)
                 VALUES (?1, ?2, ?3, ?4, 0)
                 ON CONFLICT(entity_id) DO UPDATE SET
                   kind = excluded.kind,
                   deleted = excluded.deleted,
                   queued_at = excluded.queued_at",
                params![eid, kind.as_str(), i32::from(*deleted), now],
            )?;
        }
        for (eid, rel) in asset_maps {
            match rel {
                Some(r) => {
                    tx.execute(
                        "INSERT INTO asset_map (entity_id, rel_path) VALUES (?1, ?2)
                         ON CONFLICT(entity_id) DO UPDATE SET rel_path = excluded.rel_path",
                        params![eid, r],
                    )?;
                }
                None => {
                    tx.execute("DELETE FROM asset_map WHERE entity_id = ?1", params![eid])?;
                }
            }
        }
        for (hash, rel_path, size) in blob_marks {
            tx.execute(
                "INSERT OR REPLACE INTO synced_blobs (blob_hash, rel_path, size, synced_at) VALUES (?1, ?2, ?3, ?4)",
                params![hash, rel_path, *size as i64, now],
            )?;
        }
        if let Some(seq) = new_head_seq {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('head_seq', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![seq.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_outbox_for_unlink(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM outbox;
             INSERT INTO meta (key, value) VALUES ('paused', '0')
               ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
        )?;
        Ok(())
    }

    pub fn reset_vault_state(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM meta WHERE key IN ('head_seq', 'vault_id', 'paused');
             DELETE FROM entity_state;
             DELETE FROM outbox;
             DELETE FROM synced_blobs;
             DELETE FROM asset_map;",
        )?;
        Ok(())
    }
}
