use crate::store::*;
use crate::model::Document;
use anyhow::Result;
use std::{cell::RefCell, rc::Rc, sync::{atomic::{AtomicBool, Ordering}, Arc}};

#[derive(Clone)]
pub struct SharedStore(Rc<RefCell<Store>>, Arc<AtomicBool>);
pub struct FileTaskGuard(Arc<AtomicBool>);

impl Drop for FileTaskGuard {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

impl gpui::Global for SharedStore {}
impl SharedStore {
    pub fn open() -> Result<Self> { Ok(Self(Rc::new(RefCell::new(Store::open()?)), Arc::new(AtomicBool::new(false)))) }
    pub fn file_task_busy(&self) -> bool { self.1.load(Ordering::Acquire) }
    pub fn begin_file_task(&self) -> Option<FileTaskGuard> {
        self.1.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok().then(|| FileTaskGuard(self.1.clone()))
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R { f(&mut self.0.borrow_mut()) }
    pub fn take_warnings(&self) -> Vec<String> { std::mem::take(&mut self.0.borrow_mut().warnings) }
    pub fn list(&mut self) -> Vec<Entry> { self.0.borrow_mut().list() }
    pub fn load(&self, id: &str) -> Result<Document> { self.0.borrow_mut().load(id) }
    pub fn load_for_edit(&self, id: &str, shared: bool) -> Result<Document> { self.0.borrow().load_for_edit(id, shared) }
    pub fn forget_loaded(&self, id: &str) { self.0.borrow().forget_loaded(id); }
    pub fn recover_later(&self, doc: &Document) -> Result<()> { self.0.borrow().recover_later(doc) }
    fn notify_saved(&self, id: &str, prev_ver: Option<String>) {
        let mut hints = vec![crate::sync::hint_upsert_doc(id)];
        if let Some(v) = self.0.borrow().versions(id).into_iter().next()
            && prev_ver.as_deref() != Some(v.name.as_str())
        {
            hints.push(crate::sync::hint_upsert_history(id, &v.name));
        }
        crate::sync::notify_hints(hints);
    }
    pub fn save(&mut self, id: &str, doc: &Document) -> Result<Meta> {
        let prev_ver = self.0.borrow().versions(id).into_iter().next().map(|v| v.name);
        let r = self.0.borrow_mut().save(id, doc);
        if r.is_ok() { self.notify_saved(id, prev_ver); }
        r
    }
    pub fn save_editor(&mut self, id: &str, doc: &Document) -> Result<Meta> {
        let prev_ver = self.0.borrow().versions(id).into_iter().next().map(|v| v.name);
        let r = self.0.borrow_mut().save_editor(id, doc);
        if r.is_ok() { self.notify_saved(id, prev_ver); }
        r
    }
    pub fn create(&mut self) -> Result<(String, Document)> {
        let r = self.0.borrow_mut().create();
        if let Ok((ref id, _)) = r { crate::sync::notify_upsert_doc(id); }
        r
    }
    pub fn create_from(&mut self, doc: &Document) -> Result<String> {
        let r = self.0.borrow_mut().create_from(doc);
        if let Ok(ref id) = r { crate::sync::notify_upsert_doc(id); }
        r
    }
    pub fn size_of(&self, id: &str, doc: &Document) -> u64 { self.0.borrow_mut().size_of(id, doc) }
    pub fn list_trash(&mut self) -> Vec<TrashEntry> { self.0.borrow_mut().list_trash() }
    pub fn restore(&mut self, id: &str) -> Result<bool> {
        let r = self.0.borrow_mut().restore(id);
        if r.as_ref().is_ok_and(|yes| *yes) {
            crate::sync::notify_hints(vec![crate::sync::hint_delete_trash(id), crate::sync::hint_scan()]);
        }
        r
    }
    pub fn set_width(&mut self, id: &str, width: PageWidth) -> Result<()> {
        let r = self.0.borrow_mut().set_width(id, width);
        if r.is_ok() { crate::sync::notify_upsert_doc(id); }
        r
    }
    pub fn update_meta(&mut self, id: &str, f: impl FnOnce(&mut Meta)) -> Result<()> {
        let r = self.0.borrow_mut().update_meta(id, f);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn set_catalogue(&mut self, id: &str, pinned: bool) -> Result<()> {
        let r = self.0.borrow_mut().set_catalogue(id, pinned);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn settings(&self) -> Settings { self.0.borrow_mut().settings() }
    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        let r = self.0.borrow_mut().save_settings(s);
        if r.is_ok() { crate::sync::notify_config("library_settings"); }
        r
    }
    pub fn scroll_top(&self, id: &str) -> Option<(usize, f32)> { self.0.borrow().scroll_top(id) }
    pub fn remember_scroll(&self, id: &str, top: Option<(usize, f32)>) -> Result<()> { self.0.borrow().remember_scroll(id, top) }
    pub fn versions(&self, id: &str) -> Vec<fdl_core::versions::Version> { self.0.borrow().versions(id) }
    pub fn version_diff(&self, id: &str, name: &str, baseline: Option<&str>) -> Result<Vec<fdl_core::diff::Line>> { self.0.borrow().version_diff(id, name, baseline) }
    pub fn version_document(&self, id: &str, name: &str) -> Result<Document> { self.0.borrow().version_document(id, name) }
    pub fn set_version_label(&self, id: &str, name: &str, label: Option<&str>) -> Result<()> {
        let r = self.0.borrow().set_version_label(id, name, label);
        if r.is_ok() { crate::sync::notify_upsert_history(id, name); }
        r
    }
    pub fn delete_version(&self, id: &str, name: &str) -> Result<()> {
        let r = self.0.borrow().delete_version(id, name);
        if r.is_ok() { crate::sync::notify_delete_history(id, name); }
        r
    }
    pub fn restore_version(&self, id: &str, name: &str) -> Result<Document> { self.0.borrow().restore_version(id, name) }
    pub fn save_version_now(&self, id: &str) -> Result<bool> {
        let r = self.0.borrow().save_version_now(id);
        if r.as_ref().is_ok_and(|saved| *saved)
            && let Some(v) = self.0.borrow().versions(id).into_iter().next()
        {
            crate::sync::notify_upsert_history(id, &v.name);
        }
        r
    }
    pub fn rename(&mut self, id: &str, title: &str) -> Result<Meta> {
        let r = self.0.borrow_mut().rename(id, title);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn touch(&mut self, id: &str) -> Option<Meta> { self.0.borrow_mut().touch(id) }
    pub fn create_folder(&mut self, title: &str, parent: Option<&str>) -> Result<String> {
        let r = self.0.borrow_mut().create_folder(title, parent);
        if let Ok(ref id) = r { crate::sync::notify_upsert_folder(id); }
        r
    }
    pub fn accept_file_title(&self, id: &str, title: &str) -> Result<()> {
        let r = self.0.borrow().accept_file_title(id, title);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn initialize_manual_order(&mut self) -> Result<()> {
        let r = self.0.borrow_mut().initialize_manual_order();
        if r.is_ok() { crate::sync::notify_scan(); }
        r
    }
    pub fn track_file_changes(&mut self) -> FileChanges { self.0.borrow_mut().track_file_changes() }
    pub fn tree_writer(&self) -> Store { self.0.borrow().tree_writer() }
    pub fn set_parent(&mut self, id: &str, parent: Option<&str>) -> Result<Meta> {
        let r = self.0.borrow_mut().set_parent(id, parent);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn set_starred(&mut self, id: &str, on: bool) -> Result<u64> {
        let r = self.0.borrow_mut().set_starred(id, on);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn set_pinned(&mut self, id: &str, pinned: bool) -> Result<()> {
        let r = self.0.borrow_mut().set_pinned(id, pinned);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn move_in_tree(&mut self, id: &str, parent: Option<&str>, before: Option<&str>) -> Result<()> {
        let r = self.0.borrow_mut().move_in_tree(id, parent, before);
        if r.is_ok() { crate::sync::notify_entity_upsert(id, self.is_folder(id)); }
        r
    }
    pub fn is_markdown(&self, id: &str) -> bool { self.0.borrow().is_markdown(id) }
    pub fn create_markdown(&mut self, doc: &Document, front: &str) -> Result<String> {
        let r = self.0.borrow_mut().create_markdown(doc, front);
        if let Ok(ref id) = r { crate::sync::notify_upsert_doc(id); }
        r
    }
    pub fn create_markdown_in(&mut self, doc: &Document, front: &str, parent: Option<&str>) -> Result<String> {
        let r = self.0.borrow_mut().create_markdown_in(doc, front, parent);
        if let Ok(ref id) = r { crate::sync::notify_upsert_doc(id); }
        r
    }
    pub fn create_from_in(&mut self, doc: &Document, parent: Option<&str>) -> Result<String> {
        let r = self.0.borrow_mut().create_from_in(doc, parent);
        if let Ok(ref id) = r && !self.is_mounted(id) { crate::sync::notify_upsert_doc(id); }
        r
    }
    pub fn delete(&mut self, id: &str, root: Option<&str>) -> Result<()> { self.0.borrow_mut().delete(id, root) }
    pub fn mount(&mut self, dir: &std::path::Path) -> Result<String> { self.0.borrow_mut().mount(dir) }
    pub fn mount_dirs(&self) -> Vec<std::path::PathBuf> { self.0.borrow().mount_dirs() }
    pub fn is_mounted(&self, id: &str) -> bool { self.0.borrow().is_mounted(id) }
    pub fn forget_mounted(&mut self, id: &str) { self.0.borrow_mut().forget_mounted(id) }
    pub fn create_board_in(&mut self, title: &str, board: &crate::whiteboard::Board, parent: Option<&str>) -> Result<String> {
        let r = self.0.borrow_mut().create_board_in(title, board, parent);
        if let Ok(ref id) = r { crate::sync::notify_upsert_doc(id); }
        r
    }
    #[cfg(not(target_family = "wasm"))]
    pub fn create_table_in(&mut self, title: &str, parent: Option<&str>) -> Result<String> { self.0.borrow_mut().create_table_in(title, parent) }
    #[cfg(not(target_family = "wasm"))]
    pub fn copy_table(&mut self, src: &str, title: &str) -> Result<String> { self.0.borrow_mut().copy_table(src, title) }
    #[cfg(target_family = "wasm")]
    pub fn create_table_in(&mut self, _title: &str, _parent: Option<&str>) -> Result<String> { anyhow::bail!("unsupported") }
    #[cfg(target_family = "wasm")]
    pub fn copy_table(&mut self, _src: &str, _title: &str) -> Result<String> { anyhow::bail!("unsupported") }
    pub fn front_matter_of(&self, id: &str) -> String { self.0.borrow().front_matter_of(id) }
    pub fn open_local(&mut self, path: &std::path::Path) -> Result<String> { self.0.borrow_mut().open_local(path) }
    pub fn local_path(&self, id: &str) -> Option<std::path::PathBuf> { self.0.borrow().local_path(id) }
    pub fn folder_dir(&self, id: Option<&str>) -> Option<std::path::PathBuf> { self.0.borrow().folder_dir(id) }
    pub fn file_path(&self, id: &str) -> Option<std::path::PathBuf> { self.0.borrow().file_path(id) }

    // Sync bridge (called by SyncManager without emitting local change hints).
    pub fn root(&self) -> std::path::PathBuf { self.0.borrow().root().to_path_buf() }
    pub fn sync_local_dir(&self) -> std::path::PathBuf { self.0.borrow().sync_local_dir() }
    pub fn is_folder(&self, id: &str) -> bool { self.0.borrow().is_folder(id) }
    #[allow(clippy::too_many_arguments)]
    pub fn apply_remote_document_with(&mut self, id: &str, file_name: &str, parent_id: Option<&str>, ext: &str, meta: &Meta, raw_bytes: &[u8], deleted: bool, preserve_in_local_trash: bool) -> Result<RemoteDocApplied> { self.0.borrow_mut().apply_remote_document_with(id, file_name, parent_id, ext, meta, raw_bytes, deleted, preserve_in_local_trash) }
    pub fn create_conflict_copy(&mut self, orig_id: &str, ext: &str, raw_bytes: &[u8], device_label: &str) -> Result<String> { self.0.borrow_mut().create_conflict_copy(orig_id, ext, raw_bytes, device_label) }
    pub fn adopt_entity_id(&mut self, old_id: &str, new_id: &str) -> Result<()> { self.0.borrow_mut().adopt_entity_id(old_id, new_id) }
}
