//! Store sync bridge: enumerates library entities for sync and atomically applies
//! remote folder/doc/trash/history changes under the Store write lock while keeping
//! FORMAT 4 on-disk layout and link invariants intact.

use super::*;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct InventoryFolder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub rel: String,
    pub meta: Meta,
}

#[derive(Clone, Debug)]
pub struct InventoryDoc {
    pub id: String,
    pub file_name: String,
    pub parent_id: Option<String>,
    pub rel: String,
    pub ext: String,
    pub meta: Meta,
}

#[derive(Clone, Debug)]
pub struct InventoryTrash {
    pub id: String,
    pub trashed: Trashed,
    pub ext: String,
}

#[derive(Clone, Debug)]
pub struct InventoryHistory {
    pub doc_id: String,
    pub name: String,
    pub trashed: bool,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SyncInventory {
    pub folders: Vec<InventoryFolder>,
    pub docs: Vec<InventoryDoc>,
    pub trash: Vec<InventoryTrash>,
    pub history: Vec<InventoryHistory>,
    pub assets: Vec<String>,
    pub themes: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum RawEntitySnapshot {
    Folder {
        id: String,
        name: String,
        parent_id: Option<String>,
        rel: String,
        meta: Meta,
    },
    Document {
        id: String,
        file_name: String,
        parent_id: Option<String>,
        rel: String,
        ext: String,
        meta: Meta,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
pub struct RemoteDocApplied {
    pub changed: bool,
    pub was_deleted: bool,
    pub renamed: bool,
    pub meta_changed: bool,
    pub rel: String,
    pub file_title: Option<String>,
}

impl Store {
    /// Directory outside the library where sync state DB & credentials live.
    pub fn sync_local_dir(&self) -> PathBuf {
        let tmp = crate::vfs::temp_dir();
        let base = if cfg!(test) || self.root.starts_with(&tmp) || tmp.fs_canonicalize().is_ok_and(|t| self.root.starts_with(t)) {
            tmp.join("quiet-md-local")
        } else {
            dirs::data_local_dir().unwrap_or_else(crate::vfs::temp_dir).join("quiet-md")
        };
        base.join("sync").join(library_key(&self.root))
    }

    pub fn is_flat(&self) -> bool {
        self.flat
    }

    pub fn is_folder(&self, id: &str) -> bool {
        self.index.get(id).is_some_and(|m| m.folder.is_some())
    }

    pub fn sync_inventory(&mut self) -> Result<SyncInventory> {
        let _ = self.list();
        let _ = self.list_trash();

        let mut inv = SyncInventory::default();
        for (id, m) in &self.index {
            if m.local() || m.rel.is_empty() {
                continue;
            }
            if let Some(name) = &m.folder {
                let name = if name.is_empty() { base_name(&m.rel).to_string() } else { name.clone() };
                inv.folders.push(InventoryFolder {
                    id: id.clone(),
                    name,
                    parent_id: m.parent.clone(),
                    rel: m.rel.clone(),
                    meta: m.clone(),
                });
            } else {
                inv.docs.push(InventoryDoc {
                    id: id.clone(),
                    file_name: base_name(&m.rel).to_string(),
                    parent_id: m.parent.clone(),
                    rel: m.rel.clone(),
                    ext: ext_of(m).to_string(),
                    meta: m.clone(),
                });
            }
        }
        // Parents before children.
        inv.folders.sort_by(|a, b| a.rel.matches('/').count().cmp(&b.rel.matches('/').count()).then_with(|| a.rel.cmp(&b.rel)));
        inv.docs.sort_by(|a, b| a.rel.cmp(&b.rel));

        for (id, t) in &self.trash {
            let ext = if t.meta.folder.is_some() { String::new() } else { ext_of(&t.meta).to_string() };
            inv.trash.push(InventoryTrash { id: id.clone(), trashed: t.clone(), ext });
        }
        inv.trash.sort_by(|a, b| a.id.cmp(&b.id));

        let hist_base = self.history_base();
        for (dir_name, trashed) in [("history", false), ("history-trash", true)] {
            if let Ok(doc_dirs) = crate::vfs::read_dir(hist_base.join(dir_name)) {
                for entry in doc_dirs.flatten() {
                    if !entry.file_type().is_ok_and(|ft| ft.fs_is_dir()) {
                        continue;
                    }
                    let doc_id = entry.file_name().to_string_lossy().into_owned();
                    if !crate::versions::safe_id(&doc_id) {
                        continue;
                    }
                    for v in crate::versions::list(&hist_base, &doc_id, trashed) {
                        inv.history.push(InventoryHistory { doc_id: doc_id.clone(), name: v.name, trashed, label: v.label });
                    }
                }
            }
        }

        let assets_root = Self::assets_in(&self.root);
        let assets_prefix = if self.flat { "assets" } else { ASSETS };
        collect_assets_rel(&assets_root, &assets_root, assets_prefix, &mut inv.assets);
        inv.assets.sort();

        if let Ok(rd) = crate::vfs::read_dir(self.internal().join("themes")) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("css")
                    && let Some(name) = p.file_name().and_then(|s| s.to_str())
                {
                    inv.themes.push(name.to_string());
                }
            }
        }
        inv.themes.sort();
        Ok(inv)
    }

    pub fn read_raw_entity(&self, id: &str) -> Result<Option<RawEntitySnapshot>> {
        let Some(m) = self.index.get(id) else { return Ok(None) };
        if m.local() || m.rel.is_empty() {
            return Ok(None);
        }
        if let Some(folder) = &m.folder {
            let name = if folder.is_empty() { base_name(&m.rel).to_string() } else { folder.clone() };
            return Ok(Some(RawEntitySnapshot::Folder {
                id: id.to_string(),
                name,
                parent_id: m.parent.clone(),
                rel: m.rel.clone(),
                meta: m.clone(),
            }));
        }
        let path = self.path(id);
        let bytes = match crate::vfs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(RawEntitySnapshot::Document {
            id: id.to_string(),
            file_name: base_name(&m.rel).to_string(),
            parent_id: m.parent.clone(),
            rel: m.rel.clone(),
            ext: ext_of(m).to_string(),
            meta: m.clone(),
            bytes,
        }))
    }

    pub fn read_trash_raw(&self, id: &str) -> Result<Option<(Trashed, String, Option<Vec<u8>>)>> {
        let Some(t) = self.trash.get(id).cloned() else { return Ok(None) };
        if t.meta.folder.is_some() {
            return Ok(Some((t, String::new(), None)));
        }
        let ext = ext_of(&t.meta).to_string();
        let bytes = crate::vfs::read(self.trash_path(id)).ok();
        Ok(Some((t, ext, bytes)))
    }

    pub fn read_history_raw(&self, doc_id: &str, name: &str, trashed: bool) -> Result<Option<(Vec<u8>, Option<String>)>> {
        if !crate::versions::safe_id(doc_id) || !crate::versions::safe_name(name) {
            return Ok(None);
        }
        let text = match crate::versions::read(&self.history_base(), doc_id, name, trashed) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        let label = crate::versions::list(&self.history_base(), doc_id, trashed)
            .into_iter()
            .find(|v| v.name == name)
            .and_then(|v| v.label);
        Ok(Some((text.into_bytes(), label)))
    }

    /// Extract `.assets/...` paths referenced by a `.xml`, `.md`, or `.excalidraw` file.
    pub fn doc_asset_refs_from_bytes(ext: &str, bytes: &[u8]) -> Vec<String> {
        let Ok(text) = std::str::from_utf8(bytes) else { return Vec::new() };
        let mut raw_refs = Vec::new();
        match ext {
            "excalidraw" => {
                if let Ok(bd) = crate::whiteboard::Board::from_file(text) {
                    for f in bd.files.values() {
                        if let Some(p) = &f.path {
                            raw_refs.push(p.clone());
                        }
                    }
                }
            }
            "md" => {
                let doc = crate::markdown::parse_file(text, "", None);
                collect_doc_asset_refs(&doc, &mut raw_refs);
            }
            _ => {
                if let Ok(doc) = docxml::parse(text) {
                    collect_doc_asset_refs(&doc, &mut raw_refs);
                }
            }
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in raw_refs {
            if let Some(norm) = normalize_asset_rel(&r)
                && seen.insert(norm.clone())
            {
                out.push(norm);
            }
        }
        out
    }

    pub fn apply_remote_folder(&mut self, id: &str, name: &str, parent_id: Option<&str>, meta: &Meta, deleted: bool) -> Result<bool> {
        anyhow::ensure!(is_safe_entity_id(id), "无效的文件夹实体标识");
        let _lock = self.begin_write()?;
        anyhow::ensure!(!self.flat, "旧格式文档库不支持真实文件夹同步");

        if deleted {
            let Some(m) = self.index.get(id).cloned() else { return Ok(false) };
            if m.folder.is_none() {
                return Ok(false);
            }
            let dir = self.abs(&m.rel);
            let removed_dir = if dir.fs_is_dir() {
                crate::vfs::remove_dir(&dir).is_ok()
            } else {
                true
            };
            if removed_dir {
                self.index.remove(id);
                self.save_index()?;
                return Ok(true);
            }
            return Ok(false);
        }

        let eff_parent = self.safe_parent_for(id, parent_id.filter(|p| is_safe_entity_id(p)));
        let dir_rel = self.folder_rel(eff_parent.as_deref());
        let clean_name = file_name_for(name);

        if let Some(existing) = self.index.get(id).cloned() {
            if existing.folder.is_none() {
                return Ok(false);
            }
            let cur_parent = existing.parent.as_deref();
            let cur_name = base_name(&existing.rel);
            if cur_parent != eff_parent.as_deref() || cur_name != clean_name {
                let target_rel = join_rel(&dir_rel, &self.free_name(&dir_rel, &clean_name, None, Some(id)));
                if target_rel != existing.rel {
                    self.relocate(&[(id.to_string(), target_rel)])?;
                }
            }
        } else {
            let target_rel = join_rel(&dir_rel, &self.free_name(&dir_rel, &clean_name, None, None));
            crate::vfs::create_dir_all(self.abs(&target_rel))?;
            self.index.insert(
                id.to_string(),
                Meta { folder: Some(String::new()), rel: target_rel, created: meta.created, modified: meta.modified, ..Default::default() },
            );
            derive_tree(&mut self.index);
        }

        if let Some(m) = self.index.get_mut(id) {
            copy_synced_meta_fields(m, meta);
        }
        self.save_index()?;
        Ok(true)
    }

    pub fn apply_remote_document(
        &mut self,
        id: &str,
        file_name: &str,
        parent_id: Option<&str>,
        ext: &str,
        meta: &Meta,
        raw_bytes: &[u8],
        deleted: bool,
    ) -> Result<RemoteDocApplied> {
        self.apply_remote_document_with(id, file_name, parent_id, ext, meta, raw_bytes, deleted, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_remote_document_with(
        &mut self,
        id: &str,
        file_name: &str,
        parent_id: Option<&str>,
        ext: &str,
        meta: &Meta,
        raw_bytes: &[u8],
        deleted: bool,
        preserve_in_local_trash: bool,
    ) -> Result<RemoteDocApplied> {
        anyhow::ensure!(is_safe_entity_id(id), "无效的文档实体标识");
        let _lock = self.begin_write()?;
        anyhow::ensure!(!self.flat, "旧格式文档库不支持文件夹同步");

        if deleted {
            let Some(m) = self.index.get(id).cloned() else {
                return Ok(RemoteDocApplied {
                    changed: false,
                    was_deleted: false,
                    renamed: false,
                    meta_changed: false,
                    rel: String::new(),
                    file_title: None,
                });
            };
            if m.local() || m.folder.is_some() {
                return Ok(RemoteDocApplied {
                    changed: false,
                    was_deleted: false,
                    renamed: false,
                    meta_changed: false,
                    rel: String::new(),
                    file_title: None,
                });
            }
            if preserve_in_local_trash && !self.trash.contains_key(id) {
                self.trash_one(id, None)?;
                return Ok(RemoteDocApplied {
                    changed: true,
                    was_deleted: true,
                    renamed: false,
                    meta_changed: false,
                    rel: m.rel,
                    file_title: None,
                });
            }
            let p = self.path(id);
            match crate::vfs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            self.index.remove(id);
            self.loaded.borrow_mut().remove(id);
            self.parsed.remove(id);
            self.save_index()?;
            return Ok(RemoteDocApplied {
                changed: true,
                was_deleted: true,
                renamed: false,
                meta_changed: false,
                rel: m.rel,
                file_title: None,
            });
        }

        let prev_rel = self.index.get(id).map(|m| m.rel.clone());
        let prev_meta = self.index.get(id).cloned();

        let eff_parent = parent_id.filter(|p| is_safe_entity_id(p) && self.index.get(*p).is_some_and(|m| m.folder.is_some()));
        let dir_rel = self.folder_rel(eff_parent);
        let stem = sanitized_stem(file_name, ext);
        let want_ext = if is_safe_doc_ext(ext) { ext } else { "xml" };

        if let Some(existing) = self.index.get(id).cloned() {
            let cur_parent = existing.parent.as_deref();
            let cur_name = base_name(&existing.rel);
            let cur_ext = ext_of(&existing);
            let desired_name = format!("{stem}.{want_ext}");
            if cur_parent != eff_parent || cur_name != desired_name || cur_ext != want_ext {
                let target_rel = join_rel(&dir_rel, &self.free_name(&dir_rel, &stem, Some(want_ext), Some(id)));
                if target_rel != existing.rel {
                    self.relocate(&[(id.to_string(), target_rel)])?;
                }
            }
        } else {
            let rel = join_rel(&dir_rel, &self.free_name(&dir_rel, &stem, Some(want_ext), None));
            let path = self.abs(&rel);
            if let Some(parent_dir) = path.parent() {
                crate::vfs::create_dir_all(parent_dir)?;
            }
            self.index.insert(
                id.to_string(),
                Meta { rel, created: meta.created, modified: meta.modified, ..Default::default() },
            );
            derive_tree(&mut self.index);
        }

        let path = self.path(id);
        let same_bytes = crate::vfs::read(&path).is_ok_and(|cur| cur == raw_bytes);
        if !same_bytes {
            if let Some(parent_dir) = path.parent() {
                crate::vfs::create_dir_all(parent_dir)?;
            }
            self.write_file(&path, raw_bytes)?;
        }
        if let Some(m) = self.index.get_mut(id) {
            copy_synced_meta_fields(m, meta);
        }
        let rel = self.index[id].rel.clone();
        let renamed = prev_rel.as_deref() != Some(rel.as_str());
        let meta_changed = prev_meta.as_ref().is_none_or(|pm| !synced_meta_eq(pm, &self.index[id]));
        let file_title = self.named_by_file(id).then(|| self.markdown_title(id));
        if self.loaded.borrow().contains_key(id) {
            self.loaded.borrow_mut().insert(id.to_string(), String::from_utf8_lossy(raw_bytes).into_owned());
        }
        self.parsed.remove(id);
        self.save_index()?;
        Ok(RemoteDocApplied {
            changed: !same_bytes,
            was_deleted: false,
            renamed,
            meta_changed,
            rel,
            file_title,
        })
    }

    pub fn apply_remote_trash(&mut self, id: &str, item: Option<(&Trashed, &str, Option<&[u8]>)>) -> Result<()> {
        anyhow::ensure!(is_safe_entity_id(id), "无效的回收站实体标识");
        let _lock = self.begin_write()?;
        match item {
            None => {
                if let Some(p) = self.validated_trash_target(id) {
                    let r = if p.fs_is_dir() { safe_remove_dir_no_symlinks(&p) } else { crate::vfs::remove_file(&p) };
                    if let Err(e) = r
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(e.into());
                    }
                }
                self.trash.remove(id);
                crate::versions::purge(&self.history_base(), id);
                self.save_trash()?;
            }
            Some((trashed, ext, bytes)) => {
                anyhow::ensure!(ext.is_empty() || is_safe_doc_ext(ext), "无效的回收站文件扩展名");
                let dir = self.trash_dir();
                anyhow::ensure!(!dir.fs_symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()), "回收站目录为符号链接");
                crate::vfs::create_dir_all(&dir)?;
                let canon_dir = dir.fs_canonicalize()?;
                if trashed.meta.folder.is_some() {
                    let target = dir.join(id);
                    anyhow::ensure!(!target.fs_symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()), "目标路径为符号链接");
                    crate::vfs::create_dir_all(&target)?;
                    anyhow::ensure!(target.fs_canonicalize()?.parent() == Some(canon_dir.as_path()), "回收站文件夹越界");
                } else if let Some(raw) = bytes {
                    let eff_ext = if is_safe_doc_ext(ext) { ext } else {
                        let e = ext_of(&trashed.meta);
                        if is_safe_doc_ext(e) { e } else { "xml" }
                    };
                    let target = dir.join(format!("{id}.{eff_ext}"));
                    anyhow::ensure!(!target.fs_symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()), "目标路径为符号链接");
                    anyhow::ensure!(target.parent().and_then(|p| p.fs_canonicalize().ok()) == Some(canon_dir), "回收站文件越界");
                    write_atomic(&target, raw)?;
                }
                // Never delete or clobber live documents from TrashItem arrival; document
                // deletion goes through apply_remote_document with conflict arbitration.
                self.trash.insert(id.to_string(), trashed.clone());
                self.save_trash()?;
            }
        }
        Ok(())
    }

    pub fn apply_remote_history(&mut self, doc_id: &str, name: &str, trashed: bool, content: Option<&[u8]>, label: Option<&str>) -> Result<()> {
        anyhow::ensure!(crate::versions::safe_id(doc_id), "无效的历史文档标识");
        anyhow::ensure!(crate::versions::safe_name(name), "无效的历史版本文件名");
        match content {
            None => {
                crate::versions::remove(&self.history_base(), doc_id, name, false)?;
                crate::versions::remove(&self.history_base(), doc_id, name, true)
            }
            Some(bytes) => {
                let _ = crate::versions::remove(&self.history_base(), doc_id, name, !trashed);
                crate::versions::write_raw(&self.history_base(), doc_id, name, trashed, bytes)?;
                crate::versions::set_label(&self.history_base(), doc_id, name, label, trashed)
            }
        }
    }

    pub fn is_asset_referenced(&mut self, rel_path: &str) -> bool {
        let Some(target) = normalize_asset_rel(rel_path) else {
            return false;
        };
        for (id, m) in &self.index {
            if m.local() || m.folder.is_some() {
                continue;
            }
            let ext = ext_of(m);
            if let Ok(bytes) = crate::vfs::read(self.path(id))
                && Self::doc_asset_refs_from_bytes(ext, &bytes).iter().any(|r| r == &target)
            {
                return true;
            }
        }
        for (id, t) in &self.trash {
            if t.meta.folder.is_some() {
                continue;
            }
            let ext = ext_of(&t.meta);
            if let Some(p) = self.validated_trash_target(id)
                && let Ok(bytes) = crate::vfs::read(p)
                && Self::doc_asset_refs_from_bytes(ext, &bytes).iter().any(|r| r == &target)
            {
                return true;
            }
        }
        let hist_base = self.history_base();
        for dir_name in ["history", "history-trash"] {
            if let Ok(doc_dirs) = crate::vfs::read_dir(hist_base.join(dir_name)) {
                for entry in doc_dirs.flatten() {
                    if !entry.file_type().is_ok_and(|ft| ft.fs_is_dir()) {
                        continue;
                    }
                    let doc_id = entry.file_name().to_string_lossy().into_owned();
                    if !crate::versions::safe_id(&doc_id) {
                        continue;
                    }
                    let doc_dir = entry.path();
                    if let Ok(files) = crate::vfs::read_dir(&doc_dir) {
                        for f in files.flatten() {
                            let fname = f.file_name().to_string_lossy().into_owned();
                            if !crate::versions::safe_name(&fname) {
                                continue;
                            }
                            let ext = if fname.ends_with(".md") { "md" } else { "xml" };
                            if let Ok(bytes) = crate::vfs::read(f.path())
                                && Self::doc_asset_refs_from_bytes(ext, &bytes).iter().any(|r| r == &target)
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    pub fn create_conflict_copy(&mut self, orig_id: &str, ext: &str, raw_bytes: &[u8], device_label: &str) -> Result<String> {
        anyhow::ensure!(is_safe_entity_id(orig_id), "无效的源文档标识");
        let _lock = self.begin_write()?;
        let parent_rel_dir = self.index.get(orig_id).map(|m| parent_rel(&m.rel).to_string()).unwrap_or_default();
        let orig_stem = self
            .index
            .get(orig_id)
            .map(|m| layout::unnumbered(stem_of(base_name(&m.rel))).to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::model::untitled().to_string());
        let (_, mon, day, hour, min) = crate::time::civil(now());
        let dev = file_name_for(if device_label.trim().is_empty() { "remote" } else { device_label });
        let base_title = file_name_for(&format!("{orig_stem} (冲突副本 - {dev} {mon:02}-{day:02} {hour:02}{min:02})"));
        let want_ext = if is_safe_doc_ext(ext) { ext } else { "xml" };
        let rel = join_rel(&parent_rel_dir, &self.free_name(&parent_rel_dir, &base_title, Some(want_ext), None));
        let conflict_id = uuid::Uuid::new_v4().to_string();
        let path = self.abs(&rel);
        if let Some(p) = path.parent() {
            crate::vfs::create_dir_all(p)?;
        }
        write_atomic(&path, raw_bytes)?;
        let t = now();
        self.index.insert(
            conflict_id.clone(),
            Meta { created: t, modified: t, rel, ..Default::default() },
        );
        derive_tree(&mut self.index);
        self.save_index()?;
        Ok(conflict_id)
    }

    pub fn adopt_entity_id(&mut self, old_id: &str, new_id: &str) -> Result<()> {
        anyhow::ensure!(is_safe_entity_id(old_id) && is_safe_entity_id(new_id), "无效的实体标识");
        if old_id == new_id {
            return Ok(());
        }
        let _lock = self.begin_write()?;
        anyhow::ensure!(!self.index.contains_key(new_id), "目标实体 ID 已存在");
        let Some(meta) = self.index.remove(old_id) else { return Ok(()) };
        self.index.insert(new_id.to_string(), meta);
        derive_tree(&mut self.index);
        let loaded = self.loaded.borrow_mut().remove(old_id);
        if let Some(v) = loaded {
            self.loaded.borrow_mut().insert(new_id.to_string(), v);
        }
        if let Some(v) = self.parsed.remove(old_id) {
            self.parsed.insert(new_id.to_string(), v);
        }
        let copied = self.copied.borrow_mut().remove(old_id);
        if let Some(v) = copied {
            self.copied.borrow_mut().insert(new_id.to_string(), v);
        }
        let old_hist = self.history_base().join("history").join(old_id);
        let new_hist = self.history_base().join("history").join(new_id);
        if old_hist.fs_is_dir() && !new_hist.fs_exists() {
            let _ = crate::vfs::rename(&old_hist, &new_hist);
        }
        self.save_index()
    }

    fn safe_parent_for(&self, id: &str, requested: Option<&str>) -> Option<String> {
        let mut cur = requested.filter(|p| *p != id && self.index.get(*p).is_some_and(|m| m.folder.is_some()))?;
        let mut seen = HashSet::new();
        seen.insert(id);
        while let Some(m) = self.index.get(cur) {
            if !seen.insert(cur) {
                return None;
            }
            match m.parent.as_deref() {
                Some(next) if next == id => return None,
                Some(next) => cur = next,
                None => break,
            }
        }
        requested.map(str::to_string)
    }
}

fn synced_meta_eq(a: &Meta, b: &Meta) -> bool {
    a.pinned == b.pinned
        && a.starred == b.starred
        && a.order == b.order
        && a.root_order == b.root_order
        && a.width == b.width
        && a.catalogue_closed == b.catalogue_closed
        && a.reading == b.reading
        && a.info == b.info
        && a.refs_hidden == b.refs_hidden
}

fn copy_synced_meta_fields(dst: &mut Meta, src: &Meta) {
    dst.created = if src.created > 0 { src.created } else { dst.created };
    dst.modified = src.modified.max(dst.created);
    dst.pinned = src.pinned;
    dst.starred = src.starred;
    dst.order = src.order;
    dst.root_order = src.root_order;
    dst.width = src.width;
    dst.catalogue_closed = src.catalogue_closed;
    dst.reading = src.reading;
    dst.info = src.info;
    dst.refs_hidden = src.refs_hidden;
}

fn sanitized_stem(file_name: &str, ext: &str) -> String {
    let suffix = format!(".{ext}");
    let raw = file_name.strip_suffix(&suffix).unwrap_or_else(|| stem_of(file_name));
    file_name_for(raw)
}

fn collect_assets_rel(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(rd) = crate::vfs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.ends_with(".tmp") || name == "thumbs" && dir == root {
            continue;
        }
        if entry.file_type().is_ok_and(|ft| ft.fs_is_dir()) {
            collect_assets_rel(root, &p, &format!("{prefix}/{name}"), out);
        } else if entry.file_type().is_ok_and(|ft| ft.fs_is_file()) {
            out.push(format!("{prefix}/{name}"));
        }
    }
}

fn collect_doc_asset_refs(doc: &Document, out: &mut Vec<String>) {
    if let Some(c) = &doc.cover {
        out.push(c.src.clone());
    }
    doc.for_each_block(|b| {
        match &b.kind {
            crate::model::BlockKind::Image { src, .. } => out.push(src.clone()),
            crate::model::BlockKind::File(f) if f.src_kind == "path" => out.push(f.src.clone()),
            crate::model::BlockKind::Board(bd) => {
                for f in bd.files.values() {
                    if let Some(p) = &f.path {
                        out.push(p.clone());
                    }
                }
            }
            _ => {}
        }
        for r in &b.text.runs {
            if let Some(f) = r.marks.inline.as_ref().and_then(crate::model::Inline::file)
                && f.src_kind == "path"
            {
                out.push(f.src.clone());
            }
        }
    });
}

fn normalize_asset_rel(src: &str) -> Option<String> {
    let s = src.strip_prefix("file://").unwrap_or(src).replace('\\', "/");
    if s.starts_with(".assets/") && !s.contains("..") {
        return Some(s);
    }
    if let Some(rest) = s.strip_prefix("./.assets/") && !rest.contains("..") {
        return Some(format!(".assets/{rest}"));
    }
    if let Some((_, rest)) = s.rsplit_once("/.assets/") && !rest.is_empty() && !rest.contains("..") {
        return Some(format!(".assets/{rest}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docxml;
    use crate::model::{Block, Document, RichText};

    fn new_lib(name: &str) -> (PathBuf, Store) {
        let dir = crate::vfs::temp_dir().join(format!("quiet-sync-bridge-{name}-{}", uuid::Uuid::new_v4()));
        let s = Store::open_at(dir.clone()).unwrap();
        (dir, s)
    }

    #[test]
    fn remote_folder_and_doc_with_links_and_collisions() {
        let (dir, mut s) = new_lib("folder-doc");
        let f_id = uuid::Uuid::new_v4().to_string();
        let meta = Meta { created: 100, modified: 200, pinned: true, ..Default::default() };
        assert!(s.apply_remote_folder(&f_id, "Projects", None, &meta, false).unwrap());

        let d1_id = uuid::Uuid::new_v4().to_string();
        let d2_id = uuid::Uuid::new_v4().to_string();
        let d1_bytes = docxml::serialize(&Document { title: RichText::plain("Spec"), blocks: vec![Block::text("Alpha")], ..Default::default() }).into_bytes();
        let d2_bytes = docxml::serialize(&Document { title: RichText::plain("Spec"), blocks: vec![Block::text("Beta")], ..Default::default() }).into_bytes();

        let res1 = s.apply_remote_document(&d1_id, "Spec.xml", Some(&f_id), "xml", &meta, &d1_bytes, false).unwrap();
        assert!(res1.changed);
        assert_eq!(res1.rel, "Projects/Spec.xml");

        // Second doc with same name gets the store's clash name (` (2)`) instead of overwriting d1.
        let res2 = s.apply_remote_document(&d2_id, "Spec.xml", Some(&f_id), "xml", &meta, &d2_bytes, false).unwrap();
        assert_eq!(res2.rel, "Projects/Spec (2).xml");

        // Rename folder remotely and verify children follow cleanly.
        assert!(s.apply_remote_folder(&f_id, "Specs", None, &meta, false).unwrap());
        let inv = s.sync_inventory().unwrap();
        assert_eq!(inv.folders[0].rel, "Specs");
        assert!(inv.docs.iter().any(|d| d.id == d1_id && d.rel == "Specs/Spec.xml"));

        // Conflict copy creation & adopt_entity_id.
        let conflict_id = s.create_conflict_copy(&d1_id, "xml", &d2_bytes, "MacBook").unwrap();
        assert_ne!(conflict_id, d1_id);
        assert!(s.read_raw_entity(&conflict_id).unwrap().is_some());

        let new_id = uuid::Uuid::new_v4().to_string();
        s.adopt_entity_id(&d2_id, &new_id).unwrap();
        assert!(s.read_raw_entity(&d2_id).unwrap().is_none());
        assert!(s.read_raw_entity(&new_id).unwrap().is_some());

        let _ = crate::vfs::remove_dir_all(dir);
    }

    #[test]
    fn remote_trash_history_and_cycle_breaking() {
        let (dir, mut s) = new_lib("trash-hist");
        let f1 = uuid::Uuid::new_v4().to_string();
        let f2 = uuid::Uuid::new_v4().to_string();
        let meta = Meta { created: 10, modified: 20, ..Default::default() };
        s.apply_remote_folder(&f1, "F1", None, &meta, false).unwrap();
        s.apply_remote_folder(&f2, "F2", Some(&f1), &meta, false).unwrap();
        // Attempting to make f1 a child of f2 must not loop or error; it breaks cycle cleanly.
        s.apply_remote_folder(&f1, "F1", Some(&f2), &meta, false).unwrap();

        let doc_id = uuid::Uuid::new_v4().to_string();
        let xml = b"<title>H</title>\n<p>snap</p>\n";
        s.apply_remote_history(&doc_id, "1700000000000-abcd1234.xml", false, Some(xml), Some("v1")).unwrap();
        let (raw, label) = s.read_history_raw(&doc_id, "1700000000000-abcd1234.xml", false).unwrap().unwrap();
        assert_eq!(raw, xml);
        assert_eq!(label.as_deref(), Some("v1"));

        let trashed = Trashed { meta: Meta { rel: "Gone.xml".into(), ..meta }, deleted: now(), root: None };
        s.apply_remote_trash(&doc_id, Some((&trashed, "xml", Some(xml)))).unwrap();
        assert_eq!(s.list_trash().len(), 1);
        s.apply_remote_trash(&doc_id, None).unwrap();
        assert!(s.list_trash().is_empty());

        let _ = crate::vfs::remove_dir_all(dir);
    }

    #[test]
    fn extracts_asset_refs_from_xml_md_and_board() {
        let xml = b"<title cover=\".assets/c.png\">T</title>\n<img path=\".assets/a.png\"/>\n";
        let refs = Store::doc_asset_refs_from_bytes("xml", xml);
        assert_eq!(refs, vec![".assets/c.png", ".assets/a.png"]);

        let md = b"![pic](.assets/m.png)\n";
        let refs = Store::doc_asset_refs_from_bytes("md", md);
        assert_eq!(refs, vec![".assets/m.png"]);

        let mut b = crate::whiteboard::Board::default();
        let mut img = crate::board::new_element::new_element(
            crate::board::types::ElementType::Rectangle,
            crate::board::new_element::ElementOpts { width: 100., height: 100., ..Default::default() },
        );
        img.kind = crate::board::types::Kind::Image(crate::board::types::ImageData {
            file_id: Some("f1".into()),
            status: crate::board::types::ImageStatus::Saved,
            scale: [1., 1.],
            crop: None,
        });
        b.elements.push(img);
        b.files.insert(
            "f1".into(),
            crate::board::types::FileData {
                id: "f1".into(),
                mime_type: "image/png".into(),
                data_url: None,
                path: Some(".assets/b.png".into()),
                created: 1,
                last_retrieved: None,
                version: None,
                extra: Default::default(),
            },
        );
        let refs = Store::doc_asset_refs_from_bytes("excalidraw", b.file_json().as_bytes());
        assert_eq!(refs, vec![".assets/b.png"]);
    }

    #[test]
    fn rejects_traversal_preserves_live_on_trash_tracks_rename_and_asset_refs() {
        let (dir, mut s) = new_lib("hardened");
        let meta = Meta { created: 10, modified: 20, ..Default::default() };
        assert!(s.apply_remote_trash("../../outside", None).is_err());
        assert!(s.apply_remote_history("../../outside", "1700000000000-abcd1234.xml", false, Some(b"x"), None).is_err());
        assert!(s.apply_remote_document("../bad", "x.xml", None, "xml", &meta, b"x", false).is_err());
        assert!(s.apply_remote_folder("../bad", "bad", None, &meta, false).is_err());

        let doc_id = uuid::Uuid::new_v4().to_string();
        let md_v1 = b"# Hello\n![img](.assets/pic.png)\n";
        let res1 = s.apply_remote_document(&doc_id, "First.md", None, "md", &meta, md_v1, false).unwrap();
        assert!(res1.changed && res1.renamed);
        assert!(s.is_asset_referenced(".assets/pic.png"));

        // Pure rename without body change reports renamed + file_title.
        let res2 = s.apply_remote_document(&doc_id, "Second.md", None, "md", &meta, md_v1, false).unwrap();
        assert!(!res2.changed);
        assert!(res2.renamed);
        assert_eq!(res2.file_title.as_deref(), Some("Second"));

        // Incoming TrashItem must not remove a live document.
        let trashed = Trashed { meta: Meta { rel: "Second.md".into(), markdown: true, ..meta.clone() }, deleted: now(), root: None };
        s.apply_remote_trash(&doc_id, Some((&trashed, "md", Some(md_v1)))).unwrap();
        assert!(s.read_raw_entity(&doc_id).unwrap().is_some());

        // History move between active & trash clears old copy, and tombstone clears either location.
        let snap = "1700000000000-abcd1234.xml";
        s.apply_remote_history(&doc_id, snap, false, Some(b"<title>A</title><p>1</p>"), None).unwrap();
        s.apply_remote_history(&doc_id, snap, true, Some(b"<title>A</title><p>1</p>"), None).unwrap();
        assert!(s.read_history_raw(&doc_id, snap, false).unwrap().is_none());
        assert!(s.read_history_raw(&doc_id, snap, true).unwrap().is_some());
        s.apply_remote_history(&doc_id, snap, false, None, None).unwrap();
        assert!(s.read_history_raw(&doc_id, snap, true).unwrap().is_none());

        let _ = crate::vfs::remove_dir_all(dir);
    }
}
