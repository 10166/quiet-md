use anyhow::Result;
use fdl_core::store::{
    is_safe_doc_ext, is_safe_entity_id, InfoRow, Meta, PageWidth, RawEntitySnapshot, Store,
    SyncInventory, Trashed, INTERNAL,
};
use quiet_sync_proto::entity::{sha256_hex, EntityKind, SyncInfoRow, SyncMeta};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub const LOCAL_ONLY_PREF_KEYS: &[&str] = &["seen_version", "migrate_later", "browser"];
pub const LOCAL_ONLY_SETTINGS_KEYS: &[&str] = &[
    "window_size",
    "window_origin",
    "window_display",
    "quick_note_size",
    "sidebar_width",
    "scroll_top",
];

pub fn trash_entity_id(id: &str) -> String {
    format!("trash:{id}")
}

pub fn parse_trash_entity_id(entity_id: &str) -> Option<&str> {
    let rest = entity_id.strip_prefix("trash:")?;
    is_safe_entity_id(rest).then_some(rest)
}

pub fn history_entity_id(doc_id: &str, name: &str) -> String {
    format!("hist:{doc_id}/{name}")
}

pub fn parse_history_entity_id(entity_id: &str) -> Option<(&str, &str)> {
    let rest = entity_id.strip_prefix("hist:")?;
    let (doc_id, name) = rest.split_once('/')?;
    if fdl_core::versions::safe_id(doc_id) && fdl_core::versions::safe_name(name) {
        Some((doc_id, name))
    } else {
        None
    }
}

pub fn config_entity_id(key: &str) -> String {
    format!("cfg:{key}")
}

pub fn parse_config_entity_id(entity_id: &str) -> Option<&str> {
    let key = entity_id.strip_prefix("cfg:")?;
    is_valid_config_key(key).then_some(key)
}

pub fn is_valid_config_key(key: &str) -> bool {
    match key {
        "preferences" | "shortcuts" | "library_settings" => true,
        _ => key.strip_prefix("theme:").is_some_and(|name| {
            !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..") && !name.contains('\0') && name.ends_with(".css")
        }),
    }
}

pub fn sync_meta_from_store(m: &Meta) -> SyncMeta {
    SyncMeta {
        created: m.created,
        modified: m.modified,
        pinned: m.pinned,
        order: m.order,
        root_order: m.root_order,
        starred: m.starred,
        width: match m.width {
            PageWidth::Standard => "standard".into(),
            PageWidth::Wide => "wide".into(),
            PageWidth::Full => "full".into(),
        },
        catalogue_closed: m.catalogue_closed,
        reading: m.reading,
        info: SyncInfoRow { hidden: m.info.hidden, created: m.info.created, modified: m.info.modified },
        refs_hidden: m.refs_hidden,
    }
}

pub fn store_meta_from_sync(m: &SyncMeta) -> Meta {
    Meta {
        created: m.created,
        modified: m.modified,
        pinned: m.pinned,
        order: m.order,
        root_order: m.root_order,
        starred: m.starred,
        width: match m.width.as_str() {
            "wide" => PageWidth::Wide,
            "full" => PageWidth::Full,
            _ => PageWidth::Standard,
        },
        catalogue_closed: m.catalogue_closed,
        reading: m.reading,
        info: InfoRow { hidden: m.info.hidden, created: m.info.created, modified: m.info.modified },
        refs_hidden: m.refs_hidden,
        ..Default::default()
    }
}

#[derive(Clone, Debug)]
pub enum LocalEntity {
    Folder {
        id: String,
        name: String,
        parent_id: Option<String>,
        meta: SyncMeta,
    },
    Document {
        id: String,
        file_name: String,
        parent_id: Option<String>,
        ext: String,
        meta: SyncMeta,
        bytes: Vec<u8>,
        asset_refs: Vec<String>,
    },
    Asset {
        rel_path: String,
        bytes: Vec<u8>,
        mime: String,
    },
    TrashItem {
        id: String,
        orig_rel: String,
        folder: Option<String>,
        ext: String,
        deleted: u64,
        root: Option<String>,
        meta: SyncMeta,
        bytes: Option<Vec<u8>>,
    },
    HistorySnapshot {
        doc_id: String,
        name: String,
        trashed: bool,
        label: Option<String>,
        bytes: Vec<u8>,
    },
    Config {
        key: String,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
pub enum IncomingApply {
    Folder {
        id: String,
        name: String,
        parent_id: Option<String>,
        meta: SyncMeta,
        deleted: bool,
        expected_local_hash: Option<Option<String>>,
    },
    Document {
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
    },
    AssetDelete {
        rel_path: String,
    },
    TrashItem {
        id: String,
        orig_rel: String,
        folder: Option<String>,
        ext: String,
        deleted: u64,
        root: Option<String>,
        meta: SyncMeta,
        bytes: Option<Vec<u8>>,
        purged: bool,
    },
    HistorySnapshot {
        doc_id: String,
        name: String,
        trashed: bool,
        label: Option<String>,
        bytes: Option<Vec<u8>>,
    },
    Config {
        key: String,
        bytes: Option<Vec<u8>>,
        expected_local_hash: Option<Option<String>>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    pub tree_changed: bool,
    pub updated_docs: Vec<String>,
    pub renamed_docs: Vec<String>,
    pub meta_docs: Vec<String>,
    pub deleted_docs: Vec<String>,
    pub trash_changed: bool,
    pub history_docs: Vec<String>,
    pub config_keys: Vec<String>,
    pub live_conflicts: Vec<LiveConflict>,
    pub merged_back_docs: Vec<(String, Vec<u8>, SyncMeta)>,
    pub skipped_raced_ids: Vec<(EntityKind, String)>,
    pub requeue_assets: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct LiveConflict {
    pub doc_id: String,
    pub conflict_doc_id: String,
    pub title: String,
    pub device_name: String,
}

pub trait LocalLibraryAdapter: Send + Sync + 'static {
    fn read_inventory(&self) -> BoxFuture<'_, Result<SyncInventory>>;
    fn read_entity(&self, kind: EntityKind, entity_id: &str) -> BoxFuture<'_, Result<Option<LocalEntity>>>;
    fn read_asset(&self, rel_path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>>>;
    fn write_asset(&self, rel_path: &str, bytes: &[u8]) -> BoxFuture<'_, Result<()>>;
    fn apply_remote(&self, ops: Vec<IncomingApply>) -> BoxFuture<'_, Result<ApplyReport>>;
    fn adopt_id(&self, old_id: &str, new_id: &str) -> BoxFuture<'_, Result<()>>;
    fn create_conflict_copy(&self, orig_id: &str, ext: &str, raw_bytes: &[u8], device_label: &str) -> BoxFuture<'_, Result<String>>;
}

pub struct FsLibraryAdapter {
    store: Arc<Mutex<Store>>,
}

impl FsLibraryAdapter {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<Mutex<Store>> {
        &self.store
    }
}

pub fn read_local_entity_sync(store: &mut Store, kind: EntityKind, entity_id: &str) -> Result<Option<LocalEntity>> {
    match kind {
        EntityKind::Folder | EntityKind::Document => {
            let Some(snap) = store.read_raw_entity(entity_id)? else { return Ok(None) };
            Ok(Some(match snap {
                RawEntitySnapshot::Folder { id, name, parent_id, meta, .. } => LocalEntity::Folder {
                    id,
                    name,
                    parent_id,
                    meta: sync_meta_from_store(&meta),
                },
                RawEntitySnapshot::Document { id, file_name, parent_id, ext, meta, bytes, .. } => {
                    let asset_refs = Store::doc_asset_refs_from_bytes(&ext, &bytes);
                    LocalEntity::Document {
                        id,
                        file_name,
                        parent_id,
                        ext,
                        meta: sync_meta_from_store(&meta),
                        bytes,
                        asset_refs,
                    }
                }
            }))
        }
        EntityKind::Asset => {
            let Some(bytes) = read_asset_sync(store.root(), entity_id)? else { return Ok(None) };
            Ok(Some(LocalEntity::Asset {
                rel_path: entity_id.to_string(),
                bytes,
                mime: guess_mime(entity_id).to_string(),
            }))
        }
        EntityKind::TrashItem => {
            let Some(id) = parse_trash_entity_id(entity_id) else { return Ok(None) };
            let Some((t, ext, bytes)) = store.read_trash_raw(id)? else { return Ok(None) };
            Ok(Some(LocalEntity::TrashItem {
                id: id.to_string(),
                orig_rel: t.meta.rel.clone(),
                folder: t.meta.folder.clone(),
                ext,
                deleted: t.deleted,
                root: t.root,
                meta: sync_meta_from_store(&t.meta),
                bytes,
            }))
        }
        EntityKind::HistorySnapshot => {
            let Some((doc_id, name)) = parse_history_entity_id(entity_id) else { return Ok(None) };
            if let Some((bytes, label)) = store.read_history_raw(doc_id, name, false)? {
                return Ok(Some(LocalEntity::HistorySnapshot {
                    doc_id: doc_id.to_string(),
                    name: name.to_string(),
                    trashed: false,
                    label,
                    bytes,
                }));
            }
            if let Some((bytes, label)) = store.read_history_raw(doc_id, name, true)? {
                return Ok(Some(LocalEntity::HistorySnapshot {
                    doc_id: doc_id.to_string(),
                    name: name.to_string(),
                    trashed: true,
                    label,
                    bytes,
                }));
            }
            Ok(None)
        }
        EntityKind::Config => {
            let Some(key) = parse_config_entity_id(entity_id) else { return Ok(None) };
            let Some(bytes) = read_config_sync(store.root(), store.is_flat(), key)? else { return Ok(None) };
            Ok(Some(LocalEntity::Config { key: key.to_string(), bytes }))
        }
    }
}

pub fn check_expected_local_hash(
    store: &mut Store,
    kind: EntityKind,
    id: &str,
    expected: Option<&str>,
) -> Result<bool> {
    let cur = read_local_entity_sync(store, kind, id)?;
    let cur_hash = cur.as_ref().map(local_entity_content_hash);
    Ok(cur_hash.as_deref() == expected)
}

pub fn apply_remote_sync(store: &mut Store, ops: &[IncomingApply]) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    for op in ops {
        match op {
            IncomingApply::Folder { id, name, parent_id, meta, deleted, expected_local_hash } => {
                if !is_safe_entity_id(id) {
                    anyhow::bail!("无效的远端文件夹标识: {id}");
                }
                if let Some(exp) = expected_local_hash
                    && !check_expected_local_hash(store, EntityKind::Folder, id, exp.as_deref())?
                {
                    report.skipped_raced_ids.push((EntityKind::Folder, id.clone()));
                    continue;
                }
                let m = store_meta_from_sync(meta);
                if store.apply_remote_folder(id, name, parent_id.as_deref(), &m, *deleted)? {
                    report.tree_changed = true;
                }
            }
            IncomingApply::Document { id, file_name, parent_id, ext, meta, bytes, deleted, preserve_in_local_trash, expected_local_hash, .. } => {
                if !is_safe_entity_id(id) {
                    anyhow::bail!("无效的远端文档标识: {id}");
                }
                if let Some(exp) = expected_local_hash
                    && !check_expected_local_hash(store, EntityKind::Document, id, exp.as_deref())?
                {
                    report.skipped_raced_ids.push((EntityKind::Document, id.clone()));
                    continue;
                }
                let m = store_meta_from_sync(meta);
                let res = store.apply_remote_document_with(
                    id,
                    file_name,
                    parent_id.as_deref(),
                    ext,
                    &m,
                    bytes,
                    *deleted,
                    *preserve_in_local_trash,
                )?;
                report.tree_changed = true;
                if res.was_deleted {
                    report.deleted_docs.push(id.clone());
                } else {
                    if res.changed {
                        report.updated_docs.push(id.clone());
                    }
                    if res.renamed {
                        report.renamed_docs.push(id.clone());
                    }
                    if res.meta_changed {
                        report.meta_docs.push(id.clone());
                    }
                }
            }
            IncomingApply::AssetDelete { rel_path } => {
                if store.is_asset_referenced(rel_path) {
                    report.requeue_assets.push(rel_path.clone());
                } else {
                    delete_asset_sync(store.root(), rel_path)?;
                }
            }
            IncomingApply::TrashItem { id, orig_rel, folder, ext, deleted, root, meta, bytes, purged } => {
                if !is_safe_entity_id(id) || (!ext.is_empty() && !is_safe_doc_ext(ext)) {
                    anyhow::bail!("无效的远端回收站条目: {id}");
                }
                if *purged {
                    store.apply_remote_trash(id, None)?;
                } else {
                    let mut m = store_meta_from_sync(meta);
                    m.rel = orig_rel.clone();
                    m.folder = folder.clone();
                    if let Some((kind, md)) = fdl_core::store::DocKind::of_name(orig_rel) {
                        m.kind = kind;
                        m.markdown = md;
                    }
                    let trashed = Trashed { meta: m, deleted: *deleted, root: root.clone() };
                    store.apply_remote_trash(id, Some((&trashed, ext, bytes.as_deref())))?;
                }
                report.trash_changed = true;
                report.tree_changed = true;
            }
            IncomingApply::HistorySnapshot { doc_id, name, trashed, label, bytes } => {
                store.apply_remote_history(doc_id, name, *trashed, bytes.as_deref(), label.as_deref())?;
                if !report.history_docs.contains(doc_id) {
                    report.history_docs.push(doc_id.clone());
                }
            }
            IncomingApply::Config { key, bytes, expected_local_hash } => {
                if !is_valid_config_key(key) {
                    anyhow::bail!("无效的配置实体键: {key}");
                }
                let eid = config_entity_id(key);
                if let Some(exp) = expected_local_hash
                    && !check_expected_local_hash(store, EntityKind::Config, &eid, exp.as_deref())?
                {
                    report.skipped_raced_ids.push((EntityKind::Config, eid));
                    continue;
                }
                write_config_sync(store.root(), store.is_flat(), key, bytes.as_deref())?;
                if !report.config_keys.contains(key) {
                    report.config_keys.push(key.clone());
                }
            }
        }
    }
    Ok(report)
}

pub fn delete_asset_sync(root: &Path, rel_path: &str) -> Result<()> {
    let Some(path) = safe_asset_path(root, rel_path) else {
        anyhow::bail!("invalid asset path: {rel_path}");
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn read_asset_sync(root: &Path, rel_path: &str) -> Result<Option<Vec<u8>>> {
    let Some(path) = safe_asset_path(root, rel_path) else { return Ok(None) };
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn write_asset_sync(root: &Path, rel_path: &str, bytes: &[u8]) -> Result<()> {
    let Some(path) = safe_asset_path(root, rel_path) else {
        anyhow::bail!("invalid asset path: {rel_path}");
    };
    if std::fs::read(&path).is_ok_and(|cur| cur == bytes) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if safe_asset_path(root, rel_path).is_none() {
        anyhow::bail!("unsafe asset path after dir creation: {rel_path}");
    }
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn safe_asset_path(root: &Path, rel_path: &str) -> Option<PathBuf> {
    if rel_path.contains('\0') {
        return None;
    }
    let norm = rel_path.replace('\\', "/");
    if !(norm.starts_with(".assets/") || norm.starts_with("assets/")) || norm.contains("..") {
        return None;
    }
    let canon_root = std::fs::canonicalize(root).ok()?;
    let mut cur = root.to_path_buf();
    for part in norm.split('/').filter(|s| !s.is_empty()) {
        if part == "." || part == ".." || part.contains('\\') {
            return None;
        }
        cur.push(part);
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return None;
            }
            if let Ok(canon_cur) = std::fs::canonicalize(&cur)
                && !canon_cur.starts_with(&canon_root)
            {
                return None;
            }
        }
    }
    Some(cur)
}

pub fn config_file_path(root: &Path, flat: bool, key: &str) -> Option<PathBuf> {
    let internal = if flat { root.to_path_buf() } else { root.join(INTERNAL) };
    match key {
        "preferences" => Some(internal.join("preferences.json")),
        "shortcuts" => Some(internal.join("shortcuts.json")),
        "library_settings" => Some(internal.join("settings.json")),
        _ => {
            let name = key.strip_prefix("theme:")?;
            if !is_valid_config_key(key) {
                return None;
            }
            Some(internal.join("themes").join(name))
        }
    }
}

pub fn read_config_sync(root: &Path, flat: bool, key: &str) -> Result<Option<Vec<u8>>> {
    let Some(path) = config_file_path(root, flat, key) else { return Ok(None) };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if (key == "preferences" || key == "library_settings")
        && let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(obj) = val.as_object_mut()
    {
        let strip = if key == "preferences" { LOCAL_ONLY_PREF_KEYS } else { LOCAL_ONLY_SETTINGS_KEYS };
        for k in strip {
            obj.remove(*k);
        }
        return Ok(Some(serde_json::to_vec_pretty(&val)?));
    }
    Ok(Some(bytes))
}

pub fn write_config_sync(root: &Path, flat: bool, key: &str, bytes: Option<&[u8]>) -> Result<()> {
    let Some(path) = config_file_path(root, flat, key) else { return Ok(()) };
    let Some(bytes) = bytes else {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let final_bytes = if (key == "preferences" || key == "library_settings")
        && let Ok(mut remote_val) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Ok(cur_bytes) = std::fs::read(&path)
        && let Ok(cur_val) = serde_json::from_slice::<serde_json::Value>(&cur_bytes)
    {
        if let (Some(ro), Some(co)) = (remote_val.as_object_mut(), cur_val.as_object()) {
            let keep = if key == "preferences" { LOCAL_ONLY_PREF_KEYS } else { LOCAL_ONLY_SETTINGS_KEYS };
            for k in keep {
                if let Some(v) = co.get(*k) {
                    ro.insert((*k).to_string(), v.clone());
                } else {
                    ro.remove(*k);
                }
            }
            serde_json::to_vec_pretty(&remote_val)?
        } else {
            bytes.to_vec()
        }
    } else {
        bytes.to_vec()
    };

    if std::fs::read(&path).is_ok_and(|cur| cur == final_bytes) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &final_bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn local_entity_content_hash(entity: &LocalEntity) -> String {
    match entity {
        LocalEntity::Folder { name, parent_id, meta, .. } => {
            let json = serde_json::to_vec(&(name, parent_id, meta)).unwrap_or_default();
            sha256_hex(&json)
        }
        LocalEntity::Document { file_name, parent_id, ext, meta, bytes, .. } => {
            let body_hash = sha256_hex(bytes);
            let json = serde_json::to_vec(&(file_name, parent_id, ext, meta, body_hash)).unwrap_or_default();
            sha256_hex(&json)
        }
        LocalEntity::Asset { rel_path, bytes, .. } => {
            let body_hash = sha256_hex(bytes);
            sha256_hex(format!("{rel_path}|{body_hash}").as_bytes())
        }
        LocalEntity::TrashItem { orig_rel, folder, ext, deleted, root, meta, bytes, .. } => {
            let bh = bytes.as_deref().map(sha256_hex);
            let json = serde_json::to_vec(&(orig_rel, folder, ext, deleted, root, meta, bh)).unwrap_or_default();
            sha256_hex(&json)
        }
        LocalEntity::HistorySnapshot { doc_id, name, trashed, label, bytes } => {
            let bh = sha256_hex(bytes);
            let json = serde_json::to_vec(&(doc_id, name, trashed, label, bh)).unwrap_or_default();
            sha256_hex(&json)
        }
        LocalEntity::Config { key, bytes } => {
            let bh = sha256_hex(bytes);
            sha256_hex(format!("{key}|{bh}").as_bytes())
        }
    }
}

pub fn guess_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

impl LocalLibraryAdapter for FsLibraryAdapter {
    fn read_inventory(&self) -> BoxFuture<'_, Result<SyncInventory>> {
        Box::pin(async move {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.lock().unwrap().sync_inventory()).await?
        })
    }

    fn read_entity(&self, kind: EntityKind, entity_id: &str) -> BoxFuture<'_, Result<Option<LocalEntity>>> {
        let entity_id = entity_id.to_string();
        Box::pin(async move {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || read_local_entity_sync(&mut store.lock().unwrap(), kind, &entity_id)).await?
        })
    }

    fn read_asset(&self, rel_path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>>> {
        let rel_path = rel_path.to_string();
        Box::pin(async move {
            let root = self.store.lock().unwrap().root().to_path_buf();
            tokio::task::spawn_blocking(move || read_asset_sync(&root, &rel_path)).await?
        })
    }

    fn write_asset(&self, rel_path: &str, bytes: &[u8]) -> BoxFuture<'_, Result<()>> {
        let rel_path = rel_path.to_string();
        let bytes = bytes.to_vec();
        Box::pin(async move {
            let root = self.store.lock().unwrap().root().to_path_buf();
            tokio::task::spawn_blocking(move || write_asset_sync(&root, &rel_path, &bytes)).await?
        })
    }

    fn apply_remote(&self, ops: Vec<IncomingApply>) -> BoxFuture<'_, Result<ApplyReport>> {
        Box::pin(async move {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || apply_remote_sync(&mut store.lock().unwrap(), &ops)).await?
        })
    }

    fn adopt_id(&self, old_id: &str, new_id: &str) -> BoxFuture<'_, Result<()>> {
        let old_id = old_id.to_string();
        let new_id = new_id.to_string();
        Box::pin(async move {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.lock().unwrap().adopt_entity_id(&old_id, &new_id)).await?
        })
    }

    fn create_conflict_copy(&self, orig_id: &str, ext: &str, raw_bytes: &[u8], device_label: &str) -> BoxFuture<'_, Result<String>> {
        let orig_id = orig_id.to_string();
        let ext = ext.to_string();
        let raw_bytes = raw_bytes.to_vec();
        let device_label = device_label.to_string();
        Box::pin(async move {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || {
                store.lock().unwrap().create_conflict_copy(&orig_id, &ext, &raw_bytes, &device_label)
            })
            .await?
        })
    }
}
