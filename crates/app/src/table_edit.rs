//! The "Edit table" dialog: rename the table, add, rename, retype and drop
//! columns, set not null, defaults and the primary key. Shows the SQL it will
//! run; the statements come from `rgrip_core::alter` and run in the worker
//! (`Cmd::AlterTable`) as one transaction.

use crate::icons::{self, Svg};
use crate::theme;
use eframe::egui::{self, Align, Button, Frame, Id, Layout, Margin, RichText, Stroke, TextEdit, Ui};
use rgrip_core::ColumnInfo;
use rgrip_core::alter::{self, EditColumn, TableEdit};

const WIDTH: f32 = 860.0;
const TYPES: [&str; 14] = [
    "integer", "bigint", "numeric", "double precision", "text", "varchar(255)", "boolean", "date", "timestamp", "timestamptz", "jsonb", "uuid",
    "geometry", "geometry(Geometry, 4326)",
];

pub enum Outcome {
    Keep,
    Close,
    /// Run these statements; the table is called `new_name` afterwards.
    Apply { statements: Vec<String>, new_name: String },
}

pub struct TableEditor {
    pub conn: String,
    pub schema: String,
    pub table: String,
    name: String,
    cols: Vec<EditColumn>,
    pk_constraint: Option<String>,
    loading: bool,
    running: bool,
    error: Option<String>,
    /// A red (production) connection: the Apply button says so.
    production: bool,
}

impl TableEditor {
    pub fn new(conn: String, schema: String, table: String, production: bool) -> Self {
        Self { conn, schema, name: table.clone(), table, cols: Vec::new(), pk_constraint: None, loading: true, running: false, error: None, production }
    }

    pub fn loaded(&mut self, result: Result<(Vec<ColumnInfo>, Option<String>), String>) {
        self.loading = false;
        match result {
            Ok((cols, pk)) => {
                self.cols = cols.iter().map(EditColumn::from_info).collect();
                self.pk_constraint = pk;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The statements failed: back to editing, with the server's message.
    pub fn failed(&mut self, e: String) {
        self.running = false;
        self.error = Some(e);
    }

    /// Debug screenshot scenes: some edits, so the SQL shows.
    #[cfg(debug_assertions)]
    pub fn demo_edits(&mut self) -> bool {
        if self.loading || self.cols.len() < 3 {
            return false;
        }
        self.cols[1].name = "name".into();
        self.cols[2].drop = true;
        let mut c = EditColumn::new_column();
        c.name = "verified_at".into();
        c.data_type = "timestamptz".into();
        self.cols.push(c);
        true
    }

    fn edit(&self) -> TableEdit<'_> {
        TableEdit { schema: &self.schema, table: &self.table, new_name: &self.name, columns: &self.cols, pk_constraint: self.pk_constraint.as_deref() }
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> Outcome {
        let mut out = Outcome::Keep;
        let max_h = (ctx.content_rect().height() - 330.0).max(160.0);
        let modal = egui::Modal::new(Id::new("table-editor"))
            .frame(Frame::new().fill(theme::SURFACE()).stroke(Stroke::new(1.0_f32, theme::BORDER())).corner_radius(12).inner_margin(Margin::same(20)))
            .show(ctx, |ui| {
                ui.set_width(WIDTH);
                ui.label(RichText::new("Edit table").heading().color(theme::TEXT()));
                ui.label(RichText::new(format!("{}.{}", self.schema, self.table)).size(12.5).color(theme::MUTED()));
                ui.add_space(10.0);
                if self.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Loading columns…").color(theme::MUTED()));
                    });
                } else {
                    ui.add_enabled_ui(!self.running, |ui| self.form(ui, max_h));
                }
                ui.add_space(12.0);
                if let Some(o) = self.footer(ui) {
                    out = o;
                }
            });
        if modal.should_close() && !self.running && matches!(out, Outcome::Keep) {
            out = Outcome::Close;
        }
        out
    }

    fn form(&mut self, ui: &mut Ui, max_h: f32) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Name").color(theme::MUTED()));
            ui.add(TextEdit::singleline(&mut self.name).desired_width(260.0));
        });
        ui.add_space(10.0);

        let head = |ui: &mut Ui, text: &str| {
            ui.label(RichText::new(text).size(12.0).color(theme::MUTED()));
        };
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("table-editor-cols").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("table-editor-grid").num_columns(6).spacing([12.0, 6.0]).show(ui, |ui| {
                head(ui, "");
                head(ui, "Column");
                head(ui, "Type");
                head(ui, "Not null");
                head(ui, "Default");
                head(ui, "Key");
                ui.end_row();
                for (i, c) in self.cols.iter_mut().enumerate() {
                    let ctx = ui.ctx().clone();
                    // Drop (or remove a new row) / bring back.
                    let (svg, tint, tip) = match (c.drop, c.original.is_some()) {
                        (true, _) => (Svg::Undo, theme::MUTED(), "Keep this column"),
                        (false, true) => (Svg::Trash, theme::FAINT(), "Drop this column (its data goes too)"),
                        (false, false) => (Svg::X, theme::FAINT(), "Remove"),
                    };
                    if ui.add(Button::image(icons::image(&ctx, svg, 14.0, tint)).frame(false)).on_hover_text(tip).clicked() {
                        if c.original.is_some() {
                            c.drop = !c.drop;
                        } else {
                            remove = Some(i);
                        }
                    }
                    if c.drop {
                        let name = c.original.as_ref().map_or("", |o| o.name.as_str());
                        ui.label(RichText::new(name).monospace().strikethrough().color(theme::ERROR()));
                        ui.label(RichText::new("dropped").size(12.0).color(theme::ERROR()));
                        ui.label("");
                        ui.label("");
                        ui.label("");
                        ui.end_row();
                        continue;
                    }
                    let renamed = c.original.as_ref().is_some_and(|o| o.name != c.name.trim());
                    let name = ui.add_sized([190.0, 22.0], TextEdit::singleline(&mut c.name).hint_text("column name").font(egui::TextStyle::Monospace));
                    if renamed {
                        name.on_hover_text(format!("was {}", c.original.as_ref().map_or("", |o| o.name.as_str())));
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.add_sized([180.0, 22.0], TextEdit::singleline(&mut c.data_type).font(egui::TextStyle::Monospace));
                        egui::ComboBox::from_id_salt(("table-editor-type", i)).width(22.0).selected_text("").show_ui(ui, |ui| {
                            for t in TYPES {
                                if ui.selectable_label(c.data_type == t, t).clicked() {
                                    c.data_type = t.to_string();
                                }
                            }
                        });
                    });
                    let mut not_null = !c.nullable || c.primary_key;
                    if ui.add_enabled(!c.primary_key, egui::Checkbox::without_text(&mut not_null)).on_disabled_hover_text("Key columns are never null").changed() {
                        c.nullable = !not_null;
                    }
                    ui.add_sized([170.0, 22.0], TextEdit::singleline(&mut c.default).hint_text("none").font(egui::TextStyle::Monospace));
                    ui.checkbox(&mut c.primary_key, "");
                    ui.end_row();
                }
            });
        });
        if let Some(i) = remove {
            self.cols.remove(i);
        }
        ui.add_space(4.0);
        if ui.add(Button::image_and_text(icons::image(ui.ctx(), Svg::Table, 14.0, theme::MUTED()), "Add column")).clicked() {
            self.cols.push(EditColumn::new_column());
        }

        // The SQL, as it will run.
        let statements = alter::statements(&self.edit());
        if !statements.is_empty() {
            ui.add_space(12.0);
            ui.label(RichText::new("SQL").size(12.0).color(theme::MUTED()));
            ui.add_space(2.0);
            Frame::new().fill(theme::BG()).corner_radius(6).inner_margin(Margin::same(8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().id_salt("table-editor-sql").max_height(130.0).show(ui, |ui| {
                    for s in &statements {
                        let color = if s.contains(" drop column ") || s.contains(" drop constraint ") { theme::ERROR() } else { theme::TEXT() };
                        ui.add(egui::Label::new(RichText::new(format!("{s};")).monospace().size(12.0).color(color)).wrap());
                    }
                });
            });
        }
    }

    fn footer(&mut self, ui: &mut Ui) -> Option<Outcome> {
        let mut out = None;
        let problem = if self.loading { None } else { alter::problem(&self.edit()) };
        let statements = if problem.is_none() && !self.loading { alter::statements(&self.edit()) } else { Vec::new() };
        let drops = self.cols.iter().filter(|c| c.drop && c.original.is_some()).count();
        ui.horizontal(|ui| {
            let status = ui.available_width() - 260.0;
            ui.allocate_ui_with_layout(egui::vec2(status.max(100.0), 34.0), Layout::left_to_right(Align::Center), |ui| {
                if self.running {
                    ui.spinner();
                    ui.label(RichText::new("Applying…").color(theme::MUTED()));
                } else if let Some(e) = &self.error {
                    egui::ScrollArea::vertical().max_height(60.0).id_salt("table-editor-err").show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(e).color(theme::ERROR())).wrap());
                    });
                } else if let Some(p) = &problem {
                    ui.add(egui::Label::new(RichText::new(p).size(12.5).color(theme::WARN())).wrap());
                } else if statements.is_empty() && !self.loading {
                    ui.label(RichText::new("No changes yet").size(12.5).color(theme::FAINT()));
                } else if !self.loading {
                    ui.label(RichText::new("All or nothing: if one statement fails, none apply").size(12.5).color(theme::FAINT()));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let risky = drops > 0 || self.production;
                let label = match (drops, self.production) {
                    (0, false) => "Apply".to_string(),
                    (0, true) => "Apply on production".to_string(),
                    (n, _) => format!("Apply, dropping {}", crate::app::plural(n, "column")),
                };
                let fill = if risky { theme::ERROR() } else { theme::ACCENT() };
                let ready = !statements.is_empty() && !self.running;
                if ui.add_enabled(ready, Button::new(RichText::new(label).color(theme::BG())).fill(fill)).clicked() {
                    self.running = true;
                    self.error = None;
                    out = Some(Outcome::Apply { statements: statements.clone(), new_name: self.name.trim().to_string() });
                }
                if ui.add_enabled(!self.running, Button::new("Cancel")).clicked() {
                    out = Some(Outcome::Close);
                }
            });
        });
        out
    }
}
