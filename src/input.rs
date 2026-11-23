//! Single-line text field used for search, link URLs and the code language
//! picker. Rendering reuses the editor's rich text element.

use crate::editor::core::{next_word, prev_word};
use crate::editor::text::{LayoutCache, RichTextElement, TextSpec};
use crate::model::{Align, BlockId, RichText};
use crate::theme::Theme;
use gpui::{prelude::FluentBuilder, *};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{cell::Cell, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    field,
    [Backspace, DeleteToHome, Delete, Left, Right, SelectLeft, SelectRight, WordLeft, WordRight, SelectWordLeft, SelectWordRight, Home, End, SelectAll, Copy, Cut, Paste, Submit, ShiftSubmit, Cancel, Up, Down, Swallow, Tab, ShiftTab]
);

pub fn bind_keys(cx: &mut App) {
    let c = Some("TextField");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("cmd-backspace", DeleteToHome, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        // ⌥← / ⌥→ move by word, ⌥⇧← / ⌥⇧→ select by word, as in any native field.
        KeyBinding::new("alt-left", WordLeft, c),
        KeyBinding::new("alt-right", WordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-left", Home, c),
        KeyBinding::new("cmd-right", End, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("enter", Submit, c),
        KeyBinding::new("shift-enter", ShiftSubmit, c),
        KeyBinding::new("escape", Cancel, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("ctrl-p", Up, c),
        KeyBinding::new("ctrl-n", Down, c),
        KeyBinding::new("tab", Tab, c),
        KeyBinding::new("shift-tab", ShiftTab, c),
        KeyBinding::new("cmd-z", Swallow, c),
        KeyBinding::new("cmd-shift-z", Swallow, c),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFieldEvent {
    Changed,
    Submit,
    /// Shift+Enter (查找: previous match).
    ShiftSubmit,
    Cancel,
    Up,
    Down,
    /// Backspace with nothing to delete (an image caption selects its block).
    BackspaceEmpty,
    /// Left with a caret at the start / Right with a caret at the end.
    LeftEdge,
    RightEdge,
    /// Tab / Shift+Tab when `tab_events` is set (moving between fields).
    Tab,
    ShiftTab,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(u64::MAX - 1_000_000);

pub struct TextField {
    focus: FocusHandle,
    pub text: String,
    sel: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    placeholder: SharedString,
    cache: LayoutCache,
    id: BlockId,
    pub size: f32,
    pub line_height: f32,
    pub placeholder_color: Hsla,
    pub color: Hsla,
    pub align: Align,
    /// Wrap long text onto more lines instead of scrolling it.
    pub wrap: bool,
    /// Enter and pasted newlines insert line breaks; Up/Down move by line.
    pub multiline: bool,
    /// When `multiline` is true, Enter submits while Shift+Enter inserts a newline.
    pub submit_on_enter: bool,
    pub mono: bool,
    /// Fonts by run (`TextSpec::font_runs`).
    pub font_runs: Option<crate::editor::text::FontRuns>,
    /// Longest text in chars (longer input is cut off).
    pub max_len: Option<usize>,
    /// Only ASCII digits get in (number inputs).
    pub digits: bool,
    /// Tab / Shift+Tab emit events instead of reaching the parent.
    pub tab_events: bool,
    /// Draw no caret even when focused (a field that takes keys before it
    /// looks focused, like the emoji search).
    pub hide_caret: bool,
    /// Mask characters visually with `•` and disable copy/cut (password / passphrase fields).
    pub masked: bool,
    /// Optional vertical size bounds for wrapped/multiline fields. When
    /// `max_height` is set, the field scrolls vertically inside that height and
    /// keeps the caret in view.
    pub min_height: Option<f32>,
    pub max_height: Option<f32>,
    selecting: bool,
    /// Typing hides the mouse pointer until it moves (see the editor's).
    pointer_hidden: bool,
    /// Horizontal scroll of a single-line field, shared with its text element.
    scroll: Rc<Cell<Pixels>>,
    /// Vertical scroll handle when `max_height` is set on a multiline field.
    vscroll: ScrollHandle,
    /// Request scrolling the caret into the vertical viewport on the next paint.
    reveal_caret: Rc<Cell<bool>>,
}

impl EventEmitter<TextFieldEvent> for TextField {}

impl Focusable for TextField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        TextField {
            focus: cx.focus_handle(),
            text: String::new(),
            sel: 0..0,
            reversed: false,
            marked: None,
            placeholder: placeholder.into(),
            cache: Default::default(),
            id: BlockId(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            // macOS's control text: 13pt.
            size: 13.,
            line_height: 20.,
            placeholder_color: Theme::text_placeholder(),
            color: Theme::text(),
            align: Align::Left,
            wrap: false,
            multiline: false,
            submit_on_enter: false,
            mono: false,
            font_runs: None,
            max_len: None,
            digits: false,
            tab_events: false,
            hide_caret: false,
            masked: false,
            min_height: None,
            max_height: None,
            selecting: false,
            pointer_hidden: false,
            scroll: Rc::new(Cell::new(px(0.))),
            vscroll: ScrollHandle::new(),
            reveal_caret: Rc::new(Cell::new(false)),
        }
    }

    pub fn set_placeholder(&mut self, s: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = s.into();
        cx.notify();
    }

    pub fn set_text(&mut self, s: &str, cx: &mut Context<Self>) {
        self.text = s.to_string();
        self.sel = self.text.len()..self.text.len();
        self.marked = None;
        self.reveal_caret.set(true);
        cx.notify();
    }

    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.sel = 0..self.text.len();
        self.reversed = false;
        cx.notify();
    }

    /// Selects `r` (byte offsets, clamped; a caret when empty).
    pub fn select_range(&mut self, r: Range<usize>, cx: &mut Context<Self>) {
        let (a, b) = (r.start.min(self.text.len()), r.end.min(self.text.len()));
        self.sel = a.min(b)..a.max(b);
        self.reversed = false;
        cx.notify();
    }

    /// The caret (the selection's moving end).
    pub fn caret(&self) -> usize { self.cursor() }

    /// The selection (byte offsets; empty: the caret).
    pub fn selection(&self) -> Range<usize> { self.sel.clone() }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    fn cursor(&self) -> usize {
        if self.reversed { self.sel.start } else { self.sel.end }
    }

    fn move_to(&mut self, o: usize, cx: &mut Context<Self>) {
        self.sel = o..o;
        self.reversed = false;
        self.reveal_caret.set(true);
        cx.notify();
    }

    fn select_to(&mut self, o: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.sel.start = o;
        } else {
            self.sel.end = o;
        }
        if self.sel.end < self.sel.start {
            self.reversed = !self.reversed;
            self.sel = self.sel.end..self.sel.start;
        }
        self.reveal_caret.set(true);
        cx.notify();
    }

    fn prev(&self, o: usize) -> usize {
        self.text.grapheme_indices(true).rev().find_map(|(i, _)| (i < o).then_some(i)).unwrap_or(0)
    }

    fn next(&self, o: usize) -> usize {
        self.text.grapheme_indices(true).find_map(|(i, _)| (i > o).then_some(i)).unwrap_or(self.text.len())
    }

    fn replace(&mut self, range: Range<usize>, s: &str, cx: &mut Context<Self>) {
        let mut s = if self.multiline { s.replace("\r\n", "\n").replace('\r', "\n") } else { s.replace(['\n', '\r'], " ") };
        if self.digits {
            s.retain(|c| c.is_ascii_digit());
        }
        if let Some(max) = self.max_len {
            let room = max.saturating_sub(self.text.chars().count() - self.text[range.clone()].chars().count());
            if let Some((cut, _)) = s.char_indices().nth(room) {
                s.truncate(cut);
            }
        }
        self.text.replace_range(range.clone(), &s);
        let o = range.start + s.len();
        self.sel = o..o;
        self.reversed = false;
        self.reveal_caret.set(true);
        cx.emit(TextFieldEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.text.is_empty() {
            cx.emit(TextFieldEvent::BackspaceEmpty);
            return;
        }
        if self.sel.is_empty() {
            let p = self.prev(self.cursor());
            self.select_to(p, cx);
        }
        self.replace(self.sel.clone(), "", cx);
    }

    /// ⌘Backspace: delete from the caret back to the start of the line, matching
    /// macOS. Single-line fields (名字、搜索) have one line, so it clears the prefix.
    fn delete_to_home(&mut self, _: &DeleteToHome, _: &mut Window, cx: &mut Context<Self>) {
        if self.sel.is_empty() {
            let start = self.text[..self.cursor()].rfind('\n').map_or(0, |i| i + 1);
            self.select_to(start, cx);
        }
        self.replace(self.sel.clone(), "", cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.sel.is_empty() {
            let n = self.next(self.cursor());
            self.select_to(n, cx);
        }
        self.replace(self.sel.clone(), "", cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.sel.is_empty() && self.cursor() == 0 {
            cx.emit(TextFieldEvent::LeftEdge);
        }
        let o = if self.sel.is_empty() { self.prev(self.cursor()) } else { self.sel.start };
        self.move_to(o, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.sel.is_empty() && self.cursor() == self.text.len() {
            cx.emit(TextFieldEvent::RightEdge);
        }
        let o = if self.sel.is_empty() { self.next(self.cursor()) } else { self.sel.end };
        self.move_to(o, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let p = self.prev(self.cursor());
        self.select_to(p, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.next(self.cursor());
        self.select_to(n, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let o = if self.sel.is_empty() { prev_word(&self.text, self.cursor()) } else { self.sel.start };
        self.move_to(o, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let o = if self.sel.is_empty() { next_word(&self.text, self.cursor()) } else { self.sel.end };
        self.move_to(o, cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let p = prev_word(&self.text, self.cursor());
        self.select_to(p, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let n = next_word(&self.text, self.cursor());
        self.select_to(n, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.multiline { line_start(&self.text, self.cursor()) } else { 0 };
        self.move_to(target, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let target = if self.multiline { line_end(&self.text, self.cursor()) } else { self.text.len() };
        self.move_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.masked {
            return;
        }
        if !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text[self.sel.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if self.masked {
            return;
        }
        if !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text[self.sel.clone()].to_string()));
            self.replace(self.sel.clone(), "", cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.replace(self.sel.clone(), &t, cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline && !self.submit_on_enter {
            return self.replace(self.sel.clone(), "\n", cx);
        }
        cx.emit(TextFieldEvent::Submit);
    }

    fn shift_submit(&mut self, _: &ShiftSubmit, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            return self.replace(self.sel.clone(), "\n", cx);
        }
        cx.emit(TextFieldEvent::ShiftSubmit);
    }

    fn swallow(&mut self, _: &Swallow, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            cx.propagate();
        }
    }

    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        match (self.tab_events, self.multiline) {
            (true, _) => cx.emit(TextFieldEvent::Tab),
            (false, false) => cx.propagate(),
            _ => {}
        }
    }

    fn shift_tab(&mut self, _: &ShiftTab, _: &mut Window, cx: &mut Context<Self>) {
        match (self.tab_events, self.multiline) {
            (true, _) => cx.emit(TextFieldEvent::ShiftTab),
            (false, false) => cx.propagate(),
            _ => {}
        }
    }

    /// Move the caret a line up (`-1`) or down; past the first/last line it
    /// goes to the start/end.
    fn move_line(&mut self, dir: f32, cx: &mut Context<Self>) {
        let target = {
            let cache = self.cache.borrow();
            let Some(l) = cache.get(&self.id) else { return };
            let at = l.position_for(self.to_disp(self.cursor()));
            let y = at.y + l.line_height * (0.5 + dir);
            let first = l.position_for(0).y;
            let last = l.position_for(self.to_disp(self.text.len())).y;
            if y < first {
                0
            } else if y > last + l.line_height {
                self.text.len()
            } else {
                self.from_disp(l.index_for(point(at.x, y)))
            }
        };
        self.move_to(target, cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextFieldEvent::Cancel);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            return self.move_line(-1., cx);
        }
        cx.emit(TextFieldEvent::Up);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            return self.move_line(1., cx);
        }
        cx.emit(TextFieldEvent::Down);
    }

    fn to_disp(&self, o: usize) -> usize {
        if self.masked { raw_to_masked(&self.text, o) } else { o.min(self.text.len()) }
    }

    fn from_disp(&self, o: usize) -> usize {
        if self.masked { masked_to_raw(&self.text, o) } else { o.min(self.text.len()) }
    }

    fn index_for(&self, p: Point<Pixels>) -> usize {
        self.cache.borrow().get(&self.id).map(|l| self.from_disp(l.index_for(p))).unwrap_or(0).min(self.text.len())
    }

    fn on_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let i = self.index_for(ev.position);
        // double click: the word under the pointer (as in any text field); triple: everything
        if ev.click_count >= 3 { self.select_all_text(cx); return; }
        if ev.click_count == 2 {
            let w = self.text.split_word_bound_indices().map(|(s, w)| s..s + w.len()).find(|r| r.contains(&i) || r.end == i && i == self.text.len());
            match w { Some(r) => self.select_range(r, cx), None => self.select_all_text(cx) }
            return;
        }
        self.selecting = true;
        if ev.modifiers.shift { self.select_to(i, cx) } else { self.move_to(i, cx) }
    }

    fn on_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting && ev.pressed_button == Some(MouseButton::Left) {
            let i = self.index_for(ev.position);
            self.select_to(i, cx);
        }
    }

    fn on_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    // utf16 helpers
    fn to16(&self, o: usize) -> usize {
        self.text[..o.min(self.text.len())].encode_utf16().count()
    }

    fn from16(&self, o16: usize) -> usize {
        let mut n = 0;
        for (i, ch) in self.text.char_indices() {
            if n >= o16 {
                return i;
            }
            n += ch.len_utf16();
        }
        self.text.len()
    }

    fn range16(&self, r: &Range<usize>) -> Range<usize> {
        self.to16(r.start)..self.to16(r.end)
    }

    fn range8(&self, r: &Range<usize>) -> Range<usize> {
        self.from16(r.start)..self.from16(r.end)
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let r = self.range8(&range);
        actual.replace(self.range16(&r));
        Some(if self.masked { "•".repeat(self.text[r].chars().count()) } else { self.text[r].to_string() })
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> { Some(self.text.encode_utf16().count()) }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range16(&self.sel), reversed: self.reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, s: &str, _: &mut Window, cx: &mut Context<Self>) {
        self.pointer_hidden = true;
        let r = range.map(|r| self.range8(&r)).or(self.marked.clone()).unwrap_or(self.sel.clone());
        self.marked = None;
        self.replace(r, s, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        s: &str,
        new_sel: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pointer_hidden = true;
        let raw = range.map(|r| self.range8(&r)).or(self.marked.clone()).unwrap_or(self.sel.clone());
        let r = clamp_range(&self.text, raw);
        let mut s = if self.multiline { s.replace("\r\n", "\n").replace('\r', "\n") } else { s.replace(['\n', '\r'], " ") };
        if self.digits {
            s.retain(|c| c.is_ascii_digit());
        }
        if let Some(max) = self.max_len {
            let room = max.saturating_sub(self.text.chars().count() - self.text[r.clone()].chars().count());
            if let Some((cut, _)) = s.char_indices().nth(room) {
                s.truncate(cut);
            }
        }
        self.text.replace_range(r.clone(), &s);
        self.marked = (!s.is_empty()).then(|| r.start..r.start + s.len());
        let sel = new_sel
            .map(|ns| {
                let base16 = self.to16(r.start);
                self.from16(base16 + ns.start)..self.from16(base16 + ns.end)
            })
            .unwrap_or(r.start + s.len()..r.start + s.len());
        self.sel = clamp_range(&self.text, sel);
        self.reversed = false;
        self.reveal_caret.set(true);
        cx.emit(TextFieldEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let r = clamp_range(&self.text, self.range8(&range));
        let cache = self.cache.borrow();
        let l = cache.get(&self.id)?;
        let mut a = l.position_for(self.to_disp(r.start));
        let mut b = l.position_for(self.to_disp(r.end));
        if self.max_height.is_some() {
            let vp = self.vscroll.bounds();
            if vp.size.height > px(0.) {
                let max_y = (vp.bottom() - l.line_height).max(vp.top());
                a.y = a.y.clamp(vp.top(), max_y);
                b.y = b.y.clamp(vp.top(), max_y);
            }
        }
        Some(Bounds::from_corners(a, point(b.x, b.y + l.line_height)))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.to16(self.index_for(p)))
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let mut spec = TextSpec::body();
        spec.doc_font = false;
        spec.size = self.size;
        spec.line_height = self.line_height;
        spec.color = self.color;
        spec.wrap = self.wrap;
        spec.mono = self.mono;
        spec.font_runs = self.font_runs.clone();
        let disp = if self.masked { "•".repeat(self.text.chars().count()) } else { self.text.clone() };
        let el = RichTextElement {
            id: self.id,
            text: RichText::plain(disp),
            spec,
            align: self.align,
            placeholder: Some(crate::settings::label(self.placeholder.clone()).into()),
            placeholder_color: Some(self.placeholder_color),
            placeholder_dx: 0.,
            selection: (focused && !self.sel.is_empty()).then(|| self.to_disp(self.sel.start)..self.to_disp(self.sel.end)),
            caret: Some(self.to_disp(self.cursor())),
            caret_row_end: false,
            fit_content: false,
            marked: self.marked.as_ref().map(|r| self.to_disp(r.start)..self.to_disp(r.end)),
            cache: self.cache.clone(),
            show_caret: focused && !self.hide_caret && self.sel.is_empty(),
            highlight: None,
            found: vec![],
            comment_highlights: vec![],
            eq_counter: 0,
            button_edit: None,
            scroll: Some(self.scroll.clone()),
        };
        let entity = cx.entity();
        let focus = self.focus.clone();
        let pointer_hidden = self.pointer_hidden;
        let scrollable = self.max_height.is_some();
        let min_h = self.min_height;
        let max_h = self.max_height;
        let vscroll = self.vscroll.clone();
        let reveal_caret = self.reveal_caret.clone();
        let cache = self.cache.clone();
        let id = self.id;
        let disp_caret = self.to_disp(self.cursor());
        let body: AnyElement = if let Some(max_h) = max_h {
            div()
                .id(("text-field-scroll", self.id.0))
                .w_full()
                .min_w_0()
                .when_some(min_h, |d, h| d.min_h(px(h)))
                .max_h(px(max_h))
                .overflow_y_scroll()
                .track_scroll(&self.vscroll)
                .child(el)
                .into_any_element()
        } else {
            el.into_any_element()
        };
        div()
            .id(("text-field", self.id.0))
            .track_focus(&self.focus)
            .key_context("TextField")
            .cursor(CursorStyle::IBeam)
            .relative()
            .when(!scrollable, |d| d.flex_1().when_some(min_h, |d, h| d.min_h(px(h))))
            .when(scrollable, |d| d.w_full())
            .min_w_0()
            .overflow_hidden()
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete_to_home))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::shift_submit))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::swallow))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::shift_tab))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_down))
            .on_mouse_move(cx.listener(Self::on_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_up))
            .text_color(self.color)
            .child(body)
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
                        if scrollable {
                            let vp = vscroll.bounds();
                            let max_y = vscroll.max_offset().y;
                            let cur_y = vscroll.offset().y;
                            let mut next_y = cur_y.clamp(-max_y, px(0.));
                            if reveal_caret.take() && vp.size.height > px(0.)
                                && let Some(l) = cache.borrow().get(&id)
                            {
                                let at = l.position_for(disp_caret);
                                next_y = vscroll_target(at.y, l.line_height, vp.top(), vp.bottom(), next_y, max_y);
                            }
                            if (next_y - cur_y).abs() > px(0.5) {
                                vscroll.set_offset(point(px(0.), next_y));
                                window.refresh();
                            }
                        }
                        if pointer_hidden {
                            cx.hide_cursor_until_mouse_moves();
                            let e = entity.clone();
                            window.on_mouse_event(move |_: &MouseMoveEvent, phase, _, cx| {
                                if phase == DispatchPhase::Bubble {
                                    e.update(cx, |f, cx| {
                                        f.pointer_hidden = false;
                                        cx.notify();
                                    });
                                }
                            });
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

fn clamp_range(text: &str, r: Range<usize>) -> Range<usize> {
    let e = text.floor_char_boundary(r.end.min(text.len()));
    let s = text.floor_char_boundary(r.start.min(e));
    s..e
}

fn line_start(text: &str, cursor: usize) -> usize {
    let c = text.floor_char_boundary(cursor.min(text.len()));
    text[..c].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, cursor: usize) -> usize {
    let c = text.floor_char_boundary(cursor.min(text.len()));
    text[c..].find('\n').map_or(text.len(), |i| c + i)
}

/// Vertical scroll offset (`<= 0`, clamped to `[-max_y, 0]`) that keeps the
/// caret line `[caret_top, caret_top + line_height]` inside `[vp_top, vp_bottom]`.
pub(crate) fn vscroll_target(
    caret_top: Pixels,
    line_height: Pixels,
    vp_top: Pixels,
    vp_bottom: Pixels,
    cur_y: Pixels,
    max_y: Pixels,
) -> Pixels {
    let max_y = max_y.max(px(0.));
    let mut next = cur_y.clamp(-max_y, px(0.));
    let caret_bottom = caret_top + line_height;
    if caret_top < vp_top {
        next = (next + (vp_top - caret_top)).clamp(-max_y, px(0.));
    } else if caret_bottom > vp_bottom {
        next = (next - (caret_bottom - vp_bottom)).clamp(-max_y, px(0.));
    }
    next
}

fn raw_to_masked(text: &str, raw_byte: usize) -> usize {
    let mut o = raw_byte.min(text.len());
    while o > 0 && !text.is_char_boundary(o) {
        o -= 1;
    }
    text[..o].chars().count() * '•'.len_utf8()
}

fn masked_to_raw(text: &str, masked_byte: usize) -> usize {
    text.char_indices().nth(masked_byte / '•'.len_utf8()).map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::{clamp_range, line_end, line_start, masked_to_raw, raw_to_masked, vscroll_target};
    use gpui::px;

    #[test]
    fn masked_offset_mapping_round_trips() {
        for s in ["abc123", "密码口令", "p@ss🔐中文!"] {
            for (byte, _) in s.char_indices() {
                let m = raw_to_masked(s, byte);
                assert_eq!(masked_to_raw(s, m), byte);
            }
            let end = raw_to_masked(s, s.len());
            assert_eq!(end, s.chars().count() * '•'.len_utf8());
            assert_eq!(masked_to_raw(s, end), s.len());
        }
    }

    #[test]
    fn vscroll_target_reveals_caret_and_clamps() {
        // Fits inside viewport: stays at 0.
        assert_eq!(vscroll_target(px(120.), px(20.), px(100.), px(220.), px(0.), px(0.)), px(0.));
        // Caret overflows bottom by 30px: scrolls down (-30px).
        assert_eq!(vscroll_target(px(230.), px(20.), px(100.), px(220.), px(0.), px(80.)), px(-30.));
        // Caret overflows top by 18px: scrolls up (+18px from -50 -> -32).
        assert_eq!(vscroll_target(px(82.), px(20.), px(100.), px(220.), px(-50.), px(80.)), px(-32.));
        // Content shrank so previous offset (-90) exceeds max_y (40): clamps to -40.
        assert_eq!(vscroll_target(px(120.), px(20.), px(100.), px(220.), px(-90.), px(40.)), px(-40.));
    }

    #[test]
    fn multiline_line_bounds_and_utf8_clamp() {
        let s = "第一行\nsecond line\n三";
        let mid = s.find("line").unwrap();
        assert_eq!(line_start(s, mid), s.find("second").unwrap());
        assert_eq!(line_end(s, mid), s.find("\n三").unwrap());
        // Clamping mid-UTF8 byte never panics and snaps to valid char boundary.
        let r = clamp_range(s, 1..999);
        assert!(s.is_char_boundary(r.start) && s.is_char_boundary(r.end));
    }
}
