//! Small painted widgets: icons, tree rows, status dots.
//! Everything is drawn with the painter so nothing depends on font glyphs.

use crate::theme;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Response, Sense, Stroke,
    StrokeKind, Ui, pos2, vec2,
};

#[derive(Clone, Copy)]
pub enum Icon {
    /// Connection status color, and the connection's tint if it has one.
    Database(Color32, Option<Color32>),
    Schema,
    Table,
    View,
    Column,
    Key,
    Console,
    Keyword,
    Function,
    /// Saved query (a small document).
    Query,
    /// Generated spatial SQL (a globe).
    Globe,
    /// Appearance: follow the system (a monitor).
    System,
    /// Appearance: light (a sun).
    Sun,
    /// Appearance: dark (a crescent moon).
    Moon,
    /// Copy to clipboard (two overlapping sheets).
    Copy,
    /// Close (an ×).
    Close,
    /// Column list (three vertical bars).
    Columns,
}

pub fn paint_icon(p: &Painter, c: Pos2, icon: Icon) {
    paint_icon_tinted(p, c, icon, None);
}

/// `tint` overrides the icon's own color (e.g. for selected states).
pub fn paint_icon_tinted(p: &Painter, c: Pos2, icon: Icon, tint: Option<Color32>) {
    let thin = |color| Stroke::new(1.2_f32, color);
    let color_or = |default: Color32| tint.unwrap_or(default);
    match icon {
        Icon::Database(status, color) => {
            // Cylinder: body plus two rims, with a status dot.
            let body = Rect::from_center_size(c, vec2(10.0, 11.0));
            let line = color.unwrap_or(theme::MUTED());
            p.rect_stroke(body, CornerRadius::same(3), thin(line), StrokeKind::Inside);
            p.line_segment([pos2(body.left() + 1.0, c.y - 1.5), pos2(body.right() - 1.0, c.y - 1.5)], thin(line));
            p.circle_filled(pos2(body.right(), body.bottom()), 3.0, theme::BG());
            p.circle_filled(pos2(body.right(), body.bottom()), 2.2, status);
        }
        Icon::Schema => {
            let r = Rect::from_center_size(c + vec2(0.0, 0.5), vec2(12.0, 9.0));
            p.rect_stroke(r, CornerRadius::same(2), thin(theme::MUTED()), StrokeKind::Inside);
            p.line_segment([pos2(r.left(), r.top() - 1.5), pos2(r.left() + 5.0, r.top() - 1.5)], thin(theme::MUTED()));
        }
        Icon::Table | Icon::View => {
            let color = if matches!(icon, Icon::Table) { theme::ACCENT() } else { theme::VIEW() };
            let r = Rect::from_center_size(c, vec2(12.0, 10.0));
            p.rect_stroke(r, CornerRadius::same(2), thin(color), StrokeKind::Inside);
            p.line_segment([pos2(r.left(), r.top() + 3.5), pos2(r.right(), r.top() + 3.5)], thin(color));
            p.line_segment([pos2(c.x, r.top() + 3.5), pos2(c.x, r.bottom())], thin(color.gamma_multiply(0.6)));
        }
        Icon::Column => {
            p.circle_stroke(c, 2.6, thin(theme::FAINT()));
        }
        Icon::Key => {
            p.circle_stroke(c - vec2(2.0, 0.0), 2.6, thin(theme::WARN()));
            p.line_segment([c + vec2(0.6, 0.0), c + vec2(5.0, 0.0)], thin(theme::WARN()));
            p.line_segment([c + vec2(4.0, 0.0), c + vec2(4.0, 2.2)], thin(theme::WARN()));
        }
        Icon::Keyword => {
            let s = thin(theme::MUTED());
            for (dy, w) in [(-3.0, 9.0), (0.0, 6.0), (3.0, 8.0)] {
                p.line_segment([c + vec2(-4.5, dy), c + vec2(-4.5 + w, dy)], s);
            }
        }
        Icon::Function => {
            p.text(c, Align2::CENTER_CENTER, "ƒ", FontId::proportional(13.0), theme::WARN());
        }
        Icon::Query => {
            let r = Rect::from_center_size(c, vec2(9.0, 11.5));
            p.rect_stroke(r, CornerRadius::same(2), thin(theme::MUTED()), StrokeKind::Inside);
            for dy in [-1.5, 1.0] {
                p.line_segment([pos2(r.left() + 2.2, c.y + dy), pos2(r.right() - 2.2, c.y + dy)], thin(theme::MUTED()));
            }
        }
        Icon::Globe => {
            let s = thin(color_or(theme::GEO()));
            p.circle_stroke(c, 5.5, s);
            p.line_segment([c - vec2(5.5, 0.0), c + vec2(5.5, 0.0)], s);
            // Meridian: a narrow ellipse.
            let pts: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let a = i as f32 / 24.0 * std::f32::consts::TAU;
                    c + vec2(a.cos() * 2.4, a.sin() * 5.5)
                })
                .collect();
            p.add(egui::Shape::line(pts, s));
        }
        Icon::System => {
            let s = thin(color_or(theme::MUTED()));
            let screen = Rect::from_center_size(c - vec2(0.0, 1.5), vec2(13.0, 9.0));
            p.rect_stroke(screen, CornerRadius::same(2), s, StrokeKind::Inside);
            p.line_segment([pos2(c.x, screen.bottom()), pos2(c.x, screen.bottom() + 2.5)], s);
            p.line_segment([pos2(c.x - 3.5, screen.bottom() + 2.5), pos2(c.x + 3.5, screen.bottom() + 2.5)], s);
        }
        Icon::Sun => {
            let s = thin(color_or(theme::WARN()));
            p.circle_stroke(c, 3.0, s);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let d = vec2(a.cos(), a.sin());
                p.line_segment([c + d * 5.0, c + d * 6.8], s);
            }
        }
        Icon::Moon => {
            // Crescent outline: outer arc minus an offset inner circle.
            let (r, inner_c, inner_r) = (6.0, c + vec2(3.2, -2.4), 5.0);
            let inside = |q: Pos2, center: Pos2, radius: f32| (q - center).length() < radius;
            let mut pts = Vec::new();
            for i in 0..=72 {
                let a = i as f32 / 72.0 * std::f32::consts::TAU;
                let q = c + vec2(a.cos(), a.sin()) * r;
                if !inside(q, inner_c, inner_r) {
                    pts.push(q);
                }
            }
            for i in (0..=72).rev() {
                let a = i as f32 / 72.0 * std::f32::consts::TAU;
                let q = inner_c + vec2(a.cos(), a.sin()) * inner_r;
                if inside(q, c, r) {
                    pts.push(q);
                }
            }
            p.add(egui::Shape::closed_line(pts, thin(color_or(theme::VIEW()))));
        }
        Icon::Copy => {
            let color = color_or(theme::FAINT());
            let back = Rect::from_min_size(c + vec2(-5.0, -5.0), vec2(7.5, 8.5));
            let front = back.translate(vec2(3.0, 2.5));
            p.rect_stroke(back, CornerRadius::same(2), thin(color.gamma_multiply(0.7)), StrokeKind::Inside);
            p.rect_filled(front, CornerRadius::same(2), theme::POPUP());
            p.rect_stroke(front, CornerRadius::same(2), thin(color), StrokeKind::Inside);
        }
        Icon::Close => {
            let s = Stroke::new(1.4_f32, color_or(theme::FAINT()));
            p.line_segment([c + vec2(-4.0, -4.0), c + vec2(4.0, 4.0)], s);
            p.line_segment([c + vec2(-4.0, 4.0), c + vec2(4.0, -4.0)], s);
        }
        Icon::Columns => {
            let s = thin(color_or(theme::FAINT()));
            let r = Rect::from_center_size(c, vec2(12.0, 10.0));
            p.rect_stroke(r, CornerRadius::same(2), s, StrokeKind::Inside);
            for dx in [-2.0, 2.0] {
                p.line_segment([pos2(c.x + dx, r.top()), pos2(c.x + dx, r.bottom())], s);
            }
        }
        Icon::Console => {
            let s = thin(theme::ACCENT());
            p.line_segment([c + vec2(-5.0, -3.5), c + vec2(-1.5, 0.0)], s);
            p.line_segment([c + vec2(-1.5, 0.0), c + vec2(-5.0, 3.5)], s);
            p.line_segment([c + vec2(0.5, 3.5), c + vec2(5.0, 3.5)], s);
        }
    }
}

/// A small frameless icon button; brightens on hover.
pub fn icon_button(ui: &mut Ui, icon: Icon, hover: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
    let tint = if response.hovered() { theme::TEXT() } else { theme::FAINT() };
    paint_icon_tinted(ui.painter(), rect.center(), icon, Some(tint));
    response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(hover)
}

pub fn paint_chevron(p: &Painter, c: Pos2, open: bool, color: Color32) {
    let s = Stroke::new(1.3_f32, color);
    let pts = if open {
        [c + vec2(-3.5, -1.8), c + vec2(0.0, 1.8), c + vec2(3.5, -1.8)]
    } else {
        [c + vec2(-1.8, -3.5), c + vec2(1.8, 0.0), c + vec2(-1.8, 3.5)]
    };
    p.line_segment([pts[0], pts[1]], s);
    p.line_segment([pts[1], pts[2]], s);
}

pub fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

pub struct TreeRow<'a> {
    pub depth: usize,
    /// `Some(open)` draws a chevron.
    pub open: Option<bool>,
    pub icon: Icon,
    pub label: &'a str,
    pub label_color: Color32,
    pub detail: Option<&'a str>,
    pub selected: bool,
}

pub const ROW_HEIGHT: f32 = 24.0;

pub fn tree_row(ui: &mut Ui, row: TreeRow<'_>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter().with_clip_rect(rect);
    if row.selected {
        p.rect_filled(rect, 5.0, theme::ACCENT().gamma_multiply(0.16));
    } else if resp.hovered() {
        p.rect_filled(rect, 5.0, theme::HOVER());
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 6.0 + row.depth as f32 * 14.0;
    if let Some(open) = row.open {
        let color = if resp.hovered() { theme::TEXT() } else { theme::FAINT() };
        paint_chevron(&p, pos2(x + 4.0, cy), open, color);
    }
    x += 14.0;
    paint_icon(&p, pos2(x + 6.0, cy), row.icon);
    x += 19.0;
    let label = match crate::shaping::is_complex(row.label).then(|| crate::shaping::shape(ui.ctx(), row.label, 13.5)).flatten() {
        Some(shaped) => {
            shaped.paint(&p, x, cy, row.label_color, rect);
            Rect::from_min_size(pos2(x, cy - shaped.size.y / 2.0), shaped.size)
        }
        None => p.text(pos2(x, cy), Align2::LEFT_CENTER, row.label, FontId::proportional(13.0), row.label_color),
    };
    if let Some(detail) = row.detail {
        p.text(
            pos2(label.right() + 7.0, cy + 0.5),
            Align2::LEFT_CENTER,
            detail,
            FontId::proportional(11.5),
            theme::FAINT(),
        );
    }
    resp
}

/// A muted one-line message inside the tree (loading, empty, errors).
pub fn tree_note(ui: &mut Ui, depth: usize, text: &str, color: Color32) {
    let indent = 6.0 + depth as f32 * 14.0 + 14.0;
    ui.horizontal(|ui| {
        ui.add_space(indent);
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(color)).wrap());
    });
}

/// A color choice for the connection dialog: a filled circle, or for
/// "none" an outlined circle with a slash. Ringed when selected.
pub fn color_swatch(ui: &mut Ui, color: Option<Color32>, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
    let p = ui.painter();
    let c = rect.center();
    match color {
        Some(col) => {
            p.circle_filled(c, 7.0, col);
        }
        None => {
            p.circle_stroke(c, 6.5, Stroke::new(1.2_f32, theme::FAINT()));
            p.line_segment([c + vec2(-4.0, 4.0), c + vec2(4.0, -4.0)], Stroke::new(1.2_f32, theme::FAINT()));
        }
    }
    if selected {
        p.circle_stroke(c, 10.0, Stroke::new(1.5_f32, color.unwrap_or(theme::MUTED())));
    } else if resp.hovered() {
        p.circle_stroke(c, 10.0, Stroke::new(1.0_f32, theme::BORDER()));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
