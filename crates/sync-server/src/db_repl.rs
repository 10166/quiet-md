use base64::Engine;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use uuid::Uuid;

use crate::db::{self, DbError, NodeRole, ReplEvent};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BinlogRecord {
    pub lsn: u64,
    pub ts: u64,
    pub event: ReplEvent,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplicaRow {
    pub replica_id: String,
    pub name: String,
    pub portal_url: String,
    #[serde(skip_serializing)]
    pub token_hash: String,
    pub status: String,
    pub applied_lsn: u64,
    pub blob_pending: u64,
    pub storage_bytes: u64,
    pub server_version: String,
    pub remote_addr: String,
    pub last_seen_at: u64,
    pub last_error: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamRow {
    pub primary_url: String,
    pub primary_id: String,
    pub primary_name: String,
    pub primary_portal_url: String,
    pub primary_epoch: String,
    #[serde(skip_serializing)]
    pub replica_token: String,
    pub bootstrapped: bool,
    pub applied_lsn: u64,
    pub primary_head_lsn: u64,
    pub state: String,
    pub last_sync_at: u64,
    pub last_error: String,
    pub configured_at: u64,
}

// ---- Snapshot NDJSON types ----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapUser {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub password_hash: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapDevice {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub platform: String,
    pub refresh_hash: String,
    pub refresh_expires_at: u64,
    pub last_seen_at: u64,
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapVault {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub head_seq: u64,
    pub e2ee_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_meta: Option<String>,
    pub min_retained_seq: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapEntity {
    pub vault_id: String,
    pub entity_id: String,
    pub kind: String,
    pub version: u64,
    pub seq: u64,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_b64: Option<String>,
    pub updated_at: u64,
    pub device_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapOpLog {
    pub vault_id: String,
    pub seq: u64,
    pub entity_id: String,
    pub kind: String,
    pub version: u64,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_b64: Option<String>,
    pub updated_at: u64,
    pub device_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapVaultBlob {
    pub vault_id: String,
    pub blob_hash: String,
    pub size: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapEntityBlob {
    pub vault_id: String,
    pub entity_id: String,
    pub blob_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapOpLogBlob {
    pub vault_id: String,
    pub seq: u64,
    pub blob_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SnapshotLine {
    Header {
        primary_id: String,
        primary_name: String,
        primary_portal_url: String,
        epoch: String,
        snapshot_lsn: u64,
    },
    Users { rows: Vec<SnapUser> },
    Devices { rows: Vec<SnapDevice> },
    Vaults { rows: Vec<SnapVault> },
    Entities { rows: Vec<SnapEntity> },
    OpLogs { rows: Vec<SnapOpLog> },
    VaultBlobs { rows: Vec<SnapVaultBlob> },
    EntityBlobs { rows: Vec<SnapEntityBlob> },
    OpLogBlobs { rows: Vec<SnapOpLogBlob> },
    End { snapshot_lsn: u64 },
}

// ---- Primary side: replicas & binlog ----

pub fn register_replica(
    conn: &mut Connection,
    replica_id: &str,
    name: &str,
    portal_url: &str,
    token_hash: &str,
    server_version: &str,
    remote_addr: &str,
) -> Result<ReplicaRow, DbError> {
    let now = db::unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO repl_replicas
           (replica_id, name, portal_url, token_hash, status, applied_lsn, blob_pending, storage_bytes, server_version, remote_addr, last_seen_at, last_error, created_at)
         VALUES (?1, ?2, ?3, ?4, 'active', 0, 0, 0, ?5, ?6, ?7, '', ?7)
         ON CONFLICT(replica_id) DO UPDATE SET
           name = excluded.name,
           portal_url = excluded.portal_url,
           token_hash = excluded.token_hash,
           status = 'active',
           server_version = excluded.server_version,
           remote_addr = excluded.remote_addr,
           last_seen_at = excluded.last_seen_at,
           last_error = ''",
        params![replica_id, name, portal_url.trim_end_matches('/'), token_hash, server_version, remote_addr, now as i64],
    )?;
    let row = get_replica(&tx, replica_id)?.ok_or(DbError::Internal(anyhow::anyhow!("registered replica missing")))?;
    tx.commit()?;
    Ok(row)
}

pub fn get_replica(conn: &Connection, replica_id: &str) -> Result<Option<ReplicaRow>, DbError> {
    Ok(conn
        .query_row(
            "SELECT replica_id, name, portal_url, token_hash, status, applied_lsn, blob_pending,
                    storage_bytes, server_version, remote_addr, last_seen_at, last_error, created_at
             FROM repl_replicas WHERE replica_id = ?1",
            params![replica_id],
            map_replica_row,
        )
        .optional()?)
}

pub fn list_replicas(conn: &Connection, include_evicted: bool) -> Result<Vec<ReplicaRow>, DbError> {
    let sql = if include_evicted {
        "SELECT replica_id, name, portal_url, token_hash, status, applied_lsn, blob_pending,
                storage_bytes, server_version, remote_addr, last_seen_at, last_error, created_at
         FROM repl_replicas ORDER BY status ASC, created_at DESC"
    } else {
        "SELECT replica_id, name, portal_url, token_hash, status, applied_lsn, blob_pending,
                storage_bytes, server_version, remote_addr, last_seen_at, last_error, created_at
         FROM repl_replicas WHERE status = 'active' ORDER BY created_at DESC"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], map_replica_row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn map_replica_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ReplicaRow> {
    Ok(ReplicaRow {
        replica_id: r.get(0)?,
        name: r.get(1)?,
        portal_url: r.get(2)?,
        token_hash: r.get(3)?,
        status: r.get(4)?,
        applied_lsn: r.get::<_, i64>(5)? as u64,
        blob_pending: r.get::<_, i64>(6)? as u64,
        storage_bytes: r.get::<_, i64>(7)? as u64,
        server_version: r.get(8)?,
        remote_addr: r.get(9)?,
        last_seen_at: r.get::<_, i64>(10)? as u64,
        last_error: r.get(11)?,
        created_at: r.get::<_, i64>(12)? as u64,
    })
}

pub fn update_replica_heartbeat(
    conn: &mut Connection,
    replica_id: &str,
    name: Option<&str>,
    portal_url: Option<&str>,
    applied_lsn: u64,
    blob_pending: u64,
    storage_bytes: u64,
    server_version: Option<&str>,
    remote_addr: Option<&str>,
) -> Result<bool, DbError> {
    let now = db::unix_secs() as i64;
    let n = conn.execute(
        "UPDATE repl_replicas SET
           name = COALESCE(NULLIF(?2, ''), name),
           portal_url = COALESCE(?3, portal_url),
           applied_lsn = ?4,
           blob_pending = ?5,
           storage_bytes = ?6,
           server_version = COALESCE(NULLIF(?7, ''), server_version),
           remote_addr = COALESCE(NULLIF(?8, ''), remote_addr),
           last_seen_at = ?9,
           last_error = ''
         WHERE replica_id = ?1 AND status = 'active'",
        params![
            replica_id,
            name.unwrap_or(""),
            portal_url.map(|u| u.trim().trim_end_matches('/')),
            applied_lsn as i64,
            blob_pending as i64,
            storage_bytes as i64,
            server_version.unwrap_or(""),
            remote_addr.unwrap_or(""),
            now,
        ],
    )?;
    Ok(n > 0)
}

pub fn evict_replica(conn: &mut Connection, replica_id: &str) -> Result<bool, DbError> {
    let n = conn.execute(
        "UPDATE repl_replicas SET status = 'evicted' WHERE replica_id = ?1 AND status != 'evicted'",
        params![replica_id],
    )?;
    Ok(n > 0)
}

pub fn purge_evicted_replica(conn: &mut Connection, replica_id: &str) -> Result<bool, DbError> {
    let n = conn.execute(
        "DELETE FROM repl_replicas WHERE replica_id = ?1 AND status = 'evicted'",
        params![replica_id],
    )?;
    Ok(n > 0)
}

/// Read incremental binlog events after `since_lsn`.
/// Returns `(head_lsn, min_retained_lsn, records, has_more, resync_required)`.
pub fn read_binlog_since(
    conn: &Connection,
    since_lsn: u64,
    limit: u32,
) -> Result<(u64, u64, Vec<BinlogRecord>, bool, bool), DbError> {
    let cfg = db::get_node_config(conn)?;
    let head_lsn = db::get_head_lsn(conn)?;
    if since_lsn < cfg.min_retained_lsn || since_lsn > head_lsn {
        return Ok((head_lsn, cfg.min_retained_lsn, Vec::new(), false, true));
    }
    let cap = limit.clamp(1, 500) as i64;
    let mut stmt = conn.prepare(
        "SELECT lsn, ts, payload FROM repl_binlog WHERE lsn > ?1 ORDER BY lsn ASC LIMIT ?2",
    )?;
    let mut out = Vec::new();
    let rows = stmt.query_map(params![since_lsn as i64, cap + 1], |r| {
        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, Vec<u8>>(2)?))
    })?;
    for row in rows {
        let (lsn, ts, payload) = row?;
        let event: ReplEvent = serde_json::from_slice(&payload)
            .map_err(|e| DbError::Internal(anyhow::anyhow!("corrupt binlog payload at lsn {lsn}: {e}")))?;
        out.push(BinlogRecord { lsn, ts, event });
    }
    let has_more = out.len() > cap as usize;
    if has_more {
        out.truncate(cap as usize);
    }
    Ok((head_lsn, cfg.min_retained_lsn, out, has_more, false))
}

/// Compact old binlog entries beyond `cutoff_ts` (or exceeding `max_rows`), updating `min_retained_lsn`.
pub fn compact_binlog(conn: &mut Connection, cutoff_ts: u64, max_rows: u64) -> Result<u64, DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut compacted_lsn: Option<i64> = tx
        .query_row("SELECT MAX(lsn) FROM repl_binlog WHERE ts < ?1", params![cutoff_ts as i64], |r| r.get(0))
        .optional()?
        .flatten();
    let mut deleted = tx.execute("DELETE FROM repl_binlog WHERE ts < ?1", params![cutoff_ts as i64])? as u64;

    let count: i64 = tx.query_row("SELECT COUNT(*) FROM repl_binlog", [], |r| r.get(0))?;
    if count as u64 > max_rows {
        let overflow = count - max_rows as i64;
        if let Some(cut_lsn) = tx
            .query_row(
                "SELECT lsn FROM repl_binlog ORDER BY lsn ASC LIMIT 1 OFFSET ?1",
                params![overflow - 1],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            deleted += tx.execute("DELETE FROM repl_binlog WHERE lsn <= ?1", params![cut_lsn])? as u64;
            compacted_lsn = Some(compacted_lsn.map_or(cut_lsn, |prev| prev.max(cut_lsn)));
        }
    }

    if let Some(max_del) = compacted_lsn {
        tx.execute(
            "UPDATE node_config SET min_retained_lsn = MAX(min_retained_lsn, ?1) WHERE id = 1",
            params![max_del],
        )?;
    }
    tx.commit()?;
    Ok(deleted)
}

/// Stream a point-in-time WAL-consistent NDJSON snapshot from a dedicated read-only connection.
pub fn export_snapshot_stream(
    ro_conn: &mut Connection,
    fallback_portal_url: &str,
    tx: tokio::sync::mpsc::Sender<Result<String, std::io::Error>>,
) -> anyhow::Result<()> {
    let send_line = |line: &SnapshotLine| -> anyhow::Result<bool> {
        let mut s = serde_json::to_string(line)?;
        s.push('\n');
        Ok(tx.blocking_send(Ok(s)).is_ok())
    };

    let sql_tx = ro_conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let cfg = db::get_node_config(&sql_tx).map_err(|e| anyhow::anyhow!("read node_config: {e:?}"))?;
    let snapshot_lsn = db::get_head_lsn(&sql_tx).map_err(|e| anyhow::anyhow!("read head_lsn: {e:?}"))?;
    let portal_url = if cfg.portal_url.is_empty() { fallback_portal_url.to_string() } else { cfg.portal_url };

    if !send_line(&SnapshotLine::Header {
        primary_id: cfg.node_id,
        primary_name: cfg.node_name,
        primary_portal_url: portal_url,
        epoch: cfg.repl_epoch,
        snapshot_lsn,
    })? {
        return Ok(());
    }

    const BATCH: usize = 100;
    let b64 = base64::engine::general_purpose::STANDARD;

    // 1. users
    {
        let mut stmt = sql_tx.prepare("SELECT id, email, display_name, password_hash, created_at FROM users")?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            Ok(SnapUser {
                id: r.get(0)?,
                email: r.get(1)?,
                display_name: r.get(2)?,
                password_hash: r.get(3)?,
                created_at: r.get::<_, i64>(4)? as u64,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::Users { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::Users { rows: buf })? {
            return Ok(());
        }
    }

    // 2. devices
    {
        let mut stmt = sql_tx.prepare(
            "SELECT id, user_id, name, platform, refresh_hash, refresh_expires_at, last_seen_at, revoked FROM devices",
        )?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            Ok(SnapDevice {
                id: r.get(0)?,
                user_id: r.get(1)?,
                name: r.get(2)?,
                platform: r.get(3)?,
                refresh_hash: r.get(4)?,
                refresh_expires_at: r.get::<_, i64>(5)? as u64,
                last_seen_at: r.get::<_, i64>(6)? as u64,
                revoked: r.get::<_, i64>(7)? != 0,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::Devices { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::Devices { rows: buf })? {
            return Ok(());
        }
    }

    // 3. vaults
    {
        let mut stmt = sql_tx.prepare(
            "SELECT id, owner_id, name, head_seq, e2ee_enabled, e2ee_meta, min_retained_seq, created_at FROM vaults",
        )?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            Ok(SnapVault {
                id: r.get(0)?,
                owner_id: r.get(1)?,
                name: r.get(2)?,
                head_seq: r.get::<_, i64>(3)? as u64,
                e2ee_enabled: r.get::<_, i64>(4)? != 0,
                e2ee_meta: r.get(5)?,
                min_retained_seq: r.get::<_, i64>(6)? as u64,
                created_at: r.get::<_, i64>(7)? as u64,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::Vaults { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::Vaults { rows: buf })? {
            return Ok(());
        }
    }

    // 4. entities
    {
        let mut stmt = sql_tx.prepare(
            "SELECT vault_id, entity_id, kind, version, seq, deleted, envelope, updated_at, device_id FROM entities",
        )?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            let env: Option<Vec<u8>> = r.get(6)?;
            Ok(SnapEntity {
                vault_id: r.get(0)?,
                entity_id: r.get(1)?,
                kind: r.get(2)?,
                version: r.get::<_, i64>(3)? as u64,
                seq: r.get::<_, i64>(4)? as u64,
                deleted: r.get::<_, i64>(5)? != 0,
                envelope_b64: env.map(|v| b64.encode(v)),
                updated_at: r.get::<_, i64>(7)? as u64,
                device_id: r.get(8)?,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::Entities { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::Entities { rows: buf })? {
            return Ok(());
        }
    }

    // 5. op_log
    {
        let mut stmt = sql_tx.prepare(
            "SELECT vault_id, seq, entity_id, kind, version, deleted, envelope, updated_at, device_id FROM op_log",
        )?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            let env: Option<Vec<u8>> = r.get(6)?;
            Ok(SnapOpLog {
                vault_id: r.get(0)?,
                seq: r.get::<_, i64>(1)? as u64,
                entity_id: r.get(2)?,
                kind: r.get(3)?,
                version: r.get::<_, i64>(4)? as u64,
                deleted: r.get::<_, i64>(5)? != 0,
                envelope_b64: env.map(|v| b64.encode(v)),
                updated_at: r.get::<_, i64>(7)? as u64,
                device_id: r.get(8)?,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::OpLogs { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::OpLogs { rows: buf })? {
            return Ok(());
        }
    }

    // 6. vault_blobs
    {
        let mut stmt = sql_tx.prepare("SELECT vault_id, blob_hash, size, created_at FROM vault_blobs")?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            Ok(SnapVaultBlob {
                vault_id: r.get(0)?,
                blob_hash: r.get(1)?,
                size: r.get::<_, i64>(2)? as u64,
                created_at: r.get::<_, i64>(3)? as u64,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::VaultBlobs { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::VaultBlobs { rows: buf })? {
            return Ok(());
        }
    }

    // 7. entity_blobs
    {
        let mut stmt = sql_tx.prepare("SELECT vault_id, entity_id, blob_hash FROM entity_blobs")?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            Ok(SnapEntityBlob {
                vault_id: r.get(0)?,
                entity_id: r.get(1)?,
                blob_hash: r.get(2)?,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::EntityBlobs { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::EntityBlobs { rows: buf })? {
            return Ok(());
        }
    }

    // 8. op_log_blobs
    {
        let mut stmt = sql_tx.prepare("SELECT vault_id, seq, blob_hash FROM op_log_blobs")?;
        let mut buf = Vec::with_capacity(BATCH);
        let rows = stmt.query_map([], |r| {
            Ok(SnapOpLogBlob {
                vault_id: r.get(0)?,
                seq: r.get::<_, i64>(1)? as u64,
                blob_hash: r.get(2)?,
            })
        })?;
        for row in rows {
            buf.push(row?);
            if buf.len() >= BATCH && !send_line(&SnapshotLine::OpLogBlobs { rows: std::mem::take(&mut buf) })? {
                return Ok(());
            }
        }
        if !buf.is_empty() && !send_line(&SnapshotLine::OpLogBlobs { rows: buf })? {
            return Ok(());
        }
    }

    let _ = send_line(&SnapshotLine::End { snapshot_lsn })?;
    Ok(())
}

// ---- Backup side: upstream, snapshot apply, binlog replay, pending blobs ----

pub fn get_upstream(conn: &Connection) -> Result<Option<UpstreamRow>, DbError> {
    Ok(conn
        .query_row(
            "SELECT primary_url, primary_id, primary_name, primary_portal_url, primary_epoch,
                    replica_token, bootstrapped, applied_lsn, primary_head_lsn, state,
                    last_sync_at, last_error, configured_at
             FROM repl_upstream WHERE id = 1",
            [],
            |r| {
                Ok(UpstreamRow {
                    primary_url: r.get(0)?,
                    primary_id: r.get(1)?,
                    primary_name: r.get(2)?,
                    primary_portal_url: r.get(3)?,
                    primary_epoch: r.get(4)?,
                    replica_token: r.get(5)?,
                    bootstrapped: r.get::<_, i64>(6)? != 0,
                    applied_lsn: r.get::<_, i64>(7)? as u64,
                    primary_head_lsn: r.get::<_, i64>(8)? as u64,
                    state: r.get(9)?,
                    last_sync_at: r.get::<_, i64>(10)? as u64,
                    last_error: r.get(11)?,
                    configured_at: r.get::<_, i64>(12)? as u64,
                })
            },
        )
        .optional()?)
}

pub fn configure_backup(
    conn: &mut Connection,
    primary_url: &str,
    primary_id: &str,
    primary_name: &str,
    primary_portal_url: &str,
    primary_epoch: &str,
    replica_token: &str,
    primary_head_lsn: u64,
    node_name: Option<&str>,
    portal_url: Option<&str>,
) -> Result<UpstreamRow, DbError> {
    let now = db::unix_secs();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM repl_replicas", [])?;
    tx.execute("DELETE FROM repl_binlog", [])?;
    tx.execute("DELETE FROM repl_pending_blobs", [])?;
    tx.execute(
        "UPDATE node_config SET role = 'backup', min_retained_lsn = 0, updated_at = ?1 WHERE id = 1",
        params![now as i64],
    )?;
    if let Some(name) = node_name.map(str::trim).filter(|s| !s.is_empty()) {
        tx.execute("UPDATE node_config SET node_name = ?1 WHERE id = 1", params![name])?;
    }
    if let Some(url) = portal_url.map(str::trim).filter(|s| !s.is_empty()) {
        tx.execute("UPDATE node_config SET portal_url = ?1 WHERE id = 1", params![url.trim_end_matches('/')])?;
    }
    tx.execute(
        "INSERT INTO repl_upstream
           (id, primary_url, primary_id, primary_name, primary_portal_url, primary_epoch, replica_token,
            bootstrapped, applied_lsn, primary_head_lsn, state, last_sync_at, last_error, configured_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7, 'bootstrapping', 0, '', ?8)
         ON CONFLICT(id) DO UPDATE SET
           primary_url = excluded.primary_url,
           primary_id = excluded.primary_id,
           primary_name = excluded.primary_name,
           primary_portal_url = excluded.primary_portal_url,
           primary_epoch = excluded.primary_epoch,
           replica_token = excluded.replica_token,
           bootstrapped = 0,
           applied_lsn = 0,
           primary_head_lsn = excluded.primary_head_lsn,
           state = 'bootstrapping',
           last_sync_at = 0,
           last_error = '',
           configured_at = excluded.configured_at",
        params![
            primary_url.trim_end_matches('/'),
            primary_id,
            primary_name,
            primary_portal_url.trim_end_matches('/'),
            primary_epoch,
            replica_token,
            primary_head_lsn as i64,
            now as i64,
        ],
    )?;
    let row = get_upstream(&tx)?.ok_or(DbError::Internal(anyhow::anyhow!("upstream row missing")))?;
    tx.commit()?;
    Ok(row)
}

pub fn mark_resync_needed(conn: &mut Connection) -> Result<(), DbError> {
    conn.execute(
        "UPDATE repl_upstream SET bootstrapped = 0, state = 'bootstrapping', last_error = '' WHERE id = 1",
        [],
    )?;
    Ok(())
}

pub fn update_upstream_state(
    conn: &mut Connection,
    state: &str,
    last_error: Option<&str>,
    primary_head_lsn: Option<u64>,
    primary_name: Option<&str>,
    primary_portal_url: Option<&str>,
    touch_sync: bool,
) -> Result<(), DbError> {
    let now = db::unix_secs() as i64;
    conn.execute(
        "UPDATE repl_upstream SET
           state = ?1,
           last_error = COALESCE(?2, last_error),
           primary_head_lsn = COALESCE(?3, primary_head_lsn),
           primary_name = COALESCE(NULLIF(?4, ''), primary_name),
           primary_portal_url = COALESCE(?5, primary_portal_url),
           last_sync_at = CASE WHEN ?6 != 0 THEN ?7 ELSE last_sync_at END
         WHERE id = 1",
        params![
            state,
            last_error,
            primary_head_lsn.map(|v| v as i64),
            primary_name.unwrap_or(""),
            primary_portal_url.map(|u| u.trim().trim_end_matches('/')),
            i32::from(touch_sync),
            now,
        ],
    )?;
    Ok(())
}

/// Promote this Backup node into a standalone Primary node, preserving all replicated business data.
pub fn promote_to_primary(conn: &mut Connection) -> Result<String, DbError> {
    let now = db::unix_secs() as i64;
    let new_epoch = Uuid::new_v4().to_string();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cfg = db::get_node_config(&tx)?;
    if cfg.role != NodeRole::Backup {
        return Err(DbError::Conflict("当前节点已是主服务器"));
    }
    tx.execute("DELETE FROM repl_upstream", [])?;
    tx.execute("DELETE FROM repl_pending_blobs", [])?;
    tx.execute("DELETE FROM repl_binlog", [])?;
    tx.execute(
        "UPDATE node_config SET role = 'primary', repl_epoch = ?1, min_retained_lsn = 0, updated_at = ?2 WHERE id = 1",
        params![&new_epoch, now],
    )?;
    tx.commit()?;
    Ok(new_epoch)
}

/// Atomically apply a validated NDJSON snapshot file into the Backup database.
/// Returns `(snapshot_lsn, pending_blob_count)`.
pub fn apply_snapshot_ndjson(
    conn: &mut Connection,
    path: &Path,
    expected_primary_id: &str,
    expected_configured_at: u64,
) -> Result<(u64, u64), DbError> {
    // Pass 1: verify file starts with Header and ends with matching End (guarantees complete transfer).
    let mut header_info: Option<(String, String, String, String, u64)> = None;
    let mut end_lsn: Option<u64> = None;
    {
        let file = File::open(path).map_err(|e| DbError::Internal(e.into()))?;
        let reader = BufReader::new(file);
        for line_res in reader.lines() {
            let line = line_res.map_err(|e| DbError::Internal(e.into()))?;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: SnapshotLine = serde_json::from_str(&line)
                .map_err(|e| DbError::Internal(anyhow::anyhow!("invalid snapshot line: {e}")))?;
            match parsed {
                SnapshotLine::Header { primary_id, primary_name, primary_portal_url, epoch, snapshot_lsn } => {
                    header_info = Some((primary_id, primary_name, primary_portal_url, epoch, snapshot_lsn));
                }
                SnapshotLine::End { snapshot_lsn } => {
                    end_lsn = Some(snapshot_lsn);
                }
                _ => {}
            }
        }
    }
    let (primary_id, primary_name, primary_portal_url, epoch, snapshot_lsn) =
        header_info.ok_or_else(|| DbError::Internal(anyhow::anyhow!("snapshot missing header")))?;
    if end_lsn != Some(snapshot_lsn) {
        return Err(DbError::Internal(anyhow::anyhow!("snapshot truncated or mismatched end marker")));
    }

    // Pass 2: atomic single-transaction replacement of replicated business tables.
    let now = db::unix_secs() as i64;
    let b64 = base64::engine::general_purpose::STANDARD;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cfg = db::get_node_config(&tx)?;
    if cfg.role != NodeRole::Backup {
        return Err(DbError::Conflict("节点已不再处于备份模式"));
    }
    let cur_up = get_upstream(&tx)?.ok_or(DbError::Conflict("未配置上游主服务器"))?;
    if cur_up.primary_id != expected_primary_id || cur_up.configured_at != expected_configured_at {
        return Err(DbError::Conflict("上游主服务器配置已变更，放弃过期快照"));
    }

    // Clear replicated tables in reverse FK order (local-only tables untouched!).
    tx.execute("DELETE FROM op_log_blobs", [])?;
    tx.execute("DELETE FROM entity_blobs", [])?;
    tx.execute("DELETE FROM vault_blobs", [])?;
    tx.execute("DELETE FROM op_log", [])?;
    tx.execute("DELETE FROM entities", [])?;
    tx.execute("DELETE FROM vaults", [])?;
    tx.execute("DELETE FROM devices", [])?;
    tx.execute("DELETE FROM users", [])?;
    tx.execute("DELETE FROM blob_leases", [])?;
    tx.execute("DELETE FROM repl_pending_blobs", [])?;

    let file = File::open(path).map_err(|e| DbError::Internal(e.into()))?;
    let reader = BufReader::new(file);
    for line_res in reader.lines() {
        let line = line_res.map_err(|e| DbError::Internal(e.into()))?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: SnapshotLine = serde_json::from_str(&line)
            .map_err(|e| DbError::Internal(anyhow::anyhow!("snapshot parse error: {e}")))?;
        match parsed {
            SnapshotLine::Users { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO users (id, email, display_name, password_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for r in rows {
                    stmt.execute(params![r.id, r.email, r.display_name, r.password_hash, r.created_at as i64])?;
                }
            }
            SnapshotLine::Devices { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO devices (id, user_id, name, platform, refresh_hash, refresh_expires_at, last_seen_at, revoked)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for r in rows {
                    stmt.execute(params![
                        r.id,
                        r.user_id,
                        r.name,
                        r.platform,
                        r.refresh_hash,
                        r.refresh_expires_at as i64,
                        r.last_seen_at as i64,
                        i32::from(r.revoked),
                    ])?;
                }
            }
            SnapshotLine::Vaults { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO vaults (id, owner_id, name, head_seq, e2ee_enabled, e2ee_meta, min_retained_seq, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for r in rows {
                    stmt.execute(params![
                        r.id,
                        r.owner_id,
                        r.name,
                        r.head_seq as i64,
                        i32::from(r.e2ee_enabled),
                        r.e2ee_meta,
                        r.min_retained_seq as i64,
                        r.created_at as i64,
                    ])?;
                }
            }
            SnapshotLine::Entities { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO entities (vault_id, entity_id, kind, version, seq, deleted, envelope, updated_at, device_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for r in rows {
                    let env = r.envelope_b64.as_deref().map(|s| b64.decode(s)).transpose().map_err(|e| DbError::Internal(e.into()))?;
                    stmt.execute(params![
                        r.vault_id,
                        r.entity_id,
                        r.kind,
                        r.version as i64,
                        r.seq as i64,
                        i32::from(r.deleted),
                        env,
                        r.updated_at as i64,
                        r.device_id,
                    ])?;
                }
            }
            SnapshotLine::OpLogs { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO op_log (vault_id, seq, entity_id, kind, version, deleted, envelope, updated_at, device_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for r in rows {
                    let env = r.envelope_b64.as_deref().map(|s| b64.decode(s)).transpose().map_err(|e| DbError::Internal(e.into()))?;
                    stmt.execute(params![
                        r.vault_id,
                        r.seq as i64,
                        r.entity_id,
                        r.kind,
                        r.version as i64,
                        i32::from(r.deleted),
                        env,
                        r.updated_at as i64,
                        r.device_id,
                    ])?;
                }
            }
            SnapshotLine::VaultBlobs { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO vault_blobs (vault_id, blob_hash, size, created_at) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for r in rows {
                    stmt.execute(params![r.vault_id, r.blob_hash, r.size as i64, r.created_at as i64])?;
                }
            }
            SnapshotLine::EntityBlobs { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO entity_blobs (vault_id, entity_id, blob_hash) VALUES (?1, ?2, ?3)",
                )?;
                for r in rows {
                    stmt.execute(params![r.vault_id, r.entity_id, r.blob_hash])?;
                }
            }
            SnapshotLine::OpLogBlobs { rows } => {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO op_log_blobs (vault_id, seq, blob_hash) VALUES (?1, ?2, ?3)",
                )?;
                for r in rows {
                    stmt.execute(params![r.vault_id, r.seq as i64, r.blob_hash])?;
                }
            }
            SnapshotLine::Header { .. } | SnapshotLine::End { .. } => {}
        }
    }

    // Queue all referenced blobs for verification / download by BlobFetcher.
    tx.execute(
        "INSERT OR IGNORE INTO repl_pending_blobs (blob_hash, size, attempts, last_error, created_at)
         SELECT blob_hash, MAX(size), 0, '', ?1 FROM vault_blobs GROUP BY blob_hash",
        params![now],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO repl_pending_blobs (blob_hash, size, attempts, last_error, created_at)
         SELECT DISTINCT blob_hash, 0, 0, '', ?1 FROM entity_blobs",
        params![now],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO repl_pending_blobs (blob_hash, size, attempts, last_error, created_at)
         SELECT DISTINCT blob_hash, 0, 0, '', ?1 FROM op_log_blobs",
        params![now],
    )?;
    let pending: i64 = tx.query_row("SELECT COUNT(*) FROM repl_pending_blobs", [], |r| r.get(0))?;

    tx.execute(
        "UPDATE repl_upstream SET
           primary_id = ?1,
           primary_name = ?2,
           primary_portal_url = CASE WHEN ?3 != '' THEN ?3 ELSE primary_portal_url END,
           primary_epoch = ?4,
           bootstrapped = 1,
           applied_lsn = ?5,
           primary_head_lsn = ?5,
           state = 'streaming',
           last_sync_at = ?6,
           last_error = ''
         WHERE id = 1",
        params![
            primary_id,
            primary_name,
            primary_portal_url.trim_end_matches('/'),
            epoch,
            snapshot_lsn as i64,
            now,
        ],
    )?;
    tx.commit()?;
    Ok((snapshot_lsn, pending as u64))
}

/// Atomically and idempotently apply a batch of binlog records on a Backup node.
/// Returns `(new_applied_lsn, has_new_blobs, gc_triggered)`.
pub fn apply_binlog_batch(
    conn: &mut Connection,
    records: &[BinlogRecord],
    primary_head_lsn: u64,
) -> Result<(u64, bool, bool), DbError> {
    let now = db::unix_secs();
    let b64 = base64::engine::general_purpose::STANDARD;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cfg = db::get_node_config(&tx)?;
    if cfg.role != NodeRole::Backup {
        return Err(DbError::Conflict("节点已不再处于备份模式"));
    }
    let upstream = get_upstream(&tx)?.ok_or(DbError::Conflict("未配置上游主服务器"))?;
    let mut applied = upstream.applied_lsn;
    let mut has_new_blobs = false;
    let mut gc_triggered = false;

    for rec in records {
        if rec.lsn <= applied {
            continue;
        }
        match &rec.event {
            ReplEvent::UserUpsert { id, email, display_name, password_hash, created_at } => {
                tx.execute(
                    "INSERT INTO users (id, email, display_name, password_hash, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(id) DO UPDATE SET
                       email = excluded.email,
                       display_name = excluded.display_name,
                       password_hash = excluded.password_hash",
                    params![id, email, display_name, password_hash, *created_at as i64],
                )?;
            }
            ReplEvent::UserDelete { user_id } => {
                tx.execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
                cleanup_unreferenced_pending_blobs_tx(&tx)?;
                gc_triggered = true;
            }
            ReplEvent::DeviceUpsert {
                id,
                user_id,
                name,
                platform,
                refresh_hash,
                refresh_expires_at,
                last_seen_at,
                revoked,
            } => {
                tx.execute(
                    "INSERT INTO devices (id, user_id, name, platform, refresh_hash, refresh_expires_at, last_seen_at, revoked)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(id) DO UPDATE SET
                       user_id = excluded.user_id,
                       name = excluded.name,
                       platform = excluded.platform,
                       refresh_hash = excluded.refresh_hash,
                       refresh_expires_at = excluded.refresh_expires_at,
                       last_seen_at = excluded.last_seen_at,
                       revoked = excluded.revoked",
                    params![
                        id,
                        user_id,
                        name,
                        platform,
                        refresh_hash,
                        *refresh_expires_at as i64,
                        *last_seen_at as i64,
                        i32::from(*revoked),
                    ],
                )?;
            }
            ReplEvent::VaultUpsert {
                id,
                owner_id,
                name,
                head_seq,
                e2ee_enabled,
                e2ee_meta,
                min_retained_seq,
                created_at,
            } => {
                tx.execute(
                    "INSERT INTO vaults (id, owner_id, name, head_seq, e2ee_enabled, e2ee_meta, min_retained_seq, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(id) DO UPDATE SET
                       owner_id = excluded.owner_id,
                       name = excluded.name,
                       head_seq = MAX(vaults.head_seq, excluded.head_seq),
                       e2ee_enabled = excluded.e2ee_enabled,
                       e2ee_meta = excluded.e2ee_meta,
                       min_retained_seq = MAX(vaults.min_retained_seq, excluded.min_retained_seq)",
                    params![
                        id,
                        owner_id,
                        name,
                        *head_seq as i64,
                        i32::from(*e2ee_enabled),
                        e2ee_meta,
                        *min_retained_seq as i64,
                        *created_at as i64,
                    ],
                )?;
            }
            ReplEvent::VaultDelete { vault_id } => {
                tx.execute("DELETE FROM vaults WHERE id = ?1", params![vault_id])?;
                cleanup_unreferenced_pending_blobs_tx(&tx)?;
                gc_triggered = true;
            }
            ReplEvent::VaultBlobRegister { vault_id, blob_hash, size, created_at } => {
                tx.execute(
                    "INSERT INTO vault_blobs (vault_id, blob_hash, size, created_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(vault_id, blob_hash) DO UPDATE SET size = excluded.size, created_at = excluded.created_at",
                    params![vault_id, blob_hash, *size as i64, *created_at as i64],
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO repl_pending_blobs (blob_hash, size, attempts, last_error, created_at)
                     VALUES (?1, ?2, 0, '', ?3)",
                    params![blob_hash, *size as i64, now as i64],
                )?;
                has_new_blobs = true;
            }
            ReplEvent::PushBatch { vault_id, head_seq, device_id, device_last_seen_at, items } => {
                for item in items {
                    let env_bytes = item
                        .envelope_b64
                        .as_deref()
                        .map(|s| b64.decode(s))
                        .transpose()
                        .map_err(|e| DbError::Internal(e.into()))?;
                    let del_int = i32::from(item.deleted);
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
                        params![
                            vault_id,
                            &item.entity_id,
                            &item.kind,
                            item.version as i64,
                            item.seq as i64,
                            del_int,
                            env_bytes,
                            item.updated_at as i64,
                            &item.device_id,
                        ],
                    )?;
                    tx.execute(
                        "INSERT OR REPLACE INTO op_log (vault_id, seq, entity_id, kind, version, deleted, envelope, updated_at, device_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            vault_id,
                            item.seq as i64,
                            &item.entity_id,
                            &item.kind,
                            item.version as i64,
                            del_int,
                            env_bytes,
                            item.updated_at as i64,
                            &item.device_id,
                        ],
                    )?;
                    tx.execute(
                        "DELETE FROM entity_blobs WHERE vault_id = ?1 AND entity_id = ?2",
                        params![vault_id, &item.entity_id],
                    )?;
                    if !item.deleted {
                        for h in &item.blob_hashes {
                            tx.execute(
                                "INSERT OR IGNORE INTO entity_blobs (vault_id, entity_id, blob_hash) VALUES (?1, ?2, ?3)",
                                params![vault_id, &item.entity_id, h],
                            )?;
                            tx.execute(
                                "INSERT OR IGNORE INTO op_log_blobs (vault_id, seq, blob_hash) VALUES (?1, ?2, ?3)",
                                params![vault_id, item.seq as i64, h],
                            )?;
                            tx.execute(
                                "INSERT OR IGNORE INTO repl_pending_blobs (blob_hash, size, attempts, last_error, created_at)
                                 VALUES (?1, 0, 0, '', ?2)",
                                params![h, now as i64],
                            )?;
                            has_new_blobs = true;
                        }
                    }
                }
                tx.execute(
                    "UPDATE vaults SET head_seq = MAX(head_seq, ?1) WHERE id = ?2",
                    params![*head_seq as i64, vault_id],
                )?;
                tx.execute(
                    "UPDATE devices SET last_seen_at = MAX(last_seen_at, ?1) WHERE id = ?2",
                    params![*device_last_seen_at as i64, device_id],
                )?;
            }
            ReplEvent::GcCompact { tombstone_cutoff, upload_grace_cutoff } => {
                let _ = db::prune_old_tombstones_tx(&tx, *tombstone_cutoff, *upload_grace_cutoff, now)?;
                cleanup_unreferenced_pending_blobs_tx(&tx)?;
                gc_triggered = true;
            }
        }
        applied = rec.lsn;
    }

    tx.execute(
        "UPDATE repl_upstream SET
           applied_lsn = ?1,
           primary_head_lsn = MAX(primary_head_lsn, ?2),
           state = 'streaming',
           last_sync_at = ?3,
           last_error = ''
         WHERE id = 1",
        params![applied as i64, primary_head_lsn.max(applied) as i64, now as i64],
    )?;
    tx.commit()?;
    Ok((applied, has_new_blobs, gc_triggered))
}

fn cleanup_unreferenced_pending_blobs_tx(tx: &rusqlite::Transaction<'_>) -> Result<(), DbError> {
    tx.execute(
        "DELETE FROM repl_pending_blobs
         WHERE NOT EXISTS (SELECT 1 FROM vault_blobs vb WHERE vb.blob_hash = repl_pending_blobs.blob_hash)
           AND NOT EXISTS (SELECT 1 FROM entity_blobs eb WHERE eb.blob_hash = repl_pending_blobs.blob_hash)
           AND NOT EXISTS (SELECT 1 FROM op_log_blobs ob WHERE ob.blob_hash = repl_pending_blobs.blob_hash)",
        [],
    )?;
    Ok(())
}

pub fn list_pending_blobs(conn: &Connection, limit: usize) -> Result<Vec<String>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT blob_hash FROM repl_pending_blobs ORDER BY attempts ASC, created_at ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit.clamp(1, 200) as i64], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn count_pending_blobs(conn: &Connection) -> Result<u64, DbError> {
    let c: i64 = conn.query_row("SELECT COUNT(*) FROM repl_pending_blobs", [], |r| r.get(0))?;
    Ok(c.max(0) as u64)
}

pub fn mark_pending_blob_done(conn: &mut Connection, blob_hash: &str) -> Result<(), DbError> {
    conn.execute("DELETE FROM repl_pending_blobs WHERE blob_hash = ?1", params![blob_hash])?;
    Ok(())
}

pub fn mark_pending_blob_failed(conn: &mut Connection, blob_hash: &str, err: &str) -> Result<(), DbError> {
    conn.execute(
        "UPDATE repl_pending_blobs SET attempts = attempts + 1, last_error = ?2 WHERE blob_hash = ?1",
        params![blob_hash, err],
    )?;
    Ok(())
}

/// If Primary returned 404 for a blob and it is no longer referenced in local tables, drop it from pending.
pub fn drop_if_unreferenced_pending_blob(conn: &mut Connection, blob_hash: &str) -> Result<bool, DbError> {
    let referenced = db::is_blob_referenced_anywhere(conn, blob_hash, db::unix_secs())?;
    if !referenced {
        conn.execute("DELETE FROM repl_pending_blobs WHERE blob_hash = ?1", params![blob_hash])?;
        return Ok(true);
    }
    Ok(false)
}

pub fn find_backup_orphan_blobs(conn: &Connection, all_stored_hashes: &[String]) -> Result<Vec<String>, DbError> {
    let mut live = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT blob_hash FROM vault_blobs
         UNION
         SELECT blob_hash FROM entity_blobs
         UNION
         SELECT blob_hash FROM op_log_blobs
         UNION
         SELECT blob_hash FROM repl_pending_blobs",
    )?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        live.insert(row?);
    }
    Ok(all_stored_hashes.iter().filter(|h| !live.contains(h.as_str())).cloned().collect())
}
