use std::time::Duration;

use crate::db::{self, NodeRole};
use crate::db_repl;
use crate::AppState;

pub fn spawn_gc(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 3600));
        loop {
            interval.tick().await;
            if let Err(e) = run_once(&state).await {
                tracing::warn!("gc run failed: {e:#}");
            }
        }
    });
}

pub async fn run_once(state: &AppState) -> anyhow::Result<()> {
    let _ = run_once_count(state).await?;
    Ok(())
}

pub async fn run_once_count(state: &AppState) -> anyhow::Result<usize> {
    let node = state
        .db
        .run(|conn| db::get_node_config(conn))
        .await
        .map_err(|e| anyhow::anyhow!("gc read node config failed: {e:?}"))?;
    if node.role == NodeRole::Backup {
        return run_backup_orphan_cleanup(state).await;
    }

    let now = db::unix_secs();
    let cutoff = now.saturating_sub(state.config.tombstone_days * 86_400);
    let upload_grace_cutoff = now.saturating_sub(3600);
    let deleted = run_once_with_cutoffs(state, cutoff, upload_grace_cutoff).await?;

    let binlog_cutoff = now.saturating_sub(state.config.binlog_retention_days.max(1) * 86_400);
    let _ = state
        .db
        .run(move |conn| db_repl::compact_binlog(conn, binlog_cutoff, 200_000))
        .await;
    Ok(deleted)
}

pub async fn run_once_with_cutoffs(state: &AppState, tombstone_cutoff: u64, upload_grace_cutoff: u64) -> anyhow::Result<usize> {
    state.auth.prune_ws_tickets();
    let stored = state.blobs.list_hashes().await?;
    let orphans = state
        .db
        .run(move |conn| db::prune_old_tombstones_and_find_orphan_blobs(conn, tombstone_cutoff, upload_grace_cutoff, &stored))
        .await
        .map_err(|e| anyhow::anyhow!("gc db failed: {e:?}"))?;
    Ok(reclaim_candidate_blobs(state, orphans).await)
}

pub async fn run_backup_orphan_cleanup(state: &AppState) -> anyhow::Result<usize> {
    let stored = state.blobs.list_hashes().await?;
    let orphans = state
        .db
        .run(move |conn| db_repl::find_backup_orphan_blobs(conn, &stored))
        .await
        .map_err(|e| anyhow::anyhow!("backup gc db failed: {e:?}"))?;
    Ok(reclaim_candidate_blobs(state, orphans).await)
}

pub async fn reclaim_candidate_blobs(state: &AppState, candidates: Vec<String>) -> usize {
    let mut deleted = 0usize;
    for hash in candidates {
        let _gc_write = state.blob_gc_lock.write().await;
        let h_chk = hash.clone();
        let still_referenced = match state
            .db
            .run(move |conn| {
                let in_biz = db::is_blob_referenced_anywhere(conn, &h_chk, db::unix_secs())?;
                if in_biz {
                    return Ok(true);
                }
                let in_pending: Option<i64> = rusqlite::OptionalExtension::optional(conn.query_row(
                    "SELECT 1 FROM repl_pending_blobs WHERE blob_hash = ?1",
                    rusqlite::params![&h_chk],
                    |r| r.get(0),
                ))?;
                Ok(in_pending.is_some())
            })
            .await
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        if still_referenced {
            continue;
        }
        match state.blobs.delete(&hash).await {
            Ok(()) => deleted += 1,
            Err(e) => tracing::warn!("failed to delete orphan blob {hash}: {e:#}"),
        }
    }
    deleted
}
