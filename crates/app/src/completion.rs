//! Autocomplete popup for the SQL editor.

use crate::theme;
use crate::widgets::{self, Icon};
use eframe::egui::{
    self, Align2, Area, CornerRadius, FontId, Frame, Id, Key, Margin, Modifiers, Order, Sense,
    Shadow, Stroke, TextEdit, TextFormat, Ui,
    text::{CCursor, CCursorRange, LayoutJob},
    text_edit::TextEditOutput,
    vec2,
};
use rgrip_core::complete::{Completion, ItemKind};
use std::ops::Range;

pub struct Popup {
    pub completion: Completion,
    pub selected: usize,
    /// Scroll the selected row into view (after keyboard navigation).
    pub scroll: bool,
}

impl Popup {
    pub fn new(completion: Completion) -> Self {
        Self { completion, selected: 0, scroll: true }
    }
}

#[derive(Default)]
pub struct Keys {
    /// Index of the item to insert.
    pub accept: Option<usize>,
    /// ⌃Space: open even without a prefix.
    pub force: bool,
    /// Escape closed the popup (egui also drops editor focus on Escape).
    pub dismissed: bool,
}

/// Must run before the editor so it doesn't see the navigation keys.
pub fn handle_keys(ui: &Ui, popup: &mut Option<Popup>) -> Keys {
    let mut keys = Keys {
        force: ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space)),
        ..Default::default()
    };
    let Some(p) = popup else { return keys };
    let n = p.completion.items.len();
    let pressed = |key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, key));
    if pressed(Key::ArrowDown) {
        p.selected = (p.selected + 1) % n;
        p.scroll = true;
    }
    if pressed(Key::ArrowUp) {
        p.selected = (p.selected + n - 1) % n;
        p.scroll = true;
    }
    if pressed(Key::Enter) || pressed(Key::Tab) {
        keys.accept = Some(p.selected);
    }
    if pressed(Key::Escape) {
        *popup = None;
        keys.dismissed = true;
    }
    keys
}

/// Replace `range` of `sql` with `text` and put the editor cursor after it.
pub fn replace_text(ctx: &egui::Context, editor: Id, sql: &mut String, range: Range<usize>, text: &str) -> Range<usize> {
    sql.replace_range(range.clone(), text);
    let at = sql[..range.start + text.len()].chars().count();
    if let Some(mut state) = TextEdit::load_state(ctx, editor) {
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(at))));
        state.store(ctx, editor);
    }
    at..at
}

/// Replace the completed word and move the editor cursor after it.
/// Returns the new (empty) char selection.
pub fn accept(ctx: &egui::Context, editor: Id, sql: &mut String, c: &Completion, index: usize) -> Range<usize> {
    let item = &c.items[index];
    let range = item.replace.clone().unwrap_or_else(|| c.replace.clone());
    sql.replace_range(range.clone(), &item.insert);
    let byte = range.start + item.insert.len();
    let at = sql[..byte].chars().count();
    if let Some(mut state) = TextEdit::load_state(ctx, editor) {
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(at))));
        state.store(ctx, editor);
    }
    at..at
}

pub enum Outcome {
    None,
    Accept(usize),
    Close,
}

const ROW: f32 = 24.0;
const WIDTH: f32 = 400.0;

pub fn show(ui: &Ui, id: Id, p: &mut Popup, out: &TextEditOutput, sql: &str, editor_focused: bool) -> Outcome {
    let start = sql[..p.completion.replace.start.min(sql.len())].chars().count();
    let anchor = out.galley.pos_from_cursor(CCursor::new(start));
    let pos = out.galley_pos + anchor.left_bottom().to_vec2() + vec2(-30.0, 4.0);

    let mut clicked = None;
    let area = Area::new(id)
        .order(Order::Foreground)
        .fixed_pos(pos)
        .constrain(true)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(theme::POPUP())
                .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(4))
                .shadow(Shadow { offset: [0, 8], blur: 24, spread: 0, color: egui::Color32::from_black_alpha(if theme::is_dark() { 140 } else { 40 }) })
                .show(ui, |ui| {
                    // Commands carry argument hints and longer descriptions.
                    let commands = p.completion.items.iter().any(|i| i.kind == ItemKind::Command);
                    ui.set_width(if commands { 560.0 } else { WIDTH });
                    ui.spacing_mut().item_spacing.y = 0.0;
                    egui::ScrollArea::vertical()
                        .max_height(ROW * 10.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (i, item) in p.completion.items.iter().enumerate() {
                                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
                                if i == p.selected {
                                    ui.painter().rect_filled(rect, 5.0, theme::ACCENT().gamma_multiply(0.22));
                                    if p.scroll {
                                        resp.scroll_to_me(None);
                                    }
                                } else if resp.hovered() {
                                    ui.painter().rect_filled(rect, 5.0, theme::HOVER());
                                }
                                paint_row(ui, rect, item, &p.completion.prefix);
                                if resp.clicked() {
                                    clicked = Some(i);
                                }
                            }
                        });
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        let hint = |ui: &mut Ui, t: &str| ui.label(egui::RichText::new(t).size(11.0).color(theme::FAINT()));
                        hint(ui, "↩ insert    ↑↓ select    esc dismiss    ⌃space suggest");
                    });
                    ui.add_space(2.0);
                });
        });
    p.scroll = false;

    match clicked {
        Some(i) => Outcome::Accept(i),
        None if !editor_focused && !area.response.contains_pointer() => Outcome::Close,
        None => Outcome::None,
    }
}

fn paint_row(ui: &Ui, rect: egui::Rect, item: &rgrip_core::complete::Suggestion, prefix: &str) {
    let p = ui.painter().with_clip_rect(rect);
    let cy = rect.center().y;
    let icon = match item.kind {
        ItemKind::Column => Icon::Column,
        ItemKind::Table => Icon::Table,
        ItemKind::View => Icon::View,
        ItemKind::Schema => Icon::Schema,
        ItemKind::Function => Icon::Function,
        ItemKind::Keyword => Icon::Keyword,
        ItemKind::Snippet => Icon::Globe,
        ItemKind::Command => Icon::Console,
    };
    widgets::paint_icon(&p, egui::pos2(rect.left() + 14.0, cy), icon);

    // Detail first so the label can be clipped before it.
    let detail_font = FontId::proportional(11.5);
    let detail = p.text(
        egui::pos2(rect.right() - 10.0, cy),
        Align2::RIGHT_CENTER,
        &item.detail,
        detail_font,
        theme::FAINT(),
    );

    let font = FontId::monospace(12.5);
    let mut job = LayoutJob::default();
    let split = matched_prefix_len(&item.label, prefix);
    let fmt = |color| TextFormat { font_id: font.clone(), color, ..Default::default() };
    job.append(&item.label[..split], 0.0, fmt(theme::ACCENT()));
    job.append(&item.label[split..], 0.0, fmt(theme::TEXT()));
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let label_pos = egui::pos2(rect.left() + 30.0, cy - galley.size().y / 2.0);
    let clip = egui::Rect::from_min_max(rect.min, egui::pos2(detail.left() - 8.0, rect.max.y));
    p.with_clip_rect(clip).galley(label_pos, galley, theme::TEXT());
}

/// Byte length of the label's case-insensitive match of `prefix`.
fn matched_prefix_len(label: &str, prefix: &str) -> usize {
    let n = prefix.chars().count();
    let head: String = label.chars().take(n).collect();
    if n > 0 && head.eq_ignore_ascii_case(prefix) { head.len() } else { 0 }
}
