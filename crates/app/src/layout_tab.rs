//! The Map layout tab: a page with a map of several table layers, a title,
//! legend, scale bar and north arrow you drag into place, exported as PNG or
//! PDF. The page is one SVG from `rgrip_core::layout`; a background thread
//! renders it for the preview and for exports, so they match.
//!
//! Layers load only what's in view (plus a margin), simplified to print
//! resolution by PostGIS, and reload when the map moves past that.

use crate::icons::{self, Svg};
use crate::worker::{LAYER_LIMIT, LayerQuery, LayerResult};
use crate::{snapshot, theme};
use eframe::egui::{self, Align2, Color32, ColorImage, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, TextEdit, TextureHandle, Ui, pos2, vec2};
use rgrip_core::complete::Catalog;
use rgrip_core::layout::{self, Basemap, Class, Element, Layer, Layout, PageSize, Style};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Colors for new layers, in order.
const PALETTE: [[u8; 3]; 8] = [[214, 69, 65], [41, 128, 185], [39, 174, 96], [142, 68, 173], [230, 126, 34], [22, 160, 133], [192, 57, 43], [52, 73, 94]];
/// Category colors (a qualitative set that prints well).
const CLASS_COLORS: [[u8; 3]; 12] = [
    [31, 119, 180],
    [255, 127, 14],
    [44, 160, 44],
    [214, 39, 40],
    [148, 103, 189],
    [140, 86, 75],
    [227, 119, 194],
    [127, 127, 127],
    [188, 189, 34],
    [23, 190, 207],
    [174, 199, 232],
    [255, 187, 120],
];
/// Largest PNG export, in pixels.
const MAX_PIXELS: f64 = 120_000_000.0;
/// Width of the label column in the side panel.
const LABEL_W: f32 = 76.0;

pub struct Action {
    pub layer: u64,
    pub seq: u64,
    pub kind: LayerQuery,
    pub sql: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Png,
    Pdf,
}

enum Job {
    Preview { version: u64, layout: Layout, px_per_mm: f32 },
    Export { layout: Layout, format: Format, dpi: f64, path: PathBuf },
}

enum Done {
    /// The page it was drawn for comes back too, so the picture can be
    /// stretched to a new view until the next one arrives.
    Preview { version: u64, image: ColorImage, layout: Box<Layout>, elements: Vec<(Element, ColorImage, layout::Rect)> },
    Exported(Result<PathBuf, String>),
}

enum Drag {
    /// An element, grabbed at an offset from its top-left (mm).
    Element(Element, [f64; 2]),
    /// The map, panned from the last pointer position (mm).
    Pan([f64; 2]),
}

/// Per layer: queries in flight (by sequence number) and what to show.
#[derive(Default)]
struct LayerState {
    /// Each loaded shape's lon/lat box, to count what's inside the frame.
    bounds: Vec<[f64; 4]>,
    truncated: bool,
    data: Option<(u64, [f64; 4], f64)>,
    extent: Option<u64>,
    note: Option<(String, bool)>,
    open: bool,
}

pub struct LayoutTab {
    pub layout: Layout,
    next_id: u64,
    seq: u64,
    states: HashMap<u64, LayerState>,
    /// Bumped on every change; the preview catches up.
    version: u64,
    sent: u64,
    /// When the view last moved (data reloads wait for it to settle).
    moved_at: Instant,
    texture: Option<(u64, TextureHandle, Box<Layout>)>,
    /// The page elements, drawn on their own so dragging them is instant:
    /// each texture and the page area it was drawn for.
    element_tex: Vec<(Element, TextureHandle, layout::Rect)>,
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Done>,
    drag: Option<Drag>,
    pub format: Format,
    pub dpi: u32,
    exporting: bool,
    status: Option<(String, bool, Instant)>,
    /// Fit to the layers once the first extents arrive.
    fit_pending: bool,
    add_search: String,
    /// Elements the user has dragged; the rest stay at their usual spots.
    moved: HashSet<Element>,
    margins_each: bool,
    /// The view the in-view counts were made for.
    counted: Option<[f64; 4]>,
    /// Snap lines under the element being dragged (page mm).
    guides: [Option<f64>; 2],
}

impl LayoutTab {
    pub fn new(ctx: &egui::Context) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (tx, done) = mpsc::channel::<Done>();
        let ctx = ctx.clone();
        std::thread::Builder::new().name("map-layout".into()).spawn(move || render_loop(rx, tx, ctx)).ok();
        Self {
            layout: Layout::default(),
            next_id: 1,
            seq: 0,
            states: HashMap::new(),
            version: 1,
            sent: 0,
            moved_at: Instant::now(),
            texture: None,
            element_tex: Vec::new(),
            jobs,
            done,
            drag: None,
            format: Format::Pdf,
            dpi: 300,
            exporting: false,
            status: None,
            fit_pending: true,
            add_search: String::new(),
            moved: HashSet::new(),
            // Debug screenshot scenes: `RGRIP_DEMO_LAYOUT_GRID=1` shows the
            // graticule and the per-side margins.
            counted: None,
            guides: [None, None],
            margins_each: cfg!(debug_assertions) && std::env::var("RGRIP_DEMO_LAYOUT_GRID").is_ok(),
        }
        .with_demo_grid()
    }

    fn with_demo_grid(mut self) -> Self {
        if cfg!(debug_assertions) && std::env::var("RGRIP_DEMO_LAYOUT_GRID").is_ok() {
            self.layout.graticule.visible = true;
            self.layout.margin = [14.0, 18.0, 14.0, 18.0];
        }
        self
    }

    fn query(&mut self, layer: u64, kind: LayerQuery, sql: String) -> Action {
        self.seq += 1;
        let state = self.states.entry(layer).or_default();
        match kind {
            LayerQuery::Extent => state.extent = Some(self.seq),
            LayerQuery::Data => {}
        }
        Action { layer, seq: self.seq, kind, sql }
    }

    /// Add a table's geometry column as a layer; returns the queries to run.
    pub fn add_layer(&mut self, schema: &str, table: &str, column: &str) -> Vec<Action> {
        let id = self.next_id;
        self.next_id += 1;
        let color = PALETTE[(id as usize - 1) % PALETTE.len()];
        let layer = Layer::new(id, schema, table, column, color);
        let sql = layer.extent_sql();
        // New layers go on top.
        self.layout.layers.insert(0, layer);
        self.changed();
        vec![self.query(id, LayerQuery::Extent, sql)]
    }

    pub fn layer_result(&mut self, id: u64, seq: u64, result: Result<LayerResult, String>) {
        let Some(state) = self.states.get_mut(&id) else { return };
        let Some(layer) = self.layout.layers.iter_mut().find(|l| l.id == id) else { return };
        match result {
            Ok(LayerResult::Data { shapes, values, truncated }) => {
                let Some((s, bbox, scale)) = state.data else { return };
                if s != seq {
                    return;
                }
                state.data = None;
                state.truncated = truncated;
                state.bounds = shapes
                    .iter()
                    .map(|g| {
                        let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
                        g.for_each_coord(&mut |c| b = [b[0].min(c[0]), b[1].min(c[1]), b[2].max(c[0]), b[3].max(c[1])]);
                        b
                    })
                    .collect();
                layer.shapes = shapes.into();
                layer.values = values.into();
                layer.loaded = Some((bbox, scale));
                self.counted = None;
            }
            Ok(LayerResult::Extent(extent, total)) => {
                if state.extent != Some(seq) {
                    return;
                }
                state.extent = None;
                layer.extent = extent;
                layer.total = total;
                if extent.is_none() {
                    state.note = Some(("No geometry".into(), false));
                }
                if self.fit_pending && self.states.values().all(|s| s.extent.is_none()) {
                    self.fit_pending = false;
                    self.layout.fit();
                    self.moved_at = Instant::now() - Duration::from_secs(1);
                }
            }
            Err(e) => {
                state.data = None;
                state.extent = None;
                state.note = Some((e, true));
            }
        }
        self.changed();
    }

    fn changed(&mut self) {
        self.version += 1;
        for e in Element::ALL {
            if !self.moved.contains(&e) {
                self.layout.place(e);
            }
        }
    }

    fn view_changed(&mut self) {
        self.moved_at = Instant::now();
        self.changed();
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some((text.into(), error, Instant::now()));
    }

    /// Counts and categories from the features that touch the frame (the
    /// loaded data reaches past it, so panning doesn't wait on a reload).
    fn recount(&mut self, view: [f64; 4]) {
        let mut changed = false;
        for layer in self.layout.layers.iter_mut() {
            let Some(state) = self.states.get_mut(&layer.id) else { continue };
            if layer.loaded.is_none() {
                continue;
            }
            let inside: Vec<bool> = state.bounds.iter().map(|b| b[0] <= view[2] && b[2] >= view[0] && b[1] <= view[3] && b[3] >= view[1]).collect();
            let n = inside.iter().filter(|x| **x).count();
            let in_view = layout::thousands(n as f64);
            let note = if state.truncated {
                format!("First {} loaded: zoom in for the rest", layout::thousands(LAYER_LIMIT as f64))
            } else {
                match layer.total {
                    Some(t) => format!("{in_view} in view · {} in table", layout::thousands(t as f64)),
                    None => format!("{in_view} in view"),
                }
            };
            state.note = Some((note, false));
            let values: Vec<Option<String>> = layer.values.iter().zip(&inside).filter(|(_, i)| **i).map(|(v, _)| v.clone()).collect();
            let before = layer.style.clone();
            count_classes(&mut layer.style, &values);
            changed |= layer.style != before;
        }
        if changed {
            self.changed();
        }
    }

    fn busy(&self) -> bool {
        self.states.values().any(|s| s.data.is_some() || s.extent.is_some())
    }

    /// Once the view has settled, reload the layers it has moved past.
    fn reload_view(&mut self, actions: &mut Vec<Action>) {
        if self.moved_at.elapsed() < Duration::from_millis(350) || self.fit_pending && self.states.values().any(|s| s.extent.is_some()) {
            return;
        }
        let view = self.layout.view_bbox(0.0);
        if self.counted != Some(view) {
            self.counted = Some(view);
            self.recount(view);
        }
        let scale = self.layout.scale;
        let wanted: Vec<(u64, String)> = self
            .layout
            .layers
            .iter()
            .filter(|l| l.visible && l.needs_reload(view, scale))
            .filter(|l| self.states.get(&l.id).is_none_or(|s| s.data.is_none()))
            .map(|l| (l.id, l.data_sql(self.layout.view_bbox(0.5), scale, LAYER_LIMIT + 1)))
            .collect();
        let bbox = self.layout.view_bbox(0.5);
        for (id, sql) in wanted {
            let a = self.query(id, LayerQuery::Data, sql);
            if let Some(s) = self.states.get_mut(&id) {
                s.data = Some((a.seq, bbox, scale));
            }
            actions.push(a);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, catalog: &Catalog) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Ok(done) = self.done.try_recv() {
            match done {
                Done::Preview { version, image, layout, elements } => {
                    let tex = ui.ctx().load_texture("map-layout-preview", image, egui::TextureOptions::LINEAR);
                    self.texture = Some((version, tex, layout));
                    self.element_tex = elements
                        .into_iter()
                        .map(|(e, img, r)| (e, ui.ctx().load_texture(format!("map-layout-{e:?}"), img, egui::TextureOptions::LINEAR), r))
                        .collect();
                }
                Done::Exported(r) => {
                    self.exporting = false;
                    match r {
                        Ok(path) => self.notify(format!("Exported {}", path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())), false),
                        Err(e) => self.notify(format!("Export failed: {e}"), true),
                    }
                }
            }
        }

        egui::SidePanel::left(egui::Id::new(("layout-side", self as *const Self as usize)))
            .resizable(false)
            .exact_width(340.0)
            .frame(egui::Frame::new().fill(theme::SURFACE()).inner_margin(egui::Margin { left: 16, right: 10, top: 14, bottom: 14 }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(ui.available_width() - 8.0);
                    ui.spacing_mut().slider_width = 120.0;
                    self.side(ui, catalog, &mut actions);
                });
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG())).show_inside(ui, |ui| self.canvas(ui));
        self.reload_view(&mut actions);
        if self.busy() || self.moved_at.elapsed() < Duration::from_millis(400) {
            ui.ctx().request_repaint_after(Duration::from_millis(120));
        }
        actions
    }

    fn side(&mut self, ui: &mut Ui, catalog: &Catalog, actions: &mut Vec<Action>) {
        let before = self.layout.clone();

        section(ui, "Page", true);
        row(ui, "Size", |ui| {
            egui::ComboBox::from_id_salt("layout-page").width(150.0).selected_text(self.layout.page.label()).show_ui(ui, |ui| {
                for p in PageSize::ALL {
                    let (w, h) = p.mm();
                    ui.selectable_value(&mut self.layout.page, p, format!("{}  {:.0} × {:.0} mm", p.label(), w, h));
                }
            });
        });
        row(ui, "Orientation", |ui| {
            ui.selectable_value(&mut self.layout.landscape, false, "Portrait");
            ui.selectable_value(&mut self.layout.landscape, true, "Landscape");
        });
        // Each side: two rows of two fixed-width fields, so the panel keeps its width.
        let side = |ui: &mut Ui, name: &str, v: &mut f64| {
            ui.add_sized([44.0, 20.0], egui::Label::new(RichText::new(name).size(11.5).color(theme::FAINT())));
            ui.add_sized([56.0, 20.0], egui::DragValue::new(v).range(0.0..=60.0).speed(0.5).max_decimals(1).suffix(" mm"));
        };
        row(ui, "Margins", |ui| {
            if self.margins_each {
                let [t, _, b, _] = &mut self.layout.margin;
                side(ui, "Top", t);
                side(ui, "Bottom", b);
            } else {
                let mut all = self.layout.margin[0];
                if ui.add(egui::DragValue::new(&mut all).range(0.0..=60.0).speed(0.5).max_decimals(1).suffix(" mm")).changed() {
                    self.layout.margin = [all; 4];
                }
            }
        });
        if self.margins_each {
            row(ui, "", |ui| {
                let [_, r, _, l] = &mut self.layout.margin;
                side(ui, "Left", l);
                side(ui, "Right", r);
            });
        }
        row(ui, "", |ui| {
            if ui.checkbox(&mut self.margins_each, "Each side").changed() && !self.margins_each {
                self.layout.margin = [self.layout.margin[0]; 4];
            }
        });

        section(ui, "Map", false);
        row(ui, "Basemap", |ui| {
            egui::ComboBox::from_id_salt("layout-basemap").width(150.0).selected_text(self.layout.basemap.label()).show_ui(ui, |ui| {
                for b in Basemap::ALL {
                    ui.selectable_value(&mut self.layout.basemap, b, b.label());
                }
            });
        });
        let mut fit = false;
        row(ui, "Scale", |ui| {
            ui.label(RichText::new("1 :").color(theme::MUTED()));
            let speed = self.layout.scale * 0.005;
            ui.add(egui::DragValue::new(&mut self.layout.scale).range(100.0..=50_000_000.0).speed(speed).max_decimals(0));
            fit = ui.button("Fit").on_hover_text("Fit the map to the layers").clicked();
        });
        if fit {
            self.layout.fit();
        }
        hint(ui, "Drag the map to pan, scroll over it to zoom");
        let g = &mut self.layout.graticule;
        row(ui, "Graticule", |ui| {
            ui.checkbox(&mut g.visible, "Show").on_hover_text("A latitude/longitude grid, labeled along the frame");
            if g.visible {
                ui.checkbox(&mut g.labels, "Labels");
            }
        });
        if g.visible {
            let auto = format!("Auto ({})", layout::step_label(self.layout.graticule_step()));
            let g = &mut self.layout.graticule;
            row(ui, "Every", |ui| {
                let text = g.interval.map_or(auto.clone(), layout::step_label);
                egui::ComboBox::from_id_salt("layout-grid-step").width(110.0).selected_text(text).show_ui(ui, |ui| {
                    ui.selectable_value(&mut g.interval, None, auto.as_str());
                    for &step in layout::graticule_steps(g.dms) {
                        ui.selectable_value(&mut g.interval, Some(step), layout::step_label(step));
                    }
                });
            });
            row(ui, "Labels as", |ui| {
                let dms = g.dms;
                ui.selectable_value(&mut g.dms, false, "104.92°");
                ui.selectable_value(&mut g.dms, true, "104°55′");
                // A fixed step from the other format would make odd labels.
                if g.dms != dms {
                    g.interval = None;
                }
            });
            row(ui, "Line", |ui| {
                ui.color_edit_button_srgb(&mut g.color);
                ui.add(egui::Slider::new(&mut g.opacity, 0.1..=1.0).max_decimals(2)).on_hover_text("Line opacity");
            });
        }

        section(ui, "Layers", false);
        self.add_layer_ui(ui, catalog, actions);
        let mut remove = None;
        let mut swap = None;
        let count = self.layout.layers.len();
        for i in 0..count {
            let id = self.layout.layers[i].id;
            let columns: Vec<String> = catalog
                .tables
                .iter()
                .find(|t| t.schema == self.layout.layers[i].schema && t.name == self.layout.layers[i].table)
                .map(|t| t.columns.iter().filter(|(_, ty)| !ty.starts_with("geometry") && !ty.starts_with("geography")).map(|(c, _)| c.clone()).collect())
                .unwrap_or_default();
            let mut queries = Vec::new();
            let state = self.states.entry(id).or_default();
            let loading = state.data.is_some() || state.extent.is_some();
            let layer = &mut self.layout.layers[i];
            egui::Frame::new().fill(theme::BG()).corner_radius(8).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let chevron = if state.open { Svg::ChevronDown } else { Svg::ChevronRight };
                    if ui.add(egui::Button::image(icons::image(ui.ctx(), chevron, 14.0, theme::MUTED())).frame(false)).on_hover_text("Style, filter and colors").clicked() {
                        state.open = !state.open;
                    }
                    ui.checkbox(&mut layer.visible, "").on_hover_text("Show on the map");
                    if matches!(layer.style, Style::Single) {
                        ui.color_edit_button_srgb(&mut layer.color);
                    }
                    label_edit(ui, &mut layer.label, "Legend label", ui.available_width(), true);
                });
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    if loading {
                        ui.add(egui::Spinner::new().size(11.0));
                    }
                    let sub = format!("{}.{} · {}", layer.schema, layer.table, layer.column);
                    ui.label(RichText::new(sub).size(11.0).color(theme::FAINT()));
                });
                if let Some((text, err)) = &state.note {
                    ui.horizontal(|ui| {
                        ui.add_space(22.0);
                        ui.add(egui::Label::new(RichText::new(text).size(11.0).color(if *err { theme::ERROR() } else { theme::MUTED() })).wrap());
                    });
                }
                if !state.open {
                    return;
                }
                ui.add_space(6.0);
                egui::Grid::new(("layer-style", id)).num_columns(2).spacing([10.0, 6.0]).min_col_width(52.0).show(ui, |ui| {
                    let label = |ui: &mut Ui, t: &str| {
                        ui.label(RichText::new(t).size(12.0).color(theme::MUTED()));
                    };
                    label(ui, "Filter");
                    ui.horizontal(|ui| {
                        let edit = ui.add(TextEdit::singleline(&mut layer.filter).hint_text("SQL WHERE").font(egui::TextStyle::Monospace).desired_width(ui.available_width() - 56.0));
                        let apply = ui.small_button("Apply").clicked() || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                        if apply {
                            layer.loaded = None;
                            queries.push((LayerQuery::Extent, layer.extent_sql()));
                        }
                    });
                    ui.end_row();
                    label(ui, "Color by");
                    let current = layer.category_column().map(str::to_string);
                    let mut pick: Option<Option<String>> = None;
                    egui::ComboBox::from_id_salt(("color-by", id)).width(ui.available_width() - 10.0).selected_text(current.as_deref().unwrap_or("One color")).show_ui(ui, |ui| {
                        if ui.selectable_label(current.is_none(), "One color").clicked() {
                            pick = Some(None);
                        }
                        for c in &columns {
                            if ui.selectable_label(current.as_deref() == Some(c), c).clicked() {
                                pick = Some(Some(c.clone()));
                            }
                        }
                    });
                    match pick {
                        Some(None) => {
                            layer.style = Style::Single;
                            layer.values = Default::default();
                        }
                        Some(Some(c)) if current.as_deref() != Some(&c) => {
                            layer.style = Style::Categories { column: c, classes: Vec::new(), more: false };
                            // Reload with the column's values.
                            layer.loaded = None;
                        }
                        _ => {}
                    }
                    ui.end_row();
                    label(ui, "Line");
                    ui.add(egui::Slider::new(&mut layer.width, 0.05..=2.0).suffix(" mm").max_decimals(2));
                    ui.end_row();
                    label(ui, "Point");
                    ui.add(egui::Slider::new(&mut layer.point, 0.3..=5.0).suffix(" mm").max_decimals(1));
                    ui.end_row();
                    label(ui, "Fill");
                    ui.add(egui::Slider::new(&mut layer.fill_opacity, 0.0..=1.0).max_decimals(2));
                    ui.end_row();
                });
                if let Style::Categories { classes, more, .. } = &mut layer.style {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Categories in view").size(11.5).color(theme::MUTED()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new("In view").size(11.0).color(theme::FAINT()));
                        });
                    });
                    hint(ui, "Edit a label to rename it in the legend; Enter starts a new line");
                    // Fixed columns: check, color, label, count (right-aligned).
                    const COUNT_W: f32 = 50.0;
                    let gap = ui.spacing().item_spacing.x;
                    let label_w = (ui.available_width() - 22.0 - 40.0 - COUNT_W - 3.0 * gap - 12.0).max(60.0);
                    let mut shown = 0;
                    for c in classes.iter_mut().filter(|c| c.count > 0).take(layout::MAX_CLASSES) {
                        shown += 1;
                        ui.horizontal_top(|ui| {
                            ui.add_sized([22.0, 22.0], egui::Checkbox::without_text(&mut c.visible)).on_hover_text("Show this value on the map and in the legend");
                            ui.color_edit_button_srgb(&mut c.color);
                            label_edit(ui, &mut c.label, "", label_w, true);
                            ui.allocate_ui_with_layout(vec2(COUNT_W, 22.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.set_width(COUNT_W);
                                ui.label(RichText::new(layout::thousands(c.count as f64)).size(11.0).color(theme::FAINT()));
                            });
                        });
                    }
                    if shown == 0 {
                        hint(ui, "No features in view");
                    }
                    if *more {
                        hint(ui, &format!("More values in view than {}: the rest draw as \"Other\" in the layer color", layout::MAX_CLASSES));
                    }
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if i > 0 && ui.add(egui::Button::image(icons::image(ui.ctx(), Svg::ArrowUp, 13.0, theme::MUTED()))).on_hover_text("Draw above the layer before").clicked() {
                        swap = Some((i, i - 1));
                    }
                    if i + 1 < count && ui.add(egui::Button::image(icons::image(ui.ctx(), Svg::ArrowDown, 13.0, theme::MUTED()))).on_hover_text("Draw below the next layer").clicked() {
                        swap = Some((i, i + 1));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::image_and_text(icons::image(ui.ctx(), Svg::Trash, 13.0, theme::ERROR()), RichText::new("Remove").color(theme::ERROR()))).clicked() {
                            remove = Some(i);
                        }
                    });
                });
            });
            ui.add_space(6.0);
            for (kind, sql) in queries {
                actions.push(self.query(id, kind, sql));
            }
        }
        if let Some((a, b)) = swap {
            self.layout.layers.swap(a, b);
        }
        if let Some(i) = remove {
            let id = self.layout.layers.remove(i).id;
            self.states.remove(&id);
        }


        section(ui, "On the page", false);
        hint(ui, "Drag them on the page to place them");
        let mut vis = self.layout.items.map(|i| i.visible);
        egui::Grid::new("layout-elements").num_columns(2).spacing([10.0, 6.0]).min_col_width(LABEL_W).show(ui, |ui| {
            ui.checkbox(&mut vis[Element::Title as usize], "Title");
            label_edit(ui, &mut self.layout.title, "Map title", ui.available_width() - 10.0, false);
            ui.end_row();
            ui.label("");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.layout.title_size).range(8.0..=60.0).suffix(" pt")).on_hover_text("Title size");
                ui.color_edit_button_srgb(&mut self.layout.title_color).on_hover_text("Title color");
                ui.toggle_value(&mut self.layout.title_bold, RichText::new("B").strong()).on_hover_text("Bold");
                ui.toggle_value(&mut self.layout.title_halo, "Outline").on_hover_text("A white outline around the letters, so the title reads over the map");
            });
            ui.end_row();
            ui.checkbox(&mut vis[Element::Legend as usize], "Legend");
            label_edit(ui, &mut self.layout.legend_title, "Legend title", ui.available_width() - 10.0, false);
            ui.end_row();
            ui.label("");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.layout.legend_size).range(5.0..=24.0).speed(0.1).max_decimals(1).suffix(" pt")).on_hover_text("Legend text size");
                ui.color_edit_button_srgb(&mut self.layout.legend_color).on_hover_text("Legend text color");
                ui.toggle_value(&mut self.layout.legend_box, "Box").on_hover_text("A white box behind the legend");
            });
            ui.end_row();
            ui.checkbox(&mut vis[Element::ScaleBar as usize], "Scale bar");
            ui.label("");
            ui.end_row();
            ui.checkbox(&mut vis[Element::North as usize], "North arrow");
            ui.label("");
            ui.end_row();
        });
        for e in Element::ALL {
            self.layout.item_mut(e).visible = vis[e as usize];
        }
        if ui.small_button("Reset positions").clicked() {
            self.moved.clear();
            self.layout.place_defaults();
        }

        section(ui, "Export", false);
        row(ui, "Format", |ui| {
            ui.selectable_value(&mut self.format, Format::Pdf, "PDF");
            ui.selectable_value(&mut self.format, Format::Png, "PNG");
            if self.format == Format::Png {
                egui::ComboBox::from_id_salt("layout-dpi").width(76.0).selected_text(format!("{} dpi", self.dpi)).show_ui(ui, |ui| {
                    for d in [150, 300, 600] {
                        ui.selectable_value(&mut self.dpi, d, format!("{d} dpi"));
                    }
                });
            }
        });
        let (w, h) = self.layout.page_mm();
        let what = match self.format {
            Format::Pdf => format!("{} {}, vector shapes and text", self.layout.page.label(), if self.layout.landscape { "landscape" } else { "portrait" }),
            Format::Png => format!("{} × {} px", (w / 25.4 * self.dpi as f64).round(), (h / 25.4 * self.dpi as f64).round()),
        };
        row(ui, "", |ui| hint(ui, &what));
        ui.add_space(6.0);
        let busy = self.exporting || self.busy();
        let label = if self.exporting { "Exporting…" } else { "Export…" };
        let button = egui::Button::image_and_text(icons::image(ui.ctx(), Svg::Download, 14.0, theme::BG()), RichText::new(label).color(theme::BG())).fill(theme::ACCENT());
        if ui.add_enabled(!busy, button.min_size(vec2(ui.available_width(), 30.0))).clicked() {
            self.export();
        }
        if let Some((text, err, at)) = &self.status
            && at.elapsed() < Duration::from_secs(6)
        {
            ui.add_space(4.0);
            ui.label(RichText::new(text).size(12.0).color(if *err { theme::ERROR() } else { theme::OK() }));
        }

        if self.layout.page != before.page || self.layout.landscape != before.landscape || self.layout.margin != before.margin {
            self.layout.clamp_items();
        }
        if !same(&before, &self.layout) {
            if self.layout.scale != before.scale || self.layout.page != before.page || self.layout.landscape != before.landscape || self.layout.margin != before.margin || self.layout.center != before.center {
                self.view_changed();
            } else {
                self.changed();
            }
        }
    }

    fn add_layer_ui(&mut self, ui: &mut Ui, catalog: &Catalog, actions: &mut Vec<Action>) {
        let geo: Vec<(&str, &str, &str)> = catalog
            .tables
            .iter()
            .flat_map(|t| {
                t.columns
                    .iter()
                    .filter(|(_, ty)| ty.starts_with("geometry") || ty.starts_with("geography"))
                    .map(move |(c, _)| (t.schema.as_str(), t.name.as_str(), c.as_str()))
            })
            .collect();
        let mut add = None;
        egui::ComboBox::from_id_salt("layout-add")
            .width(ui.available_width() - 10.0)
            .selected_text(RichText::new("+ Add a table…").color(theme::ACCENT()))
            // Clicking the filter field mustn't close the list.
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                if geo.len() > 8 {
                    ui.add(TextEdit::singleline(&mut self.add_search).hint_text("Filter…").desired_width(240.0));
                }
                let needle = self.add_search.to_lowercase();
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    if geo.is_empty() {
                        ui.label(RichText::new("No tables with a geometry column").color(theme::MUTED()));
                    }
                    for (s, t, c) in geo.iter().filter(|(s, t, _)| format!("{s}.{t}").to_lowercase().contains(&needle)) {
                        let label = if geo.iter().filter(|(s2, t2, _)| s2 == s && t2 == t).count() > 1 { format!("{s}.{t} · {c}") } else { format!("{s}.{t}") };
                        if ui.selectable_label(false, label).clicked() {
                            add = Some((s.to_string(), t.to_string(), c.to_string()));
                            ui.close();
                        }
                    }
                });
            });
        if let Some((s, t, c)) = add {
            let acts = self.add_layer(&s, &t, &c);
            actions.extend(acts);
        }
        ui.add_space(6.0);
    }

    /// Debug screenshot scenes: export straight to `path` once every layer
    /// has loaded. Returns true when started.
    #[cfg(debug_assertions)]
    pub fn demo_export(&mut self, path: PathBuf) -> bool {
        if self.busy() || self.layout.layers.is_empty() || self.exporting || self.layout.layers.iter().any(|l| l.loaded.is_none()) {
            return false;
        }
        self.format = if path.extension().is_some_and(|e| e == "png") { Format::Png } else { Format::Pdf };
        self.exporting = true;
        let dpi = if self.format == Format::Pdf { 300.0 } else { self.dpi as f64 };
        let _ = self.jobs.send(Job::Export { layout: self.layout.clone(), format: self.format, dpi, path });
        true
    }

    /// Debug screenshot scenes: zoom the map to `scale`.
    #[cfg(debug_assertions)]
    pub fn demo_zoom(&mut self, scale: f64) {
        self.layout.scale = scale;
        self.view_changed();
    }

    /// Debug screenshot scenes: color the first layer by `column`.
    #[cfg(debug_assertions)]
    pub fn demo_color_by(&mut self, column: &str) -> Vec<Action> {
        let Some(layer) = self.layout.layers.first_mut() else { return Vec::new() };
        layer.style = Style::Categories { column: column.into(), classes: Vec::new(), more: false };
        layer.loaded = None;
        let id = layer.id;
        self.states.entry(id).or_default().open = true;
        Vec::new()
    }

    fn export(&mut self) {
        let (ext, filter) = match self.format {
            Format::Pdf => ("pdf", "PDF"),
            Format::Png => ("png", "PNG image"),
        };
        let base = if self.layout.title.trim().is_empty() { "map".to_string() } else { self.layout.title.trim().replace(['/', ':', '\\'], "-") };
        let Some(path) = rfd::FileDialog::new().set_file_name(format!("{base}.{ext}")).add_filter(filter, &[ext]).save_file() else { return };
        let (w, h) = self.layout.page_mm();
        let px = (w / 25.4 * self.dpi as f64) * (h / 25.4 * self.dpi as f64);
        if self.format == Format::Png && px > MAX_PIXELS {
            self.notify("That's too many pixels: pick a lower dpi or a smaller page", true);
            return;
        }
        self.exporting = true;
        let dpi = if self.format == Format::Pdf { 300.0 } else { self.dpi as f64 };
        let _ = self.jobs.send(Job::Export { layout: self.layout.clone(), format: self.format, dpi, path });
    }

    fn canvas(&mut self, ui: &mut Ui) {
        let area = ui.available_rect_before_wrap().shrink(24.0);
        let (pw, ph) = self.layout.page_mm();
        let scale = (area.width() / pw as f32).min(area.height() / ph as f32).max(0.5);
        let page = Rect::from_center_size(area.center(), vec2(pw as f32 * scale, ph as f32 * scale));
        let to_screen = |p: [f64; 2]| pos2(page.left() + p[0] as f32 * scale, page.top() + p[1] as f32 * scale);
        let to_mm = |p: Pos2| [((p.x - page.left()) / scale) as f64, ((p.y - page.top()) / scale) as f64];
        let painter = ui.painter_at(ui.max_rect());

        // Ask for a fresh preview once the last one is out of date, but not
        // mid-zoom or mid-pan: the old picture is stretched meanwhile.
        let settling = self.moved_at.elapsed() < Duration::from_millis(180) || matches!(self.drag, Some(Drag::Pan(_)));
        if self.sent != self.version && !settling {
            let px_per_mm = scale * ui.ctx().pixels_per_point();
            let _ = self.jobs.send(Job::Preview { version: self.version, layout: self.layout.clone(), px_per_mm });
            self.sent = self.version;
        }

        painter.rect_filled(page.translate(vec2(0.0, 6.0)).expand(2.0), 4.0, Color32::from_black_alpha(90));
        painter.rect_filled(page, 0.0, Color32::WHITE);
        if let Some((_, tex, drawn)) = &self.texture {
            let full = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
            let same_view = drawn.center == self.layout.center && drawn.scale == self.layout.scale && drawn.frame() == self.layout.frame();
            painter.image(tex.id(), page, full, Color32::WHITE);
            if !same_view && drawn.page_mm() == self.layout.page_mm() {
                // Where the old map now sits: its frame corners, through the
                // new view. Draw that part of the old picture there.
                let f = drawn.frame();
                let a = self.layout.to_page(drawn.from_page([f.x, f.y]));
                let b = self.layout.to_page(drawn.from_page([f.x + f.w, f.y + f.h]));
                let nf = self.layout.frame();
                let clip = Rect::from_min_max(to_screen([nf.x, nf.y]), to_screen([nf.x + nf.w, nf.y + nf.h]));
                let uv = Rect::from_min_max(pos2((f.x / pw) as f32, (f.y / ph) as f32), pos2(((f.x + f.w) / pw) as f32, ((f.y + f.h) / ph) as f32));
                let framed = painter.with_clip_rect(clip);
                framed.rect_filled(clip, 0.0, Color32::from_gray(238));
                framed.image(tex.id(), Rect::from_min_max(to_screen(a), to_screen(b)), uv, Color32::WHITE);
            }
        }
        // Page elements, each at its current spot.
        if let Some((_, _, drawn)) = &self.texture {
            for (e, tex, r) in &self.element_tex {
                if !self.layout.item(*e).visible {
                    continue;
                }
                let (now, then) = (self.layout.item(*e).pos, drawn.item(*e).pos);
                let (dx, dy) = (now[0] - then[0], now[1] - then[1]);
                let rect = Rect::from_min_max(to_screen([r.x + dx, r.y + dy]), to_screen([r.x + r.w + dx, r.y + r.h + dy]));
                painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            }
        }
        if self.texture.as_ref().is_none_or(|(v, _, _)| *v != self.version) || self.busy() {
            painter.text(page.right_bottom() + vec2(-8.0, -8.0), Align2::RIGHT_BOTTOM, if self.busy() { "Loading…" } else { "Rendering…" }, FontId::proportional(11.0), Color32::from_gray(120));
        }

        // Interaction: elements first (on top), then the map frame.
        let response = ui.interact(page, egui::Id::new(("layout-page", self as *const Self as usize)), Sense::click_and_drag());
        let boxes = self.layout.boxes();
        let hover = response.hover_pos().map(to_mm);
        let element_at = |p: [f64; 2], layout: &Layout| {
            Element::ALL.into_iter().rev().find(|e| layout.item(*e).visible && boxes[*e as usize].contains(p) && (*e != Element::Title || !layout.title.trim().is_empty()))
        };
        // Where the press began: egui reports the drag a few pixels later.
        if response.drag_started()
            && let Some(p) = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos()).map(to_mm)
        {
            self.drag = match element_at(p, &self.layout) {
                Some(e) => {
                    let pos = self.layout.item(e).pos;
                    Some(Drag::Element(e, [p[0] - pos[0], p[1] - pos[1]]))
                }
                None if self.layout.frame().contains(p) => Some(Drag::Pan(p)),
                None => None,
            };
        }
        if response.dragged()
            && let Some(p) = response.interact_pointer_pos().map(to_mm)
        {
            match &mut self.drag {
                Some(Drag::Element(e, grab)) => {
                    self.moved.insert(*e);
                    let b = boxes[*e as usize];
                    let (pos, guides) = snap([p[0] - grab[0], p[1] - grab[1]], [b.w, b.h], &self.layout);
                    self.guides = guides;
                    // Only the element's own picture moves: no re-render.
                    self.layout.item_mut(*e).pos = pos;
                    self.layout.clamp_items();
                }
                Some(Drag::Pan(last)) => {
                    let d = [p[0] - last[0], p[1] - last[1]];
                    *last = p;
                    self.layout.pan(d);
                    self.view_changed();
                }
                None => {}
            }
        }
        if response.drag_stopped() {
            self.drag = None;
            self.guides = [None, None];
        }
        // Snap guides while dragging an element.
        if matches!(self.drag, Some(Drag::Element(..))) {
            let guide = Stroke::new(1.0_f32, theme::ACCENT());
            if let Some(x) = self.guides[0] {
                let x = to_screen([x, 0.0]).x;
                painter.add(egui::Shape::dashed_line(&[pos2(x, page.top()), pos2(x, page.bottom())], guide, 5.0, 4.0));
            }
            if let Some(y) = self.guides[1] {
                let y = to_screen([0.0, y]).y;
                painter.add(egui::Shape::dashed_line(&[pos2(page.left(), y), pos2(page.right(), y)], guide, 5.0, 4.0));
            }
        }
        if let Some(p) = hover
            && self.layout.frame().contains(p)
        {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.1 {
                self.layout.zoom_at((scroll as f64 / 200.0).exp(), p);
                self.view_changed();
            }
        }

        // Outlines: the hovered or dragged element, so it reads as movable.
        let active = match &self.drag {
            Some(Drag::Element(e, _)) => Some(*e),
            _ => hover.and_then(|p| element_at(p, &self.layout)),
        };
        if let Some(e) = active {
            let b = self.layout.boxes()[e as usize];
            let r = Rect::from_min_max(to_screen([b.x, b.y]), to_screen([b.x + b.w, b.y + b.h])).expand(2.0);
            painter.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.5_f32, theme::ACCENT()), egui::StrokeKind::Outside);
            ui.ctx().set_cursor_icon(if matches!(self.drag, Some(Drag::Element(..))) { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        } else if hover.is_some_and(|p| self.layout.frame().contains(p)) {
            ui.ctx().set_cursor_icon(if matches!(self.drag, Some(Drag::Pan(_))) { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Move });
        }
        if self.layout.layers.is_empty() {
            let f = self.layout.frame();
            painter.text(
                to_screen([f.x + f.w / 2.0, f.y + f.h / 2.0]),
                Align2::CENTER_CENTER,
                "Add a table with a geometry column (left)",
                FontId::proportional(14.0),
                Color32::from_gray(110),
            );
        }
    }
}

/// Update a categorized layer's classes from the values in view: counts,
/// new values (each keeps its color and label once seen), most first.
fn count_classes(style: &mut Style, values: &[Option<String>]) {
    let Style::Categories { classes, more, .. } = style else { return };
    let mut counts: HashMap<Option<String>, i64> = HashMap::new();
    for v in values {
        *counts.entry(v.clone()).or_default() += 1;
    }
    for c in classes.iter_mut() {
        c.count = counts.remove(&c.value).unwrap_or(0);
    }
    let mut new: Vec<(Option<String>, i64)> = counts.into_iter().collect();
    new.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (value, count) in new {
        let color = CLASS_COLORS[classes.len() % CLASS_COLORS.len()];
        #[allow(unused_mut)]
        let mut label = value.clone().unwrap_or_else(|| "(empty)".to_string());
        // Debug screenshot scenes: `RGRIP_DEMO_LAYOUT_CLASS_LABEL` renames the first class.
        #[cfg(debug_assertions)]
        if classes.is_empty()
            && let Ok(l) = std::env::var("RGRIP_DEMO_LAYOUT_CLASS_LABEL")
        {
            label = l.replace("\\n", "\n");
        }
        classes.push(Class { value, label, color, visible: true, count });
    }
    // Most features first; values gone from view keep their color and label.
    classes.sort_by(|a, b| b.count.cmp(&a.count));
    *more = classes.iter().filter(|c| c.count > 0).count() > layout::MAX_CLASSES;
}

/// A label field: Enter adds a line break (for the legend), and Khmer is
/// drawn shaped over egui's unshaped text, as in the SQL editor.
fn label_edit(ui: &mut Ui, text: &mut String, hint_text: &str, width: f32, multiline: bool) -> egui::Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let color = ui.visuals().text_color();
    let mut khmer = Vec::new();
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let mut job = egui::text::LayoutJob::simple(buf.as_str().to_owned(), font.clone(), color, wrap);
        khmer = crate::shaping::hide_runs(ui, &mut job);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let out = ui
        .allocate_ui(vec2(width, 22.0), |ui| {
            ui.set_max_width(width);
            let edit = if multiline { TextEdit::multiline(text).desired_rows(1) } else { TextEdit::singleline(text) };
            edit.desired_width(width - 8.0).hint_text(hint_text).layouter(&mut layouter).show(ui)
        })
        .inner;
    crate::shaping::paint_runs(ui, &out.galley, out.galley_pos, &khmer);
    out.response
}

/// A section heading, with a rule above it (except the first).
fn section(ui: &mut Ui, title: &str, first: bool) {
    if !first {
        ui.add_space(10.0);
        ui.separator();
    }
    ui.add_space(4.0);
    ui.label(RichText::new(title).size(13.0).strong().color(theme::TEXT()));
    ui.add_space(4.0);
}

/// A labeled row: the label in a fixed, left-aligned column so controls line up.
fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(vec2(LABEL_W, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(LABEL_W);
            ui.label(RichText::new(label).size(12.5).color(theme::MUTED()));
        });
        add(ui);
    });
}

fn hint(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(11.0).color(theme::FAINT())).wrap());
}

/// Snap an element's top-left when within 2 mm of: the page's edges (5 mm
/// in) and center, the frame's edges (5 mm in, or just outside, as for a
/// title above the map) and center. Returns the position and the lines it
/// snapped to (x for vertical guides, y for horizontal), for drawing.
fn snap(pos: [f64; 2], size: [f64; 2], layout: &Layout) -> ([f64; 2], [Option<f64>; 2]) {
    let f = layout.frame();
    let (pw, ph) = layout.page_mm();
    let inset = 5.0;
    let gap = 2.0;
    // (where the element's left/top goes, the guide line it lines up with)
    let xs = [
        (inset, inset),
        (pw - inset - size[0], pw - inset),
        ((pw - size[0]) / 2.0, pw / 2.0),
        (f.x + inset, f.x + inset),
        (f.x + f.w - inset - size[0], f.x + f.w - inset),
        (f.x + (f.w - size[0]) / 2.0, f.x + f.w / 2.0),
    ];
    let ys = [
        (inset, inset),
        (ph - inset - size[1], ph - inset),
        ((ph - size[1]) / 2.0, ph / 2.0),
        (f.y + inset, f.y + inset),
        (f.y + f.h - inset - size[1], f.y + f.h - inset),
        (f.y + (f.h - size[1]) / 2.0, f.y + f.h / 2.0),
        (f.y - gap - size[1], f.y - gap),
        (f.y + f.h + gap, f.y + f.h + gap),
    ];
    let pick = |v: f64, targets: &[(f64, f64)]| {
        targets.iter().copied().filter(|(t, _)| (t - v).abs() < 2.0).min_by(|a, b| (a.0 - v).abs().total_cmp(&(b.0 - v).abs())).map_or((v, None), |(t, g)| (t, Some(g)))
    };
    let (x, gx) = pick(pos[0], &xs);
    let (y, gy) = pick(pos[1], &ys);
    ([x, y], [gx, gy])
}

/// Layouts compare by everything that changes the picture (shapes by
/// identity: they're replaced, never edited).
fn same(a: &Layout, b: &Layout) -> bool {
    a.page == b.page
        && a.landscape == b.landscape
        && a.margin == b.margin
        && a.center == b.center
        && a.scale == b.scale
        && a.basemap == b.basemap
        && a.title == b.title
        && a.title_size == b.title_size
        && a.title_color == b.title_color
        && a.title_bold == b.title_bold
        && a.title_halo == b.title_halo
        && a.legend_size == b.legend_size
        && a.legend_color == b.legend_color
        && a.legend_box == b.legend_box
        && a.legend_title == b.legend_title
        && a.items == b.items
        && a.graticule == b.graticule
        && a.layers.len() == b.layers.len()
        && a.layers.iter().zip(&b.layers).all(|(x, y)| {
            x.id == y.id
                && x.label == y.label
                && x.color == y.color
                && x.width == y.width
                && x.point == y.point
                && x.fill_opacity == y.fill_opacity
                && x.visible == y.visible
                && x.style == y.style
                && std::sync::Arc::ptr_eq(&x.shapes, &y.shapes)
                && std::sync::Arc::ptr_eq(&x.values, &y.values)
        })
}

/// The SVG for `layout`, with tiles from `cache`, filled from disk and (with
/// `network`) downloaded as needed. Also returns whether tiles are missing.
fn page_svg(layout: &Layout, dpi: f64, cache: &mut HashMap<(Basemap, u8, u32, u32), Vec<u8>>, network: bool) -> (String, bool) {
    page_svg_with(layout, dpi, cache, network, true)
}

/// [`page_svg`], optionally without the page elements (the preview draws them apart).
fn page_svg_with(layout: &Layout, dpi: f64, cache: &mut HashMap<(Basemap, u8, u32, u32), Vec<u8>>, network: bool, elements: bool) -> (String, bool) {
    let (z, tiles) = layout.tiles(dpi);
    let missing: Vec<(u32, u32)> = tiles.iter().copied().filter(|(x, y)| !cache.contains_key(&(layout.basemap, z, *x, *y))).collect();
    let mut incomplete = false;
    if !missing.is_empty() {
        let got = layout::fetch_tiles(layout.basemap, z, &missing, network);
        incomplete = got.len() < missing.len();
        for ((x, y), bytes) in got {
            cache.insert((layout.basemap, z, x, y), bytes);
        }
    }
    if cache.len() > 2000 {
        cache.clear();
    }
    let basemap = layout.basemap;
    let mut tile = |z, x, y| cache.get(&(basemap, z, x, y)).cloned();
    let svg = if elements { layout.svg(&mut tile, dpi) } else { layout.svg_map(&mut tile, dpi) };
    (svg, incomplete)
}

fn render_loop(jobs: mpsc::Receiver<Job>, done: mpsc::Sender<Done>, ctx: egui::Context) {
    let mut cache = HashMap::new();
    while let Ok(first) = jobs.recv() {
        // Only the newest preview matters; exports all run.
        let mut queue = vec![first];
        while let Ok(more) = jobs.try_recv() {
            queue.push(more);
        }
        let latest_preview = queue.iter().rposition(|j| matches!(j, Job::Preview { .. }));
        for (i, job) in queue.into_iter().enumerate() {
            match job {
                Job::Preview { version, layout, px_per_mm } => {
                    if Some(i) != latest_preview {
                        continue;
                    }
                    let dpi = (px_per_mm as f64 * 25.4).clamp(48.0, 150.0);
                    // The SVG is in mm; usvg sizes it at 96 px per inch.
                    let raster = |svg: &str| {
                        snapshot::render_at(svg, px_per_mm * 25.4 / 96.0).map(|p| ColorImage::from_rgba_premultiplied([p.width() as usize, p.height() as usize], p.data()))
                    };
                    let elements: Vec<(Element, ColorImage, layout::Rect)> =
                        Element::ALL.into_iter().filter_map(|e| layout.element_svg(e).and_then(|(svg, r)| raster(&svg).map(|img| (e, img, r)))).collect();
                    let render = |svg: &str| {
                        if let Some(image) = raster(svg) {
                            let _ = done.send(Done::Preview { version, image, layout: Box::new(layout.clone()), elements: elements.clone() });
                            ctx.request_repaint();
                        }
                    };
                    // Draw now with the tiles at hand, then again once the
                    // missing ones are downloaded.
                    let (svg, incomplete) = page_svg_with(&layout, dpi, &mut cache, false, false);
                    render(&svg);
                    if incomplete {
                        let (svg, _) = page_svg_with(&layout, dpi, &mut cache, true, false);
                        render(&svg);
                    }
                }
                Job::Export { layout, format, dpi, path } => {
                    let (svg, _) = page_svg(&layout, dpi, &mut cache, true);
                    let result = match format {
                        Format::Png => snapshot::render_at(&svg, (dpi / 96.0) as f32).ok_or_else(|| "couldn't render the page".to_string()).and_then(|p| p.encode_png().map_err(|e| e.to_string())),
                        Format::Pdf => pdf(&svg),
                    }
                    .and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| e.to_string()))
                    .map(|_| path);
                    let _ = done.send(Done::Exported(result));
                    ctx.request_repaint();
                }
            }
        }
    }
}

/// The page as a PDF: shapes and text stay vector, tiles are images.
fn pdf(svg: &str) -> Result<Vec<u8>, String> {
    use svg2pdf::usvg;
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    let options = usvg::Options { fontdb: std::sync::Arc::new(db), ..Default::default() };
    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| e.to_string())?;
    // usvg sizes the mm page at 96 px per inch; dpi 96 keeps it to size.
    let page = svg2pdf::PageOptions { dpi: 96.0 };
    svg2pdf::to_pdf(&tree, svg2pdf::ConversionOptions::default(), page).map_err(|e| format!("{e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_to_page_and_frame() {
        let mut l = Layout::default();
        l.margin = [30.0, 10.0, 10.0, 10.0];
        let (pw, _) = l.page_mm();
        let f = l.frame();
        // Near the page's center line: centered on it, with a guide there.
        let (p, g) = snap([(pw - 40.0) / 2.0 + 1.5, 50.0], [40.0, 8.0], &l);
        assert_eq!(p[0], (pw - 40.0) / 2.0);
        assert_eq!(g[0], Some(pw / 2.0));
        // Just above the frame (a title in the top margin).
        let (p, g) = snap([60.0, f.y - 2.0 - 8.0 + 1.0], [40.0, 8.0], &l);
        assert_eq!(p[1], f.y - 10.0);
        assert_eq!(g[1], Some(f.y - 2.0));
        // Far from everything: left alone.
        let (p, g) = snap([61.3, 77.7], [40.0, 8.0], &l);
        assert_eq!((p, g), ([61.3, 77.7], [None, None]));
    }
}
