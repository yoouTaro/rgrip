use crate::grid::{self, GridEvent, GridState, Sort};
use crate::icons::Svg;
use crate::worker::{Cmd, ConnId, Ev, SourceTable, TabId, Worker};
use crate::widgets::{self, Icon, TreeRow};
use crate::{highlight, theme};
use eframe::egui::{
    self, Align, Button, Color32, FontId, Frame, Id, Key, Layout, Margin,
    Modifiers, RichText, Sense, Stroke, TextEdit, TextStyle, Ui,
};
use crate::completion::{self, Popup};
use rgrip_core::complete::{self, Catalog};
use rgrip_core::config::{SavedQuery, Settings, ThemeChoice};
use rgrip_core::update::{self, Release};
use rgrip_core::{config, export, sql, *};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE_SIZE: i64 = rgrip_core::postgres::PAGE_SIZE as i64;

enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> From<Result<T, String>> for Load<T> {
    fn from(r: Result<T, String>) -> Self {
        match r {
            Ok(v) => Load::Ready(v),
            Err(e) => Load::Failed(e),
        }
    }
}

enum Status {
    Disconnected,
    Connecting,
    Connected(String),
    Failed(String),
}

struct Conn {
    cfg: ConnectionConfig,
    status: Status,
    schemas: Option<Load<Vec<String>>>,
    tables: HashMap<String, Load<Vec<TableInfo>>>,
    columns: HashMap<(String, String), Load<Vec<ColumnInfo>>>,
    /// Every table and column, for autocompletion.
    catalog: Arc<Catalog>,
    /// Databases of the server, for the console's database picker.
    databases: Option<Load<Vec<String>>>,
    /// Schemas and catalog of the other databases consoles were pointed at.
    dbs: HashMap<String, DbCache>,
}

/// What a console needs to know about a database other than the connection's own.
#[derive(Default)]
struct DbCache {
    schemas: Option<Load<Vec<String>>>,
    catalog: Arc<Catalog>,
}

impl Conn {
    fn new(cfg: ConnectionConfig) -> Self {
        Self {
            cfg,
            status: Status::Disconnected,
            schemas: None,
            tables: HashMap::new(),
            columns: HashMap::new(),
            catalog: Arc::default(),
            databases: None,
            dbs: HashMap::new(),
        }
    }

    /// `database` as a key into `dbs`: `None` for the connection's own.
    fn other_db<'a>(&self, database: Option<&'a str>) -> Option<&'a str> {
        database.filter(|d| *d != self.cfg.database)
    }

    fn schemas_in(&self, database: Option<&str>) -> &Option<Load<Vec<String>>> {
        match self.other_db(database) {
            Some(d) => self.dbs.get(d).map(|db| &db.schemas).unwrap_or(&None),
            None => &self.schemas,
        }
    }

    fn catalog_in(&self, database: Option<&str>) -> Arc<Catalog> {
        match self.other_db(database) {
            Some(d) => self.dbs.get(d).map(|db| db.catalog.clone()).unwrap_or_default(),
            None => self.catalog.clone(),
        }
    }

    fn clear_cache(&mut self) {
        self.databases = None;
        self.dbs.clear();
        self.schemas = None;
        self.tables.clear();
        self.columns.clear();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Result(usize),
    /// Map of a result set's geometries.
    Map(usize),
    /// Grid and map side by side, selection linked.
    Split(usize),
    Output,
}

/// What a table tab shows.
#[derive(Clone, Copy, PartialEq, Default)]
enum Pane {
    #[default]
    Grid,
    Map,
    Split,
}

struct Console {
    sql: String,
    /// Last edit, for autosaving consoles that belong to a saved query.
    last_edit: Option<Instant>,
    /// Sorted char range of the editor cursor/selection.
    cursor: std::ops::Range<usize>,
    focus: bool,
    running: Option<Instant>,
    /// Query waiting for the connection to come up.
    pending: Option<String>,
    output: Option<Result<QueryOutput, String>>,
    view: View,
    grid: GridState,
    popup: Option<Popup>,
    /// The running query changes schema, so refresh the catalog afterwards.
    ddl: bool,
    /// Single read-only statement being paged server-side, and the current offset.
    paged: Option<String>,
    /// The statement as run, before a sort wrapped it.
    paged_base: Option<String>,
    /// Map area filter on the result (WHERE text), empty for none.
    area: String,
    /// Result sort: which result, column, ascending.
    order: Option<(usize, String, bool)>,
    /// The rows of an in-memory sorted result in their original order.
    unsorted: Option<(usize, Vec<Vec<Option<String>>>)>,
    offset: usize,
    map: crate::map::MapView,
    /// The saved query this console was opened from / saved as.
    saved_id: Option<String>,
    /// SQL of the last run (for "Copy for AI agent").
    last_sql: String,
    /// Schema picked in the toolbar; `None` = the default.
    schema: Option<String>,
    /// Schema the last run used (paging, export and queued runs reuse it).
    run_schema: Option<String>,
    /// More schemas searched after `schema`, in the order they were ticked.
    more_schemas: Vec<String>,
    /// Database picked in the toolbar; `None` = the connection's own.
    database: Option<String>,
    /// Database the last run used (like `run_schema`).
    run_database: Option<String>,
    /// Outgoing relations of the tables the result reads from, named after
    /// the result's columns (found after the run, once the server answers).
    relations: Vec<Relation>,
    /// What `relations` is computed from (again when the catalog arrives).
    sources: Option<(Vec<Option<rgrip_core::relations::Origin>>, Vec<SourceTable>)>,
    /// Related-row counts for the rows the row menu was opened on.
    probe: Option<Probe>,
    /// Related rows (and column lookups) shown in a popup over the tab.
    related: Option<Related>,
    /// Rows of the whole paged result, once counted.
    total: Option<i64>,
    /// The count was asked for (once per run, even if it fails).
    count_asked: bool,
    /// The column origins were asked for (once per run).
    origins_asked: bool,
    /// Bumped on every Run: answers from an older run are dropped.
    run: u64,
}

/// Schemas a console can pick in `database`: the connection's, without system ones.
fn user_schemas(conn: &Conn, database: Option<&str>) -> Vec<String> {
    match conn.schemas_in(database) {
        Some(Load::Ready(list)) => list
            .iter()
            .filter(|s| !matches!(s.as_str(), "pg_catalog" | "information_schema") && !s.starts_with("pg_toast"))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

impl Console {
    /// The schema unqualified names resolve in: the picked one, else `public`
    /// (or the first schema when there is no `public`).
    fn schema_or_default<'a>(&'a self, schemas: &'a [String]) -> &'a str {
        match &self.schema {
            Some(s) => s,
            None if schemas.is_empty() || schemas.iter().any(|s| s == "public") => "public",
            None => &schemas[0],
        }
    }

    /// The schema to send with a run. Not picked and not known yet: `None`,
    /// i.e. the server's own search_path.
    fn schema_for_run(&self, schemas: &[String]) -> Option<String> {
        (self.schema.is_some() || !schemas.is_empty()).then(|| self.search_list(schemas).join("\n"))
    }

    /// The picked schemas to save with a query, main one first.
    fn picked_schemas(&self) -> Vec<String> {
        self.schema.iter().chain(&self.more_schemas).cloned().collect()
    }

    /// Every schema this console searches, main one first.
    fn search_list(&self, schemas: &[String]) -> Vec<String> {
        let mut list = vec![self.schema_or_default(schemas).to_string()];
        list.extend(self.more_schemas.iter().filter(|s| !list.contains(s)).cloned().collect::<Vec<_>>());
        list
    }

    /// The paged statement with the map area filter applied (before sorting).
    fn filtered_base(&self) -> Option<String> {
        let base = self.paged_base.clone()?;
        Some(if self.area.is_empty() { base } else { sql::filtered_query(&base, &self.area).unwrap_or(base) })
    }

    /// `base` with the current result sort applied.
    fn sorted(&self, base: String) -> Option<String> {
        match &self.order {
            Some((_, name, asc)) => sql::sorted_query(&base, name, *asc).or(Some(base)),
            None => Some(base),
        }
    }
}

impl Console {
    fn new() -> Self {
        Self {
            sql: String::new(),
            last_edit: None,
            cursor: 0..0,
            focus: true,
            running: None,
            pending: None,
            output: None,
            view: View::Output,
            grid: GridState::default(),
            popup: None,
            ddl: false,
            paged: None,
            paged_base: None,
            area: String::new(),
            order: None,
            unsorted: None,
            offset: 0,
            map: Default::default(),
            saved_id: None,
            last_sql: String::new(),
            schema: None,
            run_schema: None,
            more_schemas: Vec::new(),
            database: None,
            run_database: None,
            relations: Vec::new(),
            sources: None,
            probe: None,
            related: None,
            total: None,
            count_asked: false,
            origins_asked: false,
            run: 0,
        }
    }
}

struct TableView {
    schema: String,
    table: String,
    filter: String,
    applied_filter: String,
    order: Option<(String, bool)>,
    page: i64,
    loading: bool,
    pending: bool,
    data: Option<Result<ResultSet, String>>,
    total: Option<i64>,
    grid: GridState,
    pane: Pane,
    map: crate::map::MapView,
    /// Primary key columns; empty means the table can't be edited inline.
    key: Vec<String>,
    /// Date/time columns by name, for the edit picker.
    temporal: HashMap<String, rgrip_core::datetime::TemporalKind>,
    /// Foreign keys from and to this table, then inferred relations.
    relations: Vec<Relation>,
    /// What the inferred relations are computed from (again when the
    /// catalog arrives after the page).
    columns: Vec<ColumnInfo>,
    fk_relations: Vec<Relation>,
    /// Related rows shown in a popup over the tab.
    related: Option<Related>,
    /// Related-row counts for the rows the row menu was opened on.
    probe: Option<Probe>,
    /// Pending inline edits: (row, col) → new value.
    changes: std::collections::BTreeMap<(usize, usize), Option<String>>,
    editor: Option<grid::CellEditor>,
    saving: bool,
}

/// Match counts for each relation (and each of its alternative keys) of
/// some rows, from the database.
struct Probe {
    /// The rows counted for.
    rows: Vec<usize>,
    /// (relation, variant) per probe, in the order sent.
    map: Vec<(usize, usize)>,
    /// Filled in when the counts arrive.
    counts: Vec<Option<i64>>,
}

impl Probe {
    /// For `rows`, the best count per relation (for the menu).
    fn best(&self, rows: &[usize], relations: usize) -> Vec<Option<i64>> {
        let mut best = vec![None; relations];
        if self.rows != rows {
            return Vec::new();
        }
        for (&(ri, _), c) in self.map.iter().zip(&self.counts) {
            if let (Some(c), Some(b)) = (c, best.get_mut(ri)) {
                *b = Some(b.unwrap_or(0).max(*c));
            }
        }
        best
    }

    /// For `rows`, which variants of relation `ri` matched (empty: unknown).
    fn matched(&self, rows: &[usize], ri: usize) -> Vec<usize> {
        if self.rows != rows {
            return Vec::new();
        }
        self.map.iter().zip(&self.counts).filter(|((r, _), c)| *r == ri && c.is_some_and(|c| c > 0)).map(|((_, v), _)| *v).collect()
    }
}

/// What a related-rows popup shows: rows of `schema.table` matching `filter`.
#[derive(Clone)]
struct RelatedTarget {
    schema: String,
    table: String,
    filter: String,
    /// e.g. "← posts (user_id)".
    label: String,
}

/// The related-rows popup of a table tab. Read-only; it can follow further
/// relations, with Back to return.
struct Related {
    target: RelatedTarget,
    back: Vec<RelatedTarget>,
    data: Option<Result<ResultSet, String>>,
    total: Option<i64>,
    relations: Vec<Relation>,
    columns: Vec<ColumnInfo>,
    fk_relations: Vec<Relation>,
    grid: GridState,
    probe: Option<Probe>,
}

impl Related {
    fn query(&self) -> TableQuery {
        TableQuery {
            schema: self.target.schema.clone(),
            table: self.target.table.clone(),
            filter: self.target.filter.clone(),
            order: None,
            limit: PAGE_SIZE,
            offset: 0,
        }
    }
}

impl TableView {
    fn new(schema: String, table: String, filter: String) -> Self {
        Self {
            schema,
            table,
            filter: filter.clone(),
            applied_filter: filter,
            order: None,
            page: 0,
            loading: false,
            pending: false,
            data: None,
            total: None,
            grid: GridState::default(),
            pane: Pane::Grid,
            map: Default::default(),
            key: Vec::new(),
            temporal: HashMap::new(),
            relations: Vec::new(),
            columns: Vec::new(),
            fk_relations: Vec::new(),
            related: None,
            probe: None,
            changes: Default::default(),
            editor: None,
            saving: false,
        }
    }

    fn query(&self) -> TableQuery {
        TableQuery {
            schema: self.schema.clone(),
            table: self.table.clone(),
            filter: self.applied_filter.clone(),
            order: self.order.clone(),
            limit: PAGE_SIZE,
            offset: self.page * PAGE_SIZE,
        }
    }
}

enum TabKind {
    Console(Console),
    Table(TableView),
    Layout(Box<crate::layout_tab::LayoutTab>),
    Diagram(Box<crate::diagram_tab::DiagramTab>),
}

impl TabKind {
    /// The related-rows popup and row-menu counts of a console or table tab.
    fn related_slots(&mut self) -> Option<(&mut Option<Related>, &mut Option<Probe>)> {
        match self {
            TabKind::Console(c) => Some((&mut c.related, &mut c.probe)),
            TabKind::Table(t) => Some((&mut t.related, &mut t.probe)),
            TabKind::Layout(_) | TabKind::Diagram(_) => None,
        }
    }
}

struct Tab {
    id: TabId,
    conn: ConnId,
    title: String,
    kind: TabKind,
}

struct Dialog {
    cfg: ConnectionConfig,
    /// Pasted connection string, and the result of applying it.
    url: String,
    url_status: Option<Result<(), String>>,
    port: String,
    ssh_port: String,
    is_new: bool,
    testing: bool,
    test: Option<Result<String, String>>,
    tab: DialogTab,
}

#[derive(Clone, Copy, PartialEq)]
enum DialogTab {
    General,
    Ssl,
    Ssh,
}

impl Dialog {
    fn new(cfg: ConnectionConfig, is_new: bool) -> Self {
        Self {
            port: cfg.port.to_string(),
            url: String::new(),
            url_status: None,
            ssh_port: cfg.ssh.port.to_string(),
            cfg,
            is_new,
            testing: false,
            test: None,
            tab: DialogTab::General,
        }
    }
}

enum Action {
    NewConnection,
    Connect(usize),
    Disconnect(usize),
    Edit(usize),
    /// Set a connection's tab color (explorer menu).
    SetColor(usize, rgrip_core::ConnColor),
    Delete(usize),
    Refresh(usize),
    NewConsole(usize),
    /// Open (or switch to) the ER diagram of a schema.
    OpenDiagram(usize, String),
    OpenTable(usize, String, String),
    /// Open a new console with a SELECT of a table.
    OpenInConsole(usize, String, String),
    /// Import a file into a schema, or into one of its tables.
    Import(usize, String, Option<String>),
    /// Edit a table's structure (columns, types, key, name).
    EditTable(usize, String, String),
    /// Open a new map layout tab.
    NewLayout(usize),
    /// Add a table's geometry column to the connection's map layout.
    AddToLayout(usize, String, String, String),
    LoadSchemas(usize),
    LoadTables(usize, String),
    LoadColumns(usize, String, String),
    ActivateTab(usize),
    Close(CloseWhich),
    OpenSaved(usize),
    SetTheme(ThemeChoice),
    /// Pick a palette by name; `true` for the dark list.
    SetPalette(bool, &'static str),
    SetAutoUpdate(bool),
    /// Open the slash command editor (Settings).
    EditCommands,
    /// The slash command editor on a new command with this name (empty: a generated one).
    NewCommand(String),
    CheckUpdate,
    InstallUpdate,
    RenameSaved(usize),
    DeleteSaved(usize),
    RevealSaved(usize),
    OpenQueriesFolder,
    Toggle(String),
    Select(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CloseWhich {
    One(usize),
    Others(usize),
    ToTheRight(usize),
    All,
}

/// Close tabs and keep `active` pointing at a sensible tab.
fn close_tabs<T>(tabs: &mut Vec<T>, active: &mut usize, which: CloseWhich) {
    match which {
        CloseWhich::One(i) if i < tabs.len() => {
            tabs.remove(i);
            if *active > i || *active >= tabs.len() {
                *active = active.saturating_sub(1);
            }
        }
        CloseWhich::Others(i) if i < tabs.len() => {
            let keep = tabs.remove(i);
            tabs.clear();
            tabs.push(keep);
            *active = 0;
        }
        CloseWhich::ToTheRight(i) => {
            tabs.truncate(i + 1);
            *active = (*active).min(tabs.len().saturating_sub(1));
        }
        CloseWhich::All => {
            tabs.clear();
            *active = 0;
        }
        _ => {}
    }
}

enum UpdateState {
    Idle,
    /// `manual`: report "up to date" and errors (launch checks stay quiet).
    Checking { manual: bool },
    Available(Release),
    Installing(Release),
}

/// A scripted demo (debug builds) for recording clips: `RGRIP_DEMO_STEPS`
/// is a `;`-separated list played after the scene settles. See
/// `App::demo_step` for the steps.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
struct DemoPlay {
    steps: Vec<String>,
    index: usize,
    /// When the current step started.
    since: Instant,
    /// Characters of the current `type:` / `keys:` step already sent.
    typed: usize,
}

struct CommandsEditor {
    list: Vec<rgrip_core::commands::SlashCommand>,
    selected: usize,
    /// Why Save didn't work.
    error: Option<String>,
    /// Arguments for the live preview.
    preview_args: String,
}

struct NamePrompt {
    name: String,
    target: NameTarget,
}

enum NameTarget {
    /// Save the console in this tab as a new query.
    NewFrom(TabId),
    /// Rename the saved query with this id.
    Rename(String),
}

/// Statements waiting for the user's go-ahead (see `rgrip_core::safety`).
struct Confirm {
    tab: TabId,
    kind: ConfirmKind,
    title: String,
    /// Statement, why it is risky, and whether it is dangerous anywhere
    /// (rather than just a write on production).
    items: Vec<(String, String, bool)>,
    /// The keyboard focus was put on Cancel.
    focused: bool,
}

enum ConfirmKind {
    Run { all: bool },
    Save,
    AddKey,
}

const READ_ONLY_HINT: &str = "This connection is read-only: Postgres rejects writes. Untick it in the connection's settings to edit.";

/// The statements of `sql` that need a go-ahead: dangerous ones anywhere,
/// any write on production.
fn risky_items(sql: &str, production: bool) -> Vec<(String, String, bool)> {
    rgrip_core::safety::classify(sql)
        .into_iter()
        .filter(|f| f.danger.is_some() || production)
        .map(|f| match f.danger {
            Some(why) => (sql[f.range].to_string(), why.to_string(), true),
            None => (sql[f.range].to_string(), "Writes to a production connection".to_string(), false),
        })
        .collect()
}

enum TabAction {
    /// Load the connection's schemas (for the console's schema picker).
    LoadSchemas,
    /// Map layout: run these layer queries.
    LoadLayers(Vec<crate::layout_tab::Action>),
    /// Show a message in the status bar.
    Notify(String),
    /// `/new [name]` in the console: open the slash command editor on a new command.
    NewCommand(String),
    /// Wrap the console's query to add `ST_Transform(col, 4326)` and re-run.
    Reproject(String),
    /// Filter the console's result to a map area (WHERE text), or clear it.
    ResultArea(Option<String>),
    /// Save the console's SQL (asks for a name the first time).
    Save,
    /// Write every row of the current query/table to a file.
    ExportAll(export::Format, std::path::PathBuf),
    Run { all: bool },
    /// Write a table tab's pending inline edits.
    SaveEdits,
    /// Give a table without any key an identity primary key, so it can be
    /// edited (imports and shapefile loads often come without one).
    AddKey,
    /// Open `schema.table` filtered with `filter` (following a foreign key).
    OpenFiltered { schema: String, table: String, filter: String },
    /// Open `schema.table` (from a diagram box).
    OpenTable { schema: String, table: String },
    /// Diagram: load the schema again.
    ReloadDiagram,
    /// Show related rows in the tab's popup (stacking on an open one).
    ShowRelated(RelatedTarget),
    /// Count related rows for the row menu (`modal`: the popup's rows).
    Probe { modal: bool, rows: Vec<usize>, probes: Vec<(String, String, String)>, map: Vec<(usize, usize)> },
    /// Popup: back to the previous related rows.
    RelatedBack,
    /// Sort a console result (result index, column index) by that column.
    SortResult(usize, usize),
    /// Next (`true`) or previous page of a paged console query.
    Page(bool),
    Cancel,
    Reload,
}

pub struct App {
    worker: Worker,
    conns: Vec<Conn>,
    tabs: Vec<Tab>,
    active: usize,
    next_tab: TabId,
    dialog: Option<Dialog>,
    filter: String,
    notice: Option<String>,
    saved: Vec<SavedQuery>,
    name_prompt: Option<NamePrompt>,
    confirm: Option<Confirm>,
    /// The next Run / Save was confirmed: don't ask again.
    skip_confirm: bool,
    settings: Settings,
    update: UpdateState,
    /// Close the window at the end of this frame (relaunching after an update).
    ctx_close: bool,
    was_focused: bool,
    egui_ctx: egui::Context,
    /// Transient status-bar message (text, color, when).
    toast: Option<(String, Color32, Instant)>,
    /// Keys of expanded explorer nodes.
    expanded: HashSet<String>,
    selected: Option<String>,
    /// A tree row was clicked last: typing jumps to a matching row.
    explorer_active: bool,
    /// Type-ahead buffer and when its last key came in.
    type_ahead: (String, Instant),
    /// (key, label) of the tree rows drawn last frame, in order.
    explorer_rows: Vec<(String, String)>,
    /// A row to scroll into view on the next frame.
    scroll_to_row: Option<String>,
    /// Slash command editor (Settings), while open.
    commands_editor: Option<CommandsEditor>,
    /// The Import data dialog, while open.
    import: Option<crate::import::ImportDialog>,
    table_editor: Option<crate::table_edit::TableEditor>,
    /// Debug demo recordings: the scripted steps being played.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    demo_play: Option<DemoPlay>,
    /// Debug demo recordings: pointer events for the next frame's input.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    demo_input: Vec<egui::Event>,
    /// Debug screenshot scenes: when the scene first looked settled.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    demo_settled: Option<Instant>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = config::load_settings();
        // Debug builds: RGRIP_DEMO_THEME=light|dark and RGRIP_DEMO_PALETTE=<name>
        // for screenshots (not saved).
        let mut theme_choice = match std::env::var("RGRIP_DEMO_THEME").as_deref() {
            Ok("light") if cfg!(debug_assertions) => ThemeChoice::Light,
            Ok("dark") if cfg!(debug_assertions) => ThemeChoice::Dark,
            _ => settings.theme,
        };
        let (mut dark_name, mut light_name) = (settings.dark_theme.clone(), settings.light_theme.clone());
        if let Some(name) = std::env::var("RGRIP_DEMO_PALETTE").ok().filter(|_| cfg!(debug_assertions)) {
            if theme::DARK_THEMES.iter().any(|t| t.name == name) {
                (dark_name, theme_choice) = (name, ThemeChoice::Dark);
            } else if theme::LIGHT_THEMES.iter().any(|t| t.name == name) {
                (light_name, theme_choice) = (name, ThemeChoice::Light);
            }
        }
        theme::apply(&cc.egui_ctx, theme_choice, &dark_name, &light_name);
        if let Some(commands) = config::load_commands() {
            rgrip_core::commands::set_active(commands);
        }
        let worker = Worker::spawn(cc.egui_ctx.clone());
        // Launch checks only in release builds (dev runs restart a lot), or
        // when a test API is configured.
        let check_now =
            settings.check_updates && (!cfg!(debug_assertions) || std::env::var("RGRIP_UPDATE_API").is_ok());
        if check_now {
            worker.send(Cmd::CheckUpdate);
        }
        Self {
            update: if check_now { UpdateState::Checking { manual: false } } else { UpdateState::Idle },
            ctx_close: false,
            was_focused: true,
            egui_ctx: cc.egui_ctx.clone(),
            settings,
            worker,
            conns: config::load_connections().into_iter().map(Conn::new).collect(),
            tabs: Vec::new(),
            active: 0,
            next_tab: 1,
            dialog: None,
            filter: String::new(),
            notice: config::take_keychain_warning(),
            saved: config::load_queries(),
            name_prompt: None,
            confirm: None,
            skip_confirm: false,
            toast: None,
            expanded: HashSet::new(),
            selected: None,
            explorer_active: false,
            type_ahead: (String::new(), Instant::now()),
            explorer_rows: Vec::new(),
            scroll_to_row: None,
            demo_settled: None,
            demo_play: None,
            demo_input: Vec::new(),
            commands_editor: None,
            import: None,
            table_editor: None,
        }
    }

    fn conn_index(&self, id: &str) -> Option<usize> {
        self.conns.iter().position(|c| c.cfg.id == id)
    }

    /// The catalog of a tab's connection; empty until it has loaded.
    fn tab_catalog(&self, id: TabId) -> Arc<Catalog> {
        let conn = self.tabs.iter().find(|t| t.id == id).map(|t| t.conn.clone());
        conn.and_then(|c| self.conn_index(&c)).map(|ci| self.conns[ci].catalog.clone()).unwrap_or_default()
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn save_config(&mut self) {
        let cfgs: Vec<_> = self.conns.iter().map(|c| c.cfg.clone()).collect();
        if let Err(e) = config::save_connections(&cfgs) {
            self.notice = Some(format!("Could not save connections: {e}"));
        } else if let Some(w) = config::take_keychain_warning() {
            self.notice = Some(w);
        }
    }

    // ---- worker events -------------------------------------------------

    fn handle_events(&mut self) {
        let events: Vec<Ev> = self.worker.poll().collect();
        for ev in events {
            match ev {
                Ev::Connected { conn, version } => {
                    let Some(ci) = self.conn_index(&conn) else { continue };
                    self.conns[ci].status = Status::Connected(version);
                    self.worker.send(Cmd::Catalog(conn.clone(), None));
                    self.flush_pending(&conn);
                    if !self.tabs.iter().any(|t| t.conn == conn) {
                        self.open_console(ci);
                    }
                }
                Ev::ConnectFailed { conn, error } => {
                    let Some(ci) = self.conn_index(&conn) else { continue };
                    self.conns[ci].status = Status::Failed(error.clone());
                    for tab in self.tabs.iter_mut().filter(|t| t.conn == conn) {
                        match &mut tab.kind {
                            TabKind::Console(c) if c.pending.is_some() => {
                                c.pending = None;
                                c.running = None;
                                c.output = Some(Err(error.clone()));
                            }
                            TabKind::Table(t) if t.pending => {
                                t.pending = false;
                                t.loading = false;
                                t.data = Some(Err(error.clone()));
                            }
                            TabKind::Diagram(d) if matches!(d.state, crate::diagram_tab::State::Pending) => {
                                d.state = crate::diagram_tab::State::Failed(error.clone());
                            }
                            _ => {}
                        }
                    }
                }
                Ev::TestResult(result) => {
                    if let Some(d) = &mut self.dialog {
                        d.testing = false;
                        d.test = Some(result);
                    }
                }
                Ev::Schemas { conn, database: None, result } => {
                    if let Ok(list) = &result {
                        if list.iter().any(|s| s == "public") {
                            self.expanded.insert(format!("s/{conn}/public"));
                        }
                    }
                    if let Some(ci) = self.conn_index(&conn) {
                        self.conns[ci].schemas = Some(result.into());
                    }
                }
                Ev::Schemas { conn, database: Some(db), result } => {
                    if let Err(e) = &result {
                        self.toast = Some((e.clone(), theme::ERROR(), Instant::now()));
                    }
                    if let Some(ci) = self.conn_index(&conn) {
                        self.conns[ci].dbs.entry(db).or_default().schemas = Some(result.into());
                    }
                }
                Ev::Databases { conn, result } => {
                    if let Some(ci) = self.conn_index(&conn) {
                        self.conns[ci].databases = Some(result.into());
                    }
                }
                Ev::Tables { conn, schema, result } => {
                    if let Some(ci) = self.conn_index(&conn) {
                        self.conns[ci].tables.insert(schema, result.into());
                    }
                }
                Ev::Columns { conn, schema, table, result } => {
                    if let Some(ci) = self.conn_index(&conn) {
                        self.conns[ci].columns.insert((schema, table), result.into());
                    }
                }
                Ev::Catalog { conn, database: Some(db), result } => {
                    if let (Some(ci), Ok(catalog)) = (self.conn_index(&conn), result) {
                        self.conns[ci].dbs.entry(db).or_default().catalog = Arc::new(catalog);
                    }
                }
                Ev::Catalog { conn, database: None, result } => {
                    if let (Some(ci), Ok(catalog)) = (self.conn_index(&conn), result) {
                        let catalog = Arc::new(catalog);
                        self.conns[ci].catalog = catalog.clone();
                        // Pages that arrived before the catalog only had real
                        // foreign keys: add the inferred relations now.
                        for tab in self.tabs.iter_mut().filter(|t| t.conn == conn) {
                            if let TabKind::Table(t) = &mut tab.kind {
                                t.relations = with_inferred(&t.schema, &t.table, &t.columns, &t.fk_relations, &catalog);
                                if t.key.is_empty() && !t.columns.is_empty() {
                                    t.key = edit_key(&t.schema, &t.table, &t.columns, &catalog);
                                }
                                if let Some(rel) = &mut t.related {
                                    rel.relations = with_inferred(&rel.target.schema, &rel.target.table, &rel.columns, &rel.fk_relations, &catalog);
                                }
                            }
                            if let TabKind::Console(c) = &mut tab.kind {
                                if let Some(rel) = &mut c.related {
                                    rel.relations = with_inferred(&rel.target.schema, &rel.target.table, &rel.columns, &rel.fk_relations, &catalog);
                                }
                                // Run on the connection's own database: its catalog.
                                if c.run_database.is_none()
                                    && let Some((origins, tables)) = &c.sources
                                    && let Some(Ok(out)) = &c.output
                                    && let Some(rs) = out.statements.iter().find_map(|s| s.result.as_ref())
                                {
                                    c.relations = result_relations(origins, tables, &rs.columns, &catalog);
                                    c.probe = None;
                                }
                            }
                        }
                    }
                }
                Ev::Diagram { conn, schema, result } => {
                    if let Some(Tab { kind: TabKind::Diagram(d), .. }) = self.tabs.iter_mut().find(|t| t.conn == conn && matches!(&t.kind, TabKind::Diagram(d) if d.schema == schema)) {
                        d.loaded(result);
                    }
                }
                Ev::LayerResult { tab, layer, seq, result } => {
                    if let Some(Tab { kind: TabKind::Layout(l), .. }) = self.tab_mut(tab) {
                        l.layer_result(layer, seq, result);
                    }
                }
                Ev::QueryDone { tab, result } => {
                    let mut refresh = None;
                    let mut origins = None;
                    let mut count = None;
                    if let Some(Tab { kind: TabKind::Console(c), conn, .. }) = self.tab_mut(tab) {
                        if let Ok(out) = &result {
                            let truncated = out.statements.iter().any(|s| s.result.as_ref().is_some_and(|r| r.truncated));
                            let args = (conn.clone(), tab, c.run, c.last_sql.clone(), c.run_schema.clone());
                            // The count covers the area filter too.
                            let counted = c.filtered_base().unwrap_or_else(|| c.last_sql.clone());
                            // First page of a single read-only statement on the connection's own database.
                            if c.paged_base.is_some() && c.offset == 0 && c.run_database.is_none() && !c.origins_asked {
                                c.origins_asked = true;
                                origins = Some(args.clone());
                            }
                            // More pages exist: count them in the background (once per run).
                            if c.paged_base.is_some() && truncated && !c.count_asked {
                                c.count_asked = true;
                                let (conn, tab, run, _, schema) = args.clone();
                                count = Some(((conn, tab, run, counted, schema), c.run_database.clone()));
                            }
                        }
                        if std::mem::take(&mut c.ddl) && result.is_ok() {
                            refresh = Some((conn.clone(), c.run_database.clone()));
                        }
                        c.running = None;
                        c.grid = GridState::fresh(&c.grid);
                        c.view = match &result {
                            Ok(out) => match (out.statements.iter().filter(|s| s.result.is_some()).count(), c.view) {
                                (0, _) => View::Output,
                                // On the map (an area filter, a page, a sort): stay there.
                                (n, v @ (View::Map(i) | View::Split(i))) if i < n => v,
                                (n, _) => View::Result(n - 1),
                            },
                            Err(_) => View::Output,
                        };
                        c.output = Some(result);
                    }
                    if let Some((conn, database)) = refresh {
                        self.worker.send(Cmd::Catalog(conn, database));
                    }
                    if let Some((conn, tab, run, sql, schema)) = origins {
                        self.worker.send(Cmd::QueryOrigins { conn, tab, run, sql, schema });
                    }
                    if let Some(((conn, tab, run, sql, schema), database)) = count {
                        self.worker.send(Cmd::CountRows { conn, tab, run, sql, schema, database });
                    }
                }
                Ev::QueryOrigins { tab, run, sql, origins, tables } => {
                    let catalog = self.tab_catalog(tab);
                    if let Some(Tab { kind: TabKind::Console(c), .. }) = self.tab_mut(tab)
                        && c.run == run
                        && c.running.is_none()
                        && c.last_sql == sql
                        && let Some(Ok(out)) = &c.output
                        && let Some(rs) = out.statements.iter().find_map(|s| s.result.as_ref())
                    {
                        c.relations = result_relations(&origins, &tables, &rs.columns, &catalog);
                        c.sources = Some((origins, tables));
                        c.probe = None;
                    }
                }
                Ev::QueryCount { tab, run, sql, result } => {
                    if let Some(Tab { kind: TabKind::Console(c), .. }) = self.tab_mut(tab)
                        && c.run == run
                        && c.filtered_base().unwrap_or_else(|| c.last_sql.clone()) == sql
                    {
                        c.total = result.ok();
                    }
                }
                Ev::TablePage { tab, modal: true, result, columns, relations } => {
                    let catalog = self.tab_catalog(tab);
                    if let Some(tab) = self.tab_mut(tab)
                        && let Some((related, _)) = tab.kind.related_slots()
                        && let Some(rel) = related
                    {
                        rel.relations = with_inferred(&rel.target.schema, &rel.target.table, &columns, &relations, &catalog);
                        (rel.columns, rel.fk_relations) = (columns, relations);
                        rel.grid = GridState::fresh(&rel.grid);
                        rel.data = Some(result);
                    }
                }
                Ev::TablePage { tab, modal: false, result, columns, relations } => {
                    let catalog = self.tab_catalog(tab);
                    if let Some(Tab { kind: TabKind::Table(t), .. }) = self.tab_mut(tab) {
                        t.relations = with_inferred(&t.schema, &t.table, &columns, &relations, &catalog);
                        t.fk_relations = relations;
                        t.loading = false;
                        t.grid = GridState::fresh(&t.grid);
                        t.data = Some(result);
                        t.key = edit_key(&t.schema, &t.table, &columns, &catalog);
                        t.temporal = columns
                            .iter()
                            .filter_map(|c| Some((c.name.clone(), rgrip_core::datetime::kind_of(&c.data_type)?)))
                            .collect();
                        t.columns = columns;
                        // Edits are keyed by row index: they belong to the old page.
                        t.changes.clear();
                        t.editor = None;
                    }
                }
                Ev::RelationProbe { tab, modal, key, counts } => {
                    if let Some(tab) = self.tab_mut(tab)
                        && let Some((related, probe)) = tab.kind.related_slots()
                    {
                        let probe = if modal { related.as_mut().and_then(|r| r.probe.as_mut()) } else { probe.as_mut() };
                        if let Some(p) = probe.filter(|p| p.rows == key) {
                            p.counts = counts;
                        }
                    }
                }
                Ev::EditsSaved { tab, result } => {
                    let Some(ti) = self.tabs.iter().position(|t| t.id == tab) else { continue };
                    if let TabKind::Table(t) = &mut self.tabs[ti].kind {
                        t.saving = false;
                        match result {
                            Ok(n) => {
                                t.changes.clear();
                                self.toast = Some((format!("Saved {}", plural(n, "row")), theme::OK(), Instant::now()));
                                self.tab_action(ti, TabAction::Reload);
                            }
                            Err(e) => self.toast = Some((e, theme::ERROR(), Instant::now())),
                        }
                    }
                }
                Ev::KeyAdded { tab, result } => {
                    let Some(ti) = self.tabs.iter().position(|t| t.id == tab) else { continue };
                    let conn = self.tabs[ti].conn.clone();
                    let TabKind::Table(t) = &mut self.tabs[ti].kind else { continue };
                    t.saving = false;
                    match result {
                        Ok(()) => {
                            self.toast = Some((format!("Added a primary key to {}.{}", t.schema, t.table), theme::OK(), Instant::now()));
                            // The explorer and the catalog learn the new column and key.
                            let key = (t.schema.clone(), t.table.clone());
                            if let Some(ci) = self.conn_index(&conn) {
                                self.conns[ci].columns.remove(&key);
                                self.worker.send(Cmd::Catalog(conn, None));
                            }
                            self.tab_action(ti, TabAction::Reload);
                        }
                        Err(e) => self.toast = Some((e, theme::ERROR(), Instant::now())),
                    }
                }
                Ev::TableDef { conn, schema, table, result } => {
                    if let Some(ed) = self.table_editor.as_mut().filter(|e| e.conn == conn && e.schema == schema && e.table == table) {
                        ed.loaded(result);
                    }
                }
                Ev::TableAltered { conn, schema, table, new_name, result } => {
                    match result {
                        Ok(()) => {
                            self.table_editor = None;
                            self.toast = Some((format!("Updated {schema}.{new_name}"), theme::OK(), Instant::now()));
                            if let Some(ci) = self.conn_index(&conn) {
                                self.conns[ci].columns.remove(&(schema.clone(), table.clone()));
                                self.conns[ci].columns.remove(&(schema.clone(), new_name.clone()));
                                self.worker.send(Cmd::Tables(conn.clone(), schema.clone()));
                                self.worker.send(Cmd::Catalog(conn.clone(), None));
                            }
                            // Open tabs of the table follow a rename and reload.
                            let open: Vec<usize> = (0..self.tabs.len())
                                .filter(|&i| self.tabs[i].conn == conn && matches!(&self.tabs[i].kind, TabKind::Table(t) if t.schema == schema && t.table == table))
                                .collect();
                            for i in open {
                                if let TabKind::Table(t) = &mut self.tabs[i].kind {
                                    t.table = new_name.clone();
                                    t.changes.clear();
                                    t.editor = None;
                                }
                                self.tab_action(i, TabAction::Reload);
                            }
                        }
                        Err(e) => {
                            if let Some(ed) = self.table_editor.as_mut() {
                                ed.failed(e);
                            }
                        }
                    }
                }
                Ev::ImportProgress { conn, rows } => {
                    if let Some(d) = self.import.as_mut().filter(|d| d.conn == conn) {
                        d.progress(rows);
                    }
                }
                Ev::ImportDone { conn, schema, table, result } => {
                    if let Ok(n) = &result {
                        self.toast = Some((format!("Imported {} into {schema}.{table}", plural(*n, "row")), theme::OK(), Instant::now()));
                        // The table list and the autocomplete catalog know the new table.
                        if let Some(ci) = self.conn_index(&conn) {
                            self.conns[ci].columns.remove(&(schema.clone(), table));
                            self.worker.send(Cmd::Tables(conn.clone(), schema));
                            self.worker.send(Cmd::Catalog(conn.clone(), None));
                        }
                    }
                    if let Some(d) = self.import.as_mut().filter(|d| d.conn == conn) {
                        d.finished(result);
                    }
                }
                Ev::Exported(result) => {
                    self.toast = Some(match result {
                        Ok((path, rows)) => (
                            format!("Exported {} to {}", plural(rows, "row"), path.display()),
                            theme::OK(),
                            Instant::now(),
                        ),
                        Err(e) => (format!("Export failed: {e}"), theme::ERROR(), Instant::now()),
                    });
                }
                Ev::UpdateChecked(result) => {
                    let manual = matches!(self.update, UpdateState::Checking { manual: true });
                    self.update = match result {
                        Ok(Some(release)) => UpdateState::Available(release),
                        Ok(None) => {
                            if manual {
                                self.toast = Some((format!("rgrip {} is up to date", update::CURRENT), theme::OK(), Instant::now()));
                            }
                            UpdateState::Idle
                        }
                        Err(e) => {
                            if manual {
                                self.toast = Some((e, theme::ERROR(), Instant::now()));
                            }
                            UpdateState::Idle
                        }
                    };
                }
                Ev::UpdateInstalled(result) => match result {
                    Ok(app) => match update::relaunch(&app) {
                        Ok(()) => self.ctx_close = true,
                        Err(e) => self.toast = Some((e, theme::ERROR(), Instant::now())),
                    },
                    Err(e) => {
                        if let UpdateState::Installing(r) = std::mem::replace(&mut self.update, UpdateState::Idle) {
                            self.update = UpdateState::Available(r);
                        }
                        self.toast = Some((e, theme::ERROR(), Instant::now()));
                    }
                },
                Ev::TableCount { tab, modal, result } => {
                    match self.tab_mut(tab).map(|t| &mut t.kind) {
                        Some(TabKind::Table(t)) if !modal => t.total = result.ok(),
                        Some(kind) if modal => {
                            if let Some((Some(rel), _)) = kind.related_slots() {
                                rel.total = result.ok();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Send work that was queued while the connection was coming up.
    fn flush_pending(&mut self, conn: &str) {
        for tab in self.tabs.iter_mut().filter(|t| t.conn == conn) {
            match &mut tab.kind {
                TabKind::Console(c) => {
                    if let Some(sql) = c.pending.take() {
                        let page = c.paged.is_some().then_some(c.offset);
                        self.worker.send(Cmd::Query { conn: tab.conn.clone(), tab: tab.id, sql, page, schema: c.run_schema.clone(), database: c.run_database.clone() });
                    }
                }
                TabKind::Table(t) if t.pending => {
                    t.pending = false;
                    self.worker.send(Cmd::TableData {
                        conn: tab.conn.clone(),
                        tab: tab.id,
                        query: t.query(),
                        modal: false,
                    });
                }
                TabKind::Diagram(d) if matches!(d.state, crate::diagram_tab::State::Pending) => {
                    d.state = crate::diagram_tab::State::Loading;
                    self.worker.send(Cmd::Diagram { conn: tab.conn.clone(), schema: d.schema.clone() });
                }
                _ => {}
            }
        }
    }

    // ---- actions ------------------------------------------------------

    /// Returns true when already connected; otherwise starts connecting.
    fn ensure_connected(&mut self, ci: usize) -> bool {
        match self.conns[ci].status {
            Status::Connected(_) => true,
            Status::Connecting => false,
            Status::Disconnected | Status::Failed(_) => {
                self.apply(Action::Connect(ci));
                false
            }
        }
    }

    fn open_console(&mut self, ci: usize) {
        let conn = self.conns[ci].cfg.id.clone();
        let n = self
            .tabs
            .iter()
            .filter(|t| t.conn == conn && matches!(t.kind, TabKind::Console(_)))
            .count();
        let cfg = &self.conns[ci].cfg;
        let name = conn_db_title(cfg, &cfg.database);
        let title = if n == 0 { name } else { format!("{name} ({})", n + 1) };
        self.push_tab(
            conn,
            title,
            TabKind::Console(Console::new()),
        );
        self.ensure_connected(ci);
    }

    fn open_table(&mut self, ci: usize, schema: String, table: String) {
        self.open_table_with(ci, schema, table, None);
    }

    /// Open (or switch to) a table tab showing only the rows matching `filter`.
    fn open_table_filtered(&mut self, ci: usize, schema: String, table: String, filter: String) {
        self.open_table_with(ci, schema, table, Some(filter));
    }

    fn open_table_with(&mut self, ci: usize, schema: String, table: String, filter: Option<String>) {
        let conn = self.conns[ci].cfg.id.clone();
        if let Some(i) = self.tabs.iter().position(|t| {
            t.conn == conn && matches!(&t.kind, TabKind::Table(v) if v.schema == schema && v.table == table)
        }) {
            self.active = i;
            if let (Some(filter), TabKind::Table(t)) = (filter, &mut self.tabs[i].kind) {
                if !t.changes.is_empty() {
                    self.toast = Some(("Save or discard your changes in this tab first".into(), theme::WARN(), Instant::now()));
                    return;
                }
                t.filter = filter.clone();
                t.applied_filter = filter;
                t.page = 0;
                self.tab_action(i, TabAction::Reload);
            }
            return;
        }
        let filter = filter.unwrap_or_default();
        let title = if schema == "public" { table.clone() } else { format!("{schema}.{table}") };
        self.push_tab(
            conn,
            title,
            TabKind::Table(TableView::new(schema, table, filter)),
        );
        self.tab_action(self.active, TabAction::Reload);
    }

    /// Switch to the connection's map layout tab (the last one), or open a
    /// new one when there's none or `new` is set. Returns its index.
    fn open_layout(&mut self, ci: usize, new: bool) -> usize {
        let conn = self.conns[ci].cfg.id.clone();
        if !new
            && let Some(i) = self.tabs.iter().rposition(|t| t.conn == conn && matches!(t.kind, TabKind::Layout(_)))
        {
            self.active = i;
            return i;
        }
        let n = self.tabs.iter().filter(|t| t.conn == conn && matches!(t.kind, TabKind::Layout(_))).count();
        let title = if n == 0 { "Map layout".to_string() } else { format!("Map layout ({})", n + 1) };
        self.push_tab(conn, title, TabKind::Layout(Box::new(crate::layout_tab::LayoutTab::new(&self.egui_ctx))));
        self.ensure_connected(ci);
        self.tabs.len() - 1
    }

    /// Switch to the diagram of `schema`, or open it and ask for its data.
    fn open_diagram(&mut self, ci: usize, schema: String) {
        let conn = self.conns[ci].cfg.id.clone();
        if let Some(i) = self.tabs.iter().position(|t| t.conn == conn && matches!(&t.kind, TabKind::Diagram(d) if d.schema == schema)) {
            self.active = i;
            return;
        }
        let cfg = &self.conns[ci].cfg;
        let key = format!("{}/{}/{schema}", cfg.id, cfg.database);
        let mut tab = crate::diagram_tab::DiagramTab::new(&schema, key);
        if self.ensure_connected(ci) {
            tab.state = crate::diagram_tab::State::Loading;
            self.worker.send(Cmd::Diagram { conn: conn.clone(), schema: schema.clone() });
        }
        self.push_tab(conn, format!("Diagram · {schema}"), TabKind::Diagram(Box::new(tab)));
    }

    fn push_tab(&mut self, conn: ConnId, title: String, kind: TabKind) {
        self.tabs.push(Tab { id: self.next_tab, conn, title, kind });
        self.next_tab += 1;
        self.active = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, i: usize) {
        close_tabs(&mut self.tabs, &mut self.active, CloseWhich::One(i));
    }

    fn save_settings(&mut self) {
        if let Err(e) = config::save_settings(&self.settings) {
            self.notice = Some(format!("Could not save settings: {e}"));
        }
    }

    /// Re-read the queries folder (files may be edited outside the app).
    fn reload_queries(&mut self) {
        self.saved = config::load_queries();
    }

    fn report(&mut self, result: Result<(), String>) {
        if let Err(e) = result {
            self.toast = Some((e, theme::ERROR(), Instant::now()));
        }
    }

    /// Point consoles linked to saved query `old` at `new` (None = unlink).
    fn relink(&mut self, old: &str, new: Option<(&str, &str)>) {
        for tab in &mut self.tabs {
            if let TabKind::Console(c) = &mut tab.kind {
                if c.saved_id.as_deref() == Some(old) {
                    c.saved_id = new.map(|(id, _)| id.to_string());
                    if let Some((_, name)) = new {
                        tab.title = name.to_string();
                    }
                }
            }
        }
    }

    fn open_saved(&mut self, i: usize) {
        let Some(q) = self.saved.get(i).cloned() else { return };
        let linked = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Console(c) if c.saved_id.as_ref() == Some(&q.id)));
        if let Some(t) = linked {
            self.active = t;
            return;
        }
        let current = self.tabs.get(self.active).map(|t| t.conn.clone());
        let ci = q
            .connection
            .as_deref()
            .and_then(|id| self.conn_index(id))
            .or_else(|| current.and_then(|id| self.conn_index(&id)))
            .unwrap_or(0);
        if self.conns.is_empty() {
            return;
        }
        self.open_console(ci);
        if let Some(tab) = self.tabs.last_mut() {
            tab.title = q.name.clone();
            if let TabKind::Console(c) = &mut tab.kind {
                c.sql = q.sql.clone();
                c.saved_id = Some(q.id.clone());
                c.database = q.database.clone();
                let mut schemas = q.schemas.clone();
                c.schema = (!schemas.is_empty()).then(|| schemas.remove(0));
                c.more_schemas = schemas;
            }
        }
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::NewConnection => {
                self.dialog = Some(Dialog::new(ConnectionConfig::default(), true));
            }
            Action::Connect(ci) => {
                let c = &mut self.conns[ci];
                c.status = Status::Connecting;
                c.clear_cache();
                self.worker.send(Cmd::Connect(c.cfg.clone()));
            }
            Action::Disconnect(ci) => {
                let c = &mut self.conns[ci];
                self.worker.send(Cmd::Disconnect(c.cfg.id.clone()));
                c.status = Status::Disconnected;
                c.clear_cache();
            }
            Action::SetColor(ci, color) => {
                self.conns[ci].cfg.color = color;
                self.save_config();
            }
            Action::Edit(ci) => {
                self.dialog = Some(Dialog::new(self.conns[ci].cfg.clone(), false));
            }
            Action::Delete(ci) => {
                let id = self.conns[ci].cfg.id.clone();
                self.worker.send(Cmd::Disconnect(id.clone()));
                while let Some(i) = self.tabs.iter().position(|t| t.conn == id) {
                    self.close_tab(i);
                }
                self.conns.remove(ci);
                self.save_config();
            }
            Action::Refresh(ci) => {
                self.conns[ci].clear_cache();
                self.worker.send(Cmd::Catalog(self.conns[ci].cfg.id.clone(), None));
            }
            Action::NewConsole(ci) => self.open_console(ci),
            Action::OpenDiagram(ci, schema) => self.open_diagram(ci, schema),
            Action::OpenTable(ci, s, t) => self.open_table(ci, s, t),
            Action::OpenInConsole(ci, schema, table) => {
                self.open_console(ci);
                if let Some(TabKind::Console(c)) = self.tabs.last_mut().map(|t| &mut t.kind) {
                    c.sql = format!("SELECT *\nFROM {}.{}\nLIMIT 100;", sql::quote_ident(&schema), sql::quote_ident(&table));
                    c.schema = Some(schema);
                }
            }
            Action::Import(ci, schema, table) => {
                if !matches!(self.conns[ci].tables.get(&schema), Some(Load::Ready(_) | Load::Loading)) {
                    self.apply(Action::LoadTables(ci, schema.clone()));
                }
                self.import = Some(crate::import::ImportDialog::new(self.conns[ci].cfg.id.clone(), schema, table));
            }
            Action::EditTable(ci, schema, table) => {
                let cfg = &self.conns[ci].cfg;
                if cfg.read_only {
                    self.toast = Some((READ_ONLY_HINT.into(), theme::WARN(), Instant::now()));
                    return;
                }
                let id = cfg.id.clone();
                self.table_editor = Some(crate::table_edit::TableEditor::new(id.clone(), schema.clone(), table.clone(), cfg.color == ConnColor::Red));
                self.worker.send(Cmd::TableDef { conn: id, schema, table });
            }
            Action::NewLayout(ci) => {
                self.open_layout(ci, true);
            }
            Action::AddToLayout(ci, schema, table, column) => {
                let ti = self.open_layout(ci, false);
                if let TabKind::Layout(l) = &mut self.tabs[ti].kind {
                    let loads = l.add_layer(&schema, &table, &column);
                    self.tab_action(ti, TabAction::LoadLayers(loads));
                }
            }
            Action::LoadSchemas(ci) => {
                let c = &mut self.conns[ci];
                c.schemas = Some(Load::Loading);
                self.worker.send(Cmd::Schemas(c.cfg.id.clone(), None));
            }
            Action::LoadTables(ci, schema) => {
                let c = &mut self.conns[ci];
                c.tables.insert(schema.clone(), Load::Loading);
                self.worker.send(Cmd::Tables(c.cfg.id.clone(), schema));
            }
            Action::LoadColumns(ci, schema, table) => {
                let c = &mut self.conns[ci];
                c.columns.insert((schema.clone(), table.clone()), Load::Loading);
                self.worker.send(Cmd::Columns(c.cfg.id.clone(), schema, table));
            }
            Action::Toggle(key) => {
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
            }
            Action::Select(key) => {
                self.selected = Some(key);
                self.explorer_active = true;
            }
            Action::ActivateTab(i) => self.active = i,
            Action::Close(which) => close_tabs(&mut self.tabs, &mut self.active, which),
            Action::OpenSaved(i) => self.open_saved(i),
            Action::SetTheme(choice) => {
                self.settings.theme = choice;
                theme::set_choice(&self.egui_ctx, choice);
                self.save_settings();
            }
            Action::SetPalette(dark, name) => {
                if dark {
                    self.settings.dark_theme = name.into();
                } else {
                    self.settings.light_theme = name.into();
                }
                theme::set_themes(&self.egui_ctx, &self.settings.dark_theme, &self.settings.light_theme);
                // Show the chosen theme right away.
                let wanted = if dark { ThemeChoice::Dark } else { ThemeChoice::Light };
                if self.settings.theme != ThemeChoice::System || theme::is_dark() != dark {
                    self.settings.theme = wanted;
                    theme::set_choice(&self.egui_ctx, wanted);
                }
                self.save_settings();
            }
            Action::SetAutoUpdate(on) => {
                self.settings.check_updates = on;
                self.save_settings();
            }
            Action::EditCommands => {
                self.commands_editor = Some(CommandsEditor { list: rgrip_core::commands::active().to_vec(), selected: 0, error: None, preview_args: String::new() });
            }
            Action::NewCommand(name) => {
                let mut list = rgrip_core::commands::active().to_vec();
                let name = if name.is_empty() { format!("cmd{}", list.len() + 1) } else { name.to_ascii_lowercase() };
                list.push(rgrip_core::commands::SlashCommand { name, help: String::new(), template: "select *\nfrom {table}\nwhere id in {ids};".into() });
                let selected = list.len() - 1;
                self.commands_editor = Some(CommandsEditor { list, selected, error: None, preview_args: String::new() });
            }
            Action::CheckUpdate => {
                if !matches!(self.update, UpdateState::Checking { .. } | UpdateState::Installing(_)) {
                    self.update = UpdateState::Checking { manual: true };
                    self.worker.send(Cmd::CheckUpdate);
                }
            }
            Action::InstallUpdate => {
                if let UpdateState::Available(release) = &self.update {
                    match update::current_bundle() {
                        Some(app) => {
                            let release = release.clone();
                            self.worker.send(Cmd::InstallUpdate { release: release.clone(), app });
                            self.update = UpdateState::Installing(release);
                        }
                        // Running from source: send the user to the release page.
                        None => self.egui_ctx.open_url(egui::OpenUrl::new_tab(&release.page_url)),
                    }
                }
            }
            Action::RenameSaved(i) => {
                if let Some(q) = self.saved.get(i) {
                    self.name_prompt = Some(NamePrompt { name: q.name.clone(), target: NameTarget::Rename(q.id.clone()) });
                }
            }
            Action::DeleteSaved(i) => {
                if let Some(q) = self.saved.get(i).cloned() {
                    let result = config::delete_query(&q);
                    if result.is_ok() {
                        self.relink(&q.id, None);
                        self.toast = Some((format!("Moved “{}” to the Trash", q.name), theme::MUTED(), Instant::now()));
                    }
                    self.report(result);
                    self.reload_queries();
                }
            }
            Action::RevealSaved(i) => {
                if let Some(path) = self.saved.get(i).map(|q| q.path.clone()) {
                    let result = config::reveal_in_finder(&path);
                    self.report(result);
                }
            }
            Action::OpenQueriesFolder => {
                if let Some(dir) = config::queries_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    self.egui_ctx.open_url(egui::OpenUrl::same_tab(format!("file://{}", dir.display())));
                }
            }
        }
    }

    fn tab_action(&mut self, ti: usize, action: TabAction) {
        let Some(ci) = self.conn_index(&self.tabs[ti].conn) else { return };
        if let TabAction::OpenTable { schema, table } = action {
            return self.open_table(ci, schema, table);
        }
        if let TabAction::ReloadDiagram = action {
            let conn = self.tabs[ti].conn.clone();
            let TabKind::Diagram(d) = &mut self.tabs[ti].kind else { return };
            let schema = d.schema.clone();
            if matches!(self.conns[ci].status, Status::Connected(_)) {
                d.state = crate::diagram_tab::State::Loading;
                self.worker.send(Cmd::Diagram { conn, schema });
            } else {
                d.state = crate::diagram_tab::State::Pending;
                self.ensure_connected(ci);
            }
            return;
        }
        if let TabAction::OpenFiltered { schema, table, filter } = action {
            return self.open_table_filtered(ci, schema, table, filter);
        }
        if let TabAction::LoadLayers(loads) = action {
            let (conn, tab) = (self.tabs[ti].conn.clone(), self.tabs[ti].id);
            self.ensure_connected(ci);
            for a in loads {
                self.worker.send(Cmd::LayerQuery { conn: conn.clone(), tab, layer: a.layer, seq: a.seq, kind: a.kind, sql: a.sql });
            }
            return;
        }
        let connected = matches!(self.conns[ci].status, Status::Connected(_));
        let mut needs_connect = false;
        let database = match &self.tabs[ti].kind {
            TabKind::Console(c) => c.database.clone(),
            TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => None,
        };
        let catalog = self.conns[ci].catalog_in(database.as_deref());
        let schemas = user_schemas(&self.conns[ci], database.as_deref());
        let tab = &mut self.tabs[ti];
        match (action, &mut tab.kind) {
            (TabAction::Run { all }, TabKind::Console(c)) => {
                let confirmed = std::mem::take(&mut self.skip_confirm);
                if c.running.is_some() || self.confirm.is_some() {
                    return;
                }
                let at = sql::char_to_byte(&c.sql, c.cursor.start);
                let text = if all {
                    c.sql.clone()
                } else if c.cursor.start != c.cursor.end {
                    let b = sql::char_to_byte(&c.sql, c.cursor.end);
                    c.sql[at..b].to_string()
                } else {
                    run_range(&c.sql, at).map(|r| c.sql[r].to_string()).unwrap_or_default()
                };
                // Slash commands are ours, not SQL: expand before sending.
                let text = match rgrip_core::commands::expand_all(&text, &catalog, c.schema_or_default(&schemas)) {
                    Ok(t) => t,
                    Err(e) => {
                        self.toast = Some((e, theme::WARN(), Instant::now()));
                        return;
                    }
                };
                if text.trim().is_empty() {
                    if !all && !c.sql.trim().is_empty() {
                        self.toast = Some(("Put the cursor on the statement to run".into(), theme::WARN(), Instant::now()));
                    }
                    return;
                }
                let cfg = &self.conns[ci].cfg;
                if !confirmed {
                    let items = risky_items(&text, cfg.color == ConnColor::Red);
                    if !items.is_empty() {
                        self.confirm = Some(Confirm { tab: tab.id, kind: ConfirmKind::Run { all }, title: format!("Run on {}?", cfg.display_name()), items, focused: false });
                        return;
                    }
                }
                c.running = Some(Instant::now());
                c.popup = None;
                let lower = text.to_lowercase();
                c.ddl = ["create", "alter", "drop", "truncate", "comment"]
                    .iter()
                    .any(|k| lower.split(|ch: char| !ch.is_alphanumeric()).any(|w| w == *k));
                c.offset = 0;
                c.last_sql = text.clone();
                c.run_schema = c.schema_for_run(&schemas);
                c.run_database = c.database.clone();
                c.paged = sql::paged_query(&text, 1, 0).map(|_| text.clone());
                c.paged_base = c.paged.clone();
                c.area.clear();
                c.order = None;
                c.unsorted = None;
                (c.relations, c.sources, c.probe, c.related, c.total) = (Vec::new(), None, None, None, None);
                (c.count_asked, c.origins_asked) = (false, false);
                c.run += 1;
                let page = c.paged.is_some().then_some(0);
                if connected {
                    self.worker.send(Cmd::Query { conn: tab.conn.clone(), tab: tab.id, sql: text, page, schema: c.run_schema.clone(), database: c.run_database.clone() });
                } else {
                    c.pending = Some(text);
                    needs_connect = true;
                }
            }
            (TabAction::SortResult(n, col), TabKind::Console(c)) => {
                if c.running.is_some() {
                    return;
                }
                let base = c.filtered_base();
                let Some(Ok(out)) = &mut c.output else { return };
                let Some(rs) = out.statements.iter_mut().filter_map(|s| s.result.as_mut()).nth(n) else { return };
                let Some(name) = rs.columns.get(col).cloned() else { return };
                // Cycle: ascending → descending → as returned.
                c.order = match c.order.take() {
                    Some((m, c2, true)) if m == n && c2 == name => Some((n, name.clone(), false)),
                    Some((m, c2, false)) if m == n && c2 == name => None,
                    _ => Some((n, name.clone(), true)),
                };
                match (base, connected) {
                    // Paged: sort on the server, so every page is in order.
                    (Some(base), true) => {
                        c.paged = c.sorted(base);
                        c.offset = 0;
                        c.running = Some(Instant::now());
                        let sql = c.paged.clone().unwrap_or_default();
                        self.worker.send(Cmd::Query { conn: tab.conn.clone(), tab: tab.id, sql, page: Some(0), schema: c.run_schema.clone(), database: c.run_database.clone() });
                    }
                    (Some(_), false) => {}
                    // Everything is loaded: sort in memory (keeping the original order to go back to).
                    (None, _) => {
                        if c.unsorted.as_ref().is_none_or(|(m, _)| *m != n) {
                            c.unsorted = Some((n, rs.rows.clone()));
                        }
                        match &c.order {
                            Some((_, _, asc)) => rs.sort_by_column(col, *asc),
                            None => {
                                if let Some((_, rows)) = c.unsorted.take() {
                                    rs.rows = rows;
                                }
                            }
                        }
                        c.grid = GridState::fresh(&c.grid);
                    }
                }
            }
            (TabAction::ResultArea(area), TabKind::Console(c)) => {
                if c.running.is_some() || !connected {
                    return;
                }
                c.area = area.unwrap_or_default();
                let Some(base) = c.filtered_base() else { return };
                c.paged = c.sorted(base);
                c.offset = 0;
                (c.total, c.count_asked) = (None, false);
                c.running = Some(Instant::now());
                let sql = c.paged.clone().unwrap_or_default();
                self.worker.send(Cmd::Query { conn: tab.conn.clone(), tab: tab.id, sql, page: Some(0), schema: c.run_schema.clone(), database: c.run_database.clone() });
            }
            (TabAction::Page(forward), TabKind::Console(c)) => {
                let Some(sql) = c.paged.clone().filter(|_| c.running.is_none() && connected) else { return };
                let size = PAGE_SIZE as usize;
                c.offset = if forward { c.offset + size } else { c.offset.saturating_sub(size) };
                c.running = Some(Instant::now());
                self.worker.send(Cmd::Query { conn: tab.conn.clone(), tab: tab.id, sql, page: Some(c.offset), schema: c.run_schema.clone(), database: c.run_database.clone() });
            }
            (TabAction::Save, TabKind::Console(c)) => {
                let existing = c.saved_id.as_ref().and_then(|id| self.saved.iter_mut().find(|q| &q.id == id));
                match existing {
                    Some(q) => {
                        q.sql = c.sql.clone();
                        q.connection = Some(tab.conn.clone());
                        (q.database, q.schemas) = (c.database.clone(), c.picked_schemas());
                        let (name, result) = (q.name.clone(), config::update_query(q));
                        if result.is_ok() {
                            self.toast = Some((format!("Saved “{name}”"), theme::OK(), Instant::now()));
                        }
                        self.report(result);
                    }
                    None => {
                        let name = default_query_name(&c.sql, self.saved.len() + 1);
                        self.name_prompt = Some(NamePrompt { name, target: NameTarget::NewFrom(tab.id) });
                    }
                }
            }
            (TabAction::ExportAll(format, path), kind) => {
                let (database, schema, sql, table) = match kind {
                    TabKind::Console(c) => match &c.paged {
                        Some(sql) => (c.run_database.clone(), c.run_schema.clone(), sql.clone(), "query_result".to_string()),
                        None => return,
                    },
                    TabKind::Table(t) => (
                        None,
                        None,
                        rgrip_core::postgres::table_select_sql(&t.query()),
                        format!("{}.{}", t.schema, t.table),
                    ),
                    TabKind::Layout(_) | TabKind::Diagram(_) => return,
                };
                self.toast = Some((format!("Exporting to {}…", path.display()), theme::MUTED(), Instant::now()));
                self.worker.send(Cmd::ExportAll { conn: tab.conn.clone(), database, schema, sql, format, table, path });
            }
            (TabAction::Reproject(col), TabKind::Console(c)) => {
                let Some(query) = c.paged.clone() else { return };
                let alias = format!("{col}_4326");
                c.sql = reproject_sql(&query, &col, &alias);
                c.map.prefer = Some(alias);
                c.view = View::Map(0);
                c.cursor = 0..0;
                return self.tab_action(ti, TabAction::Run { all: true });
            }
            (TabAction::LoadSchemas, _) => {
                let c = &mut self.conns[ci];
                let id = c.cfg.id.clone();
                if c.databases.is_none() {
                    c.databases = Some(Load::Loading);
                    self.worker.send(Cmd::Databases(id.clone()));
                }
                match c.other_db(database.as_deref()).map(str::to_string) {
                    None if c.schemas.is_none() => {
                        c.schemas = Some(Load::Loading);
                        self.worker.send(Cmd::Schemas(id, None));
                    }
                    Some(db) if c.schemas_in(Some(&db)).is_none() => {
                        c.dbs.entry(db.clone()).or_default().schemas = Some(Load::Loading);
                        self.worker.send(Cmd::Schemas(id.clone(), Some(db.clone())));
                        self.worker.send(Cmd::Catalog(id, Some(db)));
                    }
                    _ => {}
                }
            }
            (TabAction::Notify(text), _) => self.toast = Some((text, theme::WARN(), Instant::now())),
            (TabAction::NewCommand(name), _) => self.apply(Action::NewCommand(name)),
            (TabAction::Cancel, TabKind::Console(c)) => {
                if c.pending.take().is_some() {
                    c.running = None;
                } else if c.running.is_some() {
                    self.worker.send(Cmd::Cancel(tab.conn.clone(), c.run_database.clone()));
                }
            }
            (TabAction::SaveEdits, TabKind::Table(t)) => {
                let confirmed = std::mem::take(&mut self.skip_confirm);
                if self.conns[ci].cfg.read_only {
                    self.toast = Some((READ_ONLY_HINT.into(), theme::WARN(), Instant::now()));
                    return;
                }
                let Some(Ok(rs)) = &t.data else { return };
                if t.changes.is_empty() || t.saving || !connected {
                    return;
                }
                match edit_statements(t, rs) {
                    Ok(statements) => {
                        let cfg = &self.conns[ci].cfg;
                        if cfg.color == ConnColor::Red && !confirmed {
                            let items = statements.iter().map(|s| (s.clone(), "Updates a row on a production connection".to_string(), false)).collect();
                            self.confirm = Some(Confirm { tab: tab.id, kind: ConfirmKind::Save, title: format!("Save {} to {}?", plural(t.changes.len(), "change"), cfg.display_name()), items, focused: false });
                            return;
                        }
                        t.saving = true;
                        t.editor = None;
                        self.worker.send(Cmd::SaveEdits { conn: tab.conn.clone(), tab: tab.id, statements });
                    }
                    Err(e) => self.toast = Some((e, theme::ERROR(), Instant::now())),
                }
            }
            (TabAction::AddKey, TabKind::Table(t)) => {
                let confirmed = std::mem::take(&mut self.skip_confirm);
                if self.conns[ci].cfg.read_only {
                    self.toast = Some((READ_ONLY_HINT.into(), theme::WARN(), Instant::now()));
                    return;
                }
                if t.saving || !connected || !t.changes.is_empty() {
                    return;
                }
                let name = ["id", "gid", "fid", "row_id"]
                    .into_iter()
                    .find(|n| !t.columns.iter().any(|c| c.name == *n))
                    .unwrap_or("rgrip_id");
                let sql = format!(
                    "alter table {}.{} add column {} bigint generated always as identity primary key",
                    rgrip_core::sql::quote_ident(&t.schema),
                    rgrip_core::sql::quote_ident(&t.table),
                    rgrip_core::sql::quote_ident(name)
                );
                // Changes the table for everyone: always show the statement.
                if !confirmed {
                    let title = format!("Add a primary key to {}.{}?", t.schema, t.table);
                    let why = format!("Adds column {name}, numbered 1, 2, 3… in the current row order; rewrites the table");
                    self.confirm = Some(Confirm { tab: tab.id, kind: ConfirmKind::AddKey, title, items: vec![(sql, why, false)], focused: false });
                    return;
                }
                t.saving = true;
                self.worker.send(Cmd::AddKey { conn: tab.conn.clone(), tab: tab.id, sql });
            }
            (TabAction::ShowRelated(target), kind) => {
                let Some((related, _)) = kind.related_slots() else { return };
                match related {
                    Some(rel) => {
                        let prev = std::mem::replace(&mut rel.target, target);
                        rel.back.push(prev);
                        rel.data = None;
                        rel.total = None;
                    }
                    None => {
                        *related = Some(Related {
                            target,
                            back: Vec::new(),
                            data: None,
                            total: None,
                            relations: Vec::new(),
                            columns: Vec::new(),
                            fk_relations: Vec::new(),
                            grid: GridState::default(),
                            probe: None,
                        })
                    }
                }
                let query = related.as_ref().unwrap().query();
                if connected {
                    self.worker.send(Cmd::TableData { conn: tab.conn.clone(), tab: tab.id, query, modal: true });
                }
            }
            (TabAction::Probe { modal, rows, probes, map }, kind) => {
                let Some((related, slot)) = kind.related_slots() else { return };
                if !connected || probes.is_empty() {
                    return;
                }
                let probe = Probe { rows: rows.clone(), map, counts: Vec::new() };
                match (modal, related) {
                    (true, Some(rel)) => rel.probe = Some(probe),
                    (true, None) => return,
                    (false, _) => *slot = Some(probe),
                }
                self.worker.send(Cmd::ProbeRelations { conn: tab.conn.clone(), tab: tab.id, modal, key: rows, probes });
            }
            (TabAction::RelatedBack, kind) => {
                let Some((Some(rel), _)) = kind.related_slots() else { return };
                let Some(prev) = rel.back.pop() else { return };
                rel.target = prev;
                rel.data = None;
                rel.total = None;
                let query = rel.query();
                if connected {
                    self.worker.send(Cmd::TableData { conn: tab.conn.clone(), tab: tab.id, query, modal: true });
                }
            }
            (TabAction::Reload, TabKind::Table(t)) => {
                t.loading = true;
                t.total = None;
                if connected {
                    self.worker.send(Cmd::TableData {
                        conn: tab.conn.clone(),
                        tab: tab.id,
                        query: t.query(),
                        modal: false,
                    });
                } else {
                    t.pending = true;
                    needs_connect = true;
                }
            }
            _ => {}
        }
        if needs_connect {
            self.ensure_connected(ci);
        }
    }
}

impl App {
    /// Debug builds only: `RGRIP_DEMO_SQL` opens a console on the first
    /// connection and runs it; `RGRIP_DEMO_TABLE=schema.table` opens a table.
    /// Used to exercise and screenshot the UI without clicking.
    #[cfg(debug_assertions)]
    /// Per-frame screenshot scene setup (debug builds), applied once the
    /// active tab has data:
    /// - `RGRIP_DEMO_EXPAND=demo,public`: explorer schemas to expand
    /// - `RGRIP_DEMO_FREEZE=a,b`: frozen columns
    /// - `RGRIP_DEMO_SELECT=0,2`: selected rows
    /// - `RGRIP_DEMO_TABLE_VIEW=map` (+ `RGRIP_DEMO_FEATURE=<row>`): map view
    /// - `RGRIP_DEMO_EDITS='0:phone=+855 12 345 678;1:verified=t'`: pending edits
    /// - `RGRIP_DEMO_EDITOR=2:registered_on`: open cell editor (picker)
    /// - `RGRIP_DEMO_RELATED=parcels`: related-rows popup for the selection
    /// - `RGRIP_DEMO_COPY_IMAGE=<path>`: write "Copy as image" PNG of the selection
    /// - `RGRIP_DEMO_SCROLL_X=320`: scroll the grid sideways (once)
    /// - `RGRIP_DEMO_ONLY=1`: show just the demo connection, no saved queries
    /// - `RGRIP_DEMO_READY=<path>`: touch when settled (for the capture script)
    fn demo_scene(&mut self) {
        let env = |k: &str| std::env::var(k).ok();
        if env("RGRIP_DEMO_ONLY").is_some() {
            if let Some(name) = env("RGRIP_DEMO_CONN") {
                self.conns.retain(|c| c.cfg.display_name() == name);
            }
            self.saved.clear();
        }
        let scroll_x = env("RGRIP_DEMO_SCROLL_X").and_then(|v| v.parse::<f32>().ok());
        // `RGRIP_DEMO_COLOR=red`: tint the demo connection (not saved).
        if let (Some(color), Some(c)) = (env("RGRIP_DEMO_COLOR"), self.conns.first_mut()) {
            use rgrip_core::ConnColor::*;
            c.cfg.color = match color.as_str() {
                "red" => Red,
                "orange" => Orange,
                "yellow" => Yellow,
                "green" => Green,
                "blue" => Blue,
                "purple" => Purple,
                "gray" => Gray,
                _ => None,
            };
        }
        // `RGRIP_DEMO_MORE=demo.owners,demo.parcels`: extra table tabs, then
        // back to the first tab; `RGRIP_DEMO_SCHEMA=demo` for consoles.
        static MORE_OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(more) = env("RGRIP_DEMO_MORE")
            && !self.tabs.is_empty()
            && !MORE_OPENED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            for t in more.split(',') {
                if let Some((schema, table)) = t.split_once('.') {
                    self.open_table(0, schema.into(), table.into());
                }
            }
            self.active = 0;
        }
        if let Some(schema) = env("RGRIP_DEMO_SCHEMA") {
            let mut list: Vec<String> = schema.split(',').map(String::from).collect();
            let first = list.remove(0);
            for tab in &mut self.tabs {
                if let TabKind::Console(c) = &mut tab.kind {
                    c.schema = Some(first.clone());
                    c.more_schemas = list.clone();
                }
            }
        }
        // `RGRIP_DEMO_FILTER=order`: explorer search.
        if let Some(f) = env("RGRIP_DEMO_FILTER") {
            self.filter = f;
        }
        if let Some(db) = env("RGRIP_DEMO_DATABASE") {
            for tab in &mut self.tabs {
                if let TabKind::Console(c) = &mut tab.kind {
                    c.database = Some(db.clone());
                }
            }
        }
        static ONCE: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
        let once = |what: &'static str| {
            let mut done = ONCE.lock().unwrap();
            (!done.contains(&what)).then(|| done.push(what)).is_some()
        };
        // `RGRIP_DEMO_COMMANDS=<preview args>`: the slash command editor, once.
        static COMMANDS_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(args) = env("RGRIP_DEMO_COMMANDS") && !COMMANDS_SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed) {
            self.apply(Action::EditCommands);
            if let Some(ed) = &mut self.commands_editor {
                ed.selected = 2;
                ed.preview_args = args;
            }
        }
        if let (Some(list), Some(c)) = (env("RGRIP_DEMO_EXPAND"), self.conns.first()) {
            let id = c.cfg.id.clone();
            self.expanded.insert(format!("c/{id}"));
            for s in list.split(',') {
                self.expanded.insert(format!("s/{id}/{s}"));
            }
        }
        let mut follow_action = None;
        let mut sort_reload = false;
        let settled = match self.tabs.get_mut(self.active).map(|t| &mut t.kind) {
            None => true,
            Some(TabKind::Console(c)) => {
                if let (Some(cols), true) = (env("RGRIP_DEMO_FREEZE"), c.grid.frozen.is_empty()) {
                    c.grid.frozen = cols.split(',').map(String::from).collect();
                }
                (c.running.is_none() && (c.output.is_some() || c.sql.trim().is_empty())) || c.popup.is_some()
            }
            Some(TabKind::Table(t)) => match &t.data {
                Some(Ok(rs)) if !t.loading => {
                    let col = |name: &str| rs.columns.iter().position(|c| c == name);
                    let cell = |spec: &str| -> Option<(usize, usize)> {
                        let (r, c) = spec.split_once(':')?;
                        Some((r.parse().ok()?, col(c)?))
                    };
                    if let (Some(cols), true) = (env("RGRIP_DEMO_FREEZE"), t.grid.frozen.is_empty()) {
                        t.grid.frozen = cols.split(',').map(String::from).collect();
                        t.grid.scroll_x = scroll_x;
                    }
                    if let (Some(sel), true) = (env("RGRIP_DEMO_SELECT"), t.grid.selection.is_empty()) {
                        t.grid.selection = sel.split(',').filter_map(|v| v.parse().ok()).collect();
                    }
                    // `RGRIP_DEMO_SORT=price_usd:desc`, `RGRIP_DEMO_GOTO=pri`,
                    // `RGRIP_DEMO_ROW_MENU=1`.
                    if let Some(sort) = env("RGRIP_DEMO_SORT")
                        && once("sort")
                    {
                        let (col, dir) = sort.split_once(':').unwrap_or((&sort, "asc"));
                        t.order = Some((col.to_string(), dir != "desc"));
                        sort_reload = true;
                    }
                    if let Some(q) = env("RGRIP_DEMO_GOTO")
                        && !sort_reload
                        && once("goto")
                    {
                        t.grid.open_goto(&q);
                    }
                    if let Some(row) = env("RGRIP_DEMO_ROW_MENU").and_then(|v| v.parse().ok())
                        && once("row-menu")
                    {
                        if !t.grid.selection.contains(&row) {
                            t.grid.selection = [row].into();
                        }
                        t.grid.demo_menu = Some(row);
                    }
                    if let Some(v @ ("map" | "split")) = env("RGRIP_DEMO_TABLE_VIEW").as_deref() {
                        t.pane = if v == "split" { Pane::Split } else { Pane::Map };
                        t.map.selected = env("RGRIP_DEMO_FEATURE").and_then(|v| v.parse().ok());
                    }
                    if let (Some(edits), true) = (env("RGRIP_DEMO_EDITS"), t.changes.is_empty()) {
                        for e in edits.split(';') {
                            if let Some((at, v)) = e.split_once('=').and_then(|(spec, v)| Some((cell(spec)?, v))) {
                                t.changes.insert(at, Some(v.to_string()));
                            }
                        }
                    }
                    if let (Some(spec), true) = (env("RGRIP_DEMO_EDITOR"), t.editor.is_none()) {
                        if let Some((r, c)) = cell(&spec) {
                            t.editor = Some(grid::CellEditor::open(r, c, rs.rows[r][c].as_deref()));
                        }
                    }
                    if let Some(path) = env("RGRIP_DEMO_COPY_IMAGE").filter(|p| !std::path::Path::new(p).exists()) {
                        let rows: Vec<usize> = if t.grid.selection.is_empty() { (0..rs.rows.len().min(8)).collect() } else { t.grid.selection.iter().copied().collect() };
                        let sub = export::subset(rs, &rows);
                        if let Some(png) = crate::snapshot::table_png(&sub) {
                            let _ = std::fs::write(path, png);
                        }
                    }
                    match (env("RGRIP_DEMO_RELATED"), &t.related) {
                        (Some(other), None) => {
                            if let Some(r) = t.relations.iter().find(|r| r.other_table == other) {
                                let rows: Vec<usize> = if t.grid.selection.is_empty() { vec![0] } else { t.grid.selection.iter().copied().collect() };
                                follow_action = Some(follow(r, rs, &rows, &[]));
                            }
                            false
                        }
                        (Some(_), Some(rel)) => rel.data.is_some(),
                        _ => true,
                    }
                }
                _ => false,
            },
            Some(TabKind::Layout(_) | TabKind::Diagram(_)) => true,
        };
        // `RGRIP_DEMO_IMPORT=<file>`: the import dialog with that file picked,
        // into the first `RGRIP_DEMO_EXPAND` schema.
        static IMPORT_OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        // `RGRIP_DEMO_LAYOUT=demo.districts,demo.parcels`: a map layout with
        // those tables as layers (`RGRIP_DEMO_LAYOUT_TITLE` sets the title).
        static LAYOUT_OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if let Some(specs) = env("RGRIP_DEMO_LAYOUT") {
            if self.conns.first().is_some_and(|c| matches!(c.status, Status::Disconnected)) {
                self.apply(Action::Connect(0));
            }
            let ready = self.conns.first().is_some_and(|c| !c.catalog.tables.is_empty());
            if ready && !LAYOUT_OPENED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                for spec in specs.split(',').rev() {
                    let Some((schema, table)) = spec.split_once('.') else { continue };
                    let col = self.conns[0].catalog.tables.iter().find(|t| t.schema == schema && t.name == table).and_then(|t| {
                        t.columns.iter().find(|(_, ty)| ty.starts_with("geometry")).map(|(n, _)| n.clone())
                    });
                    if let Some(col) = col {
                        self.apply(Action::AddToLayout(0, schema.into(), table.into(), col));
                    }
                }
                if let (Some(title), Some(TabKind::Layout(l))) = (env("RGRIP_DEMO_LAYOUT_TITLE"), self.tabs.get_mut(self.active).map(|t| &mut t.kind)) {
                    l.layout.title = title;
                }
            }
            // `RGRIP_DEMO_LAYOUT_SCALE=10000`: zoom in once the layers have loaded.
            static SCALED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Some(scale) = env("RGRIP_DEMO_LAYOUT_SCALE").and_then(|v| v.parse::<f64>().ok())
                && let Some(TabKind::Layout(l)) = self.tabs.iter_mut().find(|t| matches!(t.kind, TabKind::Layout(_))).map(|t| &mut t.kind)
                && l.layout.layers.iter().all(|x| x.loaded.is_some())
                && !l.layout.layers.is_empty()
                && !SCALED.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                l.demo_zoom(scale);
            }
            // `RGRIP_DEMO_LAYOUT_COLOR_BY=land_use`: categories on the top layer.
            static COLORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Some(col) = env("RGRIP_DEMO_LAYOUT_COLOR_BY")
                && LAYOUT_OPENED.load(std::sync::atomic::Ordering::Relaxed)
                && !COLORED.swap(true, std::sync::atomic::Ordering::Relaxed)
                && let Some(ti) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Layout(_)))
                && let TabKind::Layout(l) = &mut self.tabs[ti].kind
            {
                let loads = l.demo_color_by(&col);
                self.tab_action(ti, TabAction::LoadLayers(loads));
            }
            // `RGRIP_DEMO_LAYOUT_EXPORT=/tmp/map.pdf|png`: export once loaded.
            static EXPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let (Some(path), Some(TabKind::Layout(l))) = (env("RGRIP_DEMO_LAYOUT_EXPORT"), self.tabs.get_mut(self.active).map(|t| &mut t.kind))
                && !EXPORTED.load(std::sync::atomic::Ordering::Relaxed)
                && l.demo_export(path.into())
            {
                EXPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        // `RGRIP_DEMO_EDIT_TABLE=demo.owners`: Edit table with a few edits.
        static EDIT_OPENED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        if let Some((schema, table)) = env("RGRIP_DEMO_EDIT_TABLE").as_deref().and_then(|v| v.split_once('.'))
            && matches!(self.conns.first().map(|c| &c.status), Some(Status::Connected(_)))
        {
            use std::sync::atomic::Ordering::Relaxed;
            if EDIT_OPENED.load(Relaxed) == 0 {
                EDIT_OPENED.store(1, Relaxed);
                self.apply(Action::EditTable(0, schema.into(), table.into()));
            } else if EDIT_OPENED.load(Relaxed) == 1 && self.table_editor.as_mut().is_some_and(|e| e.demo_edits()) {
                EDIT_OPENED.store(2, Relaxed);
            }
        }
        if env("RGRIP_DEMO_EDIT_TABLE").is_some() && self.conns.first().is_some_and(|c| matches!(c.status, Status::Disconnected)) {
            self.apply(Action::Connect(0));
        }
        // Connects, then waits for the schema's tables, as when opened from
        // the explorer.
        if env("RGRIP_DEMO_IMPORT").is_some() && self.conns.first().is_some_and(|c| matches!(c.status, Status::Disconnected)) {
            self.apply(Action::Connect(0));
        }
        let import_schema = env("RGRIP_DEMO_EXPAND").and_then(|v| v.split(',').next().map(String::from)).unwrap_or_else(|| "public".into());
        if let Some(file) = env("RGRIP_DEMO_IMPORT")
            && self.conns.first().is_some_and(|c| matches!(c.tables.get(&import_schema), Some(Load::Ready(_))))
            && !IMPORT_OPENED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            let schema = import_schema;
            self.apply(Action::Import(0, schema, None));
            if let Some(d) = &mut self.import {
                d.demo_open(&self.egui_ctx, file.into());
            }
        }
        // A pending confirmation or the import dialog is a scene of its own
        // (no results come).
        let settled = settled
            || self.confirm.is_some()
            || self.import.as_ref().is_some_and(|d| d.demo_ready())
            || EDIT_OPENED.load(std::sync::atomic::Ordering::Relaxed) == 2;
        if let Some(a) = follow_action {
            self.tab_action(self.active, a);
        }
        if sort_reload {
            self.tab_action(self.active, TabAction::Reload);
        }
        // Touch the ready file once the scene has looked settled for a moment.
        if let Some(path) = env("RGRIP_DEMO_READY") {
            if !settled {
                self.demo_settled = None;
            } else if self.demo_settled.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(2500) {
                let _ = std::fs::write(&path, "ready");
                // Recordings: play the script once the scene is ready.
                if self.demo_play.is_none()
                    && let Some(steps) = env("RGRIP_DEMO_STEPS")
                {
                    let steps = steps.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    self.demo_play = Some(DemoPlay { steps, index: 0, since: Instant::now(), typed: 0 });
                }
            }
            self.egui_ctx.request_repaint_after(Duration::from_millis(100));
        }
        while self.demo_step() {}
    }

    /// Advance the demo script by one step if it's ready to move on.
    /// Returns true to run the next step in the same frame.
    ///
    /// Steps: `wait:<ms>`, `type:<text>` (into the console, `\n` = newline),
    /// `keys:<text>` (keystrokes to the focused field), `key:[cmd+]Enter|Escape|/|A|Backspace`, `click:<x>,<y>`, `drag:<x1>,<y1>,<x2>,<y2>`,
    /// `run` (and wait for results), `view:map|grid`, `open:<schema.table>`
    /// (and wait for data), `select:<rows>`, `menu:<row>`,
    /// `related:<table>` (and wait), `editor:<row>:<col>`,
    /// `edit:<row>:<col>=<value>`, `freeze:<cols>`, `scrollx:<px>`,
    /// `sort:<col>:asc|desc` (and wait), `goto` (open the column search),
    /// `commands:<args>` (slash command editor preview), `done` (touch
    /// `RGRIP_DEMO_DONE`).
    #[cfg(debug_assertions)]
    fn demo_step(&mut self) -> bool {
        let Some(play) = &mut self.demo_play else { return false };
        let Some(step) = play.steps.get(play.index).cloned() else { return false };
        self.egui_ctx.request_repaint_after(Duration::from_millis(16));
        let elapsed = play.since.elapsed();
        let (verb, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        let advance = |app: &mut App| {
            if let Some(p) = &mut app.demo_play {
                p.index += 1;
                p.since = Instant::now();
                p.typed = 0;
            }
            true
        };
        let active = self.active;
        match verb {
            "wait" => {
                if elapsed.as_millis() >= arg.parse::<u128>().unwrap_or(500) {
                    return advance(self);
                }
            }
            "type" | "keys" => {
                // About 22 characters a second, like a quick typist.
                let text = arg.replace("\\n", "\n");
                let total = text.chars().count();
                let want = ((elapsed.as_millis() / 45) as usize + 1).min(total);
                let typed = play.typed;
                if want > typed {
                    let chunk: String = text.chars().skip(typed).take(want - typed).collect();
                    play.typed = want;
                    if verb == "keys" {
                        self.egui_ctx.input_mut(|i| i.events.push(egui::Event::Text(chunk)));
                    } else if let Some(TabKind::Console(c)) = self.tabs.get_mut(active).map(|t| &mut t.kind) {
                        c.sql.push_str(&chunk);
                        let n = c.sql.chars().count();
                        c.cursor = n..n;
                        let id = Id::new(("editor", self.tabs[active].id));
                        let mut state = TextEdit::load_state(&self.egui_ctx, id).unwrap_or_default();
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(n))));
                        state.store(&self.egui_ctx, id);
                    }
                }
                if want == total && elapsed.as_millis() > 45 * total as u128 + 150 {
                    return advance(self);
                }
            }
            "key" => {
                // `cmd+` in front for ⌘.
                let (modifiers, name) = arg.strip_prefix("cmd+").map_or((Modifiers::NONE, arg), |k| (Modifiers::COMMAND, k));
                let key = match name {
                    "Enter" => Key::Enter,
                    "Escape" => Key::Escape,
                    "Down" => Key::ArrowDown,
                    "/" => Key::Slash,
                    "A" => Key::A,
                    "Backspace" => Key::Backspace,
                    _ => Key::Tab,
                };
                self.egui_ctx.input_mut(|i| i.events.push(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }));
                return advance(self);
            }
            "click" => {
                // Press this frame, release the next.
                let (x, y) = arg.split_once(',').unwrap_or(("0", "0"));
                let pos = egui::pos2(x.trim().parse().unwrap_or(0.0), y.trim().parse().unwrap_or(0.0));
                let pressed = play.typed == 0;
                play.typed = 1;
                // Pointer state is read before `update`; these go in with the next frame's input.
                self.demo_input.push(egui::Event::PointerMoved(pos));
                self.demo_input.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
                self.egui_ctx.request_repaint();
                if !pressed {
                    return advance(self);
                }
            }
            "hover" => {
                // `hover:x,y`: move the pointer there (no click).
                let (x, y) = arg.split_once(',').unwrap_or(("0", "0"));
                let pos = egui::pos2(x.trim().parse().unwrap_or(0.0), y.trim().parse().unwrap_or(0.0));
                self.demo_input.push(egui::Event::PointerMoved(pos));
                self.egui_ctx.request_repaint();
                return advance(self);
            }
            "drag" => {
                // `drag:x1,y1,x2,y2`: press, move over ten frames, release.
                let v: Vec<f32> = arg.split(',').filter_map(|n| n.trim().parse().ok()).collect();
                if v.len() == 4 {
                    let step = play.typed;
                    play.typed += 1;
                    let t = (step.min(10) as f32) / 10.0;
                    let pos = egui::pos2(v[0] + (v[2] - v[0]) * t, v[1] + (v[3] - v[1]) * t);
                    self.demo_input.push(egui::Event::PointerMoved(pos));
                    if step == 0 || step == 11 {
                        self.demo_input.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: step == 0, modifiers: Modifiers::NONE });
                    }
                    self.egui_ctx.request_repaint();
                    if step < 11 {
                        return false;
                    }
                }
                return advance(self);
            }
            "run" => {
                let state = match self.tabs.get(active).map(|t| &t.kind) {
                    Some(TabKind::Console(c)) => (c.running.is_some(), c.output.is_some()),
                    _ => (false, true),
                };
                if play.typed == 0 {
                    play.typed = 1;
                    if let Some(TabKind::Console(c)) = self.tabs.get_mut(active).map(|t| &mut t.kind) {
                        c.output = None;
                    }
                    self.tab_action(active, TabAction::Run { all: true });
                } else if !state.0 && state.1 {
                    return advance(self);
                }
            }
            "view" => {
                match self.tabs.get_mut(active).map(|t| &mut t.kind) {
                    Some(TabKind::Console(c)) => {
                        c.view = match arg {
                            "map" => View::Map(0),
                            "split" => View::Split(0),
                            _ => View::Result(0),
                        }
                    }
                    Some(TabKind::Table(t)) => {
                        t.pane = match arg {
                            "map" => Pane::Map,
                            "split" => Pane::Split,
                            _ => Pane::Grid,
                        }
                    }
                    Some(TabKind::Layout(_) | TabKind::Diagram(_)) | None => {}
                }
                return advance(self);
            }
            "open" | "related" | "sort" => {
                if play.typed == 0 {
                    play.typed = 1;
                    match verb {
                        "open" => {
                            if let Some((schema, table)) = arg.split_once('.') {
                                self.open_table(0, schema.into(), table.into());
                            }
                        }
                        "sort" => {
                            if let Some(TabKind::Table(t)) = self.tabs.get_mut(active).map(|t| &mut t.kind) {
                                let (col, dir) = arg.split_once(':').unwrap_or((arg, "asc"));
                                t.order = Some((col.to_string(), dir != "desc"));
                            }
                            self.tab_action(active, TabAction::Reload);
                        }
                        _ => {
                            let a = match self.tabs.get(active).map(|t| &t.kind) {
                                Some(TabKind::Table(t)) => match (&t.data, t.relations.iter().find(|r| r.other_table == arg)) {
                                    (Some(Ok(rs)), Some(r)) => {
                                        let rows: Vec<usize> = if t.grid.selection.is_empty() { vec![0] } else { t.grid.selection.iter().copied().collect() };
                                        Some(follow(r, rs, &rows, &[]))
                                    }
                                    _ => None,
                                },
                                _ => None,
                            };
                            if let Some(a) = a {
                                self.tab_action(active, a);
                            }
                        }
                    }
                    return false;
                }
                let loaded = match self.tabs.get(self.active).map(|t| &t.kind) {
                    Some(TabKind::Table(t)) if verb == "related" => t.related.as_ref().is_some_and(|r| r.data.is_some()),
                    Some(TabKind::Table(t)) => t.data.is_some() && !t.loading,
                    _ => false,
                };
                if loaded {
                    return advance(self);
                }
            }
            _ => {
                if let Some(TabKind::Table(t)) = self.tabs.get_mut(active).map(|t| &mut t.kind)
                    && let Some(Ok(rs)) = &t.data
                {
                    let col = |name: &str| rs.columns.iter().position(|c| c == name);
                    let cell = |spec: &str| -> Option<(usize, usize)> {
                        let (r, c) = spec.split_once(':')?;
                        Some((r.parse().ok()?, col(c)?))
                    };
                    match verb {
                        "select" => t.grid.selection = arg.split(',').filter_map(|v| v.parse().ok()).collect(),
                        "menu" => {
                            if let Ok(row) = arg.parse() {
                                t.grid.demo_menu = Some(row);
                            }
                        }
                        "editor" => {
                            if let Some((r, c)) = cell(arg) {
                                t.editor = Some(grid::CellEditor::open(r, c, rs.rows[r][c].as_deref()));
                            }
                        }
                        "edit" => {
                            if let Some((at, v)) = arg.split_once('=').and_then(|(spec, v)| Some((cell(spec)?, v))) {
                                t.changes.insert(at, Some(v.to_string()));
                                t.editor = None;
                            }
                        }
                        "freeze" => t.grid.frozen = arg.split(',').map(String::from).collect(),
                        "scrollx" => t.grid.scroll_x = arg.parse().ok(),
                        "goto" => t.grid.open_goto(arg),
                        _ => {}
                    }
                }
                match verb {
                    "commands" => {
                        if self.commands_editor.is_none() {
                            self.apply(Action::EditCommands);
                        }
                        if let Some(ed) = &mut self.commands_editor {
                            ed.selected = 2;
                            let want = ((elapsed.as_millis() / 60) as usize + 1).min(arg.chars().count());
                            ed.preview_args = arg.chars().take(want).collect();
                            if want < arg.chars().count() {
                                return false;
                            }
                        }
                    }
                    "done" => {
                        if let Ok(path) = std::env::var("RGRIP_DEMO_DONE") {
                            let _ = std::fs::write(path, "done");
                        }
                    }
                    _ => {}
                }
                return advance(self);
            }
        }
        false
    }

    #[cfg(not(debug_assertions))]
    fn demo_step(&mut self) -> bool {
        false
    }

    #[cfg(debug_assertions)]
    fn demo_hook(&mut self) {
        self.demo_scene();
        if let Ok(name) = std::env::var("RGRIP_DEMO_CONN") {
            if let Some(i) = self.conns.iter().position(|c| c.cfg.display_name() == name).filter(|&i| i > 0) {
                self.conns.swap(0, i);
            }
        }
        // `RGRIP_DEMO_COMPLETE='select u.| from users u'`: `|` is the cursor.
        if let (Ok(text), Some(tab)) = (std::env::var("RGRIP_DEMO_COMPLETE"), self.tabs.first_mut()) {
            let catalog = self.conns[0].catalog.clone();
            let plain = text.replace('|', "");
            if let TabKind::Console(c) = &mut tab.kind {
                if c.popup.is_none() && !catalog.tables.is_empty() && (c.sql.is_empty() || c.sql == plain) {
                    let at = text.find('|').unwrap_or(text.len());
                    let chars = plain[..at].chars().count();
                    c.sql = plain;
                    c.cursor = chars..chars;
                    c.focus = true;
                    let id = Id::new(("editor", tab.id));
                    let ctx = self.egui_ctx.clone();
                    let mut state = TextEdit::load_state(&ctx, id).unwrap_or_default();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(chars))));
                    state.store(&ctx, id);
                    c.popup = complete::complete(&c.sql, at, &catalog, c.schema_or_default(&[]), false).map(Popup::new);
                }
            }
        }
        if std::env::var("RGRIP_DEMO_VIEW").as_deref() == Ok("map") {
            if let Some(TabKind::Console(c)) = self.tabs.first_mut().map(|t| &mut t.kind) {
                if matches!(c.output, Some(Ok(_))) && c.view == View::Result(0) {
                    c.view = View::Map(0);
                }
            }
        }
        if self.conns.is_empty() || self.next_tab != 1 {
            return;
        }
        if std::env::var("RGRIP_DEMO_COMPLETE").is_ok() {
            self.expanded.insert(format!("c/{}", self.conns[0].cfg.id));
            self.open_console(0);
        }
        if let Ok(sql) = std::env::var("RGRIP_DEMO_SQL") {
            self.expanded.insert(format!("c/{}", self.conns[0].cfg.id));
            self.open_console(0);
            if let Some(TabKind::Console(c)) = self.tabs.last_mut().map(|t| &mut t.kind) {
                c.sql = sql.replace("\\n", "\n");
                // `RGRIP_DEMO_SCHEMA=demo,legacy`: several schemas.
                let mut list: Vec<String> = std::env::var("RGRIP_DEMO_SCHEMA").ok().map(|v| v.split(',').map(String::from).collect()).unwrap_or_default();
                c.schema = (!list.is_empty()).then(|| list.remove(0));
                c.more_schemas = list;
                c.database = std::env::var("RGRIP_DEMO_DATABASE").ok();
            }
            self.tab_action(self.active, TabAction::Run { all: true });
        }
        if let Ok(tab) = std::env::var("RGRIP_DEMO_DIALOG") {
            self.apply(Action::Edit(0));
            if let Some(d) = &mut self.dialog {
                d.tab = match tab.as_str() {
                    "ssl" => DialogTab::Ssl,
                    "ssh" => DialogTab::Ssh,
                    _ => DialogTab::General,
                };
                d.cfg.ssh.enabled |= tab == "ssh";
            }
        }
        if let Ok(t) = std::env::var("RGRIP_DEMO_TABLE") {
            if let Some((schema, table)) = t.split_once('.') {
                self.open_table(0, schema.into(), table.into());
            }
        }
    }
}

impl eframe::App for App {
    #[cfg(debug_assertions)]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        raw_input.events.append(&mut self.demo_input);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::sync(ctx);
        // Pick up query files edited in another app when we come back.
        let focused = ctx.input(|i| i.focused);
        if focused && !self.was_focused {
            self.reload_queries();
        }
        self.was_focused = focused;
        #[cfg(debug_assertions)]
        self.demo_hook();
        self.handle_events();
        self.autosave();

        let mut actions = Vec::new();
        let mut tab_action = None;

        // ⌘N: new console on the current tab's connection (or the first one).
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::N)) && !self.conns.is_empty() {
            let ci = self.tabs.get(self.active).and_then(|t| self.conn_index(&t.conn)).unwrap_or(0);
            actions.push(Action::NewConsole(ci));
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W)) && !self.tabs.is_empty() {
            actions.push(Action::Close(CloseWhich::One(self.active)));
        }

        // Before the panels, or the editor's own ⌘↩ would take it.
        if self.confirm.is_some() && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
            self.accept_confirm();
        }

        self.title_bar(ctx, &mut actions);

        egui::TopBottomPanel::bottom("status")
            .exact_height(26.0)
            .frame(Frame::new().fill(theme::BG()).inner_margin(Margin::symmetric(12, 4)))
            .show(ctx, |ui| self.status_bar(ui, &mut actions));

        egui::SidePanel::left("explorer")
            .resizable(true)
            .default_width(270.0)
            .width_range(180.0..=560.0)
            .frame(Frame::new().fill(theme::BG()).inner_margin(Margin { left: 8, right: 8, top: 4, bottom: 8 }))
            .show(ctx, |ui| self.explorer(ui, &mut actions));

        egui::CentralPanel::default()
            .frame(Frame::new().fill(theme::SURFACE()))
            .show(ctx, |ui| {
                if self.tabs.is_empty() {
                    empty_state(ui, &mut actions, self.conns.is_empty());
                } else {
                    self.tab_bar(ui, &mut actions);
                    tab_action = self.tab_content(ui);
                }
            });

        self.dialog_ui(ctx);
        self.name_prompt_ui(ctx);
        self.confirm_ui(ctx);
        self.commands_editor_ui(ctx);
        self.import_ui(ctx);
        self.table_editor_ui(ctx);

        if let Some(a) = tab_action {
            self.tab_action(self.active, a);
        }
        for a in actions {
            self.apply(a);
        }
        if self.ctx_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

// ---- explorer ---------------------------------------------------------

impl App {
    fn explorer(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Explorer").size(12.0).color(theme::MUTED()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, "+").on_hover_text("New connection").clicked() {
                    actions.push(Action::NewConnection);
                }
            });
        });
        ui.add_space(6.0);
        ui.add(
            TextEdit::singleline(&mut self.filter)
                .hint_text(hint("Search tables"))
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(9, 6)),
        );
        ui.add_space(8.0);

        if self.conns.is_empty() {
            widgets::tree_note(ui, 0, "No connections yet.", theme::MUTED());
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                if ui.link("Add a connection").clicked() {
                    actions.push(Action::NewConnection);
                }
            });
            return;
        }

        self.explorer_type_ahead(ui);
        let filter = self.filter.trim().to_lowercase();
        let scroll_to = self.scroll_to_row.take();
        let tree = Tree {
            expanded: &self.expanded,
            selected: self.selected.as_deref(),
            filter: &filter,
            headers: Default::default(),
            rows: Default::default(),
            scroll_to: scroll_to.as_deref(),
        };
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        // Debug screenshots: `RGRIP_DEMO_EXPLORER_SCROLL=300` (kept, so the
        // tree has loaded by the time of the capture).
        if cfg!(debug_assertions) && let Some(y) = std::env::var("RGRIP_DEMO_EXPLORER_SCROLL").ok().and_then(|v| v.parse::<f32>().ok()) {
            scroll = scroll.vertical_scroll_offset(y);
        }
        let out = scroll.show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for (ci, conn) in self.conns.iter().enumerate() {
                tree.conn(ui, ci, conn, actions);
            }
            self.saved_section(ui, &filter, actions);
        });
        self.explorer_rows = tree.rows.take();
        // Sticky headers: the connection and schema you're scrolling through
        // stay pinned at the top (drawn last, so they sit on top).
        let headers = tree.headers.into_inner();
        let view = out.inner_rect;
        let row = widgets::ROW_HEIGHT + 1.0;
        let pinned = sticky_headers(&headers, view.top(), row);
        if !pinned.is_empty() {
            // Cover rows peeking above the band (the clip starts a bit higher).
            let band = egui::Rect::from_x_y_ranges(view.x_range(), (view.top() - 10.0)..=view.top());
            ui.painter().with_clip_rect(ui.clip_rect()).rect_filled(band, 0.0, theme::BG());
        }
        for (slot, &i) in pinned.iter().enumerate() {
            let h = &headers[i];
            let rect = egui::Rect::from_min_size(egui::pos2(view.left(), view.top() + slot as f32 * row), egui::vec2(view.width(), row));
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.painter().rect_filled(rect, 0.0, theme::BG());
                if slot + 1 == pinned.len() {
                    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, theme::BORDER()));
                }
                let resp = widgets::tree_row(
                    ui,
                    TreeRow { depth: h.depth, open: Some(true), icon: h.icon, label: &h.label, label_color: theme::TEXT(), detail: h.detail.as_deref(), selected: false },
                );
                if resp.on_hover_text("Click to collapse").clicked() {
                    actions.push(Action::Toggle(h.key.clone()));
                }
            });
        }
    }

    /// Finder-style type-to-select: while the explorer is active, typed
    /// characters select the first drawn row that matches and scroll to it.
    fn explorer_type_ahead(&mut self, ui: &Ui) {
        const IDLE: Duration = Duration::from_secs(1);
        let panel = ui.max_rect();
        let outside = ui.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !panel.contains(p)));
        if outside || ui.ctx().memory(|m| m.focused().is_some()) {
            self.explorer_active = false;
        }
        if !self.explorer_active {
            return;
        }
        let (typed, escape) = ui.input(|i| {
            let typed: String = if i.modifiers.command || i.modifiers.ctrl {
                String::new()
            } else {
                i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.as_str()) } else { None }).collect()
            };
            (typed, i.key_pressed(Key::Escape))
        });
        let (buffer, at) = &mut self.type_ahead;
        if escape || at.elapsed() > IDLE {
            buffer.clear();
        }
        if typed.is_empty() {
            return;
        }
        buffer.push_str(&typed);
        *at = Instant::now();
        if let Some(i) = type_ahead_match(&self.explorer_rows, buffer) {
            let key = self.explorer_rows[i].0.clone();
            self.selected = Some(key.clone());
            self.scroll_to_row = Some(key);
        }
    }

    /// Consoles that belong to a saved query save themselves shortly after
    /// the last edit (quietly; ⌘S still saves right away).
    fn autosave(&mut self) {
        const IDLE: Duration = Duration::from_millis(900);
        let mut errors = Vec::new();
        for tab in &self.tabs {
            let TabKind::Console(c) = &tab.kind else { continue };
            let Some(id) = &c.saved_id else { continue };
            // A new database or schema pick saves too, not just SQL edits.
            let picked = c.picked_schemas();
            let Some(q) = self.saved.iter_mut().find(|q| &q.id == id).filter(|q| q.sql != c.sql || q.database != c.database || q.schemas != picked) else { continue };
            if let Some(at) = c.last_edit
                && at.elapsed() < IDLE
            {
                self.egui_ctx.request_repaint_after(IDLE - at.elapsed());
                continue;
            }
            q.sql = c.sql.clone();
            q.connection = Some(tab.conn.clone());
            (q.database, q.schemas) = (c.database.clone(), picked);
            if let Err(e) = config::update_query(q) {
                errors.push(format!("Could not save “{}”: {e}", q.name));
            }
        }
        if let Some(e) = errors.pop() {
            self.toast = Some((e, theme::ERROR(), Instant::now()));
        }
    }

    /// Title strip for the full-size-content macOS window: drag/zoom area,
    /// plus appearance, updates, GitHub and sponsor buttons on the right.
    fn title_bar(&self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        const REPO: &str = "https://github.com/yoouTaro/rgrip";
        egui::TopBottomPanel::top("titlebar")
            .exact_height(30.0)
            .frame(Frame::new().fill(theme::BG()).inner_margin(Margin { left: 0, right: 10, top: 0, bottom: 0 }))
            .show(ctx, |ui| {
                let bar = ui.max_rect();
                ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0_f32, theme::BORDER()));
                // Registered first so the buttons below sit on top of it.
                let resp = ui.interact(bar, Id::new("titlebar-drag"), Sense::click_and_drag());
                if resp.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if resp.double_clicked() {
                    let max = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
                }
                // Painted, not a Label: a (selectable) label would take the
                // double-click and select its text instead of zooming the window.
                ui.painter().text(bar.center(), egui::Align2::CENTER_CENTER, "rgrip", FontId::proportional(12.0), theme::FAINT());

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let gear = title_button(ui, "Settings", |ui, r, hot| {
                        let tint = if hot { theme::TEXT() } else { theme::MUTED() };
                        crate::icons::paint(ui, crate::icons::Svg::Settings, r.shrink(5.0), tint);
                    });
                    self.settings_menu(&gear, actions);
                    if title_button(ui, "Sponsor rgrip", |ui, r, hot| {
                        let tint = if hot { Color32::from_rgb(0xea, 0x4a, 0xaa) } else { theme::MUTED() };
                        crate::icons::paint(ui, crate::icons::Svg::Heart, r.shrink(5.5), tint);
                    })
                    .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/sponsors/yoouTaro"));
                    }
                    if title_button(ui, "rgrip on GitHub", |ui, r, hot| {
                        let tint = if hot { theme::TEXT() } else { theme::MUTED() };
                        crate::icons::paint(ui, crate::icons::Svg::GitHub, r.shrink(5.0), tint);
                    })
                    .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab(REPO));
                    }
                    match &self.update {
                        UpdateState::Available(r) => {
                            let text = RichText::new(format!("Update to {}", r.version)).size(12.0).color(theme::BG());
                            let b = Button::new(text).fill(theme::ACCENT()).corner_radius(6);
                            if ui.add(b).on_hover_text("Download, verify and restart").clicked() {
                                actions.push(Action::InstallUpdate);
                            }
                        }
                        UpdateState::Checking { .. } | UpdateState::Installing(_) => {
                            ui.add_space(4.0);
                            ui.add(egui::Spinner::new().size(13.0));
                            ui.add_space(4.0);
                        }
                        UpdateState::Idle => {
                            let tip = format!("Check for updates (rgrip {})", update::CURRENT);
                            if title_button(ui, &tip, |ui, r, hot| {
                                let tint = if hot { theme::TEXT() } else { theme::MUTED() };
                                crate::icons::paint(ui, crate::icons::Svg::Refresh, r.shrink(6.0), tint);
                            })
                            .clicked()
                            {
                                actions.push(Action::CheckUpdate);
                            }
                        }
                    }
                    // Appearance: System → Light → Dark.
                    let (icon, label, next) = match self.settings.theme {
                        ThemeChoice::System => (Icon::System, "System", ThemeChoice::Light),
                        ThemeChoice::Light => (Icon::Sun, "Light", ThemeChoice::Dark),
                        ThemeChoice::Dark => (Icon::Moon, "Dark", ThemeChoice::System),
                    };
                    if title_button(ui, &format!("Appearance: {label} (click to switch)"), |ui, r, hot| {
                        let tint = if hot { theme::TEXT() } else { theme::MUTED() };
                        widgets::paint_icon_tinted(ui.painter(), r.center(), icon, Some(tint));
                    })
                    .clicked()
                    {
                        actions.push(Action::SetTheme(next));
                    }
                });
            });
    }

    /// The settings menu, opened from the title bar's gear button.
    fn settings_menu(&self, button: &egui::Response, actions: &mut Vec<Action>) {
        const WIDTH: f32 = 260.0;
        let mut popup = egui::Popup::menu(button);
        // Debug builds: RGRIP_DEMO_MENU=settings opens this menu (screenshots).
        if cfg!(debug_assertions) && std::env::var("RGRIP_DEMO_MENU").as_deref() == Ok("settings") {
            popup = popup.open(true);
        }
        popup.show(|ui| {
            ui.set_width(WIDTH);
            menu_heading(ui, "Appearance");
            if let Some(choice) = appearance_switch(ui, self.settings.theme, WIDTH) {
                actions.push(Action::SetTheme(choice));
            }
            ui.add_space(4.0);
            for (dark, label, themes, current) in [
                (true, "Dark theme", theme::DARK_THEMES, &self.settings.dark_theme),
                (false, "Light theme", theme::LIGHT_THEMES, &self.settings.light_theme),
            ] {
                ui.menu_button(format!("{label}:  {current}"), |ui| {
                    ui.set_width(220.0);
                    for t in themes {
                        if theme_row(ui, t, t.name == current.as_str(), 220.0).clicked() {
                            actions.push(Action::SetPalette(dark, t.name));
                        }
                    }
                });
            }
            ui.separator();
            menu_heading(ui, "Editor");
            if ui.button("Slash commands…").on_hover_text("Add or edit /commands and their SQL templates").clicked() {
                actions.push(Action::EditCommands);
                ui.close();
            }
            ui.separator();
            menu_heading(ui, "Updates");
            let busy = matches!(self.update, UpdateState::Checking { .. } | UpdateState::Installing(_));
            if ui.add_enabled(!busy, Button::new("Check for updates…")).clicked() {
                actions.push(Action::CheckUpdate);
                ui.close();
            }
            let mut auto = self.settings.check_updates;
            if ui.checkbox(&mut auto, "Check automatically").changed() {
                actions.push(Action::SetAutoUpdate(auto));
            }
            ui.separator();
            ui.label(RichText::new(format!("rgrip {}", update::CURRENT)).size(11.5).color(theme::FAINT()));
        });
    }

    fn saved_section(&self, ui: &mut Ui, filter: &str, actions: &mut Vec<Action>) {
        if self.saved.is_empty() {
            return;
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Saved queries").size(12.0).color(theme::MUTED()));
            ui.label(RichText::new(self.saved.len().to_string()).size(11.0).color(theme::FAINT()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, "↗").on_hover_text("Open queries folder in Finder").clicked() {
                    actions.push(Action::OpenQueriesFolder);
                }
            });
        });
        ui.add_space(4.0);
        let active_saved = self.tabs.get(self.active).and_then(|t| match &t.kind {
            TabKind::Console(c) => c.saved_id.clone(),
            TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => None,
        });
        for (i, q) in self.saved.iter().enumerate() {
            if !filter.is_empty() && !q.name.to_lowercase().contains(filter) {
                continue;
            }
            let conn = q.connection.as_deref().and_then(|id| self.conn_index(id)).map(|ci| self.conns[ci].cfg.display_name());
            let resp = widgets::tree_row(
                ui,
                TreeRow {
                    depth: 0,
                    open: None,
                    icon: Icon::Query,
                    label: &q.name,
                    label_color: theme::TEXT(),
                    detail: conn.as_deref(),
                    selected: active_saved.as_ref() == Some(&q.id),
                },
            );
            if resp.clicked() {
                actions.push(Action::OpenSaved(i));
            }
            let preview: String = q.sql.lines().take(12).collect::<Vec<_>>().join("\n");
            resp.on_hover_text(RichText::new(preview).monospace().size(11.5)).context_menu(|ui| {
                menu_layout(ui);
                if crate::icons::menu_item(ui, Svg::Terminal, "Open", true).clicked() {
                    actions.push(Action::OpenSaved(i));
                    ui.close();
                }
                if crate::icons::menu_item(ui, Svg::Pencil, "Rename…", true).clicked() {
                    actions.push(Action::RenameSaved(i));
                    ui.close();
                }
                if crate::icons::menu_item(ui, Svg::FolderOpen, "Reveal in Finder", true).clicked() {
                    actions.push(Action::RevealSaved(i));
                    ui.close();
                }
                ui.separator();
                if crate::icons::danger_item(ui, Svg::Trash, "Move to Trash").clicked() {
                    actions.push(Action::DeleteSaved(i));
                    ui.close();
                }
            });
        }
    }

    fn status_bar(&self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let toast = self.toast.as_ref().filter(|(_, _, at)| at.elapsed() < Duration::from_secs(8));
            if let Some((text, color, _)) = toast {
                ui.label(RichText::new(text).size(11.5).color(*color));
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            } else if let Some(n) = &self.notice {
                ui.label(RichText::new(n).size(11.5).color(theme::ERROR()));
            } else if let Some(c) = self.tabs.get(self.active).and_then(|t| self.conn_index(&t.conn)).map(|ci| &self.conns[ci]) {
                let (color, text) = status_style(&c.status);
                widgets::status_dot(ui, color);
                if let Some(col) = theme::conn_color(c.cfg.color) {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(3.0, 12.0), Sense::hover());
                    ui.painter().rect_filled(r, 1.5, col);
                }
                ui.label(
                    RichText::new(format!("{}   {}:{}/{}   {text}", c.cfg.display_name(), c.cfg.host, c.cfg.port, c.cfg.database))
                        .size(11.5)
                        .color(theme::MUTED()),
                );
                if c.cfg.read_only {
                    read_only_badge(ui);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                match &self.update {
                    UpdateState::Available(r) => {
                        let label = if update::current_bundle().is_some() { "Install & restart" } else { "Download" };
                        if ui
                            .add(Button::new(RichText::new(label).size(11.5).color(theme::BG())).fill(theme::ACCENT()).small())
                            .on_hover_text(format!("rgrip {} → {}", update::CURRENT, r.version))
                            .clicked()
                        {
                            actions.push(Action::InstallUpdate);
                        }
                        ui.label(RichText::new(format!("Update {} available", r.version)).size(11.5).color(theme::ACCENT()));
                        ui.add_space(12.0);
                    }
                    UpdateState::Installing(r) => {
                        ui.label(RichText::new(format!("Installing {}…", r.version)).size(11.5).color(theme::MUTED()));
                        ui.add(egui::Spinner::new().size(11.0));
                        ui.add_space(12.0);
                    }
                    UpdateState::Checking { manual: true } => {
                        ui.label(RichText::new("Checking for updates…").size(11.5).color(theme::MUTED()));
                        ui.add_space(12.0);
                    }
                    _ => {}
                }
                for (keys, what) in [("⌘W", "close tab"), ("⇧⌘↩", "run all"), ("⌘↩", "run")] {
                    ui.label(RichText::new(what).size(11.5).color(theme::FAINT()));
                    ui.label(RichText::new(keys).size(11.5).color(theme::MUTED()));
                    ui.add_space(8.0);
                }
            });
        });
    }
}

fn menu_heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(theme::FAINT()));
}

/// System / Light / Dark segmented control with icons, filling `width`.
fn appearance_switch(ui: &mut Ui, current: ThemeChoice, width: f32) -> Option<ThemeChoice> {
    let options = [
        (ThemeChoice::System, Icon::System, "System"),
        (ThemeChoice::Light, Icon::Sun, "Light"),
        (ThemeChoice::Dark, Icon::Moon, "Dark"),
    ];
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 30.0), Sense::hover());
    ui.painter().rect_filled(rect, 7.0, theme::HOVER());
    let seg_w = (width - 4.0) / options.len() as f32;
    let mut picked = None;
    for (i, (choice, icon, label)) in options.into_iter().enumerate() {
        let seg = egui::Rect::from_min_size(rect.min + egui::vec2(2.0 + i as f32 * seg_w, 2.0), egui::vec2(seg_w, 26.0));
        let resp = ui.interact(seg, Id::new(("appearance", i)), Sense::click());
        let selected = current == choice;
        if selected {
            ui.painter().rect_filled(seg, 5.0, theme::SURFACE());
            ui.painter().rect_stroke(seg, 5.0, Stroke::new(1.0_f32, theme::BORDER()), egui::StrokeKind::Inside);
        } else if resp.hovered() {
            ui.painter().rect_filled(seg, 5.0, theme::SELECTED().gamma_multiply(0.5));
        }
        let text_color = if selected { theme::TEXT() } else { theme::MUTED() };
        let galley = ui.painter().layout_no_wrap(label.into(), FontId::proportional(12.5), text_color);
        let total = 16.0 + 6.0 + galley.size().x;
        let left = seg.center().x - total / 2.0;
        let tint = (!selected).then(theme::MUTED);
        widgets::paint_icon_tinted(ui.painter(), egui::pos2(left + 8.0, seg.center().y), icon, tint);
        ui.painter().galley(egui::pos2(left + 22.0, seg.center().y - galley.size().y / 2.0), galley, text_color);
        if resp.clicked() && !selected {
            picked = Some(choice);
        }
    }
    picked
}

/// A theme entry: swatch (background, accent, text), name, check when active.
fn theme_row(ui: &mut Ui, t: &theme::Palette, selected: bool, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 26.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 5.0, theme::HOVER());
    }
    let swatch = egui::Rect::from_min_size(egui::pos2(rect.left() + 6.0, rect.center().y - 7.0), egui::vec2(26.0, 14.0));
    p.rect_filled(swatch, 4.0, t.bg);
    p.rect_stroke(swatch, 4.0, Stroke::new(1.0_f32, theme::BORDER()), egui::StrokeKind::Inside);
    p.circle_filled(swatch.center() - egui::vec2(4.5, 0.0), 3.5, t.accent);
    p.circle_filled(swatch.center() + egui::vec2(4.5, 0.0), 3.5, t.text);
    let color = if selected { theme::TEXT() } else { theme::MUTED() };
    p.text(egui::pos2(swatch.right() + 9.0, rect.center().y), egui::Align2::LEFT_CENTER, t.name, FontId::proportional(13.0), color);
    if selected {
        p.text(egui::pos2(rect.right() - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER, "✓", FontId::proportional(13.0), theme::ACCENT());
    }
    resp
}

fn status_style(s: &Status) -> (Color32, String) {
    match s {
        Status::Disconnected => (theme::FAINT(), "disconnected".into()),
        Status::Connecting => (theme::WARN(), "connecting…".into()),
        Status::Connected(v) => (theme::OK(), v.clone()),
        Status::Failed(_) => (theme::ERROR(), "connection failed".into()),
    }
}

/// Read-only view of explorer state; interactions are returned as actions.
struct Tree<'a> {
    expanded: &'a HashSet<String>,
    selected: Option<&'a str>,
    filter: &'a str,
    /// Open connection/schema rows as drawn, for the sticky headers.
    headers: std::cell::RefCell<Vec<StickyHeader>>,
    /// (key, label) of every row as drawn, for type-ahead.
    rows: std::cell::RefCell<Vec<(String, String)>>,
    /// Scroll this row into view (a type-ahead jump).
    scroll_to: Option<&'a str>,
}

/// The type-ahead target: the first row whose label starts with `query`,
/// else the first that contains it (ignoring case).
fn type_ahead_match(rows: &[(String, String)], query: &str) -> Option<usize> {
    let q = query.to_lowercase();
    let labels: Vec<String> = rows.iter().map(|(_, l)| l.to_lowercase()).collect();
    labels.iter().position(|l| l.starts_with(&q)).or_else(|| labels.iter().position(|l| l.contains(&q)))
}

/// An open connection (depth 0) or schema (depth 1) row, where it was drawn.
struct StickyHeader {
    depth: usize,
    key: String,
    icon: Icon,
    label: String,
    detail: Option<String>,
    top: f32,
}

/// Which headers to pin at the top of the explorer for a viewport starting
/// at `top`: the connection and schema whose rows scrolled out of view
/// while their contents are still showing.
fn sticky_headers(headers: &[StickyHeader], top: f32, row: f32) -> Vec<usize> {
    let mut pinned = Vec::new();
    for depth in 0..2 {
        let slot = top + pinned.len() as f32 * row;
        // Only look inside the pinned connection for its schemas.
        let (from, to) = match pinned.last() {
            Some(&p) => (p + 1, headers[p + 1..].iter().position(|h| h.depth == 0).map_or(headers.len(), |n| p + 1 + n)),
            None => (0, headers.len()),
        };
        let Some(i) = (from..to).rev().find(|&i| headers[i].depth == depth && headers[i].top < slot) else { break };
        // Its section ends at the next header of the same or a higher level.
        let end = headers[i + 1..].iter().find(|h| h.depth <= depth).map_or(f32::INFINITY, |h| h.top);
        if end <= slot + row {
            break;
        }
        pinned.push(i);
    }
    pinned
}

impl Tree<'_> {
    fn row(&self, ui: &mut Ui, key: &str, depth: usize, expandable: bool, icon: Icon, label: &str, detail: Option<&str>, actions: &mut Vec<Action>) -> (egui::Response, bool) {
        // While searching, connections and schemas show their matches open.
        let searching = !self.filter.is_empty() && (key.starts_with("c/") || key.starts_with("s/"));
        let open = searching || self.expanded.contains(key);
        let header_depth = if key.starts_with("c/") { Some(0) } else if key.starts_with("s/") { Some(1) } else { None };
        let resp = widgets::tree_row(
            ui,
            TreeRow {
                depth,
                open: expandable.then_some(open),
                icon,
                label,
                label_color: theme::TEXT(),
                detail,
                selected: self.selected == Some(key),
            },
        );
        if let Some(depth) = header_depth.filter(|_| open) {
            self.headers.borrow_mut().push(StickyHeader {
                depth,
                key: key.to_string(),
                icon,
                label: label.to_string(),
                detail: detail.map(str::to_string),
                top: resp.rect.top(),
            });
        }
        self.rows.borrow_mut().push((key.to_string(), label.to_string()));
        if self.scroll_to == Some(key) {
            resp.scroll_to_me(Some(Align::Center));
        }
        if resp.clicked() {
            actions.push(Action::Select(key.to_string()));
            if expandable {
                actions.push(Action::Toggle(key.to_string()));
            }
        }
        (resp, open)
    }

    fn conn(&self, ui: &mut Ui, ci: usize, conn: &Conn, actions: &mut Vec<Action>) {
        let key = format!("c/{}", conn.cfg.id);
        let (dot, _) = status_style(&conn.status);
        let detail = match conn.status {
            Status::Connecting => Some("connecting…"),
            _ if conn.cfg.read_only => Some("read-only"),
            _ => None,
        };
        let name = conn.cfg.display_name();
        let (resp, open) = self.row(ui, &key, 0, true, Icon::Database(dot, theme::conn_color(conn.cfg.color)), &name, detail, actions);
        if resp.clicked() && !open && matches!(conn.status, Status::Disconnected | Status::Failed(_)) {
            actions.push(Action::Connect(ci));
        }
        let resp = resp.on_hover_text(format!("{}@{}:{}/{}", conn.cfg.user, conn.cfg.host, conn.cfg.port, conn.cfg.database));
        demo_open_menu(ui, "RGRIP_DEMO_CONN_MENU", ci, &resp);
        resp.context_menu(|ui| conn_menu(ui, ci, conn, actions));
        if !open {
            return;
        }
        match &conn.status {
            Status::Disconnected | Status::Connecting => {}
            Status::Failed(e) => widgets::tree_note(ui, 1, e, theme::ERROR()),
            Status::Connected(_) => match &conn.schemas {
                None => actions.push(Action::LoadSchemas(ci)),
                Some(Load::Loading) => widgets::tree_note(ui, 1, "loading…", theme::FAINT()),
                Some(Load::Failed(e)) => widgets::tree_note(ui, 1, e, theme::ERROR()),
                Some(Load::Ready(list)) if !self.filter.is_empty() && !conn.catalog.tables.is_empty() => {
                    // Search every schema (not just expanded ones) via the catalog.
                    let mut any = false;
                    for schema in list {
                        let hits: Vec<TableInfo> = conn
                            .catalog
                            .tables
                            .iter()
                            .filter(|t| &t.schema == schema && t.name.to_lowercase().contains(self.filter))
                            .map(|t| TableInfo { name: t.name.clone(), kind: t.kind })
                            .collect();
                        if hits.is_empty() {
                            continue;
                        }
                        any = true;
                        let key = format!("s/{}/{schema}", conn.cfg.id);
                        self.row(ui, &key, 1, true, Icon::Schema, schema, Some(&hits.len().to_string()), actions);
                        for t in &hits {
                            self.table(ui, ci, conn, schema, t, actions);
                        }
                    }
                    if !any {
                        widgets::tree_note(ui, 1, "no matching tables", theme::FAINT());
                    }
                }
                Some(Load::Ready(list)) => {
                    for schema in list {
                        self.schema(ui, ci, conn, schema, actions);
                    }
                }
            },
        }
    }

    fn schema(&self, ui: &mut Ui, ci: usize, conn: &Conn, schema: &str, actions: &mut Vec<Action>) {
        let key = format!("s/{}/{schema}", conn.cfg.id);
        let count = match conn.tables.get(schema) {
            Some(Load::Ready(t)) => Some(t.len().to_string()),
            _ => None,
        };
        let (resp, open) = self.row(ui, &key, 1, true, Icon::Schema, schema, count.as_deref(), actions);
        resp.context_menu(|ui| {
            menu_layout(ui);
            if crate::icons::menu_item(ui, Svg::GitBranch, "Open diagram", true).clicked() {
                actions.push(Action::OpenDiagram(ci, schema.to_string()));
                ui.close();
            }
            if crate::icons::menu_item(ui, Svg::Upload, "Import data…", !conn.cfg.read_only).clicked() {
                actions.push(Action::Import(ci, schema.to_string(), None));
                ui.close();
            }
        });
        if !open {
            return;
        }
        match conn.tables.get(schema) {
            None => actions.push(Action::LoadTables(ci, schema.to_string())),
            Some(Load::Loading) => widgets::tree_note(ui, 2, "loading…", theme::FAINT()),
            Some(Load::Failed(e)) => widgets::tree_note(ui, 2, e, theme::ERROR()),
            Some(Load::Ready(tables)) => {
                let mut shown = 0;
                for t in tables.iter().filter(|t| self.filter.is_empty() || t.name.to_lowercase().contains(self.filter)) {
                    shown += 1;
                    self.table(ui, ci, conn, schema, t, actions);
                }
                if shown == 0 {
                    let msg = if tables.is_empty() { "no tables" } else { "no matches" };
                    widgets::tree_note(ui, 2, msg, theme::FAINT());
                }
            }
        }
    }

    fn table(&self, ui: &mut Ui, ci: usize, conn: &Conn, schema: &str, t: &TableInfo, actions: &mut Vec<Action>) {
        let key = format!("t/{}/{schema}/{}", conn.cfg.id, t.name);
        let icon = if matches!(t.kind, TableKind::Table | TableKind::Partitioned) { Icon::Table } else { Icon::View };
        let (resp, open) = self.row(ui, &key, 2, true, icon, &t.name, t.kind.tag(), actions);
        if resp.double_clicked() {
            actions.push(Action::OpenTable(ci, schema.to_string(), t.name.clone()));
        }
        resp.context_menu(|ui| {
            menu_layout(ui);
            if crate::icons::menu_item(ui, Svg::Table, "Open data", true).clicked() {
                actions.push(Action::OpenTable(ci, schema.to_string(), t.name.clone()));
                ui.close();
            }
            if crate::icons::menu_item(ui, Svg::Terminal, "Open in console", true).clicked() {
                actions.push(Action::OpenInConsole(ci, schema.to_string(), t.name.clone()));
                ui.close();
            }
            if matches!(t.kind, TableKind::Table | TableKind::Partitioned) && crate::icons::menu_item(ui, Svg::Upload, "Import into this table…", !conn.cfg.read_only).clicked() {
                actions.push(Action::Import(ci, schema.to_string(), Some(t.name.clone())));
                ui.close();
            }
            if matches!(t.kind, TableKind::Table | TableKind::Partitioned) && crate::icons::menu_item(ui, Svg::Pencil, "Edit table…", !conn.cfg.read_only).clicked() {
                actions.push(Action::EditTable(ci, schema.to_string(), t.name.clone()));
                ui.close();
            }
            // Tables with a geometry column can go on a map layout.
            let geom = conn.catalog.tables.iter().find(|c| c.schema == schema && c.name == t.name).and_then(|c| {
                c.columns.iter().find(|(_, ty)| ty.starts_with("geometry") || ty.starts_with("geography")).map(|(n, _)| n.clone())
            });
            if let Some(col) = geom
                && crate::icons::menu_item(ui, Svg::Map, "Add to map layout", true).clicked()
            {
                actions.push(Action::AddToLayout(ci, schema.to_string(), t.name.clone(), col));
                ui.close();
            }
            if crate::icons::menu_item(ui, Svg::Copy, "Copy name", true).clicked() {
                ui.ctx().copy_text(format!("{}.{}", sql::quote_ident(schema), sql::quote_ident(&t.name)));
                ui.close();
            }
        });
        if !open {
            return;
        }
        match conn.columns.get(&(schema.to_string(), t.name.clone())) {
            None => actions.push(Action::LoadColumns(ci, schema.to_string(), t.name.clone())),
            Some(Load::Loading) => widgets::tree_note(ui, 3, "loading…", theme::FAINT()),
            Some(Load::Failed(e)) => widgets::tree_note(ui, 3, e, theme::ERROR()),
            Some(Load::Ready(cols)) => {
                for c in cols {
                    let key = format!("{key}/{}", c.name);
                    let icon = if c.primary_key { Icon::Key } else { Icon::Column };
                    let mut ty = sql::short_type(&c.data_type);
                    if !c.nullable {
                        ty += " · not null";
                    }
                    let (resp, _) = self.row(ui, &key, 3, false, icon, &c.name, Some(&ty), actions);
                    if let Some(d) = &c.default {
                        resp.on_hover_text(format!("default {d}"));
                    }
                }
            }
        }
    }
}

/// Screenshot scenes (debug builds): `<var>=<index>` opens that item's
/// context menu once, as if right-clicked.
fn demo_open_menu(ui: &Ui, var: &'static str, index: usize, resp: &egui::Response) {
    if !cfg!(debug_assertions) || std::env::var(var).ok().and_then(|v| v.parse::<usize>().ok()) != Some(index) {
        return;
    }
    static OPENED: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut opened = OPENED.lock().unwrap();
    if !opened.contains(&var) {
        opened.push(var);
        let at = resp.rect.left_center() + egui::vec2(60.0, 6.0);
        #[allow(deprecated)] // the Popup API has no "open at" for context menus
        ui.ctx().memory_mut(|m| m.open_popup_at(egui::Popup::default_response_id(resp), at));
    }
}

/// Context menus: wide enough for their longest item, never wrapping
/// ("Copy connection URL" used to break over four lines).
pub(crate) fn menu_layout(ui: &mut Ui) {
    ui.set_min_width(190.0);
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
}

fn conn_menu(ui: &mut Ui, ci: usize, conn: &Conn, actions: &mut Vec<Action>) {
    menu_layout(ui);
    if crate::icons::menu_item(ui, Svg::Terminal, "New console", true).clicked() {
        actions.push(Action::NewConsole(ci));
        ui.close();
    }
    if crate::icons::menu_item(ui, Svg::Map, "New map layout", true).clicked() {
        actions.push(Action::NewLayout(ci));
        ui.close();
    }
    crate::icons::submenu(ui, Svg::Link, "Copy connection URL", |ui| {
        if ui.button("Without password").clicked() {
            ui.ctx().copy_text(rgrip_core::conn_string::to_url(&conn.cfg, false));
            ui.close();
        }
        if ui.button("With password").clicked() {
            ui.ctx().copy_text(rgrip_core::conn_string::to_url(&conn.cfg, true));
            ui.close();
        }
    });
    if matches!(conn.status, Status::Connected(_)) {
        if crate::icons::menu_item(ui, Svg::Refresh, "Refresh", true).clicked() {
            actions.push(Action::Refresh(ci));
            ui.close();
        }
        if crate::icons::menu_item(ui, Svg::Power, "Disconnect", true).clicked() {
            actions.push(Action::Disconnect(ci));
            ui.close();
        }
    } else if crate::icons::menu_item(ui, Svg::Power, "Connect", true).clicked() {
        actions.push(Action::Connect(ci));
        ui.close();
    }
    ui.separator();
    // Quicker than the dialog: tint this connection's tabs.
    crate::icons::submenu(ui, Svg::Droplet, "Color", |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for c in rgrip_core::ConnColor::ALL {
                let resp = widgets::color_swatch(ui, theme::conn_color(c), conn.cfg.color == c).on_hover_text(c.label());
                if resp.clicked() {
                    actions.push(Action::SetColor(ci, c));
                    ui.close();
                }
            }
        });
    })
    .on_hover_text("Tint this connection's tabs (e.g. red for production)");
    if crate::icons::menu_item(ui, Svg::Pencil, "Edit…", true).clicked() {
        actions.push(Action::Edit(ci));
        ui.close();
    }
    if crate::icons::danger_item(ui, Svg::Trash, "Delete").clicked() {
        actions.push(Action::Delete(ci));
        ui.close();
    }
}

// ---- tabs -------------------------------------------------------------

impl App {
    fn tab_bar(&self, ui: &mut Ui, actions: &mut Vec<Action>) {
        // DataGrip-style strip: square tabs edge to edge, hairline
        // separators, a border under the strip.
        Frame::new()
            .fill(theme::BG())
            .inner_margin(Margin::ZERO)
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("tabs")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for (i, tab) in self.tabs.iter().enumerate() {
                                let dirty = match &tab.kind {
                                    TabKind::Console(c) => c.saved_id.as_ref().is_some_and(|id| {
                                        self.saved.iter().find(|q| &q.id == id).is_some_and(|q| q.sql != c.sql)
                                    }),
                                    TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => false,
                                };
                                let style = match self.conn_index(&tab.conn).map(|ci| &self.conns[ci]) {
                                    Some(c) => TabStyle {
                                        title: match &tab.kind {
                                            TabKind::Console(k) => k.database.as_deref().and_then(|db| console_title(&tab.title, &c.cfg, db)),
                                            TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => None,
                                        },
                                        color: theme::conn_color(c.cfg.color),
                                        tip: format!("{} · {}:{}/{}", c.cfg.display_name(), c.cfg.host, c.cfg.port, c.cfg.database),
                                        status: status_style(&c.status).0,
                                    },
                                    None => TabStyle::default(),
                                };
                                tab_button(ui, i, tab, &style, i == self.active, dirty, self.tabs.len(), actions);
                            }
                        });
                    });
            });
        let rect = ui.available_rect_before_wrap();
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0_f32, theme::BORDER()));
    }

    fn tab_content(&mut self, ui: &mut Ui) -> Option<TabAction> {
        let read_only = self.tabs.get(self.active).and_then(|t| self.conn_index(&t.conn)).is_some_and(|ci| self.conns[ci].cfg.read_only);
        let (catalog, schemas, databases, default_db, load_schemas) = {
            let tab = self.tabs.get(self.active)?;
            let database = match &tab.kind {
                TabKind::Console(c) => c.database.as_deref(),
                TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => None,
            };
            match self.conn_index(&tab.conn).map(|ci| &self.conns[ci]) {
                Some(c) => {
                    let databases = match &c.databases {
                        Some(Load::Ready(list)) => list.clone(),
                        _ => Vec::new(),
                    };
                    let load = (c.schemas_in(database).is_none() || c.databases.is_none()) && matches!(c.status, Status::Connected(_));
                    (c.catalog_in(database), user_schemas(c, database), databases, c.cfg.database.clone(), load)
                }
                None => Default::default(),
            }
        };
        let dirty = match &self.tabs.get(self.active)?.kind {
            TabKind::Console(c) => c.saved_id.as_ref().is_some_and(|id| self.saved.iter().any(|q| &q.id == id && q.sql != c.sql)),
            TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => false,
        };
        let tab = self.tabs.get_mut(self.active)?;
        let id = tab.id;
        match &mut tab.kind {
            TabKind::Console(c) => {
                let action = console_ui(ui, id, c, &catalog, &schemas, &databases, &default_db, dirty, read_only);
                action.or(load_schemas.then_some(TabAction::LoadSchemas))
            }
            TabKind::Table(t) => table_ui(ui, id, t, read_only),
            TabKind::Layout(l) => {
                let loads = l.ui(ui, &catalog);
                (!loads.is_empty()).then_some(TabAction::LoadLayers(loads))
            }
            TabKind::Diagram(d) => {
                let out = d.ui(ui);
                match out.open {
                    Some(table) => Some(TabAction::OpenTable { schema: d.schema.clone(), table }),
                    None => out.reload.then_some(TabAction::ReloadDiagram),
                }
            }
        }
    }
}

/// The title of a console opened as `name@database` (optionally with a
/// ` (n)` suffix) once it targets `database` instead; `None` for other titles
/// (saved query names).
fn console_title(title: &str, cfg: &ConnectionConfig, database: &str) -> Option<String> {
    let rest = title.strip_prefix(&conn_db_title(cfg, &cfg.database))?;
    (rest.is_empty() || rest.starts_with(" (")).then(|| format!("{}{rest}", conn_db_title(cfg, database)))
}

/// `name@database`, or `database@host` for an unnamed connection (whose
/// display name is already `database@host`).
fn conn_db_title(cfg: &ConnectionConfig, database: &str) -> String {
    if cfg.name.trim().is_empty() { format!("{database}@{}", cfg.host) } else { format!("{}@{database}", cfg.name) }
}

/// How a tab looks beyond its own title: its connection's color, tooltip
/// and status.
struct TabStyle {
    /// Replaces the tab's own title (a console on another database).
    title: Option<String>,
    color: Option<Color32>,
    tip: String,
    status: Color32,
}

impl Default for TabStyle {
    fn default() -> Self {
        Self { title: None, color: None, tip: String::new(), status: theme::FAINT() }
    }
}

/// One tab in the tab bar. Returns the tab's rect.
fn tab_button(
    ui: &mut Ui,
    i: usize,
    tab: &Tab,
    style: &TabStyle,
    active: bool,
    dirty: bool,
    count: usize,
    actions: &mut Vec<Action>,
) -> egui::Rect {
    const PAD_L: f32 = 10.0;
    const ICON: f32 = 14.0;
    const GAP: f32 = 7.0;
    const CLOSE: f32 = 18.0;
    const PAD_R: f32 = 6.0;

    let color = if active { theme::TEXT() } else { theme::MUTED() };
    // `*` marks unsaved changes to a saved query (as in DataGrip).
    let base = style.title.as_deref().unwrap_or(&tab.title);
    let title = if dirty { format!("* {base}") } else { base.to_string() };
    let galley = ui.painter().layout_no_wrap(title, TextStyle::Body.resolve(ui.style()), color);
    let size = egui::vec2(PAD_L + ICON + GAP + galley.size().x + GAP + CLOSE + PAD_R, 36.0);

    // The tab background is registered first and the close button last, so
    // the close button sits on top and receives its own clicks.
    let (rect, tab_resp) = ui.allocate_exact_size(size, Sense::click());
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - PAD_R - CLOSE / 2.0, rect.center().y),
        egui::vec2(CLOSE, CLOSE),
    );
    let close_resp = ui.interact(close_rect, Id::new(("tab-close", tab.id)), Sense::click()).on_hover_text("Close  ⌘W");

    let painter = ui.painter();
    // Square, edge to edge: the active tab takes the content's surface so it
    // reads as attached to it; a connection color tints the whole tab.
    let fill = if active {
        theme::SURFACE()
    } else if tab_resp.hovered() {
        theme::HOVER()
    } else {
        theme::BG()
    };
    let fill = match style.color {
        Some(col) => theme::tint(fill, col, if active { 0.34 } else if tab_resp.hovered() { 0.24 } else { 0.16 }),
        None => fill,
    };
    painter.rect_filled(rect, 0.0, fill);
    painter.vline(rect.right() - 0.5, rect.y_range().shrink(9.0), Stroke::new(1.0_f32, theme::BORDER()));
    // The active tab is underlined: the connection's color, else the accent.
    if active {
        let bar = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 3.0), rect.right_bottom());
        painter.rect_filled(bar, 0.0, style.color.unwrap_or(theme::ACCENT()));
    }
    let cy = rect.center().y;
    let icon = match tab.kind {
        TabKind::Console(_) => Icon::Database(style.status, style.color),
        TabKind::Table(_) => Icon::Table,
        TabKind::Layout(_) => Icon::Globe,
        TabKind::Diagram(_) => Icon::Schema,
    };
    widgets::paint_icon(painter, egui::pos2(rect.left() + PAD_L + ICON / 2.0, cy), icon);
    painter.galley(egui::pos2(rect.left() + PAD_L + ICON + GAP, cy - galley.size().y / 2.0), galley, color);
    if close_resp.hovered() {
        painter.rect_filled(close_rect, 4.0, theme::BORDER());
    }
    let x_color = if close_resp.hovered() { theme::TEXT() } else if active || tab_resp.hovered() { theme::MUTED() } else { theme::FAINT() };
    widgets::paint_icon_tinted(painter, close_rect.center(), Icon::Close, Some(x_color));

    if close_resp.clicked() || tab_resp.middle_clicked() {
        actions.push(Action::Close(CloseWhich::One(i)));
    } else if tab_resp.clicked() {
        actions.push(Action::ActivateTab(i));
    }
    let tab_resp = if style.tip.is_empty() { tab_resp } else { tab_resp.on_hover_text(&style.tip) };
    demo_open_menu(ui, "RGRIP_DEMO_TAB_MENU", i, &tab_resp);
    tab_resp.context_menu(|ui| {
        menu_layout(ui);
        let mut item = |ui: &mut Ui, icon: Svg, label: &str, enabled: bool, which: CloseWhich| {
            if crate::icons::menu_item(ui, icon, label, enabled).clicked() {
                actions.push(Action::Close(which));
                ui.close();
            }
        };
        item(ui, Svg::X, "Close", true, CloseWhich::One(i));
        item(ui, Svg::CopyX, "Close others", count > 1, CloseWhich::Others(i));
        item(ui, Svg::ArrowRightToLine, "Close tabs to the right", i + 1 < count, CloseWhich::ToTheRight(i));
        ui.separator();
        item(ui, Svg::CircleX, "Close all", true, CloseWhich::All);
    });
    rect
}

/// `dirty`: the console differs from its saved query (autosave pending).
fn console_ui(
    ui: &mut Ui,
    tab_id: TabId,
    c: &mut Console,
    catalog: &Catalog,
    schemas: &[String],
    databases: &[String],
    default_db: &str,
    dirty: bool,
    read_only: bool,
) -> Option<TabAction> {
    let mut action = None;
    let editor_id = Id::new(("editor", tab_id));
    let keys = completion::handle_keys(ui, &mut c.popup);
    if keys.dismissed {
        ui.memory_mut(|m| m.request_focus(editor_id));
    }
    if let Some(i) = keys.accept {
        if let Some(p) = c.popup.take() {
            c.cursor = completion::accept(ui.ctx(), editor_id, &mut c.sql, &p.completion, i);
            // Picking /new opens the editor right away.
            let byte = sql::char_to_byte(&c.sql, c.cursor.start);
            if let Some((range, line)) = rgrip_core::commands::command_line_at(&c.sql, byte)
                && let Some(name) = rgrip_core::commands::new_command(line)
            {
                let name = name.to_string();
                c.cursor = completion::replace_text(ui.ctx(), editor_id, &mut c.sql, range, "");
                return Some(TabAction::NewCommand(name));
            }
        }
    }
    // Slash commands: Tab expands the command on the cursor's line; ⌘↩
    // expands and runs it.
    let tab = c.popup.is_none() && ui.input(|i| i.key_pressed(Key::Tab) && i.modifiers.is_none());
    let run = ui.input(|i| i.key_pressed(Key::Enter) && i.modifiers.command && !i.modifiers.shift);
    if tab || run {
        let byte = sql::char_to_byte(&c.sql, c.cursor.start);
        if let Some((range, line)) = rgrip_core::commands::command_line_at(&c.sql, byte) {
            if tab {
                ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
            }
            if let Some(name) = rgrip_core::commands::new_command(line).filter(|_| tab) {
                let name = name.to_string();
                c.cursor = completion::replace_text(ui.ctx(), editor_id, &mut c.sql, range, "");
                return Some(TabAction::NewCommand(name));
            }
            match rgrip_core::commands::expand(line, catalog, c.schema_or_default(schemas)) {
                Ok(expanded) => {
                    let indent: String = line.chars().take_while(|ch| ch.is_whitespace()).collect();
                    let text = format!("{indent}{expanded}");
                    c.cursor = completion::replace_text(ui.ctx(), editor_id, &mut c.sql, range, &text);
                    c.popup = None;
                }
                Err(e) => {
                    if tab {
                        return Some(TabAction::Notify(e));
                    }
                }
            }
        }
    }
    let mut format = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F));
    // ⌘/ (and ⌘?, the same key with shift) toggles `--` on the selected lines.
    let comment = ui.input_mut(|i| {
        i.consume_key(Modifiers::COMMAND, Key::Slash)
            || i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Slash)
            || i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Questionmark)
            || i.consume_key(Modifiers::COMMAND, Key::Questionmark)
    });
    // Quotes and brackets close themselves; backspace in an empty pair takes both.
    if c.running.is_none() && ui.memory(|m| m.has_focus(editor_id)) {
        let sel = TextEdit::load_state(ui.ctx(), editor_id)
            .and_then(|s| s.cursor.char_range())
            .map_or(c.cursor.clone(), |r| r.as_sorted_char_range());
        let sel = sql::char_to_byte(&c.sql, sel.start)..sql::char_to_byte(&c.sql, sel.end);
        let edit = ui.input_mut(|i| {
            let mut edit = None;
            i.events.retain(|e| {
                if edit.is_some() {
                    return true;
                }
                match e {
                    egui::Event::Text(t) if t.chars().count() == 1 => {
                        let ch = t.chars().next().unwrap();
                        edit = rgrip_core::pairs::type_char(&c.sql, sel.clone(), ch);
                    }
                    egui::Event::Key { key: Key::Backspace, pressed: true, modifiers, .. } if modifiers.is_none() && sel.start == sel.end => {
                        edit = rgrip_core::pairs::backspace(&c.sql, sel.start).map(|(t, at)| (t, at..at));
                    }
                    _ => {}
                }
                edit.is_none()
            });
            edit
        });
        if let Some((text, sel)) = edit {
            c.sql = text;
            c.cursor = c.sql[..sel.start].chars().count()..c.sql[..sel.end].chars().count();
            if let Some(mut state) = TextEdit::load_state(ui.ctx(), editor_id) {
                let range = egui::text::CCursorRange::two(egui::text::CCursor::new(c.cursor.start), egui::text::CCursor::new(c.cursor.end));
                state.cursor.set_char_range(Some(range));
                state.store(ui.ctx(), editor_id);
            }
        }
    }
    if comment && c.running.is_none() {
        let sel = sql::char_to_byte(&c.sql, c.cursor.start)..sql::char_to_byte(&c.sql, c.cursor.end);
        let (text, sel) = rgrip_core::format::toggle_comment(&c.sql, sel);
        c.sql = text;
        c.cursor = c.sql[..sel.start].chars().count()..c.sql[..sel.end].chars().count();
        if let Some(mut state) = TextEdit::load_state(ui.ctx(), editor_id) {
            let range = egui::text::CCursorRange::two(egui::text::CCursor::new(c.cursor.start), egui::text::CCursor::new(c.cursor.end));
            state.cursor.set_char_range(Some(range));
            state.store(ui.ctx(), editor_id);
        }
        c.popup = None;
        ui.memory_mut(|m| m.request_focus(editor_id));
    }
    if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter)) {
        action = Some(TabAction::Run { all: true });
    } else if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
        action = Some(TabAction::Run { all: false });
    } else if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
        action = Some(TabAction::Save);
    }

    toolbar(ui, |ui| {
        if c.running.is_some() {
            if ui.add(Button::new(RichText::new("■  Stop").color(theme::ERROR()))).clicked() {
                action = Some(TabAction::Cancel);
            }
        } else {
            if ui
                .add(Button::new(RichText::new("▶  Run").color(theme::BG())).fill(theme::ACCENT()))
                .on_hover_text("Run statement at cursor  ⌘↩")
                .clicked()
            {
                action = Some(TabAction::Run { all: false });
            }
            if ui.button("Run all").on_hover_text("⇧⌘↩").clicked() {
                action = Some(TabAction::Run { all: true });
            }
            if ghost_button(ui, "Format").on_hover_text("Format selection or all  ⇧⌘F").clicked() {
                format = true;
            }
        }
        if read_only {
            read_only_badge(ui);
        }
        // Right side: save state, then the schema (stays visible while running).
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            match (&c.saved_id, dirty) {
                // Saved queries autosave; this just shows where things are.
                (Some(_), false) => {
                    ui.label(RichText::new("Saved").size(12.5).color(theme::FAINT())).on_hover_text("Changes save automatically");
                }
                (Some(_), true) => {
                    ui.label(RichText::new("Saving…").size(12.5).color(theme::MUTED())).on_hover_text("Saves a moment after you stop typing  ⌘S now");
                }
                (None, _) => {
                    if ghost_button(ui, "Save as…").on_hover_text("⌘S").clicked() {
                        action = Some(TabAction::Save);
                    }
                }
            }
            ui.add_space(8.0);
            ui.add_enabled_ui(c.running.is_none(), |ui| schema_picker(ui, tab_id, c, schemas));
            // Right-to-left: the database ends up left of the schema.
            ui.add_enabled_ui(c.running.is_none(), |ui| database_picker(ui, tab_id, c, databases, default_db));
        });
    });

    if format && c.running.is_none() {
        // The selection if there is one, else the whole editor.
        let range = if c.cursor.start != c.cursor.end {
            sql::char_to_byte(&c.sql, c.cursor.start)..sql::char_to_byte(&c.sql, c.cursor.end)
        } else {
            0..c.sql.len()
        };
        let formatted = rgrip_core::format::format_sql(&c.sql[range.clone()]);
        c.cursor = completion::replace_text(ui.ctx(), editor_id, &mut c.sql, range, &formatted);
        c.popup = None;
        ui.memory_mut(|m| m.request_focus(editor_id));
    }

    let max_h = (ui.available_height() - 120.0).max(120.0);
    egui::TopBottomPanel::top(Id::new(("editor-panel", tab_id)))
        .resizable(true)
        .default_height(240.0)
        .height_range(80.0..=max_h)
        .frame(Frame::new().fill(theme::SURFACE()).inner_margin(Margin { left: 0, right: 14, top: 10, bottom: 8 }))
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("editor-scroll", tab_id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Khmer is drawn shaped over the laid-out text (egui doesn't shape it).
                    let mut khmer = Vec::new();
                    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                        let mut job = highlight::sql(buf.as_str(), TextStyle::Monospace.resolve(ui.style()));
                        job.wrap.max_width = wrap;
                        khmer = crate::shaping::hide_runs(ui, &mut job);
                        ui.fonts_mut(|f| f.layout_job(job))
                    };
                    let out = TextEdit::multiline(&mut c.sql)
                        .id(editor_id)
                        .code_editor()
                        .frame(false)
                        .lock_focus(true)
                        .desired_width(f32::INFINITY)
                        .desired_rows(12)
                        .hint_text(hint("select * from …"))
                        .margin(Margin { left: 48, right: 0, top: 0, bottom: 0 })
                        .layouter(&mut layouter)
                        .show(ui);
                    crate::shaping::paint_runs(ui, &out.galley, out.galley_pos, &khmer);
                    paint_run_marker(ui, &out);
                    paint_line_numbers(ui, &out);
                    let previous = c.cursor.clone();
                    if let Some(r) = out.cursor_range {
                        c.cursor = r.as_sorted_char_range();
                    }
                    if out.response.changed() {
                        c.last_edit = Some(Instant::now());
                    }
                    update_popup(c, catalog, schemas, out.response.changed(), keys.force, previous);
                    if let Some(p) = &mut c.popup {
                        let focused = out.response.has_focus();
                        match completion::show(ui, Id::new(("completion", tab_id)), p, &out, &c.sql, focused) {
                            completion::Outcome::Accept(i) => {
                                let p = c.popup.take().unwrap();
                                c.cursor = completion::accept(ui.ctx(), editor_id, &mut c.sql, &p.completion, i);
                                ui.memory_mut(|m| m.request_focus(editor_id));
                            }
                            completion::Outcome::Close => c.popup = None,
                            completion::Outcome::None => {}
                        }
                    }
                    if c.focus {
                        out.response.request_focus();
                        c.focus = false;
                    }
                });
        });

    let Console { running, output, view, grid, paged, offset, map, last_sql, order, relations, probe, related, total, area, .. } = c;
    // Only a paged (wrappable) statement can be filtered to an area.
    let area_filter = paged.is_some().then_some(area.as_str());
    if let Some(started) = running {
        centered(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(13.0));
                ui.label(
                    RichText::new(format!("Running…  {:.1}s", started.elapsed().as_secs_f32()))
                        .color(theme::MUTED()),
                );
            });
        });
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        return action;
    }

    match output {
        None => centered(ui, |ui| muted(ui, "Run a query to see results here")),
        Some(Err(e)) => {
            Frame::new().inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.label(RichText::new(e.as_str()).monospace().color(theme::ERROR()));
                });
            });
        }
        Some(Ok(out)) => {
            let sets: Vec<&ResultSet> = out.statements.iter().filter_map(|s| s.result.as_ref()).collect();
            toolbar(ui, |ui| {
                for (n, rs) in sets.iter().enumerate() {
                    if segment(ui, *view == View::Result(n), &format!("Result {}", n + 1)) {
                        *view = View::Result(n);
                        *grid = GridState::fresh(grid);
                    }
                    if !rs.geometry_columns().is_empty() {
                        // The grid's selection is per result set.
                        let other = !matches!(*view, View::Result(m) | View::Map(m) | View::Split(m) if m == n);
                        if segment(ui, *view == View::Map(n), if sets.len() > 1 { "Map ↑" } else { "Map" }) {
                            if other {
                                *grid = GridState::fresh(grid);
                            }
                            map.opened(!grid.selection.is_empty());
                            *view = View::Map(n);
                        }
                        if segment(ui, *view == View::Split(n), if sets.len() > 1 { "Split ↑" } else { "Split" })
                        {
                            if other {
                                *grid = GridState::fresh(grid);
                            }
                            map.opened(!grid.selection.is_empty());
                            *view = View::Split(n);
                        }
                    }
                }
                if segment(ui, *view == View::Output, "Output") {
                    *view = View::Output;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ms = out.elapsed.as_secs_f64() * 1000.0;
                    let result = match *view {
                        View::Result(n) | View::Map(n) | View::Split(n) => sets.get(n).copied(),
                        View::Output => None,
                    };
                    if let Some(rs) = result {
                        if let Some(a) = export_menu(ui, rs, "query_result", paged.is_some()) {
                            action = Some(a);
                        }
                        ui.add_space(6.0);
                    }
                    let summary = match result {
                        Some(rs) if paged.is_some() => {
                            // Paged: `truncated` means another page exists.
                            if ui.add_enabled(rs.truncated, Button::new("›")).on_hover_text("Next 1,000").clicked() {
                                action = Some(TabAction::Page(true));
                            }
                            if ui.add_enabled(*offset > 0, Button::new("‹")).on_hover_text("Previous 1,000").clicked() {
                                action = Some(TabAction::Page(false));
                            }
                            let to = *offset + rs.rows.len();
                            let from = if rs.rows.is_empty() { 0 } else { *offset + 1 };
                            let more = match (rs.truncated, total) {
                                (true, Some(total)) => format!(" of {}", group_thousands(*total)),
                                (true, None) => " +".to_string(),
                                _ => String::new(),
                            };
                            format!("rows {}–{}{more}  ·  {ms:.0} ms", group_thousands(from as i64), group_thousands(to as i64))
                        }
                        Some(rs) => {
                            let more = if rs.truncated { " (first 1,000 shown)" } else { "" };
                            format!("{}{more}  ·  {ms:.0} ms", plural(rs.rows.len(), "row"))
                        }
                        None => format!("{}  ·  {ms:.0} ms", plural(out.statements.len(), "statement")),
                    };
                    ui.label(RichText::new(summary).small().color(theme::MUTED()));
                });
            });
            match *view {
                View::Result(n) if n < sets.len() => {
                    let row_offset = if paged.is_some() { *offset } else { 0 };
                    let menu_rows: Vec<usize> = grid.selection.iter().copied().collect();
                    let counts = probe.as_ref().map(|p| p.best(&menu_rows, relations.len())).unwrap_or_default();
                    let source = grid::Source { table: "query_result", sql: Some(last_sql.as_str()), relations, counts: &counts };
                    let sort = order.as_ref().filter(|(m, _, _)| *m == n).map(|(_, c, asc)| grid::Sort { column: c, ascending: *asc });
                    match grid::show(ui, Id::new(("grid", tab_id, n)), sets[n], grid, Some(sort.unwrap_or(grid::Sort { column: "", ascending: true })), row_offset, &source, None) {
                        Some(grid::GridEvent::HeaderClicked(i)) => action = Some(TabAction::SortResult(n, i)),
                        Some(GridEvent::Follow { relation, rows }) => {
                            let only = probe.as_ref().map(|p| p.matched(&rows, relation)).unwrap_or_default();
                            action = relations.get(relation).map(|r| follow(r, sets[n], &rows, &only));
                        }
                        Some(GridEvent::RowsMenu { rows }) => action = probe_action(relations, sets[n], rows, false),
                        Some(GridEvent::Lookup { relation }) => action = relations.get(relation).map(lookup),
                        _ => {}
                    }
                }
                View::Map(n) if n < sets.len() => {
                    let cols = sets[n].geometry_columns().to_vec();
                    if cols.is_empty() {
                        centered(ui, |ui| muted(ui, "No geometry columns in this result"));
                    } else {
                        let link = crate::map::Link { selection: &mut grid.selection, reveal: &mut grid.reveal, layer: None, area_filter };
                        let out = map.show(ui, sets[n], &cols, paged.is_some(), link);
                        if let Some(col) = out.reproject {
                            action = Some(TabAction::Reproject(col));
                        } else if let Some(a) = out.area {
                            action = Some(area_action(a));
                        }
                    }
                }
                View::Split(n) if n < sets.len() => {
                    let cols = sets[n].geometry_columns().to_vec();
                    let row_offset = if paged.is_some() { *offset } else { 0 };
                    let menu_rows: Vec<usize> = grid.selection.iter().copied().collect();
                    let counts = probe.as_ref().map(|p| p.best(&menu_rows, relations.len())).unwrap_or_default();
                    let source = grid::Source { table: "query_result", sql: Some(last_sql.as_str()), relations, counts: &counts };
                    let can_reproject = paged.is_some();
                    let mut sort_click = None;
                    let reproject = &mut action;
                    split_view(
                        ui,
                        Id::new(("split", tab_id)),
                        grid,
                        |ui, g| {
                            let link = crate::map::Link { selection: &mut g.selection, reveal: &mut g.reveal, layer: None, area_filter };
                            let out = map.show(ui, sets[n], &cols, can_reproject, link);
                            if let Some(col) = out.reproject {
                                *reproject = Some(TabAction::Reproject(col));
                            } else if let Some(a) = out.area {
                                *reproject = Some(area_action(a));
                            }
                        },
                        |ui, g| {
                            let sort = order.as_ref().filter(|(m, _, _)| *m == n).map(|(_, c, asc)| grid::Sort { column: c, ascending: *asc });
                            match grid::show(ui, Id::new(("grid", tab_id, n)), sets[n], g, Some(sort.unwrap_or(grid::Sort { column: "", ascending: true })), row_offset, &source, None) {
                                Some(grid::GridEvent::HeaderClicked(i)) => sort_click = Some(TabAction::SortResult(n, i)),
                                Some(GridEvent::Follow { relation, rows }) => {
                                    let only = probe.as_ref().map(|p| p.matched(&rows, relation)).unwrap_or_default();
                                    sort_click = relations.get(relation).map(|r| follow(r, sets[n], &rows, &only));
                                }
                                Some(GridEvent::RowsMenu { rows }) => sort_click = probe_action(relations, sets[n], rows, false),
                                Some(GridEvent::Lookup { relation }) => sort_click = relations.get(relation).map(lookup),
                                _ => {}
                            }
                        },
                    );
                    if sort_click.is_some() {
                        action = sort_click;
                    }
                }
                _ => output_log(ui, out),
            }
        }
    }
    if let Some(a) = related_ui(ui, tab_id, related) {
        action = Some(a);
    }
    action
}

/// Toolbar combo for the console's database (the connection's own until
/// another is picked). Switching resets the schema to the new database's default.
fn database_picker(ui: &mut Ui, tab_id: TabId, c: &mut Console, databases: &[String], default_db: &str) {
    const FILTER_FROM: usize = 12;
    let current = c.database.as_deref().unwrap_or(default_db).to_string();
    let filter_id = Id::new(("database-filter", tab_id));
    let mut picked = None;
    egui::ComboBox::from_id_salt(("database", tab_id))
        .selected_text(current.clone())
        .width(110.0)
        .show_ui(ui, |ui| {
            let mut filter = String::new();
            if databases.len() > FILTER_FROM {
                filter = ui.data_mut(|d| d.get_temp::<String>(filter_id)).unwrap_or_default();
                let r = ui.add(TextEdit::singleline(&mut filter).hint_text("Filter…").desired_width(140.0));
                if r.changed() {
                    ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));
                }
            }
            let needle = filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                for d in databases.iter().filter(|d| d.to_lowercase().contains(&needle)) {
                    if ui.selectable_label(*d == current, d).clicked() {
                        picked = Some(d.clone());
                    }
                }
            });
        })
        .response
        .on_hover_text("Database");
    if let Some(d) = picked.filter(|d| *d != current) {
        c.database = (d != default_db).then_some(d);
        c.schema = None;
        c.more_schemas.clear();
        c.popup = None;
    }
}

/// Toolbar combo for the console's schema. With many schemas the list gets a
/// filter field.
/// Schemas for unqualified names. Tick several to search them all, in the
/// order ticked; the first is the main one (autocomplete, slash commands).
fn schema_picker(ui: &mut Ui, tab_id: TabId, c: &mut Console, schemas: &[String]) {
    const FILTER_FROM: usize = 12;
    let list = c.search_list(schemas);
    let title = match list.len() {
        1 => list[0].clone(),
        n => format!("{} +{}", list[0], n - 1),
    };
    let filter_id = Id::new(("schema-filter", tab_id));
    let mut toggled = None;
    egui::ComboBox::from_id_salt(("schema", tab_id))
        .selected_text(title)
        .width(110.0)
        // Ticking several in a row: stay open until a click outside.
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            menu_layout(ui);
            let mut filter = String::new();
            if schemas.len() > FILTER_FROM {
                filter = ui.data_mut(|d| d.get_temp::<String>(filter_id)).unwrap_or_default();
                let r = ui.add(TextEdit::singleline(&mut filter).hint_text("Filter…").desired_width(160.0));
                if r.changed() {
                    ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));
                }
            }
            ui.label(RichText::new("Searched in this order, then public").size(11.5).color(theme::FAINT()));
            let needle = filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                for s in schemas.iter().filter(|s| s.to_lowercase().contains(&needle)) {
                    let pos = list.iter().position(|x| x == s);
                    let mut on = pos.is_some();
                    let label = match pos {
                        Some(i) if list.len() > 1 => format!("{s}  ·{}", i + 1),
                        _ => s.clone(),
                    };
                    // Keep at least one schema ticked.
                    let lone = on && list.len() == 1;
                    if ui.add_enabled(!lone, egui::Checkbox::new(&mut on, label)).clicked() {
                        toggled = Some(s.clone());
                    }
                }
            });
        })
        .response
        .on_hover_text(list.join(", "));
    if let Some(s) = toggled {
        let mut list = list;
        match list.iter().position(|x| *x == s) {
            Some(i) => {
                list.remove(i);
            }
            None => list.push(s),
        }
        if !list.is_empty() {
            c.schema = Some(list.remove(0));
            c.more_schemas = list;
            c.popup = None;
        }
    }
}

/// Open, refresh or close the completion popup after the editor ran.
fn update_popup(c: &mut Console, catalog: &Catalog, schemas: &[String], changed: bool, force: bool, previous: std::ops::Range<usize>) {
    if c.cursor.start != c.cursor.end {
        c.popup = None;
        return;
    }
    let byte = sql::char_to_byte(&c.sql, c.cursor.start);
    let before = c.sql[..byte].chars().next_back();
    let in_word = before.is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | '"' | '/'));
    if force || (changed && in_word) {
        c.popup = complete::complete(&c.sql, byte, catalog, c.schema_or_default(schemas), force).map(Popup::new);
    } else if changed || previous != c.cursor {
        // Typed a separator, or moved the cursor away.
        c.popup = None;
    }
}

/// Export menu for a result grid: copy or save the rows shown, or (when
/// `can_export_all`) write every row of the underlying query to a file.
fn export_menu(ui: &mut Ui, rs: &ResultSet, table: &str, can_export_all: bool) -> Option<TabAction> {
    let has_geometry = !rs.geometry_columns().is_empty();
    let formats: Vec<export::Format> = export::Format::ALL
        .into_iter()
        .filter(|f| *f != export::Format::GeoJson || has_geometry)
        .collect();
    let file_name = table.rsplit('.').next().unwrap_or("export").to_string();
    let save_path = |f: export::Format| {
        rfd::FileDialog::new()
            .set_file_name(format!("{file_name}.{}", f.extension()))
            .add_filter(f.label(), &[f.extension()])
            .save_file()
    };
    let mut action = None;
    ui.menu_button(RichText::new("Export").color(theme::MUTED()), |ui| {
        ui.set_min_width(190.0);
        ui.menu_button("Copy as", |ui| {
            for f in &formats {
                let label = if *f == export::Format::Svg { "Image (PNG)" } else { f.label() };
                if ui.button(label).clicked() {
                    if *f == export::Format::Svg {
                        crate::snapshot::copy_table(ui.ctx(), rs, table);
                    } else {
                        ui.ctx().copy_text(export::export(rs, *f, table));
                    }
                    ui.close();
                }
            }
        });
        ui.menu_button(format!("Save {} as", plural(rs.rows.len(), "row")), |ui| {
            for f in &formats {
                if ui.button(format!("{}…", f.label())).clicked() {
                    ui.close();
                    if let Some(path) = save_path(*f) {
                        let _ = std::fs::write(&path, export::export(rs, *f, table));
                    }
                }
            }
        });
        if can_export_all {
            ui.menu_button("Export all rows as", |ui| {
                for f in &formats {
                    if ui.button(format!("{}…", f.label())).clicked() {
                        ui.close();
                        if let Some(path) = save_path(*f) {
                            action = Some(TabAction::ExportAll(*f, path));
                        }
                    }
                }
            });
        }
    });
    action
}

/// What Run executes for a byte cursor with no selection: a command line,
/// else the statement under the cursor.
fn run_range(sql: &str, at: usize) -> Option<std::ops::Range<usize>> {
    match rgrip_core::commands::command_line_at(sql, at) {
        // A command line is its own statement (it has no `;`).
        Some((r, _)) => Some(r),
        None => sql::statement_at(sql, at),
    }
}

/// A bar in the gutter beside the lines Run will execute.
fn paint_run_marker(ui: &Ui, out: &egui::text_edit::TextEditOutput) {
    let Some(r) = out.cursor_range else { return };
    if r.primary.index != r.secondary.index {
        return;
    }
    let text = out.galley.text();
    let Some(range) = run_range(text, sql::char_to_byte(text, r.primary.index)) else { return };
    let first = text[..range.start].matches('\n').count() + 1;
    let last = first + text[range].matches('\n').count();
    let mut line = 1;
    let (mut top, mut bottom) = (None, None);
    for row in &out.galley.rows {
        if (first..=last).contains(&line) {
            let rect = row.rect().translate(out.galley_pos.to_vec2());
            top.get_or_insert(rect.top());
            bottom = Some(rect.bottom());
        }
        if row.row.ends_with_newline {
            line += 1;
        }
    }
    if let (Some(top), Some(bottom)) = (top, bottom) {
        let x = out.galley_pos.x - 8.0;
        ui.painter().rect_filled(egui::Rect::from_x_y_ranges(x..=x + 2.0, top..=bottom), 1.0, theme::ACCENT());
    }
}

fn paint_line_numbers(ui: &Ui, out: &egui::text_edit::TextEditOutput) {
    let cursor_line = out.cursor_range.map(|r| {
        out.galley.text()[..sql::char_to_byte(out.galley.text(), r.primary.index)].matches('\n').count() + 1
    });
    let font = FontId::monospace(11.5);
    let mut line = 1;
    let mut new_line = true;
    for row in &out.galley.rows {
        if new_line {
            let y = out.galley_pos.y + row.rect().center().y;
            let color = if cursor_line == Some(line) { theme::MUTED() } else { theme::FAINT() };
            ui.painter().text(
                egui::pos2(out.galley_pos.x - 16.0, y),
                egui::Align2::RIGHT_CENTER,
                line.to_string(),
                font.clone(),
                color,
            );
            line += 1;
        }
        new_line = row.row.ends_with_newline;
    }
}

fn output_log(ui: &mut Ui, out: &QueryOutput) {
    Frame::new().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, s) in out.statements.iter().enumerate() {
                let msg = match &s.result {
                    Some(rs) => format!("{} retrieved", plural(rs.rows.len(), "row")),
                    None if s.affected > 0 => format!("{} affected", plural(s.affected as usize, "row")),
                    None => "completed".to_string(),
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("#{}", i + 1)).monospace().color(theme::FAINT()));
                    ui.label(RichText::new(msg).monospace().color(theme::OK()));
                });
            }
            ui.label(
                RichText::new(format!("finished in {:.0} ms", out.elapsed.as_secs_f64() * 1000.0))
                    .monospace()
                    .color(theme::MUTED()),
            );
        });
    });
}

/// Columns that identify a row for inline edits: the primary key, else a
/// single-column unique key (ArcGIS tables have no primary key but unique
/// `objectid` / `id` / `globalid`). Empty: read-only. Saves stay safe either
/// way: each UPDATE must change exactly one row.
fn edit_key(schema: &str, table: &str, columns: &[ColumnInfo], catalog: &Catalog) -> Vec<String> {
    let pk: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    if !pk.is_empty() {
        return pk;
    }
    let Some(t) = catalog.tables.iter().find(|t| t.schema == schema && t.name == table) else { return Vec::new() };
    let preferred = ["objectid", "id", "globalid", "gid", "fid"].iter().find_map(|p| t.keys.iter().find(|k| k.eq_ignore_ascii_case(p)));
    preferred.or(t.keys.first()).map(|k| vec![k.clone()]).unwrap_or_default()
}

/// Real foreign keys plus relations guessed from column names.
fn with_inferred(schema: &str, table: &str, columns: &[ColumnInfo], fks: &[Relation], catalog: &Catalog) -> Vec<Relation> {
    let mut all = fks.to_vec();
    all.extend(rgrip_core::relations::infer(schema, table, columns, catalog, fks));
    all
}

fn area_action(e: crate::map::AreaEvent) -> TabAction {
    match e {
        crate::map::AreaEvent::Set(sql) => TabAction::ResultArea(Some(sql)),
        crate::map::AreaEvent::Clear => TabAction::ResultArea(None),
    }
}

/// Relations of a query result: those of each source table, real and
/// inferred, renamed to the result's columns.
fn result_relations(origins: &[Option<rgrip_core::relations::Origin>], tables: &[SourceTable], names: &[String], catalog: &Catalog) -> Vec<Relation> {
    tables
        .iter()
        .flat_map(|(schema, table, columns, fks)| {
            with_inferred(schema, table, columns, fks, catalog)
                .iter()
                .filter_map(|r| rgrip_core::relations::on_result(r, schema, table, origins, names))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Show the rows related to `rows` through `r` in the popup.
/// `only`: the variants known to match (empty: try them all).
fn follow(r: &Relation, rs: &ResultSet, rows: &[usize], only: &[usize]) -> TabAction {
    match related_filter_only(r, rs, rows, only) {
        Some(filter) => {
            let (arrow, cols): (&str, Vec<&str>) = if r.outgoing {
                ("→", r.columns.iter().map(|(here, _)| here.as_str()).collect())
            } else {
                ("←", r.columns.iter().map(|(_, there)| there.as_str()).collect())
            };
            let other = if r.other_schema == "public" { r.other_table.clone() } else { format!("{}.{}", r.other_schema, r.other_table) };
            TabAction::ShowRelated(RelatedTarget {
                schema: r.other_schema.clone(),
                table: r.other_table.clone(),
                filter,
                label: format!("{arrow} {other} ({}){}", cols.join(", "), if r.inferred { "  ~ inferred" } else { "" }),
            })
        }
        None => TabAction::Notify("No related rows: the key is NULL".into()),
    }
}

/// Show the whole table `r` points at in the popup (the lookup icon).
fn lookup(r: &Relation) -> TabAction {
    let other = if r.other_schema == "public" { r.other_table.clone() } else { format!("{}.{}", r.other_schema, r.other_table) };
    TabAction::ShowRelated(RelatedTarget { schema: r.other_schema.clone(), table: r.other_table.clone(), filter: String::new(), label: other })
}

/// The column pairs a relation may join on: the main pairs, then each
/// alternative key on its own.
fn relation_variants(r: &Relation) -> Vec<Vec<(String, String)>> {
    std::iter::once(r.columns.clone()).chain(r.alternatives.iter().map(|p| vec![p.clone()])).collect()
}

/// WHERE on the other side for one variant, matching the given rows' values.
fn variant_filter(pairs: &[(String, String)], rs: &ResultSet, rows: &[usize]) -> Option<String> {
    let pairs: Vec<(usize, &str)> = pairs
        .iter()
        .map(|(here, there)| Some((rs.columns.iter().position(|c| c == here)?, there.as_str())))
        .collect::<Option<_>>()?;
    let keys: Vec<Vec<(&str, Option<&str>)>> = rows
        .iter()
        .filter_map(|&row| rs.rows.get(row))
        .map(|row| pairs.iter().map(|&(c, there)| (there, row[c].as_deref())).collect())
        .collect();
    rgrip_core::edit::match_rows(&keys)
}

/// WHERE for the other side of `r`, matching the given rows' key values:
/// any of its variants (only those in `only`, when given). `None` when
/// every key is NULL (nothing related) or a column is missing.
fn related_filter_only(r: &Relation, rs: &ResultSet, rows: &[usize], only: &[usize]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for (v, pairs) in relation_variants(r).iter().enumerate() {
        if !only.is_empty() && !only.contains(&v) {
            continue;
        }
        if let Some(f) = variant_filter(pairs, rs, rows).filter(|f| !parts.contains(f)) {
            parts.push(f);
        }
    }
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(parts.iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(" or ")),
    }
}

#[cfg(test)]
fn related_filter(r: &Relation, rs: &ResultSet, rows: &[usize]) -> Option<String> {
    related_filter_only(r, rs, rows, &[])
}

/// One count per relation variant for `rows` (what the row menu shows).
fn probe_action(relations: &[Relation], rs: &ResultSet, rows: Vec<usize>, modal: bool) -> Option<TabAction> {
    let (mut probes, mut map) = (Vec::new(), Vec::new());
    for (ri, r) in relations.iter().enumerate() {
        for (vi, pairs) in relation_variants(r).iter().enumerate() {
            if let Some(f) = variant_filter(pairs, rs, &rows) {
                probes.push((r.other_schema.clone(), r.other_table.clone(), f));
                map.push((ri, vi));
            }
        }
    }
    (!probes.is_empty()).then_some(TabAction::Probe { modal, rows, probes, map })
}

/// One UPDATE per edited row, matched on the primary key values as loaded.
fn edit_statements(t: &TableView, rs: &ResultSet) -> Result<Vec<String>, String> {
    let key_cols: Vec<usize> = t
        .key
        .iter()
        .map(|k| rs.columns.iter().position(|c| c == k).ok_or(format!("Primary key column {k} isn't in the result")))
        .collect::<Result<_, _>>()?;
    let mut rows: std::collections::BTreeMap<usize, Vec<(&str, Option<&str>)>> = Default::default();
    for ((row, col), value) in &t.changes {
        rows.entry(*row).or_default().push((rs.columns[*col].as_str(), value.as_deref()));
    }
    Ok(rows
        .into_iter()
        .map(|(row, set)| {
            let key: Vec<(&str, Option<&str>)> =
                key_cols.iter().map(|&c| (rs.columns[c].as_str(), rs.rows[row][c].as_deref())).collect();
            rgrip_core::edit::update_sql(&t.schema, &t.table, &key, &set)
        })
        .collect())
}

/// A small lock and "read-only" for connections whose session rejects writes.
fn read_only_badge(ui: &mut Ui) {
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
            crate::icons::paint(ui, Svg::Lock, r, theme::WARN());
            ui.label(RichText::new("read-only").size(11.5).color(theme::WARN()));
        })
        .response;
    resp.on_hover_text("Postgres rejects writes on this connection");
}

fn table_ui(ui: &mut Ui, tab_id: TabId, t: &mut TableView, read_only: bool) -> Option<TabAction> {
    let mut action = None;
    if !t.changes.is_empty() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
        action = Some(TabAction::SaveEdits);
    }
    // Like the ↻ button: not while edits are pending (a reload drops them).
    if t.changes.is_empty() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::R)) {
        action = Some(TabAction::Reload);
    }
    toolbar(ui, |ui| {
        ui.label(RichText::new(format!("{}.{}", t.schema, t.table)).strong());
        ui.add_space(8.0);
        let filter = ui.add(
            TextEdit::singleline(&mut t.filter)
                .hint_text(hint("WHERE …   (enter to apply)"))
                .font(TextStyle::Monospace)
                .desired_width(320.0)
                .margin(Margin::symmetric(8, 4)),
        );
        // Reloading drops pending edits, so it waits for Save / Discard.
        let locked = !t.changes.is_empty();
        if filter.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            if locked {
                action = Some(TabAction::Notify("Save or discard your changes first".into()));
            } else {
                t.applied_filter = t.filter.clone();
                t.page = 0;
                action = Some(TabAction::Reload);
            }
        }
        if ui.add_enabled_ui(!locked, |ui| ghost_button(ui, "↻").on_hover_text("Reload  ⌘R")).inner.clicked() {
            action = Some(TabAction::Reload);
        }
        if let Some(Ok(rs)) = &t.data {
            if !rs.geometry_columns().is_empty() {
                ui.add_space(6.0);
                if segment(ui, t.pane == Pane::Grid, "Grid") {
                    t.pane = Pane::Grid;
                }
                if segment(ui, t.pane == Pane::Map, "Map") {
                    if t.pane == Pane::Grid {
                        t.map.opened(!t.grid.selection.is_empty());
                    }
                    t.pane = Pane::Map;
                }
                if segment(ui, t.pane == Pane::Split, "Split") {
                    if t.pane == Pane::Grid {
                        t.map.opened(!t.grid.selection.is_empty());
                    }
                    t.pane = Pane::Split;
                }
            }
        }
        if t.loading || t.saving {
            ui.add(egui::Spinner::new().size(12.0));
        }
        if locked {
            ui.add_space(8.0);
            let cells = t.changes.len();
            ui.label(RichText::new(plural(cells, "change")).small().color(theme::ACCENT()));
            let save = Button::new(RichText::new("Save").color(theme::BG())).fill(theme::ACCENT());
            if ui.add_enabled(!t.saving, save).on_hover_text("Write the changes in one transaction  ⌘S").clicked() {
                action = Some(TabAction::SaveEdits);
            }
            if ui.add_enabled(!t.saving, Button::new("Discard")).clicked() {
                t.changes.clear();
                t.editor = None;
            }
        } else if read_only {
            ui.add_space(8.0);
            ui.label(RichText::new("read-only").small().color(theme::FAINT())).on_hover_text(READ_ONLY_HINT);
        } else if matches!(&t.data, Some(Ok(_))) && t.key.is_empty() && !t.loading {
            ui.add_space(8.0);
            ui.label(RichText::new("read-only").small().color(theme::FAINT()))
                .on_hover_text("No primary key or unique column: rows can't be identified for editing");
            if ui.add_enabled(!t.saving, Button::new(RichText::new("Add primary key").small())).on_hover_text("Add an id column numbered 1, 2, 3… as the primary key, so rows can be edited").clicked() {
                action = Some(TabAction::AddKey);
            }
        } else if !t.key.is_empty() && !t.columns.iter().any(|c| c.primary_key) && !locked {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("keyed on {}", t.key.join(", "))).small().color(theme::FAINT()))
                .on_hover_text("No primary key: edits find each row by this unique column");
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(Ok(rs)) = &t.data {
                if let Some(a) = export_menu(ui, rs, &format!("{}.{}", t.schema, t.table), true) {
                    action = Some(a);
                }
                ui.add_space(6.0);
            }
            let rows = match &t.data {
                Some(Ok(rs)) => rs.rows.len() as i64,
                _ => 0,
            };
            let last_page = match t.total {
                Some(total) => (t.page + 1) * PAGE_SIZE >= total,
                None => rows < PAGE_SIZE,
            };
            if ui.add_enabled(!last_page && !t.loading && !locked, Button::new("›")).clicked() {
                t.page += 1;
                action = Some(TabAction::Reload);
            }
            if ui.add_enabled(t.page > 0 && !t.loading && !locked, Button::new("‹")).clicked() {
                t.page -= 1;
                action = Some(TabAction::Reload);
            }
            let from = if rows == 0 { 0 } else { t.page * PAGE_SIZE + 1 };
            let range = format!("{from}–{}", t.page * PAGE_SIZE + rows);
            let text = match t.total {
                Some(total) => format!("{range} of {total}"),
                None => range,
            };
            ui.label(RichText::new(text).small().color(theme::MUTED()));
        });
    });

    match &t.data {
        None => centered(ui, |ui| muted(ui, "loading…")),
        Some(Err(e)) => {
            Frame::new().inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
                ui.label(RichText::new(e.as_str()).monospace().color(theme::ERROR()));
            });
        }
        Some(Ok(rs)) => {
            let cols = rs.geometry_columns().to_vec();
            let pane = if cols.is_empty() { Pane::Grid } else { t.pane };
            let (table_name, table_sql) = (format!("{}.{}", t.schema, t.table), rgrip_core::postgres::table_select_sql(&t.query()));
            let sort = t.order.as_ref().map(|(c, asc)| Sort { column: c, ascending: *asc });
            let offset = (t.page * PAGE_SIZE) as usize;
            let kinds: Vec<_> = rs.columns.iter().map(|c| t.temporal.get(c).copied()).collect();
            let menu_rows: Vec<usize> = t.grid.selection.iter().copied().collect();
            let counts = t.probe.as_ref().map(|p| p.best(&menu_rows, t.relations.len())).unwrap_or_default();
            let editing = (!t.key.is_empty() && !t.saving && !read_only).then_some(grid::Editing {
                changes: &t.changes,
                editor: &mut t.editor,
                kinds: &kinds,
            });
            // Both views take the grid state as an argument (they share its selection).
            let draw_grid = |ui: &mut Ui, g: &mut GridState| {
                grid::show(
                    ui,
                    Id::new(("table-grid", tab_id)),
                    rs,
                    g,
                    sort,
                    offset,
                    &grid::Source { table: &table_name, sql: Some(&table_sql), relations: &t.relations, counts: &counts },
                    editing,
                )
            };
            let applied = t.applied_filter.as_str();
            let map = &mut t.map;
            let mut draw_map = |ui: &mut Ui, g: &mut GridState| {
                let link = crate::map::Link { selection: &mut g.selection, reveal: &mut g.reveal, layer: Some(table_name.as_str()), area_filter: Some(applied) };
                map.show(ui, rs, &cols, false, link)
            };
            let mut event = None;
            let mut map_out = crate::map::MapOutput::default();
            match pane {
                Pane::Grid => event = draw_grid(ui, &mut t.grid),
                Pane::Map => map_out = draw_map(ui, &mut t.grid),
                Pane::Split => {
                    let (mut e, mut m) = (None, None);
                    split_view(ui, Id::new(("split", tab_id)), &mut t.grid, |ui, g| m = Some(draw_map(ui, g)), |ui, g| e = draw_grid(ui, g));
                    event = e;
                    map_out = m.unwrap_or_default();
                }
            }
            match map_out.area {
                // Reloading drops pending edits, so it waits for Save / Discard.
                Some(_) if !t.changes.is_empty() => {
                    action = Some(TabAction::Notify("Save or discard your changes first".into()));
                }
                Some(crate::map::AreaEvent::Set(sql)) => {
                    t.filter = sql.clone();
                    t.applied_filter = sql;
                    t.page = 0;
                    action = Some(TabAction::Reload);
                }
                Some(crate::map::AreaEvent::Clear) => {
                    t.filter.clear();
                    t.applied_filter.clear();
                    t.page = 0;
                    action = Some(TabAction::Reload);
                }
                None => {}
            }
            match event {
                Some(GridEvent::Change { row, col, value }) => {
                    if rs.rows[row][col] == value {
                        t.changes.remove(&(row, col));
                    } else {
                        t.changes.insert((row, col), value);
                    }
                }
                Some(GridEvent::Revert { row, col }) => {
                    t.changes.remove(&(row, col));
                }
                Some(GridEvent::Follow { relation, rows }) => {
                    let only = t.probe.as_ref().map(|p| p.matched(&rows, relation)).unwrap_or_default();
                    action = Some(follow(&t.relations[relation], rs, &rows, &only));
                }
                Some(GridEvent::RowsMenu { rows }) => {
                    action = probe_action(&t.relations, rs, rows, false);
                }
                Some(GridEvent::Lookup { relation }) => action = Some(lookup(&t.relations[relation])),
                // Sorting reloads the page, which would drop pending edits.
                Some(GridEvent::HeaderClicked(_)) if !t.changes.is_empty() => {
                    action = Some(TabAction::Notify("Save or discard your changes first".into()));
                }
                Some(GridEvent::HeaderClicked(i)) => {
                    let col = rs.columns[i].clone();
                    // Cycle: ascending → descending → unsorted.
                    t.order = match t.order.take() {
                        Some((c, true)) if c == col => Some((c, false)),
                        Some((c, false)) if c == col => None,
                        _ => Some((col, true)),
                    };
                    t.page = 0;
                    action = Some(TabAction::Reload);
                }
                None => {}
            }
        }
    }
    if let Some(a) = related_ui(ui, tab_id, &mut t.related) {
        action = Some(a);
    }
    action
}

/// The related-rows popup: a read-only grid of the rows on the other side
/// of a foreign key, which can follow further relations.
fn related_ui(ui: &mut Ui, tab_id: TabId, related: &mut Option<Related>) -> Option<TabAction> {
    let rel = related.as_mut()?;
    let mut action = None;
    let mut close = false;
    let screen = ui.ctx().content_rect();
    let modal = egui::Modal::new(Id::new(("related", tab_id))).show(ui.ctx(), |ui| {
        ui.set_width((screen.width() * 0.85).min(1400.0));
        ui.horizontal(|ui| {
            if !rel.back.is_empty() && ghost_button(ui, "‹ Back").clicked() {
                action = Some(TabAction::RelatedBack);
            }
            ui.label(RichText::new(&rel.target.label).strong());
            ui.label(RichText::new(&rel.target.filter).monospace().small().color(theme::MUTED()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if crate::widgets::icon_button(ui, Icon::Close, "Close  Esc").clicked() {
                    close = true;
                }
                if ghost_button(ui, "Open as tab").on_hover_text("Open the table filtered to these rows").clicked() {
                    action = Some(TabAction::OpenFiltered {
                        schema: rel.target.schema.clone(),
                        table: rel.target.table.clone(),
                        filter: rel.target.filter.clone(),
                    });
                    close = true;
                }
                if let Some(Ok(rs)) = &rel.data {
                    let n = rs.rows.len() as i64;
                    let text = match rel.total {
                        Some(total) if total > n => format!("first {n} of {total}"),
                        _ => plural(n as usize, "row"),
                    };
                    ui.label(RichText::new(text).small().color(theme::MUTED()));
                }
            });
        });
        ui.add_space(6.0);
        let height = (screen.height() * 0.7).max(200.0);
        ui.allocate_ui(egui::vec2(ui.available_width(), height), |ui| {
            ui.set_min_height(height);
            match &rel.data {
                None => centered(ui, |ui| muted(ui, "loading…")),
                Some(Err(e)) => {
                    ui.label(RichText::new(e.as_str()).monospace().color(theme::ERROR()));
                }
                Some(Ok(rs)) if rs.rows.is_empty() => centered(ui, |ui| muted(ui, if rel.target.filter.is_empty() { "no rows" } else { "no related rows" })),
                Some(Ok(rs)) => {
                    let name = format!("{}.{}", rel.target.schema, rel.target.table);
                    let menu_rows: Vec<usize> = rel.grid.selection.iter().copied().collect();
                    let counts = rel.probe.as_ref().map(|p| p.best(&menu_rows, rel.relations.len())).unwrap_or_default();
                    let source = grid::Source { table: &name, sql: None, relations: &rel.relations, counts: &counts };
                    let event = grid::show(ui, Id::new(("related-grid", tab_id)), rs, &mut rel.grid, None, 0, &source, None);
                    match event {
                        Some(GridEvent::Follow { relation, rows }) => {
                            let only = rel.probe.as_ref().map(|p| p.matched(&rows, relation)).unwrap_or_default();
                            action = Some(follow(&rel.relations[relation], rs, &rows, &only));
                        }
                        Some(GridEvent::RowsMenu { rows }) => action = probe_action(&rel.relations, rs, rows, true),
                        Some(GridEvent::Lookup { relation }) => action = Some(lookup(&rel.relations[relation])),
                        _ => {}
                    }
                }
            }
        });
    });
    if close || modal.should_close() {
        *related = None;
    }
    action
}

// ---- saved query name prompt -------------------------------------------

/// `query` wrapped to add `col` reprojected to WGS84 as `alias`.
fn reproject_sql(query: &str, col: &str, alias: &str) -> String {
    let id = |s: &str| if complete::needs_quotes(s) { sql::quote_ident(s) } else { s.to_string() };
    let body = query.trim().trim_end_matches(';').trim_end();
    format!("select q.*, ST_Transform(q.{}, 4326) as {}\nfrom (\n{body}\n) q;", id(col), id(alias))
}

/// First meaningful line of SQL, shortened, as a default query name.
fn default_query_name(sql: &str, n: usize) -> String {
    let line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or_default();
    let line: String = line.chars().take(40).collect();
    if line.is_empty() { format!("Query {n}") } else { line }
}

impl App {
    /// The import dialog, fed the schema's tables and, when appending, the
    /// target's columns.
    fn import_ui(&mut self, ctx: &egui::Context) {
        use crate::import::Outcome;
        let Some(d) = &mut self.import else { return };
        let Some(ci) = self.conns.iter().position(|c| c.cfg.id == d.conn) else {
            self.import = None;
            return;
        };
        let conn = &self.conns[ci];
        let tables = match conn.tables.get(&d.schema) {
            Some(Load::Ready(t)) => t.iter().filter(|t| matches!(t.kind, TableKind::Table | TableKind::Partitioned)).map(|t| t.name.clone()).collect(),
            _ => Vec::new(),
        };
        let target = match conn.columns.get(&(d.schema.clone(), d.table().to_string())) {
            Some(Load::Ready(cols)) => Some(cols.as_slice()),
            _ => None,
        };
        let (id, schema) = (d.conn.clone(), d.schema.clone());
        match d.ui(ctx, tables, target) {
            Outcome::Keep => {}
            Outcome::Close => self.import = None,
            Outcome::Start(request) => self.worker.send(Cmd::Import { conn: id, request }),
            Outcome::NeedColumns(table) => self.apply(Action::LoadColumns(ci, schema, table)),
            Outcome::OpenTable(table) => {
                self.import = None;
                self.open_table(ci, schema, table);
            }
        }
    }

    fn table_editor_ui(&mut self, ctx: &egui::Context) {
        use crate::table_edit::Outcome;
        let Some(ed) = &mut self.table_editor else { return };
        match ed.ui(ctx) {
            Outcome::Keep => {}
            Outcome::Close => self.table_editor = None,
            Outcome::Apply { statements, new_name } => {
                let (conn, schema, table) = (ed.conn.clone(), ed.schema.clone(), ed.table.clone());
                self.worker.send(Cmd::AlterTable { conn, schema, table, new_name, statements });
            }
        }
    }

    /// Settings → Slash commands: list on the left, the selected command's
    /// name, description and SQL template on the right, with a live preview.
    fn commands_editor_ui(&mut self, ctx: &egui::Context) {
        use rgrip_core::commands::{self, SlashCommand};
        const LIST_W: f32 = 190.0;
        const GAP: f32 = 10.0;
        /// Height of the template and preview panes.
        const PANE_H: f32 = 380.0;
        let field_label = |ui: &mut Ui, text: &str| {
            ui.label(RichText::new(text).size(12.0).color(theme::MUTED()));
            ui.add_space(2.0);
        };
        let Some(ed) = &mut self.commands_editor else { return };
        // Preview against the active tab's connection, if it has a catalog.
        let catalog = self.tabs.get(self.active).and_then(|t| self.conns.iter().find(|c| c.cfg.id == t.conn)).map(|c| c.catalog.clone()).unwrap_or_default();
        let (mut save, mut close, mut reset) = (false, false, false);
        let modal = egui::Modal::new(Id::new("commands-editor"))
            .frame(Frame::new().fill(theme::SURFACE()).stroke(Stroke::new(1.0_f32, theme::BORDER())).corner_radius(12).inner_margin(Margin::same(20)))
            .show(ctx, |ui| {
                ui.set_width((ctx.content_rect().width() - 80.0).clamp(760.0, 1180.0));
                ui.label(RichText::new("Slash commands").heading().color(theme::TEXT()));
                ui.label(RichText::new("Type /name in the editor, then Tab to expand (or run it directly).").size(12.5).color(theme::MUTED()));
                ui.add_space(12.0);
                ui.horizontal_top(|ui| {
                    // List: a heading level with "Name", left-aligned rows.
                    ui.vertical(|ui| {
                        ui.set_width(LIST_W);
                        field_label(ui, "Commands");
                        egui::ScrollArea::vertical().id_salt("cmd-list").max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for (i, c) in ed.list.iter().enumerate() {
                                let label = if c.name.is_empty() { "(unnamed)".to_string() } else { format!("/{}", c.name) };
                                if list_row(ui, &label, i == ed.selected, LIST_W).clicked() {
                                    ed.selected = i;
                                }
                            }
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("+ Add").clicked() {
                                ed.list.push(SlashCommand { name: format!("cmd{}", ed.list.len() + 1), help: String::new(), template: "select *\nfrom {table}\nlimit {n=50};".into() });
                                ed.selected = ed.list.len() - 1;
                            }
                            if ui.add_enabled(!ed.list.is_empty(), Button::new("Delete")).clicked() {
                                ed.list.remove(ed.selected);
                                ed.selected = ed.selected.min(ed.list.len().saturating_sub(1));
                            }
                        });
                    });
                    // Not a separator: in a horizontal row it stretches to
                    // the full available height and blows up the modal.
                    ui.add_space(24.0);
                    // Details: name and description on one row, then the
                    // template and its preview side by side.
                    ui.vertical(|ui| {
                        let width = ui.available_width();
                        let Some(c) = ed.list.get_mut(ed.selected) else {
                            muted(ui, "No commands. Add one, or reset to the defaults.");
                            return;
                        };
                        let half = (width - 2.0 * GAP) / 2.0;
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0 * GAP;
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                field_label(ui, "Name");
                                ui.add(TextEdit::singleline(&mut c.name).font(TextStyle::Monospace).desired_width(half).margin(Margin::symmetric(8, 6)));
                            });
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                field_label(ui, "Description");
                                ui.add(TextEdit::singleline(&mut c.help).desired_width(half).margin(Margin::symmetric(8, 6)));
                            });
                        });
                        ui.add_space(GAP);
                        let line = format!("/{} {}", c.name, ed.preview_args);
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0 * GAP;
                            // Left: the template.
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                field_label(ui, "SQL template");
                                let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                                    let mut job = highlight::sql(buf.as_str(), TextStyle::Monospace.resolve(ui.style()));
                                    job.wrap.max_width = wrap;
                                    ui.fonts_mut(|f| f.layout_job(job))
                                };
                                egui::ScrollArea::vertical().id_salt("cmd-template").max_height(PANE_H).auto_shrink([false, true]).show(ui, |ui| {
                                    ui.add(
                                        TextEdit::multiline(&mut c.template)
                                            .code_editor()
                                            .desired_rows(18)
                                            .desired_width(half)
                                            .margin(Margin::symmetric(8, 6))
                                            .layouter(&mut layouter),
                                    );
                                });
                                ui.add_space(6.0);
                                // Placeholders as chips that wrap cleanly.
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                                    for (token, what) in [
                                        ("{name}", "Your own argument, any name: pasted in as typed. Arguments go in the order the placeholders appear; the last one takes the rest of the line"),
                                        ("{name=x}", "Your own argument with a default"),
                                        ("{table}", "The table argument (schema-qualified if needed)"),
                                        ("{field}", "Geometry column: the argument, else the table's geometry column, else shape"),
                                        ("{field+_4326}", "The field name plus a suffix, quoted as needed (for aliases)"),
                                        ("{n}", "A number argument (required)"),
                                        ("{n=100}", "A number argument with a default"),
                                    ] {
                                        Frame::new().fill(theme::HOVER()).corner_radius(5).inner_margin(Margin::symmetric(6, 2)).show(ui, |ui| {
                                            ui.label(RichText::new(token).monospace().size(11.5).color(theme::MUTED()));
                                        })
                                        .response
                                        .on_hover_text(what);
                                    }
                                });
                            });
                            // Right: try it with arguments, see the SQL.
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                field_label(ui, "Preview");
                                Frame::new().fill(theme::BG()).stroke(Stroke::new(1.0_f32, theme::BORDER())).corner_radius(8).inner_margin(Margin::symmetric(8, 5)).show(ui, |ui| {
                                    ui.set_width(half - 18.0);
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        ui.label(RichText::new(format!("/{} ", c.name)).monospace().color(theme::MUTED()));
                                        ui.add(
                                            TextEdit::singleline(&mut ed.preview_args)
                                                .hint_text(hint(&c.args()))
                                                .font(TextStyle::Monospace)
                                                .frame(false)
                                                .margin(Margin::ZERO)
                                                .desired_width(f32::INFINITY),
                                        );
                                    });
                                });
                                let preview = match c.problem() {
                                    Some(p) => Err(p),
                                    None => commands::preview_with(std::slice::from_ref(c), &line, &catalog, "public"),
                                };
                                ui.add_space(6.0);
                                Frame::new().fill(theme::BG()).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
                                    ui.set_width(half - 20.0);
                                    egui::ScrollArea::vertical().id_salt("cmd-preview").max_height(PANE_H - 44.0).auto_shrink([false, false]).show(ui, |ui| {
                                        match &preview {
                                            Ok(sql) => {
                                                let mut job = highlight::sql(sql, TextStyle::Monospace.resolve(ui.style()));
                                                job.wrap.max_width = ui.available_width();
                                                ui.add(egui::Label::new(job).selectable(true));
                                            }
                                            Err(e) => {
                                                ui.label(RichText::new(e).monospace().size(12.5).color(theme::WARN()));
                                            }
                                        }
                                    });
                                });
                                // Free placeholders still waiting for a value.
                                if let Ok(sql) = &preview {
                                    let missing: Vec<String> = c.args().split_whitespace().filter(|a| a.starts_with('<')).map(|a| a.trim_matches(['<', '>']).to_string()).filter(|n| sql.contains(&format!("{{{n}}}"))).collect();
                                    if !missing.is_empty() {
                                        ui.add_space(4.0);
                                        ui.label(RichText::new(format!("Waiting for: {}", missing.join(", "))).size(12.0).color(theme::WARN()));
                                    }
                                }
                            });
                        });
                    });
                });
                if let Some(e) = &ed.error {
                    ui.add_space(8.0);
                    ui.label(RichText::new(e).color(theme::ERROR()));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Reset to defaults").on_hover_text("Restore the built-in commands").clicked() {
                        reset = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(Button::new(RichText::new("Save").color(theme::BG())).fill(theme::ACCENT())).clicked() {
                            save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if reset {
            ed.list = commands::defaults();
            ed.selected = 0;
            ed.error = None;
        }
        if save {
            let mut seen = HashSet::new();
            let problem = ed.list.iter().find_map(|c| {
                c.problem().or_else(|| (!seen.insert(c.name.clone())).then(|| format!("/{} is defined twice", c.name)))
            });
            ed.error = problem;
            if ed.error.is_none() {
                // Saving exactly the defaults forgets the file, so later
                // versions' new built-ins still show up.
                let result = if ed.list == commands::defaults() { config::reset_commands() } else { config::save_commands(&ed.list) };
                match result {
                    Ok(()) => {
                        commands::set_active(ed.list.clone());
                        self.toast = Some(("Slash commands saved".into(), theme::OK(), Instant::now()));
                        close = true;
                    }
                    Err(e) => ed.error = Some(format!("Could not save: {e}")),
                }
            }
        }
        if close || modal.should_close() {
            self.commands_editor = None;
        }
    }

    /// Run or save what the confirmation was asking about.
    fn accept_confirm(&mut self) {
        let Some(c) = self.confirm.take() else { return };
        let Some(ti) = self.tabs.iter().position(|t| t.id == c.tab) else { return };
        self.skip_confirm = true;
        self.tab_action(ti, match c.kind {
            ConfirmKind::Run { all } => TabAction::Run { all },
            ConfirmKind::Save => TabAction::SaveEdits,
            ConfirmKind::AddKey => TabAction::AddKey,
        });
        self.skip_confirm = false;
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.confirm else { return };
        match confirm_modal(ctx, c) {
            Some(true) => self.accept_confirm(),
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

/// "Run on <connection>?" with the risky statements and why. `Some(true)`:
/// run anyway; `Some(false)`: cancelled. Cancel has the focus, so a stray
/// Enter doesn't run anything.
fn confirm_modal(ctx: &egui::Context, c: &mut Confirm) -> Option<bool> {
    {
        let (mut go, mut cancel) = (false, false);
        let modal = egui::Modal::new(Id::new("risk-confirm"))
            .frame(
                Frame::new()
                    .fill(theme::SURFACE())
                    .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                    .corner_radius(12)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(480.0);
                ui.label(RichText::new(&c.title).heading().color(theme::TEXT()));
                ui.add_space(10.0);
                egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (sql, why, danger) in &c.items {
                        ui.label(RichText::new(why).size(12.0).strong().color(if *danger { theme::ERROR() } else { theme::WARN() }));
                        // Long statements are cut: the reason is what matters here.
                        let shown: String = sql.lines().take(4).collect::<Vec<_>>().join("\n");
                        let cut = shown.chars().count() > 300 || sql.lines().count() > 4;
                        let shown = if cut { format!("{}…", shown.chars().take(300).collect::<String>()) } else { shown };
                        ui.add(egui::Label::new(RichText::new(shown).monospace().size(12.0).color(theme::MUTED())).wrap());
                        ui.add_space(8.0);
                    }
                });
                ui.add_space(10.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let danger = c.items.iter().any(|i| i.2);
                    let fill = if danger { theme::ERROR() } else { theme::ACCENT() };
                    if ui.add(Button::new(RichText::new("Run anyway  ⌘↩").color(theme::BG())).fill(fill)).clicked() {
                        go = true;
                    }
                    let cancel_button = ui.button("Cancel");
                    if !c.focused {
                        cancel_button.request_focus();
                        c.focused = true;
                    }
                    if cancel_button.clicked() {
                        cancel = true;
                    }
                });
            });
        if go {
            Some(true)
        } else if cancel || modal.should_close() {
            Some(false)
        } else {
            None
        }
    }
}

impl App {
    fn name_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.name_prompt else { return };
        let title = match prompt.target {
            NameTarget::NewFrom(_) => "Save query",
            NameTarget::Rename(_) => "Rename query",
        };
        let mut done = false;
        let mut cancel = false;
        let modal = egui::Modal::new(Id::new("name-prompt"))
            .frame(
                Frame::new()
                    .fill(theme::SURFACE())
                    .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                    .corner_radius(12)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(RichText::new(title).heading().color(theme::TEXT()));
                ui.add_space(12.0);
                let edit = ui.add(
                    TextEdit::singleline(&mut prompt.name)
                        .hint_text(hint("Name"))
                        .desired_width(340.0 - 16.0)
                        .margin(Margin::symmetric(8, 6)),
                );
                if !edit.has_focus() && !edit.lost_focus() {
                    edit.request_focus();
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    done = true;
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(Button::new(RichText::new("Save").color(theme::BG())).fill(theme::ACCENT())).clicked() {
                            done = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        let name = prompt.name.trim().to_string();
        if done && !name.is_empty() {
            let Some(prompt) = self.name_prompt.take() else { return };
            let result = match prompt.target {
                NameTarget::NewFrom(tab_id) => {
                    let tab = self.tabs.iter_mut().find(|t| t.id == tab_id);
                    match tab.and_then(|t| match &mut t.kind {
                        TabKind::Console(c) => Some((&mut t.title, c, t.conn.clone())),
                        TabKind::Table(_) | TabKind::Layout(_) | TabKind::Diagram(_) => None,
                    }) {
                        Some((title, c, conn)) => config::create_query(&name, &c.sql, Some(conn), c.database.clone(), c.picked_schemas()).map(|q| {
                            c.saved_id = Some(q.id.clone());
                            *title = q.name.clone();
                            q.name
                        }),
                        None => Err("The console was closed".into()),
                    }
                }
                NameTarget::Rename(id) => match self.saved.iter().find(|q| q.id == id).cloned() {
                    Some(q) => config::rename_query(&q, &name).map(|r| {
                        self.relink(&id, Some((&r.id, &r.name)));
                        r.name
                    }),
                    None => Err("The query no longer exists".into()),
                },
            };
            match result {
                Ok(saved_as) => self.toast = Some((format!("Saved “{saved_as}”"), theme::OK(), Instant::now())),
                Err(e) => self.toast = Some((e, theme::ERROR(), Instant::now())),
            }
            self.reload_queries();
        } else if cancel || modal.should_close() {
            self.name_prompt = None;
        }
    }
}

// ---- connection dialog ------------------------------------------------

impl App {
    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.dialog else { return };
        let mut test = false;
        let mut save = false;
        let mut cancel = false;

        let modal = egui::Modal::new(Id::new("conn-dialog"))
            .frame(
                Frame::new()
                    .fill(theme::SURFACE())
                    .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                    .corner_radius(12)
                    .inner_margin(Margin::same(22)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(DIALOG_WIDTH);
                ui.set_max_width(DIALOG_WIDTH);
                ui.label(
                    RichText::new(if d.is_new { "New connection" } else { "Edit connection" })
                        .heading()
                        .color(theme::TEXT()),
                );
                ui.label(RichText::new("PostgreSQL").small().color(theme::MUTED()));
                ui.add_space(14.0);

                ui.horizontal(|ui| {
                    for (tab, label) in [(DialogTab::General, "General"), (DialogTab::Ssl, "SSL"), (DialogTab::Ssh, "SSH")] {
                        let mut text = label.to_string();
                        if tab == DialogTab::Ssh && d.cfg.ssh.enabled {
                            text += " ●";
                        }
                        if segment(ui, d.tab == tab, &text) {
                            d.tab = tab;
                        }
                    }
                });
                ui.add_space(10.0);
                // Fixed-size body (fits the tallest tab) so the modal doesn't
                // jump around when switching tabs or when a test result shows.
                let body = egui::vec2(DIALOG_WIDTH, 350.0);
                ui.allocate_ui_with_layout(body, Layout::top_down(Align::Min), |ui| {
                    ui.set_min_size(body);
                    match d.tab {
                        DialogTab::General => general_form(ui, d),
                        DialogTab::Ssl => ssl_form(ui, d),
                        DialogTab::Ssh => ssh_form(ui, d),
                    }
                });
                let status = egui::vec2(DIALOG_WIDTH, 44.0);
                ui.allocate_ui_with_layout(status, Layout::top_down(Align::Min), |ui| {
                    ui.set_min_size(status);
                    status_line(ui, d);
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!d.testing, Button::new("Test connection")).clicked() {
                        test = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(Button::new(RichText::new("Save").color(theme::BG())).fill(theme::ACCENT()))
                            .clicked()
                        {
                            save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if test || save {
            match (d.port.trim().parse::<u16>(), d.ssh_port.trim().parse::<u16>()) {
                (Ok(p), Ok(sp)) => {
                    d.cfg.port = p;
                    d.cfg.ssh.port = sp;
                }
                _ => {
                    d.test = Some(Err("Ports must be numbers between 1 and 65535".into()));
                    return;
                }
            }
        }
        if test {
            d.testing = true;
            d.test = None;
            self.worker.send(Cmd::Test(d.cfg.clone()));
        }
        if save {
            let cfg = d.cfg.clone();
            self.dialog = None;
            match self.conn_index(&cfg.id) {
                Some(ci) => {
                    let was_connected = !matches!(self.conns[ci].status, Status::Disconnected);
                    self.conns[ci].cfg = cfg;
                    if was_connected {
                        self.apply(Action::Disconnect(ci));
                        self.apply(Action::Connect(ci));
                    }
                }
                None => self.conns.push(Conn::new(cfg)),
            }
            self.save_config();
        } else if cancel || modal.should_close() {
            self.dialog = None;
        }
    }
}

const DIALOG_WIDTH: f32 = 400.0;

/// Test result under the form; long errors wrap within the reserved space.
fn status_line(ui: &mut Ui, d: &Dialog) {
    match &d.test {
        _ if d.testing => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                muted(ui, "testing…");
            });
        }
        Some(Ok(v)) => {
            ui.label(RichText::new(format!("✓  Connected · {v}")).color(theme::OK()));
        }
        Some(Err(e)) => {
            egui::ScrollArea::vertical().max_height(44.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(e).small().color(theme::ERROR())).wrap());
            });
        }
        None => {}
    }
}

const LABEL_WIDTH: f32 = 96.0;
const FIELD_WIDTH: f32 = DIALOG_WIDTH - LABEL_WIDTH - 14.0;
/// Horizontal text-field padding; `desired_width` excludes it.
const FIELD_PAD: i8 = 8;

fn form_grid(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui)) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([14.0, 9.0])
        .min_col_width(LABEL_WIDTH)
        .max_col_width(FIELD_WIDTH)
        .show(ui, add);
}

fn field(ui: &mut Ui, label: &str, edit: TextEdit<'_>) {
    ui.label(RichText::new(label).color(theme::MUTED()));
    let inner = FIELD_WIDTH - 2.0 * FIELD_PAD as f32;
    ui.add(edit.desired_width(inner).margin(Margin::symmetric(FIELD_PAD, 5)));
    ui.end_row();
}

/// Path field with a "…" button that opens a native file picker.
fn file_field(ui: &mut Ui, label: &str, path: &mut String, placeholder: &str) {
    ui.label(RichText::new(label).color(theme::MUTED()));
    ui.horizontal(|ui| {
        let button_w = 28.0;
        let w = FIELD_WIDTH - button_w - ui.spacing().item_spacing.x - 2.0 * FIELD_PAD as f32;
        ui.add(
            TextEdit::singleline(path)
                .hint_text(hint(placeholder))
                .desired_width(w)
                .margin(Margin::symmetric(FIELD_PAD, 5)),
        );
        if ui.add_sized([button_w, 26.0], Button::new("…")).on_hover_text("Choose file").clicked() {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = std::path::Path::new(path.as_str()).parent().filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            if let Some(file) = dialog.pick_file() {
                *path = file.display().to_string();
            }
        }
    });
    ui.end_row();
}

fn note(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(theme::FAINT())).wrap());
}

const COLOR_HINT: &str = "Tint this connection's tabs (e.g. red for production)";

fn general_form(ui: &mut Ui, d: &mut Dialog) {
    form_grid(ui, "conn-url", |ui| {
        let edit = TextEdit::singleline(&mut d.url).hint_text(hint("postgres://user:pass@host:5432/db"));
        ui.label(RichText::new("URL").color(theme::MUTED()));
        let inner = FIELD_WIDTH - 2.0 * FIELD_PAD as f32;
        let resp = ui.add(edit.desired_width(inner).margin(Margin::symmetric(FIELD_PAD, 5)));
        ui.end_row();
        if resp.changed() {
            d.url_status = (!d.url.trim().is_empty()).then(|| {
                let mut cfg = d.cfg.clone();
                rgrip_core::conn_string::apply(&mut cfg, &d.url).map(|()| {
                    d.port = cfg.port.to_string();
                    d.cfg = cfg;
                })
            });
        }
    });
    match &d.url_status {
        Some(Ok(())) => note(ui, "Filled in from the connection string."),
        Some(Err(e)) => {
            ui.label(RichText::new(e).size(11.5).color(theme::ERROR()));
        }
        None => note(ui, "Paste a postgres:// URL, key=value string or jdbc:postgresql:// URL."),
    }
    ui.add_space(6.0);
    form_grid(ui, "conn-general", |ui| {
        let placeholder = format!("{}@{}", d.cfg.database, d.cfg.host);
        field(ui, "Name", TextEdit::singleline(&mut d.cfg.name).hint_text(hint(&placeholder)));
        field(ui, "Host", TextEdit::singleline(&mut d.cfg.host));
        field(ui, "Port", TextEdit::singleline(&mut d.port));
        field(ui, "Database", TextEdit::singleline(&mut d.cfg.database));
        field(ui, "User", TextEdit::singleline(&mut d.cfg.user));
        field(ui, "Password", TextEdit::singleline(&mut d.cfg.password).password(true));
        ui.label(RichText::new("Color").color(theme::MUTED())).on_hover_text(COLOR_HINT);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for c in ConnColor::ALL {
                let resp = widgets::color_swatch(ui, theme::conn_color(c), d.cfg.color == c).on_hover_text(format!("{}: {COLOR_HINT}", c.label()));
                if resp.clicked() {
                    d.cfg.color = c;
                }
            }
        });
        ui.end_row();
        ui.label(RichText::new("Read-only").color(theme::MUTED()));
        ui.checkbox(&mut d.cfg.read_only, "Postgres rejects writes on this connection");
        ui.end_row();
    });
    if d.cfg.ssh.enabled {
        ui.add_space(6.0);
        note(ui, "Host and port are as seen from the SSH server.");
    }
}

fn ssl_form(ui: &mut Ui, d: &mut Dialog) {
    form_grid(ui, "conn-ssl", |ui| {
        ui.label(RichText::new("Mode").color(theme::MUTED()));
        egui::ComboBox::from_id_salt("ssl-mode")
            .selected_text(d.cfg.ssl.label())
            .width(160.0)
            .show_ui(ui, |ui| {
                for m in SslMode::ALL {
                    ui.selectable_value(&mut d.cfg.ssl, m, m.label());
                }
            });
        ui.end_row();
        file_field(ui, "CA certificate", &mut d.cfg.ssl_root_cert, "system roots");
        file_field(ui, "Client cert", &mut d.cfg.ssl_client_cert, "optional (.crt / .pem)");
        file_field(ui, "Client key", &mut d.cfg.ssl_client_key, "optional (.key / .pem)");
    });
    ui.add_space(8.0);
    note(
        ui,
        match d.cfg.ssl {
            SslMode::Disable => "No encryption.",
            SslMode::Prefer => "Encrypt if the server supports it; the certificate is not checked.",
            SslMode::Require => "Always encrypt; the certificate is not checked.",
            SslMode::VerifyCa => "Encrypt and check the certificate is signed by the CA (host name not checked).",
            SslMode::VerifyFull => "Encrypt, check the CA and that the certificate matches the host name.",
        },
    );
}

fn ssh_form(ui: &mut Ui, d: &mut Dialog) {
    let ssh = &mut d.cfg.ssh;
    ui.checkbox(&mut ssh.enabled, "Connect through an SSH tunnel");
    ui.add_space(8.0);
    ui.add_enabled_ui(ssh.enabled, |ui| {
        form_grid(ui, "conn-ssh", |ui| {
            field(ui, "SSH host", TextEdit::singleline(&mut ssh.host).hint_text(hint("bastion.example.com")));
            field(ui, "Port", TextEdit::singleline(&mut d.ssh_port));
            field(ui, "User", TextEdit::singleline(&mut ssh.user));
            ui.label(RichText::new("Auth").color(theme::MUTED()));
            ui.horizontal(|ui| {
                ui.selectable_value(&mut ssh.auth, SshAuth::Key, "Key file");
                ui.selectable_value(&mut ssh.auth, SshAuth::Password, "Password");
            });
            ui.end_row();
            match ssh.auth {
                SshAuth::Password => {
                    field(ui, "Password", TextEdit::singleline(&mut ssh.password).password(true));
                }
                SshAuth::Key => {
                    file_field(ui, "Private key", &mut ssh.key_path, "~/.ssh/id_ed25519");
                    field(
                        ui,
                        "Passphrase",
                        TextEdit::singleline(&mut ssh.passphrase).password(true).hint_text(hint("if the key has one")),
                    );
                }
            }
        });
        ui.add_space(8.0);
        note(ui, "Host keys are checked against ~/.ssh/known_hosts; a changed key is refused.");
    });
}

// ---- small widgets ----------------------------------------------------

fn empty_state(ui: &mut Ui, actions: &mut Vec<Action>, no_connections: bool) {
    centered(ui, |ui| {
        ui.label(RichText::new("rgrip").size(28.0).color(theme::FAINT()));
        ui.add_space(6.0);
        if no_connections {
            muted(ui, "Add a PostgreSQL connection to get started.");
            ui.add_space(10.0);
            if ui
                .add(Button::new(RichText::new("New connection").color(theme::BG())).fill(theme::ACCENT()))
                .clicked()
            {
                actions.push(Action::NewConnection);
            }
        } else {
            muted(ui, "Open a connection on the left to start a console.");
            muted(ui, "Double-click a table to browse its data.");
        }
    });
}

fn toolbar(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .inner_margin(Margin::symmetric(12, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(24.0);
                add(ui);
            });
        });
    let rect = ui.available_rect_before_wrap();
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0_f32, theme::BORDER()));
}

fn segment(ui: &mut Ui, selected: bool, text: &str) -> bool {
    let color = if selected { theme::TEXT() } else { theme::MUTED() };
    let r = ui.add(
        Button::new(RichText::new(text).color(color))
            .fill(if selected { theme::SELECTED() } else { Color32::TRANSPARENT })
            .frame_when_inactive(selected),
    );
    r.clicked()
}

/// A full-width, left-aligned selectable list row (monospace).
fn list_row(ui: &mut Ui, text: &str, selected: bool, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::click());
    let fill = if selected { theme::SELECTED() } else if resp.hovered() { theme::HOVER() } else { Color32::TRANSPARENT };
    ui.painter().rect_filled(rect, 6.0, fill);
    let color = if selected { theme::TEXT() } else { theme::MUTED() };
    let font = TextStyle::Monospace.resolve(ui.style());
    ui.painter().text(rect.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, text, font, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A 24 pt square title-bar button; `paint` draws its icon (hot = hovered).
fn title_button(ui: &mut Ui, hover: &str, paint: impl FnOnce(&Ui, egui::Rect, bool)) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), Sense::click());
    let hot = resp.hovered();
    if hot {
        ui.painter().rect_filled(rect, 6.0, theme::HOVER());
    }
    paint(ui, rect, hot);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(hover)
}

/// Small borderless button that brightens on hover.
fn icon_button(ui: &mut Ui, text: &str) -> egui::Response {
    let resp = ui.add(
        Button::new(RichText::new(text).size(14.0).color(theme::MUTED()))
            .frame(false)
            .min_size(egui::vec2(18.0, 18.0)),
    );
    if resp.hovered() {
        ui.painter().rect_filled(resp.rect, 4.0, theme::HOVER());
        ui.painter().text(resp.rect.center(), egui::Align2::CENTER_CENTER, text, FontId::proportional(14.0), theme::TEXT());
    }
    resp
}

fn ghost_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(Button::new(RichText::new(text).color(theme::MUTED())).frame(false))
}

fn centered(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.with_layout(Layout::centered_and_justified(egui::Direction::TopDown), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 2.0 - 60.0).max(0.0));
            add(ui);
        });
    });
}

fn hint(text: &str) -> RichText {
    RichText::new(text).color(theme::FAINT())
}

/// `12345` as `12,345`.
fn group_thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

/// Map in a resizable panel on the right, grid in the rest. Both get the
/// shared grid state: the map runs first so a click on it shows in the grid
/// in the same frame; a grid click shows on the map by repainting.
fn split_view(ui: &mut Ui, id: Id, state: &mut GridState, map: impl FnOnce(&mut Ui, &mut GridState), grid: impl FnOnce(&mut Ui, &mut GridState)) {
    let w = ui.available_width();
    egui::SidePanel::right(id.with("map"))
        .resizable(true)
        .default_width(w * 0.5)
        .width_range(240.0..=(w - 240.0).max(240.0))
        .frame(Frame::NONE)
        .show_inside(ui, |ui| map(ui, state));
    let before = state.selection.clone();
    egui::CentralPanel::default().frame(Frame::NONE).show_inside(ui, |ui| grid(ui, state));
    if state.selection != before {
        ui.ctx().request_repaint();
    }
}

fn muted(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(theme::MUTED()));
}


#[cfg(test)]
mod tests {
    //! Drives the real console UI headlessly with keyboard events.
    use super::*;
    use rgrip_core::complete::CatalogTable;

    #[test]
    fn console_title_follows_database() {
        let cfg = ConnectionConfig { name: "prod".into(), database: "app".into(), ..Default::default() };
        assert_eq!(console_title("prod@app", &cfg, "other").as_deref(), Some("prod@other"));
        assert_eq!(console_title("prod@app (2)", &cfg, "other").as_deref(), Some("prod@other (2)"));
        assert_eq!(console_title("my saved query", &cfg, "other"), None);
        assert_eq!(console_title("prod@app2", &cfg, "other"), None);
    }

    #[test]
    fn thousands_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(12345), "12,345");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }

    #[test]
    fn related_popup() {
        let s = |v: &str| Some(v.to_string());
        let rows = |n: usize| ResultSet {
            columns: vec!["id".into(), "user_id".into()],
            rows: (0..n).map(|i| vec![Some(i.to_string()), s("5")]).collect(),
            ..Default::default()
        };
        let mut t = TableView::new("public".into(), "users".into(), String::new());
        t.data = Some(Ok(rows(1)));
        let target = |label: &str| RelatedTarget {
            schema: "public".into(),
            table: "posts".into(),
            filter: r#""user_id" = '5'"#.into(),
            label: label.into(),
        };
        t.related = Some(Related {
            target: target("← posts (user_id)"),
            back: vec![target("previous")],
            data: Some(Ok(rows(3))),
            total: Some(3),
            relations: Vec::new(),
            columns: Vec::new(),
            fk_relations: Vec::new(),
            grid: GridState::default(),
            probe: None,
        });
        let ctx = egui::Context::default();
        let mut frame = |t: &mut TableView, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            let mut action = None;
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| action = table_ui(ui, 1, t, false));
            });
            action
        };
        frame(&mut t, vec![]);
        frame(&mut t, vec![]);
        // The popup is up, over the tab, with its rows.
        let modal = ctx.memory(|m| m.area_rect(Id::new(("related", 1u64)))).expect("modal shown");
        assert!(modal.width() > 900.0 && modal.height() > 400.0, "{modal:?}");
        // Esc closes it.
        let esc = egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame(&mut t, vec![esc]);
        assert!(t.related.is_none());
    }

    #[test]
    fn edit_key_falls_back_to_unique_columns() {
        let col = |name: &str, pk: bool| ColumnInfo { name: name.into(), data_type: "integer".into(), nullable: !pk, default: None, primary_key: pk };
        let catalog = Catalog {
            tables: vec![CatalogTable {
                schema: "mlmupc".into(),
                name: "parcel".into(),
                kind: TableKind::Table,
                columns: Vec::new(),
                keys: vec!["globalid".into(), "objectid".into(), "id".into()],
            }],
        };
        assert_eq!(edit_key("mlmupc", "parcel", &[col("objectid", false)], &catalog), ["objectid"]);
        assert_eq!(edit_key("mlmupc", "parcel", &[col("code", true)], &catalog), ["code"], "a primary key wins");
        assert!(edit_key("mlmupc", "other", &[col("x", false)], &catalog).is_empty());
        let named = ConnectionConfig { name: "CIS Prod".into(), database: "cis".into(), host: "db".into(), ..Default::default() };
        let unnamed = ConnectionConfig { database: "cis_next".into(), host: "cis-db2".into(), ..Default::default() };
        assert_eq!(conn_db_title(&named, "cis"), "CIS Prod@cis");
        assert_eq!(conn_db_title(&unnamed, "cis_next"), "cis_next@cis-db2");
        assert_eq!(console_title("cis_next@cis-db2 (2)", &unnamed, "other").as_deref(), Some("other@cis-db2 (2)"));
    }

    #[test]
    fn explorer_type_ahead_match() {
        let rows: Vec<(String, String)> = ["public", "customers", "order_items", "orders", "Accounts"].iter().map(|l| (format!("k/{l}"), l.to_string())).collect();
        assert_eq!(type_ahead_match(&rows, "o"), Some(2), "first prefix match in tree order");
        assert_eq!(type_ahead_match(&rows, "orders"), Some(3), "a longer buffer narrows it");
        assert_eq!(type_ahead_match(&rows, "acc"), Some(4), "ignores case");
        assert_eq!(type_ahead_match(&rows, "item"), Some(2), "falls back to a substring");
        assert_eq!(type_ahead_match(&rows, "zzz"), None);
    }

    #[test]
    fn sticky_explorer_headers() {
        let h = |depth, top: f32| StickyHeader { depth, key: String::new(), icon: Icon::Schema, label: String::new(), detail: None, top };
        // conn at 0, schema A at 25 (rows to 500), schema B at 500, conn2 at 900.
        let headers = [h(0, 0.0), h(1, 25.0), h(1, 500.0), h(0, 900.0)];
        let row = 25.0;
        assert!(sticky_headers(&headers, -1.0, row).is_empty(), "nothing scrolled yet");
        assert_eq!(sticky_headers(&headers, 200.0, row), [0, 1], "inside schema A");
        assert_eq!(sticky_headers(&headers, 600.0, row), [0, 2], "inside schema B");
        assert_eq!(sticky_headers(&headers, 860.0, row), [0], "schema B nearly done: only the connection");
        assert_eq!(sticky_headers(&headers, 1000.0, row), [3], "second connection");
    }

    #[test]
    fn related_rows_filter() {
        let s = |v: &str| Some(v.to_string());
        let users = ResultSet {
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![s("5"), s("a")], vec![s("7"), s("b")]],
            ..Default::default()
        };
        let rel = |outgoing, table: &str, cols: &[(&str, &str)]| Relation {
            name: "fk".into(),
            outgoing,
            other_schema: "public".into(),
            other_table: table.into(),
            columns: cols.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            inferred: false,
            alternatives: Vec::new(),
        };
        // users ← posts.user_id: posts whose user_id is one of these ids.
        let posts = rel(false, "posts", &[("id", "user_id")]);
        assert_eq!(related_filter(&posts, &users, &[0]).as_deref(), Some(r#""user_id" = '5'"#));
        assert_eq!(related_filter(&posts, &users, &[0, 1]).as_deref(), Some(r#""user_id" in ('5', '7')"#));
        // posts.user_id → users, with a NULL user_id: nothing to open.
        let post_rows = ResultSet {
            columns: vec!["id".into(), "user_id".into()],
            rows: vec![vec![s("1"), None], vec![s("2"), s("7")]],
            ..Default::default()
        };
        let user = rel(true, "users", &[("user_id", "id")]);
        assert_eq!(related_filter(&user, &post_rows, &[0]), None);
        assert_eq!(related_filter(&user, &post_rows, &[0, 1]).as_deref(), Some(r#""id" = '7'"#));
        // A key column that isn't in the result.
        assert_eq!(related_filter(&rel(true, "x", &[("nope", "id")]), &users, &[0]), None);
        // An inferred link that may use either key matches both.
        let parcels = ResultSet {
            columns: vec!["id".into(), "land_right".into()],
            rows: vec![vec![s("p1"), s("{6726-AB}")]],
            ..Default::default()
        };
        let mut lr = rel(true, "land_right", &[("land_right", "id")]);
        lr.alternatives = vec![("land_right".into(), "globalid".into())];
        assert_eq!(related_filter(&lr, &parcels, &[0]).as_deref(), Some(r#"("id" = '{6726-AB}') or ("globalid" = '{6726-AB}')"#));
        // Counting: one probe per variant; following keeps the variants that matched.
        let Some(TabAction::Probe { probes, map, .. }) = probe_action(std::slice::from_ref(&lr), &parcels, vec![0], false) else { panic!("probe") };
        assert_eq!(map, [(0, 0), (0, 1)]);
        assert_eq!(probes[1], ("public".into(), "land_right".into(), r#""globalid" = '{6726-AB}'"#.into()));
        let probe = Probe { rows: vec![0], map, counts: vec![Some(0), Some(1)] };
        assert_eq!(probe.best(&[0], 1), [Some(1)]);
        assert_eq!(probe.matched(&[0], 0), [1]);
        assert!(probe.best(&[5], 1).is_empty(), "counts are for other rows");
        assert_eq!(related_filter_only(&lr, &parcels, &[0], &probe.matched(&[0], 0)).as_deref(), Some(r#""globalid" = '{6726-AB}'"#));
    }

    #[test]
    fn risky_items_by_connection() {
        let sql = "select 1; insert into t values (1); delete from t";
        let dev = risky_items(sql, false);
        assert_eq!(dev.len(), 1, "only the unfiltered delete is dangerous off production");
        assert!(dev[0].2 && dev[0].0 == "delete from t");
        let prod = risky_items(sql, true);
        assert_eq!(prod.len(), 2);
        assert!(!prod[0].2 && prod[0].1.contains("production"));
        assert!(risky_items("select * from t", true).is_empty());
    }

    fn confirm_frame(ctx: &egui::Context, c: &mut Confirm, events: Vec<egui::Event>) -> Option<bool> {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), events, ..Default::default() };
        let mut out = None;
        let _ = ctx.run(input, |ctx| out = confirm_modal(ctx, c));
        out
    }

    #[test]
    fn confirm_modal_cancels_with_escape() {
        let ctx = egui::Context::default();
        let items = risky_items("delete from t", false);
        let mut c = Confirm { tab: 1, kind: ConfirmKind::Run { all: false }, title: "Run on prod?".into(), items, focused: false };
        assert_eq!(confirm_frame(&ctx, &mut c, vec![]), None);
        assert_eq!(confirm_frame(&ctx, &mut c, vec![]), None, "waits for a choice");
        let esc = egui::Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        assert_eq!(confirm_frame(&ctx, &mut c, vec![esc]), Some(false));
    }

    #[test]
    fn read_only_console_and_table_render() {
        let ctx = egui::Context::default();
        let mut console = Console::new();
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), ..Default::default() };
        let _ = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                console_ui(ui, 1, &mut console, &Catalog { tables: Vec::new() }, &[], &[], "db", false, true);
            });
        });
        let _ = ctx.run(input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| read_only_badge(ui));
        });
    }

    struct Harness {
        ctx: egui::Context,
        console: Console,
        catalog: Catalog,
    }

    impl Harness {
        fn new() -> Self {
            let table = |name: &str, cols: &[&str]| CatalogTable {
                schema: "public".into(),
                name: name.into(),
                kind: TableKind::Table,
                columns: cols.iter().map(|c| (c.to_string(), "text".into())).collect(),
                keys: Vec::new(),
            };
            let mut h = Self {
                ctx: egui::Context::default(),
                console: Console::new(),
                catalog: Catalog {
                    tables: vec![table("users", &["id", "email"]), table("orders", &["id", "total"])],
                },
            };
            h.frame(vec![]);
            h.frame(vec![]);
            h
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            let (console, catalog) = (&mut self.console, &self.catalog);
            let _ = self.ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    console_ui(ui, 1, console, catalog, &[], &[], "db", false, false);
                });
            });
        }

        /// One frame per character, like real typing.
        fn type_text(&mut self, text: &str) {
            for ch in text.chars() {
                self.frame(vec![egui::Event::Text(ch.to_string())]);
            }
        }

        fn key(&mut self, key: Key, modifiers: Modifiers) {
            self.frame(vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        }

        fn labels(&self) -> Vec<String> {
            self.console
                .popup
                .as_ref()
                .map(|p| p.completion.items.iter().map(|i| i.label.clone()).collect())
                .unwrap_or_default()
        }
    }

    fn tab(id: TabId) -> Tab {
        Tab { id, conn: "c".into(), title: format!("tab {id}"), kind: TabKind::Console(Console::new()) }
    }

    /// Run the tab bar for one frame with `events`; returns tab rects and actions.
    fn tab_frame(ctx: &egui::Context, tabs: &[Tab], events: Vec<egui::Event>) -> (Vec<egui::Rect>, Vec<Action>) {
        let mut rects = Vec::new();
        let mut actions = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0))),
            events,
            ..Default::default()
        };
        let style = TabStyle { title: None, color: theme::conn_color(ConnColor::Red), tip: "Prod · h:1/db".into(), status: theme::FAINT() };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, t) in tabs.iter().enumerate() {
                        rects.push(tab_button(ui, i, t, &style, i == 0, false, tabs.len(), &mut actions));
                    }
                });
            });
        });
        (rects, actions)
    }

    fn click(ctx: &egui::Context, tabs: &[Tab], pos: egui::Pos2, button: egui::PointerButton) -> Vec<Action> {
        let press = |pressed| egui::Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
        tab_frame(ctx, tabs, vec![egui::Event::PointerMoved(pos)]);
        let mut actions = tab_frame(ctx, tabs, vec![press(true)]).1;
        actions.extend(tab_frame(ctx, tabs, vec![press(false)]).1);
        actions.extend(tab_frame(ctx, tabs, vec![]).1);
        actions
    }

    fn closes(actions: &[Action]) -> Vec<CloseWhich> {
        actions.iter().filter_map(|a| if let Action::Close(w) = a { Some(*w) } else { None }).collect()
    }

    #[test]
    fn tab_close_button_closes_that_tab() {
        let ctx = egui::Context::default();
        let tabs = [tab(1), tab(2)];
        let (rects, _) = tab_frame(&ctx, &tabs, vec![]);
        tab_frame(&ctx, &tabs, vec![]);

        // The × sits at the right end of each tab.
        let close_of = |r: egui::Rect| egui::pos2(r.right() - 6.0 - 9.0, r.center().y);
        assert_eq!(closes(&click(&ctx, &tabs, close_of(rects[0]), egui::PointerButton::Primary)), [CloseWhich::One(0)]);
        assert_eq!(closes(&click(&ctx, &tabs, close_of(rects[1]), egui::PointerButton::Primary)), [CloseWhich::One(1)]);

        // Clicking the tab body activates it instead; middle click closes.
        let body = egui::pos2(rects[1].left() + 30.0, rects[1].center().y);
        let actions = click(&ctx, &tabs, body, egui::PointerButton::Primary);
        assert!(closes(&actions).is_empty());
        assert!(actions.iter().any(|a| matches!(a, Action::ActivateTab(1))));
        assert_eq!(closes(&click(&ctx, &tabs, body, egui::PointerButton::Middle)), [CloseWhich::One(1)]);
    }

    #[test]
    fn close_variants() {
        let run = |which, active: usize| {
            let mut tabs = vec![0, 1, 2, 3];
            let mut active = active;
            close_tabs(&mut tabs, &mut active, which);
            (tabs, active)
        };
        assert_eq!(run(CloseWhich::One(1), 3), (vec![0, 2, 3], 2));
        assert_eq!(run(CloseWhich::One(3), 3), (vec![0, 1, 2], 2));
        assert_eq!(run(CloseWhich::Others(2), 0), (vec![2], 0));
        assert_eq!(run(CloseWhich::ToTheRight(1), 3), (vec![0, 1], 1));
        assert_eq!(run(CloseWhich::ToTheRight(1), 0), (vec![0, 1], 0));
        assert_eq!(run(CloseWhich::All, 2), (vec![], 0));
        assert_eq!(run(CloseWhich::One(9), 1), (vec![0, 1, 2, 3], 1));
    }

    #[test]
    fn reprojection_query() {
        assert_eq!(
            reproject_sql("select * from parcels where id < 10;\n", "geom", "geom_4326"),
            "select q.*, ST_Transform(q.geom, 4326) as geom_4326\nfrom (\nselect * from parcels where id < 10\n) q;"
        );
        assert!(reproject_sql("select 1", "Geom Col", "Geom Col_4326").contains("q.\"Geom Col\""));
    }

    #[test]
    fn query_names() {
        assert_eq!(default_query_name("-- note\n\n  select * from users where id = 1 and name like 'x%'", 1), "select * from users where id = 1 and nam");
        assert_eq!(default_query_name("  \n-- only a comment", 3), "Query 3");
    }

    #[test]
    fn typing_shows_and_accepts_suggestions() {
        let mut h = Harness::new();
        h.type_text("select * from us");
        assert_eq!(h.labels().first().map(String::as_str), Some("users"));

        h.key(Key::Enter, Modifiers::NONE);
        assert_eq!(h.console.sql, "select * from users");
        assert!(h.console.popup.is_none());

        // Typing continues at the new cursor; Tab accepts too.
        h.type_text(" u where u.em");
        assert_eq!(h.labels(), ["email"]);
        h.key(Key::Tab, Modifiers::NONE);
        assert_eq!(h.console.sql, "select * from users u where u.email");

        h.type_text(" = 'x'");
        assert_eq!(h.console.sql, "select * from users u where u.email = 'x'");
    }

    #[test]
    fn slash_command_tab_expands() {
        let mut h = Harness::new();
        h.type_text("/con_4326 geom");
        h.key(Key::Tab, Modifiers::NONE);
        assert_eq!(h.console.sql, "select t.*, ST_Transform(t.geom, 4326) as geom_4326\nfrom table_name t;");
        h.console.sql.clear();
        h.frame(vec![]);
        h.type_text("/con_4326_2 users");
        h.key(Key::Tab, Modifiers::NONE);
        assert_eq!(h.console.sql, "select t.*, ST_SetSRID(st_astext(t.shape)::text, 4326) as shape_4326\nfrom users t;");
        // Typing just "/" lists the commands.
        h.console.sql.clear();
        h.frame(vec![]);
        h.type_text("/");
        assert!(h.labels().iter().any(|l| l.starts_with("/con_4326")));
    }

    #[test]
    fn format_shortcut_formats_the_editor() {
        let mut h = Harness::new();
        h.type_text("select a, b from t where a = 1");
        h.key(Key::F, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(h.console.sql, "select a, b\nfrom t\nwhere a = 1");
    }

    #[test]
    fn arrows_escape_and_separators() {
        let mut h = Harness::new();
        h.type_text("select o.");
        // Columns of `o` aren't known until FROM exists: nothing to show.
        assert!(h.console.popup.is_none());

        h.type_text("x from orders o");
        h.console.sql.clear();
        h.frame(vec![]);

        h.type_text("select * from ");
        assert!(h.console.popup.is_none(), "space alone shouldn't open the popup");
        h.key(Key::Space, Modifiers::CTRL);
        assert_eq!(h.labels()[..2], ["users", "orders"]);
        h.key(Key::ArrowDown, Modifiers::NONE);
        h.key(Key::Enter, Modifiers::NONE);
        assert_eq!(h.console.sql, "select * from orders");

        h.type_text(" wh");
        assert!(h.console.popup.is_some());
        h.key(Key::Escape, Modifiers::NONE);
        assert!(h.console.popup.is_none());
        // Enter now inserts a newline as usual.
        h.key(Key::Enter, Modifiers::NONE);
        assert_eq!(h.console.sql, "select * from orders wh\n");
    }
}
