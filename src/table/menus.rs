//! The chrome around the view: the tab bar (the file's tables, the table's
//! views), the toolbar, 表内搜索, the bar for ticked rows, and every menu
//! that is a plain list (field, cell, view, table, 统计, more). Forms and
//! panels are `fields` and `panels`; cell editors are `editors`.

use super::style::*;
use super::*;
use crate::feishu_theme::{c, ud};
use crate::ui::{popover, ui_text};

fn label(s: &str) -> String { crate::settings::label(s) }

/// Tables (when the file has several, or to make more) and the table's views.
pub(crate) fn tabs(v: &TableView, base: &Base, t: &Table, view: &View, cx: &Context<TableView>) -> AnyElement {
    let tid = t.id;
    let renaming_table = matches!(v.rename, Some(Rename::Table(id)) if id == tid);
    let mut bar = div().id("tb-tabs").flex_none().h(px(TABS_H)).flex().items_end().gap(px(2.)).px(px(8.)).bg(tabs_bg()).border_b_1().border_color(line()).overflow_hidden();
    // the table switcher
    bar = bar.child(div().id("tb-table").flex_none().h(px(30.)).mb(px(4.)).px(px(8.)).flex().items_center().gap(px(6.)).rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
        .child(icon("storage", 15., Theme::primary()))
        .child(if renaming_table { div().w(px(140.)).child(v.aux.clone()).into_any_element() }
            else { div().max_w(px(180.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(14.)).child(ui_text(t.name.clone())).into_any_element() })
        .when(base.tables.len() > 1, |d| d.child(div().text_size(px(12.)).text_color(Theme::text_placeholder()).child(format!("{}", base.tables.len()))))
        .child(icon("chevron-down", 12., Theme::text_caption()))
        .on_click(cx.listener(move |v, ev: &ClickEvent, _, cx| { if v.rename.is_some() { return; } let p = ev.position(); v.open_popup(point(p.x - px(20.), p.y + px(18.)), Popup::TableMenu(tid), cx); })));
    bar = bar.child(div().flex_none().w(px(1.)).h(px(16.)).mb(px(11.)).mx(px(4.)).bg(Theme::line_divider()));
    for vw in &t.views {
        let (vid, on) = (vw.id, vw.id == view.id);
        let renaming = matches!(v.rename, Some(Rename::View(id)) if id == vid);
        bar = bar.child(div().id(("tb-view", vid)).group("tb-view").flex_none().h(px(32.)).px(px(10.)).flex().items_center().gap(px(6.)).rounded_t(px(6.)).cursor_pointer()
            .when(on, |d| d.bg(Theme::bg_body()).border_1().border_b_0().border_color(line()))
            .when(!on, |d| d.hover(|s| s.bg(Theme::fill_hover())))
            .child(icon(view_glyph(vw.kind), 14., if on { Theme::primary() } else { Theme::icon() }))
            .child(if renaming { div().w(px(120.)).child(v.aux.clone()).into_any_element() }
                else { div().max_w(px(160.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if on { Theme::primary() } else { Theme::text() }).child(ui_text(vw.name.clone())).into_any_element() })
            .when(on && !renaming, |d| d.child(div().id(("tb-view-more", vid)).flex_none().size(px(18.)).rounded(px(4.)).flex().items_center().justify_center().hover(|s| s.bg(Theme::fill_hover_strong()))
                .child(icon("chevron-down", 12., Theme::text_caption()))
                .on_mouse_down(MouseButton::Left, cx.listener(move |v, ev: &MouseDownEvent, _, cx| { cx.stop_propagation(); v.open_popup(point(ev.position.x - px(12.), ev.position.y + px(14.)), Popup::ViewMenu(vid), cx); }))))
            .on_mouse_down(MouseButton::Right, cx.listener(move |v, ev: &MouseDownEvent, _, cx| v.open_popup(ev.position, Popup::ViewMenu(vid), cx)))
            .on_click(cx.listener(move |v, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 { v.begin_rename(Rename::View(vid), window, cx); }
                else if v.view != vid { v.show_view(vid, window, cx); }
            })));
    }
    bar = bar.child(div().id("tb-view-add").flex_none().h(px(28.)).mb(px(4.)).px(px(6.)).flex().items_center().gap(px(2.)).rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
        .child(icon("plus", 14., Theme::icon()))
        .on_click(cx.listener(|v, ev: &ClickEvent, _, cx| { let p = ev.position(); v.open_popup(point(p.x - px(12.), p.y + px(16.)), Popup::AddView, cx); })));
    bar = bar.child(div().flex_1());
    if !v.full {
        bar = bar.child(div().id("tb-expand").flex_none().h(px(28.)).mb(px(5.)).px(px(8.)).flex().items_center().gap(px(4.)).rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
            .child(icon("maximize", 14., Theme::icon())).child(div().text_color(Theme::text_caption()).child(label("全屏")))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(TableEvent::Expand))));
    } else if !v.standalone {
        bar = bar.child(div().id("tb-collapse").flex_none().h(px(28.)).mb(px(5.)).px(px(8.)).flex().items_center().gap(px(4.)).rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
            .child(icon("x", 14., Theme::icon())).child(div().text_color(Theme::text_caption()).child(label("退出全屏")))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(TableEvent::Close))));
    }
    bar.into_any_element()
}

pub(crate) fn toolbar(v: &TableView, _base: &Base, t: &Table, view: &View, cx: &Context<TableView>) -> AnyElement {
    let o = &view.opts;
    let count = |n: usize, name: &str| if n > 0 { format!("{} {n}", label(name)) } else { label(name) };
    let hidden = o.hidden.iter().filter(|f| t.field(**f).is_some()).count();
    let panel = |id: &'static str, glyph: &'static str, text: String, on: bool, kind: panels::Kind| tool_button(id, glyph, text, on)
        .on_click(cx.listener(move |v, ev: &ClickEvent, window, cx| { let p = ev.position(); panels::open(v, kind, point(p.x - px(24.), p.y + px(20.)), window, cx); }));
    let (can_undo, can_redo) = { let d = &v.state.read(cx).doc; (d.can_undo(), d.can_redo()) };
    let small = |id: &'static str, glyph: &'static str, on: bool| div().id(id).flex_none().size(px(28.)).rounded(px(6.)).flex().items_center().justify_center()
        .when(on, |d| d.cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))).child(icon(glyph, 15., if on { Theme::icon() } else { c(ud::N400) }));
    let grid = view.kind == ViewKind::Grid;
    div().id("tb-toolbar").flex_none().h(px(TOOLBAR_H)).flex().items_center().gap(px(4.)).px(px(12.)).border_b_1().border_color(line()).overflow_hidden()
        .child(div().id("tb-add").flex_none().h(px(28.)).pl(px(8.)).pr(px(10.)).flex().items_center().gap(px(4.)).rounded(px(6.)).cursor_pointer()
            .bg(Theme::primary()).hover(|s| s.bg(c(ud::B500))).child(icon("plus", 14., Theme::bg_body())).child(div().text_color(Theme::bg_body()).child(label("添加记录")))
            .on_click(cx.listener(|v, _, window, cx| v.add_record(None, window, cx))))
        .child(div().flex_none().w(px(1.)).h(px(16.)).mx(px(6.)).bg(Theme::line_divider()))
        .child(panel("tb-t-fields", "view-settings", count(hidden, "字段配置"), hidden > 0, panels::Kind::Fields))
        .child(panel("tb-t-filter", "text-search", count(o.filter.conds.len(), "筛选"), !o.filter.conds.is_empty(), panels::Kind::Filter))
        .child(panel("tb-t-group", "view-list", count(o.groups.len(), "分组"), !o.groups.is_empty(), panels::Kind::Group))
        .child(panel("tb-t-sort", "sort-down", count(o.sorts.len(), "排序"), !o.sorts.is_empty(), panels::Kind::Sort))
        .when(grid, |d| d.child(panel("tb-t-height", "list", label("行高"), false, panels::Kind::RowHeight)))
        .child(panel("tb-t-color", "palette", count(o.colors.len(), "填色"), !o.colors.is_empty(), panels::Kind::Color))
        .child(div().flex_1())
        .child(small("tb-undo", "undo", can_undo).on_click(cx.listener(|v, _, _, cx| v.undo(cx))))
        .child(small("tb-redo", "redo", can_redo).on_click(cx.listener(|v, _, _, cx| v.redo(cx))))
        .child(small("tb-find", "search", true).on_click(cx.listener(|v, _, window, cx| v.open_find(window, cx))))
        .child(small("tb-more", "ellipsis", true).on_click(cx.listener(|v, ev: &ClickEvent, _, cx| { let p = ev.position(); v.open_popup(point(p.x - px(160.), p.y + px(18.)), Popup::More, cx); })))
        .into_any_element()
}

/// 表内搜索: a box at the top right of the view, with the match count and ‹ ›.
pub(crate) fn find_bar(v: &TableView, cx: &Context<TableView>) -> Option<AnyElement> {
    let f = v.find.as_ref()?;
    let n = f.hits.len();
    let empty = v.find_input.read(cx).text.is_empty();
    let step = |id: &'static str, glyph: &'static str, by: i32| div().id(id).size(px(24.)).rounded(px(4.)).flex().items_center().justify_center().cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
        .child(icon(glyph, 13., Theme::icon())).on_click(cx.listener(move |v, _, _, cx| v.find_step(by, cx)));
    Some(div().absolute().top(px(8.)).right(px(16.)).child(popover().id("tb-findbar").occlude().h(px(40.)).w(px(320.)).px(px(10.)).flex().items_center().gap(px(6.))
        .child(icon("search", 14., Theme::text_placeholder()))
        .child(div().flex_1().min_w_0().child(v.find_input.clone()))
        .child(div().flex_none().text_size(px(12.)).text_color(Theme::text_caption()).child(if empty { String::new() } else if n == 0 { label("无结果") } else { format!("{}/{n}", f.at + 1) }))
        .child(step("tb-find-prev", "chevron-left", -1)).child(step("tb-find-next", "chevron-right", 1))
        .child(div().id("tb-find-close").size(px(24.)).rounded(px(4.)).flex().items_center().justify_center().cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
            .child(icon("x", 13., Theme::icon())).on_click(cx.listener(|v, _, window, cx| v.close_find(window, cx))))).into_any_element())
}

/// Rows are ticked: how many, and what can be done with them.
pub(crate) fn selection_bar(v: &TableView, _t: &Table, cx: &Context<TableView>) -> Option<AnyElement> {
    let n = v.rows_sel.len();
    if n == 0 { return None; }
    let btn = |id: &'static str, glyph: &'static str, text: &str, danger: bool| div().id(id).h(px(28.)).px(px(8.)).flex().items_center().gap(px(4.)).rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
        .child(icon(glyph, 14., if danger { Theme::danger() } else { Theme::icon() })).child(div().text_color(if danger { Theme::danger() } else { Theme::text() }).child(label(text)));
    Some(div().absolute().bottom(px(FOOTER_H + 16.)).left_0().right_0().flex().justify_center().child(popover().id("tb-selbar").occlude().h(px(44.)).px(px(12.)).flex().items_center().gap(px(8.))
        .child(div().text_color(Theme::text()).child(format!("{} {n} {}", label("已选"), label("条记录"))))
        .child(div().w(px(1.)).h(px(16.)).bg(Theme::line_divider()))
        .child(btn("tb-sel-dup", "copy", "创建副本", false).on_click(cx.listener(|v, _, _, cx| { let (tid, ids) = (v.table, v.rows_sel.iter().copied().collect::<Vec<_>>()); v.edit(cx, |d| d.duplicate_records(tid, &ids)); })))
        .child(btn("tb-sel-del", "bin", "删除", true).on_click(cx.listener(|v, _, _, cx| delete_rows(v, cx))))
        .child(btn("tb-sel-none", "x", "取消选择", false).on_click(cx.listener(|v, _, _, cx| { v.rows_sel.clear(); cx.notify(); })))).into_any_element())
}

/// Delete the ticked rows, or else the rows of the selected cells.
pub(crate) fn delete_rows(v: &mut TableView, cx: &mut Context<TableView>) {
    let ids = target_rows(v, None, cx);
    let tid = v.table;
    v.edit(cx, |d| d.remove_records(tid, &ids));
    v.rows_sel.clear();
    v.sel = None;
}

/// The rows an action is about: the ticked ones, else those of the cell range, else `rec`.
fn target_rows(v: &TableView, rec: Option<RecId>, cx: &App) -> Vec<RecId> {
    if !v.rows_sel.is_empty() && rec.is_none_or(|r| v.rows_sel.contains(&r)) { return v.rows_sel.iter().copied().collect(); }
    let st = v.state.read(cx);
    let from_range = st.doc.base.table(v.table).and_then(|t| t.view(v.view).map(|view| (t, v.cols(t, view)))).map(|(t, cols)| v.sel_cells(t, &cols).iter().filter_map(|row| row.first().map(|c| c.0)).collect::<Vec<_>>()).unwrap_or_default();
    match rec {
        Some(r) if !from_range.contains(&r) => vec![r],
        _ if from_range.is_empty() => rec.into_iter().collect(),
        _ => from_range,
    }
}

pub(crate) fn add_field(v: &mut TableView, at: Option<usize>, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<TableView>) {
    v.commit_edit(true, window, cx);
    let form = fields::FieldForm::add(v, at, window, cx);
    v.open_popup(pos, Popup::FieldForm(form), cx);
}

pub(crate) fn edit_field(v: &mut TableView, fid: FieldId, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<TableView>) {
    v.commit_edit(true, window, cx);
    let Some(form) = fields::FieldForm::edit(v, fid, window, cx) else { return };
    v.open_popup(pos, Popup::FieldForm(form), cx);
}

fn shell(id: &'static str, w: f32, cx: &Context<TableView>) -> Stateful<Div> {
    popover().id(id).occlude().w(px(w)).py(px(4.)).on_mouse_down_out(cx.listener(|v, _, window, cx| if v.dropdown.is_none() { v.close_popup(window, cx) }))
}

/// The open popup, hung at its position.
pub(crate) fn popup(v: &TableView, base: &Base, t: &Table, view: &View, _window: &mut Window, cx: &Context<TableView>) -> Option<AnyElement> {
    let (pos, p) = v.popup.as_ref()?;
    let el = match p {
        Popup::FieldMenu(f) => field_menu(v, t, view, *f, *pos, cx),
        Popup::FieldForm(form) => fields::render(v, base, t, form, cx),
        Popup::CellMenu(r) => cell_menu(v, t, view, *r, cx),
        Popup::Select { rec, field, editing } => editors::select_popup(v, t, *rec, *field, *editing, cx),
        Popup::Date { rec, field, year, month } => editors::date_popup(v, t, *rec, *field, *year, *month, cx),
        Popup::Links { rec, field } => editors::links_popup(v, base, t, *rec, *field, cx),
        Popup::Files { rec, field } => editors::files_popup(v, t, *rec, *field, cx),
        Popup::Panel(panel) => panels::render(v, base, t, view, panel, cx),
        Popup::ViewMenu(id) => view_menu(t, *id, cx),
        Popup::AddView => add_view_menu(cx),
        Popup::TableMenu(id) => table_menu(base, *id, cx),
        Popup::Stat(f) => stat_menu(t, view, *f, cx),
        Popup::More => more_menu(cx),
    };
    Some(deferred(anchored().position(*pos).snap_to_window_with_margin(px(8.)).child(el)).with_priority(5).into_any_element())
}

/// The list on top of a popup (a form's select boxes).
pub(crate) fn dropdown(v: &TableView, cx: &Context<TableView>) -> Option<AnyElement> {
    let d = v.dropdown.as_ref()?;
    let mut list = popover().id("tb-dropdown").occlude().w(px(d.width)).max_h(px(320.)).overflow_y_scroll().py(px(4.))
        .on_mouse_down_out(cx.listener(|v, _, _, cx| { v.dropdown = None; cx.notify(); }));
    for (i, it) in d.items.iter().enumerate() {
        let pick = d.pick.clone();
        list = list.child(div().id(("tb-drop", i)).flex_none().h(px(32.)).mx(px(4.)).px(px(8.)).flex().items_center().gap(px(8.)).rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover()))
            .when_some(it.glyph, |d, g| d.child(icon(g, 15., Theme::icon())))
            .child(match it.tag { Some(slot) => div().flex_1().min_w_0().flex().child(tag(&it.label, slot)), None => div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().when(it.on, |d| d.text_color(Theme::primary())).child(it.label.clone()) })
            .when(it.on, |d| d.child(icon("check", 14., Theme::primary())))
            .on_click(cx.listener(move |v, _, window, cx| { v.dropdown = None; pick(v, i, window, cx); cx.notify(); })));
    }
    Some(deferred(anchored().position(d.pos).snap_to_window_with_margin(px(8.)).child(list)).with_priority(6).into_any_element())
}

/// 飞书's column menu.
fn field_menu(v: &TableView, t: &Table, view: &View, fid: FieldId, pos: Point<Pixels>, cx: &Context<TableView>) -> AnyElement {
    let Some(at) = t.fields.iter().position(|f| f.id == fid) else { return div().into_any_element() };
    let cols = v.cols(t, view);
    let col = cols.index(fid).unwrap_or(0);
    let primary = at == 0;
    let frozen_here = cols.frozen == col + 1 && col > 0;
    let tid = t.id;
    let close = |v: &mut TableView, window: &mut Window, cx: &mut Context<TableView>| v.close_popup(window, cx);
    // a new field lands beside this one in the view, whatever the table's own order
    let insert = move |v: &mut TableView, after: bool, window: &mut Window, cx: &mut Context<TableView>| {
        v.popup = None;
        let form = fields::FieldForm::add(v, Some(at + after as usize), window, cx).beside(fid, after);
        v.open_popup(pos, Popup::FieldForm(form), cx);
    };
    let sort = move |v: &mut TableView, desc: bool, window: &mut Window, cx: &mut Context<TableView>| { v.edit_view(cx, |view| view.opts.sorts = vec![SortBy { field: fid, desc }]); close(v, window, cx); };
    shell("tb-field-menu", 228., cx)
        .child(menu_item("tb-fm-edit", "pen-line", label("修改字段/列"), false).on_click(cx.listener(move |v, _, window, cx| { v.popup = None; edit_field(v, fid, pos, window, cx); })))
        .child(menu_item("tb-fm-dup", "copy", label("复制字段/列"), false).on_click(cx.listener(move |v, _, window, cx| { v.edit(cx, |d| d.duplicate_field(tid, fid)); close(v, window, cx); })))
        .child(menu_rule())
        .when(!primary, |d| d.child(menu_item("tb-fm-left", "indent-decrease", label("向左插入字段/列"), false).on_click(cx.listener(move |v, _, window, cx| insert(v, false, window, cx)))))
        .child(menu_item("tb-fm-right", "indent-increase", label("向右插入字段/列"), false).on_click(cx.listener(move |v, _, window, cx| insert(v, true, window, cx))))
        .child(menu_item("tb-fm-freeze", "pin", label(if frozen_here { "取消冻结" } else { "冻结至此字段/列" }), false).on_click(cx.listener(move |v, _, window, cx| {
            v.edit_view(cx, |view| view.opts.frozen = if frozen_here { 1 } else { col + 1 });
            v.scroll.x = 0.;
            close(v, window, cx);
        })))
        .child(menu_rule())
        .child(menu_item("tb-fm-asc", "sort-down", label("按 A 到 Z 排序"), false).on_click(cx.listener(move |v, _, window, cx| sort(v, false, window, cx))))
        .child(menu_item("tb-fm-desc", "sort-down", label("按 Z 到 A 排序"), false).on_click(cx.listener(move |v, _, window, cx| sort(v, true, window, cx))))
        .child(menu_item("tb-fm-filter", "text-search", label("按此字段筛选"), false).on_click(cx.listener(move |v, _, window, cx| {
            v.edit_view(cx, |view| view.opts.filter.conds.push(Cond { field: fid, op: FilterOp::Contains, value: String::new(), date: DateRef::Exact }));
            v.popup = None;
            panels::open(v, panels::Kind::Filter, pos, window, cx);
        })))
        .child(menu_item("tb-fm-group", "view-list", label("按此字段分组"), false).on_click(cx.listener(move |v, _, window, cx| { v.edit_view(cx, |view| view.opts.groups = vec![SortBy { field: fid, desc: false }]); close(v, window, cx); })))
        .when(!primary, |d| d.child(menu_rule())
            .child(menu_item("tb-fm-hide", "eye", label("隐藏字段/列"), false).on_click(cx.listener(move |v, _, window, cx| { v.edit_view(cx, |view| { view.opts.hidden.insert(fid); }); v.col_sel = None; close(v, window, cx); })))
            .child(menu_item("tb-fm-del", "bin", label("删除字段/列"), true).on_click(cx.listener(move |v, _, window, cx| { v.edit(cx, |d| d.remove_field(tid, fid)); v.col_sel = None; close(v, window, cx); }))))
        .into_any_element()
}

/// The cell's (row's) right-click menu.
fn cell_menu(v: &TableView, t: &Table, view: &View, rec: RecId, cx: &Context<TableView>) -> AnyElement {
    let tid = t.id;
    let n = target_rows(v, Some(rec), cx).len();
    let manual = view.opts.sorts.is_empty() && view.opts.groups.is_empty();
    let insert = move |v: &mut TableView, below: bool, window: &mut Window, cx: &mut Context<TableView>| {
        let (with, first) = { let st = v.state.read(cx); let t = st.doc.base.table(tid); (t.zip(t.and_then(|t| t.view(v.view))).map(|(t, view)| query::defaults_for(t, view, None)).unwrap_or_default(), t.and_then(|t| t.view(v.view).and_then(|view| t.visible_fields(view).first().map(|f| f.id)))) };
        let ids = v.edit(cx, |d| { let at = d.base.table(tid).and_then(|t| t.index_of(rec)).map(|i| i + below as usize); d.add_records(tid, at, 1, &with) });
        if let (Some(id), Some(f)) = (ids.first(), first) { v.sel = Some(Sel { anchor: (*id, f), head: (*id, f) }); }
        v.close_popup(window, cx);
    };
    shell("tb-cell-menu", 216., cx)
        .when(manual, |d| d
            .child(menu_item("tb-cm-up", "plus", label("向上插入 1 行"), false).on_click(cx.listener(move |v, _, window, cx| insert(v, false, window, cx))))
            .child(menu_item("tb-cm-down", "plus", label("向下插入 1 行"), false).on_click(cx.listener(move |v, _, window, cx| insert(v, true, window, cx))))
            .child(menu_rule()))
        .child(menu_item("tb-cm-open", "expand", label("查看详情"), false).on_click(cx.listener(move |v, _, window, cx| { v.popup = None; record::open(v, rec, window, cx); })))
        .child(menu_item("tb-cm-copy", "copy", label("复制"), false).on_click(cx.listener(|v, _, window, cx| { keys::copy(v, false, cx); v.close_popup(window, cx); })))
        .child(menu_item("tb-cm-paste", "file-text", label("粘贴"), false).on_click(cx.listener(|v, _, window, cx| { keys::paste(v, cx); v.close_popup(window, cx); })))
        .child(menu_item("tb-cm-dup", "copy", label("创建副本"), false).on_click(cx.listener(move |v, _, window, cx| { let ids = target_rows(v, Some(rec), cx); v.edit(cx, |d| d.duplicate_records(tid, &ids)); v.close_popup(window, cx); })))
        .child(menu_rule())
        .child(menu_item("tb-cm-del", "bin", if n > 1 { format!("{} {n} {}", label("删除"), label("条记录")) } else { label("删除记录") }, true).on_click(cx.listener(move |v, _, window, cx| {
            let ids = target_rows(v, Some(rec), cx);
            v.edit(cx, |d| d.remove_records(tid, &ids));
            v.rows_sel.clear();
            v.sel = None;
            v.close_popup(window, cx);
        })))
        .into_any_element()
}

fn view_menu(t: &Table, vid: ViewId, cx: &Context<TableView>) -> AnyElement {
    let tid = t.id;
    let only = t.views.len() <= 1;
    shell("tb-view-menu", 180., cx)
        .child(menu_item("tb-vm-rename", "pen-line", label("重命名"), false).on_click(cx.listener(move |v, _, window, cx| v.begin_rename(Rename::View(vid), window, cx))))
        .child(menu_item("tb-vm-dup", "copy", label("复制视图"), false).on_click(cx.listener(move |v, _, window, cx| {
            if let Some(id) = v.edit(cx, |d| d.duplicate_view(tid, vid)) { v.show_view(id, window, cx); }
            v.close_popup(window, cx);
        })))
        .when(!only, |d| d.child(menu_rule()).child(menu_item("tb-vm-del", "bin", label("删除视图"), true).on_click(cx.listener(move |v, _, window, cx| {
            v.edit(cx, |d| d.run_in(tid, vec![Op::RemoveView(vid)]));
            v.close_popup(window, cx);
        }))))
        .into_any_element()
}

fn add_view_menu(cx: &Context<TableView>) -> AnyElement {
    let mut m = shell("tb-add-view", 200., cx).child(div().px(px(12.)).py(px(6.)).text_size(px(12.)).text_color(Theme::text_caption()).child(label("新建视图")));
    for kind in [ViewKind::Grid, ViewKind::Kanban, ViewKind::Gallery] {
        m = m.child(menu_item(SharedString::from(format!("tb-av-{}", kind.short())), view_glyph(kind), label(kind.label()), false).on_click(cx.listener(move |v, _, window, cx| {
            let tid = v.table;
            if let Some(id) = v.edit(cx, |d| d.add_view(tid, kind, "")) { v.show_view(id, window, cx); }
            v.close_popup(window, cx);
        })));
    }
    m.into_any_element()
}

fn table_menu(base: &Base, tid: TableId, cx: &Context<TableView>) -> AnyElement {
    let mut m = shell("tb-table-menu", 240., cx);
    for t in &base.tables {
        let id = t.id;
        m = m.child(menu_item(("tb-tm", id), "storage", t.name.clone(), false).when(id == tid, |d| d.child(icon("check", 14., Theme::primary())))
            .on_click(cx.listener(move |v, _, window, cx| { v.show_table(id, window, cx); v.close_popup(window, cx); })));
    }
    m.child(menu_rule())
        .child(menu_item("tb-tm-new", "plus", label("新建数据表"), false).on_click(cx.listener(|v, _, window, cx| {
            let id = v.edit(cx, |d| d.add_table(""));
            v.show_table(id, window, cx);
            v.close_popup(window, cx);
        })))
        .child(menu_item("tb-tm-import", "import", label("导入 CSV 为新数据表"), false).on_click(cx.listener(|v, _, window, cx| { v.close_popup(window, cx); keys::import_csv(v, window, cx); })))
        .child(menu_rule())
        .child(menu_item("tb-tm-rename", "pen-line", label("重命名"), false).on_click(cx.listener(move |v, _, window, cx| v.begin_rename(Rename::Table(tid), window, cx))))
        .when(base.tables.len() > 1, |d| d.child(menu_item("tb-tm-del", "bin", label("删除数据表"), true).on_click(cx.listener(move |v, _, window, cx| {
            v.edit(cx, |d| d.remove_table(tid));
            v.close_popup(window, cx);
        }))))
        .into_any_element()
}

/// 统计: what to show at the foot of a column.
fn stat_menu(t: &Table, view: &View, fid: FieldId, cx: &Context<TableView>) -> AnyElement {
    let Some(f) = t.field(fid) else { return div().into_any_element() };
    let cur = view.opts.stats.get(&fid).copied();
    let row = |id: usize, text: String, on: bool, stat: Option<Stat>| menu_item(("tb-stat-item", id), "", text, false).when(on, |d| d.child(icon("check", 14., Theme::primary())))
        .on_click(cx.listener(move |v, _, window, cx| { v.edit_view(cx, |view| { match stat { Some(s) => { view.opts.stats.insert(fid, s); } None => { view.opts.stats.remove(&fid); } } }); v.close_popup(window, cx); }));
    let mut m = shell("tb-stat-menu", 180., cx).child(row(0, label("不展示"), cur.is_none(), None));
    for (i, s) in query::stats_for(f.ty).iter().enumerate() { m = m.child(row(i + 1, label(s.label()), cur == Some(*s), Some(*s))); }
    m.into_any_element()
}

fn more_menu(cx: &Context<TableView>) -> AnyElement {
    shell("tb-more-menu", 220., cx)
        .child(menu_item("tb-mm-export", "export", label("导出为 CSV"), false).on_click(cx.listener(|v, _, window, cx| { v.close_popup(window, cx); keys::export_csv(v, window, cx); })))
        .child(menu_item("tb-mm-import", "import", label("导入 CSV 为新数据表"), false).on_click(cx.listener(|v, _, window, cx| { v.close_popup(window, cx); keys::import_csv(v, window, cx); })))
        .child(menu_item("tb-mm-md", "copy", label("复制为 Markdown 表格"), false).on_click(cx.listener(|v, _, window, cx| { keys::copy(v, true, cx); v.close_popup(window, cx); })))
        .into_any_element()
}
