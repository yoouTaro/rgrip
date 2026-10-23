//! The "Import data" dialog: pick a file, check the preview and the column
//! names and types, then load it into a new or an existing table.
//! Reading and inferring live in `rgrip_core::import`; the loading itself
//! runs in the worker (`Cmd::Import`).

use crate::grid::{self, GridState};
use crate::theme;
use eframe::egui::{self, Align, Button, Frame, Id, Layout, Margin, RichText, Stroke, TextEdit, Ui};
use rgrip_core::import::{self, Format, ImportRequest, Opened, PlanColumn, Src};
use rgrip_core::{ColumnInfo, ResultSet};
use std::path::PathBuf;
use std::sync::mpsc;

const PREVIEW_ROWS: usize = 50;
const WIDTH: f32 = 880.0;
const DELIMITERS: [(u8, &str); 4] = [(b',', "comma"), (b';', "semicolon"), (b'\t', "tab"), (b'|', "pipe")];

pub enum Outcome {
    Keep,
    Close,
    Start(ImportRequest),
    /// The existing table picked as target: the app should load its columns.
    NeedColumns(String),
    OpenTable(String),
}

/// The primary key of a new table.
#[derive(Clone, PartialEq)]
enum Key {
    /// A new identity column `id` (or `gid`, … when taken).
    New,
    /// An imported column, by its place in `cols`: names change as they're
    /// edited, and two can be the same for a moment.
    Column(usize),
    None,
}

#[derive(Clone, PartialEq)]
enum Kind {
    Field(usize),
    /// A point built from the chosen longitude and latitude columns.
    Point,
}

#[derive(Clone)]
struct Col {
    kind: Kind,
    include: bool,
    source: String,
    name: String,
    ty: String,
}

pub struct ImportDialog {
    pub conn: String,
    pub schema: String,
    /// Tables of the schema, to catch name clashes and to append to.
    tables: Vec<String>,
    /// The name made from the file name, until the user types one: the
    /// table name is kept clear of existing tables as they load.
    auto_name: Option<String>,
    path: Option<PathBuf>,
    loading: Option<mpsc::Receiver<Result<Opened, String>>>,
    opened: Option<Opened>,
    error: Option<String>,
    create: bool,
    table: String,
    /// Columns as inferred from the file, and as edited / mapped to a target.
    inferred: Vec<Col>,
    cols: Vec<Col>,
    lon: usize,
    lat: usize,
    srid: String,
    spatial_index: bool,
    key: Key,
    /// Target table whose columns `cols` are currently mapped to.
    mapped_for: Option<String>,
    /// Target columns that fill themselves (a default, identity or the key).
    self_filled: Vec<String>,
    /// Columns of the target as (name, type), once mapped.
    target: Vec<(String, String)>,
    asked_columns: Option<String>,
    preview: ResultSet,
    grid: GridState,
    running: bool,
    progress: usize,
    sampled: usize,
    done: Option<Result<usize, String>>,
}

impl ImportDialog {
    /// `table`: append to this table (from its context menu).
    pub fn new(conn: String, schema: String, table: Option<String>) -> Self {
        Self {
            conn,
            schema,
            tables: Vec::new(),
            auto_name: None,
            path: None,
            loading: None,
            opened: None,
            error: None,
            create: table.is_none(),
            table: table.unwrap_or_default(),
            inferred: Vec::new(),
            cols: Vec::new(),
            lon: 0,
            lat: 1,
            srid: String::new(),
            spatial_index: true,
            key: Key::New,
            mapped_for: None,
            self_filled: Vec::new(),
            target: Vec::new(),
            asked_columns: None,
            preview: ResultSet::default(),
            grid: GridState::default(),
            running: false,
            progress: 0,
            sampled: 0,
            done: None,
        }
    }

    /// `name`, `name_2`, … whichever isn't taken yet.
    fn free_name(&mut self) {
        let Some(base) = &self.auto_name else { return };
        let mut name = base.clone();
        let mut n = 2;
        while self.tables.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        self.table = name;
    }

    pub fn table(&self) -> &str {
        self.table.trim()
    }

    pub fn progress(&mut self, rows: usize) {
        self.progress = rows;
    }

    pub fn finished(&mut self, result: Result<usize, String>) {
        self.running = false;
        self.done = Some(result);
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new().add_filter("Data files", &["csv", "tsv", "txt", "geojson", "json", "shp"]);
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(file) = dialog.pick_file() {
            self.open(ctx, file, None);
        }
    }

    /// Debug screenshot scenes: start with a file already picked.
    #[cfg(debug_assertions)]
    pub fn demo_open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.open(ctx, path, None);
    }

    /// Whether a file is shown and no longer being read.
    #[cfg(debug_assertions)]
    pub fn demo_ready(&self) -> bool {
        self.opened.is_some() && self.loading.is_none()
    }

    /// Read the file on a thread: big GeoJSON takes a moment to parse.
    fn open(&mut self, ctx: &egui::Context, path: PathBuf, delimiter: Option<u8>) {
        let (tx, rx) = mpsc::channel();
        let (p, c) = (path.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(import::open_with(&p, delimiter));
            c.request_repaint();
        });
        self.path = Some(path);
        self.loading = Some(rx);
        self.error = None;
        self.done = None;
    }

    fn loaded(&mut self, mut o: Opened) {
        let names = import::unique_names(&o.columns);
        let mut cols: Vec<Col> = (0..o.columns.len())
            .map(|i| {
                let ty = if o.geometry == Some(i) {
                    "geometry"
                } else {
                    import::infer_type(o.sample.iter().filter_map(|r| r[i].as_deref()))
                };
                Col { kind: Kind::Field(i), include: true, source: o.columns[i].clone(), name: names[i].clone(), ty: ty.into() }
            })
            .collect();
        // A point column from longitude / latitude, switched on when the
        // names say so and the file has no geometry of its own.
        let has_geometry = cols.iter().any(|c| c.ty == "geometry");
        let lonlat = import::detect_lonlat(&o.columns);
        if o.columns.len() >= 2 && !has_geometry {
            (self.lon, self.lat) = lonlat.unwrap_or((0, 1));
            let mut name = "geom".to_string();
            while cols.iter().any(|c| c.name == name) {
                name.push('_');
            }
            cols.push(Col { kind: Kind::Point, include: lonlat.is_some(), source: "point from lon / lat".into(), name, ty: "geometry".into() });
        }
        self.srid = match o.srid {
            Some(s) => s.to_string(),
            None if lonlat.is_some() && !has_geometry => "4326".into(),
            None => String::new(),
        };
        if self.table.is_empty() || self.create {
            let stem = self.path.as_ref().and_then(|p| p.file_stem()).and_then(|s| s.to_str()).unwrap_or("");
            if self.create {
                self.auto_name = Some(import::sanitize_name(stem, "imported"));
                self.free_name();
            }
        }
        self.preview = ResultSet {
            columns: o.columns.clone(),
            rows: o.sample.iter().take(PREVIEW_ROWS).cloned().collect(),
            ..Default::default()
        };
        self.grid = GridState::default();
        self.inferred = cols.clone();
        self.cols = cols;
        self.key = Key::New;
        self.mapped_for = None;
        self.sampled = o.sample.len();
        // The sample lives on in `preview`; keep the rest light to clone per frame.
        o.sample.clear();
        self.opened = Some(o);
    }

    /// Match the file's columns to the columns of the table being appended to.
    fn map_to_target(&mut self, columns: &[ColumnInfo]) {
        self.target = columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect();
        self.self_filled = columns.iter().filter(|c| c.default.is_some() || c.primary_key).map(|c| c.name.clone()).collect();
        self.cols = self.inferred.clone();
        for c in &mut self.cols {
            let hit = match c.kind {
                Kind::Field(_) => columns.iter().find(|t| t.name.eq_ignore_ascii_case(&c.name)),
                Kind::Point => columns.iter().find(|t| t.data_type.starts_with("geometry")),
            };
            c.include = c.include && hit.is_some();
            if let Some(t) = hit {
                c.name = t.name.clone();
                c.ty = ty_of(&t.data_type);
            }
        }
        // geometry(Point,4326): the table's SRID wins over a guess.
        if let Some(srid) = columns.iter().find_map(|t| t.data_type.strip_prefix("geometry(")?.trim_end_matches(')').rsplit(',').next()?.trim().parse::<i32>().ok()).filter(|s| *s > 0) {
            self.srid = srid.to_string();
        }
        self.mapped_for = Some(self.table.clone());
    }

    fn set_create(&mut self, create: bool) {
        if self.create != create {
            self.create = create;
            self.mapped_for = None;
            self.asked_columns = None;
            self.target.clear();
            self.cols = self.inferred.clone();
            self.key = Key::New;
            if create && let Some(p) = &self.path {
                self.auto_name = Some(import::sanitize_name(p.file_stem().and_then(|s| s.to_str()).unwrap_or(""), "imported"));
                self.free_name();
            } else if !create {
                self.table = self.tables.first().cloned().unwrap_or_default();
            }
        }
    }

    /// The key column's name, a new one clear of the imported columns.
    fn key_name(&self) -> Option<String> {
        let taken = |n: &str| self.cols.iter().any(|c| c.include && c.name.trim() == n);
        match &self.key {
            Key::New => Some(["id", "gid", "fid", "row_id"].into_iter().find(|n| !taken(n)).unwrap_or("rgrip_id").to_string()),
            Key::Column(i) => self.cols.get(*i).filter(|c| c.include).map(|c| c.name.trim().to_string()),
            Key::None => None,
        }
    }

    /// Unticking the key column takes the key back to a new one.
    fn settle_key(&mut self) {
        if let Key::Column(i) = self.key
            && !self.cols.get(i).is_some_and(|c| c.include && c.ty != "geometry")
        {
            self.key = Key::New;
        }
    }

    fn has_geometry(&self) -> bool {
        self.cols.iter().any(|c| c.include && c.ty == "geometry")
    }

    /// Why the import can't start yet.
    fn problem(&self) -> Option<String> {
        self.opened.as_ref()?;
        let table = self.table.trim();
        if table.is_empty() {
            return Some("Choose a table name".into());
        }
        if self.create && self.tables.iter().any(|t| t == table) {
            return Some(format!("{}.{table} already exists: pick another name or append to it", self.schema));
        }
        if !self.create && !self.tables.iter().any(|t| t == table) {
            return Some("Choose the table to append to".into());
        }
        let used: Vec<&Col> = self.cols.iter().filter(|c| c.include).collect();
        if used.is_empty() {
            return Some("No columns selected".into());
        }
        for (i, c) in used.iter().enumerate() {
            if c.name.trim().is_empty() {
                return Some("A column has no name".into());
            }
            if used[..i].iter().any(|o| o.name == c.name) {
                return Some(format!("Column name \"{}\" is used twice", c.name));
            }
            if !self.create && !self.target.is_empty() && !self.target.iter().any(|(n, _)| *n == c.name) {
                return Some(format!("The table has no column \"{}\"", c.name));
            }
        }
        if self.has_geometry() && !self.srid.trim().parse::<i32>().is_ok_and(|s| s > 0) {
            return Some("Enter the SRID of the geometry (4326 for WGS 84 longitude / latitude)".into());
        }
        None
    }

    fn request(&self) -> Option<ImportRequest> {
        let o = self.opened.as_ref()?;
        let columns = self
            .cols
            .iter()
            .filter(|c| c.include)
            .map(|c| PlanColumn {
                name: c.name.trim().to_string(),
                ty: c.ty.clone(),
                src: match c.kind {
                    Kind::Field(i) => Src::Field(i),
                    Kind::Point => Src::Point { lon: self.lon, lat: self.lat },
                },
            })
            .collect();
        Some(ImportRequest {
            path: self.path.clone()?,
            format: o.format,
            delimiter: o.delimiter,
            schema: self.schema.clone(),
            table: self.table.trim().to_string(),
            create: self.create,
            columns,
            srid: if self.has_geometry() { self.srid.trim().parse().unwrap_or(0) } else { 0 },
            spatial_index: self.spatial_index,
            key: if self.create { self.key_name() } else { None },
        })
    }

    /// Draw the dialog. `tables`: the tables of the schema (the explorer loads
    /// them). `target`: columns of the table being appended to,
    /// when the explorer has them.
    pub fn ui(&mut self, ctx: &egui::Context, tables: Vec<String>, target: Option<&[ColumnInfo]>) -> Outcome {
        if tables != self.tables {
            self.tables = tables;
            if self.create {
                self.free_name();
            }
        }
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(Ok(o)) => {
                    self.loading = None;
                    self.loaded(o);
                }
                Ok(Err(e)) => {
                    self.loading = None;
                    self.opened = None;
                    self.error = Some(e);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.loading = None,
            }
        }
        let mut out = Outcome::Keep;
        if self.opened.is_some() && !self.create && !self.table.is_empty() && self.mapped_for.as_deref() != Some(&self.table) {
            match target {
                Some(cols) => self.map_to_target(cols),
                None if self.asked_columns.as_deref() != Some(&self.table) => {
                    self.asked_columns = Some(self.table.clone());
                    out = Outcome::NeedColumns(self.table.clone());
                }
                None => {}
            }
        }

        let max_h = (ctx.content_rect().height() - 150.0).max(300.0);
        let modal = egui::Modal::new(Id::new("import-dialog"))
            .frame(Frame::new().fill(theme::SURFACE()).stroke(Stroke::new(1.0_f32, theme::BORDER())).corner_radius(12).inner_margin(Margin::same(20)))
            .show(ctx, |ui| {
                ui.set_width(WIDTH);
                ui.label(RichText::new("Import data").heading().color(theme::TEXT()));
                ui.label(RichText::new(format!("into schema {}", self.schema)).size(12.5).color(theme::MUTED()));
                ui.add_space(10.0);
                egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                    ui.add_enabled_ui(!self.running && !matches!(self.done, Some(Ok(_))), |ui| self.form(ui, ctx));
                });
                ui.add_space(10.0);
                if let Some(o) = self.footer(ui) {
                    out = o;
                }
            });
        if modal.should_close() && !self.running && matches!(out, Outcome::Keep) {
            out = Outcome::Close;
        }
        out
    }

    fn form(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        // File.
        ui.horizontal(|ui| {
            if ui.button("Choose file…").clicked() {
                self.pick_file(ctx);
            }
            match (&self.path, &self.opened) {
                (Some(p), Some(o)) => {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    ui.label(RichText::new(name).monospace().color(theme::TEXT())).on_hover_text(p.display().to_string());
                    ui.label(RichText::new(format!("{} · {} sampled", o.format.label(), plural_rows(self.sampled))).size(12.0).color(theme::MUTED()));
                }
                (Some(_), None) if self.loading.is_some() => {
                    ui.spinner();
                    ui.label(RichText::new("Reading…").color(theme::MUTED()));
                }
                _ => {
                    ui.label(RichText::new("CSV, TSV, GeoJSON or Shapefile (.shp)").color(theme::FAINT()));
                }
            }
        });
        if let Some(e) = &self.error {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(e).color(theme::ERROR())).wrap());
        }
        let Some(o) = self.opened.clone() else {
            if self.path.is_none() {
                ui.add_space(24.0);
            }
            return;
        };
        if let Some(note) = &o.note {
            ui.add_space(2.0);
            ui.label(RichText::new(note).size(12.0).color(theme::WARN()));
        }
        ui.add_space(10.0);

        // Target.
        ui.horizontal(|ui| {
            ui.label(RichText::new("Target").color(theme::MUTED()));
            if ui.radio(self.create, "New table").clicked() {
                self.set_create(true);
            }
            if ui.add_enabled(!self.tables.is_empty(), egui::RadioButton::new(!self.create, "Append to existing table")).clicked() {
                self.set_create(false);
            }
            ui.add_space(8.0);
            if self.create {
                if ui.add(TextEdit::singleline(&mut self.table).desired_width(220.0).hint_text("table name")).changed() {
                    self.auto_name = None;
                }
            } else {
                let before = self.table.clone();
                egui::ComboBox::from_id_salt("import-table").selected_text(&self.table).width(220.0).show_ui(ui, |ui| {
                    for t in &self.tables {
                        ui.selectable_value(&mut self.table, t.clone(), t);
                    }
                });
                if self.table != before {
                    self.mapped_for = None;
                    self.target.clear();
                }
            }
            if o.format == Format::Csv {
                ui.add_space(12.0);
                ui.label(RichText::new("Delimiter").color(theme::MUTED()));
                let current = DELIMITERS.iter().find(|(d, _)| *d == o.delimiter).map_or("?", |(_, n)| *n);
                let mut chosen = None;
                egui::ComboBox::from_id_salt("import-delim").selected_text(current).width(100.0).show_ui(ui, |ui| {
                    for (d, name) in DELIMITERS {
                        if ui.selectable_label(d == o.delimiter, name).clicked() {
                            chosen = Some(d);
                        }
                    }
                });
                if let (Some(d), Some(p)) = (chosen.filter(|d| *d != o.delimiter), self.path.clone()) {
                    self.open(ctx, p, Some(d));
                }
            }
        });

        // Geometry options.
        let point = self.cols.iter().position(|c| c.kind == Kind::Point);
        if let Some(pi) = point {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.cols[pi].include, "Add a point column from");
                ui.add_enabled_ui(self.cols[pi].include, |ui| {
                    let label = |i: usize| o.columns.get(i).cloned().unwrap_or_default();
                    egui::ComboBox::from_id_salt("import-lon").selected_text(format!("lon: {}", label(self.lon))).show_ui(ui, |ui| {
                        for (i, c) in o.columns.iter().enumerate() {
                            ui.selectable_value(&mut self.lon, i, c);
                        }
                    });
                    egui::ComboBox::from_id_salt("import-lat").selected_text(format!("lat: {}", label(self.lat))).show_ui(ui, |ui| {
                        for (i, c) in o.columns.iter().enumerate() {
                            ui.selectable_value(&mut self.lat, i, c);
                        }
                    });
                });
            });
        }
        if self.has_geometry() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("SRID").color(theme::MUTED()));
                ui.add(TextEdit::singleline(&mut self.srid).desired_width(70.0).hint_text("4326"));
                let known = o.srid.is_some_and(|s| self.srid.trim() == s.to_string());
                let hint = if known { "read from the file" } else { "coordinate system of the geometry" };
                ui.label(RichText::new(hint).size(12.0).color(theme::FAINT()));
                if self.create {
                    ui.add_space(12.0);
                    ui.checkbox(&mut self.spatial_index, "Spatial index (GiST)");
                }
            });
        }
        if self.create {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Primary key").color(theme::MUTED()));
                self.settle_key();
                let new = format!("new column {}", self.key_name().filter(|_| self.key == Key::New).unwrap_or_else(|| "id".into()));
                let text = match &self.key {
                    Key::New => new.clone(),
                    Key::Column(i) => self.cols[*i].name.trim().to_string(),
                    Key::None => "none".into(),
                };
                egui::ComboBox::from_id_salt("import-key").selected_text(text).width(200.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.key, Key::New, new);
                    for (i, c) in self.cols.iter().enumerate().filter(|(_, c)| c.include && c.ty != "geometry") {
                        ui.selectable_value(&mut self.key, Key::Column(i), c.name.trim());
                    }
                    ui.selectable_value(&mut self.key, Key::None, "none");
                });
                let hint = match self.key {
                    Key::New => "numbered 1, 2, 3…: lets rows be edited",
                    Key::Column(_) => "must be unique and not empty",
                    Key::None => "rows can't be edited without a key",
                };
                ui.label(RichText::new(hint).size(12.0).color(theme::FAINT()));
            });
        }
        ui.add_space(10.0);

        // Columns.
        let append = !self.create;
        let target = self.target.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Columns").color(theme::MUTED()));
            // Matching is by name: say which of the table's columns stay empty,
            // so a renamed field isn't silently dropped.
            let empty: Vec<&str> = target
                .iter()
                .filter(|(n, _)| !self.self_filled.contains(n) && !self.cols.iter().any(|c| c.include && c.name == *n))
                .map(|(n, _)| n.as_str())
                .collect();
            if append && !empty.is_empty() && !target.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("No values for {}: pick a column of the table next to a field", empty.join(", "))).size(12.0).color(theme::WARN()));
            }
        });
        ui.add_space(2.0);
        egui::ScrollArea::vertical().id_salt("import-cols").max_height(190.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("import-cols-grid").num_columns(4).spacing([14.0, 5.0]).striped(true).show(ui, |ui| {
                for (i, c) in self.cols.iter_mut().enumerate() {
                    ui.checkbox(&mut c.include, "");
                    let source = RichText::new(&c.source).monospace().color(if c.include { theme::TEXT() } else { theme::FAINT() });
                    ui.allocate_ui_with_layout(egui::vec2(220.0, 20.0), Layout::left_to_right(Align::Center), |ui| {
                        ui.set_min_width(220.0);
                        ui.add(egui::Label::new(source).truncate());
                    });
                    if append && !target.is_empty() {
                        egui::ComboBox::from_id_salt(("import-target", i)).selected_text(&c.name).width(200.0).show_ui(ui, |ui| {
                            for (name, ty) in &target {
                                if ui.selectable_label(*name == c.name, name).clicked() {
                                    c.name = name.clone();
                                    c.ty = ty_of(ty);
                                    c.include = true;
                                }
                            }
                        });
                        ui.label(RichText::new(sql_short(&c.ty, &target, &c.name)).size(12.0).color(theme::MUTED()));
                    } else {
                        ui.add_sized([200.0, 20.0], TextEdit::singleline(&mut c.name));
                        if append {
                            ui.label(RichText::new("loading columns…").size(12.0).color(theme::FAINT()));
                        } else {
                            egui::ComboBox::from_id_salt(("import-type", i)).selected_text(&c.ty).width(110.0).show_ui(ui, |ui| {
                                for t in import::TYPES {
                                    ui.selectable_value(&mut c.ty, t.to_string(), t);
                                }
                            });
                        }
                    }
                    ui.end_row();
                }
            });
        });
        ui.add_space(10.0);

        // Preview.
        ui.label(RichText::new(format!("Preview (first {})", plural_rows(self.preview.rows.len()))).color(theme::MUTED()));
        ui.add_space(2.0);
        let size = egui::vec2(ui.available_width(), 200.0);
        ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            let source = grid::Source { table: "import", sql: None, relations: &[], counts: &[] };
            grid::show(ui, Id::new("import-preview"), &self.preview, &mut self.grid, None, 0, &source, None);
        });
    }

    fn footer(&mut self, ui: &mut Ui) -> Option<Outcome> {
        let mut out = None;
        let problem = if self.running || self.done.is_some() { None } else { self.problem() };
        ui.horizontal(|ui| {
            // Status on the left: progress, result or what's missing.
            let status = ui.available_width() - 190.0;
            ui.allocate_ui_with_layout(egui::vec2(status.max(100.0), 34.0), Layout::left_to_right(Align::Center), |ui| match &self.done {
                Some(Ok(n)) => {
                    ui.add(egui::Label::new(RichText::new(format!("Imported {} into {}.{}", plural_rows(*n), self.schema, self.table)).color(theme::OK())).wrap());
                }
                Some(Err(e)) => {
                    egui::ScrollArea::vertical().max_height(60.0).id_salt("import-err").show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(e).color(theme::ERROR())).wrap());
                    });
                }
                None if self.running => {
                    ui.spinner();
                    ui.label(RichText::new(format!("Importing… {}", plural_rows(self.progress))).color(theme::MUTED()));
                }
                None => {
                    if let Some(p) = &problem {
                        ui.add(egui::Label::new(RichText::new(p).size(12.5).color(theme::WARN())).wrap());
                    }
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if matches!(self.done, Some(Ok(_))) {
                    if ui.add(Button::new(RichText::new("Open table").color(theme::BG())).fill(theme::ACCENT())).clicked() {
                        out = Some(Outcome::OpenTable(self.table.clone()));
                    }
                    if ui.button("Close").clicked() {
                        out = Some(Outcome::Close);
                    }
                    return;
                }
                let ready = self.opened.is_some() && problem.is_none() && !self.running;
                if ui.add_enabled(ready, Button::new(RichText::new("Import").color(theme::BG())).fill(theme::ACCENT())).clicked()
                    && let Some(req) = self.request()
                {
                    self.running = true;
                    self.progress = 0;
                    self.done = None;
                    out = Some(Outcome::Start(req));
                }
                if ui.add_enabled(!self.running, Button::new("Cancel")).clicked() {
                    out = Some(Outcome::Close);
                }
            });
        });
        out
    }
}

/// The dialog's type name for a column type of an existing table: only
/// geometry matters (it gets EWKT values), the server converts the rest.
fn ty_of(data_type: &str) -> String {
    if data_type.starts_with("geometry") { "geometry".into() } else { data_type.to_string() }
}

/// The table's full type for a target column, for display.
fn sql_short(ty: &str, target: &[(String, String)], name: &str) -> String {
    target.iter().find(|(n, _)| n == name).map_or_else(|| ty.to_string(), |(_, t)| t.clone())
}

fn plural_rows(n: usize) -> String {
    crate::app::plural(n, "row")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(i: usize, name: &str) -> Col {
        Col { kind: Kind::Field(i), include: true, source: name.into(), name: name.into(), ty: "text".into() }
    }

    /// Rename a column to `code` and make it the key, untick it, then name
    /// another one `code`: the key doesn't follow the name.
    #[test]
    fn key_follows_the_column_not_its_name() {
        let mut d = ImportDialog::new("c".into(), "public".into(), None);
        d.cols = vec![col(0, "shapeiso"), col(1, "shapename")];
        d.cols[0].name = "code".into();
        d.key = Key::Column(0);
        assert_eq!(d.key_name().as_deref(), Some("code"));
        d.cols[0].include = false;
        d.cols[1].name = "code".into();
        d.settle_key();
        assert!(d.key == Key::New);
        assert_eq!(d.key_name().as_deref(), Some("id"));
    }
}
