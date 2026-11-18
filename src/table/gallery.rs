//! 画册视图: the view's records as a grid of cards (`kanban`'s), each under
//! its cover: the first image of the view's `cover` field, or of the table's
//! first 附件 field when none is set.

use super::style::*;
use super::*;
use crate::feishu_theme::{c, ud};

const CARD_W: f32 = 240.;
const COVER_H: f32 = 140.;
/// Cards drawn; the rest are only counted.
const MAX_CARDS: usize = 500;
const CARD_FIELDS: usize = 5;

pub(crate) fn render(v: &TableView, base: &Base, t: &Table, view: &View, _window: &mut Window, cx: &Context<TableView>) -> AnyElement {
    let attachment = |id: &FieldId| t.field(*id).is_some_and(|f| f.ty == FieldType::Attachment);
    let cover_field = view.opts.cover.filter(attachment).or_else(|| t.fields.iter().find(|f| f.ty == FieldType::Attachment).map(|f| f.id));
    let total = v.result.records.len();
    let mut grid = div().w_full().p(px(20.)).flex().flex_wrap().gap(px(16.));
    for r in v.result.records.iter().filter_map(|i| t.records.get(*i)).take(MAX_CARDS) {
        let rid = r.id;
        grid = grid.child(kanban::card_shell(("tb-gl-card", rid as usize)).w(px(CARD_W))
            // a table without 附件 has no covers at all; with one, a card without an image keeps the space
            .when(cover_field.is_some(), |d| d.child(match kanban::cover_path(r, cover_field) {
                Some(p) => kanban::cover(p, COVER_H),
                None => div().flex_none().w_full().h(px(COVER_H)).bg(c(ud::N100)).flex().items_center().justify_center().child(icon("image", 28., c(ud::N400))).into_any_element(),
            }))
            .child(kanban::card_text(base, t, view, r, None, CARD_FIELDS, true))
            .on_click(cx.listener(move |v, _, window, cx| record::open(v, rid, window, cx))));
    }
    if total > MAX_CARDS { grid = grid.child(kanban::more(total - MAX_CARDS).w_full()); }
    grid = grid.child(div().id("tb-gl-add").flex_none().w(px(CARD_W)).min_h(px(96.)).flex().items_center().justify_center().gap(px(4.)).rounded(px(6.)).border_1().border_dashed().border_color(c(ud::N400))
        .cursor_pointer().text_color(Theme::text_caption()).hover(|s| s.bg(Theme::fill_hover()))
        .child(icon("plus", 14., Theme::icon())).child(crate::settings::label("新增记录"))
        .on_click(cx.listener(|v, _, window, cx| {
            // `add_record` selects what it made: open that to be filled in
            v.sel = None;
            v.add_record(None, window, cx);
            if let Some(s) = v.sel { record::open(v, s.head.0, window, cx); }
        })));
    div().id("tb-gallery").flex_1().min_h_0().w_full().overflow_y_scroll().child(grid).into_any_element()
}

/// A view kind that has no layout yet.
pub(crate) fn unsupported(kind: ViewKind) -> AnyElement {
    div().flex_1().flex().items_center().justify_center().text_color(Theme::text_placeholder()).child(format!("{}{}", crate::settings::label(kind.label()), crate::settings::label("暂未支持"))).into_any_element()
}
