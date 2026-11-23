//! GPUI main-thread bridge for local MCP requests.
//! Reads live editor state when a document is open, applies edits through
//! `editor::Core` so Cmd+Z undo works, creates pre-edit version history
//! snapshots when configured, syncs sibling windows, and saves cleanly.

use super::mcp::{ApiBackend, ApiCall, BoxFuture, DocListItem, SearchHit};
use crate::app::{SaveStatus, Workspace};
use crate::shared_store::SharedStore;
use fdl_core::agent::{
    apply_delete_blocks, apply_edit_block_text, apply_insert_blocks_with_format, apply_replace_blocks,
    block_ref_for_id, check_revision, document_revision, inspect_block, parse_agent_markdown,
    project_doc, AgentError,
};
use fdl_core::editor::core::{Core, TITLE};
use fdl_core::model::{Block, Document, RichText};
use fdl_core::store::{DocKind, Entry};
use gpui::*;
use quiet_sync_client::tokio::sync::{mpsc, oneshot};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct BridgeReq {
    pub call: ApiCall,
    pub snapshot_before_edit: bool,
    pub reply: oneshot::Sender<Result<Value, AgentError>>,
}

pub struct GpuiBackend {
    tx: mpsc::UnboundedSender<BridgeReq>,
    read_only: Arc<AtomicBool>,
    snapshot_before_edit: Arc<AtomicBool>,
}

impl GpuiBackend {
    pub fn new(
        tx: mpsc::UnboundedSender<BridgeReq>,
        read_only: Arc<AtomicBool>,
        snapshot_before_edit: Arc<AtomicBool>,
    ) -> Self {
        Self { tx, read_only, snapshot_before_edit }
    }
}

impl ApiBackend for GpuiBackend {
    fn execute(&self, call: ApiCall) -> BoxFuture<'_, Result<Value, AgentError>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = BridgeReq {
            call,
            snapshot_before_edit: self.snapshot_before_edit.load(Ordering::Acquire),
            reply: reply_tx,
        };
        let sent = self.tx.send(req).is_ok();
        Box::pin(async move {
            if !sent {
                return Err(AgentError::new("BRIDGE_CLOSED", "Quiet.md app bridge is unavailable"));
            }
            reply_rx
                .await
                .map_err(|_| AgentError::new("BRIDGE_CLOSED", "Quiet.md app bridge dropped request"))?
        })
    }

    fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }
}

/// Dispatch one request on the GPUI main thread.
pub fn handle_request(req: BridgeReq, cx: &mut App) {
    // Skip queued requests whose HTTP caller already timed out or disconnected.
    if req.reply.is_closed() {
        return;
    }
    let res = dispatch(req.call, req.snapshot_before_edit, cx);
    let _ = req.reply.send(res);
}

fn workspaces(cx: &App) -> Vec<WindowHandle<Workspace>> {
    cx.windows().into_iter().filter_map(|w| w.downcast::<Workspace>()).collect()
}

fn active_workspace(cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.active_window()
        .and_then(|w| w.downcast::<Workspace>())
        .or_else(|| workspaces(cx).into_iter().next())
}

fn doc_format_str(entry: &Entry) -> &'static str {
    if entry.meta.folder.is_some() {
        "folder"
    } else {
        match entry.meta.kind {
            DocKind::Doc => if entry.meta.markdown { "markdown" } else { "docxml" },
            DocKind::Board => "board",
            DocKind::Table => "table",
        }
    }
}

fn normalize_rel(p: &str) -> String {
    p.trim().replace('\\', "/").trim_matches('/').to_string()
}

fn entry_matches_folder(entry_rel: &str, folder_norm: &str, recursive: bool) -> bool {
    let e = normalize_rel(entry_rel);
    if folder_norm.is_empty() {
        if recursive {
            return true;
        }
        return !e.contains('/');
    }
    let Some(rest) = e.strip_prefix(folder_norm).and_then(|s| s.strip_prefix('/')) else {
        return false;
    };
    if recursive {
        !rest.is_empty()
    } else {
        !rest.is_empty() && !rest.contains('/')
    }
}

fn find_open_doc(doc_id: &str, cx: &App) -> Option<Document> {
    for h in workspaces(cx) {
        let Ok(ent) = h.entity(cx) else { continue };
        let ws = ent.read(cx);
        if ws.current.as_deref() == Some(doc_id) && ws.preview.is_none()
            && let Some(ed) = &ws.editor
        {
            return Some(ed.read(cx).core.doc.clone());
        }
    }
    None
}

fn load_doc_live_or_disk(store: &SharedStore, doc_id: &str, cx: &App) -> Result<(Document, bool, String), AgentError> {
    // Every open root's documents, as the sidebar lists them.
    let mut store = store.clone();
    let entries = store.list();
    let entry = entries.iter().find(|e| e.id == doc_id).ok_or_else(|| {
        AgentError::new("DOC_NOT_FOUND", format!("document {doc_id:?} not found"))
    })?;
    if entry.meta.folder.is_some() {
        return Err(AgentError::new("NOT_A_DOCUMENT", format!("{doc_id:?} is a folder, not a document")));
    }
    if entry.meta.kind != DocKind::Doc {
        return Err(AgentError::new(
            "UNSUPPORTED_DOC_KIND",
            format!("document {doc_id:?} is a {} (only rich-text and markdown documents are supported)", doc_format_str(entry)),
        ));
    }
    let is_md = entry.meta.markdown;
    let front = if is_md { store.front_matter_of(doc_id) } else { String::new() };
    let doc = match find_open_doc(doc_id, cx) {
        Some(live) => live,
        None => store.load(doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?,
    };
    Ok((doc, is_md, front))
}

fn dispatch(call: ApiCall, snapshot_before_edit: bool, cx: &mut App) -> Result<Value, AgentError> {
    let mut store = cx.global::<SharedStore>().clone();
    match call {
        ApiCall::GetCurrentContext => {
            let Some(h) = active_workspace(cx) else {
                return Ok(json!({ "doc_id": null }));
            };
            let Ok(ent) = h.entity(cx) else {
                return Ok(json!({ "doc_id": null }));
            };
            let ws = ent.read(cx);
            let Some(doc_id) = ws.current.clone() else {
                return Ok(json!({ "doc_id": null }));
            };
            let entry = ws.entries.iter().find(|e| e.id == doc_id).cloned();
            let path = entry.as_ref().map(|e| e.meta.rel.clone()).unwrap_or_default();
            let is_md = store.is_markdown(&doc_id);
            let front = if is_md { store.front_matter_of(&doc_id) } else { String::new() };
            let in_preview = ws.preview.is_some();
            if let Some(ed_ent) = ws.editor.as_ref() {
                let ed = ed_ent.read(cx);
                let doc = &ed.core.doc;
                let rev = document_revision(doc, is_md, &front);
                let sel_text = ed.core.selected_text();
                let selection = if sel_text.is_empty() {
                    Value::Null
                } else {
                    let s = ed.core.sel.anchor.block;
                    let e = ed.core.sel.head.block;
                    json!({
                        "text": sel_text,
                        "start_block_ref": if s == TITLE { Some("title".to_string()) } else { block_ref_for_id(doc, s) },
                        "end_block_ref": if e == TITLE { Some("title".to_string()) } else { block_ref_for_id(doc, e) },
                    })
                };
                return Ok(json!({
                    "doc_id": doc_id,
                    "title": doc.title.text,
                    "path": path,
                    "format": if is_md { "markdown" } else { "docxml" },
                    "revision": rev,
                    "read_only": ed.read_only,
                    "in_preview": in_preview,
                    "selection": selection,
                }));
            }
            Ok(json!({
                "doc_id": doc_id,
                "title": entry.as_ref().map(|e| e.title.clone()).unwrap_or_default(),
                "path": path,
                "format": entry.as_ref().map(doc_format_str).unwrap_or("docxml"),
                "in_preview": in_preview,
                "selection": null,
            }))
        }

        ApiCall::ListDocs { folder_path, recursive, filter, limit, offset } => {
            let folder_norm = folder_path.as_deref().map(normalize_rel).unwrap_or_default();
            let entries = store.list();
            let mut items: Vec<DocListItem> = entries
                .into_iter()
                .filter(|e| entry_matches_folder(&e.meta.rel, &folder_norm, recursive))
                .filter(|e| match filter.as_str() {
                    "starred" => e.meta.starred > 0,
                    "pinned" => e.meta.pinned,
                    _ => true,
                })
                .map(|e| DocListItem {
                    doc_id: e.id.clone(),
                    title: e.title.clone(),
                    path: e.meta.rel.clone(),
                    format: doc_format_str(&e).to_string(),
                    is_folder: e.meta.folder.is_some(),
                    pinned: e.meta.pinned,
                    starred: e.meta.starred > 0,
                    modified: e.meta.modified,
                })
                .collect();
            items.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
            let total = items.len();
            let page: Vec<DocListItem> = items.into_iter().skip(offset).take(limit).collect();
            let has_more = offset + page.len() < total;
            Ok(json!({
                "total": total,
                "offset": offset,
                "has_more": has_more,
                "items": page,
            }))
        }

        ApiCall::SearchDocs { query, folder_path, limit } => {
            let q = query.trim();
            if q.is_empty() {
                return Err(AgentError::new("INVALID_PARAMS", "query must not be empty"));
            }
            let q_low = q.to_ascii_lowercase();
            let folder_norm = folder_path.as_deref().map(normalize_rel).unwrap_or_default();
            let entries = store.list();
            let mut scored: Vec<(u8, u64, SearchHit)> = Vec::new();
            for e in entries {
                if e.meta.folder.is_some() { continue; }
                if !folder_norm.is_empty() && !entry_matches_folder(&e.meta.rel, &folder_norm, true) {
                    continue;
                }
                let (title, body) = match find_open_doc(&e.id, cx) {
                    Some(live) => (live.title.text.clone(), live.plain_text()),
                    None => (e.title.clone(), e.body.to_string()),
                };
                let t_low = title.to_ascii_lowercase();
                let b_low = body.to_ascii_lowercase();
                let in_title = t_low.contains(&q_low);
                let body_pos = b_low.find(&q_low);
                if !in_title && body_pos.is_none() {
                    continue;
                }
                let snippet = match body_pos {
                    Some(pos) => make_snippet(&body, pos, q.len()),
                    None => body.chars().take(120).collect(),
                };
                let rank = if in_title && body_pos.is_some() { 3 } else if in_title { 2 } else { 1 };
                scored.push((
                    rank,
                    e.meta.modified,
                    SearchHit {
                        doc_id: e.id.clone(),
                        title,
                        path: e.meta.rel.clone(),
                        format: doc_format_str(&e).to_string(),
                        snippet,
                    },
                ));
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            let hits: Vec<SearchHit> = scored.into_iter().take(limit).map(|(_, _, h)| h).collect();
            Ok(json!({ "query": q, "count": hits.len(), "hits": hits }))
        }

        ApiCall::ReadDoc { doc_id, opts } => {
            let (doc, is_md, front) = load_doc_live_or_disk(&store, &doc_id, cx)?;
            let proj = project_doc(&doc, is_md, &front, opts);
            Ok(json!({
                "doc_id": doc_id,
                "format": if is_md { "markdown" } else { "docxml" },
                "title": proj.title,
                "revision": proj.revision,
                "total_blocks": proj.total_blocks,
                "start_block": proj.start_block,
                "end_block": proj.end_block,
                "has_more": proj.has_more,
                "content": proj.content,
                "comments": proj.comments,
            }))
        }

        ApiCall::ReadBlock { doc_id, block_ref } => {
            let (doc, is_md, front) = load_doc_live_or_disk(&store, &doc_id, cx)?;
            let mut detail = inspect_block(&doc, &block_ref)?;
            if let Some(obj) = detail.as_object_mut() {
                obj.insert("doc_id".into(), json!(doc_id));
                obj.insert("revision".into(), json!(document_revision(&doc, is_md, &front)));
            }
            Ok(detail)
        }

        ApiCall::CreateDoc { title, markdown, folder_path, format } => {
            let parent_id = match folder_path.as_deref().map(normalize_rel).filter(|s| !s.is_empty()) {
                None => None,
                Some(fp) => {
                    let entries = store.list();
                    let found = entries.iter().find(|e| e.meta.folder.is_some() && normalize_rel(&e.meta.rel) == fp);
                    match found {
                        Some(f) => Some(f.id.clone()),
                        None => return Err(AgentError::new("FOLDER_NOT_FOUND", format!("folder {fp:?} does not exist"))),
                    }
                }
            };
            let blocks = if markdown.trim().is_empty() {
                vec![Block::text("")]
            } else {
                let parsed = parse_agent_markdown(&markdown)?;
                if parsed.is_empty() { vec![Block::text("")] } else { parsed }
            };
            let clean_title = title.replace(['\r', '\n'], " ");
            let doc = Document {
                title: RichText::plain(clean_title),
                icon: None,
                cover: None,
                blocks,
                comments: Vec::new(),
            };
            let use_md = match format.as_deref() {
                Some("markdown") => true,
                Some("docxml") => false,
                _ => crate::settings::quick_new_markdown(),
            };
            let doc_id = if use_md {
                store.create_markdown_in(&doc, "", parent_id.as_deref())
            } else {
                store.create_from_in(&doc, parent_id.as_deref())
            }
            .map_err(|e| AgentError::new("CREATE_FAILED", e.to_string()))?;

            refresh_all_sidebars(&mut store, cx);
            let saved = store.load(&doc_id).unwrap_or(doc);
            let rel = store.list().into_iter().find(|e| e.id == doc_id).map(|e| e.meta.rel).unwrap_or_default();
            let rev = document_revision(&saved, use_md, "");
            Ok(json!({
                "doc_id": doc_id,
                "path": rel,
                "format": if use_md { "markdown" } else { "docxml" },
                "revision": rev,
            }))
        }

        ApiCall::InsertBlocks { doc_id, expected_revision, position, anchor_block_ref, markdown } => {
            mutate_doc(&mut store, &doc_id, &expected_revision, snapshot_before_edit, cx, |core, is_md| {
                let refs = apply_insert_blocks_with_format(core, position, anchor_block_ref.as_deref(), &markdown, is_md)?;
                Ok(json!({ "inserted_refs": refs }))
            }, None)
        }

        ApiCall::ReplaceBlocks { doc_id, expected_revision, start_block_ref, end_block_ref, markdown, allow_rich_loss } => {
            mutate_doc(&mut store, &doc_id, &expected_revision, snapshot_before_edit, cx, |core, _| {
                let refs = apply_replace_blocks(core, &start_block_ref, end_block_ref.as_deref(), &markdown, allow_rich_loss)?;
                Ok(json!({ "replaced_refs": refs }))
            }, None)
        }

        ApiCall::DeleteBlocks { doc_id, expected_revision, start_block_ref, end_block_ref, allow_rich_loss } => {
            mutate_doc(&mut store, &doc_id, &expected_revision, snapshot_before_edit, cx, |core, _| {
                apply_delete_blocks(core, &start_block_ref, end_block_ref.as_deref(), allow_rich_loss)?;
                Ok(json!({}))
            }, None)
        }

        ApiCall::EditBlockText { doc_id, expected_revision, block_ref, old_text, new_text, occurrence } => {
            mutate_doc(&mut store, &doc_id, &expected_revision, snapshot_before_edit, cx, |core, _| {
                let updated = apply_edit_block_text(core, &block_ref, &old_text, &new_text, occurrence)?;
                Ok(json!({ "updated_block_ref": updated }))
            }, None)
        }

        ApiCall::UpdateDocTitle { doc_id, expected_revision, title } => {
            let clean = title.replace(['\r', '\n'], " ");
            mutate_doc(&mut store, &doc_id, &expected_revision, snapshot_before_edit, cx, |core, _| {
                core.agent_set_title(&clean);
                Ok(json!({ "title": clean }))
            }, Some(clean.clone()))
        }
    }
}

fn map_conditional_save_err(err: anyhow::Error) -> AgentError {
    let msg = err.to_string();
    if let Some(actual) = msg.strip_prefix("REVISION_CONFLICT:") {
        return AgentError::new(
            "REVISION_CONFLICT",
            format!("on-disk document changed externally (current revision {actual}); re-read before editing"),
        )
        .with_details(json!({ "current_revision": actual }));
    }
    AgentError::new("SAVE_FAILED", msg)
}

/// Apply a document mutation with revision validation (both in-memory and on-disk
/// under the store write lock), IME/preview/read-only guards, pre-edit version
/// history snapshot, disk-first persistence, and live multi-window hot reload
/// preserving Cmd+Z undo.
fn mutate_doc(
    store: &mut SharedStore,
    doc_id: &str,
    expected_revision: &str,
    snapshot_before_edit: bool,
    cx: &mut App,
    mutate: impl Fn(&mut Core, bool) -> Result<Value, AgentError>,
    rename_title: Option<String>,
) -> Result<Value, AgentError> {
    let entries = store.list();
    let entry = entries.iter().find(|e| e.id == doc_id).ok_or_else(|| {
        AgentError::new("DOC_NOT_FOUND", format!("document {doc_id:?} not found"))
    })?;
    if entry.meta.folder.is_some() || entry.meta.kind != DocKind::Doc {
        return Err(AgentError::new("UNSUPPORTED_DOC_KIND", "only rich-text and markdown documents can be edited"));
    }
    let is_md = entry.meta.markdown;
    let front = if is_md { store.front_matter_of(doc_id) } else { String::new() };

    let ws_handles = workspaces(cx);
    let mut open_windows = Vec::new();
    for h in &ws_handles {
        let Ok(ent) = h.entity(cx) else { continue };
        let ws = ent.read(cx);
        if ws.current.as_deref() == Some(doc_id) {
            if ws.preview.is_some() {
                return Err(AgentError::new("DOC_IN_PREVIEW", "document is currently in version history preview; exit preview before editing"));
            }
            if let Some(ed) = &ws.editor {
                let ed_ref = ed.read(cx);
                if ed_ref.read_only {
                    return Err(AgentError::new("DOC_READ_ONLY", "document is currently in read-only mode"));
                }
                if ed_ref.has_ime_composition() {
                    return Err(AgentError::new("EDITOR_BUSY_IME", "user is actively composing text with an IME; retry shortly"));
                }
                open_windows.push(*h);
            }
        }
    }

    if !open_windows.is_empty() {
        // Flush any pending unsaved typing first so disk and pre-edit snapshot reflect user edits.
        for h in &open_windows {
            let ok = h
                .update(cx, |ws, _, cx| if ws.unsaved { ws.flush(cx) } else { true })
                .unwrap_or(false);
            if !ok {
                return Err(AgentError::new("SAVE_FAILED", "could not save pending local edits before applying MCP change"));
            }
        }
        // Validate revision & dry-run mutation on a clone first so failed ops never pollute undo or UI.
        let primary_ed = open_windows[0]
            .entity(cx)
            .ok()
            .and_then(|e| e.read(cx).editor.clone())
            .ok_or_else(|| AgentError::new("EDITOR_UNAVAILABLE", "open editor disappeared"))?;
        let mut dry_core = primary_ed.read(cx).core.clone();
        check_revision(&dry_core.doc, is_md, &front, expected_revision)?;
        let mut extra = mutate(&mut dry_core, is_md)?;

        // Verify disk revision before snapshotting so we don't snapshot stale state on conflict.
        let disk_doc = store.load(doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
        check_revision(&disk_doc, is_md, &front, expected_revision)?;

        if snapshot_before_edit {
            let _ = store.save_version_now(doc_id);
        }

        // Persist to disk FIRST under the store write lock checking disk revision atomically.
        let mut meta = store
            .save_if_revision_matches(doc_id, &dry_core.doc, expected_revision)
            .map_err(map_conditional_save_err)?;

        let mut persisted_title: Option<String> = None;
        if is_md && let Some(ref new_title) = rename_title {
            meta = store
                .rename(doc_id, new_title)
                .map_err(|e| AgentError::new("RENAME_FAILED", e.to_string()))?;
            let reloaded = store.load(doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
            persisted_title = Some(reloaded.title.text.clone());
            if let Value::Object(ref mut m) = extra {
                m.insert("title".into(), json!(reloaded.title.text));
            }
        }

        // Disk write succeeded: now apply to primary live editor (recording one clean Cmd+Z step).
        let mut new_core: Option<Core> = None;
        primary_ed.update(cx, |ed, cx| {
            ed.core.break_coalescing();
            if mutate(&mut ed.core, is_md).is_ok() {
                if let Some(ref actual_title) = persisted_title
                    && ed.core.doc.title.text != *actual_title
                {
                    ed.core.doc.title = RichText::plain(actual_title.clone());
                    ed.core.fix_selection();
                }
                ed.finish_agent_edit(cx);
                new_core = Some(ed.core.clone());
            }
        });
        let updated_core = new_core.ok_or_else(|| AgentError::new("EDIT_FAILED", "failed to apply mutation to open editor"))?;

        // Sync sibling windows showing the same document.
        for h in open_windows.iter().skip(1) {
            let _ = h.update(cx, |ws, _, cx| {
                if let Some(ed) = &ws.editor {
                    ed.update(cx, |e, cx| e.receive_document(&updated_core, cx));
                }
            });
        }

        let (title, icon, body, reminders, links) = (
            updated_core.doc.title.text.clone(),
            updated_core.doc.icon.clone(),
            updated_core.doc.plain_text(),
            updated_core.doc.reminders(),
            updated_core.doc.doc_links(),
        );
        let refreshed_entries = rename_title.is_some().then(|| store.list());
        for h in &ws_handles {
            let _ = h.update(cx, |ws, _, cx| {
                if let Some(ref all) = refreshed_entries {
                    ws.entries = all.clone();
                    ws.sort();
                } else if let Some(e) = ws.entries.iter_mut().find(|e| e.id == doc_id) {
                    e.title = title.clone();
                    e.icon = icon.clone();
                    e.body = body.clone().into();
                    e.reminders = reminders.clone();
                    e.links = links.clone();
                    e.meta = meta.clone();
                }
                if ws.current.as_deref() == Some(doc_id) {
                    ws.clear_unsaved_state();
                    ws.status = SaveStatus::Saved;
                }
                cx.notify();
            });
        }

        let new_front = if is_md { store.front_matter_of(doc_id) } else { front };
        let new_rev = document_revision(&updated_core.doc, is_md, &new_front);
        let rel = store.list().into_iter().find(|e| e.id == doc_id).map(|e| e.meta.rel).unwrap_or_default();
        return Ok(merge_reply(doc_id, &new_rev, &rel, extra));
    }

    // Document is not open in any window: edit via Core and save conditionally under write lock.
    let doc = store.load(doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
    check_revision(&doc, is_md, &front, expected_revision)?;
    let mut core = Core::new(doc);
    let mut extra = mutate(&mut core, is_md)?;
    if snapshot_before_edit {
        let _ = store.save_version_now(doc_id);
    }
    store
        .save_if_revision_matches(doc_id, &core.doc, expected_revision)
        .map_err(map_conditional_save_err)?;
    let persisted_doc = if is_md && let Some(ref new_title) = rename_title {
        store
            .rename(doc_id, new_title)
            .map_err(|e| AgentError::new("RENAME_FAILED", e.to_string()))?;
        let reloaded = store.load(doc_id).map_err(|e| AgentError::new("LOAD_FAILED", e.to_string()))?;
        if let Value::Object(ref mut m) = extra {
            m.insert("title".into(), json!(reloaded.title.text));
        }
        reloaded
    } else {
        core.doc
    };
    refresh_all_sidebars(store, cx);
    let new_front = if is_md { store.front_matter_of(doc_id) } else { front };
    let new_rev = document_revision(&persisted_doc, is_md, &new_front);
    let rel = store.list().into_iter().find(|e| e.id == doc_id).map(|e| e.meta.rel).unwrap_or_default();
    Ok(merge_reply(doc_id, &new_rev, &rel, extra))
}

fn merge_reply(doc_id: &str, revision: &str, path: &str, extra: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("doc_id".into(), json!(doc_id));
    map.insert("revision".into(), json!(revision));
    map.insert("path".into(), json!(path));
    if let Value::Object(extra_map) = extra {
        for (k, v) in extra_map {
            map.insert(k, v);
        }
    }
    Value::Object(map)
}

fn refresh_all_sidebars(store: &mut SharedStore, cx: &mut App) {
    let entries = store.list();
    for h in workspaces(cx) {
        let _ = h.update(cx, |ws, _, cx| {
            if ws.rename.is_some() || ws.move_dlg.is_some() { return; }
            ws.entries = entries.clone();
            ws.sort();
            cx.notify();
        });
    }
}

fn make_snippet(body: &str, byte_pos: usize, q_len: usize) -> String {
    let start = body[..byte_pos].char_indices().rev().nth(40).map(|(i, _)| i).unwrap_or(0);
    let after = (byte_pos + q_len).min(body.len());
    let after = body.ceil_char_boundary(after);
    let end = body[after..].char_indices().nth(80).map(|(i, _)| after + i).unwrap_or(body.len());
    let mut s = body[start..end].replace('\n', " ");
    if start > 0 { s.insert(0, '…'); }
    if end < body.len() { s.push('…'); }
    s
}
