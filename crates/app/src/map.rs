//! Built-in map for PostGIS results: slippy-map basemap (walkers) with the
//! result's geometries drawn on top.

use crate::{shaping, theme};
use eframe::egui::{
    self, DragPanButtons, Align2, Color32, FontId, Frame, Margin, Mesh, Pos2, Rect, RichText, Shape, Stroke, Ui, Vec2,
    vec2,
};
use rgrip_core::{ResultSet, export};
use rgrip_core::geo::{self, Coord, Geometry};
use std::collections::{BTreeSet, HashMap};
use walkers::sources::{Attribution, OpenStreetMap, TileSource};
use walkers::{HttpOptions, HttpTiles, MapMemory, Plugin, Projector, TileId, lon_lat};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Basemap {
    Dark,
    /// Minimal white/grey canvas.
    Light,
    Streets,
    Satellite,
}

impl Basemap {
    const ALL: [Basemap; 4] = [Basemap::Dark, Basemap::Light, Basemap::Streets, Basemap::Satellite];

    fn label(self) -> &'static str {
        match self {
            Basemap::Dark => "Dark",
            Basemap::Light => "Light",
            Basemap::Streets => "Streets",
            Basemap::Satellite => "Satellite",
        }
    }
}

/// Esri Dark Gray Canvas: a quiet basemap that suits the dark UI (no API key).
struct EsriDarkGray;

impl TileSource for EsriDarkGray {
    fn tile_url(&self, t: TileId) -> String {
        format!(
            "https://server.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Dark_Gray_Base/MapServer/tile/{}/{}/{}",
            t.zoom, t.y, t.x
        )
    }
    fn attribution(&self) -> Attribution {
        Attribution { text: "Esri, HERE, Garmin, © OpenStreetMap contributors", url: "https://www.esri.com", logo_light: None, logo_dark: None }
    }
    fn max_zoom(&self) -> u8 {
        16
    }
}

/// Esri Light Gray Canvas: clean white basemap (CARTO Positron now needs an API key).
struct EsriLightGray;

impl TileSource for EsriLightGray {
    fn tile_url(&self, t: TileId) -> String {
        format!(
            "https://server.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Light_Gray_Base/MapServer/tile/{}/{}/{}",
            t.zoom, t.y, t.x
        )
    }
    fn attribution(&self) -> Attribution {
        Attribution { text: "Esri, HERE, Garmin, © OpenStreetMap contributors", url: "https://www.esri.com", logo_light: None, logo_dark: None }
    }
    fn max_zoom(&self) -> u8 {
        16
    }
}

/// Google satellite. `scale=2` returns 512 px images for the same 256 px
/// tile grid, so tiles render at double density (sharp on Retina).
struct GoogleSatellite;

impl TileSource for GoogleSatellite {
    fn tile_url(&self, t: TileId) -> String {
        format!("https://mt1.google.com/vt/lyrs=s&x={}&y={}&z={}&scale=2", t.x, t.y, t.zoom)
    }
    fn attribution(&self) -> Attribution {
        Attribution { text: "Imagery © Google", url: "https://www.google.com/maps", logo_light: None, logo_dark: None }
    }
    fn max_zoom(&self) -> u8 {
        20
    }
}

fn attribution(b: Basemap) -> &'static str {
    match b {
        Basemap::Dark => EsriDarkGray.attribution().text,
        Basemap::Light => EsriLightGray.attribution().text,
        Basemap::Streets => "© OpenStreetMap contributors",
        Basemap::Satellite => GoogleSatellite.attribution().text,
    }
}

/// One drawable feature (a result row), in WGS84.
struct Feature {
    row: usize,
    points: Vec<Coord>,
    lines: Vec<Vec<Coord>>,
    /// Triangulated polygons: vertices and triangle indices, plus rings for outlines.
    fills: Vec<(Vec<Coord>, Vec<usize>)>,
    rings: Vec<Vec<Coord>>,
}

pub struct MapData {
    features: Vec<Feature>,
    /// Extent of the non-outlier features (what "Fit" zooms to).
    bounds: Option<(Coord, Coord)>,
    /// Features with a vertex far from the rest of the data (often swapped
    /// lat/lon or a stray coordinate), drawn as long spikes.
    outlier: Vec<bool>,
    /// Rows whose SRID we can't convert, with the SRIDs seen.
    skipped: usize,
    srids: Vec<i32>,
    /// SRID of the column as stored (first geometry that has one), for the
    /// area filter's `ST_Transform`.
    srid: Option<i32>,
}

impl MapData {
    pub fn build(rs: &ResultSet, col: usize) -> Self {
        let mut data = MapData { features: Vec::new(), bounds: None, outlier: Vec::new(), skipped: 0, srids: Vec::new(), srid: None };
        for (row, values) in rs.rows.iter().enumerate() {
            let Some(g) = values[col].as_deref().and_then(geo::parse_hex) else { continue };
            if data.srid.is_none() {
                data.srid = g.srid.filter(|s| *s > 0);
            }
            let Some(wgs) = g.to_wgs84() else {
                data.skipped += 1;
                if let Some(s) = g.srid.filter(|s| !data.srids.contains(s)) {
                    data.srids.push(s);
                }
                continue;
            };
            let mut f = Feature { row, points: Vec::new(), lines: Vec::new(), fills: Vec::new(), rings: Vec::new() };
            add_geometry(&mut f, &wgs);
            wgs.for_each_coord(&mut |c| {
                if c[0].is_finite() && c[1].is_finite() {
                    data.bounds = Some(match data.bounds {
                        None => (c, c),
                        Some((lo, hi)) => ([lo[0].min(c[0]), lo[1].min(c[1])], [hi[0].max(c[0]), hi[1].max(c[1])]),
                    });
                }
            });
            data.features.push(f);
        }
        data.outlier = find_outliers(&data.features);
        data.bounds = None;
        for (f, out) in data.features.iter().zip(&data.outlier) {
            if *out {
                continue;
            }
            for c in vertices(f) {
                data.bounds = Some(match data.bounds {
                    None => (c, c),
                    Some((lo, hi)) => ([lo[0].min(c[0]), lo[1].min(c[1])], [hi[0].max(c[0]), hi[1].max(c[1])]),
                });
            }
        }
        data
    }

    fn outlier_count(&self) -> usize {
        self.outlier.iter().filter(|o| **o).count()
    }

    /// Extent of the features whose result row is in `rows`.
    fn selection_bounds(&self, rows: &BTreeSet<usize>) -> Option<(Coord, Coord)> {
        extent(self.features.iter().filter(|f| rows.contains(&f.row)).flat_map(vertices))
    }

    fn feature_of_row(&self, row: usize) -> Option<usize> {
        self.features.iter().position(|f| f.row == row)
    }
}

fn extent(coords: impl Iterator<Item = Coord>) -> Option<(Coord, Coord)> {
    coords.fold(None, |b, c| {
        Some(match b {
            None => (c, c),
            Some((lo, hi)) => ([lo[0].min(c[0]), lo[1].min(c[1])], [hi[0].max(c[0]), hi[1].max(c[1])]),
        })
    })
}

/// A box drawn on the map: `[min, max]` as (lon, lat).
pub type BBox = [Coord; 2];

/// What the map asks the table to do about its area filter.
pub enum AreaEvent {
    /// Filter to this WHERE text (the box was just drawn).
    Set(String),
    /// The user cleared the area.
    Clear,
}

#[derive(Default)]
pub struct MapOutput {
    /// Geometry column to reproject to 4326.
    pub reproject: Option<String>,
    pub area: Option<AreaEvent>,
}

/// The map's two-way link to the grid it shares a result with.
pub struct Link<'a> {
    /// Selected result rows, shared with the grid.
    pub selection: &'a mut BTreeSet<usize>,
    /// Asks the grid to scroll a row into view.
    pub reveal: &'a mut Option<usize>,
    /// Name for the legend of an exported map (the table); else the column.
    pub layer: Option<&'a str>,
    /// Table tabs: the applied WHERE text, which enables "Filter by area".
    pub area_filter: Option<&'a str>,
}

/// Apply a map click to the shared selection. `row` is the clicked feature's
/// row (`None`: empty map); `additive` toggles instead of replacing.
/// Returns whether the row ended up selected.
fn apply_click(selection: &mut BTreeSet<usize>, row: Option<usize>, additive: bool) -> bool {
    match (row, additive) {
        (Some(r), true) => {
            if !selection.remove(&r) {
                selection.insert(r);
                return true;
            }
            false
        }
        (Some(r), false) => {
            *selection = [r].into();
            true
        }
        (None, false) => {
            selection.clear();
            false
        }
        (None, true) => false,
    }
}

/// Order a dragged corner pair into `[min, max]`, kept inside the world.
fn normalize_bbox(a: Coord, b: Coord) -> BBox {
    let lon = |v: f64| v.clamp(-180.0, 180.0);
    let lat = |v: f64| v.clamp(-90.0, 90.0);
    [[lon(a[0].min(b[0])), lat(a[1].min(b[1]))], [lon(a[0].max(b[0])), lat(a[1].max(b[1]))]]
}

/// WHERE text keeping rows of `column` that touch the box. The box is built
/// in WGS84 and moved into the column's SRID (no transform for 4326, or when
/// the SRID isn't known from the data).
pub fn area_filter_sql(column: &str, srid: Option<i32>, bbox: BBox) -> String {
    // 6 decimals is about 0.1 m, and keeps the text short.
    let n = |v: f64| {
        let t = format!("{v:.6}");
        t.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let env = format!("ST_MakeEnvelope({}, {}, {}, {}, 4326)", n(bbox[0][0]), n(bbox[0][1]), n(bbox[1][0]), n(bbox[1][1]));
    let env = match srid {
        Some(s) if s != 4326 && s > 0 => format!("ST_Transform({env}, {s})"),
        _ => env,
    };
    format!("ST_Intersects({}, {env})", rgrip_core::sql::quote_ident(column))
}

/// Centre and zoom that fit `[lo, hi]` in a viewport of `size` with margin.
fn view_for(lo: Coord, hi: Coord, size: Vec2) -> (Coord, f64) {
    let center = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0];
    // World width at zoom z is 256·2^z px; fit the larger span with margin.
    let span_x = (hi[0] - lo[0]).max(1e-6) / 360.0;
    let merc = |lat: f64| (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln();
    let span_y = ((merc(hi[1]) - merc(lo[1])).abs() / (2.0 * std::f64::consts::PI)).max(1e-6);
    let zx = (size.x as f64 * 0.8 / 256.0 / span_x).log2();
    let zy = (size.y as f64 * 0.8 / 256.0 / span_y).log2();
    (center, zx.min(zy).clamp(1.0, 17.0))
}

fn vertices(f: &Feature) -> impl Iterator<Item = Coord> + '_ {
    f.points
        .iter()
        .chain(f.lines.iter().flatten())
        .chain(f.rings.iter().flatten())
        .copied()
        .filter(|c| c[0].is_finite() && c[1].is_finite())
}

/// Flag features with a vertex much farther from the data's centre than
/// most features are (10× the 90th-percentile distance, at least ~1 km).
fn find_outliers(features: &[Feature]) -> Vec<bool> {
    let centroids: Vec<Coord> = features
        .iter()
        .map(|f| {
            let (n, sum) = vertices(f).fold((0.0, [0.0, 0.0]), |(n, s), c| (n + 1.0, [s[0] + c[0], s[1] + c[1]]));
            if n > 0.0 { [sum[0] / n, sum[1] / n] } else { [f64::NAN, f64::NAN] }
        })
        .collect();
    let mut lons: Vec<f64> = centroids.iter().map(|c| c[0]).filter(|v| v.is_finite()).collect();
    let mut lats: Vec<f64> = centroids.iter().map(|c| c[1]).filter(|v| v.is_finite()).collect();
    if lons.len() < 5 {
        return vec![false; features.len()];
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let center = [median(&mut lons), median(&mut lats)];
    let k = center[1].to_radians().cos().max(0.1);
    let dist = |c: Coord| ((c[0] - center[0]) * k).hypot(c[1] - center[1]);
    let mut d: Vec<f64> = centroids.iter().copied().map(dist).filter(|v| v.is_finite()).collect();
    d.sort_by(f64::total_cmp);
    let p90 = d[(d.len() * 9 / 10).min(d.len() - 1)];
    let limit = (p90 * 10.0).max(0.01);
    features.iter().map(|f| vertices(f).any(|c| dist(c) > limit)).collect()
}

fn add_geometry(f: &mut Feature, g: &Geometry) {
    match g {
        Geometry::Point(c) if c[0].is_finite() => f.points.push(*c),
        Geometry::Point(_) => {}
        Geometry::MultiPoint(ps) => f.points.extend(ps),
        Geometry::LineString(l) => f.lines.push(l.clone()),
        Geometry::MultiLineString(ls) => f.lines.extend(ls.iter().cloned()),
        Geometry::Polygon(p) => add_polygon(f, p),
        Geometry::MultiPolygon(mp) => mp.iter().for_each(|p| add_polygon(f, p)),
        Geometry::Collection(gs) => gs.iter().for_each(|g| add_geometry(f, g)),
    }
}

fn add_polygon(f: &mut Feature, rings: &[Vec<Coord>]) {
    let mut flat = Vec::new();
    let mut holes = Vec::new();
    let mut verts = Vec::new();
    for (i, ring) in rings.iter().enumerate() {
        if i > 0 {
            holes.push(verts.len());
        }
        for c in ring {
            flat.extend([c[0], c[1]]);
            verts.push(*c);
        }
    }
    if let Ok(tris) = earcutr::earcut(&flat, &holes, 2) {
        f.fills.push((verts, tris));
    }
    f.rings.extend(rings.iter().cloned());
}

pub struct MapView {
    tiles: HashMap<Basemap, HttpTiles>,
    memory: MapMemory,
    basemap: Basemap,
    /// Identity of the data the view was last fitted to.
    fitted: Option<usize>,
    pub selected: Option<usize>,
    column: usize,
    /// "Filter by area" tool is armed (also active while ⇧ is held).
    area_tool: bool,
    /// Box being dragged, as (start, current) in lon/lat.
    drawing: Option<(Coord, Coord)>,
    /// The box the table is filtered to, and the WHERE text it produced
    /// (the area is dropped once the table's filter no longer matches).
    area: Option<(BBox, String)>,
    hide_outliers: bool,
    /// Column to show when it appears (e.g. after reprojecting).
    pub prefer: Option<String>,
    cache: Option<(usize, MapData)>,
    /// Export map: open while setting it up, and while capturing.
    export: Option<crate::map_export::Export>,
    /// Meters per screen point at the map's center (for the scale bar).
    meters_per_px: Option<f64>,
    /// A short message on the map ("Saved map.png").
    notice: Option<(String, std::time::Instant)>,
}

impl Default for MapView {
    fn default() -> Self {
        Self {
            tiles: HashMap::new(),
            memory: MapMemory::default(),
            // Debug builds: RGRIP_DEMO_BASEMAP=satellite|streets for screenshots.
            basemap: match std::env::var("RGRIP_DEMO_BASEMAP").as_deref() {
                Ok("satellite") if cfg!(debug_assertions) => Basemap::Satellite,
                Ok("streets") if cfg!(debug_assertions) => Basemap::Streets,
                Ok("light") if cfg!(debug_assertions) => Basemap::Light,
                // Match the app: dark canvas in dark themes, light canvas in light ones.
                _ if theme::is_dark() => Basemap::Dark,
                _ => Basemap::Light,
            },
            fitted: None,
            selected: None,
            column: 0,
            area_tool: false,
            drawing: None,
            area: None,
            hide_outliers: false,
            prefer: None,
            cache: None,
            export: None,
            meters_per_px: None,
            notice: None,
        }
    }
}

fn http_options() -> HttpOptions {
    let cache = std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join("Library/Caches/rgrip/tiles"));
    HttpOptions {
        cache,
        user_agent: Some(walkers::HeaderValue::from_static("rgrip/0.1 (desktop PostgreSQL client)")),
        ..Default::default()
    }
}

impl MapView {
    fn tiles(&mut self, ctx: &egui::Context) -> &mut HttpTiles {
        let basemap = self.basemap;
        self.tiles.entry(basemap).or_insert_with(|| match basemap {
            Basemap::Dark => HttpTiles::with_options(EsriDarkGray, http_options(), ctx.clone()),
            Basemap::Light => HttpTiles::with_options(EsriLightGray, http_options(), ctx.clone()),
            Basemap::Streets => HttpTiles::with_options(OpenStreetMap, http_options(), ctx.clone()),
            Basemap::Satellite => HttpTiles::with_options(GoogleSatellite, http_options(), ctx.clone()),
        })
    }

    fn fit(&mut self, data: &MapData, size: Vec2) {
        if let Some(b) = data.bounds {
            self.fit_bounds(b, size);
        }
    }

    fn fit_bounds(&mut self, (lo, hi): (Coord, Coord), size: Vec2) {
        let (center, zoom) = view_for(lo, hi, size);
        self.memory.center_at(lon_lat(center[0], center[1]));
        let _ = self.memory.set_zoom(zoom);
    }

    /// The map is being shown again: fit to the grid's selection if it has
    /// one (otherwise keep the view as the user left it).
    pub fn opened(&mut self, has_selection: bool) {
        if has_selection {
            self.fitted = None;
        }
    }

    /// Show the map for `rs`; `geom_cols` are its geometry columns.
    /// With `can_reproject`, unsupported SRIDs offer a "Reproject" button;
    /// returns the geometry column the user asked to reproject.
    pub fn show(&mut self, ui: &mut Ui, rs: &ResultSet, geom_cols: &[usize], can_reproject: bool, link: Link<'_>) -> MapOutput {
        if let Some(i) = self.prefer.as_ref().and_then(|p| rs.columns.iter().position(|c| c == p)).filter(|i| geom_cols.contains(i)) {
            self.column = i;
            self.prefer = None;
        }
        if !geom_cols.contains(&self.column) {
            self.column = geom_cols[0];
        }
        // Re-decode only when the result or column changes. A reloaded page
        // can reuse the address and row count: the end rows tell them apart.
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (rs as *const ResultSet as usize, rs.rows.len(), self.column).hash(&mut h);
            for row in [rs.rows.first(), rs.rows.last()].into_iter().flatten() {
                row.get(self.column).hash(&mut h);
            }
            h.finish() as usize
        };
        if self.cache.as_ref().is_none_or(|(k, _)| *k != key) {
            // New data: prefer a reprojected `*_4326` column (from /con_4326 etc.).
            if let Some(&i) = geom_cols.iter().find(|&&i| rs.columns[i].ends_with("_4326")) {
                if i != self.column {
                    self.column = i;
                    return self.show(ui, rs, geom_cols, can_reproject, link);
                }
            }
            self.cache = Some((key, MapData::build(rs, self.column)));
            self.selected = None;
        }
        let (_, data) = self.cache.take().unwrap();

        let rect = ui.available_rect_before_wrap();
        if self.fitted != Some(key) {
            match data.selection_bounds(link.selection) {
                Some(b) => self.fit_bounds(b, rect.size()),
                None => self.fit(&data, rect.size()),
            }
            self.fitted = Some(key);
        }
        // The area belongs to the table's filter: gone once that changes.
        match link.area_filter {
            Some(f) => {
                if self.area.as_ref().is_some_and(|(_, sql)| sql != f) {
                    self.area = None;
                }
            }
            None => {
                self.area = None;
                self.area_tool = false;
            }
        }
        if self.area_tool && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.area_tool = false;
        }
        let draw_mode = link.area_filter.is_some() && (self.area_tool || self.drawing.is_some() || ui.input(|i| i.modifiers.shift));

        let mut hovered = None;
        let mut clicked = None;
        let ctx = ui.ctx().clone();
        let selected = self.selected;
        let hide_outliers = self.hide_outliers;
        let mut finished = None;
        // Moved out: `self.tiles()` below borrows all of self.
        let mut drawing = self.drawing.take();
        let area = self.area.as_ref().map(|(b, _)| *b);
        let mut memory = std::mem::take(&mut self.memory);
        let mut scale = self.meters_per_px;
        {
            let tiles = self.tiles(&ctx);
            let map = walkers::Map::new(Some(tiles), &mut memory, lon_lat(0.0, 0.0))
                .double_click_to_zoom(true)
                // Wheel/trackpad scroll zooms (drag pans) instead of scrolling the map.
                .zoom_with_ctrl(false)
                // Drawing a box: the drag must not pan the map.
                .drag_pan_buttons(if draw_mode { DragPanButtons::empty() } else { DragPanButtons::PRIMARY })
                .with_plugin(Features {
                    data: &data,
                    selected,
                    rows: &*link.selection,
                    hide_outliers,
                    hovered: &mut hovered,
                    clicked: &mut clicked,
                    draw_mode,
                    drawing: &mut drawing,
                    finished: &mut finished,
                    area,
                    scale: &mut scale,
                });
            ui.put(rect, map);
        }
        self.memory = memory;
        self.meters_per_px = scale;
        self.drawing = drawing;
        let mut out = MapOutput::default();
        if let Some((hit, additive)) = clicked {
            let row = hit.map(|i| data.features[i].row);
            let on = apply_click(link.selection, row, additive);
            // The popup follows a plain click (or the feature just added).
            self.selected = hit.filter(|_| on);
            if on {
                *link.reveal = row;
            }
        }
        if let Some((a, b)) = finished {
            let bbox = normalize_bbox(a, b);
            let sql = area_filter_sql(&rs.columns[self.column], data.srid, bbox);
            self.area = Some((bbox, sql.clone()));
            self.area_tool = false;
            out.area = Some(AreaEvent::Set(sql));
        }

        let overlay = self.overlay(ui, rect, &data, rs, geom_cols, can_reproject, &link);
        out.reproject = overlay.0;
        if overlay.1 {
            self.area = None;
            out.area = Some(AreaEvent::Clear);
        }
        let exporting = self.export.is_some();
        if let Some(i) = hovered.filter(|_| !exporting && (self.selected.is_none() || hovered != self.selected)) {
            let f = &data.features[i];
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::new("map-hover"), egui::PopupAnchor::Pointer)
                .show(|ui| feature_title(ui, rs, f.row, self.column));
        }
        if let Some(i) = self.selected.filter(|i| *i < data.features.len() && !exporting) {
            self.details(ui, rect, rs, data.features[i].row);
        }
        self.export_step(ui, rect, &data, rs, link.selection, link.layer);
        self.cache = Some((key, data));
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn overlay(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        data: &MapData,
        rs: &ResultSet,
        geom_cols: &[usize],
        can_reproject: bool,
        link: &Link<'_>,
    ) -> (Option<String>, bool) {
        let mut reproject = None;
        let mut clear_area = false;
        let sel_bounds = data.selection_bounds(link.selection);
        let panel = |ui: &mut Ui, pos: Pos2, align: Align2, id: &str, add: &mut dyn FnMut(&mut Ui)| {
            egui::Area::new(egui::Id::new(id))
                .order(egui::Order::Foreground)
                .pivot(align)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    Frame::new()
                        .fill(theme::POPUP().gamma_multiply(0.94))
                        .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                        .corner_radius(8)
                        .inner_margin(Margin::symmetric(8, 6))
                        .show(ui, |ui| ui.horizontal(|ui| add(ui)));
                });
        };

        let mut fit = false;
        let mut zoom_sel = false;
        let mut export = false;
        let capturing = self.export.as_ref().is_some_and(|e| e.capturing());
        if !capturing { panel(ui, rect.left_top() + vec2(12.0, 12.0), Align2::LEFT_TOP, "map-controls", &mut |ui| {
            for b in Basemap::ALL {
                if ui.selectable_label(self.basemap == b, b.label()).clicked() {
                    self.basemap = b;
                }
            }
            ui.separator();
            if geom_cols.len() > 1 {
                egui::ComboBox::from_id_salt("map-col")
                    .selected_text(&rs.columns[self.column])
                    .show_ui(ui, |ui| {
                        for &c in geom_cols {
                            ui.selectable_value(&mut self.column, c, &rs.columns[c]);
                        }
                    });
            }
            if ui.button("Fit").on_hover_text("Zoom to all features").clicked() {
                fit = true;
            }
            if sel_bounds.is_some() && ui.button("Zoom to selected").on_hover_text("Zoom to the rows selected in the grid or on the map").clicked() {
                zoom_sel = true;
            }
            if ui.button("Export map").on_hover_text("Save the map as a picture, with a title, legend, scale bar and north arrow").clicked() {
                export = true;
            }
            if link.area_filter.is_some() {
                if ui
                    .selectable_label(self.area_tool, "Filter by area")
                    .on_hover_text("Drag a box on the map to keep only the rows inside it (or hold ⇧ and drag)")
                    .clicked()
                {
                    self.area_tool = !self.area_tool;
                }
                if self.area.is_some() {
                    ui.label(RichText::new("area filter").size(11.5).color(theme::ACCENT()));
                    if ui.small_button("Clear").on_hover_text("Remove the area filter").clicked() {
                        clear_area = true;
                    }
                }
            }
            let shown = link.selection.iter().filter(|r| data.feature_of_row(**r).is_some()).count();
            let count = crate::app::plural(data.features.len(), "feature");
            let text = if shown > 0 { format!("{count}, {shown} selected") } else { count };
            ui.label(RichText::new(text).size(11.5).color(theme::MUTED()));
            let outliers = data.outlier_count();
            if outliers > 0 {
                let label = if self.hide_outliers { "Show" } else { "Hide" };
                ui.label(RichText::new(format!("⚠ {outliers} far off")).size(11.5).color(theme::WARN())).on_hover_text(
                    "Features with a vertex far from the rest of the data, drawn as long spikes.\n\
                     Often swapped lat/lon or a stray coordinate. Click a spike to see its row.",
                );
                if ui.small_button(label).clicked() {
                    self.hide_outliers = !self.hide_outliers;
                }
            }
            if data.skipped > 0 {
                let srids = data.srids.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ");
                ui.label(RichText::new(format!("{} not shown (SRID {srids})", data.skipped)).size(11.5).color(theme::WARN()))
                    .on_hover_text("Only WGS84, Web Mercator and UTM are drawn directly.");
                if can_reproject
                    && ui
                        .button("Reproject to 4326")
                        .on_hover_text("Re-run the query with ST_Transform(…, 4326) in PostGIS")
                        .clicked()
                {
                    reproject = Some(rs.columns[self.column].clone());
                }
            }
        }); }
        if export {
            self.export = Some(Default::default());
            self.selected = None;
        }
        if fit {
            self.fit(data, rect.size());
        }
        if let (true, Some(b)) = (zoom_sel, sel_bounds) {
            self.fit_bounds(b, rect.size());
        }

        // Attribution on a small backing so it reads on any basemap.
        let galley = ui.painter().layout_no_wrap(attribution(self.basemap).into(), FontId::proportional(10.0), theme::MUTED());
        let pos = rect.right_bottom() - vec2(8.0, 6.0) - galley.size();
        let back = Rect::from_min_size(pos, galley.size()).expand2(vec2(5.0, 2.0));
        ui.painter().rect_filled(back, 4.0, theme::POPUP().gamma_multiply(0.85));
        ui.painter().galley(pos, galley, theme::MUTED());
        (reproject, clear_area)
    }

    /// Export map: the layout over the map, the options panel, and the
    /// capture (controls hidden for one frame, then a screenshot).
    fn export_step(&mut self, ui: &mut Ui, rect: Rect, data: &MapData, rs: &ResultSet, selection: &BTreeSet<usize>, layer: Option<&str>) {
        use crate::map_export::{self as mx, PanelAction, Stage};
        let layer = layer.map(str::to_string).or_else(|| rs.columns.get(self.column).cloned()).unwrap_or_default();
        if let Some(e) = &mut self.export {
            let mut legend = vec![mx::LegendItem { color: theme::GEO(), label: format!("{layer} ({})", data.features.len()) }];
            let shown = selection.iter().filter(|r| data.feature_of_row(**r).is_some()).count();
            if shown > 0 {
                legend.push(mx::LegendItem { color: theme::ACCENT(), label: format!("Selected ({shown})") });
            }
            mx::paint(&ui.painter().with_clip_rect(rect), rect, e, self.meters_per_px, &legend);
            match e.stage {
                Stage::Edit => match mx::panel(ui, rect, e) {
                    PanelAction::Save => e.stage = Stage::Hide { copy: false },
                    PanelAction::Copy => e.stage = Stage::Hide { copy: true },
                    PanelAction::Close => self.export = None,
                    PanelAction::None => {}
                },
                // This frame is drawn without the controls: capture it.
                Stage::Hide { copy } => {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                    e.stage = Stage::Wait { copy, since: std::time::Instant::now() };
                    ui.ctx().request_repaint();
                }
                Stage::Wait { copy, since } => {
                    let shot = ui.input(|i| {
                        i.events.iter().find_map(|ev| match ev {
                            egui::Event::Screenshot { image, .. } => Some(image.clone()),
                            _ => None,
                        })
                    });
                    if let Some(screen) = shot {
                        let image = mx::crop(&screen, rect, ui.ctx().pixels_per_point());
                        let name = mx::file_name(&e.title, &layer);
                        let message = if copy {
                            ui.ctx().copy_image(image);
                            Some("Copied the map".to_string())
                        } else {
                            match rfd::FileDialog::new().set_file_name(&name).add_filter("PNG image", &["png"]).save_file() {
                                Some(path) => match mx::png(&image).and_then(|b| std::fs::write(&path, b).map_err(|e| e.to_string())) {
                                    Ok(()) => Some(format!("Saved {}", path.file_name().map_or(name.clone(), |n| n.to_string_lossy().into_owned()))),
                                    Err(err) => Some(format!("Couldn't save the map: {err}")),
                                },
                                None => None,
                            }
                        };
                        self.export = None;
                        self.notice = message.map(|m| (m, std::time::Instant::now()));
                    } else if since.elapsed() > std::time::Duration::from_secs(3) {
                        self.export = None;
                        self.notice = Some(("Couldn't capture the map".into(), std::time::Instant::now()));
                    } else {
                        ui.ctx().request_repaint();
                    }
                }
            }
        }
        if let Some((text, at)) = &self.notice {
            if at.elapsed() > std::time::Duration::from_millis(2500) {
                self.notice = None;
            } else {
                let galley = ui.painter().layout_no_wrap(text.clone(), FontId::proportional(12.5), theme::TEXT());
                let back = Rect::from_center_size(rect.center_bottom() - vec2(0.0, 40.0), galley.size() + vec2(24.0, 12.0));
                ui.painter().rect(back, 8.0, theme::POPUP(), Stroke::new(1.0_f32, theme::BORDER()), egui::StrokeKind::Inside);
                ui.painter().galley(back.center() - galley.size() / 2.0, galley, theme::TEXT());
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
    }

    fn details(&mut self, ui: &mut Ui, rect: Rect, rs: &ResultSet, row: usize) {
        let mut close = false;
        egui::Area::new(egui::Id::new("map-details"))
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_TOP)
            .fixed_pos(rect.right_top() + vec2(-12.0, 12.0))
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(theme::POPUP())
                    .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                    .corner_radius(10)
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(320.0);
                        ui.horizontal(|ui| {
                            feature_title(ui, rs, row, self.column);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if crate::widgets::icon_button(ui, crate::widgets::Icon::Close, "Close").clicked() {
                                    close = true;
                                }
                                ui.menu_button(RichText::new("Copy").size(12.0), |ui| {
                                    let one = ResultSet {
                                        columns: rs.columns.clone(),
                                        rows: vec![rs.rows[row].clone()],
                                        ..Default::default()
                                    };
                                    for f in [export::Format::Json, export::Format::GeoJson, export::Format::Tsv, export::Format::SqlInsert] {
                                        if ui.button(f.label()).clicked() {
                                            ui.ctx().copy_text(export::export(&one, f, "feature").trim_start_matches('\u{FEFF}').to_string());
                                            ui.close();
                                        }
                                    }
                                });
                            });
                        });
                        ui.add_space(6.0);
                        // Size the value column so the copy icons sit at the
                        // right edge, under the close button.
                        let (gap, icon_w) = (10.0, 18.0);
                        let label_w = rs
                            .columns
                            .iter()
                            .map(|c| ui.fonts_mut(|f| f.layout_no_wrap(c.clone(), egui::FontId::proportional(12.0), theme::MUTED()).size().x))
                            .fold(0.0, f32::max);
                        let value_w = (ui.available_width() - label_w - icon_w - 2.0 * gap).max(80.0);
                        egui::ScrollArea::vertical().max_height(rect.height() - 120.0).show(ui, |ui| {
                            egui::Grid::new("map-attrs").num_columns(3).spacing([gap, 6.0]).show(ui, |ui| {
                                for (i, name) in rs.columns.iter().enumerate() {
                                    ui.label(RichText::new(name).size(12.0).color(theme::MUTED()));
                                    let value = rs.rows[row][i].as_deref();
                                    // What the copy button copies: geometries as WKT.
                                    let copy_text = match value {
                                        None => None,
                                        Some(v) => Some(geo::parse_hex(v).map_or_else(|| v.to_string(), |g| g.geometry.to_wkt())),
                                    };
                                    ui.scope(|ui| {
                                        ui.set_min_width(value_w);
                                        ui.set_max_width(value_w);
                                        match value {
                                            None => {
                                                ui.label(RichText::new("NULL").italics().color(theme::FAINT()));
                                            }
                                            Some(v) => match geo::parse_hex(v) {
                                                Some(g) => {
                                                    ui.add(egui::Label::new(RichText::new(g.summary()).size(12.0).color(theme::GEO())).wrap());
                                                }
                                                // Khmer is drawn shaped (not selectable); use the copy button.
                                                None if shaping::is_complex(v) => {
                                                    shaping::label(ui, v, 12.5, theme::TEXT());
                                                }
                                                None => {
                                                    let shown: String = v.chars().take(400).collect();
                                                    let more = if shown.len() < v.len() { "…" } else { "" };
                                                    ui.add(egui::Label::new(RichText::new(format!("{shown}{more}")).size(12.5)).wrap().selectable(true));
                                                }
                                            },
                                        }
                                    });
                                    match copy_text {
                                        Some(text) => {
                                            if crate::widgets::icon_button(ui, crate::widgets::Icon::Copy, "Copy value").clicked() {
                                                ui.ctx().copy_text(text);
                                            }
                                        }
                                        None => {
                                            ui.label("");
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                        });
                    });
            });
        if close {
            self.selected = None;
        }
    }
}

/// "Row 12" or the first text-like column's value.
fn feature_title(ui: &mut Ui, rs: &ResultSet, row: usize, geom_col: usize) {
    let name = ["name", "title", "label", "name_en", "name_km"]
        .iter()
        .find_map(|n| rs.columns.iter().position(|c| c.eq_ignore_ascii_case(n)))
        .filter(|&c| c != geom_col)
        .and_then(|c| rs.rows[row][c].clone());
    match name {
        Some(n) => {
            shaping::label(ui, &n, 13.5, theme::TEXT());
        }
        None => {
            ui.label(RichText::new(format!("Row {}", row + 1)).color(theme::TEXT()));
        }
    }
}

struct Features<'a> {
    data: &'a MapData,
    /// Feature whose popup is open.
    selected: Option<usize>,
    /// Result rows selected in the grid (or by earlier map clicks).
    rows: &'a BTreeSet<usize>,
    hide_outliers: bool,
    hovered: &'a mut Option<usize>,
    /// A click: the feature hit, and whether ⌘ was held (add/remove).
    clicked: &'a mut Option<(Option<usize>, bool)>,
    /// Drags draw a box instead of panning.
    draw_mode: bool,
    drawing: &'a mut Option<(Coord, Coord)>,
    finished: &'a mut Option<(Coord, Coord)>,
    /// The box the table is currently filtered to.
    area: Option<BBox>,
    /// Out: meters per screen point at the map's center.
    scale: &'a mut Option<f64>,
}

impl Plugin for Features<'_> {
    fn run(self: Box<Self>, ui: &mut Ui, response: &egui::Response, projector: &Projector, _: &MapMemory) {
        let painter = ui.painter_at(response.rect);
        let screen = |c: &Coord| -> Pos2 { projector.project(lon_lat(c[0], c[1])).to_pos2() };
        // Scale: the ground distance across 100 points at the center.
        let mid = response.rect.center();
        let (a, b) = (projector.unproject(mid.to_vec2()), projector.unproject((mid + vec2(100.0, 0.0)).to_vec2()));
        *self.scale = Some(crate::map_export::haversine([a.x(), a.y()], [b.x(), b.y()]) / 100.0);
        let pointer = response.hover_pos();
        let hide = self.hide_outliers;
        let data = self.data;
        let visible = move || data.features.iter().enumerate().filter(move |(i, _)| !(hide && data.outlier[*i]));

        // Hit test, topmost first: points, then lines, then polygons.
        let mut hit = None;
        if let Some(p) = pointer {
            for (i, f) in visible().rev() {
                if f.points.iter().any(|c| screen(c).distance(p) < 8.0) {
                    hit = Some(i);
                    break;
                }
            }
            if hit.is_none() {
                hit = visible().rev().find_map(|(i, f)| {
                    f.lines.iter().any(|l| near_line(&l.iter().map(screen).collect::<Vec<_>>(), p, 6.0)).then_some(i)
                });
            }
            if hit.is_none() {
                hit = visible().rev().find_map(|(i, f)| {
                    f.fills.iter().any(|(v, t)| {
                        let pts: Vec<Pos2> = v.iter().map(screen).collect();
                        t.chunks(3).any(|tri| in_triangle(p, pts[tri[0]], pts[tri[1]], pts[tri[2]]))
                    })
                    .then_some(i)
                });
            }
        }
        *self.hovered = hit;
        if response.clicked() {
            *self.clicked = Some((hit, ui.input(|i| i.modifiers.command)));
        }
        if self.draw_mode {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            let at = |p: Pos2| -> Coord {
                let c = projector.unproject(p.to_vec2());
                [c.x(), c.y()]
            };
            if response.drag_started_by(egui::PointerButton::Primary) && let Some(p) = response.interact_pointer_pos() {
                *self.drawing = Some((at(p), at(p)));
            }
            if response.dragged_by(egui::PointerButton::Primary) && let (Some((a, _)), Some(p)) = (*self.drawing, response.interact_pointer_pos()) {
                *self.drawing = Some((a, at(p)));
            }
            if response.drag_stopped() && let Some((a, b)) = self.drawing.take() {
                // A click-sized drag is a stray click, not a box.
                if screen(&a).distance(screen(&b)) > 6.0 {
                    *self.finished = Some((a, b));
                }
            }
        }
        if hit.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let rows = self.rows;
        let is_selected = |i: usize| rows.contains(&data.features[i].row);
        let style = |i: usize| -> Color32 {
            if Some(i) == self.selected {
                theme::WARN()
            } else if is_selected(i) {
                theme::ACCENT()
            } else if Some(i) == hit {
                Color32::WHITE
            } else {
                theme::GEO()
            }
        };
        let mut shapes = Vec::new();
        // Selected features last, so they sit on top of the rest.
        for (layer, pass) in (0..2).flat_map(|l| (0..3).map(move |p| (l, p))) {
            for (i, f) in visible().filter(|(i, _)| is_selected(*i) == (layer == 1)) {
                let color = style(i);
                let sel = layer == 1;
                // Selected: a dark halo under thicker strokes and bigger dots.
                let (ring_w, line_w, dot) = if sel { (2.5, 3.5, 6.0) } else { (1.5_f32, 2.5_f32, 4.5) };
                match pass {
                    0 => {
                        for (verts, tris) in &f.fills {
                            let mut mesh = Mesh::default();
                            for v in verts {
                                mesh.colored_vertex(screen(v), color.gamma_multiply(0.22));
                            }
                            mesh.indices = tris.iter().map(|&t| t as u32).collect();
                            shapes.push(Shape::mesh(mesh));
                        }
                        for ring in &f.rings {
                            if sel {
                                shapes.push(path_shape(ring.iter().map(screen), true, Stroke::new(ring_w + 2.5, Color32::from_black_alpha(150))));
                            }
                            shapes.push(path_shape(ring.iter().map(screen), true, Stroke::new(ring_w, color)));
                        }
                    }
                    1 => {
                        for l in &f.lines {
                            if sel {
                                shapes.push(path_shape(l.iter().map(screen), false, Stroke::new(line_w + 2.5, Color32::from_black_alpha(150))));
                            }
                            shapes.push(path_shape(l.iter().map(screen), false, Stroke::new(line_w, color)));
                        }
                    }
                    _ => {
                        for c in &f.points {
                            let p = screen(c);
                            shapes.push(Shape::circle_filled(p, dot + 1.5, if sel { Color32::WHITE } else { Color32::from_black_alpha(160) }));
                            shapes.push(Shape::circle_filled(p, dot, color));
                        }
                    }
                }
            }
        }
        painter.extend(shapes);

        // The area the table is filtered to, and the box being dragged.
        let boxes = self.area.map(|[a, b]| (a, b, false)).into_iter().chain(self.drawing.map(|(a, b)| (a, b, true)));
        for (a, b, live) in boxes {
            let r = Rect::from_two_pos(screen(&a), screen(&b));
            painter.rect_filled(r, 0.0, theme::ACCENT().gamma_multiply(if live { 0.18 } else { 0.10 }));
            painter.rect_stroke(r, 0.0, Stroke::new(if live { 1.5 } else { 2.0_f32 }, theme::ACCENT()), egui::StrokeKind::Middle);
        }
    }
}

/// A line or ring outline that egui can stroke safely. Repeated vertices
/// (and a ring's closing point, which egui adds itself) make zero-length
/// segments; two in a row give the stroke a NaN/infinite corner, drawn as a
/// spike across the map. Where the outline doubles back (coastlines, slivers)
/// egui's miter join reaches far past the corner, so the path is split
/// there. Sub-pixel steps are dropped first: they turn every which way.
/// Collapsed paths become a dot.
fn path_shape(points: impl Iterator<Item = Pos2>, closed: bool, stroke: Stroke) -> Shape {
    let mut pts: Vec<Pos2> = Vec::new();
    for p in points.filter(|p| p.x.is_finite() && p.y.is_finite()) {
        if pts.last().is_none_or(|q| q.distance_sq(p) > 0.25) {
            pts.push(p);
        }
    }
    if closed {
        while pts.len() > 1 && pts[0].distance_sq(*pts.last().unwrap()) <= 0.25 {
            pts.pop();
        }
    }
    let n = pts.len();
    // Turning by more than ~110 degrees.
    let sharp = |i: usize| {
        let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
        (b - a).normalized().dot((c - b).normalized()) < -0.35
    };
    match n {
        0 => Shape::Noop,
        1 => Shape::circle_filled(pts[0], stroke.width.max(1.5), stroke.color),
        2 => Shape::line_segment([pts[0], pts[1]], stroke),
        _ => {
            let corners: Vec<usize> = if closed { (0..n).filter(|&i| sharp(i)).collect() } else { (1..n - 1).filter(|&i| sharp(i)).collect() };
            if corners.is_empty() {
                return if closed { Shape::closed_line(pts, stroke) } else { Shape::line(pts, stroke) };
            }
            // Open pieces from corner to corner; a ring starts at its first
            // corner and comes back to it.
            let path: Vec<Pos2> = if closed { (0..=n).map(|i| pts[(corners[0] + i) % n]).collect() } else { pts.clone() };
            let cut: Vec<bool> = if closed {
                (0..=n).map(|i| corners.contains(&((corners[0] + i) % n))).collect()
            } else {
                (0..n).map(|i| corners.contains(&i)).collect()
            };
            let mut shapes = Vec::new();
            let mut start = 0;
            for i in 1..path.len() {
                if cut[i] || i == path.len() - 1 {
                    let piece = &path[start..=i];
                    shapes.push(if piece.len() == 2 { Shape::line_segment([piece[0], piece[1]], stroke) } else { Shape::line(piece.to_vec(), stroke) });
                    start = i;
                }
            }
            Shape::Vec(shapes)
        }
    }
}

fn near_line(points: &[Pos2], p: Pos2, tolerance: f32) -> bool {
    points.windows(2).any(|w| {
        let (a, b) = (w[0], w[1]);
        let ab = b - a;
        let t = ((p - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
        (a + ab * t).distance(p) < tolerance
    })
}

fn in_triangle(p: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    let sign = |p1: Pos2, p2: Pos2, p3: Pos2| (p1.x - p3.x) * (p2.y - p3.y) - (p2.x - p3.x) * (p1.y - p3.y);
    let (d1, d2, d3) = (sign(p, a, b), sign(p, b, c), sign(p, c, a));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{TessellationOptions, Tessellator};

    /// Real rings from placemarks (ids 4021, 4104, 4112): a repeated vertex
    /// next to the closing point used to tessellate to NaN/inf vertices.
    #[test]
    fn repeated_vertices_dont_spike() {
        let rings: [Vec<[f64; 2]>; 3] = [
            vec![[104.8881900106798,11.5757230297559],[104.8881900106798,11.5757230297559],[104.8881866622101,11.57576302279848],[104.8878748431896,11.57574350274659],[104.8878765571624,11.57570701957044],[104.8881900106798,11.5757230297559]],
            vec![[104.8881793744038,11.57586940547118],[104.8881690870962,11.5760477149109],[104.8878569281968,11.57603972829357],[104.8878679931232,11.57585340170502],[104.8878679931232,11.57585340170502],[104.8881793744038,11.57586940547118]],
            vec![[104.8878959432044,11.57609912055814],[104.8878210045372,11.57610007750799],[104.8878210045372,11.57610007750799],[104.887822875971,11.57604262372333],[104.8878228389796,11.57603731721991],[104.8878967401051,11.57604099787687],[104.8878959432044,11.57609912055814]],
        ];
        for zoom in [6.0f64, 10.0, 14.0, 18.0, 21.0] {
            let k = 2f64.powf(zoom) * 256.0 / 360.0;
            let screen = |c: &[f64; 2]| Pos2::new(((c[0] - 104.88) * k) as f32 + 900.0, ((11.57 - c[1]) * k) as f32 + 600.0);
            for ring in &rings {
                let origin = screen(&ring[0]);
                for (closed, width) in [(true, 1.5), (false, 2.5)] {
                    let shape = path_shape(ring.iter().map(screen), closed, Stroke::new(width, Color32::WHITE));
                    let mut mesh = Mesh::default();
                    Tessellator::new(2.0, TessellationOptions::default(), [1, 1], vec![]).tessellate_shape(shape, &mut mesh);
                    let size = ring.iter().map(|c| screen(c).distance(origin)).fold(0.0, f32::max);
                    for v in &mesh.vertices {
                        assert!(v.pos.x.is_finite() && v.pos.y.is_finite(), "zoom {zoom}: non-finite vertex");
                        // Nothing strays past the ring plus the stroke.
                        assert!(v.pos.distance(origin) <= size + 10.0, "zoom {zoom}: spike to {:?}", v.pos);
                    }
                }
            }
        }
    }

    /// A coastline that doubles back on itself: the miter join at the
    /// hairpin used to reach far out to sea.
    #[test]
    fn hairpins_dont_spike() {
        let ring = [Pos2::new(100.0, 100.0), Pos2::new(300.0, 101.0), Pos2::new(100.0, 102.5), Pos2::new(100.0, 200.0), Pos2::new(50.0, 150.0)];
        for closed in [true, false] {
            for width in [1.5, 2.5] {
                let shape = path_shape(ring.iter().copied(), closed, Stroke::new(width, Color32::WHITE));
                let mut mesh = Mesh::default();
                Tessellator::new(2.0, TessellationOptions::default(), [1, 1], vec![]).tessellate_shape(shape, &mut mesh);
                for v in &mesh.vertices {
                    assert!(v.pos.x <= 300.0 + 5.0, "spike to {:?}", v.pos);
                }
            }
        }
    }

    fn feature(row: usize, points: &[Coord]) -> Feature {
        Feature { row, points: points.to_vec(), lines: vec![], fills: vec![], rings: vec![] }
    }

    fn data(features: Vec<Feature>) -> MapData {
        let n = features.len();
        MapData { features, bounds: None, outlier: vec![false; n], skipped: 0, srids: vec![], srid: None }
    }

    #[test]
    fn click_updates_shared_selection() {
        let mut sel: BTreeSet<usize> = [2, 5].into();
        // Plain click replaces the selection.
        assert!(apply_click(&mut sel, Some(7), false));
        assert_eq!(sel, [7].into());
        // ⌘-click adds, then removes.
        assert!(apply_click(&mut sel, Some(3), true));
        assert_eq!(sel, [3, 7].into());
        assert!(!apply_click(&mut sel, Some(7), true));
        assert_eq!(sel, [3].into());
        // Empty map: ⌘ keeps the selection, a plain click clears it.
        assert!(!apply_click(&mut sel, None, true));
        assert_eq!(sel, [3].into());
        assert!(!apply_click(&mut sel, None, false));
        assert!(sel.is_empty());
    }

    #[test]
    fn rows_map_to_features() {
        // Rows without geometry are skipped, so feature index != row.
        let d = data(vec![feature(0, &[[1.0, 1.0]]), feature(3, &[[2.0, 5.0]]), feature(4, &[[9.0, -3.0]])]);
        assert_eq!(d.feature_of_row(3), Some(1));
        assert_eq!(d.feature_of_row(1), None);
        let sel: BTreeSet<usize> = [3, 4, 8].into();
        assert_eq!(d.selection_bounds(&sel), Some(([2.0, -3.0], [9.0, 5.0])));
        assert_eq!(d.selection_bounds(&[1].into()), None);
        assert_eq!(d.selection_bounds(&BTreeSet::new()), None);
    }

    #[test]
    fn fit_selection_zooms_in_further_than_everything() {
        let size = vec2(800.0, 600.0);
        let all = view_for([0.0, 0.0], [10.0, 10.0], size);
        let one = view_for([4.0, 4.0], [4.001, 4.001], size);
        assert!(one.1 > all.1);
        assert!((one.0[0] - 4.0005).abs() < 1e-9 && (one.0[1] - 4.0005).abs() < 1e-9);
        // A single point can't zoom past the cap.
        assert_eq!(view_for([4.0, 4.0], [4.0, 4.0], size).1, 17.0);
    }

    #[test]
    fn area_filter_text() {
        let b = normalize_bbox([104.95, 11.60], [104.90, 11.55]);
        assert_eq!(b, [[104.90, 11.55], [104.95, 11.60]]);
        assert_eq!(
            area_filter_sql("geom", Some(4326), b),
            r#"ST_Intersects("geom", ST_MakeEnvelope(104.9, 11.55, 104.95, 11.6, 4326))"#
        );
        assert_eq!(
            area_filter_sql("the geom", Some(32648), b),
            r#"ST_Intersects("the geom", ST_Transform(ST_MakeEnvelope(104.9, 11.55, 104.95, 11.6, 4326), 32648))"#
        );
        // SRID not known from the data: no transform.
        assert!(!area_filter_sql("geom", None, b).contains("ST_Transform"));
        // Dragged past the edge of the world.
        assert_eq!(normalize_bbox([-200.0, 95.0], [10.0, 5.0]), [[-180.0, 5.0], [10.0, 90.0]]);
    }

    #[test]
    fn collapsed_paths() {
        let p = Pos2::new(5.0, 5.0);
        let s = Stroke::new(1.5, Color32::WHITE);
        assert!(matches!(path_shape([p, p, p].into_iter(), true, s), Shape::Circle(_)));
        assert!(matches!(path_shape([p, Pos2::new(9.0, 5.0), p].into_iter(), true, s), Shape::LineSegment { .. }));
        assert!(matches!(path_shape(std::iter::empty(), false, s), Shape::Noop));
        let nan = Pos2::new(f32::NAN, 1.0);
        assert!(matches!(path_shape([nan, p].into_iter(), false, s), Shape::Circle(_)));
    }
}
