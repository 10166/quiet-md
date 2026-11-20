//! Wide-screen floating comment cards aligned along the Y-axis with their
//! anchored paragraphs / text spans in the right page margin.

use super::catalogue::{COMMENT_CARD_W, COMMENT_RAIL_GAP, page_layout};
use super::core::{CommentTarget, Pos};
use super::overlays::Probe;
use super::render::flattens;
use super::view::Editor;
use crate::editor_theme::{self as theme, Theme};
use crate::input::{TextField, TextFieldEvent};
use crate::model::{Block, BlockId, BlockKind};
use crate::ui::{TipExt, format_time, icon, primary_button, secondary_button};
use gpui::{prelude::FluentBuilder, *};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const DRAFT_KEY: &str = "__draft__";
const CARD_GAP: f32 = 10.;
const CARD_MIN_Y: f32 = 12.;
const DEFAULT_CARD_H: f32 = 96.;
const ACTIVE_POP_LEFT: f32 = 8.;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleAnchor {
    pub thread_id: String,
    pub block: BlockId,
    pub inline_offset: Option<usize>,
}

pub struct RailDraft {
    pub target: CommentTarget,
    pub quote: String,
    pub anchor_gen: u64,
    pub stale: bool,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

pub struct RailReply {
    pub thread_id: String,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

pub struct RailEdit {
    pub thread_id: String,
    pub reply_id: String,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

#[derive(Default)]
pub struct CommentRail {
    /// True while the full right-side comment drawer is open in Workspace.
    pub drawer_open: bool,
    pub draft: Option<RailDraft>,
    pub replying: Option<RailReply>,
    pub editing: Option<RailEdit>,
    /// Anchors seen in the virtualized list rows painted this frame.
    pub visible_anchors: RefCell<Vec<VisibleAnchor>>,
    /// Anchors the rail rendered from on the last build (for stale check).
    last_anchors: Vec<VisibleAnchor>,
    /// Actual painted heights of floating cards (key -> px).
    heights: Rc<RefCell<HashMap<String, f32>>>,
    /// Heights snapshot used when the rail was built this frame.
    last_heights: HashMap<String, f32>,
}

/// 1D collision-avoidance layout for margin cards.
///
/// Each input is `(key, anchor_y, height)`. When `active` matches an item,
/// that card pins to its `anchor_y` clamped inside `[min_y, max_bottom - h]`
/// and pushes colliding cards above it upward and below it downward.
pub(crate) fn layout_cards(items: &[(String, f32, f32)], active: Option<&str>, gap: f32, min_y: f32, max_bottom: f32) -> Vec<(String, f32)> {
    if items.is_empty() {
        return vec![];
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| items[a].1.total_cmp(&items[b].1).then_with(|| a.cmp(&b)));
    let n = order.len();
    let mut ys = vec![0.0_f32; n];
    let active_ix = active.and_then(|aid| order.iter().position(|&ix| items[ix].0 == aid));
    match active_ix {
        Some(ai) => {
            let max_top = (max_bottom - items[order[ai]].2).max(min_y);
            ys[ai] = items[order[ai]].1.clamp(min_y, max_top);
            for i in (0..ai).rev() {
                let h = items[order[i]].2;
                ys[i] = items[order[i]].1.min(ys[i + 1] - gap - h);
            }
            for i in ai + 1..n {
                let prev_h = items[order[i - 1]].2;
                ys[i] = items[order[i]].1.max(ys[i - 1] + prev_h + gap);
            }
        }
        None => {
            let mut cursor = min_y;
            for i in 0..n {
                let y = items[order[i]].1.max(cursor);
                ys[i] = y;
                cursor = y + items[order[i]].2 + gap;
            }
        }
    }
    order.into_iter().zip(ys).map(|(ix, y)| (items[ix].0.clone(), y)).collect()
}

fn make_rail_field(placeholder: &'static str, initial: &str, cx: &mut Context<Editor>) -> Entity<TextField> {
    cx.new(|cx| {
        let mut f = TextField::new(placeholder, cx);
        f.wrap = true;
        f.multiline = true;
        f.submit_on_enter = true;
        f.size = 13.;
        f.line_height = 19.;
        if !initial.is_empty() {
            f.set_text(initial, cx);
        }
        f
    })
}

fn record_card_height(key: String, map: Rc<RefCell<HashMap<String, f32>>>) -> impl IntoElement {
    canvas(
        move |b, _, _| {
            map.borrow_mut().insert(key.clone(), f32::from(b.size.height));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

impl Editor {
    /// True when the editor should reserve right-margin space for floating cards.
    pub(super) fn has_rail_comments(&self) -> bool {
        !self.markdown
            && !self.preview
            && !self.board_file
            && !self.table_file
            && !self.board_open()
            && !self.table_open()
            && !self.comment_rail.drawer_open
            && (self.comment_rail.draft.is_some() || self.core.doc.open_comment_count() > 0)
    }

    /// True when wide-screen floating margin cards are actively displayed.
    pub(crate) fn rail_active(&self) -> bool {
        self.layout.comment_rail.is_some() && !self.comment_rail.drawer_open && !self.preview
    }

    /// True when the pane has enough width to host floating cards (even before
    /// the first comment exists on the document).
    pub(super) fn can_use_comment_rail(&self) -> bool {
        if self.markdown || self.preview || self.board_file || self.table_file || self.board_open() || self.table_open() || self.comment_rail.drawer_open {
            return false;
        }
        let o = f32::from(self.pane_size().width);
        page_layout(o, self.column_max(), !self.cat_entries.is_empty(), self.cat_pinned, true).comment_rail.is_some()
    }

    /// Record unresolved comment anchors inside `b` (and its non-flattened
    /// descendants) during virtualized row rendering.
    pub(super) fn collect_visible_comments(&self, b: &Block) {
        if self.markdown || self.core.doc.comments.is_empty() {
            return;
        }
        let mut out = self.comment_rail.visible_anchors.borrow_mut();
        fn walk(ed: &Editor, b: &Block, root_flat: bool, out: &mut Vec<VisibleAnchor>) {
            for cid in &b.comment_ids {
                if ed.is_comment_open(cid) && !out.iter().any(|a| a.thread_id == *cid) {
                    out.push(VisibleAnchor { thread_id: cid.clone(), block: b.id, inline_offset: None });
                }
            }
            for (range, m) in b.text.spans() {
                for cid in &m.comment_ids {
                    if ed.is_comment_open(cid) && !out.iter().any(|a| a.thread_id == *cid) {
                        out.push(VisibleAnchor { thread_id: cid.clone(), block: b.id, inline_offset: Some(range.start) });
                    }
                }
            }
            if !root_flat && !b.folded {
                for c in &b.children {
                    walk(ed, c, false, out);
                }
            }
            if let BlockKind::Table(t) = &b.kind {
                for (_, _, cell) in t.cells() {
                    for c in &cell.blocks {
                        walk(ed, c, false, out);
                    }
                }
            }
        }
        walk(self, b, flattens(b), &mut out);
    }

    /// True when newly painted anchors or card heights differ from what the
    /// rail laid out on this frame (triggers a single next-frame redraw).
    pub(super) fn comment_rail_stale(&self) -> bool {
        if !self.rail_active() && self.comment_rail.draft.is_none() {
            return false;
        }
        if *self.comment_rail.visible_anchors.borrow() != self.comment_rail.last_anchors {
            return true;
        }
        let cur = self.comment_rail.heights.borrow();
        self.comment_rail.last_heights.iter().any(|(k, &prev)| cur.get(k).is_some_and(|&now| (now - prev).abs() > 1.0))
    }

    pub(crate) fn clear_rail_inputs(&mut self) {
        self.comment_rail.draft = None;
        self.comment_rail.replying = None;
        self.comment_rail.editing = None;
    }

    /// Extract an unsent non-empty rail draft when transferring to the full drawer.
    pub(crate) fn take_nonempty_rail_draft(&mut self, cx: &App) -> Option<(CommentTarget, String, String, u64, bool)> {
        let d = self.comment_rail.draft.as_ref()?;
        let text = d.field.read(cx).text.clone();
        if text.trim().is_empty() {
            return None;
        }
        let d = self.comment_rail.draft.take()?;
        Some((d.target, d.quote, text, d.anchor_gen, d.stale))
    }

    pub(super) fn start_rail_draft(&mut self, target: CommentTarget, quote: String, window: &mut Window, cx: &mut Context<Self>) {
        // Keep any unsent text if the user already has a draft open and picks a new anchor.
        let existing = self.comment_rail.draft.as_ref().map(|d| d.field.read(cx).text.clone()).unwrap_or_default();
        let field = make_rail_field(crate::settings::tr("输入评论，Enter 发送", "Write a comment, Enter to post"), &existing, cx);
        let focus = field.focus_handle(cx);
        let subs = vec![cx.subscribe_in(&field, window, |e, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => e.submit_rail_draft(window, cx),
            TextFieldEvent::Cancel => e.cancel_rail_draft(window, cx),
            _ => {}
        })];
        self.draft_comment = Some(target);
        self.active_comment = None;
        if self.comment_rail.replying.as_ref().is_some_and(|r| r.field.read(cx).text.trim().is_empty()) {
            self.comment_rail.replying = None;
        }
        if self.comment_rail.editing.as_ref().is_some_and(|e| e.field.read(cx).text.trim().is_empty()) {
            self.comment_rail.editing = None;
        }
        self.comment_rail.draft = Some(RailDraft { target, quote, anchor_gen: self.core.generation, stale: false, field, _subs: subs });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn rebind_rail_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, quote)) = self.current_comment_target() else { return };
        if let Some(d) = self.comment_rail.draft.as_mut() {
            d.target = target;
            d.quote = quote;
            d.anchor_gen = self.core.generation;
            d.stale = false;
            let focus = d.field.focus_handle(cx);
            self.draft_comment = Some(target);
            window.focus(&focus, cx);
            cx.notify();
        }
    }

    fn cancel_rail_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comment_rail.draft = None;
        self.draft_comment = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn submit_rail_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, quote, anchor_gen, body)) = self.comment_rail.draft.as_ref().map(|d| (d.target, d.quote.clone(), d.anchor_gen, d.field.read(cx).text.clone())) else {
            return;
        };
        if body.trim().is_empty() {
            return;
        }
        let Some(valid_target) = self.core.validate_comment_target(target, &quote, self.core.generation == anchor_gen) else {
            if let Some(d) = self.comment_rail.draft.as_mut() {
                d.stale = true;
            }
            self.draft_comment = None;
            cx.notify();
            return;
        };
        let now = crate::time::now();
        if let Some(tid) = self.core.add_comment(valid_target, String::new(), body, now) {
            self.comment_rail.draft = None;
            self.draft_comment = None;
            self.active_comment = Some(tid);
            window.focus(&self.focus, cx);
            self.changed_preserving_viewport(window, cx);
        }
    }

    fn start_rail_reply(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = make_rail_field(crate::settings::tr("回复评论，Enter 发送", "Reply, Enter to post"), "", cx);
        let focus = field.focus_handle(cx);
        let subs = vec![cx.subscribe_in(&field, window, |e, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => e.submit_rail_reply(window, cx),
            TextFieldEvent::Cancel => {
                e.comment_rail.replying = None;
                window.focus(&e.focus, cx);
                cx.notify();
            }
            _ => {}
        })];
        self.active_comment = Some(thread_id.to_string());
        self.comment_rail.editing = None;
        self.comment_rail.replying = Some(RailReply { thread_id: thread_id.to_string(), field, _subs: subs });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn submit_rail_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((tid, body)) = self.comment_rail.replying.as_ref().map(|r| (r.thread_id.clone(), r.field.read(cx).text.clone())) else {
            return;
        };
        if body.trim().is_empty() {
            return;
        }
        let now = crate::time::now();
        if self.core.add_reply(&tid, String::new(), body, now).is_some() {
            self.comment_rail.replying = None;
            window.focus(&self.focus, cx);
            self.changed_preserving_viewport(window, cx);
        }
    }

    fn start_rail_edit(&mut self, thread_id: &str, reply_id: &str, body: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = make_rail_field(crate::settings::tr("编辑评论", "Edit comment"), body, cx);
        let focus = field.focus_handle(cx);
        let subs = vec![cx.subscribe_in(&field, window, |e, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => e.submit_rail_edit(window, cx),
            TextFieldEvent::Cancel => {
                e.comment_rail.editing = None;
                window.focus(&e.focus, cx);
                cx.notify();
            }
            _ => {}
        })];
        self.active_comment = Some(thread_id.to_string());
        self.comment_rail.replying = None;
        self.comment_rail.editing = Some(RailEdit {
            thread_id: thread_id.to_string(),
            reply_id: reply_id.to_string(),
            field,
            _subs: subs,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn submit_rail_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((tid, rid, body)) = self.comment_rail.editing.as_ref().map(|e| (e.thread_id.clone(), e.reply_id.clone(), e.field.read(cx).text.clone())) else {
            return;
        };
        if body.trim().is_empty() {
            return;
        }
        let now = crate::time::now();
        if self.core.edit_reply(&tid, &rid, body, now) {
            self.comment_rail.editing = None;
            window.focus(&self.focus, cx);
            self.changed_preserving_viewport(window, cx);
        } else {
            self.comment_rail.editing = None;
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn rail_set_resolved(&mut self, thread_id: &str, resolved: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.core.set_comment_resolved(thread_id, resolved) {
            if self.active_comment.as_deref() == Some(thread_id) {
                self.active_comment = None;
            }
            if self.comment_rail.replying.as_ref().is_some_and(|r| r.thread_id == thread_id) {
                self.comment_rail.replying = None;
            }
            if self.comment_rail.editing.as_ref().is_some_and(|e| e.thread_id == thread_id) {
                self.comment_rail.editing = None;
            }
            self.changed_preserving_viewport(window, cx);
        }
    }

    fn rail_delete_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.core.delete_comment(thread_id) {
            if self.active_comment.as_deref() == Some(thread_id) {
                self.active_comment = None;
            }
            if self.comment_rail.replying.as_ref().is_some_and(|r| r.thread_id == thread_id) {
                self.comment_rail.replying = None;
            }
            if self.comment_rail.editing.as_ref().is_some_and(|e| e.thread_id == thread_id) {
                self.comment_rail.editing = None;
            }
            self.changed_preserving_viewport(window, cx);
        }
    }

    fn rail_delete_reply(&mut self, thread_id: &str, reply_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.core.delete_reply(thread_id, reply_id) {
            if self.core.doc.comments.iter().all(|t| t.id != thread_id) && self.active_comment.as_deref() == Some(thread_id) {
                self.active_comment = None;
            }
            if self.comment_rail.editing.as_ref().is_some_and(|e| e.thread_id == thread_id && e.reply_id == reply_id) {
                self.comment_rail.editing = None;
            }
            self.changed_preserving_viewport(window, cx);
        }
    }

    pub(super) fn escape_comment_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.comment_rail.editing.take().is_some() || self.comment_rail.replying.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
            return true;
        }
        if self.comment_rail.draft.is_some() {
            self.cancel_rail_draft(window, cx);
            return true;
        }
        if self.rail_active() && self.active_comment.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    fn anchor_y(&self, block: BlockId, inline_offset: Option<usize>) -> Option<f32> {
        if let Some(off) = inline_offset {
            let p = Pos::new(block, off);
            let probe = Probe::Point(p);
            let val = self.probe(probe);
            self.probes.borrow_mut().push((probe, val));
            if let Some(b) = val {
                return Some(f32::from(self.rel(b.origin).y));
            }
        }
        let probe = Probe::Block(block);
        let val = self.probe(probe);
        self.probes.borrow_mut().push((probe, val));
        val.map(|b| f32::from(self.rel(b.origin).y))
    }

    fn target_anchor_y(&self, target: CommentTarget) -> f32 {
        match target {
            CommentTarget::Block(id) => self.anchor_y(id, None).unwrap_or(CARD_MIN_Y),
            CommentTarget::Inline { start, end } => {
                let off = if start.block == end.block { start.offset.min(end.offset) } else { start.offset };
                self.anchor_y(start.block, Some(off)).or_else(|| self.anchor_y(start.block, None)).unwrap_or(CARD_MIN_Y)
            }
        }
    }

    /// Renders floating Y-aligned comment cards in the right margin on wide screens.
    pub(super) fn render_comment_rail(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active_rail = self.rail_active();
        if !active_rail && self.comment_rail.draft.is_none() {
            self.comment_rail.last_anchors.clear();
            self.comment_rail.last_heights.clear();
            return None;
        }
        let anchors = self.comment_rail.visible_anchors.borrow().clone();
        self.comment_rail.last_anchors = anchors.clone();

        let heights = self.comment_rail.heights.clone();
        let h_snap = heights.borrow().clone();
        let mut items: Vec<(String, f32, f32)> = vec![];
        let mut used_heights: HashMap<String, f32> = HashMap::new();

        if active_rail {
            for a in &anchors {
                if !self.is_comment_open(&a.thread_id) {
                    continue;
                }
                let Some(y) = self.anchor_y(a.block, a.inline_offset) else { continue };
                let h = h_snap.get(&a.thread_id).copied().unwrap_or(DEFAULT_CARD_H);
                used_heights.insert(a.thread_id.clone(), h);
                items.push((a.thread_id.clone(), y, h));
            }
        }
        // Revalidate rail draft anchor whenever the document has changed.
        if let Some(d) = self.comment_rail.draft.as_mut()
            && self.core.generation != d.anchor_gen
        {
            match self.core.validate_comment_target(d.target, &d.quote, false) {
                Some(rebased) => {
                    d.target = rebased;
                    d.anchor_gen = self.core.generation;
                    d.stale = false;
                    self.draft_comment = Some(rebased);
                }
                None => {
                    d.stale = true;
                    self.draft_comment = None;
                }
            }
        }
        if let Some(draft) = &self.comment_rail.draft {
            let y = if draft.stale { CARD_MIN_Y } else { self.target_anchor_y(draft.target) };
            let h = h_snap.get(DRAFT_KEY).copied().unwrap_or(148.);
            used_heights.insert(DRAFT_KEY.into(), h);
            items.push((DRAFT_KEY.into(), y, h));
        }
        self.comment_rail.last_heights = used_heights;
        if items.is_empty() {
            return None;
        }

        let active_key: Option<&str> = if self.comment_rail.draft.is_some() {
            Some(DRAFT_KEY)
        } else {
            self.active_comment.as_deref()
        };
        let max_bottom = (f32::from(self.pane_size().height) - 12.).max(CARD_MIN_Y + 80.);
        let placed = layout_cards(&items, active_key, CARD_GAP, CARD_MIN_Y, max_bottom);

        let pane_w = f32::from(self.pane_size().width);
        let rail_left = if active_rail {
            (self.layout.ml + self.layout.content + COMMENT_RAIL_GAP - ACTIVE_POP_LEFT).max(0.)
        } else {
            // Window was resized narrower while typing a draft: keep draft visible at the right edge.
            (pane_w - COMMENT_CARD_W - ACTIVE_POP_LEFT - 14.).max(8.)
        };
        let rail_w = COMMENT_CARD_W + ACTIVE_POP_LEFT + 6.;
        let read_only = self.read_only || self.preview;

        let mut cards: Vec<AnyElement> = Vec::with_capacity(placed.len());
        for (ci, (key, y)) in placed.into_iter().enumerate() {
            if key == DRAFT_KEY {
                let Some(d) = &self.comment_rail.draft else { continue };
                let quote = if d.quote.is_empty() {
                    crate::settings::tr("当前段落", "Current block").to_string()
                } else {
                    d.quote.clone()
                };
                let empty = d.field.read(cx).text.trim().is_empty();
                let stale = d.stale;
                cards.push(
                    div()
                        .id("rail-comment-draft")
                        .occlude()
                        .absolute()
                        .top(px(y))
                        .left(px(0.))
                        .w(px(COMMENT_CARD_W))
                        .p(px(11.))
                        .rounded(px(9.))
                        .border_1()
                        .border_color(if stale { Theme::danger() } else { Theme::comment_underline() })
                        .bg(Theme::bg_body())
                        .shadow(theme::shadow_panel())
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(record_card_height(DRAFT_KEY.into(), heights.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(div().flex_none().w(px(2.)).h(px(13.)).rounded(px(1.)).bg(if stale { Theme::danger() } else { Theme::comment_underline() }))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(11.5))
                                        .text_color(Theme::text_caption())
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(quote),
                                ),
                        )
                        .when(stale, |el| {
                            el.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(6.))
                                    .px(px(7.))
                                    .py(px(4.))
                                    .rounded(px(5.))
                                    .bg(Theme::fill_hover())
                                    .child(div().flex_1().text_size(px(11.)).text_color(Theme::danger()).child(crate::settings::tr("原位置已变动，请重选", "Anchor moved; reselect")))
                                    .child(
                                        div()
                                            .id("rail-draft-rebind")
                                            .flex_none()
                                            .px(px(6.))
                                            .py(px(2.))
                                            .rounded(px(4.))
                                            .bg(Theme::primary_fill_1())
                                            .text_size(px(11.))
                                            .text_color(Theme::primary())
                                            .cursor_pointer()
                                            .child(crate::settings::tr("更新位置", "Re-anchor"))
                                            .on_click(cx.listener(|e, _, window, cx| e.rebind_rail_draft(window, cx))),
                                    ),
                            )
                        })
                        .child(
                            div()
                                .min_h(px(48.))
                                .max_h(px(130.))
                                .px(px(8.))
                                .py(px(5.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(Theme::line_border())
                                .bg(crate::theme::raised(Theme::bg_body()))
                                .child(d.field.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap(px(6.))
                                .child(
                                    secondary_button("rail-draft-cancel", crate::settings::tr("取消", "Cancel"))
                                        .on_click(cx.listener(|e, _, window, cx| e.cancel_rail_draft(window, cx))),
                                )
                                .child(
                                    primary_button("rail-draft-post", crate::settings::tr("发送", "Post"))
                                        .when(empty || stale, |b| b.opacity(0.45))
                                        .on_click(cx.listener(|e, _, window, cx| e.submit_rail_draft(window, cx))),
                                ),
                        )
                        .into_any_element(),
                );
                continue;
            }

            let Some(thread) = self.core.doc.comments.iter().find(|t| t.id == key).cloned() else { continue };
            let tid = thread.id.clone();
            let active = self.active_comment.as_deref() == Some(tid.as_str());
            let card_x = if active { 0. } else { ACTIVE_POP_LEFT };
            let quote = if thread.quote.is_empty() {
                crate::settings::tr("段落评论", "Block comment").to_string()
            } else {
                thread.quote.clone()
            };
            let tid_click = tid.clone();
            let tid_resolve = tid.clone();
            let tid_delete = tid.clone();
            let tid_reply = tid.clone();

            let action_btn = |id: SharedString, ic: &'static str, tip: &'static str| {
                div()
                    .id(id)
                    .size(px(21.))
                    .rounded(px(5.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::fill_hover_strong()))
                    .tip(tip, None)
                    .child(icon(ic, 12.5, Theme::icon()))
            };

            let reply_els: Vec<AnyElement> = thread
                .replies
                .iter()
                .enumerate()
                .map(|(ri, r)| {
                    let is_editing = self.comment_rail.editing.as_ref().is_some_and(|e| e.thread_id == tid && e.reply_id == r.id);
                    if is_editing && let Some(ed_state) = &self.comment_rail.editing {
                        let empty = ed_state.field.read(cx).text.trim().is_empty();
                        return div()
                            .id(SharedString::from(format!("rcm-edit-{ci}-{ri}")))
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .pt(px(3.))
                            .child(
                                div()
                                    .min_h(px(40.))
                                    .px(px(7.))
                                    .py(px(4.))
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(Theme::primary_500())
                                    .bg(crate::theme::raised(Theme::bg_body()))
                                    .child(ed_state.field.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap(px(5.))
                                    .child(secondary_button("rcm-edit-cancel", crate::settings::tr("取消", "Cancel")).on_click(cx.listener(|e, _, window, cx| {
                                        e.comment_rail.editing = None;
                                        window.focus(&e.focus, cx);
                                        cx.notify();
                                    })))
                                    .child(primary_button("rcm-edit-save", crate::settings::tr("保存", "Save")).when(empty, |b| b.opacity(0.45)).on_click(cx.listener(|e, _, window, cx| e.submit_rail_edit(window, cx)))),
                            )
                            .into_any_element();
                    }
                    let g = SharedString::from(format!("rcm-reply-{ci}-{ri}"));
                    let when = if r.created > 0 { format_time(r.created) } else { String::new() };
                    let edited = r.updated > r.created;
                    let tid_e = tid.clone();
                    let rid_e = r.id.clone();
                    let body_e = r.body.clone();
                    let tid_d = tid.clone();
                    let rid_d = r.id.clone();
                    let lines: Vec<String> = r.body.split('\n').map(str::to_string).collect();
                    div()
                        .id(g.clone())
                        .group(g.clone())
                        .when(ri > 0, |d| d.pt(px(7.)).mt(px(7.)).border_t_1().border_color(Theme::line_divider()))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(5.))
                                        .min_w_0()
                                        .when(!r.author.is_empty(), |d| {
                                            d.child(div().text_size(px(11.5)).font_weight(FontWeight::MEDIUM).text_color(Theme::text()).child(r.author.clone()))
                                        })
                                        .when(!when.is_empty(), |d| {
                                            d.child(div().text_size(px(10.5)).text_color(Theme::text_caption()).child(when))
                                        })
                                        .when(edited, |d| {
                                            d.child(div().text_size(px(10.5)).text_color(Theme::text_placeholder()).child(crate::settings::tr("已编辑", "edited")))
                                        }),
                                )
                                .when(!read_only, |d| {
                                    d.child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(2.))
                                            .opacity(0.)
                                            .group_hover(g.clone(), |s| s.opacity(1.))
                                            .child(
                                                div()
                                                    .id(SharedString::from(format!("rcm-ed-{ci}-{ri}")))
                                                    .px(px(4.))
                                                    .py(px(1.))
                                                    .rounded(px(4.))
                                                    .text_size(px(10.5))
                                                    .text_color(Theme::text_caption())
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(Theme::fill_hover_strong()).text_color(Theme::text()))
                                                    .child(crate::settings::tr("编辑", "Edit"))
                                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                    .on_click(cx.listener(move |e, _, window, cx| e.start_rail_edit(&tid_e, &rid_e, &body_e, window, cx))),
                                            )
                                            .when(ri > 0, |d| {
                                                d.child(
                                                    div()
                                                        .id(SharedString::from(format!("rcm-rm-{ci}-{ri}")))
                                                        .px(px(4.))
                                                        .py(px(1.))
                                                        .rounded(px(4.))
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_caption())
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(Theme::fill_hover_strong()).text_color(Theme::danger()))
                                                        .child(crate::settings::tr("删除", "Delete"))
                                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                        .on_click(cx.listener(move |e, _, window, cx| e.rail_delete_reply(&tid_d, &rid_d, window, cx))),
                                                )
                                            }),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .text_size(px(12.5))
                                .line_height(px(18.))
                                .text_color(Theme::text())
                                .children(lines.into_iter().map(|line| {
                                    div().min_h(px(18.)).child(if line.is_empty() { " ".to_string() } else { line })
                                })),
                        )
                        .into_any_element()
                })
                .collect();

            let is_replying = self.comment_rail.replying.as_ref().is_some_and(|r| r.thread_id == tid);
            let reply_area = (!read_only && (active || is_replying)).then(|| {
                if is_replying && let Some(rep) = &self.comment_rail.replying {
                    let empty = rep.field.read(cx).text.trim().is_empty();
                    div()
                        .mt(px(8.))
                        .pt(px(7.))
                        .border_t_1()
                        .border_color(Theme::line_divider())
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child(
                            div()
                                .min_h(px(40.))
                                .max_h(px(110.))
                                .px(px(7.))
                                .py(px(4.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(Theme::line_border())
                                .bg(crate::theme::raised(Theme::bg_body()))
                                .child(rep.field.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(5.))
                                .child(secondary_button("rcm-rep-cancel", crate::settings::tr("取消", "Cancel")).on_click(cx.listener(|e, _, window, cx| {
                                    e.comment_rail.replying = None;
                                    window.focus(&e.focus, cx);
                                    cx.notify();
                                })))
                                .child(primary_button("rcm-rep-send", crate::settings::tr("回复", "Reply")).when(empty, |b| b.opacity(0.45)).on_click(cx.listener(|e, _, window, cx| e.submit_rail_reply(window, cx)))),
                        )
                        .into_any_element()
                } else {
                    div()
                        .id(SharedString::from(format!("rcm-rep-open-{ci}")))
                        .mt(px(8.))
                        .h(px(26.))
                        .px(px(8.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(Theme::line_border())
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(Theme::text_placeholder())
                        .cursor_pointer()
                        .hover(|s| s.bg(Theme::fill_hover()))
                        .child(crate::settings::tr("添加回复…", "Reply…"))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |e, _, window, cx| e.start_rail_reply(&tid_reply, window, cx)))
                        .into_any_element()
                }
            });

            cards.push(
                div()
                    .id(SharedString::from(format!("rcm-card-{ci}")))
                    .occlude()
                    .absolute()
                    .top(px(y))
                    .left(px(card_x))
                    .w(px(COMMENT_CARD_W))
                    .p(px(10.))
                    .rounded(px(9.))
                    .border_1()
                    .border_color(if active { Theme::comment_underline() } else { Theme::line_border() })
                    .bg(Theme::bg_body())
                    .shadow(if active { theme::shadow_panel() } else { theme::shadow_button() })
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, cx.listener(move |e, _, _, cx| {
                        cx.stop_propagation();
                        e.active_comment = Some(tid_click.clone());
                        if e.comment_rail.replying.as_ref().is_some_and(|r| r.thread_id != tid_click && r.field.read(cx).text.trim().is_empty()) {
                            e.comment_rail.replying = None;
                        }
                        if e.comment_rail.editing.as_ref().is_some_and(|ed| ed.thread_id != tid_click && ed.field.read(cx).text.trim().is_empty()) {
                            e.comment_rail.editing = None;
                        }
                        cx.notify();
                    }))
                    .child(record_card_height(tid.clone(), heights.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(4.))
                            .mb(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .child(div().flex_none().w(px(2.)).h(px(12.)).rounded(px(1.)).bg(Theme::comment_underline()))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(11.5))
                                            .text_color(Theme::text_caption())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(quote),
                                    ),
                            )
                            .when(!read_only, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap(px(2.))
                                        .child(
                                            action_btn(SharedString::from(format!("rcm-res-{ci}")), "check", crate::settings::tr("解决", "Resolve"))
                                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                .on_click(cx.listener(move |e, _, window, cx| e.rail_set_resolved(&tid_resolve, true, window, cx))),
                                        )
                                        .child(
                                            action_btn(SharedString::from(format!("rcm-del-{ci}")), "bin", crate::settings::tr("删除", "Delete"))
                                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                .on_click(cx.listener(move |e, _, window, cx| e.rail_delete_thread(&tid_delete, window, cx))),
                                        ),
                                )
                            }),
                    )
                    .children(reply_els)
                    .children(reply_area)
                    .into_any_element(),
            );
        }

        Some(
            div()
                .id("comment-rail")
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(rail_left))
                .w(px(rail_w))
                .overflow_hidden()
                .children(cards)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::layout_cards;

    #[test]
    fn layout_cards_avoids_overlap_and_pins_active() {
        let items = vec![
            ("c1".into(), 100., 80.),
            ("c2".into(), 120., 80.),
            ("c3".into(), 140., 80.),
        ];
        // Without active: cascades cleanly downward.
        let laid = layout_cards(&items, None, 10., 12., 800.);
        assert_eq!(laid, vec![("c1".into(), 100.), ("c2".into(), 190.), ("c3".into(), 280.)]);

        // With c2 active: c2 stays pinned at 120, c1 shifts up to 30, c3 shifts down to 210.
        let laid = layout_cards(&items, Some("c2"), 10., 12., 800.);
        assert_eq!(laid, vec![("c1".into(), 30.), ("c2".into(), 120.), ("c3".into(), 210.)]);

        // Active card anchored near the bottom clamps inside the viewport so composer stays visible.
        let bottom = vec![("c1".into(), 500., 80.), ("__draft__".into(), 570., 150.)];
        let laid = layout_cards(&bottom, Some("__draft__"), 10., 12., 600.);
        assert_eq!(laid, vec![("c1".into(), 360.), ("__draft__".into(), 450.)]);
    }
}
