//! Map export: the map as it is on screen, laid out like a printed map
//! (title, legend, scale bar, north arrow, attribution) and saved as a PNG
//! or copied. The basemap and features are captured from the screen, so the
//! picture matches what you see; the map's controls are hidden for that frame.

use crate::theme;
use eframe::egui::{self, Align2, Color32, ColorImage, FontId, Painter, Pos2, Rect, Stroke, Vec2, pos2, vec2};
use resvg::tiny_skia;
use std::time::Instant;

pub struct Export {
    pub title: String,
    pub legend: bool,
    pub scale_bar: bool,
    pub north_arrow: bool,
    pub stage: Stage,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Stage {
    /// The options panel is open; the layout shows on the map as a preview.
    Edit,
    /// One frame drawn without the map's controls, then the screenshot.
    Hide { copy: bool },
    Wait { copy: bool, since: Instant },
}

impl Default for Export {
    fn default() -> Self {
        Self { title: String::new(), legend: true, scale_bar: true, north_arrow: true, stage: Stage::Edit }
    }
}

impl Export {
    /// Controls and popups stay off the map while it's being captured.
    pub fn capturing(&self) -> bool {
        !matches!(self.stage, Stage::Edit)
    }
}

/// One legend row: a swatch in the features' style and its label.
pub struct LegendItem {
    pub color: Color32,
    pub label: String,
}

/// A round scale-bar length: the largest 1, 2 or 5 × 10ⁿ meters that fits in
/// `max_px` at `meters_per_px`. Returns (length in px, label).
pub fn scale_bar(meters_per_px: f64, max_px: f32) -> Option<(f32, String)> {
    if !(meters_per_px.is_finite() && meters_per_px > 0.0) {
        return None;
    }
    let max_m = meters_per_px * max_px as f64;
    let mut unit = 10f64.powf(max_m.log10().floor());
    let meters = loop {
        if 5.0 * unit <= max_m {
            break 5.0 * unit;
        } else if 2.0 * unit <= max_m {
            break 2.0 * unit;
        } else if unit <= max_m {
            break unit;
        }
        unit /= 10.0;
    };
    let label = if meters >= 1000.0 { format!("{} km", trim(meters / 1000.0)) } else { format!("{} m", trim(meters)) };
    Some(((meters / meters_per_px) as f32, label))
}

fn trim(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Great-circle distance in meters between two lon/lat points.
pub fn haversine(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (la1, la2) = (a[1].to_radians(), b[1].to_radians());
    let dla = la2 - la1;
    let dlo = (b[0] - a[0]).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * h.sqrt().asin()
}

/// Paint the layout over the map. Panels use a light card so they read on
/// any basemap, like a printed map's legend box.
pub fn paint(painter: &Painter, rect: Rect, e: &Export, meters_per_px: Option<f64>, legend: &[LegendItem]) {
    let card = Color32::from_rgba_unmultiplied(255, 255, 255, 236);
    let ink = Color32::from_rgb(28, 30, 34);
    let soft = Color32::from_rgb(92, 96, 104);
    let edge = Stroke::new(1.0_f32, Color32::from_black_alpha(40));
    let pad = 14.0;

    let title = e.title.trim();
    if !title.is_empty() {
        let galley = painter.layout(title.to_string(), FontId::proportional(20.0), ink, rect.width() - 160.0);
        let back = Rect::from_center_size(pos2(rect.center().x, rect.top() + pad + galley.size().y / 2.0 + 8.0), galley.size() + vec2(28.0, 16.0));
        painter.rect(back, 8.0, card, edge, egui::StrokeKind::Inside);
        painter.galley(back.center() - galley.size() / 2.0, galley, ink);
    }

    if e.north_arrow {
        let c = pos2(rect.right() - pad - 20.0, rect.top() + pad + 26.0);
        painter.circle(c, 20.0, card, edge);
        // A two-tone arrowhead pointing up, then "N".
        let tip = c + vec2(0.0, -13.0);
        let base_l = c + vec2(-7.0, 7.0);
        let base_r = c + vec2(7.0, 7.0);
        let notch = c + vec2(0.0, 3.0);
        painter.add(egui::Shape::convex_polygon(vec![tip, notch, base_l], ink, Stroke::NONE));
        painter.add(egui::Shape::convex_polygon(vec![tip, base_r, notch], soft, Stroke::NONE));
        painter.text(c + vec2(0.0, 12.5), Align2::CENTER_CENTER, "N", FontId::proportional(9.5), ink);
    }

    // Bottom left, stacked upward: scale bar, then the legend above it.
    let mut bottom = rect.bottom() - pad;
    if e.scale_bar
        && let Some((len, label)) = meters_per_px.and_then(|m| scale_bar(m, (rect.width() * 0.25).clamp(80.0, 180.0)))
    {
        let text = painter.layout_no_wrap(label, FontId::proportional(11.0), ink);
        let w = len.max(text.size().x) + 24.0;
        let back = Rect::from_min_max(pos2(rect.left() + pad, bottom - 38.0), pos2(rect.left() + pad + w, bottom));
        painter.rect(back, 6.0, card, edge, egui::StrokeKind::Inside);
        // Alternating black/white quarters, like a printed scale bar.
        let y = back.bottom() - 12.0;
        let x0 = back.left() + 12.0;
        for i in 0..4 {
            let seg = Rect::from_min_size(pos2(x0 + len / 4.0 * i as f32, y - 5.0), vec2(len / 4.0, 5.0));
            painter.rect_filled(seg, 0.0, if i % 2 == 0 { ink } else { Color32::WHITE });
        }
        painter.rect_stroke(Rect::from_min_size(pos2(x0, y - 5.0), vec2(len, 5.0)), 0.0, Stroke::new(1.0_f32, ink), egui::StrokeKind::Middle);
        painter.text(pos2(x0, back.top() + 7.0), Align2::LEFT_TOP, "0", FontId::proportional(10.0), soft);
        painter.galley(pos2(x0 + len - text.size().x, back.top() + 6.0), text, ink);
        bottom = back.top() - 8.0;
    }

    if e.legend && !legend.is_empty() {
        let font = FontId::proportional(12.0);
        let rows: Vec<_> = legend.iter().map(|l| painter.layout_no_wrap(l.label.clone(), font.clone(), ink)).collect();
        let w = rows.iter().map(|g| g.size().x).fold(0.0, f32::max) + 50.0;
        let h = rows.len() as f32 * 22.0 + 14.0;
        let back = Rect::from_min_max(pos2(rect.left() + pad, bottom - h), pos2(rect.left() + pad + w, bottom));
        painter.rect(back, 6.0, card, edge, egui::StrokeKind::Inside);
        for (i, (item, galley)) in legend.iter().zip(rows).enumerate() {
            let y = back.top() + 7.0 + i as f32 * 22.0 + 11.0;
            let sw = Rect::from_center_size(pos2(back.left() + 20.0, y), Vec2::splat(12.0));
            painter.rect(sw, 3.0, item.color.gamma_multiply(0.55), Stroke::new(1.5_f32, item.color), egui::StrokeKind::Inside);
            painter.galley(pos2(back.left() + 36.0, y - galley.size().y / 2.0), galley, ink);
        }
    }
}

/// The captured screen, cropped to the map.
pub fn crop(screen: &ColorImage, rect: Rect, pixels_per_point: f32) -> ColorImage {
    let full = Rect::from_min_size(Pos2::ZERO, vec2(screen.width() as f32, screen.height() as f32) / pixels_per_point);
    screen.region(&rect.intersect(full), Some(pixels_per_point))
}

pub fn png(image: &ColorImage) -> Result<Vec<u8>, String> {
    let size = tiny_skia::IntSize::from_wh(image.width() as u32, image.height() as u32).ok_or("empty image")?;
    // Screenshots are opaque, so straight and premultiplied RGBA agree.
    let pixmap = tiny_skia::Pixmap::from_vec(image.as_raw().to_vec(), size).ok_or("bad image size")?;
    pixmap.encode_png().map_err(|e| e.to_string())
}

/// A file name from the title, or the layer.
pub fn file_name(title: &str, layer: &str) -> String {
    let base = if title.trim().is_empty() { layer } else { title.trim() };
    let clean: String = base.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let clean = clean.trim_matches('_');
    format!("{}.png", if clean.is_empty() { "map" } else { clean })
}

/// The panel that sets up the export (top right of the map).
pub enum PanelAction {
    None,
    Save,
    Copy,
    Close,
}

pub fn panel(ui: &mut egui::Ui, rect: Rect, e: &mut Export) -> PanelAction {
    let mut action = PanelAction::None;
    egui::Area::new(egui::Id::new("map-export"))
        .order(egui::Order::Foreground)
        // Middle of the right edge: clear of the title, north arrow, legend and scale bar.
        .pivot(Align2::RIGHT_CENTER)
        .fixed_pos(rect.right_center() + vec2(-12.0, 0.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(theme::POPUP())
                .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                .corner_radius(10)
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(250.0);
                    ui.label(egui::RichText::new("Export map").strong().color(theme::TEXT()));
                    ui.add_space(6.0);
                    ui.add(egui::TextEdit::singleline(&mut e.title).hint_text("Title (optional)").desired_width(f32::INFINITY));
                    ui.add_space(4.0);
                    ui.checkbox(&mut e.legend, "Legend");
                    ui.checkbox(&mut e.scale_bar, "Scale bar");
                    ui.checkbox(&mut e.north_arrow, "North arrow");
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("Saved as you see it, at screen resolution").size(11.0).color(theme::MUTED()));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(egui::RichText::new("Save PNG…").color(theme::BG())).fill(theme::ACCENT())).clicked() {
                            action = PanelAction::Save;
                        }
                        if ui.button("Copy").clicked() {
                            action = PanelAction::Copy;
                        }
                        if ui.button("Cancel").clicked() {
                            action = PanelAction::Close;
                        }
                    });
                });
        });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = PanelAction::Close;
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_bar_lengths_are_round() {
        // 10 m per px, up to 150 px: 1500 m fits, so 1 km.
        let (px, label) = scale_bar(10.0, 150.0).unwrap();
        assert_eq!(label, "1 km");
        assert!((px - 100.0).abs() < 0.01);
        assert_eq!(scale_bar(0.5, 150.0).unwrap().1, "50 m");
        assert_eq!(scale_bar(30.0, 150.0).unwrap().1, "2 km");
        assert_eq!(scale_bar(0.004, 150.0).unwrap().1, "0.5 m");
        assert!(scale_bar(0.0, 150.0).is_none());
    }

    #[test]
    fn png_round_trip() {
        let image = ColorImage::new([3, 2], vec![Color32::from_rgb(10, 20, 30); 6]);
        let bytes = png(&image).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        let back = tiny_skia::Pixmap::decode_png(&bytes).unwrap();
        assert_eq!((back.width(), back.height()), (3, 2));
        assert_eq!(&back.data()[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn distances_and_names() {
        // One degree of latitude is about 111 km.
        let d = haversine([104.9, 11.5], [104.9, 12.5]);
        assert!((d - 111_195.0).abs() < 300.0, "{d}");
        assert_eq!(file_name("Phnom Penh parcels", "geom"), "Phnom_Penh_parcels.png");
        assert_eq!(file_name("  ", "geom"), "geom.png");
        assert_eq!(file_name("///", "x"), "map.png");
    }
}
