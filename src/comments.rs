//! 评论: right-side comment panel for inline text selections and blocks
//! (paragraphs, headings, lists, code, images, file attachments, tables).

use crate::app::Workspace;
use crate::editor::core::CommentTarget;
use crate::input::{TextField, TextFieldEvent};
use crate::theme::{self, Theme};
use crate::ui::{TipExt, format_time, icon, primary_button, secondary_button};
use gpui::{prelude::FluentBuilder, *};

/// Width of the docked right-side comment panel (matches shortcuts & history).
pub const DRAWER_W: f32 = 300.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentTab {
    Open,
    Resolved,
}

pub struct CommentDraft {
    pub target: CommentTarget,
    pub quote: String,
    pub anchor_gen: u64,
    pub stale: bool,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

pub struct CommentReplyInput {
    pub thread_id: String,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

pub struct CommentEditInput {
    pub thread_id: String,
    pub reply_id: String,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

pub struct CommentPanel {
    pub tab: CommentTab,
    pub active: Option<String>,
    pub draft: Option<CommentDraft>,
    pub replying: Option<CommentReplyInput>,
    pub editing: Option<CommentEditInput>,
    pub pending_scroll: Option<String>,
    pub scroll: ScrollHandle,
}

impl CommentPanel {
    fn new() -> Self {
        CommentPanel {
            tab: CommentTab::Open,
            active: None,
            draft: None,
            replying: None,
            editing: None,
            pending_scroll: None,
            scroll: ScrollHandle::new(),
        }
    }
}

fn make_comment_field(placeholder: &'static str, initial: &str, cx: &mut Context<Workspace>) -> Entity<TextField> {
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

impl Workspace {
    /// True when the open document supports comments (live DocxXML, not Markdown/preview/board/table-file).
    pub(crate) fn comments_supported(&self, cx: &App) -> bool {
        self.current.is_some()
            && self.preview.is_none()
            && self.editor.as_ref().is_some_and(|ed| {
                let e = ed.read(cx);
                !e.markdown && !e.preview && !e.board_file && !e.table_file && !e.board_open() && !e.table_open()
            })
    }

    pub(crate) fn open_comment_count(&self, cx: &App) -> usize {
        if !self.comments_supported(cx) {
            return 0;
        }
        self.editor.as_ref().map_or(0, |ed| ed.read(cx).core.doc.open_comment_count())
    }

    fn sync_editor_comment_state(&self, cx: &mut Context<Self>) {
        let drawer_open = self.comments.is_some();
        let active = self.comments.as_ref().and_then(|c| c.active.clone());
        let draft = self.comments.as_ref().and_then(|c| c.draft.as_ref().filter(|d| !d.stale).map(|d| d.target));
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| {
                e.comment_rail.drawer_open = drawer_open;
                if drawer_open {
                    e.clear_rail_inputs();
                }
                e.active_comment = active;
                e.draft_comment = draft;
                cx.notify();
            });
        }
    }

    pub fn open_comments(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if !self.comments_supported(cx) {
            return;
        }
        if self.history.is_some() {
            self.close_history(cx);
        }
        // Preserve any unsent floating rail draft when switching to the drawer.
        let migrated = self.editor.as_ref().and_then(|ed| ed.update(cx, |e, cx| e.take_nonempty_rail_draft(cx)));
        self.shortcuts = false;
        self.doc_menu = false;
        self.doc_sub = None;
        if self.comments.is_none() {
            self.comments = Some(CommentPanel::new());
        }
        if let (Some((target, quote, text, anchor_gen, stale)), Some(window)) = (migrated, window) {
            let field = make_comment_field(crate::settings::tr("输入评论，Enter 发送", "Write a comment, Enter to post"), &text, cx);
            let subs = vec![cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
                TextFieldEvent::Submit => w.submit_comment_draft(window, cx),
                TextFieldEvent::Cancel => w.cancel_comment_draft(window, cx),
                _ => {}
            })];
            if let Some(p) = self.comments.as_mut()
                && p.draft.is_none()
            {
                p.draft = Some(CommentDraft { target, quote, anchor_gen, stale, field, _subs: subs });
            }
        }
        self.sync_editor_comment_state(cx);
        cx.notify();
    }

    pub fn toggle_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.comments.is_some() {
            self.close_comments(cx);
        } else {
            self.open_comments(Some(window), cx);
        }
    }

    pub fn close_comments(&mut self, cx: &mut Context<Self>) {
        if self.comments.take().is_none() {
            return;
        }
        self.sync_editor_comment_state(cx);
        cx.notify();
    }

    pub fn start_comment_draft(&mut self, target: CommentTarget, quote: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.comments_supported(cx) {
            return;
        }
        let existing = self.comments.as_ref()
            .and_then(|p| p.draft.as_ref())
            .map(|d| d.field.read(cx).text.clone())
            .unwrap_or_default();
        self.open_comments(Some(window), cx);
        let anchor_gen = self.editor.as_ref().map_or(0, |ed| ed.read(cx).core.generation);
        let field = make_comment_field(crate::settings::tr("输入评论，Enter 发送", "Write a comment, Enter to post"), &existing, cx);
        let focus = field.focus_handle(cx);
        let subs = vec![cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => w.submit_comment_draft(window, cx),
            TextFieldEvent::Cancel => w.cancel_comment_draft(window, cx),
            _ => {}
        })];
        if let Some(p) = self.comments.as_mut() {
            p.tab = CommentTab::Open;
            p.active = None;
            if p.replying.as_ref().is_some_and(|r| r.field.read(cx).text.trim().is_empty()) {
                p.replying = None;
            }
            if p.editing.as_ref().is_some_and(|e| e.field.read(cx).text.trim().is_empty()) {
                p.editing = None;
            }
            p.draft = Some(CommentDraft { target, quote, anchor_gen, stale: false, field, _subs: subs });
        }
        self.sync_editor_comment_state(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn rebind_comment_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, quote, anchor_gen)) = self.editor.as_ref().and_then(|ed| {
            let e = ed.read(cx);
            let (t, q) = e.current_comment_target()?;
            Some((t, q, e.core.generation))
        }) else {
            self.show_toast_kind(crate::ui::ToastKind::Info, crate::settings::tr("请先在正文中选中文字或段落", "Select text or a block in the document first"), cx);
            return;
        };
        if let Some(p) = self.comments.as_mut()
            && let Some(d) = p.draft.as_mut()
        {
            d.target = target;
            d.quote = quote;
            d.anchor_gen = anchor_gen;
            d.stale = false;
            let focus = d.field.focus_handle(cx);
            window.focus(&focus, cx);
        }
        self.sync_editor_comment_state(cx);
        cx.notify();
    }

    fn cancel_comment_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.comments.as_mut() {
            p.draft = None;
        }
        self.sync_editor_comment_state(cx);
        if let Some(ed) = &self.editor {
            let f = ed.read(cx).focus.clone();
            window.focus(&f, cx);
        }
        cx.notify();
    }

    fn submit_comment_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, quote, anchor_gen, body)) = self.comments.as_ref().and_then(|p| {
            let d = p.draft.as_ref()?;
            Some((d.target, d.quote.clone(), d.anchor_gen, d.field.read(cx).text.clone()))
        }) else { return };
        if body.trim().is_empty() {
            return;
        }
        let valid = self.editor.as_ref().and_then(|ed| {
            let e = ed.read(cx);
            e.core.validate_comment_target(target, &quote, e.core.generation == anchor_gen)
        });
        let Some(valid_target) = valid else {
            if let Some(p) = self.comments.as_mut()
                && let Some(d) = p.draft.as_mut()
            {
                d.stale = true;
            }
            self.sync_editor_comment_state(cx);
            self.show_toast_kind(crate::ui::ToastKind::Info, crate::settings::tr("原位置已变动，请重新选中内容后更新位置", "Anchor moved; reselect content and click Re-anchor"), cx);
            cx.notify();
            return;
        };
        let now = crate::time::now();
        let created = self.editor.as_ref().and_then(|ed| {
            ed.update(cx, |e, cx| {
                let id = e.core.add_comment(valid_target, String::new(), body, now)?;
                e.draft_comment = None;
                e.active_comment = Some(id.clone());
                e.changed_preserving_viewport(window, cx);
                Some(id)
            })
        });
        if let Some(tid) = created {
            if let Some(p) = self.comments.as_mut() {
                p.draft = None;
                p.tab = CommentTab::Open;
                p.active = Some(tid.clone());
                p.pending_scroll = Some(tid);
            }
            self.sync_editor_comment_state(cx);
            cx.notify();
        }
    }

    pub fn focus_comment_thread(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        if !self.comments_supported(cx) {
            return;
        }
        let resolved = self.editor.as_ref()
            .and_then(|ed| ed.read(cx).core.doc.comments.iter().find(|t| t.id == thread_id).map(|t| t.resolved))
            .unwrap_or(false);
        self.open_comments(None, cx);
        if let Some(p) = self.comments.as_mut() {
            p.tab = if resolved { CommentTab::Resolved } else { CommentTab::Open };
            p.active = Some(thread_id.to_string());
            p.pending_scroll = Some(thread_id.to_string());
            // Keep unsent non-empty inputs intact when navigating between threads.
            if p.draft.as_ref().is_some_and(|d| d.field.read(cx).text.trim().is_empty()) {
                p.draft = None;
            }
            if p.replying.as_ref().is_some_and(|r| r.thread_id != thread_id && r.field.read(cx).text.trim().is_empty()) {
                p.replying = None;
            }
            if p.editing.as_ref().is_some_and(|e| e.thread_id != thread_id && e.field.read(cx).text.trim().is_empty()) {
                p.editing = None;
            }
        }
        self.sync_editor_comment_state(cx);
        cx.notify();
    }

    fn select_comment_card(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        if let Some(p) = self.comments.as_mut() {
            p.active = Some(thread_id.to_string());
            if p.replying.as_ref().is_some_and(|r| r.thread_id != thread_id && r.field.read(cx).text.trim().is_empty()) {
                p.replying = None;
            }
            if p.editing.as_ref().is_some_and(|e| e.thread_id != thread_id && e.field.read(cx).text.trim().is_empty()) {
                p.editing = None;
            }
        }
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| e.reveal_comment(thread_id, cx));
        }
        cx.notify();
    }

    fn start_comment_reply(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = make_comment_field(crate::settings::tr("回复评论，Enter 发送", "Reply, Enter to post"), "", cx);
        let focus = field.focus_handle(cx);
        let subs = vec![cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => w.submit_comment_reply(window, cx),
            TextFieldEvent::Cancel => {
                if let Some(p) = w.comments.as_mut() { p.replying = None; }
                cx.notify();
            }
            _ => {}
        })];
        if let Some(p) = self.comments.as_mut() {
            p.active = Some(thread_id.to_string());
            p.editing = None;
            p.replying = Some(CommentReplyInput { thread_id: thread_id.to_string(), field, _subs: subs });
        }
        self.sync_editor_comment_state(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn submit_comment_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((tid, body)) = self.comments.as_ref().and_then(|p| {
            let r = p.replying.as_ref()?;
            Some((r.thread_id.clone(), r.field.read(cx).text.clone()))
        }) else { return };
        if body.trim().is_empty() {
            return;
        }
        let now = crate::time::now();
        let ok = self.editor.as_ref().is_some_and(|ed| {
            ed.update(cx, |e, cx| {
                let added = e.core.add_reply(&tid, String::new(), body, now).is_some();
                if added {
                    e.changed_preserving_viewport(window, cx);
                }
                added
            })
        });
        if ok {
            if let Some(p) = self.comments.as_mut() {
                p.replying = None;
            }
            cx.notify();
        }
    }

    fn start_comment_edit(&mut self, thread_id: &str, reply_id: &str, body: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = make_comment_field(crate::settings::tr("编辑评论", "Edit comment"), body, cx);
        let focus = field.focus_handle(cx);
        let subs = vec![cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => w.submit_comment_edit(window, cx),
            TextFieldEvent::Cancel => {
                if let Some(p) = w.comments.as_mut() { p.editing = None; }
                cx.notify();
            }
            _ => {}
        })];
        if let Some(p) = self.comments.as_mut() {
            p.active = Some(thread_id.to_string());
            p.replying = None;
            p.editing = Some(CommentEditInput {
                thread_id: thread_id.to_string(),
                reply_id: reply_id.to_string(),
                field,
                _subs: subs,
            });
        }
        window.focus(&focus, cx);
        cx.notify();
    }

    fn submit_comment_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((tid, rid, body)) = self.comments.as_ref().and_then(|p| {
            let e = p.editing.as_ref()?;
            Some((e.thread_id.clone(), e.reply_id.clone(), e.field.read(cx).text.clone()))
        }) else { return };
        if body.trim().is_empty() {
            return;
        }
        let now = crate::time::now();
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| {
                if e.core.edit_reply(&tid, &rid, body, now) {
                    e.changed_preserving_viewport(window, cx);
                }
            });
        }
        if let Some(p) = self.comments.as_mut() {
            p.editing = None;
        }
        cx.notify();
    }

    fn set_comment_resolved(&mut self, thread_id: &str, resolved: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| {
                if e.core.set_comment_resolved(thread_id, resolved) {
                    if resolved && e.active_comment.as_deref() == Some(thread_id) {
                        e.active_comment = None;
                    }
                    e.changed_preserving_viewport(window, cx);
                }
            });
        }
        if let Some(p) = self.comments.as_mut() {
            if p.active.as_deref() == Some(thread_id) {
                p.active = None;
            }
            if p.replying.as_ref().is_some_and(|r| r.thread_id == thread_id) {
                p.replying = None;
            }
            if p.editing.as_ref().is_some_and(|e| e.thread_id == thread_id) {
                p.editing = None;
            }
        }
        cx.notify();
    }

    fn delete_comment_thread(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| {
                if e.core.delete_comment(thread_id) {
                    if e.active_comment.as_deref() == Some(thread_id) {
                        e.active_comment = None;
                    }
                    e.changed_preserving_viewport(window, cx);
                }
            });
        }
        if let Some(p) = self.comments.as_mut() {
            if p.active.as_deref() == Some(thread_id) {
                p.active = None;
            }
            if p.replying.as_ref().is_some_and(|r| r.thread_id == thread_id) {
                p.replying = None;
            }
            if p.editing.as_ref().is_some_and(|e| e.thread_id == thread_id) {
                p.editing = None;
            }
        }
        cx.notify();
    }

    fn delete_comment_reply(&mut self, thread_id: &str, reply_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| {
                if e.core.delete_reply(thread_id, reply_id) {
                    if e.core.doc.comments.iter().all(|t| t.id != thread_id) && e.active_comment.as_deref() == Some(thread_id) {
                        e.active_comment = None;
                    }
                    e.changed_preserving_viewport(window, cx);
                }
            });
        }
        if let Some(p) = self.comments.as_mut()
            && p.editing.as_ref().is_some_and(|e| e.thread_id == thread_id && e.reply_id == reply_id)
        {
            p.editing = None;
        }
        cx.notify();
    }

    /// Esc inside the comment panel: cancels active input first, then closes.
    pub(crate) fn escape_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.comments.as_mut() else { return false };
        if p.editing.take().is_some() || p.replying.take().is_some() {
            cx.notify();
            return true;
        }
        if p.draft.is_some() {
            self.cancel_comment_draft(window, cx);
            return true;
        }
        self.close_comments(cx);
        true
    }

    /// Right-side comment panel beside the editor, below the top bar.
    pub(crate) fn render_comments(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ed = self.editor.clone()?;
        // Revalidate drawer draft anchor whenever the document changes.
        let mut draft_highlight_sync: Option<Option<CommentTarget>> = None;
        if let Some(p) = self.comments.as_mut()
            && let Some(d) = p.draft.as_mut()
        {
            let e = ed.read(cx);
            if e.core.generation != d.anchor_gen {
                match e.core.validate_comment_target(d.target, &d.quote, false) {
                    Some(rebased) => {
                        d.target = rebased;
                        d.anchor_gen = e.core.generation;
                        d.stale = false;
                        draft_highlight_sync = Some(Some(rebased));
                    }
                    None => {
                        d.stale = true;
                        draft_highlight_sync = Some(None);
                    }
                }
            }
        }
        if let Some(hl) = draft_highlight_sync {
            ed.update(cx, |e, _| e.draft_comment = hl);
        }

        let panel = self.comments.as_mut()?;
        let e = ed.read(cx);
        let read_only = e.read_only || e.preview;
        let positions = e.core.doc.comment_positions();
        let all_threads = e.core.doc.comments.clone();
        let open_count = all_threads.iter().filter(|t| !t.resolved).count();
        let resolved_count = all_threads.len() - open_count;

        let mut threads: Vec<_> = all_threads
            .into_iter()
            .filter(|t| match panel.tab {
                CommentTab::Open => !t.resolved,
                CommentTab::Resolved => t.resolved,
            })
            .collect();
        threads.sort_by_key(|t| (positions.get(&t.id).map_or(usize::MAX, |(ord, _)| *ord), t.created));

        if let Some(target_id) = panel.pending_scroll.take()
            && let Some(pos) = threads.iter().position(|t| t.id == target_id)
        {
            let offset = usize::from(panel.draft.is_some());
            panel.scroll.scroll_to_item(offset + pos);
        }
        let panel = self.comments.as_ref()?;

        let tab_btn = |id: &'static str, tab: CommentTab, label: String| {
            let active = panel.tab == tab;
            div()
                .id(id)
                .h(px(26.))
                .px(px(10.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .text_size(px(12.))
                .font_weight(if active { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                .cursor_pointer()
                .when(active, |d| d.bg(Theme::fill_hover_strong()).text_color(Theme::text()))
                .when(!active, |d| d.text_color(Theme::text_caption()).hover(|s| s.bg(Theme::fill_hover())))
                .child(label)
                .on_click(cx.listener(move |w, _, _, cx| {
                    if let Some(p) = w.comments.as_mut() {
                        p.tab = tab;
                    }
                    cx.notify();
                }))
        };

        let open_label = if open_count > 0 {
            crate::localized_format!("未解决 ({})", "Open ({})", open_count)
        } else {
            crate::settings::tr("未解决", "Open").to_string()
        };
        let resolved_label = if resolved_count > 0 {
            crate::localized_format!("已解决 ({})", "Resolved ({})", resolved_count)
        } else {
            crate::settings::tr("已解决", "Resolved").to_string()
        };

        let draft_card = panel.draft.as_ref().map(|d| {
            let quote = if d.quote.is_empty() {
                crate::settings::tr("当前段落", "Current block").to_string()
            } else {
                d.quote.clone()
            };
            let empty = d.field.read(cx).text.trim().is_empty();
            let stale = d.stale;
            div()
                .id("comment-draft")
                .mb(px(12.))
                .p(px(12.))
                .rounded(px(8.))
                .border_1()
                .border_color(if stale { Theme::danger() } else { Theme::comment_underline() })
                .bg(Theme::bg_body())
                .shadow(theme::shadow_button())
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_none().w(px(2.)).h(px(14.)).rounded(px(1.)).bg(if stale { Theme::danger() } else { Theme::comment_underline() }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
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
                            .px(px(8.))
                            .py(px(5.))
                            .rounded(px(6.))
                            .bg(Theme::fill_hover())
                            .child(div().flex_1().text_size(px(11.)).text_color(Theme::danger()).child(crate::settings::tr("原位置已变动，请重选", "Anchor moved; reselect")))
                            .child(
                                div()
                                    .id("comment-draft-rebind")
                                    .flex_none()
                                    .px(px(7.))
                                    .py(px(2.))
                                    .rounded(px(4.))
                                    .bg(Theme::primary_fill_1())
                                    .text_size(px(11.))
                                    .text_color(Theme::primary())
                                    .cursor_pointer()
                                    .child(crate::settings::tr("更新位置", "Re-anchor"))
                                    .on_click(cx.listener(|w, _, window, cx| w.rebind_comment_draft(window, cx))),
                            ),
                    )
                })
                .child(
                    div()
                        .min_h(px(52.))
                        .max_h(px(140.))
                        .px(px(8.))
                        .py(px(6.))
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
                        .justify_between()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(Theme::text_placeholder())
                                .child(crate::settings::tr("Shift+Enter 换行", "Shift+Enter newline")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    secondary_button("comment-draft-cancel", crate::settings::tr("取消", "Cancel"))
                                        .on_click(cx.listener(|w, _, window, cx| w.cancel_comment_draft(window, cx))),
                                )
                                .child(
                                    primary_button("comment-draft-post", crate::settings::tr("发送", "Post"))
                                        .when(empty || stale, |b| b.opacity(0.45))
                                        .on_click(cx.listener(|w, _, window, cx| w.submit_comment_draft(window, cx))),
                                ),
                        ),
                )
        });

        let cards: Vec<AnyElement> = threads
            .into_iter()
            .enumerate()
            .map(|(ti, thread)| {
                let tid = thread.id.clone();
                let active = panel.active.as_deref() == Some(tid.as_str());
                let orphaned = !positions.contains_key(&tid);
                let resolved = thread.resolved;
                let quote = if thread.quote.is_empty() {
                    crate::settings::tr("段落评论", "Block comment").to_string()
                } else {
                    thread.quote.clone()
                };
                let tid_select = tid.clone();
                let tid_resolve = tid.clone();
                let tid_delete = tid.clone();
                let tid_reply = tid.clone();

                let action_btn = |id: SharedString, ic: &'static str, tip: &'static str| {
                    div()
                        .id(id)
                        .size(px(22.))
                        .rounded(px(5.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|s| s.bg(Theme::fill_hover_strong()))
                        .tip(tip, None)
                        .child(icon(ic, 13., Theme::icon()))
                };

                let reply_els: Vec<AnyElement> = thread
                    .replies
                    .iter()
                    .enumerate()
                    .map(|(ri, r)| {
                        let is_editing = panel.editing.as_ref().is_some_and(|e| e.thread_id == tid && e.reply_id == r.id);
                        if is_editing && let Some(ed_state) = &panel.editing {
                            let empty = ed_state.field.read(cx).text.trim().is_empty();
                            return div()
                                .id(SharedString::from(format!("cm-edit-{ti}-{ri}")))
                                .flex()
                                .flex_col()
                                .gap(px(6.))
                                .pt(px(4.))
                                .child(
                                    div()
                                        .min_h(px(44.))
                                        .px(px(8.))
                                        .py(px(5.))
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
                                        .gap(px(6.))
                                        .child(secondary_button("cm-edit-cancel", crate::settings::tr("取消", "Cancel")).on_click(cx.listener(|w, _, _, cx| {
                                            if let Some(p) = w.comments.as_mut() { p.editing = None; }
                                            cx.notify();
                                        })))
                                        .child(primary_button("cm-edit-save", crate::settings::tr("保存", "Save")).when(empty, |b| b.opacity(0.45)).on_click(cx.listener(|w, _, window, cx| w.submit_comment_edit(window, cx)))),
                                )
                                .into_any_element();
                        }
                        let g = SharedString::from(format!("cm-reply-{ti}-{ri}"));
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
                            .when(ri > 0, |d| d.pt(px(8.)).mt(px(8.)).border_t_1().border_color(Theme::line_divider()))
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .min_w_0()
                                            .when(!r.author.is_empty(), |d| {
                                                d.child(
                                                    div()
                                                        .text_size(px(12.))
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .text_color(Theme::text())
                                                        .child(r.author.clone()),
                                                )
                                            })
                                            .when(!when.is_empty(), |d| {
                                                d.child(div().text_size(px(11.)).text_color(Theme::text_caption()).child(when))
                                            })
                                            .when(edited, |d| {
                                                d.child(
                                                    div()
                                                        .text_size(px(11.))
                                                        .text_color(Theme::text_placeholder())
                                                        .child(crate::settings::tr("已编辑", "edited")),
                                                )
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
                                                        .id(SharedString::from(format!("cm-ed-{ti}-{ri}")))
                                                        .px(px(4.))
                                                        .py(px(1.))
                                                        .rounded(px(4.))
                                                        .text_size(px(11.))
                                                        .text_color(Theme::text_caption())
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(Theme::fill_hover_strong()).text_color(Theme::text()))
                                                        .child(crate::settings::tr("编辑", "Edit"))
                                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                        .on_click(cx.listener(move |w, _, window, cx| w.start_comment_edit(&tid_e, &rid_e, &body_e, window, cx))),
                                                )
                                                .when(ri > 0, |d| {
                                                    d.child(
                                                        div()
                                                            .id(SharedString::from(format!("cm-rm-{ti}-{ri}")))
                                                            .px(px(4.))
                                                            .py(px(1.))
                                                            .rounded(px(4.))
                                                            .text_size(px(11.))
                                                            .text_color(Theme::text_caption())
                                                            .cursor_pointer()
                                                            .hover(|s| s.bg(Theme::fill_hover_strong()).text_color(Theme::danger()))
                                                            .child(crate::settings::tr("删除", "Delete"))
                                                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                            .on_click(cx.listener(move |w, _, window, cx| w.delete_comment_reply(&tid_d, &rid_d, window, cx))),
                                                    )
                                                }),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .text_size(px(13.))
                                    .line_height(px(19.))
                                    .text_color(Theme::text())
                                    .children(lines.into_iter().map(|line| {
                                        div().min_h(px(19.)).child(if line.is_empty() { " ".to_string() } else { line })
                                    })),
                            )
                            .into_any_element()
                    })
                    .collect();

                let is_replying = panel.replying.as_ref().is_some_and(|r| r.thread_id == tid);
                let reply_area = (!read_only && !resolved && (active || is_replying)).then(|| {
                    if is_replying && let Some(rep) = &panel.replying {
                        let empty = rep.field.read(cx).text.trim().is_empty();
                        div()
                            .mt(px(10.))
                            .pt(px(8.))
                            .border_t_1()
                            .border_color(Theme::line_divider())
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(
                                div()
                                    .min_h(px(44.))
                                    .max_h(px(120.))
                                    .px(px(8.))
                                    .py(px(5.))
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(Theme::line_border())
                                    .bg(crate::theme::raised(Theme::bg_body()))
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                    .child(rep.field.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap(px(6.))
                                    .child(
                                        secondary_button("cm-rep-cancel", crate::settings::tr("取消", "Cancel"))
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                            .on_click(cx.listener(|w, _, _, cx| {
                                                if let Some(p) = w.comments.as_mut() { p.replying = None; }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        primary_button("cm-rep-send", crate::settings::tr("回复", "Reply"))
                                            .when(empty, |b| b.opacity(0.45))
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                            .on_click(cx.listener(|w, _, window, cx| w.submit_comment_reply(window, cx))),
                                    ),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .id(SharedString::from(format!("cm-rep-open-{ti}")))
                            .mt(px(10.))
                            .h(px(28.))
                            .px(px(8.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(Theme::line_border())
                            .flex()
                            .items_center()
                            .text_size(px(12.))
                            .text_color(Theme::text_placeholder())
                            .cursor_pointer()
                            .hover(|s| s.bg(Theme::fill_hover()))
                            .child(crate::settings::tr("添加回复…", "Reply…"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |w, _, window, cx| w.start_comment_reply(&tid_reply, window, cx)))
                            .into_any_element()
                    }
                });

                div()
                    .id(SharedString::from(format!("cm-card-{ti}")))
                    .mb(px(10.))
                    .p(px(12.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if active { Theme::comment_underline() } else { Theme::line_border() })
                    .bg(Theme::bg_body())
                    .when(active, |d| d.shadow(theme::shadow_button()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |w, _, _, cx| w.select_comment_card(&tid_select, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(6.))
                            .mb(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(2.))
                                            .h(px(14.))
                                            .rounded(px(1.))
                                            .bg(if orphaned { Theme::text_placeholder() } else { Theme::comment_underline() }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(12.))
                                            .text_color(Theme::text_caption())
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(quote),
                                    )
                                    .when(orphaned, |d| {
                                        d.child(
                                            div()
                                                .flex_none()
                                                .px(px(5.))
                                                .py(px(1.))
                                                .rounded(px(4.))
                                                .bg(Theme::fill_hover())
                                                .text_size(px(10.))
                                                .text_color(Theme::text_placeholder())
                                                .child(crate::settings::tr("原文已删除", "Deleted")),
                                        )
                                    }),
                            )
                            .when(!read_only, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap(px(2.))
                                        .child(
                                            action_btn(
                                                SharedString::from(format!("cm-res-{ti}")),
                                                if resolved { "restore" } else { "check" },
                                                if resolved { crate::settings::tr("重新打开", "Reopen") } else { crate::settings::tr("解决", "Resolve") },
                                            )
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                            .on_click(cx.listener(move |w, _, window, cx| w.set_comment_resolved(&tid_resolve, !resolved, window, cx))),
                                        )
                                        .child(
                                            action_btn(
                                                SharedString::from(format!("cm-del-{ti}")),
                                                "bin",
                                                crate::settings::tr("删除", "Delete"),
                                            )
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                            .on_click(cx.listener(move |w, _, window, cx| w.delete_comment_thread(&tid_delete, window, cx))),
                                        ),
                                )
                            }),
                    )
                    .children(reply_els)
                    .children(reply_area)
                    .into_any_element()
            })
            .collect();

        let empty_el = (draft_card.is_none() && cards.is_empty()).then(|| {
            div()
                .pt(px(56.))
                .px(px(16.))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.))
                .child(icon("comment", 26., Theme::text_placeholder()))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(Theme::text_caption())
                        .child(match panel.tab {
                            CommentTab::Open => crate::settings::tr("暂无评论", "No comments yet"),
                            CommentTab::Resolved => crate::settings::tr("暂无已解决的评论", "No resolved comments"),
                        }),
                )
                .when(panel.tab == CommentTab::Open && !read_only, |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(Theme::text_placeholder())
                            .text_center()
                            .child(crate::settings::tr("选中文字或打开段落菜单即可添加评论", "Select text or open a block menu to add a comment")),
                    )
                })
        });

        Some(
            div()
                .id("comments-drawer")
                .flex_none()
                .w(px(DRAWER_W))
                .h_full()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(Theme::line_divider())
                .bg(crate::theme::raised(Theme::bg_body()))
                .child(
                    div()
                        .flex_none()
                        .h(px(49.))
                        .px(px(12.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(6.))
                        .border_b_1()
                        .border_color(Theme::line_divider())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .child(tab_btn("cm-tab-open", CommentTab::Open, open_label))
                                .child(tab_btn("cm-tab-resolved", CommentTab::Resolved, resolved_label)),
                        )
                        .child(
                            crate::trash::close_button("comments-close")
                                .on_click(cx.listener(|w, _, _, cx| w.close_comments(cx))),
                        ),
                )
                .child(
                    div()
                        .id("comments-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&panel.scroll)
                        .p(px(12.))
                        .children(draft_card)
                        .children(cards)
                        .children(empty_el),
                )
                .into_any_element(),
        )
    }
}
