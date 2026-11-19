//! Three-way merge for documents, Markdown files, Excalidraw boards, metadata,
//! and JSON config files. Runs purely in `fdl_core` with no network or UI deps.

use crate::board::fractional_index::{order_by_fractional_index, sync_invalid_indices};
use crate::board::restore::ExportedAppState;
use crate::board::types::Element;
use crate::docxml;
use crate::markdown;
use crate::model::{Block, BlockId, BlockKind, Cover, Document, RichText, Table};
use crate::store::Meta;
use crate::whiteboard::Board;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictNote {
    pub location: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct MergeResult<T> {
    pub value: T,
    pub conflicts: Vec<ConflictNote>,
}

impl<T> MergeResult<T> {
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// Three-way merge of two DocxXML-model documents against their common `base`.
/// Non-overlapping block changes, table cells, nested children, and embedded
/// whiteboards merge cleanly; same-block divergent edits keep `local` and
/// report a `ConflictNote` so the caller can preserve `remote` as a conflict copy.
pub fn merge_documents(base: &Document, local: &Document, remote: &Document) -> MergeResult<Document> {
    let mut conflicts = Vec::new();
    let title = merge_value(&base.title, &local.title, &remote.title, "title", &mut conflicts);
    let icon = merge_value(&base.icon, &local.icon, &remote.icon, "icon", &mut conflicts);
    let cover = merge_cover(&base.cover, &local.cover, &remote.cover, &mut conflicts);
    let mut blocks = merge_block_list(&base.blocks, &local.blocks, &remote.blocks, &mut conflicts);
    if blocks.is_empty() {
        blocks.push(Block::text(""));
    }
    reassign_fresh_ids(&mut blocks);
    MergeResult {
        value: Document { title, icon, cover, blocks },
        conflicts,
    }
}

/// Three-way merge of Markdown files, keeping YAML front matter, untouched formatting, and CRLF intact.
pub fn merge_markdown(base_raw: &str, local_raw: &str, remote_raw: &str, title: &str) -> MergeResult<String> {
    if local_raw == remote_raw || remote_raw == base_raw {
        return MergeResult { value: local_raw.to_string(), conflicts: Vec::new() };
    }
    if local_raw == base_raw {
        return MergeResult { value: remote_raw.to_string(), conflicts: Vec::new() };
    }
    let mut conflicts = Vec::new();
    let (base_bom, base_body) = strip_bom(base_raw);
    let (local_bom, local_body) = strip_bom(local_raw);
    let (remote_bom, remote_body) = strip_bom(remote_raw);

    let (base_fm, base_rest) = markdown::front_matter(base_body);
    let (local_fm, local_rest) = markdown::front_matter(local_body);
    let (remote_fm, remote_rest) = markdown::front_matter(remote_body);

    let fm = merge_value(&base_fm.to_string(), &local_fm.to_string(), &remote_fm.to_string(), "front_matter", &mut conflicts);
    let bom = if local_bom == remote_bom || local_bom != base_bom { local_bom } else { remote_bom };

    if local_rest == remote_rest || remote_rest == base_rest {
        return MergeResult { value: format!("{bom}{fm}{local_rest}"), conflicts };
    }
    if local_rest == base_rest {
        return MergeResult { value: format!("{bom}{fm}{remote_rest}"), conflicts };
    }

    let base_doc = markdown::parse_file(base_raw, title, None);
    let local_doc = markdown::parse_file(local_raw, title, None);
    let remote_doc = markdown::parse_file(remote_raw, title, None);
    let mut merged = merge_documents(&base_doc, &local_doc, &remote_doc);
    conflicts.append(&mut merged.conflicts);

    let opts = markdown::Options { in_place: true, original: Some(local_raw), ..Default::default() };
    let out = markdown::write_file_with(&merged.value, &fm, &opts).text;
    MergeResult { value: format!("{bom}{out}"), conflicts }
}

/// Deterministic element-level three-way merge for Excalidraw scenes/files.
pub fn merge_boards(base: &Board, local: &Board, remote: &Board) -> MergeResult<Board> {
    let mut conflicts = Vec::new();
    let base_map: HashMap<&str, &Element> = base.elements.iter().map(|e| (e.id.as_str(), e)).collect();
    let local_map: HashMap<&str, &Element> = local.elements.iter().map(|e| (e.id.as_str(), e)).collect();
    let remote_map: HashMap<&str, &Element> = remote.elements.iter().map(|e| (e.id.as_str(), e)).collect();

    let mut all_ids: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for e in local.elements.iter().chain(remote.elements.iter()) {
        if seen.insert(e.id.as_str()) {
            all_ids.push(e.id.as_str());
        }
    }

    let mut merged_elements = Vec::new();
    for id in all_ids {
        let b = base_map.get(id).copied();
        let l = local_map.get(id).copied();
        let r = remote_map.get(id).copied();
        match (b, l, r) {
            (_, Some(le), Some(re)) => {
                if le == re {
                    merged_elements.push(le.clone());
                } else {
                    merged_elements.push(pick_winning_element(le, re));
                }
            }
            (Some(be), Some(le), None) => {
                // Remote deleted this element; keep only if local concurrently modified it.
                if element_modified(be, le) {
                    merged_elements.push(le.clone());
                }
            }
            (None, Some(le), None) => merged_elements.push(le.clone()),
            (Some(be), None, Some(re)) => {
                // Local deleted this element; keep only if remote concurrently modified it.
                if element_modified(be, re) {
                    merged_elements.push(re.clone());
                }
            }
            (None, None, Some(re)) => merged_elements.push(re.clone()),
            (_, None, None) => {}
        }
    }

    order_by_fractional_index(&mut merged_elements);
    sync_invalid_indices(&mut merged_elements);

    let mut files = BTreeMap::new();
    for (k, v) in &remote.files {
        files.insert(k.clone(), v.clone());
    }
    for (k, v) in &local.files {
        if let Some(rv) = files.get(k) {
            if rv != v && !base.files.get(k).is_some_and(|bv| bv == v) {
                files.insert(k.clone(), v.clone());
            }
        } else {
            files.insert(k.clone(), v.clone());
        }
    }

    let app = merge_app_state(&base.app, &local.app, &remote.app);
    let height = if (local.height - base.height).abs() < f32::EPSILON {
        remote.height
    } else if (remote.height - base.height).abs() < f32::EPSILON || (local.height - remote.height).abs() < f32::EPSILON {
        local.height
    } else {
        conflicts.push(ConflictNote { location: "board.height".into(), summary: "画板高度在多端同时修改".into() });
        local.height
    };

    MergeResult {
        value: Board { elements: merged_elements, files, app, height },
        conflicts,
    }
}

/// Field-level three-way merge for document/folder `Meta`.
pub fn merge_meta(base: &Meta, local: &Meta, remote: &Meta) -> Meta {
    let pick_u64 = |b: u64, l: u64, r: u64| -> u64 {
        if l == b { r } else if r == b || l == r { l } else if remote.modified > local.modified { r } else { l }
    };
    let pick_opt_u64 = |b: Option<u64>, l: Option<u64>, r: Option<u64>| -> Option<u64> {
        if l == b { r } else if r == b || l == r { l } else if remote.modified > local.modified { r } else { l }
    };
    let pick_bool = |b: bool, l: bool, r: bool| -> bool {
        if l == b { r } else if r == b || l == r { l } else if remote.modified > local.modified { r } else { l }
    };
    let pick_val = |b: &crate::store::PageWidth, l: &crate::store::PageWidth, r: &crate::store::PageWidth| {
        if l == b { *r } else if r == b || l == r { *l } else if remote.modified > local.modified { *r } else { *l }
    };
    let pick_info = |b: &crate::store::InfoRow, l: &crate::store::InfoRow, r: &crate::store::InfoRow| {
        if l == b { *r } else if r == b || l == r { *l } else if remote.modified > local.modified { *r } else { *l }
    };

    let created = match (local.created, remote.created) {
        (0, r) => r,
        (l, 0) => l,
        (l, r) => l.min(r),
    };
    let mut out = local.clone();
    out.created = created;
    out.modified = local.modified.max(remote.modified);
    out.pinned = pick_bool(base.pinned, local.pinned, remote.pinned);
    out.starred = pick_u64(base.starred, local.starred, remote.starred);
    out.order = pick_u64(base.order, local.order, remote.order);
    out.root_order = pick_opt_u64(base.root_order, local.root_order, remote.root_order);
    out.width = pick_val(&base.width, &local.width, &remote.width);
    out.catalogue_closed = pick_bool(base.catalogue_closed, local.catalogue_closed, remote.catalogue_closed);
    out.reading = pick_bool(base.reading, local.reading, remote.reading);
    out.info = pick_info(&base.info, &local.info, &remote.info);
    out.refs_hidden = pick_bool(base.refs_hidden, local.refs_hidden, remote.refs_hidden);
    out
}

/// Key-level three-way merge for JSON config objects (`preferences.json`,
/// `shortcuts.json`, `settings.json`), always preserving `preserve_local_keys`.
pub fn merge_json_object(base: &Value, local: &Value, remote: &Value, preserve_local_keys: &[&str]) -> Value {
    let (Some(lo), Some(ro)) = (local.as_object(), remote.as_object()) else {
        return local.clone();
    };
    let bo = base.as_object();
    let mut keys: Vec<&String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for k in lo.keys().chain(ro.keys()).chain(bo.into_iter().flat_map(|m| m.keys())) {
        if seen.insert(k.as_str()) {
            keys.push(k);
        }
    }
    let mut out = serde_json::Map::new();
    for k in keys {
        if preserve_local_keys.contains(&k.as_str()) {
            if let Some(v) = lo.get(k) {
                out.insert(k.clone(), v.clone());
            }
            continue;
        }
        let b = bo.and_then(|m| m.get(k));
        let l = lo.get(k);
        let r = ro.get(k);
        let picked = match (b, l, r) {
            (_, Some(lv), Some(rv)) if lv == rv => Some(lv.clone()),
            (Some(bv), Some(lv), Some(rv)) if lv == bv => Some(rv.clone()),
            (Some(bv), Some(lv), Some(_)) if rv_eq_opt(Some(bv), r) => Some(lv.clone()),
            (None, None, Some(rv)) => Some(rv.clone()),
            (None, Some(lv), None) => Some(lv.clone()),
            (Some(_), Some(_), None) if l == b => None,
            (Some(_), None, Some(_)) if r == b => None,
            (_, Some(lv), _) => Some(lv.clone()),
            (_, None, Some(rv)) => Some(rv.clone()),
            (_, None, None) => None,
        };
        if let Some(v) = picked {
            out.insert(k.clone(), v);
        }
    }
    Value::Object(out)
}

fn rv_eq_opt(a: Option<&Value>, b: Option<&Value>) -> bool {
    a == b
}

fn strip_bom(s: &str) -> (&str, &str) {
    match s.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", s),
    }
}

fn merge_value<T: Clone + PartialEq>(base: &T, local: &T, remote: &T, loc: &str, conflicts: &mut Vec<ConflictNote>) -> T {
    if local == base {
        remote.clone()
    } else if remote == base || local == remote {
        local.clone()
    } else {
        conflicts.push(ConflictNote { location: loc.into(), summary: format!("{loc} 在多端同时修改") });
        local.clone()
    }
}

fn merge_cover(base: &Option<Cover>, local: &Option<Cover>, remote: &Option<Cover>, conflicts: &mut Vec<ConflictNote>) -> Option<Cover> {
    if local == base {
        remote.clone()
    } else if remote == base || local == remote {
        local.clone()
    } else {
        conflicts.push(ConflictNote { location: "cover".into(), summary: "封面在多端同时修改".into() });
        local.clone()
    }
}

fn merge_app_state(base: &ExportedAppState, local: &ExportedAppState, remote: &ExportedAppState) -> ExportedAppState {
    let pick = |same_local: bool, same_remote: bool, l_val: &dyn Fn() -> ExportedAppState| {
        let _ = (same_local, same_remote, l_val);
    };
    let _ = pick;
    let pick_str = |b: &str, l: &str, r: &str| if l == b { r.to_string() } else { l.to_string() };
    let pick_f64 = |b: f64, l: f64, r: f64| if (l - b).abs() < 1e-9 { r } else { l };
    let pick_bool = |b: bool, l: bool, r: bool| if l == b { r } else { l };
    ExportedAppState {
        view_background_color: pick_str(&base.view_background_color, &local.view_background_color, &remote.view_background_color),
        grid_size: pick_f64(base.grid_size, local.grid_size, remote.grid_size),
        grid_step: pick_f64(base.grid_step, local.grid_step, remote.grid_step),
        grid_mode_enabled: pick_bool(base.grid_mode_enabled, local.grid_mode_enabled, remote.grid_mode_enabled),
    }
}

fn element_modified(base: &Element, cur: &Element) -> bool {
    base.version != cur.version || base.version_nonce != cur.version_nonce || base != cur
}

fn pick_winning_element(local: &Element, remote: &Element) -> Element {
    let key = |e: &Element| (e.version, e.version_nonce, e.updated);
    let kl = key(local);
    let kr = key(remote);
    if kr > kl {
        remote.clone()
    } else if kl > kr {
        local.clone()
    } else {
        // Exact tie: use deterministic JSON comparison so every device converges identically.
        let jl = crate::board::json::serialize_scene(std::slice::from_ref(local), &BTreeMap::new());
        let jr = crate::board::json::serialize_scene(std::slice::from_ref(remote), &BTreeMap::new());
        if jr > jl { remote.clone() } else { local.clone() }
    }
}

fn block_full_fp(b: &Block) -> String {
    docxml::serialize(&Document { title: RichText::default(), icon: None, cover: None, blocks: vec![b.clone()] })
}

fn block_shallow_fp(b: &Block) -> String {
    let mut shallow = b.clone();
    shallow.children.clear();
    shallow.folded = false;
    docxml::serialize(&Document { title: RichText::default(), icon: None, cover: None, blocks: vec![shallow] })
}

#[derive(Clone, Debug)]
struct SideEdit {
    b_start: usize,
    b_end: usize,
    s_start: usize,
    s_end: usize,
}

const MAX_LCS_CELLS: usize = 4_000_000;

/// Compute LCS matching indices `pairs: Vec<(base_idx, side_idx)>` over canonical fingerprints.
/// Trims common prefix/suffix in O(N) and aborts if the remaining matrix would exceed `MAX_LCS_CELLS`.
fn lcs_pairs(base: &[String], side: &[String]) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (base.len(), side.len());
    let mut prefix = 0usize;
    while prefix < n && prefix < m && base[prefix] == side[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < (n - prefix) && suffix < (m - prefix) && base[n - 1 - suffix] == side[m - 1 - suffix] {
        suffix += 1;
    }

    let n_mid = n - prefix - suffix;
    let m_mid = m - prefix - suffix;
    if n_mid.saturating_add(1).saturating_mul(m_mid.saturating_add(1)) > MAX_LCS_CELLS {
        return None;
    }

    let mut out = Vec::with_capacity(prefix + suffix + n_mid.min(m_mid));
    for i in 0..prefix {
        out.push((i, i));
    }

    if n_mid > 0 && m_mid > 0 {
        let b_mid = &base[prefix..prefix + n_mid];
        let s_mid = &side[prefix..prefix + m_mid];
        let w = m_mid + 1;
        let mut dp = vec![0u32; (n_mid + 1) * w];
        for i in (0..n_mid).rev() {
            for j in (0..m_mid).rev() {
                dp[i * w + j] = if b_mid[i] == s_mid[j] {
                    dp[(i + 1) * w + j + 1] + 1
                } else {
                    dp[(i + 1) * w + j].max(dp[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n_mid && j < m_mid {
            if b_mid[i] == s_mid[j] {
                out.push((prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * w + j] >= dp[i * w + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }

    for k in 0..suffix {
        out.push((n - suffix + k, m - suffix + k));
    }
    Some(out)
}

/// Convert LCS anchors to contiguous non-empty edits against `base`.
fn diff_edits(base_len: usize, side_len: usize, anchors: &[(usize, usize)]) -> Vec<SideEdit> {
    let mut edits = Vec::new();
    let (mut bi, mut si) = (0usize, 0usize);
    for &(ab, as_) in anchors {
        if ab > bi || as_ > si {
            edits.push(SideEdit { b_start: bi, b_end: ab, s_start: si, s_end: as_ });
        }
        bi = ab + 1;
        si = as_ + 1;
    }
    if bi < base_len || si < side_len {
        edits.push(SideEdit { b_start: bi, b_end: base_len, s_start: si, s_end: side_len });
    }
    edits
}

/// Check if two base intervals `[a0, a1)` and `[b0, b1)` overlap. Pure insertions
/// at an edit boundary merge cleanly without conflict unless they fall strictly
/// inside the other side's modified/deleted range, or both insert at the same spot.
fn edits_overlap(a: &SideEdit, b: &SideEdit) -> bool {
    if a.b_start == a.b_end && b.b_start == b.b_end {
        return a.b_start == b.b_start;
    }
    if a.b_start == a.b_end {
        return a.b_start > b.b_start && a.b_start < b.b_end;
    }
    if b.b_start == b.b_end {
        return b.b_start > a.b_start && b.b_start < a.b_end;
    }
    a.b_start < b.b_end && b.b_start < a.b_end
}

fn merge_block_list(base: &[Block], local: &[Block], remote: &[Block], conflicts: &mut Vec<ConflictNote>) -> Vec<Block> {
    let b_fps: Vec<String> = base.iter().map(block_full_fp).collect();
    let l_fps: Vec<String> = local.iter().map(block_full_fp).collect();
    let r_fps: Vec<String> = remote.iter().map(block_full_fp).collect();

    let (Some(l_anchors), Some(r_anchors)) = (lcs_pairs(&b_fps, &l_fps), lcs_pairs(&b_fps, &r_fps)) else {
        conflicts.push(ConflictNote {
            location: "blocks".into(),
            summary: "文档差异过大，已保留本地版本并创建冲突副本".into(),
        });
        return local.to_vec();
    };
    let l_edits = diff_edits(base.len(), local.len(), &l_anchors);
    let r_edits = diff_edits(base.len(), remote.len(), &r_anchors);

    let mut out = Vec::new();
    let mut b_cur = 0usize;
    let mut li = 0usize;
    let mut ri = 0usize;

    while li < l_edits.len() || ri < r_edits.len() {
        let pick_left = match (l_edits.get(li), r_edits.get(ri)) {
            (Some(le), Some(re)) => {
                if edits_overlap(le, re) {
                    // Build minimal overlapping cluster [b_lo..b_hi).
                    let mut b_lo = le.b_start.min(re.b_start);
                    let mut b_hi = le.b_end.max(re.b_end);
                    let l_start_idx = li;
                    let r_start_idx = ri;
                    li += 1;
                    ri += 1;
                    loop {
                        let mut grew = false;
                        while let Some(next_l) = l_edits.get(li) {
                            let cur = SideEdit { b_start: b_lo, b_end: b_hi, s_start: 0, s_end: 0 };
                            if edits_overlap(&cur, next_l) {
                                b_lo = b_lo.min(next_l.b_start);
                                b_hi = b_hi.max(next_l.b_end);
                                li += 1;
                                grew = true;
                            } else {
                                break;
                            }
                        }
                        while let Some(next_r) = r_edits.get(ri) {
                            let cur = SideEdit { b_start: b_lo, b_end: b_hi, s_start: 0, s_end: 0 };
                            if edits_overlap(&cur, next_r) {
                                b_lo = b_lo.min(next_r.b_start);
                                b_hi = b_hi.max(next_r.b_end);
                                ri += 1;
                                grew = true;
                            } else {
                                break;
                            }
                        }
                        if !grew {
                            break;
                        }
                    }

                    // Emit unchanged base blocks before cluster.
                    while b_cur < b_lo {
                        out.push(base[b_cur].clone());
                        b_cur += 1;
                    }

                    let l_slice = slice_for_base_cluster(&l_edits[l_start_idx..li], b_lo, b_hi, &l_anchors, local);
                    let r_slice = slice_for_base_cluster(&r_edits[r_start_idx..ri], b_lo, b_hi, &r_anchors, remote);
                    let l_fps_slice = slice_fp_for(&l_edits[l_start_idx..li], b_lo, b_hi, &l_anchors, &l_fps);
                    let r_fps_slice = slice_fp_for(&r_edits[r_start_idx..ri], b_lo, b_hi, &r_anchors, &r_fps);
                    let b_slice = &base[b_lo..b_hi];

                    resolve_hunk(b_slice, &l_slice, &r_slice, &l_fps_slice, &r_fps_slice, &mut out, conflicts);
                    b_cur = b_hi;
                    continue;
                }
                le.b_start < re.b_start || (le.b_start == re.b_start && le.b_end <= re.b_end)
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };

        if pick_left {
            let le = &l_edits[li];
            while b_cur < le.b_start {
                out.push(base[b_cur].clone());
                b_cur += 1;
            }
            out.extend_from_slice(&local[le.s_start..le.s_end]);
            b_cur = le.b_end;
            li += 1;
        } else {
            let re = &r_edits[ri];
            while b_cur < re.b_start {
                out.push(base[b_cur].clone());
                b_cur += 1;
            }
            out.extend_from_slice(&remote[re.s_start..re.s_end]);
            b_cur = re.b_end;
            ri += 1;
        }
    }

    while b_cur < base.len() {
        out.push(base[b_cur].clone());
        b_cur += 1;
    }
    out
}

fn side_range_for_base(edits: &[SideEdit], b_lo: usize, b_hi: usize, anchors: &[(usize, usize)]) -> (usize, usize) {
    let s_lo = edits.first().map(|e| e.s_start).unwrap_or(0);
    let s_hi = edits.last().map(|e| e.s_end).unwrap_or(s_lo);
    let start = if edits.first().is_some_and(|e| e.b_start == b_lo) {
        s_lo
    } else if let Some(&(_, si)) = anchors.iter().find(|&&(bi, _)| bi == b_lo) {
        si
    } else {
        s_lo
    };
    let end = if edits.last().is_some_and(|e| e.b_end == b_hi) {
        s_hi
    } else if b_hi > 0 && let Some(&(_, si)) = anchors.iter().find(|&&(bi, _)| bi == b_hi - 1) {
        si + 1
    } else {
        s_hi
    };
    (start.min(end), end)
}

fn slice_for_base_cluster(edits: &[SideEdit], b_lo: usize, b_hi: usize, anchors: &[(usize, usize)], side: &[Block]) -> Vec<Block> {
    let (s, e) = side_range_for_base(edits, b_lo, b_hi, anchors);
    side[s..e].to_vec()
}

fn slice_fp_for(edits: &[SideEdit], b_lo: usize, b_hi: usize, anchors: &[(usize, usize)], fps: &[String]) -> Vec<String> {
    let (s, e) = side_range_for_base(edits, b_lo, b_hi, anchors);
    fps[s..e].to_vec()
}

fn resolve_hunk(
    base: &[Block],
    local: &[Block],
    remote: &[Block],
    l_fps: &[String],
    r_fps: &[String],
    out: &mut Vec<Block>,
    conflicts: &mut Vec<ConflictNote>,
) {
    if l_fps == r_fps {
        out.extend_from_slice(local);
        return;
    }
    if base.is_empty() {
        // Both sides inserted at the same boundary: keep both cleanly.
        out.extend_from_slice(local);
        out.extend_from_slice(remote);
        return;
    }
    if base.len() == 1
        && local.len() == 1
        && remote.len() == 1
        && let Some(merged) = try_merge_single_block(&base[0], &local[0], &remote[0], conflicts)
    {
        out.push(merged);
        return;
    }
    conflicts.push(ConflictNote {
        location: "blocks".into(),
        summary: "同一内容块在多端被同时改写".into(),
    });
    out.extend_from_slice(local);
}

fn try_merge_single_block(base: &Block, local: &Block, remote: &Block, conflicts: &mut Vec<ConflictNote>) -> Option<Block> {
    if let (BlockKind::Board(bb), BlockKind::Board(lb), BlockKind::Board(rb)) = (&base.kind, &local.kind, &remote.kind) {
        let mut merged = merge_boards(bb, lb, rb);
        conflicts.append(&mut merged.conflicts);
        let mut out = local.clone();
        out.kind = BlockKind::Board(Arc::new(merged.value));
        return Some(out);
    }
    if let (BlockKind::Table(bt), BlockKind::Table(lt), BlockKind::Table(rt)) = (&base.kind, &local.kind, &remote.kind)
        && let Some(merged_table) = try_merge_tables(bt, lt, rt, conflicts)
    {
        let mut out = local.clone();
        out.kind = BlockKind::Table(merged_table);
        return Some(out);
    }
    if block_shallow_fp(base) == block_shallow_fp(local) && block_shallow_fp(base) == block_shallow_fp(remote) {
        let folded = if local.folded == base.folded { remote.folded } else { local.folded };
        let children = merge_block_list(&base.children, &local.children, &remote.children, conflicts);
        let mut out = local.clone();
        out.folded = folded;
        out.children = children;
        return Some(out);
    }
    None
}

fn try_merge_tables(base: &Table, local: &Table, remote: &Table, conflicts: &mut Vec<ConflictNote>) -> Option<Table> {
    if base.rows.len() != local.rows.len()
        || base.rows.len() != remote.rows.len()
        || base.ncols() != local.ncols()
        || base.ncols() != remote.ncols()
    {
        return None;
    }
    let header_rows = if local.header_rows == base.header_rows { remote.header_rows } else { local.header_rows };
    let header_col = if local.header_col == base.header_col { remote.header_col } else { local.header_col };
    let mut col_widths = local.col_widths.clone();
    for i in 0..col_widths.len() {
        if (local.col_widths[i] - base.col_widths[i]).abs() < 1e-3 {
            col_widths[i] = remote.col_widths[i];
        }
    }
    let mut rows = local.rows.clone();
    for r in 0..rows.len() {
        for c in 0..rows[r].len() {
            let bc = &base.rows[r][c];
            let lc = &local.rows[r][c];
            let rc = &remote.rows[r][c];
            if (bc.rowspan, bc.colspan) != (lc.rowspan, lc.colspan) || (bc.rowspan, bc.colspan) != (rc.rowspan, rc.colspan) {
                return None;
            }
            if bc.is_covered() {
                continue;
            }
            let bg = merge_value(&bc.bg, &lc.bg, &rc.bg, "table.cell.bg", conflicts);
            let valign = merge_value(&bc.valign, &lc.valign, &rc.valign, "table.cell.valign", conflicts);
            let mut blocks = merge_block_list(&bc.blocks, &lc.blocks, &rc.blocks, conflicts);
            if blocks.is_empty() {
                blocks.push(Block::text(""));
            }
            rows[r][c].bg = bg;
            rows[r][c].valign = valign;
            rows[r][c].blocks = blocks;
        }
    }
    let mut t = Table { col_widths, header_rows, header_col, rows };
    t.repair();
    Some(t)
}

fn reassign_fresh_ids(blocks: &mut [Block]) {
    for b in blocks {
        b.id = BlockId::new();
        reassign_fresh_ids(&mut b.children);
        if let BlockKind::Table(t) = &mut b.kind {
            for row in &mut t.rows {
                for cell in row {
                    reassign_fresh_ids(&mut cell.blocks);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::new_element::{new_element, ElementOpts};
    use crate::board::types::ElementType;
    use crate::model::TableCell;

    fn doc_with(title: &str, paras: &[&str]) -> Document {
        Document {
            title: RichText::plain(title),
            icon: None,
            cover: None,
            blocks: paras.iter().map(|s| Block::text(s)).collect(),
        }
    }

    #[test]
    fn non_overlapping_paragraphs_and_deletions_merge_cleanly() {
        let base = doc_with("Doc", &["A", "B", "C", "D"]);
        let local = doc_with("Doc", &["B", "C", "D"]); // deleted A
        let remote = doc_with("Doc", &["A", "B", "C2", "D", "E"]); // edited C, appended E
        let res = merge_documents(&base, &local, &remote);
        assert!(!res.has_conflicts(), "{:?}", res.conflicts);
        let texts: Vec<&str> = res.value.blocks.iter().map(|b| b.text.text.as_str()).collect();
        assert_eq!(texts, vec!["B", "C2", "D", "E"]);
        let ids: HashSet<BlockId> = res.value.blocks.iter().map(|b| b.id).collect();
        assert_eq!(ids.len(), res.value.blocks.len());

        // Editing block 0 while remote inserts immediately after block 0 merges cleanly.
        let b2 = doc_with("Doc", &["A", "B"]);
        let l2 = doc_with("Doc", &["A1", "B"]);
        let r2 = doc_with("Doc", &["A", "Ins", "B"]);
        let m2 = merge_documents(&b2, &l2, &r2);
        assert!(!m2.has_conflicts(), "{:?}", m2.conflicts);
        let t2: Vec<&str> = m2.value.blocks.iter().map(|b| b.text.text.as_str()).collect();
        assert_eq!(t2, vec!["A1", "Ins", "B"]);
    }

    #[test]
    fn same_block_concurrent_edit_reports_conflict_and_keeps_local() {
        let base = doc_with("Title", &["Hello", "World"]);
        let local = doc_with("Title", &["Hello from Mac", "World"]);
        let remote = doc_with("Title", &["Hello from Win", "World"]);
        let res = merge_documents(&base, &local, &remote);
        assert!(res.has_conflicts());
        assert_eq!(res.value.blocks[0].text.text, "Hello from Mac");
    }

    #[test]
    fn table_different_cells_merge_without_conflict() {
        let mk_table = |a: &str, b: &str| -> Document {
            let t = Table {
                col_widths: vec![100., 100.],
                header_rows: 1,
                header_col: false,
                rows: vec![vec![
                    TableCell { blocks: vec![Block::text(a)], ..TableCell::empty() },
                    TableCell { blocks: vec![Block::text(b)], ..TableCell::empty() },
                ]],
            };
            Document {
                title: RichText::plain("T"),
                blocks: vec![Block::new(BlockKind::Table(t), RichText::default())],
                ..Default::default()
            }
        };
        let base = mk_table("L0", "R0");
        let local = mk_table("L1", "R0");
        let remote = mk_table("L0", "R1");
        let res = merge_documents(&base, &local, &remote);
        assert!(!res.has_conflicts());
        let BlockKind::Table(t) = &res.value.blocks[0].kind else { panic!() };
        assert_eq!(t.rows[0][0].blocks[0].text.text, "L1");
        assert_eq!(t.rows[0][1].blocks[0].text.text, "R1");
    }

    #[test]
    fn nested_children_merge_cleanly() {
        let base = Document {
            title: RichText::plain("N"),
            blocks: vec![Block::text("Parent").with_children(vec![Block::text("c1"), Block::text("c2")])],
            ..Default::default()
        };
        let mut local = base.clone();
        local.blocks[0].children[0].text = RichText::plain("c1-local");
        let mut remote = base.clone();
        remote.blocks[0].children.push(Block::text("c3-remote"));
        let res = merge_documents(&base, &local, &remote);
        assert!(!res.has_conflicts());
        let kids: Vec<&str> = res.value.blocks[0].children.iter().map(|b| b.text.text.as_str()).collect();
        assert_eq!(kids, vec!["c1-local", "c2", "c3-remote"]);
    }

    #[test]
    fn markdown_merge_preserves_front_matter() {
        let base = "---\ntags: [a]\n---\nfirst\n\nsecond\n";
        let local = "---\ntags: [a]\n---\nfirst local\n\nsecond\n";
        let remote = "---\ntags: [a]\n---\nfirst\n\nsecond remote\n";
        let res = merge_markdown(base, local, remote, "Note");
        assert!(!res.has_conflicts());
        assert!(res.value.starts_with("---\ntags: [a]\n---\n"));
        assert!(res.value.contains("first local"));
        assert!(res.value.contains("second remote"));
    }

    #[test]
    fn board_element_merge_is_deterministic_and_respects_deletes() {
        let mk_rect = |id: &str, x: f64, ver: i64, nonce: i64, idx: &str| {
            let mut e = new_element(ElementType::Rectangle, ElementOpts { x, y: 0., width: 100., height: 80., ..Default::default() });
            e.id = id.into();
            e.version = ver;
            e.version_nonce = nonce;
            e.index = Some(idx.into());
            e
        };
        let e1 = mk_rect("e1", 0., 1, 10, "a0");
        let e2 = mk_rect("e2", 100., 1, 20, "a1");
        let base = Board { elements: vec![e1.clone(), e2.clone()], ..Default::default() };

        // Local deletes e2 and modifies e1 (ver 2); remote adds e3 and modifies e1 (ver 3).
        let e1_local = mk_rect("e1", 50., 2, 11, "a0");
        let e1_remote = mk_rect("e1", 80., 3, 12, "a0");
        let e3_remote = mk_rect("e3", 200., 1, 30, "a2");
        let local = Board { elements: vec![e1_local], ..Default::default() };
        let remote = Board { elements: vec![e1_remote.clone(), e2, e3_remote.clone()], ..Default::default() };

        let ab = merge_boards(&base, &local, &remote);
        let ba = merge_boards(&base, &remote, &local);
        assert!(!ab.has_conflicts());
        assert_eq!(ab.value, ba.value);
        assert_eq!(ab.value.elements.len(), 2);
        assert_eq!(ab.value.elements[0].id, "e1");
        assert_eq!(ab.value.elements[0].x, 80.);
        assert_eq!(ab.value.elements[1].id, "e3");
    }

    #[test]
    fn json_merge_protects_local_keys() {
        let base = serde_json::json!({"theme": "craft", "appearance": "system", "window_size": [800, 600]});
        let local = serde_json::json!({"theme": "craft", "appearance": "dark", "window_size": [1024, 768]});
        let remote = serde_json::json!({"theme": "notion", "appearance": "system", "window_size": [1400, 900]});
        let merged = merge_json_object(&base, &local, &remote, &["window_size"]);
        assert_eq!(merged["theme"], "notion");
        assert_eq!(merged["appearance"], "dark");
        assert_eq!(merged["window_size"], serde_json::json!([1024, 768]));
    }

    #[test]
    fn markdown_merge_preserves_untouched_syntax_and_crlf() {
        let base = "---\r\na: 1\r\n---\r\n* apple\r\n+ pear\r\n\r\n__bold__ keep\r\n\r\nsecond\r\n";
        let local = "---\r\na: 1\r\n---\r\n* apple\r\n+ pear\r\n\r\n__bold__ local\r\n\r\nsecond\r\n";
        let remote = "---\r\na: 1\r\n---\r\n* apple\r\n+ pear\r\n\r\n__bold__ keep\r\n\r\nsecond remote\r\n";
        let merged = merge_markdown(base, local, remote, "Note");
        assert!(!merged.has_conflicts(), "{:?}", merged.conflicts);
        assert!(merged.value.contains("\r\n"), "CRLF should stay CRLF: {:?}", merged.value);
        assert!(merged.value.contains("* apple\r\n+ pear"));
        assert!(merged.value.contains("__bold__ local"));
        assert!(merged.value.contains("second remote"));
    }

    #[test]
    fn large_doc_prefix_suffix_trim_and_lcs_budget_guard() {
        let mut base = doc_with("Big", &[]);
        base.blocks = (0..5000).map(|i| Block::text(&format!("para-{i}"))).collect();
        let mut local = base.clone();
        local.blocks[10].text = RichText::plain("local-10");
        let mut remote = base.clone();
        remote.blocks[4990].text = RichText::plain("remote-4990");
        let merged = merge_documents(&base, &local, &remote);
        assert!(!merged.has_conflicts());
        assert_eq!(merged.value.blocks[10].text.text, "local-10");
        assert_eq!(merged.value.blocks[4990].text.text, "remote-4990");

        let div_a: Vec<String> = (0..2200).map(|i| format!("a-{i}")).collect();
        let div_b: Vec<String> = (0..2200).map(|i| format!("b-{i}")).collect();
        assert!(lcs_pairs(&div_a, &div_b).is_none());
    }
}
