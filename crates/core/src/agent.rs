//! Agent / MCP document projection, block addressing, and lossless editing
//! primitives built on [`crate::editor::Core`]. Independent of GPUI.

use crate::docxml;
use crate::editor::core::Core;
use crate::markdown;
use crate::model::*;
use crate::paste;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1_smol::Sha1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl AgentError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), details: None }
    }
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AgentError {}

/// Compute a stable 16-hex revision token from the canonical serialized document.
pub fn document_revision(doc: &Document, is_markdown: bool, front_matter: &str) -> String {
    let body = if is_markdown {
        markdown::write_file(doc, front_matter, None)
    } else {
        docxml::serialize(doc)
    };
    let mut h = Sha1::new();
    h.update(if is_markdown { b"md:" } else { b"xml:" });
    h.update(doc.title.text.as_bytes());
    h.update(b"\0");
    h.update(body.as_bytes());
    let hex = h.digest().to_string();
    hex[..16].to_string()
}

/// Format an index path such as `[2, 1]` as `"2.1"`.
pub fn format_path(path: &[usize]) -> String {
    path.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(".")
}

/// 6-hex content fingerprint for a single block (kind + text + structural summary).
pub fn block_hash(b: &Block) -> String {
    let mut h = Sha1::new();
    h.update(block_kind_name(&b.kind).as_bytes());
    h.update(b":");
    match &b.kind {
        BlockKind::Heading { level, .. } => h.update(&[*level]),
        BlockKind::Todo { done } => h.update(&[u8::from(*done)]),
        BlockKind::Ordered { seq } => h.update(&seq.unwrap_or(0).to_le_bytes()),
        BlockKind::Code { lang, caption, .. } => {
            h.update(lang.as_bytes());
            if let Some(c) = caption { h.update(c.as_bytes()); }
        }
        BlockKind::Callout { emoji, .. } => h.update(emoji.as_bytes()),
        BlockKind::Image { src, caption, .. } => {
            h.update(src.as_bytes());
            if let Some(c) = caption { h.update(c.as_bytes()); }
        }
        BlockKind::Table(t) => {
            h.update(&(t.rows.len() as u32).to_le_bytes());
            h.update(&(t.ncols() as u32).to_le_bytes());
            for (_, _, cell) in t.cells() {
                for cb in &cell.blocks { h.update(cb.text.text.as_bytes()); h.update(b"\x1f"); }
            }
        }
        BlockKind::Board(bd) => {
            h.update(&(bd.elements.len() as u32).to_le_bytes());
            h.update(&(bd.height as u32).to_le_bytes());
        }
        BlockKind::File(f) => {
            h.update(f.name.as_bytes());
            h.update(f.src.as_bytes());
        }
        BlockKind::DataTable(tr) => h.update(tr.src.as_bytes()),
        BlockKind::Raw { tag, xml } => {
            h.update(tag.as_bytes());
            h.update(xml.as_bytes());
        }
        _ => {}
    }
    h.update(b"\0");
    h.update(b.text.text.as_bytes());
    h.update(&(b.children.len() as u32).to_le_bytes());
    let hex = h.digest().to_string();
    hex[..6].to_string()
}

pub fn make_block_ref(path: &[usize], b: &Block) -> String {
    format!("{}@{}", format_path(path), block_hash(b))
}

/// Find a block's index path from its runtime [`BlockId`] (top-level / child tree).
pub fn path_of_block_id(blocks: &[Block], id: BlockId) -> Option<Vec<usize>> {
    fn walk(blocks: &[Block], id: BlockId, cur: &mut Vec<usize>) -> Option<Vec<usize>> {
        for (i, b) in blocks.iter().enumerate() {
            cur.push(i);
            if b.id == id { return Some(cur.clone()); }
            if let Some(found) = walk(&b.children, id, cur) { return Some(found); }
            cur.pop();
        }
        None
    }
    walk(blocks, id, &mut Vec::new())
}

pub fn block_ref_for_id(doc: &Document, id: BlockId) -> Option<String> {
    let path = path_of_block_id(&doc.blocks, id)?;
    let b = block_at(&doc.blocks, &path)?;
    Some(make_block_ref(&path, b))
}

pub fn block_at<'a>(blocks: &'a [Block], path: &[usize]) -> Option<&'a Block> {
    let (&first, rest) = path.split_first()?;
    let b = blocks.get(first)?;
    if rest.is_empty() { Some(b) } else { block_at(&b.children, rest) }
}

/// Parse `"2.1@a1b2c3"` into `([2, 1], "a1b2c3")`.
pub fn parse_block_ref(s: &str) -> Result<(Vec<usize>, String), AgentError> {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    let (path_str, hash) = s.split_once('@').ok_or_else(|| {
        AgentError::new("INVALID_BLOCK_REF", format!("invalid block_ref {s:?}: expected '<path>@<hash6>'"))
    })?;
    if hash.len() != 6 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AgentError::new("INVALID_BLOCK_REF", format!("invalid block_ref hash in {s:?}")));
    }
    let mut path = Vec::new();
    for seg in path_str.split('.') {
        let idx: usize = seg.parse().map_err(|_| {
            AgentError::new("INVALID_BLOCK_REF", format!("invalid path segment {seg:?} in {s:?}"))
        })?;
        path.push(idx);
    }
    if path.is_empty() {
        return Err(AgentError::new("INVALID_BLOCK_REF", format!("empty path in {s:?}")));
    }
    Ok((path, hash.to_ascii_lowercase()))
}

/// Resolve and fingerprint-check a `block_ref` against `doc`.
pub fn resolve_block<'a>(doc: &'a Document, block_ref: &str) -> Result<(&'a Block, Vec<usize>), AgentError> {
    let (path, expected_hash) = parse_block_ref(block_ref)?;
    let b = block_at(&doc.blocks, &path).ok_or_else(|| {
        AgentError::new("BLOCK_NOT_FOUND", format!("block {block_ref:?} does not exist in document"))
    })?;
    let actual_hash = block_hash(b);
    if actual_hash != expected_hash {
        return Err(AgentError::new(
            "BLOCK_REF_MISMATCH",
            format!("block at path {} changed (expected hash {expected_hash}, actual {actual_hash}); re-read the document", format_path(&path)),
        ).with_details(json!({ "actual_block_ref": format!("{}@{actual_hash}", format_path(&path)) })));
    }
    Ok((b, path))
}

pub fn block_kind_name(k: &BlockKind) -> &'static str {
    match k {
        BlockKind::Text => "text",
        BlockKind::Heading { .. } => "heading",
        BlockKind::Bullet => "bullet",
        BlockKind::Ordered { .. } => "ordered",
        BlockKind::Todo { .. } => "todo",
        BlockKind::Code { .. } => "code",
        BlockKind::Quote => "quote",
        BlockKind::Callout { .. } => "callout",
        BlockKind::Divider => "divider",
        BlockKind::Image { .. } => "image",
        BlockKind::Table(_) => "table",
        BlockKind::Grid => "grid",
        BlockKind::Column { .. } => "column",
        BlockKind::SubPageList => "sub_page_list",
        BlockKind::Countdown(_) => "countdown",
        BlockKind::File(_) => "file",
        BlockKind::Board(_) => "board",
        BlockKind::DataTable(_) => "data_table",
        BlockKind::Raw { .. } => "raw",
    }
}

/// Rich/non-plain-text blocks (or blocks containing inline objects like
/// attachments, buttons, equations, reminders or mentions) guarded against
/// accidental replacement or deletion.
fn has_inline_objects(b: &Block) -> bool {
    b.text.runs.iter().any(|r| r.marks.inline.is_some())
}

fn is_protected_rich_block(b: &Block) -> bool {
    matches!(
        b.kind,
        BlockKind::Table(_)
            | BlockKind::Grid
            | BlockKind::Image { .. }
            | BlockKind::File(_)
            | BlockKind::Board(_)
            | BlockKind::DataTable(_)
            | BlockKind::Countdown(_)
            | BlockKind::SubPageList
            | BlockKind::Raw { .. }
    ) || has_inline_objects(b)
}

fn collect_protected(blocks: &[Block], open_comments: &[&str], path_prefix: &[usize], out: &mut Vec<Value>) {
    for (i, b) in blocks.iter().enumerate() {
        let mut p = path_prefix.to_vec();
        p.push(i);
        let has_open_comment = b.comment_ids.iter().any(|c| open_comments.contains(&c.as_str()))
            || b.text.runs.iter().any(|r| r.marks.comment_ids.iter().any(|c| open_comments.contains(&c.as_str())));
        let has_inline = has_inline_objects(b);
        if is_protected_rich_block(b) || has_open_comment {
            out.push(json!({
                "block_ref": make_block_ref(&p, b),
                "kind": block_kind_name(&b.kind),
                "has_open_comment": has_open_comment,
                "has_inline_object": has_inline,
            }));
        }
        collect_protected(&b.children, open_comments, &p, out);
    }
}

// ---- Document projection

#[derive(Clone, Debug)]
pub struct ReadDocOpts {
    pub start_block: usize,
    pub limit: usize,
    pub include_comments: bool,
}

impl Default for ReadDocOpts {
    fn default() -> Self {
        Self { start_block: 0, limit: 150, include_comments: true }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CommentSummary {
    pub id: String,
    pub quote: String,
    pub resolved: bool,
    pub replies_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadDocResult {
    pub title: String,
    pub revision: String,
    pub total_blocks: usize,
    pub start_block: usize,
    pub end_block: usize,
    pub has_more: bool,
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<CommentSummary>,
}

pub fn project_doc(doc: &Document, is_markdown: bool, front_matter: &str, opts: ReadDocOpts) -> ReadDocResult {
    let revision = document_revision(doc, is_markdown, front_matter);
    let total = doc.blocks.len();
    let start = opts.start_block.min(total);
    let limit = opts.limit.clamp(1, 500);
    let end = (start + limit).min(total);
    let open_ids: Vec<&str> = doc.comments.iter().filter(|t| !t.resolved).map(|t| t.id.as_str()).collect();

    let mut out = String::new();
    let mut path = Vec::new();
    let mut seq = 0u32;
    for b in &doc.blocks[..start] {
        match &b.kind {
            BlockKind::Ordered { seq: Some(n) } => seq = *n,
            BlockKind::Ordered { seq: None } => seq = if seq > 0 { seq + 1 } else { 1 },
            _ => seq = 0,
        }
    }
    for (idx, b) in doc.blocks[start..end].iter().enumerate() {
        let abs = start + idx;
        match &b.kind {
            BlockKind::Ordered { seq: Some(n) } => seq = *n,
            BlockKind::Ordered { seq: None } => seq = if seq > 0 { seq + 1 } else { 1 },
            _ => seq = 0,
        }
        path.clear();
        path.push(abs);
        if !out.is_empty() { out.push('\n'); }
        project_block(b, &mut path, 0, seq, &open_ids, &mut out);
    }

    let comments = if opts.include_comments {
        doc.comments.iter().map(|t| CommentSummary {
            id: t.id.clone(),
            quote: t.quote.clone(),
            resolved: t.resolved,
            replies_count: t.replies.len(),
            block_ref: doc.comment_anchor(&t.id).and_then(|bid| block_ref_for_id(doc, bid)),
        }).collect()
    } else {
        Vec::new()
    };

    ReadDocResult {
        title: doc.title.text.clone(),
        revision,
        total_blocks: total,
        start_block: start,
        end_block: end,
        has_more: end < total,
        content: out,
        comments,
    }
}

fn open_comment_count_on(b: &Block, open_ids: &[&str]) -> usize {
    let mut seen = Vec::new();
    for cid in &b.comment_ids {
        if open_ids.contains(&cid.as_str()) && !seen.contains(&cid.as_str()) { seen.push(cid.as_str()); }
    }
    for r in &b.text.runs {
        for cid in &r.marks.comment_ids {
            if open_ids.contains(&cid.as_str()) && !seen.contains(&cid.as_str()) { seen.push(cid.as_str()); }
        }
    }
    seen.len()
}

fn project_block(b: &Block, path: &mut Vec<usize>, depth: usize, seq: u32, open_ids: &[&str], out: &mut String) {
    let indent = "  ".repeat(depth);
    let bref = make_block_ref(path, b);
    let ccount = open_comment_count_on(b, open_ids);
    let csuffix = if ccount > 0 { format!(" <!-- comments: {ccount} open -->") } else { String::new() };

    match &b.kind {
        BlockKind::Text => {
            let line = markdown::inline_markdown(&b.text);
            out.push_str(&format!("{indent}[{bref}] {line}{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
        }
        BlockKind::Heading { level, .. } => {
            let hashes = "#".repeat((*level).clamp(1, 6) as usize);
            let line = markdown::inline_markdown(&b.text);
            out.push_str(&format!("{indent}[{bref}] {hashes} {line}{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
        }
        BlockKind::Bullet => {
            let line = markdown::inline_markdown(&b.text);
            out.push_str(&format!("{indent}[{bref}] - {line}{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
        }
        BlockKind::Ordered { .. } => {
            let n = seq.max(1);
            let line = markdown::inline_markdown(&b.text);
            out.push_str(&format!("{indent}[{bref}] {n}. {line}{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
        }
        BlockKind::Todo { done } => {
            let mark = if *done { "x" } else { " " };
            let line = markdown::inline_markdown(&b.text);
            out.push_str(&format!("{indent}[{bref}] - [{mark}] {line}{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
        }
        BlockKind::Code { lang, caption, .. } => {
            let cap = caption.as_deref().filter(|c| !c.is_empty()).map(|c| format!(" title=\"{}\"", escape_attr(c))).unwrap_or_default();
            out.push_str(&format!("{indent}[{bref}] ```{lang}{cap}{csuffix}\n"));
            for line in b.text.text.split('\n') {
                out.push_str(&format!("{indent}{line}\n"));
            }
            out.push_str(&format!("{indent}```"));
        }
        BlockKind::Divider => {
            out.push_str(&format!("{indent}[{bref}] ---{csuffix}"));
        }
        BlockKind::Quote => {
            out.push_str(&format!("{indent}[{bref}] <quote>{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
            out.push_str(&format!("\n{indent}</quote>"));
        }
        BlockKind::Callout { emoji, .. } => {
            out.push_str(&format!("{indent}[{bref}] <callout emoji=\"{}\">{csuffix}", escape_attr(emoji)));
            project_children(&b.children, path, depth + 1, open_ids, out);
            out.push_str(&format!("\n{indent}</callout>"));
        }
        BlockKind::Grid => {
            out.push_str(&format!("{indent}[{bref}] <grid columns=\"{}\">{csuffix}", b.children.len()));
            project_children(&b.children, path, depth + 1, open_ids, out);
            out.push_str(&format!("\n{indent}</grid>"));
        }
        BlockKind::Column { ratio } => {
            out.push_str(&format!("{indent}[{bref}] <column ratio=\"{ratio:.2}\">{csuffix}"));
            project_children(&b.children, path, depth + 1, open_ids, out);
            out.push_str(&format!("\n{indent}</column>"));
        }
        BlockKind::Image { src, caption, .. } => {
            let cap = caption.as_deref().unwrap_or("");
            out.push_str(&format!("{indent}[{bref}] <image src=\"{}\" caption=\"{}\"/>{csuffix}", escape_attr(src), escape_attr(cap)));
        }
        BlockKind::Table(t) => {
            let preview = table_preview(t);
            out.push_str(&format!("{indent}[{bref}] <table rows=\"{}\" cols=\"{}\" preview=\"{}\"/>{csuffix}", t.rows.len(), t.ncols(), escape_attr(&preview)));
        }
        BlockKind::Board(bd) => {
            out.push_str(&format!("{indent}[{bref}] <whiteboard elements=\"{}\" height=\"{}\"/>{csuffix}", bd.elements.len(), bd.height as i32));
        }
        BlockKind::File(f) => {
            out.push_str(&format!("{indent}[{bref}] <file name=\"{}\" src=\"{}\"/>{csuffix}", escape_attr(&f.name), escape_attr(&f.src)));
        }
        BlockKind::DataTable(tr) => {
            out.push_str(&format!("{indent}[{bref}] <datatable src=\"{}\"/>{csuffix}", escape_attr(&tr.src)));
        }
        BlockKind::Countdown(c) => {
            out.push_str(&format!("{indent}[{bref}] <countdown date=\"{}\" time=\"{}\"/>{csuffix}", escape_attr(&c.date), escape_attr(&c.time)));
        }
        BlockKind::SubPageList => {
            out.push_str(&format!("{indent}[{bref}] <sub-page-list/>{csuffix}"));
        }
        BlockKind::Raw { tag, .. } => {
            out.push_str(&format!("{indent}[{bref}] <raw tag=\"{}\"/>{csuffix}", escape_attr(tag)));
        }
    }
}

fn project_children(children: &[Block], path: &mut Vec<usize>, depth: usize, open_ids: &[&str], out: &mut String) {
    let mut seq = 0u32;
    for (i, c) in children.iter().enumerate() {
        match &c.kind {
            BlockKind::Ordered { seq: Some(n) } => seq = *n,
            BlockKind::Ordered { seq: None } => seq = if seq > 0 { seq + 1 } else { 1 },
            _ => seq = 0,
        }
        path.push(i);
        out.push('\n');
        project_block(c, path, depth, seq, open_ids, out);
        path.pop();
    }
}

fn table_preview(t: &Table) -> String {
    let Some(first_row) = t.rows.first() else { return String::new() };
    let cells: Vec<String> = first_row.iter().filter(|c| !c.is_covered()).map(|c| {
        let s: String = c.blocks.iter().map(|b| b.text.plain_text()).collect::<Vec<_>>().join(" ");
        let trimmed = s.trim();
        if trimmed.chars().count() > 24 {
            format!("{}…", trimmed.chars().take(24).collect::<String>())
        } else {
            trimmed.to_string()
        }
    }).collect();
    cells.join(" | ")
}

fn escape_attr(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

/// Inspect one block in detail (structured JSON).
pub fn inspect_block(doc: &Document, block_ref: &str) -> Result<Value, AgentError> {
    let (b, path) = resolve_block(doc, block_ref)?;
    let mut obj = serde_json::Map::new();
    obj.insert("block_ref".into(), json!(make_block_ref(&path, b)));
    obj.insert("path".into(), json!(format_path(&path)));
    obj.insert("kind".into(), json!(block_kind_name(&b.kind)));
    obj.insert("children_count".into(), json!(b.children.len()));
    if b.kind.has_text() {
        obj.insert("text".into(), json!(b.text.plain_text()));
        obj.insert("raw_text".into(), json!(b.text.text));
        obj.insert("markdown".into(), json!(markdown::inline_markdown(&b.text)));
    }
    match &b.kind {
        BlockKind::Heading { level, .. } => { obj.insert("level".into(), json!(level)); }
        BlockKind::Todo { done } => { obj.insert("done".into(), json!(done)); }
        BlockKind::Code { lang, caption, .. } => {
            obj.insert("lang".into(), json!(lang));
            if let Some(c) = caption { obj.insert("caption".into(), json!(c)); }
        }
        BlockKind::Callout { emoji, .. } => { obj.insert("emoji".into(), json!(emoji)); }
        BlockKind::Image { src, caption, width, height, .. } => {
            obj.insert("src".into(), json!(src));
            obj.insert("caption".into(), json!(caption));
            obj.insert("width".into(), json!(width));
            obj.insert("height".into(), json!(height));
        }
        BlockKind::Table(t) => {
            let rows: Vec<Vec<Option<String>>> = t.rows.iter().map(|r| {
                r.iter().map(|c| {
                    if c.is_covered() {
                        None
                    } else {
                        Some(c.blocks.iter().map(|cb| markdown::inline_markdown(&cb.text)).collect::<Vec<_>>().join("\n"))
                    }
                }).collect()
            }).collect();
            obj.insert("table".into(), json!({
                "rows_count": t.rows.len(),
                "cols_count": t.ncols(),
                "header_rows": t.header_rows,
                "rows": rows,
            }));
        }
        BlockKind::Board(bd) => {
            obj.insert("board".into(), json!({
                "elements_count": bd.elements.len(),
                "height": bd.height,
                "searchable_text": bd.plain_text(),
            }));
        }
        BlockKind::File(f) => {
            obj.insert("file".into(), json!({ "name": f.name, "src": f.src, "src_kind": f.src_kind }));
        }
        BlockKind::DataTable(tr) => { obj.insert("src".into(), json!(tr.src)); }
        BlockKind::Raw { tag, .. } => { obj.insert("tag".into(), json!(tag)); }
        _ => {}
    }
    if !b.children.is_empty() {
        let child_refs: Vec<String> = b.children.iter().enumerate().map(|(i, c)| {
            let mut cp = path.clone();
            cp.push(i);
            make_block_ref(&cp, c)
        }).collect();
        obj.insert("children".into(), json!(child_refs));
    }
    Ok(Value::Object(obj))
}

// ---- Mutations

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InsertPosition {
    Before,
    After,
    Append,
    Prepend,
}

/// Convert agent-supplied Markdown into sanitized blocks, blocking unsafe local
/// file traversal in image references.
pub fn parse_agent_markdown(md: &str) -> Result<Vec<Block>, AgentError> {
    if md.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut blocks = paste::markdown_to_blocks(md, paste::Off { file: true, alt: true, ..Default::default() });
    sanitize_external_links(&mut blocks)?;
    Ok(blocks)
}

fn is_safe_agent_image_src(src: &str) -> bool {
    let s = src.trim();
    if s.is_empty() { return false; }
    if s.starts_with("http://") || s.starts_with("https://") { return true; }
    if s.starts_with('/') || s.starts_with('\\') || s.contains(':') { return false; }
    let norm = s.replace('\\', "/");
    if norm.split('/').any(|seg| seg == "..") { return false; }
    norm.starts_with(".assets/") || norm.starts_with("assets/")
}

fn sanitize_external_links(blocks: &mut [Block]) -> Result<(), AgentError> {
    for b in blocks {
        if let BlockKind::Image { src, .. } = &b.kind
            && !is_safe_agent_image_src(src)
        {
            return Err(AgentError::new(
                "UNSAFE_IMAGE_PATH",
                format!("image source {src:?} is not allowed; only http(s) URLs or library-relative .assets/ paths are permitted"),
            ));
        }
        sanitize_external_links(&mut b.children)?;
    }
    Ok(())
}

pub fn check_revision(doc: &Document, is_markdown: bool, front_matter: &str, expected: &str) -> Result<String, AgentError> {
    let actual = document_revision(doc, is_markdown, front_matter);
    if actual != expected.trim() {
        return Err(AgentError::new(
            "REVISION_CONFLICT",
            format!("document revision mismatch (expected {}, actual {actual}); re-read the document before editing", expected.trim()),
        ).with_details(json!({ "current_revision": actual })));
    }
    Ok(actual)
}

fn markdown_parent_can_hold_children(k: &BlockKind) -> bool {
    matches!(k, BlockKind::Bullet | BlockKind::Ordered { .. } | BlockKind::Todo { .. } | BlockKind::Quote | BlockKind::Callout { .. })
}

fn check_markdown_nesting(doc: &Document, parent_path: &[usize], is_markdown: bool) -> Result<(), AgentError> {
    if !is_markdown || parent_path.is_empty() {
        return Ok(());
    }
    let parent = block_at(&doc.blocks, parent_path)
        .ok_or_else(|| AgentError::new("BLOCK_NOT_FOUND", "parent block not found"))?;
    if !markdown_parent_can_hold_children(&parent.kind) {
        return Err(AgentError::new(
            "MARKDOWN_NESTING_UNSUPPORTED",
            format!(
                "cannot nest child blocks inside a {} block in a Markdown document because Markdown flattens them on save; insert before or after the block instead",
                block_kind_name(&parent.kind)
            ),
        ));
    }
    Ok(())
}

pub fn apply_insert_blocks(
    core: &mut Core,
    position: InsertPosition,
    anchor_block_ref: Option<&str>,
    markdown_text: &str,
) -> Result<Vec<String>, AgentError> {
    apply_insert_blocks_with_format(core, position, anchor_block_ref, markdown_text, false)
}

pub fn apply_insert_blocks_with_format(
    core: &mut Core,
    position: InsertPosition,
    anchor_block_ref: Option<&str>,
    markdown_text: &str,
    is_markdown: bool,
) -> Result<Vec<String>, AgentError> {
    let blocks = parse_agent_markdown(markdown_text)?;
    if blocks.is_empty() {
        return Err(AgentError::new("EMPTY_CONTENT", "markdown produced no blocks to insert"));
    }
    let (parent_path, index) = match position {
        InsertPosition::Append => match anchor_block_ref {
            None => (Vec::new(), core.doc.blocks.len()),
            Some(r) => {
                let (b, p) = resolve_block(&core.doc, r)?;
                (p, b.children.len())
            }
        },
        InsertPosition::Prepend => match anchor_block_ref {
            None => (Vec::new(), 0),
            Some(r) => {
                let (_, p) = resolve_block(&core.doc, r)?;
                (p, 0)
            }
        },
        InsertPosition::Before | InsertPosition::After => {
            let r = anchor_block_ref.ok_or_else(|| {
                AgentError::new("MISSING_ANCHOR", "anchor_block_ref is required for position 'before' or 'after'")
            })?;
            let (_, mut p) = resolve_block(&core.doc, r)?;
            let idx = p.pop().unwrap();
            let target_idx = if position == InsertPosition::Before { idx } else { idx + 1 };
            (p, target_idx)
        }
    };
    check_markdown_nesting(&core.doc, &parent_path, is_markdown)?;
    let ids = core.agent_insert_blocks(&parent_path, index, blocks)
        .map_err(|e| AgentError::new("EDIT_FAILED", e))?;
    Ok(ids.into_iter().filter_map(|id| block_ref_for_id(&core.doc, id)).collect())
}

fn resolve_sibling_range(doc: &Document, start_ref: &str, end_ref: Option<&str>) -> Result<(Vec<usize>, usize, usize), AgentError> {
    let (_, mut sp) = resolve_block(doc, start_ref)?;
    let start_idx = sp.pop().unwrap();
    let end_idx = match end_ref {
        None => start_idx,
        Some(er) => {
            let (_, mut ep) = resolve_block(doc, er)?;
            let ei = ep.pop().unwrap();
            if ep != sp {
                return Err(AgentError::new("RANGE_PARENT_MISMATCH", "start_block_ref and end_block_ref must share the same parent"));
            }
            if ei < start_idx {
                return Err(AgentError::new("INVALID_RANGE", "end_block_ref must not precede start_block_ref"));
            }
            ei
        }
    };
    Ok((sp, start_idx, end_idx))
}

fn check_rich_loss(doc: &Document, parent_path: &[usize], start: usize, end: usize, allow_rich_loss: bool) -> Result<(), AgentError> {
    if allow_rich_loss { return Ok(()); }
    let siblings = if parent_path.is_empty() {
        &doc.blocks
    } else {
        &block_at(&doc.blocks, parent_path)
            .ok_or_else(|| AgentError::new("BLOCK_NOT_FOUND", "parent block not found"))?
            .children
    };
    let open_ids: Vec<&str> = doc.comments.iter().filter(|t| !t.resolved).map(|t| t.id.as_str()).collect();
    let mut protected = Vec::new();
    for (offset, b) in siblings[start..=end].iter().enumerate() {
        let mut p = parent_path.to_vec();
        p.push(start + offset);
        let has_open_comment = b.comment_ids.iter().any(|c| open_ids.contains(&c.as_str()))
            || b.text.runs.iter().any(|r| r.marks.comment_ids.iter().any(|c| open_ids.contains(&c.as_str())));
        let has_inline = has_inline_objects(b);
        if is_protected_rich_block(b) || has_open_comment {
            protected.push(json!({
                "block_ref": make_block_ref(&p, b),
                "kind": block_kind_name(&b.kind),
                "has_open_comment": has_open_comment,
                "has_inline_object": has_inline,
            }));
        }
        collect_protected(&b.children, &open_ids, &p, &mut protected);
    }
    if !protected.is_empty() {
        return Err(AgentError::new(
            "RICH_BLOCK_PROTECTED",
            "target range contains protected rich blocks (table/board/image/grid/etc.) or open comments; pass allow_rich_loss=true to confirm",
        ).with_details(json!({ "protected_blocks": protected })));
    }
    Ok(())
}

pub fn apply_replace_blocks(
    core: &mut Core,
    start_ref: &str,
    end_ref: Option<&str>,
    markdown_text: &str,
    allow_rich_loss: bool,
) -> Result<Vec<String>, AgentError> {
    let (parent_path, start, end) = resolve_sibling_range(&core.doc, start_ref, end_ref)?;
    check_rich_loss(&core.doc, &parent_path, start, end, allow_rich_loss)?;
    let blocks = parse_agent_markdown(markdown_text)?;
    let ids = core.agent_replace_blocks(&parent_path, start, end, blocks)
        .map_err(|e| AgentError::new("EDIT_FAILED", e))?;
    Ok(ids.into_iter().filter_map(|id| block_ref_for_id(&core.doc, id)).collect())
}

pub fn apply_delete_blocks(
    core: &mut Core,
    start_ref: &str,
    end_ref: Option<&str>,
    allow_rich_loss: bool,
) -> Result<(), AgentError> {
    let (parent_path, start, end) = resolve_sibling_range(&core.doc, start_ref, end_ref)?;
    check_rich_loss(&core.doc, &parent_path, start, end, allow_rich_loss)?;
    core.agent_delete_blocks(&parent_path, start, end)
        .map_err(|e| AgentError::new("EDIT_FAILED", e))
}

pub fn apply_edit_block_text(
    core: &mut Core,
    block_ref: &str,
    old_text: &str,
    new_text: &str,
    occurrence: Option<usize>,
) -> Result<String, AgentError> {
    let (_, path) = resolve_block(&core.doc, block_ref)?;
    let id = core.agent_edit_block_text(&path, old_text, new_text, occurrence)
        .map_err(|e| AgentError::new("EDIT_FAILED", e))?;
    Ok(block_ref_for_id(&core.doc, id).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> Document {
        docxml::parse(concat!(
            "<title>测试文档</title>",
            "<h1>第一章</h1>",
            "<p>这里有<b>加粗重点</b>和普通文字</p>",
            "<table><colgroup><col width=\"100\"/><col width=\"100\"/></colgroup><tbody><tr><td><p>A</p></td><td><p>B</p></td></tr></tbody></table>",
            "<whiteboard type=\"excalidraw\" height=\"240\">{\"elements\":[],\"files\":{}}</whiteboard>",
        )).unwrap()
    }

    #[test]
    fn projection_and_revision_are_stable() {
        let doc = sample_doc();
        let r1 = project_doc(&doc, false, "", ReadDocOpts::default());
        let r2 = project_doc(&doc, false, "", ReadDocOpts::default());
        assert_eq!(r1.revision, r2.revision);
        assert_eq!(r1.total_blocks, 4);
        assert!(r1.content.contains("# 第一章"));
        assert!(r1.content.contains("<table rows=\"1\" cols=\"2\""));
        assert!(r1.content.contains("<whiteboard elements=\"0\" height=\"240\"/>"));
    }

    #[test]
    fn edit_block_text_preserves_marks_and_comments_and_is_undoable() {
        let mut core = Core::new(sample_doc());
        let p_id = core.doc.blocks[1].id;
        // Anchor a comment on "加粗重点".
        let start = core.doc.blocks[1].text.text.find("加粗重点").unwrap();
        let end = start + "加粗重点".len();
        let cid = core.add_comment(
            crate::editor::core::CommentTarget::Inline {
                start: crate::editor::core::Pos::new(p_id, start),
                end: crate::editor::core::Pos::new(p_id, end),
            },
            "User".into(),
            "确认措辞".into(),
            100,
        ).unwrap();

        let bref = block_ref_for_id(&core.doc, p_id).unwrap();
        let updated_ref = apply_edit_block_text(&mut core, &bref, "加粗重点", "核心要点", None).unwrap();
        assert_ne!(bref, updated_ref);
        assert_eq!(core.doc.blocks[1].text.text, "这里有核心要点和普通文字");
        let mid = core.doc.blocks[1].text.text.find("核心要点").unwrap() + 1;
        let marks = core.doc.blocks[1].text.marks_at(mid);
        assert!(marks.bold, "bold mark must survive edit_block_text");
        assert!(marks.comment_ids.contains(&cid), "comment anchor must survive edit_block_text");

        // Undo reverts cleanly.
        assert!(core.undo());
        assert_eq!(core.doc.blocks[1].text.text, "这里有加粗重点和普通文字");
    }

    #[test]
    fn rich_block_protection_blocks_accidental_loss() {
        let mut core = Core::new(sample_doc());
        let table_ref = block_ref_for_id(&core.doc, core.doc.blocks[2].id).unwrap();
        let err = apply_delete_blocks(&mut core, &table_ref, None, false).unwrap_err();
        assert_eq!(err.code, "RICH_BLOCK_PROTECTED");

        // Explicitly confirming succeeds and can be undone.
        apply_delete_blocks(&mut core, &table_ref, None, true).unwrap();
        assert_eq!(core.doc.blocks.len(), 3);
        assert!(core.undo());
        assert_eq!(core.doc.blocks.len(), 4);
    }

    #[test]
    fn insert_and_replace_blocks_round_trip() {
        let mut core = Core::new(sample_doc());
        let h1_ref = block_ref_for_id(&core.doc, core.doc.blocks[0].id).unwrap();
        let inserted = apply_insert_blocks(&mut core, InsertPosition::After, Some(&h1_ref), "- 条目一\n- 条目二").unwrap();
        assert_eq!(inserted.len(), 2);
        assert_eq!(core.doc.blocks.len(), 6);
        let xml = docxml::serialize(&core.doc);
        let reparsed = docxml::parse(&xml).unwrap();
        assert_eq!(docxml::serialize(&reparsed), xml);
    }

    #[test]
    fn unsafe_image_path_is_rejected() {
        let err = parse_agent_markdown("![secret](../../etc/passwd)").unwrap_err();
        assert_eq!(err.code, "UNSAFE_IMAGE_PATH");
        assert!(parse_agent_markdown("![ok](.assets/pic.png)").is_ok());
        assert!(parse_agent_markdown("![web](https://example.com/a.png)").is_ok());
    }

    #[test]
    fn all_blocks_fixture_projects_without_panic() {
        let xml = include_str!("../tests/fixtures/all-blocks.xml");
        let doc = docxml::parse(xml).unwrap();
        let res = project_doc(&doc, false, "", ReadDocOpts::default());
        assert!(!res.content.is_empty());
        assert_eq!(res.total_blocks, doc.blocks.len());
    }

    #[test]
    fn inline_objects_are_protected_from_unconfirmed_replacement() {
        let doc = docxml::parse("<title>T</title><p>公式 <latex>E=mc^2</latex> 结尾</p>").unwrap();
        let mut core = Core::new(doc);
        let p_ref = block_ref_for_id(&core.doc, core.doc.blocks[0].id).unwrap();
        let err = apply_replace_blocks(&mut core, &p_ref, None, "纯文本", false).unwrap_err();
        assert_eq!(err.code, "RICH_BLOCK_PROTECTED");
        assert_eq!(err.details.unwrap()["protected_blocks"][0]["has_inline_object"], json!(true));
    }

    #[test]
    fn comment_anchor_survives_replacement_starting_in_unannotated_text() {
        let doc = docxml::parse("<title>T</title><p>前缀 目标词 后缀</p>").unwrap();
        let mut core = Core::new(doc);
        let p_id = core.doc.blocks[0].id;
        let s = core.doc.blocks[0].text.text.find("目标词").unwrap();
        let e = s + "目标词".len();
        let cid = core.add_comment(
            crate::editor::core::CommentTarget::Inline {
                start: crate::editor::core::Pos::new(p_id, s),
                end: crate::editor::core::Pos::new(p_id, e),
            },
            "User".into(),
            "注意".into(),
            10,
        ).unwrap();
        let p_ref = block_ref_for_id(&core.doc, p_id).unwrap();
        // Replacement starts in unannotated "前缀 " and consumes the commented "目标词".
        apply_edit_block_text(&mut core, &p_ref, "前缀 目标词", "新前缀 新目标", None).unwrap();
        assert!(core.doc.blocks[0].text.runs.iter().any(|r| r.marks.comment_ids.contains(&cid)));
    }

    #[test]
    fn markdown_rejects_nesting_under_heading() {
        let doc = markdown::parse_file("# 标题\n\n正文\n", "Note", None);
        let mut core = Core::new(doc);
        let h_ref = block_ref_for_id(&core.doc, core.doc.blocks[0].id).unwrap();
        let err = apply_insert_blocks_with_format(&mut core, InsertPosition::Append, Some(&h_ref), "子段落", true).unwrap_err();
        assert_eq!(err.code, "MARKDOWN_NESTING_UNSUPPORTED");
    }

    #[test]
    fn paginated_read_preserves_ordered_list_numbering() {
        let doc = docxml::parse("<title>T</title><ol><li>一</li><li>二</li><li>三</li></ol>").unwrap();
        let page2 = project_doc(&doc, false, "", ReadDocOpts { start_block: 1, limit: 2, include_comments: false });
        assert!(page2.content.contains("] 2. 二"), "{}", page2.content);
        assert!(page2.content.contains("] 3. 三"), "{}", page2.content);
    }

    #[test]
    fn save_if_revision_matches_rejects_external_markdown_edit() {
        let dir = std::env::temp_dir().join(format!("quiet-rev-cond-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::store::Store::open_at(dir.clone()).unwrap();
        let doc = markdown::parse_file("original body\n", "Note", None);
        let id = store.create_markdown(&doc, "").unwrap();
        let loaded = store.load(&id).unwrap();
        let rev = document_revision(&loaded, true, "");

        // External editor modifies the Markdown file directly on disk.
        let path = store.file_path(&id).unwrap();
        std::fs::write(&path, "external newer body\n").unwrap();

        let mut updated = loaded.clone();
        updated.blocks[0].text = RichText::plain("mcp stale edit");
        let err = store.save_if_revision_matches(&id, &updated, &rev).unwrap_err();
        assert!(err.to_string().starts_with("REVISION_CONFLICT:"), "{err}");
        // On-disk external edit must remain intact!
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external newer body\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
