//! 看板视图: the view's records as cards in columns, one column per option of
//! its `stack` field (单选 / 人员), 未分类 first. A card dragged onto another
//! column takes that column's option and lands where the line shows; with no
//! sort on the view it also keeps that place among the cards (the table's own
//! record order, as 表格's row drag). A click opens 记录详情.

use super::style::*;
use super::*;
use crate::feishu_theme::{c, ud};
use crate::ui::ui_text;

/// 飞书's kanban column: 256 wide, 8 apart (EMBED.md §5).
const COL_W: f32 = 256.;
/// Cards drawn per column; the rest are only counted (a long table would draw slowly).
const MAX_CARDS: usize = 200;
/// Fields listed on a card, after its title.
const CARD_FIELDS: usize = 6;

fn label(s: &str) -> String { crate::settings::label(s) }

/// A card on its way to another column, and its picture under the pointer.
#[derive(Clone)]
struct CardDrag { rec: RecId, title: SharedString }

impl Render for CardDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(COL_W - 16.)).px(px(12.)).py(px(10.)).rounded(px(6.)).bg(crate::theme::raised(Theme::bg_body())).shadow(crate::theme::shadow_menu()).opacity(0.96)
            .font_family(crate::theme::UI_FONT).text_size(px(TEXT)).text_color(Theme::text()).overflow_hidden().whitespace_nowrap().text_ellipsis()
            .child(ui_text(self.title.clone()))
    }
}

/// A value on a card or in 记录详情: `cell::value_view`, flush left.
pub(crate) fn value(base: &Base, f: &Field, v: &Value) -> AnyElement {
    match v {
        Value::Bool(b) => checkbox(*b).into_any_element(),
        Value::Number(n) if !matches!(f.ty, FieldType::Rating | FieldType::Progress) => div().w_full().line_height(px(20.)).child(ui_text(field::format_number(*n, f))).into_any_element(),
        _ => cell::value_view(base, f, v, true),
    }
}

/// The first image among the files of a record's `field` cell, as a path on disk.
pub(crate) fn cover_path(r: &Record, field: Option<FieldId>) -> Option<PathBuf> {
    let Value::Files(files) = r.get(field?) else { return None };
    let image = |a: &&Attachment| a.path.rsplit_once('.').is_some_and(|(_, ext)| matches!(ext.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"));
    files.iter().find(image).map(|a| crate::store::Store::root_dir().join(&a.path))
}

/// A card's cover: the image, cropped to fill.
pub(crate) fn cover(path: PathBuf, h: f32) -> AnyElement {
    img(path).flex_none().w_full().h(px(h)).object_fit(ObjectFit::Cover).into_any_element()
}

/// The box of a card.
pub(crate) fn card_shell(id: impl Into<ElementId>) -> Stateful<Div> {
    div().id(id).flex_none().flex().flex_col().rounded(px(6.)).bg(Theme::bg_body()).border_1().border_color(c(ud::N200)).overflow_hidden().cursor_pointer()
        .hover(|s| s.shadow(crate::theme::shadow_card()))
}

/// A card's text: the record's title, then up to `max` of the view's fields that are filled in (never `skip`),
/// the values alone, as 飞书's.
fn card_text(base: &Base, t: &Table, view: &View, r: &Record, skip: Option<FieldId>, max: usize) -> Div {
    let title = t.title(r);
    let mut d = div().w_full().px(px(12.)).py(px(10.)).flex().flex_col().gap(px(8.))
        .child(div().w_full().line_height(px(20.)).font_weight(FontWeight::MEDIUM).line_clamp(3).when(title.is_empty(), |d| d.text_color(Theme::text_placeholder()))
            .child(SharedString::from(if title.is_empty() { label("未命名记录") } else { title })));
    for f in t.visible_fields(view).into_iter().skip(1).filter(|f| Some(f.id) != skip && !r.get(f.id).is_empty()).take(max) {
        d = d.child(div().w_full().child(value(base, f, r.get(f.id))));
    }
    d
}

/// The line after the last card drawn: how many more there are.
pub(crate) fn more(n: usize) -> Div {
    div().flex_none().py(px(8.)).text_size(px(12.)).text_color(Theme::text_placeholder()).child(format!("{} {n} {}", label("还有"), label("条，使用筛选缩小范围")))
}

fn stackable(f: &&Field) -> bool { matches!(f.ty, FieldType::SingleSelect | FieldType::Person) }

pub(crate) fn render(v: &TableView, base: &Base, t: &Table, view: &View, _window: &mut Window, cx: &Context<TableView>) -> AnyElement {
    let Some(stack) = view.opts.stack.and_then(|id| t.field(id)).filter(stackable) else { return setup(t, cx) };
    let (sid, people, cover_field) = (stack.id, stack.ty == FieldType::Person, view.opts.cover);
    // the columns of `query::kanban`, filled from the rows the view has already filtered and sorted
    let mut cols: Vec<(Option<&SelectOption>, Vec<&Record>)> = std::iter::once((None, vec![])).chain(stack.opts.options.iter().map(|o| (Some(o), vec![]))).collect();
    let at: HashMap<OptId, usize> = stack.opts.options.iter().enumerate().map(|(i, o)| (o.id, i + 1)).collect();
    for r in v.result.records.iter().filter_map(|i| t.records.get(*i)) {
        let col = match r.get(sid) { Value::Select(ids) => ids.first().and_then(|id| at.get(id)).copied().unwrap_or(0), _ => 0 };
        cols[col].1.push(r);
    }
    // cards keep a place of their own only while no sort orders them
    let manual = view.opts.sorts.is_empty();
    let drop_at = if cx.has_active_drag() { v.kb_drop.filter(|_| manual) } else { None };
    let mut board = div().id("tb-kanban").flex_1().min_h_0().w_full().flex().gap(px(8.)).px(px(16.)).py(px(12.)).overflow_x_scroll().track_scroll(&v.board_scroll);
    for (ci, (opt, recs)) in cols.iter().enumerate() {
        // 未分类 only when something is in it
        if opt.is_none() && recs.is_empty() { continue; }
        let oid = opt.map(|o| o.id);
        let add = move |v: &mut TableView, window: &mut Window, cx: &mut Context<TableView>| add_card(v, sid, oid, window, cx);
        let head = div().flex_none().h(px(40.)).pl(px(12.)).pr(px(8.)).flex().items_center().gap(px(8.))
            .child(match opt {
                Some(o) if people => person(&o.name, o.color),
                Some(o) => tag(&o.name, o.color),
                None => div().flex_none().text_color(Theme::text_caption()).child(label("未分类")),
            })
            .child(div().flex_none().text_size(px(12.)).text_color(Theme::text_caption()).child(format!("{}", recs.len())))
            .child(div().flex_1())
            .child(div().id(("tb-kb-add", ci)).flex_none().size(px(24.)).rounded(px(4.)).flex().items_center().justify_center().cursor_pointer()
                .invisible().group_hover("tb-kb-col", |s| s.visible()).hover(|s| s.bg(Theme::fill_hover()))
                .child(icon("plus", 14., Theme::icon()))
                .on_click(cx.listener(move |v, _, window, cx| add(v, window, cx))));
        let mut list = div().id(("tb-kb-list", ci)).flex_1().min_h_0().overflow_y_scroll().px(px(8.)).pt(px(3.)).flex().flex_col().gap(px(4.));
        let shown = &recs[..recs.len().min(MAX_CARDS)];
        // the insertion line, in the 4px gap above the card it goes before (or under the last)
        let line_at = |before: Option<RecId>| drop_at == Some((oid, before));
        let line = || div().flex_none().h(px(2.)).mx(px(2.)).my(px(-3.)).rounded_full().bg(Theme::primary());
        for (k, r) in shown.iter().enumerate() {
            let rid = r.id;
            let next = shown.get(k + 1).map(|r| r.id);
            let title = t.title(r);
            if line_at(Some(rid)) { list = list.child(line()); }
            list = list.child(card_shell(("tb-kb-card", rid as usize))
                .children(cover_path(r, cover_field).map(|p| cover(p, 120.)))
                .child(card_text(base, t, view, r, Some(sid), CARD_FIELDS))
                .on_click(cx.listener(move |v, _, window, cx| record::open(v, rid, window, cx)))
                .on_drag(CardDrag { rec: rid, title: if title.is_empty() { label("未命名记录") } else { title }.into() }, |d, _, _, cx| cx.new(|_| d.clone()))
                // over a card: before it in its top half, after it in the bottom one
                .on_drag_move(cx.listener(move |v, ev: &DragMoveEvent<CardDrag>, _, cx| {
                    let (b, y) = (ev.bounds, ev.event.position.y);
                    if !b.contains(&ev.event.position) { return }
                    let at = Some((oid, if y < b.center().y { Some(rid) } else { next }));
                    if v.kb_drop != at { v.kb_drop = at; cx.notify(); }
                })));
        }
        if line_at(None) && !shown.is_empty() { list = list.child(line()); }
        if recs.len() > MAX_CARDS { list = list.child(more(recs.len() - MAX_CARDS).px(px(4.))); }
        // the column takes its option's colour, faintly (飞书: #f4f7ff for blue)
        let tint = match opt { Some(o) => { let mut h = tag_colors(o.color).0; h.a = 0.4; h } None => c(ud::N50) };
        board = board.child(div().id(("tb-kb-col", ci)).group("tb-kb-col").flex_none().w(px(COL_W)).flex().flex_col().rounded(px(8.)).bg(tint)
            // over the column but not a card (its head, the gap under the cards): the end of it
            .on_drag_move(cx.listener(move |v, ev: &DragMoveEvent<CardDrag>, _, cx| {
                if !ev.bounds.contains(&ev.event.position) || v.kb_drop.is_some_and(|(o, _)| o == oid) { return }
                v.kb_drop = Some((oid, None)); cx.notify();
            }))
            .on_drop(cx.listener(move |v, d: &CardDrag, _, cx| {
                let before = v.kb_drop.take().filter(|(o, _)| *o == oid).and_then(|(_, b)| b);
                move_card(v, d.rec, sid, oid, if manual { Some(before) } else { None }, cx)
            }))
            .child(head)
            .child(list)
            .child(div().id(("tb-kb-new", ci)).flex_none().h(px(36.)).mx(px(8.)).my(px(4.)).flex().items_center().justify_center().rounded(px(6.)).cursor_pointer()
                .hover(|s| s.bg(Theme::fill_hover()))
                .child(icon("plus", 15., Theme::icon()))
                .on_click(cx.listener(move |v, _, window, cx| add(v, window, cx)))));
    }
    board.into_any_element()
}

/// No field to stack by yet: say so, and offer the fields that would do.
fn setup(t: &Table, cx: &Context<TableView>) -> AnyElement {
    let fields: Vec<&Field> = t.fields.iter().filter(stackable).collect();
    let row = |id: ElementId, glyph: &'static str, name: String| div().id(id).w_full().h(px(36.)).px(px(12.)).flex().items_center().gap(px(8.)).rounded(px(6.)).border_1().border_color(line()).cursor_pointer()
        .hover(|s| s.bg(Theme::fill_hover()))
        .child(icon(glyph, 15., Theme::icon()))
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(ui_text(name)));
    let mut list = div().w(px(300.)).flex().flex_col().items_center().gap(px(8.))
        .child(icon("columns-2", 32., c(ud::N400)))
        .child(div().pb(px(8.)).text_color(Theme::text_caption()).child(label(if fields.is_empty() { "看板需要按单选字段分组，请先添加一个单选字段" } else { "选择一个单选或人员字段作为看板的分组依据" })));
    for f in &fields {
        let fid = f.id;
        list = list.child(row(("tb-kb-stack", fid).into(), field_glyph(f.ty), f.name.clone()).on_click(cx.listener(move |v, _, _, cx| v.edit_view(cx, |view| view.opts.stack = Some(fid)))));
    }
    if fields.is_empty() {
        list = list.child(row("tb-kb-field".into(), "plus", label("新增字段")).on_click(cx.listener(|v, ev: &ClickEvent, window, cx| { let p = ev.position(); menus::add_field(v, None, point(p.x - px(20.), p.y + px(22.)), window, cx); })));
    }
    div().flex_1().min_h_0().w_full().flex().items_center().justify_center().child(list).into_any_element()
}

/// A new record in the column of `opt` (None: 未分类), opened to be filled in.
fn add_card(v: &mut TableView, stack: FieldId, opt: Option<OptId>, window: &mut Window, cx: &mut Context<TableView>) {
    v.commit_edit(true, window, cx);
    let mut with = { let st = v.state.read(cx); st.doc.base.table(v.table).and_then(|t| t.view(v.view).map(|view| query::defaults_for(t, view, None))).unwrap_or_default() };
    with.retain(|(f, _)| *f != stack);
    if let Some(o) = opt { with.push((stack, Value::Select(vec![o]))); }
    let tid = v.table;
    let ids = v.edit(cx, |d| d.add_records(tid, None, 1, &with));
    if let Some(id) = ids.first() { record::open(v, *id, window, cx); }
}

/// A card dropped on the column of `opt`: its cell's first option (the one that places it) becomes that one,
/// and with `place` it moves before that record (None: after the last), one undo step for both.
fn move_card(v: &mut TableView, rec: RecId, stack: FieldId, opt: Option<OptId>, place: Option<Option<RecId>>, cx: &mut Context<TableView>) {
    let tid = v.table;
    // the record that ends the column now: "the end" is just after it, not after the whole table
    let last = place.filter(Option::is_none).and_then(|_| {
        let st = v.state.read(cx);
        let t = st.doc.base.table(tid)?;
        v.result.records.iter().filter_map(|i| t.records.get(*i)).rfind(|r| r.id != rec && opt_of(r, stack) == opt).map(|r| r.id)
    });
    v.edit(cx, |d| {
        let Some(mut ids) = d.base.table(tid).and_then(|t| t.record(rec)).map(|r| match r.get(stack) { Value::Select(ids) => ids.clone(), _ => vec![] }) else { return };
        let mut changed = false;
        if !(ids.first().copied() == opt && opt.is_some()) && !(opt.is_none() && ids.is_empty()) {
            match opt {
                // 人员 may hold several: the others stay
                Some(o) => { if !ids.is_empty() { ids.remove(0); } ids.retain(|i| *i != o); ids.insert(0, o); }
                None => ids.clear(),
            }
            changed = d.set_cells(tid, vec![(rec, stack, Value::Select(ids).normal())]);
        }
        let Some(before) = place else { return };
        let Some(t) = d.base.table(tid) else { return };
        let pos = |id: RecId| t.records.iter().position(|r| r.id == id);
        let to = match (before, last) {
            (Some(b), _) if b != rec => pos(b),
            (Some(_), _) => None,
            (None, Some(l)) => pos(l).map(|i| i + 1),
            (None, None) => None,
        };
        let Some(to) = to else { return };
        if changed { d.move_records_joined(tid, &[rec], to); } else { d.move_records(tid, &[rec], to); }
    });
}

/// The option that places a record in its column.
fn opt_of(r: &Record, stack: FieldId) -> Option<OptId> { match r.get(stack) { Value::Select(ids) => ids.first().copied(), _ => None } }
