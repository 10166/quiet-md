use gpui::*;
use crate::app::Workspace;

#[derive(Clone, Debug)]
pub struct ConflictBanner {
    pub conflict_doc_id: String,
    pub title: String,
    pub device_name: String,
}

#[derive(Clone, Debug)]
pub enum Hint {
    UpsertDoc(String),
    UpsertFolder(String),
    UpsertTrash(String),
    DeleteTrash(String),
    UpsertHistory(String, String),
    DeleteHistory(String, String),
    Config(String),
    Scan,
}

pub fn hint_upsert_doc(id: &str) -> Hint { Hint::UpsertDoc(id.to_string()) }
pub fn hint_upsert_history(doc_id: &str, name: &str) -> Hint { Hint::UpsertHistory(doc_id.to_string(), name.to_string()) }
pub fn hint_delete_trash(id: &str) -> Hint { Hint::DeleteTrash(id.to_string()) }
pub fn hint_scan() -> Hint { Hint::Scan }

pub fn notify_hints(hints: Vec<Hint>) { imp::notify_hints(hints); }
pub fn notify_user_activity() { imp::notify_user_activity(); }
pub fn notify_config(key: &str) { imp::notify_hints(vec![Hint::Config(key.to_string())]); }
pub fn notify_scan() { imp::notify_hints(vec![Hint::Scan]); }
pub fn notify_upsert_doc(id: &str) { imp::notify_hints(vec![Hint::UpsertDoc(id.to_string())]); }
pub fn notify_upsert_folder(id: &str) { imp::notify_hints(vec![Hint::UpsertFolder(id.to_string())]); }
pub fn notify_entity_upsert(id: &str, is_folder: bool) {
    if is_folder { notify_upsert_folder(id); } else { notify_upsert_doc(id); }
}
pub fn notify_upsert_history(doc_id: &str, name: &str) {
    imp::notify_hints(vec![Hint::UpsertHistory(doc_id.to_string(), name.to_string())]);
}
pub fn notify_delete_history(doc_id: &str, name: &str) {
    imp::notify_hints(vec![Hint::DeleteHistory(doc_id.to_string(), name.to_string())]);
}
pub fn notify_delete_docs(ids: &[String]) {
    if ids.is_empty() { return; }
    let mut hints = Vec::with_capacity(ids.len() + 1);
    for id in ids {
        hints.push(Hint::UpsertTrash(id.clone()));
    }
    hints.push(Hint::Scan);
    imp::notify_hints(hints);
}
pub fn notify_purged_trash(ids: &[String]) {
    let hints = ids.iter().map(|id| Hint::DeleteTrash(id.clone())).collect();
    imp::notify_hints(hints);
}

pub fn init(cx: &mut App) { imp::init(cx); }
pub fn reinit_for_library(cx: &mut App) { imp::reinit_for_library(cx); }
pub fn flush_before_quit(cx: &mut App) { imp::flush_before_quit(cx); }
pub fn set_active_doc(doc_id: Option<String>, cx: &App) { imp::set_active_doc(doc_id, cx); }
pub fn status_pill(cx: &mut Context<Workspace>) -> Option<AnyElement> { imp::status_pill(cx) }
pub fn other_device_viewing(doc_id: &str, cx: &App) -> Vec<String> { imp::other_device_viewing(doc_id, cx) }
pub struct PendingRemoteDoc(imp::PendingRemoteDocInner);
pub struct PendingRemoteBoard(imp::PendingRemoteBoardInner);
pub fn apply_pending_remote_doc(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) { imp::apply_pending_remote_doc(ws, window, cx); }
pub fn apply_pending_remote_board(ws: &mut Workspace, cx: &mut Context<Workspace>) -> bool { imp::apply_pending_remote_board(ws, cx) }

#[cfg(not(target_family = "wasm"))]
pub use imp::{ensure_handle, handle, online_devices, status};

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    pub struct PendingRemoteDocInner;
    pub struct PendingRemoteBoardInner;
    pub fn notify_hints(_: Vec<Hint>) {}
    pub fn notify_user_activity() {}
    pub fn init(_: &mut App) {}
    pub fn reinit_for_library(_: &mut App) {}
    pub fn flush_before_quit(_: &mut App) {}
    pub fn set_active_doc(_: Option<String>, _: &App) {}
    pub fn status_pill(_: &mut Context<Workspace>) -> Option<AnyElement> { None }
    pub fn other_device_viewing(_: &str, _: &App) -> Vec<String> { Vec::new() }
    pub fn apply_pending_remote_doc(_: &mut Workspace, _: &mut Window, _: &mut Context<Workspace>) {}
    pub fn apply_pending_remote_board(_: &mut Workspace, _: &mut Context<Workspace>) -> bool { false }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::*;
    use crate::app::View;
    use crate::shared_store::SharedStore;
    use crate::theme::Theme;
    use crate::ui::{TipExt, TipSide, TitleBarExt, icon};
    use anyhow::Result;
    use fdl_core::merge::{merge_boards, merge_documents, merge_markdown};
    use fdl_core::store::{is_safe_doc_ext, is_safe_entity_id, Meta as StoreMeta, Store, SyncInventory};
    use fdl_core::whiteboard::Board;
    use quiet_sync_client::adapter::{
        apply_remote_sync, check_expected_local_hash, config_entity_id, history_entity_id, read_asset_sync,
        read_local_entity_sync, store_meta_from_sync, trash_entity_id, write_asset_sync,
        ApplyReport, BoxFuture, IncomingApply, LiveConflict, LocalEntity, LocalLibraryAdapter,
    };
    use quiet_sync_client::{LocalChangeHint, SyncEvent, SyncHandle, SyncStatus};
    use quiet_sync_proto::api::OnlineDevice;
    use quiet_sync_proto::entity::{EntityKind, SyncMeta};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use quiet_sync_client::tokio;
    use quiet_sync_client::tokio::sync::{mpsc, oneshot};

    static ACTIVE_HANDLE: Mutex<Option<SyncHandle>> = Mutex::new(None);

    pub struct PendingRemoteDocInner {
        pub id: String,
        pub file_name: String,
        pub parent_id: Option<String>,
        pub ext: String,
        pub meta: SyncMeta,
        pub bytes: Vec<u8>,
        pub base_bytes: Option<Vec<u8>>,
        pub device_name: String,
    }

    pub struct PendingRemoteBoardInner {
        pub doc_id: String,
        pub file_name: String,
        pub parent_id: Option<String>,
        pub meta: SyncMeta,
        pub base: Option<Arc<Board>>,
        pub remote: Arc<Board>,
        pub device_name: String,
    }

    enum AdapterReq {
        GetStoreSnapshot(oneshot::Sender<Store>),
        ApplyRemote(Vec<IncomingApply>, oneshot::Sender<Result<ApplyReport>>),
        AdoptId(String, String, oneshot::Sender<Result<()>>),
        CreateConflictCopy(String, String, Vec<u8>, String, oneshot::Sender<Result<String>>),
    }

    struct GpuiLibraryAdapter {
        root: PathBuf,
        tx: mpsc::UnboundedSender<AdapterReq>,
    }

    impl LocalLibraryAdapter for GpuiLibraryAdapter {
        fn read_inventory(&self) -> BoxFuture<'_, Result<SyncInventory>> {
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send(AdapterReq::GetStoreSnapshot(tx));
            Box::pin(async move {
                let mut store = rx.await.map_err(|_| anyhow::anyhow!("sync adapter closed"))?;
                tokio::task::spawn_blocking(move || store.sync_inventory())
                    .await
                    .map_err(|e| anyhow::anyhow!("inventory task failed: {e}"))?
            })
        }

        fn read_entity(&self, kind: EntityKind, entity_id: &str) -> BoxFuture<'_, Result<Option<LocalEntity>>> {
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send(AdapterReq::GetStoreSnapshot(tx));
            let entity_id = entity_id.to_string();
            Box::pin(async move {
                let mut store = rx.await.map_err(|_| anyhow::anyhow!("sync adapter closed"))?;
                tokio::task::spawn_blocking(move || read_local_entity_sync(&mut store, kind, &entity_id))
                    .await
                    .map_err(|e| anyhow::anyhow!("read_entity task failed: {e}"))?
            })
        }

        fn read_asset(&self, rel_path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>>> {
            let root = self.root.clone();
            let rel_path = rel_path.to_string();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || read_asset_sync(&root, &rel_path))
                    .await
                    .map_err(|e| anyhow::anyhow!("read_asset task failed: {e}"))?
            })
        }

        fn write_asset(&self, rel_path: &str, bytes: &[u8]) -> BoxFuture<'_, Result<()>> {
            let root = self.root.clone();
            let rel_path = rel_path.to_string();
            let bytes = bytes.to_vec();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || write_asset_sync(&root, &rel_path, &bytes))
                    .await
                    .map_err(|e| anyhow::anyhow!("write_asset task failed: {e}"))?
            })
        }

        fn apply_remote(&self, ops: Vec<IncomingApply>) -> BoxFuture<'_, Result<ApplyReport>> {
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send(AdapterReq::ApplyRemote(ops, tx));
            Box::pin(async move { rx.await.map_err(|_| anyhow::anyhow!("sync adapter closed"))? })
        }

        fn adopt_id(&self, old_id: &str, new_id: &str) -> BoxFuture<'_, Result<()>> {
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send(AdapterReq::AdoptId(old_id.to_string(), new_id.to_string(), tx));
            Box::pin(async move { rx.await.map_err(|_| anyhow::anyhow!("sync adapter closed"))? })
        }

        fn create_conflict_copy(&self, orig_id: &str, ext: &str, raw_bytes: &[u8], device_label: &str) -> BoxFuture<'_, Result<String>> {
            let (tx, rx) = oneshot::channel();
            let _ = self.tx.send(AdapterReq::CreateConflictCopy(
                orig_id.to_string(),
                ext.to_string(),
                raw_bytes.to_vec(),
                device_label.to_string(),
                tx,
            ));
            Box::pin(async move { rx.await.map_err(|_| anyhow::anyhow!("sync adapter closed"))? })
        }
    }

    pub struct SyncManager {
        pub handle: Option<SyncHandle>,
        pub sync_dir: PathBuf,
        pub root: PathBuf,
        pub status: SyncStatus,
        pub online: Vec<OnlineDevice>,
        adapter_rx: Option<Rc<RefCell<mpsc::UnboundedReceiver<AdapterReq>>>>,
        _tasks: Vec<Task<()>>,
    }

    impl Global for SyncManager {}

    fn to_client_hint(h: Hint) -> LocalChangeHint {
        match h {
            Hint::UpsertDoc(id) => LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: id },
            Hint::UpsertFolder(id) => LocalChangeHint::Upsert { kind: EntityKind::Folder, entity_id: id },
            Hint::UpsertTrash(id) => LocalChangeHint::Upsert { kind: EntityKind::TrashItem, entity_id: trash_entity_id(&id) },
            Hint::DeleteTrash(id) => LocalChangeHint::Delete { kind: EntityKind::TrashItem, entity_id: trash_entity_id(&id) },
            Hint::UpsertHistory(doc, name) => LocalChangeHint::Upsert { kind: EntityKind::HistorySnapshot, entity_id: history_entity_id(&doc, &name) },
            Hint::DeleteHistory(doc, name) => LocalChangeHint::Delete { kind: EntityKind::HistorySnapshot, entity_id: history_entity_id(&doc, &name) },
            Hint::Config(key) => LocalChangeHint::Upsert { kind: EntityKind::Config, entity_id: config_entity_id(&key) },
            Hint::Scan => LocalChangeHint::ScanAll,
        }
    }

    pub fn notify_hints(hints: Vec<Hint>) {
        if hints.is_empty() { return; }
        if let Some(h) = ACTIVE_HANDLE.lock().unwrap().as_ref() {
            h.notify_changes(hints.into_iter().map(to_client_hint).collect());
        }
    }

    pub fn notify_user_activity() {
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        let now = Instant::now();
        {
            let mut last = LAST.lock().unwrap();
            if last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(500)) {
                return;
            }
            *last = Some(now);
        }
        if let Some(h) = ACTIVE_HANDLE.lock().unwrap().as_ref()
            && h.sync_interval_secs() > 0
        {
            h.notify_user_activity();
        }
    }

    pub fn handle(cx: &App) -> Option<SyncHandle> {
        cx.try_global::<SyncManager>().and_then(|m| m.handle.clone())
    }

    pub fn ensure_handle(cx: &mut App) -> Option<SyncHandle> {
        if !cx.has_global::<SyncManager>() {
            init(cx);
        }
        if let Some(h) = cx.try_global::<SyncManager>().and_then(|m| m.handle.clone()) {
            return Some(h);
        }
        let (sync_dir, root) = cx.try_global::<SyncManager>().map(|m| (m.sync_dir.clone(), m.root.clone()))?;
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let adapter = Arc::new(GpuiLibraryAdapter { root, tx: req_tx });
        let (handle, mut ev_rx) = match SyncHandle::start(sync_dir, adapter) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("sync init failed: {e:#}");
                return None;
            }
        };
        *ACTIVE_HANDLE.lock().unwrap() = Some(handle.clone());
        let status = handle.status();
        let online = handle.online_devices();
        let adapter_rx = Rc::new(RefCell::new(req_rx));
        let pump_rx = adapter_rx.clone();

        let adapter_task = cx.spawn(async move |cx| {
            loop {
                let req = std::future::poll_fn(|tcx| pump_rx.borrow_mut().poll_recv(tcx)).await;
                let Some(req) = req else { break };
                cx.update(|cx| handle_adapter_req(req, cx));
            }
        });

        let event_task = cx.spawn(async move |cx| {
            loop {
                let ev = std::future::poll_fn(|tcx| ev_rx.poll_recv(tcx)).await;
                let Some(ev) = ev else { break };
                cx.update(|cx| handle_sync_event(ev, cx));
            }
        });

        if cx.has_global::<SyncManager>() {
            let mgr = cx.global_mut::<SyncManager>();
            mgr.handle = Some(handle.clone());
            mgr.status = status;
            mgr.online = online;
            mgr.adapter_rx = Some(adapter_rx);
            mgr._tasks = vec![adapter_task, event_task];
        }
        Some(handle)
    }

    pub fn status(cx: &App) -> SyncStatus {
        cx.try_global::<SyncManager>().map_or(SyncStatus::Unconfigured, |m| m.status.clone())
    }

    pub fn online_devices(cx: &App) -> Vec<OnlineDevice> {
        cx.try_global::<SyncManager>().map_or_default(|m| m.online.clone())
    }

    pub fn init(cx: &mut App) {
        let Some(store) = cx.try_global::<SharedStore>().cloned() else { return };
        let sync_dir = store.sync_local_dir();
        let root = store.root();
        let configured = sync_dir.join("credentials.json").exists() || sync_dir.join("state.db").exists();
        cx.set_global(SyncManager {
            handle: None,
            sync_dir,
            root,
            status: SyncStatus::Unconfigured,
            online: Vec::new(),
            adapter_rx: None,
            _tasks: Vec::new(),
        });
        if configured {
            let _ = ensure_handle(cx);
        }
    }

    pub fn reinit_for_library(cx: &mut App) {
        flush_before_quit(cx);
        if cx.has_global::<SyncManager>() {
            cx.remove_global::<SyncManager>();
        }
        init(cx);
        refresh_all_workspaces(cx);
    }

    pub fn flush_before_quit(cx: &mut App) {
        for w in cx.windows() {
            if let Some(h) = w.downcast::<Workspace>()
                && h.is_active(cx).is_some()
            {
                let _ = h.update(cx, |ws, _, cx| { let _ = ws.flush(cx); });
            }
        }
        *ACTIVE_HANDLE.lock().unwrap() = None;
        let Some(mgr) = cx.try_global::<SyncManager>() else { return };
        let Some(handle) = mgr.handle.clone() else { return };
        let Some(adapter_rx) = mgr.adapter_rx.clone() else { return };
        let Some(mut done_rx) = handle.begin_shutdown() else { return };
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            while let Ok(req) = adapter_rx.borrow_mut().try_recv() {
                handle_adapter_req(req, cx);
            }
            if done_rx.try_recv().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        while let Ok(req) = adapter_rx.borrow_mut().try_recv() {
            handle_adapter_req(req, cx);
        }
    }

    pub fn set_active_doc(doc_id: Option<String>, cx: &App) {
        if let Some(h) = cx.try_global::<SyncManager>().and_then(|m| m.handle.as_ref()) {
            h.set_active_doc(doc_id);
        }
    }

    pub fn other_device_viewing(doc_id: &str, cx: &App) -> Vec<String> {
        let Some(m) = cx.try_global::<SyncManager>() else { return Vec::new() };
        let Some(handle) = m.handle.as_ref() else { return Vec::new() };
        let self_id = handle.credentials().map(|c| c.device_id);
        m.online
            .iter()
            .filter(|d| Some(&d.device_id) != self_id.as_ref() && d.active_entity_id.as_deref() == Some(doc_id))
            .map(|d| d.device_name.clone())
            .collect()
    }

    pub fn status_pill(cx: &mut Context<Workspace>) -> Option<AnyElement> {
        let status = cx.try_global::<SyncManager>()?.status.clone();
        if matches!(status, SyncStatus::Unconfigured) {
            return None;
        }
        let online_cnt = cx.try_global::<SyncManager>().map_or(0, |m| m.online.len());
        let (icon_name, color, tip) = match &status {
            SyncStatus::Synced { .. } => {
                let msg = if online_cnt > 1 {
                    format!("{} · {}", crate::settings::tr("已云端同步", "Synced to cloud"), crate::localized_format!("{} 台设备在线", "{} devices online", online_cnt))
                } else {
                    crate::settings::tr("已云端同步", "Synced to cloud").to_string()
                };
                ("cloud-check", Theme::icon(), msg)
            }
            SyncStatus::Pending { pending } => {
                let msg = crate::localized_format!("{} 项待同步（停止输入后自动上传）", "{} pending (uploads when idle)", pending);
                ("cloud", Theme::text_caption(), msg)
            }
            SyncStatus::Connecting | SyncStatus::Syncing { .. } => {
                ("cloud", Theme::primary(), crate::settings::tr("正在同步…", "Syncing…").to_string())
            }
            SyncStatus::Paused => {
                ("cloud-off", Theme::text_caption(), crate::settings::tr("同步已暂停", "Sync paused").to_string())
            }
            SyncStatus::Offline { pending } => {
                let msg = if *pending > 0 {
                    crate::localized_format!("离线（{} 项待同步）", "Offline ({} pending)", pending)
                } else {
                    crate::settings::tr("离线（等待连接）", "Offline").to_string()
                };
                ("cloud-off", Theme::text_caption(), msg)
            }
            SyncStatus::LoggedOut | SyncStatus::Unlinked => {
                ("cloud-off", Theme::text_caption(), crate::settings::tr("未连接文档库", "Sync not linked").to_string())
            }
            SyncStatus::AuthExpired => {
                ("cloud-off", crate::theme::c(crate::theme::craft::R400), crate::settings::tr("同步登录已过期，点击重新登录", "Sync sign-in expired").to_string())
            }
            SyncStatus::Error(err) => {
                ("cloud-off", crate::theme::c(crate::theme::craft::R400), format!("{}: {err}", crate::settings::tr("同步出错", "Sync error")))
            }
            SyncStatus::Unconfigured => unreachable!(),
        };

        Some(
            div()
                .id("sync-status-pill")
                .size(px(24.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(Theme::fill_hover()))
                .no_drag()
                .tip_label(tip, TipSide::Below)
                .child(icon(icon_name, 14., color))
                .on_click(cx.listener(|w, _, _, cx| {
                    w.open_settings(crate::settings::Page::Sync, cx);
                }))
                .into_any_element(),
        )
    }

    fn handle_sync_event(ev: SyncEvent, cx: &mut App) {
        if cx.has_global::<SyncManager>() {
            let mgr = cx.global_mut::<SyncManager>();
            match &ev {
                SyncEvent::Status(st) => mgr.status = st.clone(),
                SyncEvent::Presence(devs) => mgr.online = devs.clone(),
                SyncEvent::AuthChanged => {
                    if let Some(h) = mgr.handle.as_ref() {
                        mgr.status = h.status();
                        mgr.online = h.online_devices();
                    }
                }
                _ => {}
            }
        }
        match &ev {
            SyncEvent::Status(_) | SyncEvent::Presence(_) | SyncEvent::AuthChanged | SyncEvent::Applied(_) => {
                for w in cx.windows() {
                    if let Some(h) = w.downcast::<Workspace>()
                        && h.is_active(cx).is_some()
                    {
                        let _ = h.update(cx, |_, _, cx| cx.notify());
                    }
                }
            }
            SyncEvent::ConflictNotice { doc_id, conflict_doc_id, title, device_name } => {
                let entries = cx.try_global::<SharedStore>().map(|s| s.clone().list());
                for w in cx.windows() {
                    if let Some(h) = w.downcast::<Workspace>()
                        && h.is_active(cx).is_some()
                    {
                        let entries_c = entries.clone();
                        let _ = h.update(cx, |ws, _, cx| {
                            if ws.current.as_deref() == Some(doc_id.as_str()) {
                                ws.conflict_banner = Some(ConflictBanner {
                                    conflict_doc_id: conflict_doc_id.clone(),
                                    title: title.clone(),
                                    device_name: device_name.clone(),
                                });
                            }
                            if let Some(ent) = entries_c {
                                ws.entries = ent;
                                ws.sort();
                            }
                            ws.show_toast(&format!("{}: {} ({})", crate::settings::tr("发现同步冲突，已保留本地修改并创建副本", "Sync conflict resolved with a conflict copy"), title, device_name), cx);
                            cx.notify();
                        });
                    }
                }
            }
        }
    }

    fn refresh_all_workspaces(cx: &mut App) {
        let mut store = cx.try_global::<SharedStore>().cloned();
        let entries = store.as_mut().map(|s| s.list());
        let trash = store.as_mut().map(|s| s.list_trash());
        for w in cx.windows() {
            if let Some(h) = w.downcast::<Workspace>()
                && h.is_active(cx).is_some()
            {
                let entries_c = entries.clone();
                let trash_c = trash.clone();
                let _ = h.update(cx, |ws, _, cx| {
                    if let Some(ent) = entries_c { ws.entries = ent; }
                    if let Some(tr) = trash_c { ws.trash = tr; }
                    ws.sort();
                    ws.sync_sub_pages(cx);
                    cx.notify();
                });
            }
        }
    }

    fn handle_adapter_req(req: AdapterReq, cx: &mut App) {
        let Some(mut store) = cx.try_global::<SharedStore>().cloned() else { return };
        match req {
            AdapterReq::GetStoreSnapshot(reply) => {
                let _ = reply.send(store.tree_writer());
            }
            AdapterReq::AdoptId(old_id, new_id, reply) => {
                let res = store.adopt_entity_id(&old_id, &new_id);
                if res.is_ok() {
                    let entries = store.list();
                    for w in cx.windows() {
                        if let Some(h) = w.downcast::<Workspace>()
                            && h.is_active(cx).is_some()
                        {
                            let entries_c = entries.clone();
                            let _ = h.update(cx, |ws, _, cx| {
                                if ws.current.as_deref() == Some(old_id.as_str()) {
                                    ws.current = Some(new_id.clone());
                                }
                                if ws.folder_current.as_deref() == Some(old_id.as_str()) {
                                    ws.folder_current = Some(new_id.clone());
                                }
                                ws.entries = entries_c;
                                ws.sort();
                                cx.notify();
                            });
                        }
                    }
                }
                let _ = reply.send(res);
            }
            AdapterReq::CreateConflictCopy(orig_id, ext, raw_bytes, device_label, reply) => {
                let _ = reply.send(store.create_conflict_copy(&orig_id, &ext, &raw_bytes, &device_label));
            }
            AdapterReq::ApplyRemote(ops, reply) => {
                let _ = reply.send(apply_remote_in_app(&mut store, ops, cx));
            }
        }
    }

    fn apply_remote_in_app(store: &mut SharedStore, ops: Vec<IncomingApply>, cx: &mut App) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for op in ops {
            match op {
                IncomingApply::Document {
                    id,
                    file_name,
                    parent_id,
                    ext,
                    meta,
                    bytes,
                    deleted,
                    preserve_in_local_trash,
                    expected_local_hash,
                    base_bytes,
                    device_name,
                } => {
                    apply_remote_document_op(
                        store,
                        &mut report,
                        id,
                        file_name,
                        parent_id,
                        ext,
                        meta,
                        bytes,
                        deleted,
                        preserve_in_local_trash,
                        expected_local_hash,
                        base_bytes,
                        device_name,
                        cx,
                    )?;
                }
                other => {
                    let sub = store.with(|s| apply_remote_sync(s, &[other]))?;
                    report.tree_changed |= sub.tree_changed;
                    report.trash_changed |= sub.trash_changed;
                    for r in sub.skipped_raced_ids {
                        if !report.skipped_raced_ids.contains(&r) { report.skipped_raced_ids.push(r); }
                    }
                    for a in sub.requeue_assets {
                        if !report.requeue_assets.contains(&a) { report.requeue_assets.push(a); }
                    }
                    for d in sub.history_docs {
                        if !report.history_docs.contains(&d) { report.history_docs.push(d); }
                    }
                    for k in sub.config_keys {
                        if !report.config_keys.contains(&k) { report.config_keys.push(k); }
                    }
                }
            }
        }

        if !report.config_keys.is_empty() {
            reload_synced_configs(&report.config_keys, cx);
        }

        if report.tree_changed || report.trash_changed || !report.history_docs.is_empty() {
            let entries = report.tree_changed.then(|| store.list());
            let trash = report.trash_changed.then(|| store.list_trash());
            for w in cx.windows() {
                if let Some(h) = w.downcast::<Workspace>()
                    && h.is_active(cx).is_some()
                {
                    let entries_c = entries.clone();
                    let trash_c = trash.clone();
                    let updated_c = report.updated_docs.clone();
                    let _ = h.update(cx, |ws, window, cx| {
                        if let Some(ent) = entries_c { ws.entries = ent; }
                        if let Some(tr) = trash_c { ws.trash = tr; }
                        if report.tree_changed {
                            ws.sort();
                            ws.sync_sub_pages(cx);
                        }
                        if ws.current.as_ref().is_some_and(|c| updated_c.contains(c)) {
                            ws.sync_document(window, cx);
                        }
                        cx.notify();
                    });
                }
            }
        }

        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_remote_document_op(
        store: &mut SharedStore,
        report: &mut ApplyReport,
        id: String,
        file_name: String,
        parent_id: Option<String>,
        ext: String,
        meta: SyncMeta,
        bytes: Vec<u8>,
        deleted: bool,
        preserve_in_local_trash: bool,
        expected_local_hash: Option<Option<String>>,
        base_bytes: Option<Vec<u8>>,
        device_name: String,
        cx: &mut App,
    ) -> Result<()> {
        anyhow::ensure!(is_safe_entity_id(&id), "无效的文档实体标识: {id}");
        if !deleted {
            anyhow::ensure!(is_safe_doc_ext(&ext), "无效的文档扩展名: {ext}");
        }
        let store_meta = store_meta_from_sync(&meta);
        let ws_handles: Vec<WindowHandle<Workspace>> = cx.windows().into_iter().filter_map(|w| w.downcast::<Workspace>()).collect();

        if deleted {
            // Preserve unsaved live edits from any active window viewing this document.
            let mut preserved: Option<(WindowHandle<Workspace>, String)> = None;
            for h in &ws_handles {
                if h.is_active(cx).is_none() { continue; }
                let unsaved_bytes = h.update(cx, |ws, _, cx| {
                    if ws.current.as_deref() == Some(id.as_str()) && ws.unsaved {
                        extract_live_bytes(store, &id, &ext, ws, cx)
                    } else {
                        None
                    }
                }).unwrap_or(None);
                if let Some(live_bytes) = unsaved_bytes {
                    let copy_id = store.create_conflict_copy(&id, &ext, &live_bytes, crate::settings::tr("本地未保存副本", "Local unsaved copy"))?;
                    report.merged_back_docs.push((copy_id.clone(), Vec::new(), SyncMeta::default()));
                    preserved = Some((*h, copy_id));
                    break;
                }
            }

            if preserved.is_none()
                && let Some(exp) = expected_local_hash.as_ref()
                && !store.with(|s| check_expected_local_hash(s, EntityKind::Document, &id, exp.as_deref()))?
            {
                report.skipped_raced_ids.push((EntityKind::Document, id));
                return Ok(());
            }

            let res = store.apply_remote_document_with(&id, &file_name, parent_id.as_deref(), &ext, &store_meta, &bytes, true, preserve_in_local_trash)?;
            report.tree_changed = true;
            if res.was_deleted {
                report.deleted_docs.push(id.clone());
            }

            let entries = store.list();
            for h in &ws_handles {
                if h.is_active(cx).is_none() { continue; }
                let entries_c = entries.clone();
                let _ = h.update(cx, |ws, window, cx| {
                    if ws.current.as_deref() != Some(id.as_str()) { return; }
                    ws.pending_remote_doc = None;
                    ws.pending_remote_board = None;
                    ws.conflict_banner = None;
                    ws.clear_unsaved_state();
                    ws.entries = entries_c;
                    ws.sort();
                    if let Some((pref_h, ref copy_id)) = preserved
                        && pref_h.window_id() == window.window_handle().window_id()
                    {
                        ws.open(copy_id, window, cx);
                        ws.show_toast(crate::settings::tr("当前文档已在其他端删除，已保留本地修改为副本", "Deleted on another device; kept your unsaved changes as a copy"), cx);
                    } else {
                        ws.current = None;
                        ws.editor = None;
                        ws.editor_sub = None;
                        ws.view = View::Home;
                        ws.show_toast(crate::settings::tr("当前文档已在其他端删除", "Document was deleted on another device"), cx);
                    }
                    cx.notify();
                });
            }
            return Ok(());
        }

        // Defer applying into any active window mid-IME composition so preedit never leaks or gets clobbered.
        if ext != "excalidraw" {
            let ime_windows: Vec<WindowHandle<Workspace>> = ws_handles
                .iter()
                .copied()
                .filter(|h| {
                    h.is_active(cx).is_some()
                        && h.update(cx, |ws, _, cx| {
                            ws.current.as_deref() == Some(id.as_str())
                                && ws.editor.as_ref().is_some_and(|e| e.read(cx).has_ime_composition())
                        })
                        .unwrap_or(false)
                })
                .collect();
            if !ime_windows.is_empty() {
                store.apply_remote_document_with(&id, &file_name, parent_id.as_deref(), &ext, &store_meta, &bytes, false, preserve_in_local_trash)?;
                report.tree_changed = true;
                for h in ime_windows {
                    let id_c = id.clone();
                    let file_c = file_name.clone();
                    let parent_c = parent_id.clone();
                    let ext_c = ext.clone();
                    let meta_c = meta.clone();
                    let bytes_c = bytes.clone();
                    let base_c = base_bytes.clone();
                    let dev_c = device_name.clone();
                    let _ = h.update(cx, |ws, _, cx| {
                        let kept_base = ws
                            .pending_remote_doc
                            .as_ref()
                            .filter(|p| p.0.id == id_c)
                            .and_then(|p| p.0.base_bytes.clone())
                            .or(base_c);
                        ws.pending_remote_doc = Some(PendingRemoteDoc(PendingRemoteDocInner {
                            id: id_c,
                            file_name: file_c,
                            parent_id: parent_c,
                            ext: ext_c,
                            meta: meta_c,
                            bytes: bytes_c,
                            base_bytes: kept_base,
                            device_name: dev_c,
                        }));
                        cx.notify();
                    });
                }
                return Ok(());
            }
        } else {
            // Defer applying into any active window whose standalone BoardView is mid-gesture.
            let busy_board_windows: Vec<WindowHandle<Workspace>> = ws_handles
                .iter()
                .copied()
                .filter(|h| {
                    h.is_active(cx).is_some()
                        && h.update(cx, |ws, _, cx| {
                            ws.current.as_deref() == Some(id.as_str())
                                && ws
                                    .editor
                                    .as_ref()
                                    .and_then(|e| e.read(cx).board_view_ref())
                                    .is_some_and(|(_, bv)| bv.read(cx).is_busy())
                        })
                        .unwrap_or(false)
                })
                .collect();
            if !busy_board_windows.is_empty()
                && let Ok(r_str) = std::str::from_utf8(&bytes)
                && let Ok(remote_board) = Board::from_file(r_str)
            {
                let disk_base = store
                    .file_path(&id)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|s| Board::from_file(&s).ok())
                    .map(Arc::new);
                let parsed_base = base_bytes
                    .as_deref()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|s| Board::from_file(s).ok())
                    .map(Arc::new)
                    .or(disk_base);
                store.apply_remote_document_with(&id, &file_name, parent_id.as_deref(), &ext, &store_meta, &bytes, false, preserve_in_local_trash)?;
                report.tree_changed = true;
                let remote_arc = Arc::new(remote_board);
                for h in busy_board_windows {
                    let id_c = id.clone();
                    let file_c = file_name.clone();
                    let parent_c = parent_id.clone();
                    let meta_c = meta.clone();
                    let base_c = parsed_base.clone();
                    let remote_c = remote_arc.clone();
                    let dev_c = device_name.clone();
                    let _ = h.update(cx, |ws, window, cx| {
                        let kept_base = ws
                            .pending_remote_board
                            .as_ref()
                            .filter(|p| p.0.doc_id == id_c)
                            .and_then(|p| p.0.base.clone())
                            .or(base_c);
                        ws.pending_remote_board = Some(PendingRemoteBoard(PendingRemoteBoardInner {
                            doc_id: id_c,
                            file_name: file_c,
                            parent_id: parent_c,
                            meta: meta_c,
                            base: kept_base,
                            remote: remote_c,
                            device_name: dev_c,
                        }));
                        schedule_pending_board_retry(window, cx);
                        cx.notify();
                    });
                }
                return Ok(());
            }
        }

        // Check whether any active window currently has unsaved live edits on `id`.
        let mut live_unsaved: Option<(WindowHandle<Workspace>, Vec<u8>)> = None;
        for h in &ws_handles {
            if h.is_active(cx).is_none() { continue; }
            let maybe = h.update(cx, |ws, _, cx| {
                if ws.current.as_deref() == Some(id.as_str()) && ws.unsaved {
                    extract_live_bytes(store, &id, &ext, ws, cx)
                } else {
                    None
                }
            }).unwrap_or(None);
            if let Some(live) = maybe {
                live_unsaved = Some((*h, live));
                break;
            }
        }

        if let Some((_, local_bytes)) = live_unsaved {
            let stem = file_name.strip_suffix(&format!(".{ext}")).unwrap_or(&file_name);
            let merged = try_live_three_way_merge(&ext, stem, base_bytes.as_deref(), &local_bytes, &bytes);
            match merged {
                Some(merged_bytes) => {
                    let res = store.apply_remote_document_with(&id, &file_name, parent_id.as_deref(), &ext, &store_meta, &merged_bytes, false, preserve_in_local_trash)?;
                    report.tree_changed = true;
                    report.updated_docs.push(id.clone());
                    if merged_bytes != bytes {
                        report.merged_back_docs.push((id.clone(), merged_bytes.clone(), meta));
                    }
                    hot_reload_open_workspaces(store, &id, &ext, &merged_bytes, &store_meta, res.file_title.as_deref(), true, None, &ws_handles, cx);
                }
                None => {
                    let res = store.apply_remote_document_with(&id, &file_name, parent_id.as_deref(), &ext, &store_meta, &local_bytes, false, preserve_in_local_trash)?;
                    let conflict_id = store.create_conflict_copy(&id, &ext, &bytes, &device_name)?;
                    let title = stem.to_string();
                    report.tree_changed = true;
                    report.live_conflicts.push(LiveConflict {
                        doc_id: id.clone(),
                        conflict_doc_id: conflict_id.clone(),
                        title: title.clone(),
                        device_name: device_name.clone(),
                    });
                    let banner = ConflictBanner { conflict_doc_id: conflict_id, title, device_name };
                    hot_reload_open_workspaces(store, &id, &ext, &local_bytes, &store_meta, res.file_title.as_deref(), true, Some(banner), &ws_handles, cx);
                }
            }
            return Ok(());
        }

        if let Some(exp) = expected_local_hash.as_ref()
            && !store.with(|s| check_expected_local_hash(s, EntityKind::Document, &id, exp.as_deref()))?
        {
            report.skipped_raced_ids.push((EntityKind::Document, id));
            return Ok(());
        }

        let res = store.apply_remote_document_with(&id, &file_name, parent_id.as_deref(), &ext, &store_meta, &bytes, false, preserve_in_local_trash)?;
        report.tree_changed = true;
        if res.changed {
            report.updated_docs.push(id.clone());
            if res.renamed { report.renamed_docs.push(id.clone()); }
            if res.meta_changed { report.meta_docs.push(id.clone()); }
            hot_reload_open_workspaces(store, &id, &ext, &bytes, &store_meta, res.file_title.as_deref(), false, None, &ws_handles, cx);
        } else if res.renamed || res.meta_changed {
            if res.renamed { report.renamed_docs.push(id.clone()); }
            if res.meta_changed { report.meta_docs.push(id.clone()); }
            hot_reload_metadata_or_title(&ws_handles, &id, &store_meta, res.renamed.then_some(res.file_title).flatten(), cx);
        }
        Ok(())
    }

    fn extract_live_bytes(store: &SharedStore, id: &str, ext: &str, ws: &Workspace, cx: &App) -> Option<Vec<u8>> {
        let ed = ws.editor.as_ref()?.read(cx);
        if ext == "excalidraw"
            && let Some((_, bv)) = ed.board_view_ref()
        {
            return Some(bv.read(cx).board().file_json().into_bytes());
        }
        let doc = ed.core.doc.clone();
        if ext == "md" || store.is_markdown(id) {
            let front = store.front_matter_of(id);
            let base = store.file_path(id);
            let full = fdl_core::markdown::write_file(&doc, &front, base.as_deref().and_then(|p| p.parent()));
            Some(full.into_bytes())
        } else {
            Some(fdl_core::docxml::serialize(&doc).into_bytes())
        }
    }

    fn try_live_three_way_merge(ext: &str, title: &str, base: Option<&[u8]>, local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
        if local == remote {
            return Some(remote.to_vec());
        }
        let base_str = std::str::from_utf8(base?).ok()?;
        let local_str = std::str::from_utf8(local).ok()?;
        let remote_str = std::str::from_utf8(remote).ok()?;
        match ext {
            "excalidraw" => {
                let bb = Board::from_file(base_str).ok()?;
                let lb = Board::from_file(local_str).ok()?;
                let rb = Board::from_file(remote_str).ok()?;
                let res = merge_boards(&bb, &lb, &rb);
                (!res.has_conflicts()).then(|| res.value.file_json().into_bytes())
            }
            "md" => {
                let res = merge_markdown(base_str, local_str, remote_str, title);
                (!res.has_conflicts()).then(|| res.value.into_bytes())
            }
            _ => {
                let bd = fdl_core::docxml::parse(base_str).ok()?;
                let ld = fdl_core::docxml::parse(local_str).ok()?;
                let rd = fdl_core::docxml::parse(remote_str).ok()?;
                let res = merge_documents(&bd, &ld, &rd);
                (!res.has_conflicts()).then(|| fdl_core::docxml::serialize(&res.value).into_bytes())
            }
        }
    }

    fn hot_reload_metadata_or_title(
        ws_handles: &[WindowHandle<Workspace>],
        id: &str,
        store_meta: &StoreMeta,
        file_title: Option<String>,
        cx: &mut App,
    ) {
        for h in ws_handles {
            if h.is_active(cx).is_none() { continue; }
            let id_owned = id.to_string();
            let meta_c = store_meta.clone();
            let title_c = file_title.clone();
            let _ = h.update(cx, |ws, window, cx| {
                if ws.current.as_deref() != Some(id_owned.as_str()) { return; }
                let in_preview = ws.preview.is_some();
                if let Some(t) = title_c.as_deref() {
                    if let Some(p) = &mut ws.preview {
                        p.core.receive_file_title(t);
                    } else if let Some(ed) = ws.editor.clone() {
                        ed.update(cx, |e, cx| e.receive_file_title(t, cx));
                    }
                }
                if let Some(p) = &mut ws.preview {
                    p.reading = meta_c.reading;
                }
                if let Some(ed) = ws.editor.clone() {
                    ed.update(cx, |e, cx| e.apply_synced_meta(&meta_c, in_preview, cx));
                }
                if !in_preview {
                    ws.sync_document(window, cx);
                }
                cx.notify();
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn hot_reload_open_workspaces(
        store: &SharedStore,
        id: &str,
        ext: &str,
        bytes: &[u8],
        store_meta: &StoreMeta,
        file_title: Option<&str>,
        clear_unsaved: bool,
        banner: Option<ConflictBanner>,
        ws_handles: &[WindowHandle<Workspace>],
        cx: &mut App,
    ) {
        for h in ws_handles {
            if h.is_active(cx).is_none() { continue; }
            let id_owned = id.to_string();
            let meta_c = store_meta.clone();
            let title_c = file_title.map(str::to_string);
            let banner_clone = banner.clone();
            if ext == "excalidraw" {
                let Ok(s) = std::str::from_utf8(bytes) else { continue };
                let Ok(board) = Board::from_file(s) else { continue };
                let board = Arc::new(board);
                let _ = h.update(cx, |ws, _, cx| {
                    if ws.current.as_deref() != Some(id_owned.as_str()) { return; }
                    if clear_unsaved {
                        ws.clear_unsaved_state();
                    }
                    if let Some(b) = banner_clone { ws.conflict_banner = Some(b); }
                    let in_preview = ws.preview.is_some();
                    if let Some(p) = &mut ws.preview {
                        p.reading = meta_c.reading;
                        if let Some(t) = title_c.as_deref() {
                            p.core.receive_file_title(t);
                        }
                    }
                    if let Some(ed) = ws.editor.clone() {
                        if let Some((_, bv)) = ed.read(cx).board_view_ref() {
                            ws.pending_remote_board = None;
                            let board_c = board.clone();
                            bv.update(cx, |b, cx| b.apply_remote_board(board_c, cx));
                        }
                        ed.update(cx, |e, cx| {
                            e.sync_open_board_to_core(board);
                            if let Some(t) = title_c.as_deref() {
                                e.receive_file_title(t, cx);
                            }
                            e.apply_synced_meta(&meta_c, in_preview, cx);
                        });
                    }
                    cx.notify();
                });
            } else {
                let is_md = ext == "md" || store.is_markdown(id);
                let doc = if let Ok(d) = store.load(id) {
                    d
                } else {
                    continue;
                };
                let _ = h.update(cx, |ws, window, cx| {
                    if ws.current.as_deref() != Some(id_owned.as_str()) { return; }
                    if clear_unsaved {
                        ws.clear_unsaved_state();
                    }
                    if let Some(b) = banner_clone { ws.conflict_banner = Some(b); }
                    if let Some(p) = &mut ws.preview {
                        p.core.receive_remote(doc, is_md);
                        p.reading = meta_c.reading;
                        if let Some(t) = title_c.as_deref() {
                            p.core.receive_file_title(t);
                        }
                        if let Some(ed) = ws.editor.clone() {
                            ed.update(cx, |e, cx| e.apply_synced_meta(&meta_c, true, cx));
                        }
                    } else if let Some(ed) = ws.editor.clone() {
                        ws.pending_remote_doc = None;
                        ed.update(cx, |e, cx| {
                            e.receive_remote(doc, is_md, cx);
                            if let Some(t) = title_c.as_deref() {
                                e.receive_file_title(t, cx);
                            }
                            e.apply_synced_meta(&meta_c, false, cx);
                        });
                        ws.sync_document(window, cx);
                    }
                    cx.notify();
                });
            }
        }
    }

    pub fn apply_pending_remote_doc(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(PendingRemoteDoc(p)) = ws.pending_remote_doc.take() else { return };
        if ws.current.as_deref() != Some(p.id.as_str()) { return; }
        let Some(local_bytes) = extract_live_bytes(&ws.store, &p.id, &p.ext, ws, cx) else {
            ws.pending_remote_doc = Some(PendingRemoteDoc(p));
            return;
        };
        let store_meta = store_meta_from_sync(&p.meta);
        let stem = p.file_name.strip_suffix(&format!(".{}", p.ext)).unwrap_or(&p.file_name);
        match try_live_three_way_merge(&p.ext, stem, p.base_bytes.as_deref(), &local_bytes, &p.bytes) {
            Some(merged_bytes) => {
                if ws.store.apply_remote_document_with(&p.id, &p.file_name, p.parent_id.as_deref(), &p.ext, &store_meta, &merged_bytes, false, true).is_err() {
                    ws.pending_remote_doc = Some(PendingRemoteDoc(p));
                    return;
                }
                let is_md = p.ext == "md" || ws.store.is_markdown(&p.id);
                if let Ok(doc) = ws.store.load(&p.id)
                    && let Some(ed) = ws.editor.clone()
                {
                    ws.clear_unsaved_state();
                    ed.update(cx, |e, cx| {
                        e.receive_remote(doc, is_md, cx);
                        e.apply_synced_meta(&store_meta, ws.preview.is_some(), cx);
                    });
                    ws.sync_document(window, cx);
                }
                ws.entries = ws.store.list();
                ws.sort();
                if merged_bytes != p.bytes {
                    crate::sync::notify_upsert_doc(&p.id);
                }
            }
            None => {
                if ws.store.apply_remote_document_with(&p.id, &p.file_name, p.parent_id.as_deref(), &p.ext, &store_meta, &local_bytes, false, true).is_err() {
                    ws.pending_remote_doc = Some(PendingRemoteDoc(p));
                    return;
                }
                let Ok(conflict_id) = ws.store.create_conflict_copy(&p.id, &p.ext, &p.bytes, &p.device_name) else {
                    ws.pending_remote_doc = Some(PendingRemoteDoc(p));
                    return;
                };
                let title = stem.to_string();
                ws.clear_unsaved_state();
                ws.conflict_banner = Some(ConflictBanner {
                    conflict_doc_id: conflict_id.clone(),
                    title: title.clone(),
                    device_name: p.device_name.clone(),
                });
                ws.show_toast(&format!("{}: {} ({})", crate::settings::tr("发现同步冲突，已保留本地修改并创建副本", "Sync conflict resolved with a conflict copy"), title, p.device_name), cx);
                ws.entries = ws.store.list();
                ws.sort();
                crate::sync::notify_hints(vec![hint_upsert_doc(&p.id), hint_upsert_doc(&conflict_id)]);
            }
        }
        cx.notify();
    }

    pub fn apply_pending_remote_board(ws: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
        let Some(PendingRemoteBoard(p)) = ws.pending_remote_board.take() else { return false };
        if ws.current.as_deref() != Some(p.doc_id.as_str()) {
            return false;
        }
        let Some(ed) = ws.editor.clone() else { return false };
        let Some((_, bv)) = ed.read(cx).board_view_ref() else { return false };
        if bv.read(cx).is_busy() {
            ws.pending_remote_board = Some(PendingRemoteBoard(p));
            return true;
        }
        let local_board = bv.read(cx).board();
        let store_meta = store_meta_from_sync(&p.meta);

        if *local_board == *p.remote {
            let remote_bytes = p.remote.file_json().into_bytes();
            if ws.store.apply_remote_document_with(&p.doc_id, &p.file_name, p.parent_id.as_deref(), "excalidraw", &store_meta, &remote_bytes, false, true).is_err() {
                ws.pending_remote_board = Some(PendingRemoteBoard(p));
                return true;
            }
            let r_c = p.remote.clone();
            bv.update(cx, |b, cx| b.apply_remote_board(r_c, cx));
            ed.update(cx, |e, cx| {
                e.sync_open_board_to_core(p.remote);
                e.apply_synced_meta(&store_meta, ws.preview.is_some(), cx);
            });
            ws.clear_unsaved_state();
            ws.entries = ws.store.list();
            ws.sort();
            cx.notify();
            return false;
        }

        if let Some(ref base) = p.base {
            let res = merge_boards(base, &local_board, &p.remote);
            if !res.has_conflicts() {
                let merged = Arc::new(res.value);
                let merged_bytes = merged.file_json().into_bytes();
                if ws.store.apply_remote_document_with(&p.doc_id, &p.file_name, p.parent_id.as_deref(), "excalidraw", &store_meta, &merged_bytes, false, true).is_err() {
                    ws.pending_remote_board = Some(PendingRemoteBoard(p));
                    return true;
                }
                let m_bv = merged.clone();
                let m_ed = merged.clone();
                bv.update(cx, |b, cx| b.apply_remote_board(m_bv, cx));
                ed.update(cx, |e, cx| {
                    e.sync_open_board_to_core(m_ed);
                    e.apply_synced_meta(&store_meta, ws.preview.is_some(), cx);
                });
                ws.clear_unsaved_state();
                ws.entries = ws.store.list();
                ws.sort();
                if *merged != *p.remote {
                    crate::sync::notify_upsert_doc(&p.doc_id);
                }
                cx.notify();
                return false;
            }
        }

        let local_bytes = local_board.file_json().into_bytes();
        let remote_bytes = p.remote.file_json().into_bytes();
        if ws.store.apply_remote_document_with(&p.doc_id, &p.file_name, p.parent_id.as_deref(), "excalidraw", &store_meta, &local_bytes, false, true).is_err() {
            ws.pending_remote_board = Some(PendingRemoteBoard(p));
            return true;
        }
        let Ok(conflict_id) = ws.store.create_conflict_copy(&p.doc_id, "excalidraw", &remote_bytes, &p.device_name) else {
            ws.pending_remote_board = Some(PendingRemoteBoard(p));
            return true;
        };
        let l_c = local_board.clone();
        bv.update(cx, |b, cx| b.apply_remote_board(l_c, cx));
        ed.update(cx, |e, cx| {
            e.sync_open_board_to_core(local_board);
            e.apply_synced_meta(&store_meta, ws.preview.is_some(), cx);
        });
        ws.clear_unsaved_state();
        let stem = p.file_name.strip_suffix(".excalidraw").unwrap_or(&p.file_name).to_string();
        ws.conflict_banner = Some(ConflictBanner {
            conflict_doc_id: conflict_id.clone(),
            title: stem.clone(),
            device_name: p.device_name.clone(),
        });
        ws.show_toast(&format!("{}: {} ({})", crate::settings::tr("发现同步冲突，已保留本地修改并创建副本", "Sync conflict resolved with a conflict copy"), stem, p.device_name), cx);
        ws.entries = ws.store.list();
        ws.sort();
        crate::sync::notify_hints(vec![hint_upsert_doc(&p.doc_id), hint_upsert_doc(&conflict_id)]);
        cx.notify();
        false
    }

    fn schedule_pending_board_retry(window: &mut Window, cx: &mut Context<Workspace>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let keep_going = this.update(cx, apply_pending_remote_board).unwrap_or(false);
                if !keep_going { break; }
            }
        }).detach();
    }

    fn reload_synced_configs(keys: &[String], cx: &mut App) {
        let mut need_prefs = false;
        let mut need_shortcuts = false;
        let mut need_themes = false;
        for k in keys {
            if k == "preferences" { need_prefs = true; }
            else if k == "shortcuts" { need_shortcuts = true; }
            else if k.starts_with("theme:") { need_themes = true; }
        }
        if need_prefs {
            crate::settings::load();
            let ap = crate::settings::prefs().appearance;
            crate::settings::apply_appearance(&ap);
            crate::settings::sync_dark(cx);
        }
        if need_shortcuts {
            crate::shortcuts::reload(cx);
        }
        for w in cx.windows() {
            if let Some(h) = w.downcast::<Workspace>()
                && h.is_active(cx).is_some()
            {
                let _ = h.update(cx, |ws, _, cx| {
                    if need_themes { ws.reload_themes(cx); }
                    if need_prefs || need_shortcuts { crate::settings::refresh_all(ws, cx); }
                    cx.notify();
                });
            }
        }
    }
}
