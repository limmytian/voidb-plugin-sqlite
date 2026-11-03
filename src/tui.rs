//! Standalone SQLite schema-aware data-inspector TUI (Requirement 83 pilot).
//!
//! The pilot follows the retained family TUI pattern (see the Redis pilot,
//! the accepted gate precedent, and the MySQL/PostgreSQL SQL adaptations):
//! overlay-first routing, a non-modal error banner, per-view selection
//! memory, a persistent hint footer, full-screen scrollable viewers,
//! confirm-quit, loading markers, Length(4) header/status panels, fixture
//! launch with `build_sqlite_tui_evidence`, a secret-leak scan, and
//! secret-free preflight JSON. All reads go through the existing
//! `SqliteService` channel contract (`SchemaCommand` + `LoadCommand`); the
//! only mutation is a single PK-anchored row delete sent through the
//! existing `CrudCommand::SaveChanges` path with an empty pending-edit set
//! and one deleted row index (the bounded path
//! `service/value_convert.rs::build_pk_where` anchors the DELETE). The TUI
//! adds no backend operations.
//!
//! Scope is deliberately bounded to local-file schema triage: tables with
//! exact row counts → structure (columns/indexes/FKs plus read-only CREATE
//! TABLE DDL) → bounded data pages → typed row inspection (NULL renders
//! literally, BLOB renders a size label plus a hex head) → typed row
//! delete. There is no database level (a SQLite file holds one database).
//! Row updates, inserts, paste/import, DDL execution, and a server view are
//! deferred as `later candidate`; the CLI remains the supported general
//! path.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use voidb_core::database::types::ForeignKeyInfo;
use voidb_core::{
    CellValue, ColumnInfo, Row, TabInfo, TabManager, TuiLaunchPlan, retained_tui_quality_gate,
};

use crate::config::SqliteConfig;
use crate::service::{
    CrudCommand, CrudEvent, IndexInfo, LoadCommand, LoadEvent, SchemaCommand, SchemaEvent,
    SqliteCommand, SqliteEvent, SqliteService, TableMeta,
};

const TABLES_HINTS: &str = "1 tables 2 structure j/k / filter i data r refresh ? q quit";
const STRUCTURE_HINTS: &str = "1 tables 2 structure j/k r refresh ? q quit";
const DATA_HINTS: &str =
    "j/k PgUp/PgDn n page b back / filter i row detail d del row r refetch Esc close";
const ROW_HINTS: &str = "row detail: j/k PgUp/PgDn scroll d del row Esc close";
const PAGE_SIZE: usize = 5;
const VIEWER_LINE_LIMIT: usize = 2_000;
const VIEWER_BYTE_LIMIT: usize = 2 * 1024 * 1024;
const INPUT_LIMIT: usize = 128;
const PAGE_STEP: isize = 10;
const CELL_MAX_WIDTH: usize = 28;
const STRUCTURE_DDL_LINE_LIMIT: usize = 12;

/// Where the TUI launch came from. Mirrors the family `MySqlTuiSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqliteTuiSource {
    Profile,
    Connection,
    Fixture,
}

/// Launch description for the standalone SQLite data-inspector TUI. Mirrors
/// the family launch shape; a SQLite target is a file, so there is no
/// database override argument.
#[derive(Debug, Clone)]
pub struct SqliteTuiLaunch {
    pub profile_label: String,
    pub config: Option<SqliteConfig>,
    pub source: SqliteTuiSource,
    pub fixture_path: Option<String>,
    pub purpose: String,
    pub readonly: bool,
    pub restore: bool,
    pub launch_plan: Option<TuiLaunchPlan>,
}

// ---------------------------------------------------------------------------
// Fixture model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SqliteTuiFixture {
    profile_label: Option<String>,
    file_label: String,
    connection_kind: String,
    status: Option<String>,
    target_error: Option<String>,
    #[serde(default)]
    error_tables: Vec<String>,
    error_message: Option<String>,
    #[serde(default)]
    tables: Vec<FixtureTableMeta>,
    #[serde(default)]
    views: Vec<String>,
    #[serde(default)]
    table_data: BTreeMap<String, FixtureTableData>,
    #[serde(default)]
    structures: BTreeMap<String, FixtureStructure>,
}

#[derive(Debug, Deserialize)]
struct FixtureTableMeta {
    name: String,
    #[serde(default)]
    table_type: String,
    rows: Option<u64>,
}

/// Fixture table payload. Rows are bare cell arrays decoded into `Row`s so
/// the viewer code is shared with the live service path.
#[derive(Debug, Deserialize)]
struct FixtureTableData {
    #[serde(default)]
    columns: Vec<ColumnInfo>,
    #[serde(default)]
    pks: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<CellValue>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FixtureIndex {
    name: String,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    unique: bool,
    #[serde(default)]
    index_type: String,
}

#[derive(Debug, Deserialize)]
struct FixtureStructure {
    #[serde(default)]
    columns: Vec<ColumnInfo>,
    #[serde(default)]
    indexes: Vec<FixtureIndex>,
    #[serde(default)]
    foreign_keys: Vec<ForeignKeyInfo>,
    #[serde(default)]
    ddl: Option<String>,
}

/// Internal fixture table with decoded rows.
#[derive(Debug, Clone)]
struct FixtureTable {
    columns: Vec<ColumnInfo>,
    pks: Vec<String>,
    rows: Vec<Row>,
}

/// In-memory fixture state. Fixture actions mutate this state directly so
/// the "row gone from the refetched page" success state is observable
/// without a database file.
struct SqliteTuiData {
    profile_label: String,
    file_label: String,
    connection_kind: String,
    status: String,
    target_error: Option<String>,
    error_tables: Vec<String>,
    error_message: String,
    tables: Vec<TableMeta>,
    views: Vec<String>,
    table_data: BTreeMap<String, FixtureTable>,
    structures: BTreeMap<String, FixtureStructure>,
}

impl SqliteTuiData {
    /// Tables are sorted so fixture evidence is deterministic and matches
    /// the `sqlite_master ORDER BY name` ordering the live service reports.
    fn resolve_tables(&self) -> (Vec<TableMeta>, Vec<String>) {
        let mut tables = self.tables.clone();
        tables.sort_by(|left, right| left.name.cmp(&right.name));
        (tables, self.views.clone())
    }

    /// Resolve one bounded data page. Unknown tables return an empty page so
    /// the TUI always has a visible empty state to render.
    fn resolve_page(
        &self,
        table: &str,
        offset: u64,
        page_size: usize,
    ) -> (Vec<ColumnInfo>, Vec<Row>, u64, Vec<String>) {
        let Some(fixture_table) = self.table_data.get(table) else {
            return (Vec::new(), Vec::new(), 0, Vec::new());
        };
        let start = offset as usize;
        let end = start
            .saturating_add(page_size)
            .min(fixture_table.rows.len());
        let rows = if start >= fixture_table.rows.len() {
            Vec::new()
        } else {
            fixture_table.rows[start..end].to_vec()
        };
        (
            fixture_table.columns.clone(),
            rows,
            fixture_table.rows.len() as u64,
            fixture_table.pks.clone(),
        )
    }

    /// Resolve the structure snapshot for one table (columns, indexes,
    /// foreign keys, and the CREATE TABLE DDL rendered read-only).
    fn resolve_structure(&self, table: &str) -> std::result::Result<StructureView, String> {
        let Some(found) = self.structures.get(table) else {
            return Err(format!(
                "target error: no fixture structure for table {table} (fixture)"
            ));
        };
        Ok(StructureView {
            table: table.to_string(),
            columns: found.columns.clone(),
            indexes: found
                .indexes
                .iter()
                .map(|index| IndexInfo {
                    name: index.name.clone(),
                    columns: index.columns.clone(),
                    unique: index.unique,
                    index_type: if index.index_type.is_empty() {
                        "BTREE".to_string()
                    } else {
                        index.index_type.clone()
                    },
                })
                .collect(),
            foreign_keys: found.foreign_keys.clone(),
            ddl: found.ddl.clone(),
        })
    }

    /// Apply a staged row delete to the fixture table. Tables listed in
    /// `error_tables` deliberately fail so the TUI can prove a target error
    /// keeps it responsive.
    fn apply_row_delete(
        &mut self,
        plan: &OperationPlanView,
    ) -> std::result::Result<String, String> {
        if self.error_tables.iter().any(|table| table == &plan.table) {
            return Err(self.error_message.clone());
        }
        let Some(fixture_table) = self.table_data.get_mut(&plan.table) else {
            return Err(format!("target error: no fixture data for {}", plan.table));
        };
        if fixture_table.pks.is_empty() {
            return Err(format!(
                "target error: {} has no primary key; bounded row delete unavailable",
                plan.table
            ));
        }
        let index = plan.row_offset as usize;
        if index >= fixture_table.rows.len() {
            return Err(format!(
                "target error: staged row {} is no longer present in the fixture table",
                plan.row_offset
            ));
        }
        fixture_table.rows.remove(index);
        Ok(format!(
            "row {} removed from the fixture table",
            plan.pk_summary
        ))
    }
}

fn load_fixture(path: &str) -> Result<SqliteTuiData> {
    let text = fs::read_to_string(path).with_context(|| format!("read fixture {path}"))?;
    let fixture: SqliteTuiFixture =
        serde_json::from_str(&text).with_context(|| format!("parse fixture {path}"))?;
    Ok(SqliteTuiData {
        profile_label: fixture
            .profile_label
            .unwrap_or_else(|| "fixture-sqlite".to_string()),
        file_label: fixture.file_label,
        connection_kind: fixture.connection_kind,
        status: fixture
            .status
            .unwrap_or_else(|| "fixture sqlite operations ready".to_string()),
        target_error: fixture.target_error,
        error_tables: fixture.error_tables,
        error_message: fixture
            .error_message
            .unwrap_or_else(|| "target error: fixture action rejected".to_string()),
        tables: fixture
            .tables
            .into_iter()
            .map(|meta| TableMeta {
                name: meta.name,
                table_type: if meta.table_type.is_empty() {
                    "table".to_string()
                } else {
                    meta.table_type
                },
                rows: meta.rows,
            })
            .collect(),
        views: fixture.views,
        table_data: fixture
            .table_data
            .into_iter()
            .map(|(table, data)| {
                (
                    table,
                    FixtureTable {
                        columns: data.columns,
                        pks: data.pks,
                        rows: data.rows.into_iter().map(|values| Row { values }).collect(),
                    },
                )
            })
            .collect(),
        structures: fixture.structures,
    })
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Tables,
    Structure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browser,
    Filter,
}

/// Full-screen or popup surfaces stacked on top of the browser. Input is
/// routed to the active overlay first, so each surface owns its key space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    DataViewer,
    RowViewer,
    ConfirmDeleteRow,
    ConfirmQuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    DeleteRow,
}

impl OperationKind {
    fn label(&self) -> &'static str {
        match self {
            OperationKind::DeleteRow => "delete row",
        }
    }

    fn risk(&self) -> &'static str {
        match self {
            OperationKind::DeleteRow => "destructive",
        }
    }
}

/// Staged mutation plan rendered in the details panel before execution. The
/// only slice 1 mutation is a single PK-anchored row delete.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationPlanView {
    kind: OperationKind,
    table: String,
    /// Absolute row index inside the table (page offset + index in page).
    row_offset: u64,
    /// Offset of the page the row was staged from, so a stale plan is
    /// detected before execution.
    page_offset: u64,
    /// Primary-key summary rendered in the plan panel (for example `id=7`).
    pk_summary: String,
    risk: &'static str,
    confirmations_required: u8,
    confirmations: u8,
}

impl OperationPlanView {
    /// Index of the staged row within its page (the `SaveChanges` deleted
    /// index).
    fn local_index(&self) -> usize {
        self.row_offset.saturating_sub(self.page_offset) as usize
    }
}

/// Structure snapshot for the table the structure view shows.
struct StructureView {
    table: String,
    columns: Vec<ColumnInfo>,
    indexes: Vec<IndexInfo>,
    foreign_keys: Vec<ForeignKeyInfo>,
    ddl: Option<String>,
}

/// Current bounded page of table data in the full-screen data viewer. Only
/// one page is held at a time; pages are replaced, never appended.
struct TableDataState {
    table: String,
    columns: Vec<ColumnInfo>,
    pks: Vec<String>,
    rows: Vec<Row>,
    total: u64,
    offset: u64,
    row_selection: usize,
    filter: String,
    loading: bool,
}

impl TableDataState {
    /// Client-side filter over the current page. Returns the page index of
    /// each match so staging can map back to the absolute row offset.
    fn filtered_rows(&self) -> Vec<(usize, &Row)> {
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty()
                    || row
                        .values
                        .iter()
                        .any(|value| cell_display(value).to_lowercase().contains(&needle))
            })
            .collect()
    }

    fn selected_row(&self) -> Option<(usize, Row)> {
        let matches = self.filtered_rows();
        if matches.is_empty() {
            return None;
        }
        let index = self.row_selection.min(matches.len() - 1);
        let (page_index, row) = matches[index];
        Some((page_index, row.clone()))
    }

    fn trim_selection(&mut self) {
        let matches = self.filtered_rows();
        self.row_selection = self.row_selection.min(matches.len().saturating_sub(1));
    }
}

/// Bounded text buffer for viewer bodies (same contract as the family TUIs).
struct BoundedLines {
    lines: VecDeque<String>,
    bytes: usize,
    dropped_lines: usize,
    max_lines: usize,
    max_bytes: usize,
}

pub fn write_sqlite_tui_preflight(launch: &SqliteTuiLaunch) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&preflight_value(launch))?
    );
    Ok(())
}

pub fn build_sqlite_tui_evidence(launch: &SqliteTuiLaunch) -> Result<Value> {
    let data = if let Some(path) = &launch.fixture_path {
        load_fixture(path)?
    } else {
        let config = launch
            .config
            .as_ref()
            .context("sqlite tui evidence requires a profile, connection, or fixture")?;
        config_data(launch.profile_label.clone(), config)
    };

    let mut evidence = json!({
        "schema_version": 1,
        "kind": "sqlite_tui_fixture_evidence",
        "quality_gate": retained_tui_quality_gate(
            "sqlite",
            &["fixture-sqlite-ops", "fixture-sqlite-local"],
            &["target error"],
            80,
            24,
            10_000
        ),
        "preflight": preflight_value(launch),
        "transcript": {
            "profile_label": data.profile_label,
            "file_label": data.file_label,
            "connection_kind": data.connection_kind,
            "status": data.status,
            "target_error": data.target_error,
            "tables": data.resolve_tables().0.iter().map(table_meta_value).collect::<Vec<_>>(),
            "views": data.resolve_tables().1,
            "table_data": data.table_data.iter().map(table_data_value).collect::<Vec<_>>(),
            "structures": data.structures.iter().map(structure_value).collect::<Vec<_>>(),
            "page_size": PAGE_SIZE,
            "viewer_bounds": {
                "max_lines": VIEWER_LINE_LIMIT,
                "max_bytes": VIEWER_BYTE_LIMIT
            }
        },
        "plan_transcript": [
            {
                "action": "delete_row",
                "risk": "destructive",
                "typed_confirmation": "exact_table_name_required",
                "second_confirmation": "explicit_y",
                "primary_key_required": true,
                "bounded_to": "single PK-anchored DELETE via CrudCommand::SaveChanges with empty pending edits and one deleted row index (value_convert::build_pk_where anchors the WHERE clause)",
                "readonly_behavior": "plan_visible_but_blocked",
                "post_action": "current_page_refetch"
            }
        ],
        "coverage": [
            "startup",
            "tables_structure_tabs",
            "tables_browser_row_counts",
            "table_data_paging",
            "partial_page_state",
            "empty_table_state",
            "client_filter_match_count",
            "row_viewer_typed_columns",
            "null_value_rendering",
            "blob_value_rendering",
            "long_text_bounded_viewer",
            "structure_viewer_columns_indexes_fks",
            "structure_ddl_read_only",
            "typed_delete_confirmation",
            "delete_requires_primary_key",
            "readonly_block",
            "mutation_refetch",
            "erroring_mutation_responsive",
            "non_modal_error_banner",
            "target_error_responsive",
            "quit_confirmation",
            "resize",
            "quit_restore",
            "secret_leak_scan"
        ],
        "deferred": {
            "server_view": "later candidate: the sqlite service channel contract exposes no bounded diagnostics command (and sqlite is embedded, so there is no server process to introspect); if PRAGMA-based diagnostics such as page_count or integrity_check gain a bounded service command they can slot into hotkey 3 with their own fixture and PTY evidence",
            "row_updates": "later candidate: CrudCommand::SaveChanges cell edits need their own fixture, confirmation, and redaction evidence",
            "row_inserts": "later candidate: CrudCommand::InsertRow needs the same evidence",
            "paste_and_import": "later candidate: CrudCommand::PasteRows and ImportCsv are bulk write paths outside the slice 1 mutation gate",
            "ddl_and_table_management": "later candidate: BrowserCommand DDL (drop/rename/duplicate) blast radius exceeds the slice 1 mutation gate; the structure view renders CREATE TABLE DDL read-only and never executes it",
            "server_side_list_filtering": "later candidate: SchemaCommand::ListTables exposes no filter parameter; the tables filter is client-side",
            "outcome": "later candidate within the accepted pilot",
            "supported_mutations": ["delete_row"],
            "cli_remains_supported_general_path": true
        },
        "mutation_rescan_policy": {
            "page_semantics": "one bounded LIMIT/OFFSET page at a time; the next page is fetched with offset + page_size and pages are replaced, never appended",
            "after_mutation": "the current page offset is refetched so the deleted row disappears and totals stay accurate",
            "fixture_mutations": "applied to the in-memory fixture rows so paging evidence stays deterministic"
        },
        "secret_leak_scan": null
    });

    let rendered = serde_json::to_string(&evidence)?;
    let markers = secret_leak_markers(&rendered);
    evidence["secret_leak_scan"] = json!({
        "passed": markers.is_empty(),
        "marker_count": markers.len(),
        "markers": markers
    });
    Ok(evidence)
}

pub fn run_sqlite_tui(launch: SqliteTuiLaunch) -> Result<()> {
    let mut app = SqliteTuiApp::new(launch)?;
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app);
    app.shutdown();
    ratatui::restore();
    result
}

fn preflight_value(launch: &SqliteTuiLaunch) -> Value {
    let fixture_data = launch
        .fixture_path
        .as_ref()
        .and_then(|path| load_fixture(path).ok());
    let connection = launch
        .config
        .as_ref()
        .map(connection_value)
        .or_else(|| {
            fixture_data.as_ref().map(|data| {
                json!({
                    "kind": data.connection_kind,
                    "file_label": data.file_label,
                    "secret_material": "redacted"
                })
            })
        })
        .unwrap_or_else(|| {
            json!({
                "kind": "fixture",
                "file_label": "fixture",
                "secret_material": "redacted"
            })
        });
    let launch_plan = launch.launch_plan.as_ref().map(|plan| {
        json!({
            "schema_version": plan.schema_version,
            "plugin_id": plan.plugin_id,
            "command": plan.command,
            "args": plan.args,
            "profile": plan.profile,
            "purpose": plan.purpose,
            "readonly": plan.readonly,
            "restore": plan.restore,
            "raw_input": plan.raw_input,
            "credential_ref_count": plan.credential_grant.credential_refs.len(),
            "credential_grant_id": plan.credential_grant.id,
            "redaction": plan.redaction
        })
    });

    json!({
        "ok": true,
        "command": "sqlite tui",
        "plugin_id": "sqlite",
        "profile_label": fixture_data
            .as_ref()
            .map(|data| data.profile_label.clone())
            .unwrap_or_else(|| launch.profile_label.clone()),
        "source": launch.source,
        "purpose": launch.purpose,
        "readonly": launch.readonly,
        "restore": launch.restore,
        "raw_input": false,
        "fixture": launch.fixture_path.is_some(),
        "connection": connection,
        "privacy": {
            "diagnostics_include_password": false,
            "diagnostics_include_connection_url": false,
            "diagnostics_include_row_values": false,
            "diagnostics_include_full_file_path": false
        },
        "stream_bounds": {
            "table_page": { "page_size": PAGE_SIZE },
            "row_viewer": { "max_lines": VIEWER_LINE_LIMIT, "max_bytes": VIEWER_BYTE_LIMIT }
        },
        "service_boundary": "SqliteService::Channel(SyncWorker)",
        "modes": [
            "tables",
            "structure",
            "filter",
            "data_viewer",
            "row_viewer",
            "typed_delete_confirmation",
            "confirm_quit",
            "target_error"
        ],
        "launch_plan": launch_plan
    })
}

/// The connection value reports the file basename only: SQLite profiles carry
/// no credentials (`secret_classes = []`), and a full path can embed the
/// operator's username, so preflight keeps the target identifiable without
/// leaking the absolute path.
fn connection_value(config: &SqliteConfig) -> Value {
    json!({
        "kind": "file",
        "file": file_label(config),
        "auth": "none",
        "secret_material": "redacted"
    })
}

fn file_label(config: &SqliteConfig) -> String {
    Path::new(&config.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "memory".to_string())
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut SqliteTuiApp) -> Result<()> {
    terminal.draw(|frame| app.draw(frame))?;
    loop {
        let mut dirty = app.drain_service();
        if app.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    app.handle_key(key);
                    dirty = true;
                }
                Event::Resize(cols, rows) => {
                    app.status = format!("resized sqlite view to {cols}x{rows}");
                    dirty = true;
                }
                _ => {}
            }
        }
        if dirty {
            terminal.draw(|frame| app.draw(frame))?;
        }
    }
}

fn config_data(profile_label: String, config: &SqliteConfig) -> SqliteTuiData {
    SqliteTuiData {
        profile_label,
        file_label: file_label(config),
        connection_kind: "file".to_string(),
        status: "opening the SQLite file through the SqliteService channel".to_string(),
        target_error: None,
        error_tables: Vec::new(),
        error_message: "target error: fixture action rejected".to_string(),
        tables: Vec::new(),
        views: Vec::new(),
        table_data: BTreeMap::new(),
        structures: BTreeMap::new(),
    }
}

struct SqliteTuiApp {
    profile_label: String,
    file_label: String,
    connection_kind: String,
    source: SqliteTuiSource,
    purpose: String,
    readonly: bool,
    restore: bool,
    view: View,
    mode: Mode,
    /// Browser-level filter for the tables list; the data viewer keeps its
    /// own filter.
    browser_filter: String,
    overlay: Overlay,
    error_banner: Option<String>,
    tables: Vec<TableMeta>,
    views: Vec<String>,
    tables_loaded: bool,
    table_selection: usize,
    schema_pending: bool,
    structure_request: Option<String>,
    structure_view: Option<StructureView>,
    structure_pending: bool,
    structure_selection: usize,
    data: Option<TableDataState>,
    data_pending: bool,
    data_filtering: bool,
    row_viewer_table: Option<String>,
    row_viewer_offset: u64,
    row_viewer_lines: BoundedLines,
    row_viewer_scroll: usize,
    typed_table_input: String,
    operation_plan: Option<OperationPlanView>,
    fixture: Option<SqliteTuiData>,
    service: Option<SqliteService>,
    status: String,
    should_quit: bool,
    render_quit: Arc<AtomicBool>,
}

impl SqliteTuiApp {
    fn new(launch: SqliteTuiLaunch) -> Result<Self> {
        let data = if let Some(path) = &launch.fixture_path {
            load_fixture(path)?
        } else {
            let config = launch
                .config
                .as_ref()
                .context("sqlite tui requires a profile, connection, or fixture")?;
            config_data(launch.profile_label.clone(), config)
        };

        let render_quit = Arc::new(AtomicBool::new(false));
        let service = if launch.fixture_path.is_none() {
            let config = launch
                .config
                .clone()
                .context("sqlite tui requires config outside fixture mode")?;
            let tabs = Arc::new(StandaloneSqliteTabManager::new(render_quit.clone()));
            // The SyncWorker opens the database file on its own thread; a
            // failure to open surfaces here, before the TUI takes over the
            // terminal.
            let service = SqliteService::new(config, tabs)
                .map_err(|e| anyhow::anyhow!("sqlite tui could not open the database: {e}"))?;
            Some(service)
        } else {
            None
        };

        let mut app = Self {
            profile_label: data.profile_label.clone(),
            file_label: data.file_label.clone(),
            connection_kind: data.connection_kind.clone(),
            source: launch.source,
            purpose: launch.purpose,
            readonly: launch.readonly,
            restore: launch.restore,
            view: View::Tables,
            mode: Mode::Browser,
            browser_filter: String::new(),
            overlay: Overlay::None,
            error_banner: data.target_error.clone(),
            tables: Vec::new(),
            views: Vec::new(),
            tables_loaded: false,
            table_selection: 0,
            schema_pending: false,
            structure_request: None,
            structure_view: None,
            structure_pending: false,
            structure_selection: 0,
            data: None,
            data_pending: false,
            data_filtering: false,
            row_viewer_table: None,
            row_viewer_offset: 0,
            row_viewer_lines: BoundedLines::new(VIEWER_LINE_LIMIT, VIEWER_BYTE_LIMIT),
            row_viewer_scroll: 0,
            typed_table_input: String::new(),
            operation_plan: None,
            fixture: if launch.fixture_path.is_some() {
                Some(data)
            } else {
                None
            },
            service,
            status: String::new(),
            should_quit: false,
            render_quit,
        };
        // Fixture mode resolves the catalog eagerly so the first frame
        // already shows the tables; live mode sends ListTables through the
        // service channel (there is no Connect step for the SyncWorker).
        app.request_tables();
        if let Some(error) = app
            .fixture
            .as_ref()
            .and_then(|fixture| fixture.target_error.clone())
        {
            app.error_banner = Some(error.clone());
            app.status = format!("{} | {}", app.status, safe_error_summary(&error));
        }
        Ok(app)
    }

    fn shutdown(&mut self) {
        // Dropping SqliteService stops the SyncWorker thread and closes the
        // database file; the channel contract has no Disconnect command.
        self.service = None;
    }

    fn drain_service(&mut self) -> bool {
        let mut changed = false;
        if self.render_quit.load(Ordering::SeqCst) {
            changed |= !self.should_quit;
            self.should_quit = true;
        }
        let Some(mut service) = self.service.take() else {
            return changed;
        };
        while let Some(event) = service.poll_event() {
            changed = true;
            self.handle_service_event(event);
        }
        self.service = Some(service);
        changed
    }

    fn handle_service_event(&mut self, event: SqliteEvent) {
        match event {
            SqliteEvent::Schema(SchemaEvent::TablesLoaded { tables, views }) => {
                self.schema_pending = false;
                self.tables = tables;
                self.views = views;
                self.tables_loaded = true;
                self.trim_table_selection();
                self.status = format!(
                    "loaded {} tables and {} views",
                    self.tables.len(),
                    self.views.len()
                );
            }
            SqliteEvent::Schema(SchemaEvent::StructureLoaded {
                table,
                columns,
                indexes,
                foreign_keys,
            }) => {
                self.structure_pending = false;
                if self.structure_request.as_deref() == Some(table.as_str()) {
                    self.structure_view = Some(StructureView {
                        table: table.clone(),
                        columns,
                        indexes,
                        foreign_keys,
                        ddl: None,
                    });
                    self.structure_selection = 0;
                    self.status = format!("structure loaded for {table}");
                }
            }
            SqliteEvent::Schema(SchemaEvent::CreateTableDdl { table, ddl }) => {
                if self.structure_request.as_deref() == Some(table.as_str())
                    && let Some(view) = self
                        .structure_view
                        .as_mut()
                        .filter(|view| view.table == table)
                {
                    view.ddl = Some(ddl);
                }
            }
            SqliteEvent::Load(LoadEvent::TableDataLoaded {
                table,
                pks,
                columns,
                rows,
                total,
            }) => {
                self.data_pending = false;
                let matches_request = self
                    .data
                    .as_ref()
                    .is_some_and(|state| state.table == table && state.loading);
                if matches_request && let Some(state) = self.data.as_mut() {
                    state.loading = false;
                    state.columns = columns;
                    state.pks = pks;
                    state.rows = rows;
                    state.total = total;
                    state.trim_selection();
                    self.status = format!(
                        "loaded {} rows at offset {} of {total}",
                        state.rows.len(),
                        state.offset
                    );
                }
            }
            SqliteEvent::Crud(CrudEvent::ChangesSaved { updates, deletes }) => {
                self.operation_plan = None;
                self.status = format!("saved: {updates} update(s), {deletes} delete(s)");
                self.refetch_current_page();
            }
            SqliteEvent::Schema(_)
            | SqliteEvent::Crud(_)
            | SqliteEvent::Load(_)
            | SqliteEvent::Browser(_) => {
                self.schema_pending = false;
                self.structure_pending = false;
                self.data_pending = false;
                if let Some(state) = self.data.as_mut() {
                    state.loading = false;
                }
                self.status = "service event outside the pilot read path".to_string();
            }
            SqliteEvent::Error(message) => {
                // Background and request failures surface as a non-modal
                // banner so a failing fetch or mutation never hijacks the
                // browser.
                self.schema_pending = false;
                self.structure_pending = false;
                self.data_pending = false;
                if let Some(state) = self.data.as_mut() {
                    state.loading = false;
                }
                self.error_banner = Some(message);
                self.status = safe_error_summary(self.error_banner.as_deref().unwrap_or(""));
            }
        }
    }

    // === Input routing ===

    fn handle_key(&mut self, key: KeyEvent) {
        // The error banner is informational: any key acknowledges it while
        // the key itself keeps its normal meaning, so background failures
        // never trap the operator in a modal.
        self.error_banner = None;
        match self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                self.overlay = Overlay::None;
                self.status = "help closed".to_string();
                return;
            }
            Overlay::DataViewer => {
                self.handle_data_viewer_key(key);
                return;
            }
            Overlay::RowViewer => {
                self.handle_row_viewer_key(key);
                return;
            }
            Overlay::ConfirmDeleteRow => {
                self.handle_confirm_delete_key(key);
                return;
            }
            Overlay::ConfirmQuit => {
                self.handle_confirm_quit_key(key);
                return;
            }
        }
        if self.mode == Mode::Filter {
            self.handle_filter_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('1') => self.set_view(View::Tables),
            KeyCode::Char('2') => self.set_view(View::Structure),
            KeyCode::Tab => {
                let next = match self.view {
                    View::Tables => View::Structure,
                    View::Structure => View::Tables,
                };
                self.set_view(next);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-PAGE_STEP),
            KeyCode::PageDown => self.move_selection(PAGE_STEP),
            KeyCode::Home | KeyCode::Char('g') => self.set_selected(0),
            KeyCode::End | KeyCode::Char('G') => {
                let len = self.current_items_len();
                self.set_selected(len.saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Char('i') => self.inspect_selected(),
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.status = "filter: type to narrow live, Enter apply, Esc clear".to_string();
            }
            KeyCode::Char('r') => self.refresh_view(),
            KeyCode::Char('d') => {
                self.status = "no table-level mutations in this pilot; row deletes live in the table data viewer (i)"
                    .to_string();
            }
            KeyCode::Char('?') => self.show_help(),
            KeyCode::Char('y') => self.confirm_plan(),
            KeyCode::Esc => self.cancel_plan(),
            _ => {
                self.status = format!(
                    "sqlite: {}",
                    match self.view {
                        View::Tables => TABLES_HINTS,
                        View::Structure => STRUCTURE_HINTS,
                    }
                );
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.mode = Mode::Browser;
                self.set_selected(0);
                self.status = format!(
                    "filter applied: {} ({} matches)",
                    self.filter_label(),
                    self.filtered_tables().len()
                );
            }
            KeyCode::Esc => {
                self.browser_filter.clear();
                self.mode = Mode::Browser;
                self.set_selected(0);
                self.status = "filter cleared".to_string();
            }
            KeyCode::Backspace => {
                self.browser_filter.pop();
                self.set_selected(0);
            }
            KeyCode::Char(ch) => {
                if self.browser_filter.len() < INPUT_LIMIT {
                    self.browser_filter.push(ch);
                }
                self.set_selected(0);
            }
            _ => {}
        }
    }

    fn handle_data_viewer_key(&mut self, key: KeyEvent) {
        // While the viewer filter input is open every character belongs to
        // the filter, mirroring the family filter-mode routing.
        if self.data_filtering {
            match key.code {
                KeyCode::Esc => {
                    self.data_filtering = false;
                    if let Some(state) = self.data.as_mut() {
                        state.filter.clear();
                        state.trim_selection();
                    }
                    self.status = "viewer filter cancelled".to_string();
                }
                KeyCode::Backspace => {
                    if let Some(state) = self.data.as_mut() {
                        state.filter.pop();
                        state.trim_selection();
                    }
                }
                KeyCode::Enter => {
                    self.data_filtering = false;
                    let (matches, filter) = {
                        let Some(state) = self.data.as_ref() else {
                            return;
                        };
                        (state.filtered_rows().len(), state.filter.clone())
                    };
                    self.status = format!(
                        "filter applied: {} ({} matches)",
                        if filter.is_empty() {
                            "none".to_string()
                        } else {
                            filter
                        },
                        matches
                    );
                }
                KeyCode::Char(ch) => {
                    if let Some(state) = self.data.as_mut()
                        && state.filter.len() < INPUT_LIMIT
                    {
                        state.filter.push(ch);
                        state.trim_selection();
                    }
                }
                _ => {}
            }
            return;
        }
        let matches = self
            .data
            .as_ref()
            .map(|state| state.filtered_rows().len())
            .unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
                self.data = None;
                self.data_filtering = false;
                self.status = "table data viewer closed".to_string();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_data_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_data_selection(1),
            KeyCode::PageUp => self.move_data_selection(-PAGE_STEP),
            KeyCode::PageDown => self.move_data_selection(PAGE_STEP),
            KeyCode::Home | KeyCode::Char('g') => self.set_data_selection(0),
            KeyCode::End | KeyCode::Char('G') => self.set_data_selection(matches.saturating_sub(1)),
            KeyCode::Char('n') => self.next_data_page(),
            KeyCode::Char('b') => self.previous_data_page(),
            KeyCode::Char('/') => {
                self.data_filtering = true;
                self.status =
                    "viewer filter: type to narrow the current page, Enter applies, Esc clears"
                        .to_string();
            }
            KeyCode::Char('r') => {
                self.refetch_current_page();
                self.status = "refetching current page".to_string();
            }
            KeyCode::Enter | KeyCode::Char('i') => self.open_row_viewer(),
            KeyCode::Char('d') => self.open_delete_prompt(),
            KeyCode::Char('y') => self.confirm_plan(),
            _ => {
                self.status = format!("sqlite data viewer: {DATA_HINTS}");
            }
        }
    }

    fn handle_row_viewer_key(&mut self, key: KeyEvent) {
        let max_scroll = self.row_viewer_lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::DataViewer;
                self.row_viewer_table = None;
                self.status = "row viewer closed".to_string();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.row_viewer_scroll = self.row_viewer_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.row_viewer_scroll = (self.row_viewer_scroll + 1).min(max_scroll);
            }
            KeyCode::PageUp => {
                self.row_viewer_scroll = self.row_viewer_scroll.saturating_sub(PAGE_STEP as usize);
            }
            KeyCode::PageDown => {
                self.row_viewer_scroll =
                    (self.row_viewer_scroll + PAGE_STEP as usize).min(max_scroll);
            }
            KeyCode::Home | KeyCode::Char('g') => self.row_viewer_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.row_viewer_scroll = max_scroll,
            KeyCode::Char('d') => self.open_delete_prompt(),
            KeyCode::Char('y') => self.confirm_plan(),
            _ => {}
        }
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent) {
        let Some(state) = self.data.as_ref() else {
            self.overlay = Overlay::None;
            return;
        };
        let table = state.table.clone();
        match key.code {
            KeyCode::Esc => {
                self.typed_table_input.clear();
                self.overlay = Overlay::DataViewer;
                self.status = "delete cancelled".to_string();
            }
            KeyCode::Backspace => {
                self.typed_table_input.pop();
            }
            KeyCode::Enter => {
                if self.typed_table_input != table {
                    self.status = format!("typed name does not match {table}; delete not staged");
                    self.typed_table_input.clear();
                    return;
                }
                match self.staged_row_delete() {
                    Some((table, row_offset, page_offset, pk_summary)) => {
                        let staged_table = table.clone();
                        let staged_summary = pk_summary.clone();
                        self.typed_table_input.clear();
                        // Return to the data viewer the confirm was opened
                        // from. The typed table name is the first of two
                        // confirmations; `y` remains the explicit
                        // non-default acknowledgement.
                        self.overlay = Overlay::DataViewer;
                        self.operation_plan = Some(OperationPlanView {
                            kind: OperationKind::DeleteRow,
                            table,
                            row_offset,
                            page_offset,
                            pk_summary,
                            risk: OperationKind::DeleteRow.risk(),
                            confirmations_required: 2,
                            confirmations: 1,
                        });
                        self.status = format!(
                            "delete plan staged for row {staged_summary} in {staged_table} (typed confirmation recorded); press y to execute"
                        );
                    }
                    None => {
                        self.overlay = Overlay::DataViewer;
                        self.status = "row delete unavailable without a primary key".to_string();
                    }
                }
            }
            KeyCode::Char(ch) if self.typed_table_input.len() < INPUT_LIMIT => {
                self.typed_table_input.push(ch);
            }
            _ => {}
        }
    }

    fn handle_confirm_quit_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.should_quit = true;
            }
            _ => {
                self.overlay = Overlay::None;
                self.status = "quit cancelled".to_string();
            }
        }
    }

    // === Selection and views ===

    fn filtered_tables(&self) -> Vec<TableMeta> {
        let needle = self.filter_needle();
        self.tables
            .iter()
            .filter(|meta| needle.is_empty() || meta.name.to_lowercase().contains(&needle))
            .cloned()
            .collect()
    }

    fn filter_needle(&self) -> String {
        self.browser_filter.to_lowercase()
    }

    fn filter_label(&self) -> String {
        if self.browser_filter.is_empty() {
            "none".to_string()
        } else {
            self.browser_filter.clone()
        }
    }

    fn current_items_len(&self) -> usize {
        match self.view {
            View::Tables => self.filtered_tables().len(),
            View::Structure => self.structure_columns().len(),
        }
    }

    fn selected(&self) -> usize {
        match self.view {
            View::Tables => self.table_selection,
            View::Structure => self.structure_selection,
        }
    }

    fn set_selected(&mut self, value: usize) {
        let len = self.current_items_len();
        let clamped = value.min(len.saturating_sub(1));
        match self.view {
            View::Tables => self.table_selection = clamped,
            View::Structure => self.structure_selection = clamped,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.current_items_len();
        if len == 0 {
            self.set_selected(0);
            return;
        }
        let last = len as isize - 1;
        let next = (self.selected() as isize + delta).clamp(0, last) as usize;
        self.set_selected(next);
    }

    fn trim_table_selection(&mut self) {
        let len = self.filtered_tables().len();
        self.table_selection = self.table_selection.min(len.saturating_sub(1));
    }

    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.status = format!(
            "view: {}",
            match view {
                View::Tables => "tables",
                View::Structure => "structure",
            }
        );
        if view == View::Structure {
            self.request_structure_for_selected();
        }
    }

    // === Service requests ===

    fn request_tables(&mut self) {
        if let Some(fixture) = &self.fixture {
            let (tables, views) = fixture.resolve_tables();
            self.tables = tables;
            self.views = views;
            self.tables_loaded = true;
            self.trim_table_selection();
            self.status = format!(
                "fixture loaded {} tables and {} views",
                self.tables.len(),
                self.views.len()
            );
            return;
        }
        self.schema_pending = true;
        if let Some(service) = &self.service {
            self.status = "loading tables".to_string();
            service.send(SqliteCommand::Schema(SchemaCommand::ListTables));
        } else {
            self.schema_pending = false;
        }
    }

    /// Load the structure snapshot for the table selected in the tables
    /// view. Live mode sends `LoadStructure` (columns/indexes/FKs) and
    /// `ShowCreateTable` (read-only DDL); fixture mode resolves both from
    /// the fixture snapshot.
    fn request_structure_for_selected(&mut self) {
        let Some(meta) = self.filtered_tables().get(self.table_selection).cloned() else {
            self.status = "no table selected for structure".to_string();
            return;
        };
        if self
            .structure_view
            .as_ref()
            .is_some_and(|view| view.table == meta.name)
            && !self.structure_pending
        {
            return;
        }
        self.structure_request = Some(meta.name.clone());
        self.structure_selection = 0;
        if let Some(fixture) = &self.fixture {
            match fixture.resolve_structure(&meta.name) {
                Ok(view) => {
                    self.structure_view = Some(view);
                    self.structure_pending = false;
                    self.status = format!("structure loaded for {}", meta.name);
                }
                Err(message) => {
                    self.structure_view = None;
                    self.structure_pending = false;
                    self.error_banner = Some(message);
                    self.status = safe_error_summary(self.error_banner.as_deref().unwrap_or(""));
                }
            }
            return;
        }
        self.structure_pending = true;
        self.structure_view = None;
        if let Some(service) = &self.service {
            self.status = format!("loading structure for {}", meta.name);
            service.send(SqliteCommand::Schema(SchemaCommand::LoadStructure {
                table: meta.name.clone(),
            }));
            service.send(SqliteCommand::Schema(SchemaCommand::ShowCreateTable {
                table: meta.name,
            }));
        } else {
            self.structure_pending = false;
        }
    }

    /// Send one bounded page request (`LIMIT page_size OFFSET n`). Fixture
    /// mode resolves the page synchronously; live mode sends
    /// `LoadCommand::LoadTableData`.
    fn request_table_data(&mut self, table: String, offset: u64) {
        if let Some(state) = self.data.as_mut() {
            state.table = table.clone();
            state.offset = offset;
            state.row_selection = 0;
            state.loading = true;
        }
        if let Some(fixture) = &self.fixture {
            let (columns, rows, total, pks) = fixture.resolve_page(&table, offset, PAGE_SIZE);
            if let Some(state) = self.data.as_mut() {
                state.columns = columns;
                state.rows = rows;
                state.total = total;
                state.pks = pks;
                state.loading = false;
                state.trim_selection();
            }
            let loaded = self
                .data
                .as_ref()
                .map(|state| state.rows.len())
                .unwrap_or(0);
            self.status = format!("fixture loaded {loaded} rows at offset {offset} of {total}");
        } else if let Some(service) = &self.service {
            self.data_pending = true;
            service.send(SqliteCommand::Load(LoadCommand::LoadTableData {
                table,
                page_size: PAGE_SIZE,
                offset,
            }));
            self.status = format!("loading rows at offset {offset}");
        } else if let Some(state) = self.data.as_mut() {
            state.loading = false;
        }
    }

    fn open_table_data(&mut self, table: String) {
        self.data = Some(TableDataState {
            table: table.clone(),
            columns: Vec::new(),
            pks: Vec::new(),
            rows: Vec::new(),
            total: 0,
            offset: 0,
            row_selection: 0,
            filter: String::new(),
            loading: true,
        });
        self.overlay = Overlay::DataViewer;
        self.data_filtering = false;
        self.request_table_data(table.clone(), 0);
        self.status = format!("table data viewer: {table} (bounded page of {PAGE_SIZE})");
    }

    /// `n`: replace the current page with the page at `offset + page_size`.
    /// Pages are replaced (not appended) so the viewport stays bounded.
    fn next_data_page(&mut self) {
        let Some(state) = self.data.as_ref() else {
            return;
        };
        let next_offset = state.offset + PAGE_SIZE as u64;
        if next_offset >= state.total {
            self.status = "end of table reached; press b to go back or / to filter".to_string();
            return;
        }
        let table = state.table.clone();
        self.request_table_data(table, next_offset);
    }

    fn previous_data_page(&mut self) {
        let Some(state) = self.data.as_ref() else {
            return;
        };
        if state.offset == 0 {
            self.status = "no previous page".to_string();
            return;
        }
        let previous = state.offset.saturating_sub(PAGE_SIZE as u64);
        let table = state.table.clone();
        self.request_table_data(table, previous);
    }

    /// After a mutation the current page offset is refetched so the deleted
    /// row disappears and totals stay accurate without leaving the TUI.
    fn refetch_current_page(&mut self) {
        let Some(state) = self.data.as_ref() else {
            return;
        };
        let (table, offset) = (state.table.clone(), state.offset);
        self.request_table_data(table, offset);
    }

    fn refresh_view(&mut self) {
        match self.view {
            View::Tables => {
                self.request_tables();
                self.status = "refreshing tables".to_string();
            }
            View::Structure => {
                self.request_structure_for_selected();
                self.status = "refreshing structure".to_string();
            }
        }
    }

    // === Inspect ===

    fn inspect_selected(&mut self) {
        match self.view {
            View::Tables => {
                let Some(meta) = self.filtered_tables().get(self.selected()).cloned() else {
                    self.status = "nothing selected".to_string();
                    return;
                };
                self.open_table_data(meta.name);
            }
            View::Structure => {
                self.status =
                    "structure view lists columns; data inspection lives in the tables view (i)"
                        .to_string();
            }
        }
    }

    fn open_row_viewer(&mut self) {
        let Some(state) = self.data.as_ref() else {
            return;
        };
        let Some((page_index, row)) = state.selected_row() else {
            self.status = "nothing selected".to_string();
            return;
        };
        let columns = state.columns.clone();
        let pks = state.pks.clone();
        let total = state.total;
        let offset = state.offset + page_index as u64;
        let table = state.table.clone();

        let mut lines = BoundedLines::new(VIEWER_LINE_LIMIT, VIEWER_BYTE_LIMIT);
        lines.push_line(format!("row {} of {total} in {table}", offset + 1));
        lines.push_line(String::new());
        for (index, column) in columns.iter().enumerate() {
            let value = row.values.get(index).unwrap_or(&CellValue::Null);
            let mut flags = Vec::new();
            if column.is_primary_key || pks.contains(&column.name) {
                flags.push("PK");
            }
            flags.push(if column.nullable {
                "nullable"
            } else {
                "NOT NULL"
            });
            lines.push_line(format!(
                "{:<22} {:<18} {:<16} {}",
                column.name,
                column.data_type,
                flags.join(" "),
                cell_display(value)
            ));
            // SQLite storage classes surface here: a BLOB cell renders a hex
            // head below its size label (CellValue::Blob decodes straight
            // from the sqlite value_ref probe in service/value_convert.rs).
            if let CellValue::Blob(bytes) = value {
                lines.push_line(format!("  {}", blob_hex_head(bytes)));
            }
        }
        self.row_viewer_table = Some(table);
        self.row_viewer_offset = offset;
        self.row_viewer_lines = lines;
        self.row_viewer_scroll = 0;
        self.overlay = Overlay::RowViewer;
        self.status = format!("row viewer: row {} of {total}", offset + 1);
    }

    // === Mutations ===

    fn open_delete_prompt(&mut self) {
        let Some(state) = self.data.as_ref() else {
            self.status = "no table data loaded".to_string();
            return;
        };
        if state.pks.is_empty() {
            self.status = format!(
                "{} has no primary key; bounded row delete unavailable (CLI remains the path)",
                state.table
            );
            return;
        }
        if state.selected_row().is_none() {
            self.status = "select a row to delete".to_string();
            return;
        }
        self.typed_table_input.clear();
        self.overlay = Overlay::ConfirmDeleteRow;
        self.status = format!(
            "delete row from {}: type the exact table name to confirm, Esc cancels",
            state.table
        );
    }

    /// Derive the staging payload for the currently selected row. Returns
    /// `(table, absolute_row_offset, page_offset, pk_summary)`.
    fn staged_row_delete(&self) -> Option<(String, u64, u64, String)> {
        let state = self.data.as_ref()?;
        if state.pks.is_empty() {
            return None;
        }
        let (page_index, row) = state.selected_row()?;
        let pk_summary = pk_summary(&state.columns, &row, &state.pks);
        Some((
            state.table.clone(),
            state.offset + page_index as u64,
            state.offset,
            pk_summary,
        ))
    }

    fn confirm_plan(&mut self) {
        let Some(mut plan) = self.operation_plan.take() else {
            self.status = "no operation plan to confirm".to_string();
            return;
        };
        plan.confirmations = plan.confirmations.saturating_add(1);
        if plan.confirmations < plan.confirmations_required {
            self.status = format!(
                "{} on {} is {}; press y again to execute",
                plan.kind.label(),
                plan.table,
                plan.risk
            );
            self.operation_plan = Some(plan);
            return;
        }

        if self.readonly {
            self.status = format!(
                "readonly launch blocked {} on {}",
                plan.kind.label(),
                plan.table
            );
            self.operation_plan = Some(plan);
            return;
        }

        if let Some(fixture) = self.fixture.as_mut() {
            match fixture.apply_row_delete(&plan) {
                Ok(summary) => {
                    // Refetch first so the mutation summary is the final
                    // status the operator sees on the refreshed page.
                    self.refetch_current_page();
                    self.status = format!(
                        "fixture executed {} on {} ({}): {}",
                        plan.kind.label(),
                        plan.table,
                        plan.pk_summary,
                        summary
                    );
                }
                Err(message) => {
                    self.error_banner = Some(message);
                    self.status = safe_error_summary(self.error_banner.as_deref().unwrap_or(""));
                }
            }
        } else if let Some(service) = &self.service {
            let stale = self.data.as_ref().is_none_or(|state| {
                state.table != plan.table
                    || state.offset != plan.page_offset
                    || plan.local_index() >= state.rows.len()
            });
            if stale {
                self.status = "staged page no longer current; delete plan dropped".to_string();
                return;
            }
            let Some(state) = self.data.as_ref() else {
                return;
            };
            // The bounded delete path: the current page rows plus a single
            // deleted index; pending cell edits stay empty so the service
            // generates exactly one PK-anchored DELETE via
            // value_convert::build_pk_where.
            service.send(SqliteCommand::Crud(CrudCommand::SaveChanges {
                table: plan.table.clone(),
                columns: state.columns.clone(),
                rows: state.rows.clone(),
                pk_columns: state.pks.clone(),
                pending: HashMap::new(),
                deleted: vec![plan.local_index()],
            }));
            self.status = format!("{} sent to SqliteService", plan.kind.label());
        } else {
            self.status = "no service connection; plan dropped".to_string();
        }
    }

    fn cancel_plan(&mut self) {
        if self.operation_plan.take().is_some() {
            self.status = "operation plan cancelled".to_string();
        } else {
            self.mode = Mode::Browser;
            self.status = "browser mode".to_string();
        }
    }

    fn request_quit(&mut self) {
        let work_active = self.schema_pending
            || self.structure_pending
            || self.data_pending
            || self.operation_plan.is_some();
        if work_active {
            self.overlay = Overlay::ConfirmQuit;
            self.status =
                "quit? a schema/data fetch or staged plan is active: y quits, any other key stays"
                    .to_string();
            return;
        }
        self.should_quit = true;
    }

    fn show_help(&mut self) {
        self.overlay = Overlay::Help;
        self.status = "help open; any key closes".to_string();
    }

    // === Data viewer selection ===

    fn data_row_selection(&self) -> usize {
        self.data.as_ref().map_or(0, |state| state.row_selection)
    }

    fn set_data_selection(&mut self, value: usize) {
        if let Some(state) = self.data.as_mut() {
            let len = state.filtered_rows().len();
            state.row_selection = value.min(len.saturating_sub(1));
        }
    }

    fn move_data_selection(&mut self, delta: isize) {
        let len = self
            .data
            .as_ref()
            .map(|state| state.filtered_rows().len())
            .unwrap_or(0);
        if len == 0 {
            self.set_data_selection(0);
            return;
        }
        let last = len as isize - 1;
        let next = (self.data_row_selection() as isize + delta).clamp(0, last) as usize;
        self.set_data_selection(next);
    }

    fn structure_columns(&self) -> Vec<ColumnInfo> {
        self.structure_view
            .as_ref()
            .map(|view| view.columns.clone())
            .unwrap_or_default()
    }

    // === Drawing ===

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                // Header and status each carry two content rows (title+tabs,
                // status+hints) on top of their borders.
                Constraint::Length(4),
                Constraint::Min(12),
                Constraint::Length(4),
            ])
            .split(area);

        frame.render_widget(self.header(), vertical[0]);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(vertical[1]);
        match self.view {
            View::Tables => {
                frame.render_widget(self.table_list(body[0]), body[0]);
                frame.render_widget(self.table_details_panel(), body[1]);
            }
            View::Structure => {
                frame.render_widget(self.structure_columns_panel(body[0]), body[0]);
                frame.render_widget(self.structure_details_panel(), body[1]);
            }
        }
        frame.render_widget(self.status_panel(), vertical[2]);

        match self.overlay {
            Overlay::None => {}
            Overlay::Help => self.draw_help(frame, area),
            Overlay::DataViewer => self.draw_data_viewer(frame, area),
            Overlay::RowViewer => self.draw_row_viewer(frame, area),
            Overlay::ConfirmDeleteRow => self.draw_confirm_delete(frame, area),
            Overlay::ConfirmQuit => self.draw_confirm_quit(frame, area),
        }
        if let Some(error) = &self.error_banner {
            self.draw_error_banner(frame, area, error);
        }
    }

    fn header(&self) -> Paragraph<'_> {
        let source = format!("{:?}", self.source).to_lowercase();
        let tables_mark = if self.schema_pending || self.data_pending {
            "\u{27f3}"
        } else {
            ""
        };
        let structure_mark = if self.structure_pending {
            "\u{27f3}"
        } else {
            ""
        };
        let tables_tab = format!("1:tables{tables_mark}");
        let structure_tab = format!("2:structure{structure_mark}");
        let tabs = match self.view {
            View::Tables => format!("[{tables_tab}] {structure_tab}"),
            View::Structure => format!("{tables_tab} [{structure_tab}]"),
        };
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "SQLite Data TUI (pilot)",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  profile={} file={}",
                    self.profile_label, self.file_label
                )),
            ]),
            Line::from(format!(
                "{}  source={} kind={} purpose={} readonly={} restore={} filter={}",
                tabs,
                source,
                self.connection_kind,
                self.purpose,
                self.readonly,
                self.restore,
                self.filter_label()
            )),
        ])
        .block(Block::default().borders(Borders::ALL))
    }

    fn table_list(&self, area: Rect) -> Paragraph<'_> {
        let items = self.filtered_tables();
        let visible = area.height.saturating_sub(2).max(1) as usize;
        let start = window_start(self.selected(), visible, items.len());
        let lines = items
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(idx, meta)| {
                let marker = if idx == self.selected() { ">" } else { " " };
                let style = if idx == self.selected() {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(
                    format!(
                        "{marker} {:<24} {:<10} {:>8}",
                        meta.name,
                        meta.table_type,
                        meta.rows
                            .map(|rows| rows.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        Paragraph::new(if !lines.is_empty() {
            lines
        } else if self.schema_pending {
            vec![Line::from("loading\u{2026}")]
        } else {
            vec![Line::from(
                "No tables visible. Press r to refresh or / to filter.",
            )]
        })
        .block(
            Block::default()
                .title(format!(
                    " tables ({}/{}) views={} ",
                    self.selected().saturating_add(1).min(items.len().max(1)),
                    items.len(),
                    self.views.len()
                ))
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false })
    }

    fn table_details_panel(&self) -> Paragraph<'_> {
        let mut lines = match self.filtered_tables().get(self.selected()) {
            Some(meta) => vec![
                Line::from(format!("table: {}", meta.name)),
                Line::from(format!(
                    "type: {}  rows: {}",
                    meta.table_type,
                    meta.rows
                        .map(|rows| rows.to_string())
                        .unwrap_or_else(|| "-".to_string())
                )),
                Line::from("i/Enter opens the bounded data viewer"),
                Line::from("row delete: d inside the viewer (typed table name + y)"),
            ],
            None => vec![Line::from(
                "Select a table to page through its rows. Views are counted in the list title.",
            )],
        };

        if let Some(plan) = &self.operation_plan {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "operation plan",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!("action: {}", plan.kind.label())));
            lines.push(Line::from(format!(
                "target: {} row {}",
                plan.table, plan.pk_summary
            )));
            lines.push(Line::from(format!("risk: {}", plan.risk)));
            lines.push(Line::from(format!(
                "confirmations: {}/{}",
                plan.confirmations, plan.confirmations_required
            )));
        }

        Paragraph::new(lines)
            .block(Block::default().title(" Details ").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    fn structure_columns_panel(&self, area: Rect) -> Paragraph<'_> {
        let Some(view) = &self.structure_view else {
            let hint = if self.structure_pending {
                "loading\u{2026}"
            } else {
                "No structure loaded. Select a table in tables view (1), then press 2."
            };
            return Paragraph::new(vec![Line::from(hint)]).block(
                Block::default()
                    .title(format!(
                        " columns \u{b7} {} ",
                        self.structure_request.as_deref().unwrap_or("-")
                    ))
                    .borders(Borders::ALL),
            );
        };
        let count = view.columns.len();
        let visible = area.height.saturating_sub(2).max(1) as usize;
        let start = window_start(self.structure_selection, visible, count);
        let mut lines: Vec<Line> = view
            .columns
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(idx, column)| {
                let marker = if idx == self.structure_selection {
                    ">"
                } else {
                    " "
                };
                let style = if idx == self.structure_selection {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(
                    format!(
                        "{marker} {:<22} {:<18} {:<4} {:<4} {}",
                        column.name,
                        column.data_type,
                        if column.nullable { "Y" } else { "N" },
                        if column.is_primary_key { "PRI" } else { "" },
                        column.default_value.as_deref().unwrap_or("")
                    ),
                    style,
                ))
            })
            .collect();
        if lines.is_empty() {
            lines.push(Line::from("(no columns)"));
        }
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(
                        " columns ({}/{}) \u{b7} {} ",
                        (self.structure_selection + 1).min(count.max(1)),
                        count,
                        view.table
                    ))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
    }

    /// Right panel of the structure view: indexes, foreign keys, and the
    /// CREATE TABLE DDL rendered read-only (bounded to
    /// `STRUCTURE_DDL_LINE_LIMIT` lines with a visible truncation marker).
    fn structure_details_panel(&self) -> Paragraph<'_> {
        let mut lines: Vec<Line> = Vec::new();
        match &self.structure_view {
            Some(view) => {
                lines.push(Line::from(Span::styled(
                    "indexes",
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                if view.indexes.is_empty() {
                    lines.push(Line::from("  (none)"));
                }
                for index in &view.indexes {
                    lines.push(Line::from(format!(
                        "  {} ({}) [{}]",
                        index.name,
                        index.columns.join(", "),
                        if index.unique {
                            "UNIQUE".to_string()
                        } else {
                            index.index_type.clone()
                        }
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "foreign keys",
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                if view.foreign_keys.is_empty() {
                    lines.push(Line::from("  (none)"));
                }
                for fk in &view.foreign_keys {
                    lines.push(Line::from(format!(
                        "  {} ({}) -> {} ({}) on_update={} on_delete={}",
                        fk.name,
                        fk.columns.join(", "),
                        fk.referenced_table,
                        fk.referenced_columns.join(", "),
                        fk.on_update,
                        fk.on_delete
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "create statement (read-only)",
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                match &view.ddl {
                    Some(ddl) => {
                        let ddl_lines: Vec<&str> = ddl.lines().collect();
                        let shown = ddl_lines.len().min(STRUCTURE_DDL_LINE_LIMIT);
                        for line in ddl_lines.iter().take(shown) {
                            lines.push(Line::from(format!("  {line}")));
                        }
                        if ddl_lines.len() > shown {
                            lines.push(Line::from(format!(
                                "  \u{2026} {} more DDL lines (bounded)",
                                ddl_lines.len() - shown
                            )));
                        }
                    }
                    None => lines.push(Line::from("  loading\u{2026}")),
                }
            }
            None => {
                lines.push(Line::from(
                    "Structure loads here for the selected table (r refreshes).",
                ));
            }
        }
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Indexes & FKs & DDL ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
    }

    fn status_panel(&self) -> Paragraph<'_> {
        let hints = if self.data_filtering {
            let (filter, matches) = self
                .data
                .as_ref()
                .map(|state| (state.filter.clone(), state.filtered_rows().len()))
                .unwrap_or_default();
            format!(
                "viewer filter: {}\u{258f} Enter apply \u{b7} Esc clear \u{b7} {matches} match(es)",
                if filter.is_empty() { "" } else { &filter }
            )
        } else if self.mode == Mode::Filter {
            format!(
                "filter: {}\u{258f} Enter apply \u{b7} Esc clear \u{b7} {} match(es)",
                self.filter_label(),
                self.filtered_tables().len()
            )
        } else {
            match self.overlay {
                Overlay::RowViewer => ROW_HINTS.to_string(),
                Overlay::DataViewer => DATA_HINTS.to_string(),
                _ => match self.view {
                    View::Tables => TABLES_HINTS.to_string(),
                    View::Structure => STRUCTURE_HINTS.to_string(),
                },
            }
        };
        let (data_window, data_total, data_matches) = self
            .data
            .as_ref()
            .map(|state| {
                (
                    if state.rows.is_empty() {
                        "0".to_string()
                    } else {
                        format!(
                            "{}-{}",
                            state.offset + 1,
                            state.offset + state.rows.len() as u64
                        )
                    },
                    state.total,
                    state.filtered_rows().len(),
                )
            })
            .unwrap_or(("-".to_string(), 0, 0));
        Paragraph::new(vec![
            Line::from(format!(
                "{} | mode={} | tables {}/{} views {} | structure={} | data rows {} of {} (match {}) | viewer {}/{} dropped {} | pending tables={} structure={} data={}",
                self.status,
                mode_label(self.mode),
                self.filtered_tables().len(),
                self.tables.len(),
                self.views.len(),
                self.structure_request.as_deref().unwrap_or("-"),
                data_window,
                data_total,
                data_matches,
                self.row_viewer_lines.len(),
                self.row_viewer_lines.max_lines,
                self.row_viewer_lines.dropped_lines,
                self.schema_pending,
                self.structure_pending,
                self.data_pending,
            )),
            Line::from(Span::styled(hints, Style::default().fg(Color::DarkGray))),
        ])
        .block(Block::default().title(" Status ").borders(Borders::ALL))
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(78, 84, area);
        frame.render_widget(Clear, popup);
        let help = vec![
            Line::from(Span::styled(
                "Navigation",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("1 tables browser \u{b7} 2 structure (Tab cycles)"),
            Line::from("j/k or arrows move \u{b7} PgUp/PgDn page \u{b7} g/G home/end"),
            Line::from("/ client-side filter (live, Esc clears)"),
            Line::from(Span::styled(
                "Schema & paging",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("tables browser shows name/type/exact row count for every table"),
            Line::from("2 shows columns, indexes, foreign keys, and read-only CREATE TABLE DDL"),
            Line::from("r refresh current view"),
            Line::from(Span::styled(
                "Inspect",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("i/Enter on a table: bounded data viewer (LIMIT/OFFSET pages)"),
            Line::from(
                "viewer: j/k PgUp/PgDn move \u{b7} n next page \u{b7} b back \u{b7} / filter page \u{b7} i row detail \u{b7} Esc close",
            ),
            Line::from(
                "row viewer: typed columns; NULL renders literally; BLOB shows a hex head \u{b7} Esc back",
            ),
            Line::from(Span::styled(
                "Mutations (staged plans, y confirms)",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("d on a row: delete (type the exact table name, then y)"),
            Line::from(
                "single PK-anchored delete only \u{b7} tables without a primary key refuse \u{b7} readonly launches block",
            ),
            Line::from(Span::styled(
                "Sessions",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("q: quit (asks for confirmation while a fetch or plan is active)"),
            Line::from("errors appear in a non-modal banner; any key acknowledges"),
        ];
        frame.render_widget(
            Paragraph::new(help)
                .block(Block::default().title(" Help ").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn draw_data_viewer(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(92, 84, area);
        frame.render_widget(Clear, popup);
        let Some(state) = self.data.as_ref() else {
            frame.render_widget(
                Paragraph::new("no table data loaded")
                    .block(Block::default().title(" data ").borders(Borders::ALL)),
                popup,
            );
            return;
        };
        let matches = state.filtered_rows();
        let visible = popup.height.saturating_sub(2).max(4) as usize;
        let header_rows = 2usize;
        let body_visible = visible.saturating_sub(header_rows).max(1);
        let start = window_start(self.data_row_selection(), body_visible, matches.len());

        let row_refs: Vec<&Row> = matches.iter().map(|(_, row)| *row).collect();
        let widths = column_widths(&state.columns, &row_refs);
        let header_line = Line::from(Span::styled(
            format!("  {}", grid_row(&state.columns, None, &widths)),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        let separator = Line::from(format!(
            "  {}",
            widths
                .iter()
                .map(|width| "\u{2500}".repeat(*width))
                .collect::<Vec<_>>()
                .join("  ")
        ));

        let mut body: Vec<Line> = vec![header_line, separator];
        for (position, (_, row)) in matches.iter().enumerate().skip(start).take(body_visible) {
            let marker = if position == self.data_row_selection() {
                ">"
            } else {
                " "
            };
            let style = if position == self.data_row_selection() {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            body.push(Line::from(Span::styled(
                format!("{marker}{}", grid_row(&state.columns, Some(*row), &widths)),
                style,
            )));
        }
        if state.loading && state.rows.is_empty() {
            body.push(Line::from("loading\u{2026}"));
        } else if state.total == 0 {
            body.push(Line::from(
                "No rows in this table (total 0). Press Esc to close.",
            ));
        } else if state.rows.is_empty() {
            body.push(Line::from(
                "No rows on this page. Press b to go back, n for the next page, or r to refetch.",
            ));
        } else if matches.is_empty() {
            body.push(Line::from(
                "No rows match the filter on this page. Esc in the filter clears it.",
            ));
        }

        if self.data_filtering {
            body.push(Line::from(Span::styled(
                format!("filter: {}\u{258f}", state.filter),
                Style::default().fg(Color::Cyan),
            )));
        }

        let window = if state.rows.is_empty() {
            "0".to_string()
        } else {
            format!(
                "{}-{}",
                state.offset + 1,
                state.offset + state.rows.len() as u64
            )
        };
        let pk_label = if state.pks.is_empty() {
            String::new()
        } else {
            format!(" \u{b7} pk {}", state.pks.join(","))
        };
        frame.render_widget(
            Paragraph::new(body)
                .block(
                    Block::default()
                        .title(format!(
                            " data {} \u{b7} rows {window} of {} \u{b7} offset {}{} \u{b7} j/k move \u{b7} n page \u{b7} b back \u{b7} i row detail \u{b7} d del row \u{b7} Esc close ",
                            state.table, state.total, state.offset, pk_label
                        ))
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn draw_row_viewer(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(88, 84, area);
        frame.render_widget(Clear, popup);
        let visible = popup.height.saturating_sub(2).max(1) as usize;
        let total = self.row_viewer_lines.len();
        let body: Vec<Line> = if total == 0 {
            vec![Line::from("no row selected; press Enter on a row")]
        } else {
            let start = self.row_viewer_scroll.min(total.saturating_sub(1));
            self.row_viewer_lines
                .range(start, start + visible)
                .into_iter()
                .map(Line::from)
                .collect()
        };
        frame.render_widget(
            Paragraph::new(body)
                .block(
                    Block::default()
                        .title(format!(
                            " row detail \u{b7} {} \u{b7} row {} \u{b7} line {}/{} \u{b7} j/k PgUp/PgDn scroll \u{b7} d del row \u{b7} Esc close ",
                            self.row_viewer_table.as_deref().unwrap_or("(none)"),
                            self.row_viewer_offset + 1,
                            (self.row_viewer_scroll + 1).min(total.max(1)),
                            total
                        ))
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn draw_confirm_delete(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(64, 26, area);
        frame.render_widget(Clear, popup);
        let target = self
            .data
            .as_ref()
            .map(|state| {
                let row = state
                    .selected_row()
                    .map(|(_, row)| pk_summary(&state.columns, &row, &state.pks))
                    .unwrap_or_else(|| "(no row selected)".to_string());
                format!("{} row {}", state.table, row)
            })
            .unwrap_or_else(|| "no table data loaded".to_string());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Delete row (destructive)",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("target: {target}")),
                Line::from(
                    "One PK-anchored DELETE is sent; the whole table is never targeted.",
                ),
                Line::from("Type the exact table name to confirm:"),
                Line::from(Span::styled(
                    format!("{}\u{258f}", self.typed_table_input),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(
                    "Enter stages the plan (typed confirmation) \u{b7} y executes \u{b7} Esc cancels",
                ),
            ])
            .block(
                Block::default()
                    .title(" Confirm Delete Row ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn draw_confirm_quit(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(56, 22, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Quit with active work?",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(self.active_work_summary()),
                Line::from("y/Enter: quit \u{b7} any other key: stay"),
            ])
            .block(
                Block::default()
                    .title(" Confirm Quit ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn active_work_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.schema_pending {
            parts.push("tables fetch in flight".to_string());
        }
        if self.structure_pending {
            parts.push("structure fetch in flight".to_string());
        }
        if self.data_pending {
            parts.push("data page fetch in flight".to_string());
        }
        if let Some(plan) = &self.operation_plan {
            parts.push(format!("{} plan on {}", plan.kind.label(), plan.table));
        }
        if parts.is_empty() {
            "no active fetches or plans".to_string()
        } else {
            parts.join(", ")
        }
    }

    fn draw_error_banner(&self, frame: &mut Frame, area: Rect, error: &str) {
        let banner = Rect {
            x: area.x + 1,
            y: area.height.saturating_sub(6),
            width: area.width.saturating_sub(2),
            height: 3,
        };
        frame.render_widget(Clear, banner);
        frame.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                safe_error_summary(error),
                Style::default().fg(Color::Red),
            ))])
            .block(
                Block::default()
                    .title(" Error (any key acknowledges) ")
                    .borders(Borders::ALL)
                    .style(Style::default().fg(Color::Red)),
            )
            .wrap(Wrap { trim: false }),
            banner,
        );
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

impl BoundedLines {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            dropped_lines: 0,
            max_lines,
            max_bytes,
        }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    fn push_line(&mut self, mut line: String) {
        if line.len() > self.max_bytes {
            line.truncate(self.max_bytes);
        }
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines || self.bytes > self.max_bytes {
            if let Some(removed) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.len());
                self.dropped_lines = self.dropped_lines.saturating_add(1);
            } else {
                break;
            }
        }
    }

    fn range(&self, start: usize, end: usize) -> Vec<String> {
        self.lines
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .cloned()
            .collect()
    }
}

fn cell_display(value: &CellValue) -> String {
    let text = value.display();
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

/// Hex head for a BLOB cell in the row detail viewer. Mirrors the storage
/// probe in `service/value_convert.rs` (`decode_sqlite_value` maps SQLite
/// blobs to `CellValue::Blob`); only the first 16 bytes render so a huge
/// blob cannot flood the bounded viewer.
fn blob_hex_head(bytes: &[u8]) -> String {
    let head: Vec<String> = bytes
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let suffix = if bytes.len() > 16 { " \u{2026}" } else { "" };
    format!("hex {}{}", head.join(" "), suffix)
}

fn cell_value_json(value: &CellValue) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn pk_summary(columns: &[ColumnInfo], row: &Row, pks: &[String]) -> String {
    if pks.is_empty() {
        return "(no primary key)".to_string();
    }
    pks.iter()
        .map(|pk| {
            let value = columns
                .iter()
                .position(|column| column.name == *pk)
                .and_then(|index| row.values.get(index))
                .map(cell_display)
                .unwrap_or_else(|| "?".to_string());
            format!("{pk}={value}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn column_widths(columns: &[ColumnInfo], rows: &[&Row]) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut width = column.name.chars().count();
            for row in rows {
                if let Some(value) = row.values.get(index) {
                    width = width.max(cell_display(value).chars().count());
                }
            }
            width.clamp(4, CELL_MAX_WIDTH)
        })
        .collect()
}

fn fit_cell(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        format!("{text:<width$}")
    } else {
        let truncated: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{truncated}\u{2026}")
    }
}

fn grid_row(columns: &[ColumnInfo], row: Option<&Row>, widths: &[usize]) -> String {
    widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let text = match row {
                Some(row) => row.values.get(index).map(cell_display).unwrap_or_default(),
                None => columns
                    .get(index)
                    .map(|column| column.name.clone())
                    .unwrap_or_default(),
            };
            fit_cell(&text, *width)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn safe_error_summary(message: &str) -> String {
    let mut summary = message.replace('\n', " ");
    if summary.len() > 180 {
        summary.truncate(177);
        summary.push_str("...");
    }
    summary
}

fn secret_leak_markers(text: &str) -> Vec<String> {
    [
        "sqlite_password_value",
        "sqlite_secret_data_value",
        "plaintext_credential_value",
        "raw_connection_url",
    ]
    .into_iter()
    .filter(|marker| text.contains(marker))
    .map(str::to_string)
    .collect()
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Browser => "browser",
        Mode::Filter => "filter",
    }
}

fn window_start(selected: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        0
    } else if selected >= visible {
        (selected + 1).saturating_sub(visible)
    } else {
        0
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn table_meta_value(meta: &TableMeta) -> Value {
    json!({
        "name": meta.name,
        "table_type": meta.table_type,
        "rows": meta.rows
    })
}

fn table_data_value((table, data): (&String, &FixtureTable)) -> Value {
    json!({
        "table": table,
        "columns": data.columns,
        "pks": data.pks,
        "total_rows": data.rows.len(),
        "rows": data
            .rows
            .iter()
            .map(|row| {
                Value::Array(row.values.iter().map(cell_value_json).collect())
            })
            .collect::<Vec<_>>()
    })
}

fn structure_value((table, structure): (&String, &FixtureStructure)) -> Value {
    json!({
        "table": table,
        "columns": structure.columns,
        "indexes": structure.indexes,
        "foreign_keys": structure.foreign_keys,
        "ddl": structure.ddl
    })
}

// ---------------------------------------------------------------------------
// Standalone tab manager
// ---------------------------------------------------------------------------

struct StandaloneSqliteTabManager {
    render_tx: tokio::sync::mpsc::UnboundedSender<()>,
    should_quit: Arc<AtomicBool>,
}

impl StandaloneSqliteTabManager {
    fn new(should_quit: Arc<AtomicBool>) -> Self {
        let (render_tx, _render_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            render_tx,
            should_quit,
        }
    }

    fn unsupported_tabs_error() -> anyhow::Error {
        anyhow::anyhow!(
            "SQLite TUI does not host plugin tabs; use plugin-owned CLI commands instead"
        )
    }
}

impl TabManager for StandaloneSqliteTabManager {
    fn open(&self, _title: String, _plugin_id: String, _context: Value) -> Result<()> {
        Err(Self::unsupported_tabs_error())
    }

    fn close_current(&self) -> Result<()> {
        self.quit()
    }

    fn set_title(&self, _title: String) -> Result<()> {
        Ok(())
    }

    fn request_render(&self) -> Result<()> {
        let _ = self.render_tx.send(());
        Ok(())
    }

    fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        Ok(vec![TabInfo {
            index: 0,
            title: "SQLite".to_string(),
            plugin_id: "sqlite".to_string(),
            context: json!({}),
            is_active: true,
        }])
    }

    fn close_tab(&self, index: usize) -> Result<()> {
        if index == 0 {
            self.quit()
        } else {
            Err(anyhow::anyhow!("SQLite TUI has no tab {index}"))
        }
    }

    fn switch_to(&self, index: usize) -> Result<()> {
        if index == 0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("SQLite TUI has no tab {index}"))
        }
    }

    fn active_tab_index(&self) -> Result<usize> {
        Ok(0)
    }

    fn quit(&self) -> Result<()> {
        self.should_quit.store(true, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn fixture_launch(readonly: bool) -> SqliteTuiLaunch {
        SqliteTuiLaunch {
            profile_label: "fixture".to_string(),
            config: None,
            source: SqliteTuiSource::Fixture,
            fixture_path: Some(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("fixtures/sqlite_tui_operations.json")
                    .display()
                    .to_string(),
            ),
            purpose: "operations".to_string(),
            readonly,
            restore: true,
            launch_plan: None,
        }
    }

    fn app() -> SqliteTuiApp {
        SqliteTuiApp::new(fixture_launch(false)).unwrap()
    }

    fn press(app: &mut SqliteTuiApp, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut SqliteTuiApp, text: &str) {
        for ch in text.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }

    /// Open the data viewer for the table at `index` (fixture tables are
    /// sorted by name: documents, empty_archive, keyvalue, locked_table,
    /// sessions, users).
    fn open_table(app: &mut SqliteTuiApp, index: usize) {
        assert_eq!(app.view, View::Tables);
        app.set_selected(index);
        press(app, KeyCode::Enter);
        assert_eq!(app.overlay, Overlay::DataViewer);
    }

    fn sorted_table_names() -> Vec<&'static str> {
        vec![
            "documents",
            "empty_archive",
            "keyvalue",
            "locked_table",
            "sessions",
            "users",
        ]
    }

    #[test]
    fn fixture_evidence_has_coverage_and_no_secret_markers() {
        let evidence = build_sqlite_tui_evidence(&fixture_launch(false)).unwrap();
        let rendered = serde_json::to_string(&evidence).unwrap();
        assert!(rendered.contains("sqlite_tui_fixture_evidence"));
        assert!(rendered.contains("typed_delete_confirmation"));
        assert!(rendered.contains("table_data_paging"));
        assert!(rendered.contains("blob_value_rendering"));
        assert!(rendered.contains("structure_ddl_read_only"));
        assert!(rendered.contains("delete_requires_primary_key"));
        assert!(rendered.contains("row_viewer_typed_columns"));
        assert!(rendered.contains("later candidate"));
        assert_eq!(evidence["secret_leak_scan"]["passed"], json!(true));
        assert_eq!(evidence["quality_gate"]["plugin_id"], json!("sqlite"));
        assert_eq!(
            evidence["quality_gate"]["layout"]["minimum_viewport"]["width"],
            json!(80)
        );
    }

    #[test]
    fn preflight_reports_file_target_without_leaking_paths() {
        let launch = SqliteTuiLaunch {
            profile_label: "live-sqlite".to_string(),
            config: Some(SqliteConfig::new(
                "/Users/operator/hidden-dir/app-store.db".to_string(),
            )),
            source: SqliteTuiSource::Profile,
            fixture_path: None,
            purpose: "triage".to_string(),
            readonly: false,
            restore: true,
            launch_plan: None,
        };
        let rendered = serde_json::to_string(&preflight_value(&launch)).unwrap();
        assert!(rendered.contains("redacted"));
        assert!(rendered.contains("app-store.db"), "basename stays visible");
        assert!(
            !rendered.contains("operator"),
            "the absolute path (and any embedded username) must not leak"
        );
        assert!(!rendered.contains("hidden-dir"));
        let value = preflight_value(&launch);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["command"], json!("sqlite tui"));
        assert_eq!(value["connection"]["kind"], json!("file"));
        assert_eq!(value["connection"]["auth"], json!("none"));
        assert_eq!(value["raw_input"], json!(false));
    }

    #[test]
    fn startup_target_error_banner_is_non_modal() {
        let mut app = app();
        assert!(app.error_banner.is_some());
        assert!(
            app.error_banner
                .as_deref()
                .unwrap_or_default()
                .contains("target error"),
            "the fixture target error reaches the banner family marker"
        );
        assert_eq!(app.mode, Mode::Browser, "errors must not hijack the mode");
        assert_eq!(app.view, View::Tables);
        assert_eq!(app.tables.len(), 6, "the fixture catalog is loaded eagerly");
        assert_eq!(app.views.len(), 1);
        press(&mut app, KeyCode::Char('j'));
        assert!(app.error_banner.is_none(), "any key acknowledges");
        assert_eq!(
            app.table_selection, 1,
            "the cursor kept its meaning while the banner was open"
        );
    }

    #[test]
    fn hotkeys_switch_views_and_tab_cycles() {
        let mut app = app();
        assert_eq!(app.view, View::Tables);
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.view, View::Structure);
        assert!(
            app.status.contains("structure loaded for"),
            "the structure snapshot loads for the selected table"
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Tables);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.view, View::Structure);
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.view, View::Tables);
    }

    #[test]
    fn tables_browser_reports_row_counts() {
        let app = app();
        let tables = app.filtered_tables();
        let names: Vec<&str> = tables.iter().map(|meta| meta.name.as_str()).collect();
        assert_eq!(names, sorted_table_names());
        let users = tables
            .iter()
            .find(|meta| meta.name == "users")
            .expect("users table");
        assert_eq!(users.rows, Some(12), "row counts come from the catalog");
        assert_eq!(users.table_type, "table");
    }

    #[test]
    fn table_data_paging_tracks_offset_and_total() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, index);
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.table, "users");
        assert_eq!(state.total, 12);
        assert_eq!(state.offset, 0);
        assert_eq!(state.rows.len(), 5, "one bounded page, not the whole table");
        assert_eq!(state.pks, vec!["id".to_string()]);

        press(&mut app, KeyCode::Char('n'));
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.offset, 5);
        assert_eq!(state.rows.len(), 5);

        press(&mut app, KeyCode::Char('n'));
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.offset, 10);
        assert_eq!(state.rows.len(), 2, "partial last page is visible");

        press(&mut app, KeyCode::Char('n'));
        assert!(app.status.contains("end of table"));

        press(&mut app, KeyCode::Char('b'));
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.offset, 5);

        press(&mut app, KeyCode::Char('b'));
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.offset, 0);

        press(&mut app, KeyCode::Char('b'));
        assert!(app.status.contains("no previous page"));

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.data.is_none(), "closing the viewer drops the page");
    }

    #[test]
    fn filter_reports_match_count_in_tables_and_viewer() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'));
        assert_eq!(app.mode, Mode::Filter);
        type_text(&mut app, "key");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Browser);
        assert_eq!(app.filtered_tables().len(), 1);
        assert!(app.status.contains("(1 matches)"));

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.status, "browser mode");

        // Re-open the filter and clear it with Esc.
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Esc);
        assert!(app.status.contains("filter cleared"));
        assert_eq!(app.filtered_tables().len(), 6);

        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('/'));
        assert!(app.data_filtering);
        type_text(&mut app, "alice");
        press(&mut app, KeyCode::Enter);
        assert!(!app.data_filtering);
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.filtered_rows().len(), 1);
        assert!(app.status.contains("(1 matches)"));

        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Esc);
        assert!(!app.data_filtering);
        let state = app.data.as_ref().expect("data viewer state");
        assert!(state.filter.is_empty());
        assert!(app.status.contains("viewer filter cancelled"));
    }

    #[test]
    fn row_viewer_renders_typed_columns_with_null_and_blob() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "sessions")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay, Overlay::RowViewer);
        let rendered = app
            .row_viewer_lines
            .range(0, app.row_viewer_lines.len())
            .join("\n");
        assert!(rendered.contains("token"), "column names render");
        assert!(rendered.contains("TEXT"));
        assert!(rendered.contains("PK"));
        assert!(rendered.contains("NULL"), "NULL values render literally");
        assert!(
            rendered.contains("[BLOB 4 bytes]"),
            "blob size label renders"
        );
        assert!(
            rendered.contains("hex de ad be ef"),
            "the blob hex head renders below the size label"
        );
        assert!(rendered.contains("2026-11-02 18:30:00"), "datetime renders");

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, Overlay::DataViewer);

        // Long text stays inside the bounded row viewer buffer. Mallory
        // (row 12) sits on the third page of the users fixture table.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, Overlay::None);
        let users_index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, users_index);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('n'));
        app.set_data_selection(1); // mallory, long settings_json
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay, Overlay::RowViewer);
        let rendered = app
            .row_viewer_lines
            .range(0, app.row_viewer_lines.len())
            .join("\n");
        assert!(rendered.contains("telemetry-optout"));
        assert!(rendered.contains("TEXT"));
        assert!(
            app.row_viewer_lines.len() <= VIEWER_LINE_LIMIT,
            "the bounded buffer respects its line budget"
        );
    }

    #[test]
    fn structure_view_shows_columns_indexes_fks_and_ddl() {
        let mut app = app();
        // Select sessions in the tables view; the structure view loads the
        // snapshot for the table selected there.
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "sessions")
            .unwrap();
        app.set_selected(index);
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.view, View::Structure);
        let view = app.structure_view.as_ref().expect("structure snapshot");
        assert_eq!(view.table, "sessions");
        assert_eq!(view.columns.len(), 4);
        assert_eq!(view.indexes.len(), 1);
        assert_eq!(view.indexes[0].name, "idx_sessions_user_id");
        assert!(!view.indexes[0].unique);
        assert_eq!(view.foreign_keys.len(), 1);
        assert_eq!(view.foreign_keys[0].referenced_table, "users");
        let ddl = view.ddl.as_deref().expect("fixture ddl");
        assert!(ddl.contains("CREATE TABLE"));

        // Switching tables re-loads the structure: documents carries a
        // UNIQUE index.
        press(&mut app, KeyCode::Char('1'));
        let documents_index = sorted_table_names()
            .iter()
            .position(|name| *name == "documents")
            .unwrap();
        app.set_selected(documents_index);
        press(&mut app, KeyCode::Char('2'));
        let view = app.structure_view.as_ref().expect("documents structure");
        assert_eq!(view.table, "documents");
        assert!(view.indexes[0].unique);
        assert_eq!(app.structure_request.as_deref(), Some("documents"));

        // A missing structure snapshot surfaces a target error without
        // leaving the structure view, and the banner stays non-modal.
        app.fixture
            .as_mut()
            .expect("fixture")
            .structures
            .remove("documents");
        press(&mut app, KeyCode::Char('1'));
        app.set_selected(documents_index);
        app.structure_view = None; // force past the cached-snapshot early return
        press(&mut app, KeyCode::Char('2'));
        assert!(app.error_banner.is_some());
        assert!(
            app.error_banner
                .as_deref()
                .unwrap_or_default()
                .contains("no fixture structure")
        );
        assert!(app.structure_view.is_none());
        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.view, View::Tables);
        assert!(
            app.error_banner.is_none(),
            "banner stays non-modal in tables view"
        );
    }

    #[test]
    fn typed_delete_requires_exact_table_name() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.overlay, Overlay::ConfirmDeleteRow);
        type_text(&mut app, "sessions");
        press(&mut app, KeyCode::Enter);
        assert!(app.operation_plan.is_none());
        assert!(app.status.contains("typed name does not match users"));
        assert_eq!(app.overlay, Overlay::ConfirmDeleteRow);
        assert!(
            app.typed_table_input.is_empty(),
            "a mismatched name clears the typed input"
        );

        type_text(&mut app, "users");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overlay, Overlay::DataViewer);
        let plan = app.operation_plan.as_ref().expect("staged plan");
        assert_eq!(plan.kind, OperationKind::DeleteRow);
        assert_eq!(plan.table, "users");
        assert_eq!(plan.row_offset, 0);
        assert_eq!(plan.confirmations_required, 2);
        assert_eq!(plan.confirmations, 1);
        assert_eq!(plan.pk_summary, "id=1");
        assert!(app.status.contains("typed confirmation recorded"));
    }

    #[test]
    fn delete_executes_after_y_and_refetches_page() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('d'));
        type_text(&mut app, "users");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        assert!(app.operation_plan.is_none());
        assert!(app.status.contains("fixture executed delete row"));
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.total, 11, "the refetched page reports the new total");
        assert_eq!(
            state.rows[0].values[0],
            CellValue::Int(2),
            "the deleted row is gone from the refetched page"
        );
    }

    #[test]
    fn delete_refused_without_primary_key() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "keyvalue")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('d'));
        assert!(app.status.contains("no primary key"));
        assert_eq!(
            app.overlay,
            Overlay::DataViewer,
            "the refusal keeps the operator in the data viewer"
        );
        assert!(app.operation_plan.is_none());
    }

    #[test]
    fn readonly_launch_blocks_confirmed_plan() {
        let mut app = SqliteTuiApp::new(fixture_launch(true)).unwrap();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('d'));
        type_text(&mut app, "users");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        assert!(app.operation_plan.is_some());
        assert!(app.status.contains("readonly launch blocked"));
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(
            state.total, 12,
            "readonly launches must not mutate the table"
        );
    }

    #[test]
    fn erroring_delete_shows_banner_and_stays_responsive() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "locked_table")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('d'));
        type_text(&mut app, "locked_table");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('y'));
        assert!(app.error_banner.is_some());
        assert!(app.status.contains("target error"));
        assert!(app.operation_plan.is_none());
        let matches_before = app
            .data
            .as_ref()
            .map(|state| state.filtered_rows().len())
            .unwrap_or(0);
        assert_eq!(
            matches_before, 1,
            "a failed mutation must not remove the row"
        );
        press(&mut app, KeyCode::Char('j'));
        assert!(app.error_banner.is_none(), "banner stays non-modal");
        assert_eq!(
            app.data_row_selection(),
            0,
            "the cursor keeps its meaning (clamped to the single row)"
        );
    }

    #[test]
    fn empty_table_shows_empty_state() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "empty_archive")
            .unwrap();
        open_table(&mut app, index);
        let state = app.data.as_ref().expect("data viewer state");
        assert_eq!(state.total, 0);
        assert!(state.rows.is_empty());
        assert!(!state.loading);
        press(&mut app, KeyCode::Char('n'));
        assert!(app.status.contains("end of table"));
        press(&mut app, KeyCode::Char('d'));
        assert!(
            app.status.contains("select a row to delete"),
            "mutations refuse to stage with nothing selected"
        );
        assert!(app.operation_plan.is_none());
    }

    #[test]
    fn quit_with_active_plan_requires_confirmation() {
        let mut app = app();
        let index = sorted_table_names()
            .iter()
            .position(|name| *name == "users")
            .unwrap();
        open_table(&mut app, index);
        press(&mut app, KeyCode::Char('d'));
        type_text(&mut app, "users");
        press(&mut app, KeyCode::Enter);
        // Close the data viewer first; q inside the viewer closes it.
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, Overlay::None);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.overlay, Overlay::ConfirmQuit);
        assert!(app.operation_plan.is_some(), "the staged plan survives");
        assert!(!app.should_quit);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.overlay, Overlay::None);
        assert!(!app.should_quit);
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.should_quit);
    }

    #[test]
    fn page_keys_move_selection() {
        let mut app = app();
        press(&mut app, KeyCode::PageDown);
        assert_eq!(
            app.selected(),
            app.filtered_tables().len() - 1,
            "PageDown clamps to the last table"
        );
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn cell_display_and_fit_cell_render_bounded_values() {
        assert_eq!(cell_display(&CellValue::Null), "NULL");
        assert_eq!(
            cell_display(&CellValue::Blob(vec![0xDE, 0xAD])),
            "[BLOB 2 bytes]"
        );
        assert_eq!(blob_hex_head(&[0xDE, 0xAD, 0xBE, 0xEF]), "hex de ad be ef");
        let long_blob: Vec<u8> = (0..20).collect();
        assert!(
            blob_hex_head(&long_blob).ends_with('\u{2026}'),
            "long blob hex heads carry a visible truncation marker"
        );
        assert_eq!(fit_cell("users", 8), "users   ");
        assert_eq!(fit_cell("a-very-long-cell-value", 8), "a-very-\u{2026}");
        assert_eq!(
            pk_summary(
                &[ColumnInfo {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                    nullable: false,
                    is_primary_key: true,
                    default_value: None,
                    max_length: None,
                    extra: String::new(),
                }],
                &Row {
                    values: vec![CellValue::Int(7)]
                },
                &["id".to_string()]
            ),
            "id=7"
        );
        assert_eq!(
            pk_summary(&[], &Row { values: vec![] }, &[]),
            "(no primary key)"
        );
    }
}
