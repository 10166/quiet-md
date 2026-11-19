use anyhow::{Context, Result};
use base64::Engine;
use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::decompress_to_vec_zlib_with_limit;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const INLINE_THRESHOLD: usize = 128 * 1024;
pub const MAX_INLINE_DECOMPRESSED: usize = 2 * 1024 * 1024;
pub const MAX_PAYLOAD_DECOMPRESSED: usize = 4 * 1024 * 1024;
pub const MAX_BASE_DECOMPRESSED: usize = 32 * 1024 * 1024;

pub fn is_safe_entity_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn is_safe_doc_ext(ext: &str) -> bool {
    matches!(ext, "xml" | "md" | "excalidraw")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Folder,
    Document,
    Asset,
    TrashItem,
    HistorySnapshot,
    Config,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Document => "document",
            Self::Asset => "asset",
            Self::TrashItem => "trash_item",
            Self::HistorySnapshot => "history_snapshot",
            Self::Config => "config",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "folder" => Some(Self::Folder),
            "document" => Some(Self::Document),
            "asset" => Some(Self::Asset),
            "trash_item" => Some(Self::TrashItem),
            "history_snapshot" => Some(Self::HistorySnapshot),
            "config" => Some(Self::Config),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncInfoRow {
    pub hidden: bool,
    pub created: bool,
    pub modified: bool,
}

impl Default for SyncInfoRow {
    fn default() -> Self {
        Self { hidden: false, created: false, modified: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncMeta {
    pub created: u64,
    pub modified: u64,
    pub pinned: bool,
    pub order: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_order: Option<u64>,
    pub starred: u64,
    pub width: String,
    pub catalogue_closed: bool,
    pub reading: bool,
    pub info: SyncInfoRow,
    pub refs_hidden: bool,
}

impl Default for SyncMeta {
    fn default() -> Self {
        Self {
            created: 0,
            modified: 0,
            pinned: false,
            order: 0,
            root_order: None,
            starred: 0,
            width: "standard".into(),
            catalogue_closed: true,
            reading: false,
            info: SyncInfoRow::default(),
            refs_hidden: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum ContentBody {
    Inline {
        deflated_b64: String,
        raw_sha256: String,
        raw_size: u64,
    },
    Blob {
        hash: String,
        raw_sha256: String,
        raw_size: u64,
    },
}

impl ContentBody {
    pub fn inline_from_bytes(raw: &[u8]) -> Self {
        let compressed = compress_bytes(raw);
        Self::Inline {
            deflated_b64: base64::engine::general_purpose::STANDARD.encode(compressed),
            raw_sha256: sha256_hex(raw),
            raw_size: raw.len() as u64,
        }
    }

    pub fn raw_sha256(&self) -> &str {
        match self {
            Self::Inline { raw_sha256, .. } | Self::Blob { raw_sha256, .. } => raw_sha256,
        }
    }

    pub fn raw_size(&self) -> u64 {
        match self {
            Self::Inline { raw_size, .. } | Self::Blob { raw_size, .. } => *raw_size,
        }
    }

    pub fn blob_hash(&self) -> Option<&str> {
        match self {
            Self::Inline { .. } => None,
            Self::Blob { hash, .. } => Some(hash.as_str()),
        }
    }

    pub fn decode_inline(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Inline { deflated_b64, raw_sha256, raw_size } => {
                anyhow::ensure!(*raw_size <= MAX_INLINE_DECOMPRESSED as u64, "inline payload exceeds size limit");
                let compressed = base64::engine::general_purpose::STANDARD
                    .decode(deflated_b64)
                    .context("invalid inline base64")?;
                let raw = decompress_bytes_with_limit(&compressed, *raw_size as usize)?;
                anyhow::ensure!(raw.len() as u64 == *raw_size, "inline payload size mismatch");
                anyhow::ensure!(sha256_hex(&raw) == *raw_sha256, "inline payload checksum mismatch");
                Ok(Some(raw))
            }
            Self::Blob { .. } => Ok(None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntityPayload {
    Folder {
        name: String,
        parent_id: Option<String>,
        meta: SyncMeta,
    },
    Document {
        name: String,
        parent_id: Option<String>,
        ext: String,
        meta: SyncMeta,
        content: ContentBody,
    },
    Asset {
        rel_path: String,
        blob_hash: String,
        raw_sha256: String,
        size: u64,
        mime: String,
    },
    TrashItem {
        orig_rel: String,
        folder: Option<String>,
        ext: String,
        deleted: u64,
        root: Option<String>,
        meta: SyncMeta,
        content: Option<ContentBody>,
    },
    HistorySnapshot {
        doc_id: String,
        name: String,
        trashed: bool,
        label: Option<String>,
        content: ContentBody,
    },
    Config {
        key: String,
        content: ContentBody,
    },
}

impl EntityPayload {
    pub fn kind(&self) -> EntityKind {
        match self {
            Self::Folder { .. } => EntityKind::Folder,
            Self::Document { .. } => EntityKind::Document,
            Self::Asset { .. } => EntityKind::Asset,
            Self::TrashItem { .. } => EntityKind::TrashItem,
            Self::HistorySnapshot { .. } => EntityKind::HistorySnapshot,
            Self::Config { .. } => EntityKind::Config,
        }
    }

    pub fn encode_bytes(&self) -> Result<Vec<u8>> {
        let json = serde_json::to_vec(self)?;
        Ok(compress_bytes(&json))
    }

    pub fn decode_bytes(bytes: &[u8]) -> Result<Self> {
        let json = decompress_bytes_with_limit(bytes, MAX_PAYLOAD_DECOMPRESSED)?;
        Ok(serde_json::from_slice(&json)?)
    }

    pub fn referenced_blobs(&self) -> Vec<String> {
        match self {
            Self::Folder { .. } => vec![],
            Self::Document { content, .. }
            | Self::HistorySnapshot { content, .. }
            | Self::Config { content, .. } => content.blob_hash().map(str::to_string).into_iter().collect(),
            Self::Asset { blob_hash, .. } => vec![blob_hash.clone()],
            Self::TrashItem { content, .. } => content.as_ref().and_then(ContentBody::blob_hash).map(str::to_string).into_iter().collect(),
        }
    }
}

pub fn compress_bytes(raw: &[u8]) -> Vec<u8> {
    compress_to_vec_zlib(raw, 6)
}

pub fn decompress_bytes(compressed: &[u8]) -> Result<Vec<u8>> {
    decompress_bytes_with_limit(compressed, MAX_BASE_DECOMPRESSED)
}

pub fn decompress_bytes_with_limit(compressed: &[u8], max_size: usize) -> Result<Vec<u8>> {
    decompress_to_vec_zlib_with_limit(compressed, max_size).map_err(|e| anyhow::anyhow!("zlib decompress failed: {e:?}"))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip_and_blob_refs() {
        let doc = EntityPayload::Document {
            name: "Notes.xml".into(),
            parent_id: Some("folder-1".into()),
            ext: "xml".into(),
            meta: SyncMeta { pinned: true, starred: 1700, width: "wide".into(), ..Default::default() },
            content: ContentBody::inline_from_bytes(b"<title>Hello</title>"),
        };
        let encoded = doc.encode_bytes().unwrap();
        let decoded = EntityPayload::decode_bytes(&encoded).unwrap();
        assert_eq!(doc, decoded);
        assert!(decoded.referenced_blobs().is_empty());

        let asset = EntityPayload::Asset {
            rel_path: ".assets/pic.png".into(),
            blob_hash: "abc123".into(),
            raw_sha256: "def456".into(),
            size: 42,
            mime: "image/png".into(),
        };
        assert_eq!(asset.referenced_blobs(), vec!["abc123"]);
    }

    #[test]
    fn decompress_enforces_limits_and_id_validation() {
        let big = vec![b'A'; 4096];
        let compressed = compress_bytes(&big);
        assert!(decompress_bytes_with_limit(&compressed, 1024).is_err());

        let tampered_size = ContentBody::Inline {
            deflated_b64: base64::engine::general_purpose::STANDARD.encode(&compressed),
            raw_sha256: sha256_hex(&big),
            raw_size: 100,
        };
        assert!(tampered_size.decode_inline().is_err());

        let too_large_declared = ContentBody::Inline {
            deflated_b64: String::new(),
            raw_sha256: String::new(),
            raw_size: (MAX_INLINE_DECOMPRESSED as u64) + 1,
        };
        assert!(too_large_declared.decode_inline().is_err());

        assert!(is_safe_entity_id("a1b2-c3_d4"));
        assert!(!is_safe_entity_id(""));
        assert!(!is_safe_entity_id("../escape"));
        assert!(!is_safe_entity_id("foo/bar"));
        assert!(!is_safe_entity_id("foo\\bar"));
        assert!(is_safe_doc_ext("xml"));
        assert!(is_safe_doc_ext("md"));
        assert!(is_safe_doc_ext("excalidraw"));
        assert!(!is_safe_doc_ext("txt"));
    }
}
