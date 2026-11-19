use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

use crate::db::{self, DbError, ReplEvent};

#[derive(Clone, Debug, Serialize)]
pub struct OverviewDbStats {
    pub user_count: u64,
    pub device_count: u64,
    pub active_device_count: u64,
    pub vault_count: u64,
    pub entity_count: u64,
    pub op_log_count: u64,
    pub entity_bytes: u64,
    pub op_log_bytes: u64,
    pub unique_blob_count: u64,
    pub unique_blob_bytes: u64,
    pub logical_blob_bytes: u64,
    pub binlog_count: u64,
    pub binlog_bytes: u64,
    pub head_lsn: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdminUserSummary {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub created_at: u64,
    pub vault_count: u64,
    pub device_count: u64,
    pub active_device_count: u64,
    pub online_device_count: u64,
    pub entity_count: u64,
    pub entity_bytes: u64,
    pub blob_count: u64,
    pub blob_bytes: u64,
    pub total_storage_bytes: u64,
    pub last_seen_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdminVaultDetail {
    pub id: String,
    pub owner_id: String,
    pub owner_email: String,
    pub name: String,
    pub head_seq: u64,
    pub e2ee_enabled: bool,
    pub entity_count: u64,
    pub entity_bytes: u64,
    pub blob_count: u64,
    pub blob_bytes: u64,
    pub total_storage_bytes: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdminDeviceDetail {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub last_seen_at: u64,
    pub revoked: bool,
    pub online: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdminUserDetail {
    pub user: AdminUserSummary,
    pub vaults: Vec<AdminVaultDetail>,
    pub devices: Vec<AdminDeviceDetail>,
}

pub fn apply_startup_overrides(
    conn: &mut Connection,
    node_name: Option<&str>,
    public_url: Option<&str>,
    admin_password_hash: Option<&str>,
    bump_admin_version: bool,
) -> Result<(), DbError> {
    let now = db::unix_secs() as i64;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(name) = node_name.map(str::trim).filter(|s| !s.is_empty()) {
        tx.execute("UPDATE node_config SET node_name = ?1, updated_at = ?2 WHERE id = 1", params![name, now])?;
    }
    if let Some(url) = public_url.map(str::trim).filter(|s| !s.is_empty()) {
        let normalized = url.trim_end_matches('/');
        tx.execute("UPDATE node_config SET portal_url = ?1, updated_at = ?2 WHERE id = 1", params![normalized, now])?;
    }
    if let Some(hash) = admin_password_hash {
        if bump_admin_version {
            tx.execute(
                "UPDATE node_config SET admin_password_hash = ?1, admin_token_version = admin_token_version + 1, updated_at = ?2 WHERE id = 1",
                params![hash, now],
            )?;
        } else {
            tx.execute("UPDATE node_config SET admin_password_hash = ?1, updated_at = ?2 WHERE id = 1", params![hash, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn setup_admin_password(
    conn: &mut Connection,
    password_hash: &str,
    node_name: Option<&str>,
    portal_url: Option<&str>,
) -> Result<u64, DbError> {
    let now = db::unix_secs() as i64;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<String> = tx.query_row("SELECT admin_password_hash FROM node_config WHERE id = 1", [], |r| r.get(0))?;
    if existing.is_some() {
        return Err(DbError::Conflict("管理员密码已初始化"));
    }
    tx.execute(
        "UPDATE node_config SET admin_password_hash = ?1, admin_token_version = 1, updated_at = ?2 WHERE id = 1",
        params![password_hash, now],
    )?;
    if let Some(name) = node_name.map(str::trim).filter(|s| !s.is_empty()) {
        tx.execute("UPDATE node_config SET node_name = ?1 WHERE id = 1", params![name])?;
    }
    if let Some(url) = portal_url.map(str::trim).filter(|s| !s.is_empty()) {
        tx.execute("UPDATE node_config SET portal_url = ?1 WHERE id = 1", params![url.trim_end_matches('/')])?;
    }
    tx.commit()?;
    Ok(1)
}

pub fn change_admin_password(conn: &mut Connection, new_hash: &str) -> Result<u64, DbError> {
    let now = db::unix_secs() as i64;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE node_config SET admin_password_hash = ?1, admin_token_version = admin_token_version + 1, updated_at = ?2 WHERE id = 1",
        params![new_hash, now],
    )?;
    let ver: i64 = tx.query_row("SELECT admin_token_version FROM node_config WHERE id = 1", [], |r| r.get(0))?;
    tx.commit()?;
    Ok(ver as u64)
}

pub fn update_node_settings(
    conn: &mut Connection,
    node_name: Option<&str>,
    portal_url: Option<&str>,
    allow_register_override: Option<bool>,
    invite_code_override: Option<Option<&str>>,
) -> Result<db::NodeConfig, DbError> {
    let now = db::unix_secs() as i64;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(name) = node_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            tx.execute("UPDATE node_config SET node_name = ?1, updated_at = ?2 WHERE id = 1", params![trimmed, now])?;
        }
    }
    if let Some(url) = portal_url {
        let trimmed = url.trim().trim_end_matches('/');
        tx.execute("UPDATE node_config SET portal_url = ?1, updated_at = ?2 WHERE id = 1", params![trimmed, now])?;
    }
    if let Some(allow) = allow_register_override {
        tx.execute(
            "UPDATE node_config SET allow_register_override = ?1, updated_at = ?2 WHERE id = 1",
            params![i32::from(allow), now],
        )?;
    }
    if let Some(invite_opt) = invite_code_override {
        let val = invite_opt.map(|s| s.trim().to_string()).unwrap_or_default();
        tx.execute(
            "UPDATE node_config SET invite_code_override = ?1, updated_at = ?2 WHERE id = 1",
            params![val, now],
        )?;
    }
    let cfg = db::get_node_config(&tx)?;
    tx.commit()?;
    Ok(cfg)
}

pub fn get_overview_stats(conn: &Connection) -> Result<OverviewDbStats, DbError> {
    let user_count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    let device_count: i64 = conn.query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))?;
    let active_device_count: i64 = conn.query_row("SELECT COUNT(*) FROM devices WHERE revoked = 0", [], |r| r.get(0))?;
    let vault_count: i64 = conn.query_row("SELECT COUNT(*) FROM vaults", [], |r| r.get(0))?;
    let (entity_count, entity_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(envelope)), 0) FROM entities WHERE deleted = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (op_log_count, op_log_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(envelope)), 0) FROM op_log",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (unique_blob_count, unique_blob_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(sz), 0) FROM (SELECT blob_hash, MAX(size) AS sz FROM vault_blobs GROUP BY blob_hash)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let logical_blob_bytes: i64 = conn.query_row("SELECT COALESCE(SUM(size), 0) FROM vault_blobs", [], |r| r.get(0))?;
    let (binlog_count, binlog_bytes): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(payload)), 0) FROM repl_binlog",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let head_lsn = db::get_head_lsn(conn)?;

    Ok(OverviewDbStats {
        user_count: user_count as u64,
        device_count: device_count as u64,
        active_device_count: active_device_count as u64,
        vault_count: vault_count as u64,
        entity_count: entity_count as u64,
        op_log_count: op_log_count as u64,
        entity_bytes: entity_bytes as u64,
        op_log_bytes: op_log_bytes as u64,
        unique_blob_count: unique_blob_count as u64,
        unique_blob_bytes: unique_blob_bytes as u64,
        logical_blob_bytes: logical_blob_bytes as u64,
        binlog_count: binlog_count as u64,
        binlog_bytes: binlog_bytes as u64,
        head_lsn,
    })
}

pub fn list_users_admin(
    conn: &Connection,
    query: &str,
    sort: &str,
    limit: u32,
    offset: u32,
    online_by_user: &HashMap<String, HashSet<String>>,
) -> Result<(u64, Vec<AdminUserSummary>), DbError> {
    let q = query.trim().to_lowercase();
    let like_pat = format!("%{q}%");

    let total: i64 = if q.is_empty() {
        conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM users WHERE LOWER(email) LIKE ?1 OR LOWER(display_name) LIKE ?1 OR LOWER(id) LIKE ?1",
            params![&like_pat],
            |r| r.get(0),
        )?
    };

    let order_by = match sort {
        "created_desc" => "u.created_at DESC",
        "created_asc" => "u.created_at ASC",
        "email_asc" => "u.email ASC",
        _ => "total_storage_bytes DESC, u.created_at DESC",
    };

    let sql = format!(
        "SELECT
            u.id,
            u.email,
            u.display_name,
            u.created_at,
            COALESCE(v_agg.vault_count, 0) AS vault_count,
            COALESCE(d_agg.device_count, 0) AS device_count,
            COALESCE(d_agg.active_device_count, 0) AS active_device_count,
            COALESCE(d_agg.last_seen_at, 0) AS last_seen_at,
            COALESCE(e_agg.entity_count, 0) AS entity_count,
            COALESCE(e_agg.entity_bytes, 0) AS entity_bytes,
            COALESCE(b_agg.blob_count, 0) AS blob_count,
            COALESCE(b_agg.blob_bytes, 0) AS blob_bytes,
            (COALESCE(e_agg.entity_bytes, 0) + COALESCE(b_agg.blob_bytes, 0)) AS total_storage_bytes
         FROM users u
         LEFT JOIN (
            SELECT owner_id, COUNT(*) AS vault_count FROM vaults GROUP BY owner_id
         ) v_agg ON v_agg.owner_id = u.id
         LEFT JOIN (
            SELECT user_id,
                   COUNT(*) AS device_count,
                   SUM(CASE WHEN revoked = 0 THEN 1 ELSE 0 END) AS active_device_count,
                   MAX(last_seen_at) AS last_seen_at
            FROM devices GROUP BY user_id
         ) d_agg ON d_agg.user_id = u.id
         LEFT JOIN (
            SELECT v.owner_id AS owner_id,
                   COUNT(e.entity_id) AS entity_count,
                   COALESCE(SUM(LENGTH(e.envelope)), 0) AS entity_bytes
            FROM vaults v
            JOIN entities e ON e.vault_id = v.id AND e.deleted = 0
            GROUP BY v.owner_id
         ) e_agg ON e_agg.owner_id = u.id
         LEFT JOIN (
            SELECT v.owner_id AS owner_id,
                   COUNT(vb.blob_hash) AS blob_count,
                   COALESCE(SUM(vb.size), 0) AS blob_bytes
            FROM vaults v
            JOIN vault_blobs vb ON vb.vault_id = v.id
            GROUP BY v.owner_id
         ) b_agg ON b_agg.owner_id = u.id
         {}
         ORDER BY {order_by}
         LIMIT ?1 OFFSET ?2",
        if q.is_empty() {
            ""
        } else {
            "WHERE LOWER(u.email) LIKE ?3 OR LOWER(u.display_name) LIKE ?3 OR LOWER(u.id) LIKE ?3"
        }
    );

    let cap = limit.clamp(1, 200) as i64;
    let off = offset as i64;
    let mut stmt = conn.prepare(&sql)?;
    let mut users = Vec::new();
    let mapper = |r: &rusqlite::Row<'_>| -> rusqlite::Result<AdminUserSummary> {
        let id: String = r.get(0)?;
        let online_device_count = online_by_user.get(&id).map_or(0, HashSet::len) as u64;
        Ok(AdminUserSummary {
            id,
            email: r.get(1)?,
            display_name: r.get(2)?,
            created_at: r.get::<_, i64>(3)? as u64,
            vault_count: r.get::<_, i64>(4)? as u64,
            device_count: r.get::<_, i64>(5)? as u64,
            active_device_count: r.get::<_, i64>(6)? as u64,
            online_device_count,
            last_seen_at: r.get::<_, i64>(7)? as u64,
            entity_count: r.get::<_, i64>(8)? as u64,
            entity_bytes: r.get::<_, i64>(9)? as u64,
            blob_count: r.get::<_, i64>(10)? as u64,
            blob_bytes: r.get::<_, i64>(11)? as u64,
            total_storage_bytes: r.get::<_, i64>(12)? as u64,
        })
    };

    if q.is_empty() {
        for row in stmt.query_map(params![cap, off], mapper)? {
            users.push(row?);
        }
    } else {
        for row in stmt.query_map(params![cap, off, &like_pat], mapper)? {
            users.push(row?);
        }
    }
    Ok((total as u64, users))
}

pub fn get_user_detail_admin(
    conn: &Connection,
    user_id: &str,
    online_devices: &HashSet<String>,
) -> Result<AdminUserDetail, DbError> {
    let user = conn
        .query_row(
            "SELECT
                u.id,
                u.email,
                u.display_name,
                u.created_at,
                (SELECT COUNT(*) FROM vaults WHERE owner_id = u.id),
                (SELECT COUNT(*) FROM devices WHERE user_id = u.id),
                (SELECT COUNT(*) FROM devices WHERE user_id = u.id AND revoked = 0),
                COALESCE((SELECT MAX(last_seen_at) FROM devices WHERE user_id = u.id), 0),
                (SELECT COUNT(*) FROM entities e JOIN vaults v ON v.id = e.vault_id WHERE v.owner_id = u.id AND e.deleted = 0),
                COALESCE((SELECT SUM(LENGTH(e.envelope)) FROM entities e JOIN vaults v ON v.id = e.vault_id WHERE v.owner_id = u.id AND e.deleted = 0), 0),
                (SELECT COUNT(*) FROM vault_blobs vb JOIN vaults v ON v.id = vb.vault_id WHERE v.owner_id = u.id),
                COALESCE((SELECT SUM(vb.size) FROM vault_blobs vb JOIN vaults v ON v.id = vb.vault_id WHERE v.owner_id = u.id), 0)
             FROM users u
             WHERE u.id = ?1",
            params![user_id],
            |r| {
                let entity_bytes = r.get::<_, i64>(9)? as u64;
                let blob_bytes = r.get::<_, i64>(11)? as u64;
                Ok(AdminUserSummary {
                    id: r.get(0)?,
                    email: r.get(1)?,
                    display_name: r.get(2)?,
                    created_at: r.get::<_, i64>(3)? as u64,
                    vault_count: r.get::<_, i64>(4)? as u64,
                    device_count: r.get::<_, i64>(5)? as u64,
                    active_device_count: r.get::<_, i64>(6)? as u64,
                    online_device_count: online_devices.len() as u64,
                    last_seen_at: r.get::<_, i64>(7)? as u64,
                    entity_count: r.get::<_, i64>(8)? as u64,
                    entity_bytes,
                    blob_count: r.get::<_, i64>(10)? as u64,
                    blob_bytes,
                    total_storage_bytes: entity_bytes + blob_bytes,
                })
            },
        )
        .optional()?
        .ok_or(DbError::NotFound("用户不存在"))?;

    let mut v_stmt = conn.prepare(
        "SELECT
            v.id,
            v.owner_id,
            u.email,
            v.name,
            v.head_seq,
            v.e2ee_enabled,
            COALESCE(e_agg.entity_count, 0),
            COALESCE(e_agg.entity_bytes, 0),
            COALESCE(b_agg.blob_count, 0),
            COALESCE(b_agg.blob_bytes, 0),
            v.created_at
         FROM vaults v
         JOIN users u ON u.id = v.owner_id
         LEFT JOIN (
            SELECT vault_id, COUNT(*) AS entity_count, COALESCE(SUM(LENGTH(envelope)), 0) AS entity_bytes
            FROM entities WHERE deleted = 0 GROUP BY vault_id
         ) e_agg ON e_agg.vault_id = v.id
         LEFT JOIN (
            SELECT vault_id, COUNT(*) AS blob_count, COALESCE(SUM(size), 0) AS blob_bytes
            FROM vault_blobs GROUP BY vault_id
         ) b_agg ON b_agg.vault_id = v.id
         WHERE v.owner_id = ?1
         ORDER BY v.created_at ASC",
    )?;
    let vaults = v_stmt
        .query_map(params![user_id], |r| {
            let eb = r.get::<_, i64>(7)? as u64;
            let bb = r.get::<_, i64>(9)? as u64;
            Ok(AdminVaultDetail {
                id: r.get(0)?,
                owner_id: r.get(1)?,
                owner_email: r.get(2)?,
                name: r.get(3)?,
                head_seq: r.get::<_, i64>(4)? as u64,
                e2ee_enabled: r.get::<_, i64>(5)? != 0,
                entity_count: r.get::<_, i64>(6)? as u64,
                entity_bytes: eb,
                blob_count: r.get::<_, i64>(8)? as u64,
                blob_bytes: bb,
                total_storage_bytes: eb + bb,
                created_at: r.get::<_, i64>(10)? as u64,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut d_stmt = conn.prepare(
        "SELECT id, name, platform, last_seen_at, revoked FROM devices WHERE user_id = ?1 ORDER BY revoked ASC, last_seen_at DESC",
    )?;
    let devices = d_stmt
        .query_map(params![user_id], |r| {
            let id: String = r.get(0)?;
            let revoked = r.get::<_, i64>(4)? != 0;
            Ok(AdminDeviceDetail {
                online: !revoked && online_devices.contains(&id),
                id,
                name: r.get(1)?,
                platform: r.get(2)?,
                last_seen_at: r.get::<_, i64>(3)? as u64,
                revoked,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(AdminUserDetail { user, vaults, devices })
}

pub fn top_vaults_by_storage(conn: &Connection, limit: u32) -> Result<Vec<AdminVaultDetail>, DbError> {
    let cap = limit.clamp(1, 100) as i64;
    let mut stmt = conn.prepare(
        "SELECT
            v.id,
            v.owner_id,
            u.email,
            v.name,
            v.head_seq,
            v.e2ee_enabled,
            COALESCE(e_agg.entity_count, 0),
            COALESCE(e_agg.entity_bytes, 0),
            COALESCE(b_agg.blob_count, 0),
            COALESCE(b_agg.blob_bytes, 0),
            v.created_at,
            (COALESCE(e_agg.entity_bytes, 0) + COALESCE(b_agg.blob_bytes, 0)) AS total_bytes
         FROM vaults v
         JOIN users u ON u.id = v.owner_id
         LEFT JOIN (
            SELECT vault_id, COUNT(*) AS entity_count, COALESCE(SUM(LENGTH(envelope)), 0) AS entity_bytes
            FROM entities WHERE deleted = 0 GROUP BY vault_id
         ) e_agg ON e_agg.vault_id = v.id
         LEFT JOIN (
            SELECT vault_id, COUNT(*) AS blob_count, COALESCE(SUM(size), 0) AS blob_bytes
            FROM vault_blobs GROUP BY vault_id
         ) b_agg ON b_agg.vault_id = v.id
         ORDER BY total_bytes DESC, v.created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![cap], |r| {
        let eb = r.get::<_, i64>(7)? as u64;
        let bb = r.get::<_, i64>(9)? as u64;
        Ok(AdminVaultDetail {
            id: r.get(0)?,
            owner_id: r.get(1)?,
            owner_email: r.get(2)?,
            name: r.get(3)?,
            head_seq: r.get::<_, i64>(4)? as u64,
            e2ee_enabled: r.get::<_, i64>(5)? != 0,
            entity_count: r.get::<_, i64>(6)? as u64,
            entity_bytes: eb,
            blob_count: r.get::<_, i64>(8)? as u64,
            blob_bytes: bb,
            total_storage_bytes: eb + bb,
            created_at: r.get::<_, i64>(10)? as u64,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Delete a user and return candidate blob hashes that may now be unreferenced.
pub fn delete_user_admin(conn: &mut Connection, user_id: &str) -> Result<Vec<String>, DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists: Option<i64> = tx
        .query_row("SELECT 1 FROM users WHERE id = ?1", params![user_id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Err(DbError::NotFound("用户不存在"));
    }
    let mut candidate_hashes = HashSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT vb.blob_hash FROM vault_blobs vb JOIN vaults v ON v.id = vb.vault_id WHERE v.owner_id = ?1
             UNION
             SELECT DISTINCT eb.blob_hash FROM entity_blobs eb JOIN vaults v ON v.id = eb.vault_id WHERE v.owner_id = ?1
             UNION
             SELECT DISTINCT ob.blob_hash FROM op_log_blobs ob JOIN vaults v ON v.id = ob.vault_id WHERE v.owner_id = ?1",
        )?;
        for r in stmt.query_map(params![user_id], |r| r.get::<_, String>(0))? {
            candidate_hashes.insert(r?);
        }
    }
    tx.execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
    db::append_binlog(&tx, &ReplEvent::UserDelete { user_id: user_id.into() })?;
    tx.commit()?;
    Ok(candidate_hashes.into_iter().collect())
}

/// Delete a vault as admin and return candidate blob hashes that may now be unreferenced.
pub fn delete_vault_admin(conn: &mut Connection, vault_id: &str) -> Result<Vec<String>, DbError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists: Option<i64> = tx
        .query_row("SELECT 1 FROM vaults WHERE id = ?1", params![vault_id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Err(DbError::NotFound("文档库不存在"));
    }
    let mut candidate_hashes = HashSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT blob_hash FROM vault_blobs WHERE vault_id = ?1
             UNION
             SELECT DISTINCT blob_hash FROM entity_blobs WHERE vault_id = ?1
             UNION
             SELECT DISTINCT blob_hash FROM op_log_blobs WHERE vault_id = ?1",
        )?;
        for r in stmt.query_map(params![vault_id], |r| r.get::<_, String>(0))? {
            candidate_hashes.insert(r?);
        }
    }
    tx.execute("DELETE FROM vaults WHERE id = ?1", params![vault_id])?;
    db::append_binlog(&tx, &ReplEvent::VaultDelete { vault_id: vault_id.into() })?;
    tx.commit()?;
    Ok(candidate_hashes.into_iter().collect())
}
