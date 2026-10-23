//! Virtualized result grid.
//!
//! Only visible rows are laid out, and everything derived from the whole
//! result (column widths, geometry columns, geometry summaries) is computed
//! once per result set, so large pages with very long values stay smooth.

use crate::theme;
use crate::widgets::{self, Icon};
use eframe::egui::{self, Align, Id, Label, Layout, RichText, Sense, Stroke, TextStyle, Ui};
use egui_extras::{Column, TableBuilder};
use rgrip_core::ResultSet;
use rgrip_core::datetime::{self as dt, Temporal, TemporalKind};
use std::collections::{BTreeMap, HashMap};

/// Characters of a value laid out in a cell; a cell shows one line anyway.
const CELL_CHARS: usize = 240;
/// Characters shown in a hover tooltip.
const HOVER_CHARS: usize = 2000;

#[derive(Default)]
pub struct GridState {
    /// Selected row indexes (click, ⇧-click range, ⌘-click toggle, ⌘A).
    pub selection: std::collections::BTreeSet<usize>,
    /// Where a ⇧-click range starts.
    anchor: Option<usize>,
    /// Column of the last right-clicked cell (for cell actions in the menu).
    menu_col: Option<usize>,
    /// Frozen (pinned left) columns by name, in the order they were frozen.
    /// By name so they survive reloads and re-runs.
    pub frozen: Vec<String>,
    /// Shared vertical offset of the frozen and scrolling halves.
    scroll_y: f32,
    /// One-shot: scroll this row into view (set when the map selects a row).
    pub reveal: Option<usize>,
    /// One-shot horizontal offset for the scrolling half (screenshot scenes).
    pub scroll_x: Option<f32>,
    /// The "Go to column" popup, when open.
    goto: Option<Goto>,
    /// Screenshot scenes: open this row's right-click menu once.
    pub demo_menu: Option<usize>,
    /// Column briefly highlighted after a jump, with the time it started.
    flash: Option<(usize, f64)>,
    /// The value popup (double-click a JSON cell, or "View value").
    viewer: Option<Viewer>,
    cache: Option<Cache>,
}

struct Viewer {
    /// Column name and 1-based row number, for the title.
    column: String,
    row: usize,
    text: String,
    /// Pretty-printed, when the value is a JSON object or array.
    json: Option<String>,
}

impl Viewer {
    fn new(column: &str, row: usize, text: &str) -> Self {
        Self { column: column.to_string(), row, text: text.to_string(), json: rgrip_core::format::pretty_json(text) }
    }
}

/// The value popup: the whole value, JSON indented and colored, selectable.
fn viewer_ui(ctx: &egui::Context, viewer: &mut Option<Viewer>) {
    let Some(v) = viewer.as_ref() else { return };
    let mut close = false;
    let screen = ctx.content_rect();
    let modal = egui::Modal::new(Id::new("grid-value-viewer"))
        .frame(egui::Frame::new().fill(theme::SURFACE()).stroke(Stroke::new(1.0_f32, theme::BORDER())).corner_radius(12).inner_margin(egui::Margin::same(16)))
        .show(ctx, |ui| {
            ui.set_width((screen.width() * 0.6).clamp(420.0, 900.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new(&v.column).strong().color(theme::TEXT()));
                ui.label(RichText::new(format!("row {}{}", v.row, if v.json.is_some() { " · JSON" } else { "" })).size(12.0).color(theme::MUTED()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                    if v.json.is_some() && ui.button("Copy compact").on_hover_text("The value as stored, on one line").clicked() {
                        ui.ctx().copy_text(v.text.clone());
                    }
                    if crate::icons::menu_item(ui, crate::icons::Svg::Copy, "Copy", true).clicked() {
                        ui.ctx().copy_text(v.json.clone().unwrap_or_else(|| v.text.clone()));
                    }
                });
            });
            ui.add_space(8.0);
            egui::Frame::new().fill(theme::BG()).corner_radius(8).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                egui::ScrollArea::both().max_height((screen.height() * 0.7).max(200.0)).auto_shrink([false, true]).show(ui, |ui| {
                    let font = TextStyle::Monospace.resolve(ui.style());
                    // Coloring is per frame: skip it for huge values.
                    let job = match v.json.as_ref().filter(|j| j.len() < 200_000) {
                        Some(j) => crate::highlight::json(j, font),
                        None => {
                            let mut job = egui::text::LayoutJob::simple(v.json.clone().unwrap_or_else(|| v.text.clone()), font, theme::TEXT(), ui.available_width());
                            job.wrap.max_width = ui.available_width();
                            job
                        }
                    };
                    ui.add(Label::new(job).selectable(true));
                });
            });
        });
    if close || modal.should_close() {
        *viewer = None;
    }
}

struct Goto {
    query: String,
    highlight: usize,
    focus: bool,
}

const FLASH_SECS: f64 = 1.2;

/// Where the rows came from, for "Copy as SQL" / "Copy for AI agent".
pub struct Source<'a> {
    pub table: &'a str,
    pub sql: Option<&'a str>,
    /// Foreign keys of the table, for "Related" in the row menu.
    pub relations: &'a [rgrip_core::Relation],
    /// Matching rows per relation for the rows the menu acts on (empty: not
    /// counted yet; `None`: couldn't count).
    pub counts: &'a [Option<i64>],
}

/// Per-result-set derived data.
struct Cache {
    key: (usize, usize, usize),
    widths: Vec<f32>,
    geom_cols: Vec<usize>,
    /// (row, col) → geometry summary and WKT preview.
    geoms: HashMap<(usize, usize), (String, String)>,
}

enum Freeze {
    Add(String),
    Remove(String),
    Clear,
}

pub enum GridEvent {
    HeaderClicked(usize),
    /// A cell was edited (or set to NULL); `value` may equal the original.
    Change { row: usize, col: usize, value: Option<String> },
    /// Drop the pending change of a cell.
    Revert { row: usize, col: usize },
    /// Open the rows related to `rows` through `source.relations[relation]`.
    Follow { relation: usize, rows: Vec<usize> },
    /// The row menu opened for `rows`: a chance to count related rows.
    RowsMenu { rows: Vec<usize> },
    /// The lookup icon of a relation column: show the table it references.
    Lookup { relation: usize },
}

/// Inline editing, for table tabs whose rows have a primary key.
pub struct Editing<'a> {
    /// Pending changes: (row, col) → new value.
    pub changes: &'a BTreeMap<(usize, usize), Option<String>>,
    /// The cell being edited.
    pub editor: &'a mut Option<CellEditor>,
    /// Date/time kind of each column, for the picker.
    pub kinds: &'a [Option<TemporalKind>],
}

pub struct CellEditor {
    row: usize,
    col: usize,
    text: String,
    focus: bool,
    /// Month shown by the date picker.
    view: Option<(i32, u32)>,
}

impl CellEditor {
    /// Start editing a cell with its current (possibly pending) value.
    pub(crate) fn open(row: usize, col: usize, value: Option<&str>) -> Self {
        Self { row, col, text: value.unwrap_or_default().to_string(), focus: true, view: None }
    }
}

pub struct Sort<'a> {
    pub column: &'a str,
    pub ascending: bool,
}

const CHAR_W: f32 = 7.6;
const PAD: f32 = 10.0;

/// The first `max` chars of `s` on one line (newlines shown as ↵).
fn cell_text(s: &str, max: usize) -> String {
    let cut = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
    let head = &s[..cut];
    let mut out = if head.contains('\n') { head.replace('\n', "↵ ") } else { head.to_owned() };
    if cut < s.len() {
        out.push('…');
    }
    out
}

fn column_widths(rs: &ResultSet) -> Vec<f32> {
    rs.columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let widest = rs
                .rows
                .iter()
                .take(100)
                // Bounded scan: long values are clamped to the max width anyway.
                .map(|r| r[i].as_deref().map_or(4, |s| s.chars().take(60).take_while(|c| *c != '\n').count()))
                .max()
                .unwrap_or(0)
                .max(name.chars().count() + 2);
            (widest as f32 * CHAR_W + 2.0 * PAD + 8.0).clamp(64.0, 360.0)
        })
        .collect()
}

impl GridState {
    /// Screenshot scenes: show the Go to column popup with `query` typed.
    pub fn open_goto(&mut self, query: &str) {
        self.goto = Some(Goto { query: query.to_string(), highlight: 0, focus: true });
    }

    /// State for a new result: selection and scroll reset, frozen columns kept.
    pub fn fresh(old: &GridState) -> Self {
        Self { frozen: old.frozen.clone(), ..Default::default() }
    }

    fn cache(&mut self, rs: &ResultSet) -> &mut Cache {
        let key = (rs as *const ResultSet as usize, rs.rows.len(), rs.columns.len());
        if self.cache.as_ref().is_none_or(|c| c.key != key) {
            self.cache = Some(Cache {
                key,
                widths: column_widths(rs),
                geom_cols: rs.geometry_columns().to_vec(),
                geoms: HashMap::new(),
            });
        }
        self.cache.as_mut().unwrap()
    }
}

/// `row_offset` is added to the row-number gutter (for paged views).
pub fn show(
    ui: &mut Ui,
    id: Id,
    rs: &ResultSet,
    state: &mut GridState,
    sort: Option<Sort<'_>>,
    row_offset: usize,
    source: &Source<'_>,
    mut edit: Option<Editing<'_>>,
) -> Option<GridEvent> {
    let mut event = None;
    let row_h = ui.text_style_height(&TextStyle::Monospace) + 10.0;
    let gutter_w = (((row_offset + rs.rows.len()).max(1) as f32).log10().floor() + 1.0) * CHAR_W + 22.0;

    // Tight cells with our own padding, and barely-there column dividers.
    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
    // Resize lines only show on hover; header cells draw their own dividers.
    ui.visuals_mut().widgets.noninteractive.bg_stroke = Stroke::NONE;

    keyboard(ui, rs, state);
    // Both halves follow `scroll_y`, so setting it moves the whole grid.
    if let Some(i) = state.reveal.take() {
        state.scroll_y = reveal_offset(i, row_h);
    }
    let selection = state.selection.clone();
    let mut clicked_row = None;
    let mut menu_row = None;
    let geom = !rs.geometry_columns().is_empty();
    let mut menu_col = state.menu_col;
    // A cell to show in the value popup: (row, column).
    let mut view: Option<(usize, usize)> = None;
    // Row of the last plain click: a double-click's first click selects it,
    // so quick clicks on two rows don't count as one.
    let anchor = state.anchor;
    let demo_menu = state.demo_menu.take();
    // Frozen columns (by name) that exist in this result, in freeze order.
    let frozen: Vec<usize> = state.frozen.iter().filter_map(|n| rs.columns.iter().position(|c| c == n)).collect();
    let rest: Vec<usize> = (0..rs.columns.len()).filter(|c| !frozen.contains(c)).collect();
    let scroll_y = state.scroll_y;
    let scroll_x = state.scroll_x.take();
    let flash_state = state.flash;
    let cache = state.cache(rs);
    let cache_widths = cache.widths.clone();
    let changes = edit.as_ref().map(|e| e.changes);
    let kinds = edit.as_ref().map(|e| e.kinds);
    let mut close_editor = false;

    let mut freeze: Option<Freeze> = None;
    let mut open_goto = false;
    let mut goto_anchor: Option<egui::Rect> = None;
    let now = ui.input(|i| i.time);
    // Highlight strength of the jumped-to column, fading out.
    let flash = flash_state.map(|(c, t)| (c, (1.0 - (now - t) / FLASH_SECS).clamp(0.0, 1.0) as f32)).filter(|f| f.1 > 0.0);
    if flash.is_some() {
        ui.ctx().request_repaint();
    }
    let sortable = sort.is_some();

    // Draws one half: the frozen part (row numbers + frozen columns, no
    // horizontal scroll) or the scrolling rest. Returns its vertical offset.
    let mut draw = |ui: &mut Ui, salt: &str, cols: &[usize], gutter: bool| -> f32 {
        let ctx = ui.ctx().clone();
        let mut table = TableBuilder::new(ui)
            .id_salt(id.with(salt))
            .striped(true)
            .resizable(!gutter)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .vertical_scroll_offset(scroll_y)
            .cell_layout(Layout::left_to_right(Align::Center));
        if gutter {
            table = table
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .column(Column::exact(gutter_w));
            for &c in cols {
                table = table.column(Column::exact(cache.widths[c]).clip(true));
            }
        } else {
            for &c in cols {
                table = table.column(Column::initial(cache.widths[c]).at_least(40.0).clip(true));
            }
        }
        let out = table
            .header(row_h + 4.0, |mut header| {
                if gutter {
                    header.col(|ui| {
                        header_bg(ui);
                        let rect = ui.max_rect();
                        goto_anchor = Some(rect);
                        let r = ui.interact(rect, id.with("goto-btn"), Sense::click());
                        let tint = if r.hovered() { theme::TEXT() } else { theme::FAINT() };
                        widgets::paint_icon_tinted(ui.painter(), rect.center(), Icon::Columns, Some(tint));
                        if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Go to column  ⌘J").clicked() {
                            open_goto = true;
                        }
                    });
                }
                for &i in cols {
                    let name = &rs.columns[i];
                    header.col(|ui| {
                        header_bg(ui);
                        let rect = ui.max_rect();
                        if let Some((c, a)) = flash && c == i {
                            ui.painter().rect_filled(rect, 0.0, theme::ACCENT().gamma_multiply(0.4 * a));
                        }
                        // The whole cell takes clicks; the lookup icon, added later, sits on top.
                        let r = ui.interact(rect, id.with(("header", salt, i)), Sense::click());
                        ui.add_space(PAD);
                        // The outgoing relation this column is part of: gets a lookup icon.
                        let lookup = source.relations.iter().position(|r| r.outgoing && r.columns.iter().any(|(here, _)| here == name));
                        // Keep the right edge free for the sort indicator and lookup icon.
                        let label_w = rect.width() - PAD - if sortable { 22.0 } else { 0.0 } - if lookup.is_some() { 20.0 } else { 0.0 };
                        ui.allocate_ui_with_layout(egui::vec2(label_w.max(8.0), rect.height()), Layout::left_to_right(Align::Center), |ui| {
                            ui.add(Label::new(RichText::new(name).size(12.5).color(theme::TEXT())).truncate().selectable(false));
                        });
                        if let Some(s) = sort.as_ref() {
                            let c = egui::pos2(rect.right() - 13.0, rect.center().y);
                            if s.column == name {
                                paint_sort(ui.painter(), c, Some(s.ascending), theme::ACCENT());
                            } else if ui.rect_contains_pointer(rect) {
                                paint_sort(ui.painter(), c, None, theme::FAINT());
                            }
                        }
                        if r.clicked() {
                            event = Some(GridEvent::HeaderClicked(i));
                        }
                        if let Some(ri) = lookup {
                            let x = rect.right() - if sortable { 34.0 } else { 13.0 };
                            let icon = egui::Rect::from_center_size(egui::pos2(x, rect.center().y), egui::vec2(16.0, 16.0));
                            let hit = ui.interact(icon.expand(2.0), id.with(("lookup", salt, i)), Sense::click());
                            let tint = if hit.hovered() { theme::ACCENT() } else { theme::FAINT() };
                            crate::icons::paint(ui, crate::icons::Svg::Search, icon.shrink(1.5), tint);
                            let rel = &source.relations[ri];
                            let other = if rel.other_schema == "public" { rel.other_table.clone() } else { format!("{}.{}", rel.other_schema, rel.other_table) };
                            if hit.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Look up in {other}")).clicked() {
                                event = Some(GridEvent::Lookup { relation: ri });
                            }
                        }
                        let is_frozen = frozen.contains(&i);
                        r.context_menu(|ui| {
                        crate::app::menu_layout(ui);
                            ui.label(RichText::new(name).small().color(theme::MUTED()));
                            if is_frozen {
                                if crate::icons::menu_item(ui, crate::icons::Svg::Pin, "Unfreeze column", true).clicked() {
                                    freeze = Some(Freeze::Remove(name.clone()));
                                    ui.close();
                                }
                            } else if ui
                                .add_enabled_ui(rest.len() > 1, |ui| crate::icons::menu_item(ui, crate::icons::Svg::Pin, "Freeze column", rest.len() > 1))
                                .inner
                                .on_hover_text("Keep it on the left while scrolling sideways")
                                .clicked()
                            {
                                freeze = Some(Freeze::Add(name.clone()));
                                ui.close();
                            }
                            if crate::icons::menu_item(ui, crate::icons::Svg::Search, "Go to column…  ⌘J", true).clicked() {
                                open_goto = true;
                                ui.close();
                            }
                            ui.separator();
                            let all: Vec<usize> = (0..rs.rows.len()).collect();
                            copy_column_items(ui, rs, &all, i, "Copy column values");
                            if !frozen.is_empty() && crate::icons::menu_item(ui, crate::icons::Svg::Pin, "Unfreeze all", true).clicked() {
                                freeze = Some(Freeze::Clear);
                                ui.close();
                            }
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, rs.rows.len(), |mut row| {
                    let i = row.index();
                    row.set_selected(selection.contains(&i));
                    if gutter {
                        row.col(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add_space(8.0);
                                ui.label(RichText::new((row_offset + i + 1).to_string()).size(11.0).color(theme::FAINT()));
                            });
                        });
                    }
                    for &col in cols {
                        let pending = changes.and_then(|c| c.get(&(i, col)));
                        let kind = kinds.and_then(|k| k.get(col).copied().flatten());
                        let value = pending.map_or(rs.rows[i][col].as_deref(), |v| v.as_deref());
                        let editing = edit.as_mut().and_then(|e| e.editor.as_mut()).filter(|ed| ed.row == i && ed.col == col);
                        let (_, cell_resp) = row.col(|ui| {
                            if pending.is_some() {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, theme::ACCENT().gamma_multiply(0.16));
                            }
                            if let Some((c, a)) = flash && c == col {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, theme::ACCENT().gamma_multiply(0.16 * a));
                            }
                            ui.add_space(PAD);
                            match editing {
                                Some(ed) => {
                                    if let EditorOutcome::Close(e) = cell_editor(ui, ed, rs.rows[i][col].as_deref(), kind) {
                                        close_editor = true;
                                        if e.is_some() {
                                            event = e;
                                        }
                                    }
                                }
                                None => cell(ui, cache, i, col, value),
                            }
                        });
                        // Double-click: JSON (or any value, read-only) opens the
                        // value popup; other editable cells open the editor.
                        if cell_resp.double_clicked() && !cache.geom_cols.contains(&col) {
                            let json = value.is_some_and(|v| matches!(v.trim_start().as_bytes().first(), Some(b'{' | b'[')));
                            match edit.as_mut() {
                                Some(e) if !json => *e.editor = Some(CellEditor::open(i, col, value)),
                                _ if value.is_some() && anchor == Some(i) => view = Some((i, col)),
                                _ => {}
                            }
                        }
                        if cell_resp.secondary_clicked() {
                            menu_col = Some(col);
                        }
                    }
                    let response = row.response();
                    if !gutter && demo_menu == Some(i) {
                        menu_col = cols.get(1).or(cols.first()).copied();
                        // Context menus open at a stored pointer position.
                        let at = response.rect.left_center() + egui::vec2(180.0, 0.0);
                        #[allow(deprecated)] // the Popup API has no "open at" for context menus
                        ctx.memory_mut(|m| m.open_popup_at(egui::Popup::default_response_id(&response), at));
                        menu_row = Some(i);
                    }
                    if response.clicked() {
                        clicked_row = Some(i);
                    }
                    if response.secondary_clicked() {
                        menu_row = Some(i);
                    }
                    response.context_menu(|ui| {
                        crate::app::menu_layout(ui);
                        // The menu acts on the selection (or just this row).
                        let rows: Vec<usize> = if selection.contains(&i) { selection.iter().copied().collect() } else { vec![i] };
                        // Cell actions for the right-clicked cell.
                        if let Some(col) = menu_col.filter(|c| *c < rs.columns.len()) {
                            ui.label(RichText::new(&rs.columns[col]).small().color(theme::MUTED()));
                            let shown = changes.and_then(|c| c.get(&(i, col))).map_or(rs.rows[i][col].as_deref(), |v| v.as_deref());
                            if crate::icons::menu_item(ui, crate::icons::Svg::Copy, "Copy value", shown.is_some()).clicked() {
                                ui.ctx().copy_text(shown.unwrap_or_default().to_string());
                                ui.close();
                            }
                            if crate::icons::menu_item(ui, crate::icons::Svg::Search, "View value", shown.is_some()).on_hover_text("JSON indented; or double-click a JSON cell").clicked() {
                                view = Some((i, col));
                                ui.close();
                            }
                            if rows.len() > 1 {
                                copy_column_items(ui, rs, &rows, col, &format!("Copy {} ({} values)", rs.columns[col], rows.len()));
                            }
                            if edit.is_some() && !cache.geom_cols.contains(&col) {
                                ui.separator();
                            }
                        }
                        if let (Some(e), Some(col)) = (edit.as_mut(), menu_col.filter(|c| !cache.geom_cols.contains(c))) {
                            let pending = e.changes.get(&(i, col));
                            let value = pending.map_or(rs.rows[i][col].as_deref(), |v| v.as_deref());
                            if crate::icons::menu_item(ui, crate::icons::Svg::Pencil, "Edit cell", true).on_hover_text("Or double-click it").clicked() {
                                *e.editor = Some(CellEditor::open(i, col, value));
                                ui.close();
                            }
                            if crate::icons::menu_item(ui, crate::icons::Svg::Ban, "Set NULL", value.is_some()).clicked() {
                                event = Some(GridEvent::Change { row: i, col, value: None });
                                ui.close();
                            }
                            if pending.is_some() && crate::icons::menu_item(ui, crate::icons::Svg::Undo, "Revert cell", true).clicked() {
                                event = Some(GridEvent::Revert { row: i, col });
                                ui.close();
                            }
                            ui.separator();
                        }
                        if menu_col.is_some() {
                            ui.separator();
                        }
                        if let Some(e) = rows_menu(ui, rs, &rows, source, geom) {
                            event = Some(e);
                        }
                    });
                });
            });
        out.state.offset.y
    };

    let left_w = gutter_w + frozen.iter().map(|&c| cache_widths[c]).sum::<f32>();
    let mut offsets = [scroll_y; 2];
    ui.horizontal_top(|ui| {
        let h = ui.available_height();
        ui.allocate_ui_with_layout(egui::vec2(left_w, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(left_w);
            offsets[0] = draw(ui, "frozen", &frozen, true);
        });
        if !frozen.is_empty() {
            // Edge of the frozen block.
            let x = ui.cursor().left();
            ui.painter().vline(x, ui.max_rect().y_range(), Stroke::new(1.5_f32, theme::ACCENT().gamma_multiply(0.6)));
        }
        let mut scroll = egui::ScrollArea::horizontal().id_salt(id.with("h")).auto_shrink([false, false]);
        if let Some(x) = scroll_x {
            scroll = scroll.horizontal_scroll_offset(x);
        }
        scroll
            .show(ui, |ui| {
                // The table stacks header and body: it needs a vertical layout.
                ui.vertical(|ui| offsets[1] = draw(ui, "main", &rest, false));
            });
    });
    // Whichever half was scrolled leads; the other follows next frame.
    let moved = offsets.iter().copied().find(|y| (y - scroll_y).abs() > 0.5);
    if let Some(y) = moved {
        state.scroll_y = y;
        ui.ctx().request_repaint();
    }
    match freeze {
        Some(Freeze::Add(name)) => state.frozen.push(name),
        Some(Freeze::Remove(name)) => state.frozen.retain(|n| *n != name),
        Some(Freeze::Clear) => state.frozen.clear(),
        None => {}
    }
    state.menu_col = menu_col;
    if let Some((r, c)) = view {
        let pending = edit.as_ref().and_then(|e| e.changes.get(&(r, c)));
        let value = pending.map_or(rs.rows[r][c].as_deref(), |v| v.as_deref());
        state.viewer = value.map(|v| Viewer::new(&rs.columns[c], row_offset + r + 1, v));
    }
    viewer_ui(ui.ctx(), &mut state.viewer);
    let opened_menu = menu_row;
    if flash.is_none() {
        state.flash = None;
    }
    if open_goto && state.goto.is_none() {
        state.goto = Some(Goto { query: String::new(), highlight: 0, focus: true });
    }
    if state.goto.is_some() {
        let anchor = goto_anchor.unwrap_or_else(|| ui.min_rect());
        if let Some(col) = goto_popup(ui, id, rs, state, anchor) {
            jump(state, col, &rest, &cache_widths, now);
            ui.ctx().request_repaint();
        }
    }
    if close_editor && let Some(e) = edit.as_mut() {
        *e.editor = None;
    }
    if let Some(i) = clicked_row {
        let mods = ui.input(|inp| inp.modifiers);
        match state.anchor.filter(|_| mods.shift) {
            Some(a) => {
                state.selection = (a.min(i)..=a.max(i)).collect();
            }
            None if mods.command => {
                if !state.selection.remove(&i) {
                    state.selection.insert(i);
                }
                state.anchor = Some(i);
            }
            None => {
                state.selection = [i].into();
                state.anchor = Some(i);
            }
        }
    }
    if let Some(i) = menu_row.filter(|i| !state.selection.contains(i)) {
        state.selection = [i].into();
        state.anchor = Some(i);
    }
    if opened_menu.is_some() && event.is_none() && !source.relations.is_empty() {
        event = Some(GridEvent::RowsMenu { rows: state.selection.iter().copied().collect() });
    }
    event
}

/// Vertical offset that shows `row` a couple of rows below the top edge.
fn reveal_offset(row: usize, row_h: f32) -> f32 {
    (row as f32 - 2.0).max(0.0) * row_h
}

/// Scroll the scrolling half so `col` is near its left edge, and flash it.
fn jump(state: &mut GridState, col: usize, rest: &[usize], widths: &[f32], now: f64) {
    if let Some(p) = rest.iter().position(|&c| c == col) {
        state.scroll_x = Some(rest[..p].iter().map(|&c| widths[c]).sum());
    }
    state.flash = Some((col, now));
}

/// The "Go to column" popup. Returns the chosen column.
fn goto_popup(ui: &mut Ui, id: Id, rs: &ResultSet, state: &mut GridState, anchor: egui::Rect) -> Option<usize> {
    let ctx = ui.ctx().clone();
    let g = state.goto.as_mut()?;
    // Keys first, so the text field doesn't swallow them.
    let (up, down, enter, esc) = ctx.input_mut(|i| {
        let n = egui::Modifiers::NONE;
        (i.consume_key(n, egui::Key::ArrowUp), i.consume_key(n, egui::Key::ArrowDown), i.consume_key(n, egui::Key::Enter), i.consume_key(n, egui::Key::Escape))
    });
    if esc {
        state.goto = None;
        return None;
    }
    let mut chosen = None;
    let mut moved = false;
    let area = egui::Area::new(id.with("goto"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor.left_bottom() + egui::vec2(4.0, 2.0))
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(260.0);
                let te = ui.add(egui::TextEdit::singleline(&mut g.query).hint_text("Search column…").desired_width(f32::INFINITY));
                if g.focus {
                    te.request_focus();
                    g.focus = false;
                }
                let q = g.query.to_lowercase();
                let matches: Vec<usize> = (0..rs.columns.len()).filter(|&c| q.is_empty() || rs.columns[c].to_lowercase().contains(&q)).collect();
                if te.changed() {
                    g.highlight = 0;
                }
                if down && g.highlight + 1 < matches.len() {
                    g.highlight += 1;
                    moved = true;
                }
                if up && g.highlight > 0 {
                    g.highlight -= 1;
                    moved = true;
                }
                g.highlight = g.highlight.min(matches.len().saturating_sub(1));
                if enter {
                    chosen = matches.get(g.highlight).copied();
                }
                ui.add_space(4.0);
                if matches.is_empty() {
                    ui.label(RichText::new("No matching columns").size(12.0).color(theme::MUTED()));
                }
                egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (k, &c) in matches.iter().enumerate() {
                        let (rect, r) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), Sense::click());
                        if k == g.highlight {
                            ui.painter().rect_filled(rect, 4.0, theme::ACCENT().gamma_multiply(0.18));
                            if moved {
                                ui.scroll_to_rect(rect, None);
                            }
                        } else if r.hovered() {
                            ui.painter().rect_filled(rect, 4.0, theme::HOVER());
                        }
                        let p = ui.painter().with_clip_rect(rect.with_max_x(rect.right() - 40.0));
                        p.text(rect.left_center() + egui::vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, &rs.columns[c], egui::FontId::proportional(12.5), theme::TEXT());
                        ui.painter().text(rect.right_center() - egui::vec2(8.0, 0.0), egui::Align2::RIGHT_CENTER, format!("#{}", c + 1), egui::FontId::proportional(11.0), theme::MUTED());
                        if r.clicked() {
                            chosen = Some(c);
                        }
                    }
                });
            });
        });
    // A press outside the popup dismisses it.
    let outside = ctx.input(|i| i.pointer.any_pressed()) && ctx.pointer_interact_pos().is_some_and(|p| !area.response.rect.contains(p));
    if chosen.is_some() || outside {
        state.goto = None;
    }
    chosen
}

/// A small filled triangle: up/down when sorted, both when `asc` is None.
fn paint_sort(p: &egui::Painter, c: egui::Pos2, asc: Option<bool>, color: egui::Color32) {
    let tri = |c: egui::Pos2, up: bool, s: f32| {
        let d = if up { -1.0 } else { 1.0 };
        p.add(egui::Shape::convex_polygon(
            vec![c + egui::vec2(-s, -d * s * 0.6), c + egui::vec2(s, -d * s * 0.6), c + egui::vec2(0.0, d * s * 0.8)],
            color,
            Stroke::NONE,
        ));
    };
    match asc {
        Some(a) => tri(c, a, 4.0),
        None => {
            tri(c + egui::vec2(0.0, -3.5), true, 3.0);
            tri(c + egui::vec2(0.0, 3.5), false, 3.0);
        }
    }
}

/// Copy one column of `rows`: every value (one per line, NULL as `NULL`),
/// the distinct values, or the distinct values as an SQL list.
fn copy_column_items(ui: &mut Ui, rs: &ResultSet, rows: &[usize], col: usize, label: &str) {
    if crate::icons::menu_item(ui, crate::icons::Svg::Text, label, true).clicked() {
        ui.ctx().copy_text(column_lines(rs, rows, col));
        ui.close();
    }
    if crate::icons::menu_item(ui, crate::icons::Svg::Copy, "Copy distinct values", true).clicked() {
        ui.ctx().copy_text(column_distinct(rs, rows, col).join("\n"));
        ui.close();
    }
    if crate::icons::menu_item(ui, crate::icons::Svg::Table, "Copy as IN ('a', 'b')", true)
        .on_hover_text("Distinct values, quoted, for  where col in ('a', 'b')")
        .clicked()
    {
        ui.ctx().copy_text(column_sql_list(rs, rows, col));
        ui.close();
    }
}

fn column_lines(rs: &ResultSet, rows: &[usize], col: usize) -> String {
    rows.iter().filter_map(|&r| rs.rows.get(r)).map(|row| row[col].as_deref().unwrap_or("NULL")).collect::<Vec<_>>().join("\n")
}

/// Distinct non-NULL values, in first-seen order.
fn column_distinct<'a>(rs: &'a ResultSet, rows: &[usize], col: usize) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    rows.iter().filter_map(|&r| rs.rows.get(r)?[col].as_deref()).filter(|v| seen.insert(*v)).collect()
}

/// `('a', 'b')`: always quoted, since Postgres reads a quoted literal as
/// the column's type, so it works for text and number columns alike.
fn column_sql_list(rs: &ResultSet, rows: &[usize], col: usize) -> String {
    let items: Vec<String> = column_distinct(rs, rows, col).iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect();
    format!("({})", items.join(", "))
}

/// Copy / export actions for the given rows.
fn rows_menu(ui: &mut Ui, rs: &ResultSet, rows: &[usize], source: &Source<'_>, geom: bool) -> Option<GridEvent> {
    use rgrip_core::export::{self, Format};
    let n = rows.len();
    let subset = || export::subset(rs, rows);
    let noun = if n == 1 { "row".to_string() } else { format!("{n} rows") };
    let mut event = None;
    if !source.relations.is_empty() {
        crate::icons::submenu(ui, crate::icons::Svg::GitBranch, "Related", |ui| {
            for (i, r) in source.relations.iter().enumerate() {
                let cols: Vec<&str> = r.columns.iter().map(|(here, there)| if r.outgoing { here.as_str() } else { there.as_str() }).collect();
                let other = if r.other_schema == "public" { r.other_table.clone() } else { format!("{}.{}", r.other_schema, r.other_table) };
                let (arrow, hover) = if r.outgoing { ("→", "Rows these rows reference") } else { ("←", "Rows that reference these rows") };
                let mut label = format!("{arrow} {other}  ({})", cols.join(", "));
                let mut hover = format!("{hover}\n{}", r.name);
                if r.inferred {
                    label.push_str("  ~ inferred");
                    hover.push_str("\nGuessed from column names (no foreign key constraint)");
                }
                // Counted matches for these rows; a guess with none is dimmed.
                let count = source.counts.get(i).copied().flatten();
                match count {
                    Some(n) if n >= crate::worker::PROBE_CAP as i64 => label.push_str(&format!("  · {}+", crate::worker::PROBE_CAP)),
                    Some(n) => label.push_str(&format!("  · {n}")),
                    None => {}
                }
                let text = if count == Some(0) { RichText::new(label).color(theme::FAINT()) } else { RichText::new(label) };
                if ui.button(text).on_hover_text(hover).clicked() {
                    event = Some(GridEvent::Follow { relation: i, rows: rows.to_vec() });
                    ui.close();
                }
            }
        });
        ui.separator();
    }
    if crate::icons::menu_item(ui, crate::icons::Svg::Copy, format!("Copy {noun}"), true).on_hover_text("Tab-separated  ⌘C").clicked() {
        ui.ctx().copy_text(rows_tsv(rs, rows, false));
        ui.close();
    }
    if crate::icons::menu_item(ui, crate::icons::Svg::Text, "Copy with header", true).clicked() {
        ui.ctx().copy_text(rows_tsv(rs, rows, true));
        ui.close();
    }
    if crate::icons::menu_item(ui, crate::icons::Svg::Image, "Copy as image", true).on_hover_text("PNG of the table: paste into Telegram, Slack…").clicked() {
        crate::snapshot::copy_table(ui.ctx(), &subset(), source.table);
        ui.close();
    }
    crate::icons::submenu(ui, crate::icons::Svg::Copy, "Copy as", |ui| {
        let item = |ui: &mut Ui, label: &str, text: &dyn Fn() -> String| {
            if ui.button(label).clicked() {
                ui.ctx().copy_text(text());
                ui.close();
            }
        };
        item(ui, "CSV", &|| export::export(&subset(), Format::Csv, source.table).trim_start_matches('\u{FEFF}').to_string());
        item(ui, "JSON", &|| export::export(&subset(), Format::Json, source.table));
        item(ui, "Markdown", &|| export::export(&subset(), Format::Markdown, source.table));
        if geom {
            item(ui, "GeoJSON", &|| export::export(&subset(), Format::GeoJson, source.table));
        }
        ui.separator();
        item(ui, "SQL INSERT", &|| export::export(&subset(), Format::SqlInsert, source.table));
        item(ui, "SQL  select … where id in (…)", &|| export::where_in(&subset(), source.table));
    });
    if crate::icons::menu_item(ui, crate::icons::Svg::Bot, "Copy for AI agent", true)
        .on_hover_text("Markdown with the query, column types and rows: paste into an agent")
        .clicked()
    {
        ui.ctx().copy_text(export::for_agent(&subset(), source.table, source.sql));
        ui.close();
    }
    ui.separator();
    crate::icons::submenu(ui, crate::icons::Svg::Download, &format!("Save {noun} as"), |ui| {
        for f in Format::ALL.into_iter().filter(|f| *f != Format::GeoJson || geom) {
            if ui.button(format!("{}…", f.label())).clicked() {
                ui.close();
                let name = source.table.rsplit('.').next().unwrap_or("rows");
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(format!("{name}.{}", f.extension()))
                    .add_filter(f.label(), &[f.extension()])
                    .save_file()
                {
                    let _ = std::fs::write(path, export::export(&subset(), f, source.table));
                }
            }
        }
    });
    event
}

fn rows_tsv(rs: &ResultSet, rows: &[usize], header: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    if header {
        lines.push(rs.columns.join("\t"));
    }
    lines.extend(rows.iter().filter_map(|&i| rs.rows.get(i)).map(|r| row_tsv(r)));
    lines.join("\n")
}

fn cell(ui: &mut Ui, cache: &mut Cache, row: usize, col: usize, value: Option<&str>) {
    let Some(s) = value else {
        ui.add(Label::new(RichText::new("NULL").italics().color(theme::FAINT())).selectable(false));
        return;
    };
    if cache.geom_cols.contains(&col) {
        // Hex EWKB is unreadable: show a summary, WKT on hover.
        let (summary, wkt) = cache.geoms.entry((row, col)).or_insert_with(|| match rgrip_core::geo::parse_hex(s) {
            Some(g) => (g.summary(), cell_text(&g.geometry.to_wkt(), HOVER_CHARS)),
            None => (cell_text(s, CELL_CHARS), String::new()),
        });
        let r = ui.add(Label::new(RichText::new(summary.as_str()).monospace().color(theme::GEO())).truncate().selectable(false));
        if !wkt.is_empty() {
            let wkt = wkt.clone();
            r.on_hover_ui(|ui| {
                ui.label(RichText::new(wkt).monospace().size(11.5));
            });
        }
        return;
    }
    let text = cell_text(s, CELL_CHARS);
    let r = if crate::shaping::is_complex(&text) {
        // Khmer needs real shaping, which egui's text layout lacks.
        crate::shaping::label(ui, &text, 13.5, theme::TEXT())
    } else {
        ui.add(Label::new(RichText::new(text).monospace()).truncate().selectable(false))
    };
    if s.len() > 40 || s.contains('\n') {
        // Built only while hovered.
        r.on_hover_ui(|ui| {
            ui.set_max_width(520.0);
            ui.label(cell_text(s, HOVER_CHARS));
        });
    }
}

/// The in-cell text field. Enter (or clicking away) keeps the edit, Esc
/// drops it; either way the editor closes.
fn cell_editor(ui: &mut Ui, ed: &mut CellEditor, original: Option<&str>, kind: Option<TemporalKind>) -> EditorOutcome {
    let out = egui::TextEdit::singleline(&mut ed.text)
        .font(TextStyle::Monospace)
        .frame(false)
        .margin(egui::Margin::ZERO)
        .desired_width(f32::INFINITY)
        .show(ui);
    let r = out.response;
    if std::mem::take(&mut ed.focus) {
        r.request_focus();
        // Select everything so typing replaces the value.
        let mut state = out.state;
        let n = ed.text.chars().count();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
        state.store(ui.ctx(), r.id);
    }
    let cell = ui.max_rect();
    ui.painter().rect_stroke(cell.shrink(1.0), 2.0, Stroke::new(1.0_f32, theme::ACCENT()), egui::StrokeKind::Inside);
    // An empty edit of a NULL keeps it NULL.
    let commit = |ed: &CellEditor| {
        let value = if ed.text.is_empty() && original.is_none() { None } else { Some(ed.text.clone()) };
        EditorOutcome::Close(Some(GridEvent::Change { row: ed.row, col: ed.col, value }))
    };
    let Some(kind) = kind else {
        if !r.lost_focus() {
            return EditorOutcome::Editing;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return EditorOutcome::Close(None);
        }
        return commit(ed);
    };
    // Date/time: the picker takes clicks, so losing text focus doesn't end
    // the edit. Enter / Done / a click elsewhere keeps it, Esc drops it.
    let (popup, done) = picker(ui, cell, ed, kind);
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        return EditorOutcome::Close(None);
    }
    let clicked_away = ui.input(|i| {
        i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !cell.contains(p) && !popup.contains(p))
    });
    if done || clicked_away || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        return commit(ed);
    }
    EditorOutcome::Editing
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// Local wall-clock now and the local UTC offset, in seconds.
fn now_local() -> (i64, i64) {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    #[cfg(unix)]
    {
        let t = unix as libc::time_t;
        let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: localtime_r writes into `tm` and returns null on failure.
        if !unsafe { libc::localtime_r(&t, tm.as_mut_ptr()) }.is_null() {
            #[allow(clippy::unnecessary_cast)] // `c_long` isn't i64 everywhere
            return (unix, unsafe { tm.assume_init() }.tm_gmtoff as i64);
        }
    }
    (unix, 0)
}

/// Calendar and/or time fields under the cell. Edits `ed.text`; returns the
/// popup's rect and whether Done was clicked.
fn picker(ui: &Ui, cell: egui::Rect, ed: &mut CellEditor, kind: TemporalKind) -> (egui::Rect, bool) {
    let (unix, offset) = now_local();
    let now = dt::from_unix(unix, offset);
    let parsed = dt::parse(kind, &ed.text);
    // Unparseable text (empty, `infinity`, …): start from now.
    let mut t = parsed.clone().unwrap_or_else(|| Temporal { frac: String::new(), ..now.clone() });
    if parsed.is_none() && !kind.has_tz() {
        t.tz.clear();
    }
    let (mut vy, mut vm) = ed.view.unwrap_or((t.year, t.month));
    let mut changed = false;
    let mut done = false;
    let area = egui::Area::new(Id::new(("cell-picker", ed.row, ed.col)))
        .order(egui::Order::Foreground)
        .fixed_pos(cell.left_bottom() + egui::vec2(0.0, 2.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                if kind.has_date() {
                    ui.horizontal(|ui| {
                        if ui.small_button("‹").clicked() {
                            (vy, vm) = if vm == 1 { (vy - 1, 12) } else { (vy, vm - 1) };
                        }
                        let title = format!("{} {vy}", MONTHS[vm as usize - 1]);
                        ui.add_sized([150.0, 18.0], Label::new(RichText::new(title).strong()));
                        if ui.small_button("›").clicked() {
                            (vy, vm) = if vm == 12 { (vy + 1, 1) } else { (vy, vm + 1) };
                        }
                    });
                    egui::Grid::new("days").spacing([2.0, 2.0]).show(ui, |ui| {
                        for d in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] {
                            ui.add_sized([26.0, 16.0], Label::new(RichText::new(d).small().color(theme::FAINT())));
                        }
                        ui.end_row();
                        let lead = dt::weekday(vy, vm, 1);
                        for _ in 0..lead {
                            ui.label("");
                        }
                        for d in 1..=dt::days_in_month(vy, vm) {
                            let selected = parsed.is_some() && (t.year, t.month, t.day) == (vy, vm, d);
                            let today = (now.year, now.month, now.day) == (vy, vm, d);
                            let mut b = egui::Button::new(RichText::new(d.to_string()).size(12.0)).selected(selected).min_size(egui::vec2(26.0, 22.0));
                            if today && !selected {
                                b = b.stroke(Stroke::new(1.0_f32, theme::ACCENT()));
                            }
                            if ui.add(b).clicked() {
                                (t.year, t.month, t.day) = (vy, vm, d);
                                changed = true;
                            }
                            if (lead + d) % 7 == 0 {
                                ui.end_row();
                            }
                        }
                    });
                }
                if kind.has_time() {
                    ui.horizontal(|ui| {
                        let two = |n: f64, _| format!("{:02}", n as u32);
                        changed |= ui.add(egui::DragValue::new(&mut t.hour).range(0..=23).custom_formatter(two)).changed();
                        ui.label(":");
                        changed |= ui.add(egui::DragValue::new(&mut t.minute).range(0..=59).custom_formatter(two)).changed();
                        ui.label(":");
                        changed |= ui.add(egui::DragValue::new(&mut t.second).range(0..=59).custom_formatter(two)).changed();
                        if kind.has_tz() && !t.tz.is_empty() {
                            ui.label(RichText::new(&t.tz).color(theme::MUTED()));
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let label = if kind.has_time() { "Now" } else { "Today" };
                    if ui.button(label).clicked() {
                        t = Temporal { frac: String::new(), ..now.clone() };
                        (vy, vm) = (t.year, t.month);
                        changed = true;
                    }
                    let fill = egui::Button::new(RichText::new("Done").color(theme::BG())).fill(theme::ACCENT());
                    done = ui.add(fill).on_hover_text("Enter").clicked();
                });
            });
        });
    if changed {
        ed.text = dt::format(kind, &t);
    }
    ed.view = Some((vy, vm));
    (area.response.rect, done)
}

enum EditorOutcome {
    Editing,
    /// Close the editor, with the change to apply (none when cancelled).
    Close(Option<GridEvent>),
}

/// ⌘C copies the selected rows and ⌘A selects all, unless a text field has focus.
fn keyboard(ui: &Ui, rs: &ResultSet, state: &mut GridState) {
    if ui.ctx().memory(|m| m.focused().is_some()) {
        return;
    }
    let hovered = ui.ui_contains_pointer();
    if (hovered || !state.selection.is_empty()) && state.goto.is_none() && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::J)) {
        state.goto = Some(Goto { query: String::new(), highlight: 0, focus: true });
    }
    if (hovered || !state.selection.is_empty()) && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
        state.selection = (0..rs.rows.len()).collect();
    }
    if !state.selection.is_empty() && ui.input(|inp| inp.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
        let rows: Vec<usize> = state.selection.iter().copied().filter(|&i| i < rs.rows.len()).collect();
        ui.ctx().copy_text(rows_tsv(rs, &rows, false));
    }
}

fn row_tsv(row: &[Option<String>]) -> String {
    row.iter()
        .map(|v| v.as_deref().unwrap_or("NULL"))
        .collect::<Vec<_>>()
        .join("\t")
}

fn header_bg(ui: &mut Ui) {
    let r = ui.max_rect();
    let p = ui.painter();
    p.rect_filled(r, 0.0, theme::BG());
    p.hline(r.x_range(), r.bottom(), Stroke::new(1.0_f32, theme::BORDER()));
    p.vline(r.right(), r.y_range().shrink(7.0), Stroke::new(1.0_f32, theme::BORDER()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the grid one frame with `events`; returns the selection.
    fn frame(ctx: &egui::Context, rs: &ResultSet, state: &mut GridState, events: Vec<egui::Event>) {
        frame_mods(ctx, rs, state, events, egui::Modifiers::NONE);
    }

    fn frame_mods(ctx: &egui::Context, rs: &ResultSet, state: &mut GridState, events: Vec<egui::Event>, modifiers: egui::Modifiers) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
            events,
            modifiers,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(ui, Id::new("g"), rs, state, None, 0, &Source { table: "t", sql: None, relations: &[], counts: &[] }, None);
            });
        });
    }

    fn click(ctx: &egui::Context, rs: &ResultSet, state: &mut GridState, row: usize, modifiers: egui::Modifiers) {
        // Header ~30pt, rows ~25pt: aim at the middle of `row` in the first column.
        let h = ctx.style().text_styles[&TextStyle::Monospace].size;
        let row_h = ctx.fonts_mut(|f| f.row_height(&egui::FontId::monospace(h))) + 10.0;
        let pos = egui::pos2(120.0, 8.0 + row_h + 4.0 + row_h * (row as f32 + 0.5));
        let press = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers };
        frame_mods(ctx, rs, state, vec![egui::Event::PointerMoved(pos)], modifiers);
        frame_mods(ctx, rs, state, vec![press(true)], modifiers);
        frame_mods(ctx, rs, state, vec![press(false)], modifiers);
    }

    #[test]
    fn multi_row_selection() {
        let rs = ResultSet {
            columns: vec!["id".into(), "name".into()],
            rows: (0..10).map(|i| vec![Some(i.to_string()), Some(format!("row {i}"))]).collect(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut st = GridState::default();
        frame(&ctx, &rs, &mut st, vec![]);
        click(&ctx, &rs, &mut st, 1, egui::Modifiers::NONE);
        assert_eq!(st.selection.iter().copied().collect::<Vec<_>>(), [1]);
        click(&ctx, &rs, &mut st, 4, egui::Modifiers::SHIFT);
        assert_eq!(st.selection.iter().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
        click(&ctx, &rs, &mut st, 2, egui::Modifiers::COMMAND);
        assert_eq!(st.selection.iter().copied().collect::<Vec<_>>(), [1, 3, 4]);
        click(&ctx, &rs, &mut st, 6, egui::Modifiers::NONE);
        assert_eq!(st.selection.iter().copied().collect::<Vec<_>>(), [6]);
        let cmd_a = egui::Event::Key { key: egui::Key::A, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND };
        frame_mods(&ctx, &rs, &mut st, vec![cmd_a], egui::Modifiers::COMMAND);
        assert_eq!(st.selection.len(), 10);
        assert_eq!(rows_tsv(&rs, &[1, 3], true), "id\tname\n1\trow 1\n3\trow 3");
    }

    /// One frame with inline editing on; returns the grid's event.
    fn frame_edit(
        ctx: &egui::Context,
        rs: &ResultSet,
        state: &mut GridState,
        changes: &BTreeMap<(usize, usize), Option<String>>,
        editor: &mut Option<CellEditor>,
        events: Vec<egui::Event>,
        time: f64,
        kinds: &[Option<TemporalKind>],
    ) -> Option<GridEvent> {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
            events,
            time: Some(time),
            ..Default::default()
        };
        let mut out = None;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let edit = Some(Editing { changes, editor: &mut *editor, kinds });
                out = show(ui, Id::new("g"), rs, state, None, 0, &Source { table: "t", sql: None, relations: &[], counts: &[] }, edit);
            });
        });
        out
    }

    #[test]
    fn inline_edit() {
        let rs = ResultSet {
            columns: vec!["id".into(), "name".into()],
            rows: (0..5).map(|i| vec![Some(i.to_string()), Some(format!("row {i}"))]).collect(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut st = GridState::default();
        let changes = BTreeMap::new();
        let mut editor = None;
        let mut t = 0.0;
        let mut step = |events: Vec<egui::Event>, editor: &mut Option<CellEditor>, st: &mut GridState| {
            t += 0.05;
            frame_edit(&ctx, &rs, st, &changes, editor, events, t, &[None, None])
        };
        step(vec![], &mut editor, &mut st);
        // Double-click row 2's "name" cell (x = 120 is past the gutter and id column).
        let h = ctx.style().text_styles[&TextStyle::Monospace].size;
        let row_h = ctx.fonts_mut(|f| f.row_height(&egui::FontId::monospace(h))) + 10.0;
        let pos = egui::pos2(120.0, 8.0 + row_h + 4.0 + row_h * 2.5);
        let press = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        step(vec![egui::Event::PointerMoved(pos)], &mut editor, &mut st);
        for _ in 0..2 {
            step(vec![press(true)], &mut editor, &mut st);
            step(vec![press(false)], &mut editor, &mut st);
        }
        let ed = editor.as_ref().expect("double-click opens the editor");
        assert_eq!((ed.row, ed.col, ed.text.as_str()), (2, 1, "row 2"));
        // Typing replaces the (selected) value; Enter keeps it.
        step(vec![], &mut editor, &mut st);
        step(vec![egui::Event::Text("renamed".into())], &mut editor, &mut st);
        let enter = egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let ev = step(vec![enter], &mut editor, &mut st);
        match ev {
            Some(GridEvent::Change { row: 2, col: 1, value: Some(v) }) => assert_eq!(v, "renamed"),
            _ => panic!("expected a change"),
        }
        assert!(editor.is_none(), "the editor closes");

        // Esc cancels without a change.
        editor = Some(CellEditor::open(0, 1, Some("row 0")));
        step(vec![], &mut editor, &mut st);
        step(vec![egui::Event::Text("zzz".into())], &mut editor, &mut st);
        let esc = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        assert!(step(vec![esc], &mut editor, &mut st).is_none());
        assert!(editor.is_none());
    }

    #[test]
    fn date_picker() {
        let rs = ResultSet {
            columns: vec!["id".into(), "due".into()],
            rows: vec![vec![Some("1".into()), Some("2024-02-10".into())]],
            ..Default::default()
        };
        let kinds = [None, Some(TemporalKind::Date)];
        let ctx = egui::Context::default();
        let mut st = GridState::default();
        let changes = BTreeMap::new();
        let mut editor = Some(CellEditor::open(0, 1, Some("2024-02-10")));
        let mut t = 0.0;
        let mut step = |events: Vec<egui::Event>, editor: &mut Option<CellEditor>| {
            t += 0.05;
            frame_edit(&ctx, &rs, &mut st, &changes, editor, events, t, &kinds)
        };
        step(vec![], &mut editor);
        step(vec![], &mut editor);
        let popup = ctx.memory(|m| m.area_rect(Id::new(("cell-picker", 0usize, 1usize)))).expect("picker shown");
        assert!(popup.height() > 150.0, "calendar rows: {popup:?}");
        // Clicking inside the picker keeps the editor open.
        let inside = popup.center_top() + egui::vec2(0.0, 8.0);
        let press = |pressed| egui::Event::PointerButton { pos: inside, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        step(vec![egui::Event::PointerMoved(inside)], &mut editor);
        step(vec![press(true)], &mut editor);
        step(vec![press(false)], &mut editor);
        assert!(editor.is_some());
        // Typed text and Enter still work.
        editor.as_mut().unwrap().text = "2024-02-29".into();
        let enter = egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        match step(vec![enter], &mut editor) {
            Some(GridEvent::Change { value: Some(v), .. }) => assert_eq!(v, "2024-02-29"),
            _ => panic!("expected a change"),
        }
        assert!(editor.is_none());
        // Esc cancels.
        editor = Some(CellEditor::open(0, 1, Some("2024-02-10")));
        step(vec![], &mut editor);
        let esc = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        assert!(step(vec![esc], &mut editor).is_none());
        assert!(editor.is_none());
    }

    #[test]
    fn frozen_columns_scroll_together() {
        let rs = ResultSet {
            columns: vec!["id".into(), "name".into(), "note".into()],
            rows: (0..200).map(|i| vec![Some(i.to_string()), Some(format!("row {i}")), Some("x".repeat(80))]).collect(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut st = GridState { frozen: vec!["note".into()], ..Default::default() };
        frame(&ctx, &rs, &mut st, vec![]);
        frame(&ctx, &rs, &mut st, vec![]);
        // Wheel over the scrolling half (right side): both halves follow.
        let over = egui::pos2(700.0, 300.0);
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -400.0), modifiers: egui::Modifiers::NONE };
        frame(&ctx, &rs, &mut st, vec![egui::Event::PointerMoved(over), wheel]);
        for _ in 0..30 {
            frame(&ctx, &rs, &mut st, vec![]);
        }
        let y = st.scroll_y;
        assert!(y > 100.0, "scrolled: {y}");
        // A click in the frozen half selects the row shown there.
        let h = ctx.style().text_styles[&TextStyle::Monospace].size;
        let row_h = ctx.fonts_mut(|f| f.row_height(&egui::FontId::monospace(h))) + 10.0;
        let target = 8.0 + row_h + 4.0 + row_h * 2.5; // third visible row
        let pos = egui::pos2(120.0, target);
        let press = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(&ctx, &rs, &mut st, vec![egui::Event::PointerMoved(pos)]);
        frame(&ctx, &rs, &mut st, vec![press(true)]);
        frame(&ctx, &rs, &mut st, vec![press(false)]);
        let expected = ((y + row_h * 2.5) / row_h) as usize;
        assert_eq!(st.selection.iter().copied().collect::<Vec<_>>(), [expected], "scroll {y}, row_h {row_h}");
        // Freezing survives a new result with the same column names.
        assert_eq!(st.frozen, ["note"]);
    }

    #[test]
    fn reveal_scrolls_row_into_view() {
        let rs = ResultSet {
            columns: vec!["id".into()],
            rows: (0..300).map(|i| vec![Some(i.to_string())]).collect(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut st = GridState::default();
        frame(&ctx, &rs, &mut st, vec![]);
        st.reveal = Some(150);
        for _ in 0..3 {
            frame(&ctx, &rs, &mut st, vec![]);
        }
        let h = ctx.style().text_styles[&TextStyle::Monospace].size;
        let row_h = ctx.fonts_mut(|f| f.row_height(&egui::FontId::monospace(h))) + 10.0;
        assert!(st.reveal.is_none());
        assert!((st.scroll_y - 148.0 * row_h).abs() < 1.0, "scroll {}", st.scroll_y);
        assert_eq!(reveal_offset(1, 25.0), 0.0);
    }

    fn key(k: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn go_to_column() {
        let rs = ResultSet {
            columns: (0..40).map(|i| format!("col_{i}")).collect(),
            rows: (0..3).map(|r| (0..40).map(|c| Some(format!("{r}:{c}"))).collect()).collect(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut st = GridState::default();
        let none = egui::Modifiers::NONE;
        frame(&ctx, &rs, &mut st, vec![]);
        // Hover the grid so the shortcut applies.
        frame(&ctx, &rs, &mut st, vec![egui::Event::PointerMoved(egui::pos2(300.0, 200.0))]);
        assert!(st.goto.is_none());
        frame_mods(&ctx, &rs, &mut st, vec![key(egui::Key::J, egui::Modifiers::COMMAND)], egui::Modifiers::COMMAND);
        assert!(st.goto.is_some(), "cmd+J opens the popup");
        frame(&ctx, &rs, &mut st, vec![]);
        frame(&ctx, &rs, &mut st, vec![egui::Event::Text("col_3".into())]);
        frame(&ctx, &rs, &mut st, vec![]);
        assert_eq!(st.goto.as_ref().unwrap().query, "col_3");
        // Esc closes without jumping.
        frame(&ctx, &rs, &mut st, vec![key(egui::Key::Escape, none)]);
        assert!(st.goto.is_none());
        assert!(st.scroll_x.is_none() && st.flash.is_none());
        // Reopen, filter to col_3x (10 matches), move down once and jump.
        st.goto = Some(Goto { query: String::new(), highlight: 0, focus: true });
        frame(&ctx, &rs, &mut st, vec![]);
        frame(&ctx, &rs, &mut st, vec![egui::Event::Text("COL_3".into())]);
        frame(&ctx, &rs, &mut st, vec![key(egui::Key::ArrowDown, none)]);
        assert_eq!(st.goto.as_ref().unwrap().highlight, 1);
        frame(&ctx, &rs, &mut st, vec![key(egui::Key::Enter, none)]);
        assert!(st.goto.is_none(), "enter closes");
        // Matches are col_3, col_30.. so the highlighted one is col_30.
        assert_eq!(st.flash.map(|f| f.0), Some(30));
        assert!(st.scroll_x.is_some_and(|x| x > 500.0), "scroll_x {:?}", st.scroll_x);
    }

    #[test]
    fn column_copies() {
        let s = |v: &str| Some(v.to_string());
        let rs = ResultSet {
            columns: vec!["id".into(), "land_right".into()],
            rows: vec![
                vec![s("1"), s("{23F7220B-490C}")],
                vec![s("2"), None],
                vec![s("3"), s("O'Neil")],
                vec![s("4"), s("{23F7220B-490C}")],
            ],
            ..Default::default()
        };
        let all = [0, 1, 2, 3];
        assert_eq!(column_lines(&rs, &all, 1), "{23F7220B-490C}\nNULL\nO'Neil\n{23F7220B-490C}");
        assert_eq!(column_distinct(&rs, &all, 1), ["{23F7220B-490C}", "O'Neil"]);
        assert_eq!(column_sql_list(&rs, &all, 1), "('{23F7220B-490C}', 'O''Neil')");
        assert_eq!(column_sql_list(&rs, &[0, 2], 0), "('1', '3')");
        let codes = ResultSet { columns: vec!["zip".into()], rows: vec![vec![s("012")], vec![s("7")]], ..Default::default() };
        assert_eq!(column_sql_list(&codes, &[0, 1], 0), "('012', '7')");
    }

    #[test]
    fn cell_text_is_bounded() {
        assert_eq!(cell_text("a\nb", 10), "a↵ b");
        assert_eq!(cell_text("abcdef", 3), "abc…");
        assert_eq!(cell_text("ភ្នំពេញ", 3).chars().count(), 4);
        assert_eq!(cell_text(&"x".repeat(100_000), 240).chars().count(), 241);
    }
}

#[cfg(test)]
mod perf {
    //! `cargo test --release -p rgrip grid_frame_time -- --ignored --nocapture`
    use super::*;
    use std::time::Instant;

    /// A page shaped like a real PostGIS table: long coordinate text,
    /// Khmer HTML, NULLs and a geometry column.
    fn heavy_page() -> ResultSet {
        let coords: String = (0..1500).map(|i| format!("104.91{i:05},11.61{i:05},0 ")).collect();
        let khmer = "<div>ផ្លូវកោះពេជ្រ&nbsp;<span>សង្កាត់ទន្លេបាសាក់ ខណ្ឌចំការមន រាជធានីភ្នំពេញ</span></div>".repeat(20);
        let geom = "0101000020E61000008F537424973B5A40AA8251499D202740".to_string();
        let rows = (0..1000)
            .map(|i| {
                vec![
                    Some(i.to_string()),
                    Some(format!("Untitled polygon {i}")),
                    if i % 3 == 0 { Some(khmer.clone()) } else { None },
                    Some(coords.clone()),
                    Some("Polygon".into()),
                    Some(geom.clone()),
                ]
            })
            .collect();
        ResultSet {
            columns: ["id", "name", "description", "coordinates", "geometry_type", "geom"].map(String::from).to_vec(),
            rows,
            ..Default::default()
        }
    }

    #[test]
    #[ignore]
    fn grid_frame_time() {
        let rs = heavy_page();
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(2.0);
        crate::theme::apply(&ctx, rgrip_core::config::ThemeChoice::Dark, "rgrip Dark", "rgrip Light");
        let mut state = GridState::default();
        let mut frame = |scroll: f32| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0))),
                events: vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, scroll),
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            };
            let start = Instant::now();
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show(ui, egui::Id::new("perf"), &rs, &mut state, None, 0, &Source { table: "t", sql: None, relations: &[], counts: &[] }, None);
                });
            });
            // Tessellation runs every frame in the real app too.
            let _ = ctx.tessellate(out.shapes, 2.0);
            start.elapsed()
        };
        for _ in 0..5 {
            frame(0.0);
        }
        let n = 60;
        let total: std::time::Duration = (0..n).map(|i| frame(if i % 2 == 0 { -40.0 } else { 0.0 })).sum();
        println!("grid frame: {:.2} ms avg over {n} frames", total.as_secs_f64() * 1000.0 / n as f64);
    }
}
