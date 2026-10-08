//! SQLite service layer.
//!
//! Provides `SqliteService`, the service facade for the SQLite plugin.
//! Unlike MySQL/Postgres (which use tokio::spawn), SQLite uses
//! `SyncWorker` because `rusqlite::Connection` is `!Send`.
//!
//! The worker thread owns the connection and processes commands
//! sequentially. Inner service handler functions execute on the
//! worker thread with `&Connection` access.

pub mod browser;
pub mod commands;
pub mod crud;
pub mod data_loader;
pub mod events;
pub mod schema;
pub mod value_convert;

pub use commands::{
    BrowserCommand, CrudCommand, LoadCommand, PasteMode, SqliteCommand, SchemaCommand,
    TableCopyData,
};
pub use events::{
    BrowserEvent, CrudEvent, IndexInfo, LoadEvent, SchemaEvent, SqliteEvent, StatementResult,
    TableMeta,
};

use std::sync::Arc;

use tokio::sync::mpsc;
use voidb_core::sync_worker::SyncWorker;
use voidb_core::TabManager;

use crate::config::SqliteConfig;

// Internal command/response types for the SyncWorker channel.
// These mirror the public types but are fully owned (no channel types).
enum InternalCmd {
    Schema(commands::SchemaCommand),
    Crud(commands::CrudCommand),
    Load(commands::LoadCommand),
    Browser(commands::BrowserCommand),
}

enum InternalResp {
    Schema(SchemaResult),
    Crud(CrudResult),
    Load(LoadResult),
    Browser(BrowserResult),
}

enum SchemaResult {
    TablesLoaded {
        tables: Vec<events::TableMeta>,
        views: Vec<String>,
    },
    CreateTableDdl {
        table: String,
        ddl: String,
    },
    StructureLoaded {
        table: String,
        columns: Vec<voidb_core::ColumnInfo>,
        indexes: Vec<events::IndexInfo>,
        foreign_keys: Vec<voidb_core::database::types::ForeignKeyInfo>,
    },
    ForeignKeysLoaded {
        table: String,
        foreign_keys: Vec<voidb_core::database::types::ForeignKeyInfo>,
    },
    Error(String),
}

enum CrudResult {
    QueryResult {
        columns: Vec<voidb_core::ColumnInfo>,
        rows: Vec<voidb_core::Row>,
    },
    MultiQueryResult {
        results: Vec<events::StatementResult>,
    },
    ChangesSaved {
        updates: usize,
        deletes: usize,
    },
    RowInserted {
        sql: String,
    },
    PasteComplete {
        inserted: usize,
    },
    ImportComplete,
    Error(String),
}

enum LoadResult {
    TableDataLoaded {
        table: String,
        pks: Vec<String>,
        columns: Vec<voidb_core::ColumnInfo>,
        rows: Vec<voidb_core::Row>,
        total: u64,
    },
    RowCountLoaded {
        table: String,
        count: u64,
    },
    PrimaryKeysLoaded {
        table: String,
        pks: Vec<String>,
    },
    Error(String),
}

enum BrowserResult {
    Event(events::BrowserEvent),
    CopyData(Vec<commands::TableCopyData>),
    Error(String),
}

/// No-op TabManager for direct mode (CLI usage).
struct NoOpTabManager;

impl TabManager for NoOpTabManager {
    fn open(&self, _title: String, _plugin_id: String, _context: serde_json::Value) -> anyhow::Result<()> { Ok(()) }
    fn close_current(&self) -> anyhow::Result<()> { Ok(()) }
    fn close_tab(&self, _index: usize) -> anyhow::Result<()> { Ok(()) }
    fn set_title(&self, _title: String) -> anyhow::Result<()> { Ok(()) }
    fn request_render(&self) -> anyhow::Result<()> { Ok(()) }
    fn list_tabs(&self) -> anyhow::Result<Vec<voidb_core::shell_capabilities::TabInfo>> { Ok(vec![]) }
    fn switch_to(&self, _index: usize) -> anyhow::Result<()> { Ok(()) }
    fn active_tab_index(&self) -> anyhow::Result<usize> { Ok(0) }
    fn quit(&self) -> anyhow::Result<()> { Ok(()) }
}

/// SQLite service facade.
///
/// Owns the SyncWorker and event channels. The worker thread owns
/// the `rusqlite::Connection` and processes commands sequentially.
///
/// # Send + Sync
///
/// `SqliteService` is `Send` but NOT `Sync` (because `UnboundedReceiver`
/// is `!Sync`). Plugin structs wrap it in `std::sync::Mutex` to satisfy
/// `Plugin: Send + Sync`.
pub struct SqliteService {
    worker: SyncWorker<InternalCmd, InternalResp>,
    event_tx: mpsc::UnboundedSender<SqliteEvent>,
    event_rx: mpsc::UnboundedReceiver<SqliteEvent>,
    tabs: Arc<dyn TabManager>,
}

impl SqliteService {
    /// Create a new SqliteService backed by a SyncWorker.
    ///
    /// Opens the SQLite database on a dedicated worker thread.
    /// Returns an error if the database cannot be opened.
    pub fn new(
        config: SqliteConfig,
        tabs: Arc<dyn TabManager>,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<SqliteEvent>();

        let worker = SyncWorker::spawn(
            move || {
                let conn = rusqlite::Connection::open(&config.path)
                    .map_err(|e| format!("Failed to open SQLite database '{}': {}", config.path, e))?;
                conn.execute_batch("PRAGMA foreign_keys = ON;")
                    .map_err(|e| format!("Failed to enable foreign keys: {}", e))?;
                Ok(conn)
            },
            |conn, cmd: InternalCmd| {
                let resp = match cmd {
                    InternalCmd::Schema(sc) => InternalResp::Schema(handle_schema_cmd(conn, sc)),
                    InternalCmd::Crud(cc) => InternalResp::Crud(handle_crud_cmd(conn, cc)),
                    InternalCmd::Load(lc) => InternalResp::Load(handle_load_cmd(conn, lc)),
                    InternalCmd::Browser(bc) => InternalResp::Browser(handle_browser_cmd(conn, bc)),
                };
                Some(resp)
            },
        )?;

        Ok(Self {
            worker,
            event_tx,
            event_rx,
            tabs,
        })
    }

    /// Send a command to the worker thread.
    pub fn send(&self, cmd: SqliteCommand) {
        let internal = match cmd {
            SqliteCommand::Schema(sc) => InternalCmd::Schema(sc),
            SqliteCommand::Crud(cc) => InternalCmd::Crud(cc),
            SqliteCommand::Load(lc) => InternalCmd::Load(lc),
            SqliteCommand::Browser(bc) => InternalCmd::Browser(bc),
        };
        let _ = self.worker.send(internal);
    }

    /// Poll for the next event from the service.
    ///
    /// Drains all available worker responses, translates them to events,
    /// then returns the first buffered event.
    pub fn poll_event(&mut self) -> Option<SqliteEvent> {
        // Drain worker responses into the event channel
        while let Some(resp) = self.worker.try_recv() {
            let event = translate_response(resp);
            let _ = self.event_tx.send(event);
            let _ = self.tabs.request_render();
        }

        self.event_rx.try_recv().ok()
    }

    /// Create a SqliteService in direct mode for CLI usage.
    ///
    /// Spawns the worker thread for the !Send rusqlite::Connection but does
    /// not require a real TabManager. Use direct async methods instead of
    /// send/poll_event.
    pub fn new_direct(config: &SqliteConfig) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::unbounded_channel::<SqliteEvent>();
        let path = config.path.clone();

        let worker = SyncWorker::spawn(
            move || {
                let conn = rusqlite::Connection::open(&path)
                    .map_err(|e| format!("Failed to open SQLite database '{}': {}", path, e))?;
                conn.execute_batch("PRAGMA foreign_keys = ON;")
                    .map_err(|e| format!("Failed to enable foreign keys: {}", e))?;
                Ok(conn)
            },
            |conn, cmd: InternalCmd| {
                let resp = match cmd {
                    InternalCmd::Schema(sc) => InternalResp::Schema(handle_schema_cmd(conn, sc)),
                    InternalCmd::Crud(cc) => InternalResp::Crud(handle_crud_cmd(conn, cc)),
                    InternalCmd::Load(lc) => InternalResp::Load(handle_load_cmd(conn, lc)),
                    InternalCmd::Browser(bc) => InternalResp::Browser(handle_browser_cmd(conn, bc)),
                };
                Some(resp)
            },
        )?;

        Ok(Self {
            worker,
            event_tx,
            event_rx,
            tabs: Arc::new(NoOpTabManager),
        })
    }

    // === Direct mode async methods (CLI) ===

    /// List all tables and views (direct mode).
    pub async fn list_tables(&mut self) -> anyhow::Result<(Vec<TableMeta>, Vec<String>)> {
        self.worker
            .send(InternalCmd::Schema(commands::SchemaCommand::ListTables))
            .map_err(|_| anyhow::anyhow!("Worker channel closed"))?;
        match self.worker.recv().await {
            Some(InternalResp::Schema(SchemaResult::TablesLoaded { tables, views })) => {
                Ok((tables, views))
            }
            Some(InternalResp::Schema(SchemaResult::Error(e))) => Err(anyhow::anyhow!(e)),
            None => Err(anyhow::anyhow!("Worker thread exited")),
            _ => Err(anyhow::anyhow!("Unexpected response")),
        }
    }

    /// Describe a table: columns, indexes, foreign keys (direct mode).
    pub async fn describe_table(
        &mut self,
        table: &str,
    ) -> anyhow::Result<(Vec<voidb_core::ColumnInfo>, Vec<IndexInfo>, Vec<voidb_core::database::types::ForeignKeyInfo>)> {
        self.worker
            .send(InternalCmd::Schema(commands::SchemaCommand::LoadStructure {
                table: table.to_string(),
            }))
            .map_err(|_| anyhow::anyhow!("Worker channel closed"))?;
        match self.worker.recv().await {
            Some(InternalResp::Schema(SchemaResult::StructureLoaded {
                columns,
                indexes,
                foreign_keys,
                ..
            })) => Ok((columns, indexes, foreign_keys)),
            Some(InternalResp::Schema(SchemaResult::Error(e))) => Err(anyhow::anyhow!(e)),
            None => Err(anyhow::anyhow!("Worker thread exited")),
            _ => Err(anyhow::anyhow!("Unexpected response")),
        }
    }

    /// Execute a SQL query (direct mode).
    pub async fn execute_query(
        &mut self,
        sql: &str,
    ) -> anyhow::Result<Vec<StatementResult>> {
        self.worker
            .send(InternalCmd::Crud(commands::CrudCommand::ExecuteMultiQuery {
                sql: sql.to_string(),
            }))
            .map_err(|_| anyhow::anyhow!("Worker channel closed"))?;
        match self.worker.recv().await {
            Some(InternalResp::Crud(CrudResult::MultiQueryResult { results })) => Ok(results),
            Some(InternalResp::Crud(CrudResult::Error(e))) => Err(anyhow::anyhow!(e)),
            None => Err(anyhow::anyhow!("Worker thread exited")),
            _ => Err(anyhow::anyhow!("Unexpected response")),
        }
    }
}

// ---------------------------------------------------------------------------
// Worker-thread dispatch functions (called inside handler_fn)
// ---------------------------------------------------------------------------

fn handle_schema_cmd(conn: &mut rusqlite::Connection, cmd: commands::SchemaCommand) -> SchemaResult {
    match cmd {
        commands::SchemaCommand::ListTables => {
            match schema::handle_list_tables(conn) {
                Ok((tables, views)) => SchemaResult::TablesLoaded { tables, views },
                Err(e) => SchemaResult::Error(e),
            }
        }
        commands::SchemaCommand::ShowCreateTable { table } => {
            match schema::handle_show_create_table(conn, &table) {
                Ok(ddl) => SchemaResult::CreateTableDdl { table, ddl },
                Err(e) => SchemaResult::Error(e),
            }
        }
        commands::SchemaCommand::LoadStructure { table } => {
            match schema::handle_load_structure(conn, &table) {
                Ok((columns, indexes, foreign_keys)) => {
                    SchemaResult::StructureLoaded { table, columns, indexes, foreign_keys }
                }
                Err(e) => SchemaResult::Error(e),
            }
        }
        commands::SchemaCommand::ListForeignKeys { table } => {
            match schema::handle_list_foreign_keys(conn, &table) {
                Ok(foreign_keys) => SchemaResult::ForeignKeysLoaded { table, foreign_keys },
                Err(e) => SchemaResult::Error(e),
            }
        }
    }
}

fn handle_crud_cmd(conn: &mut rusqlite::Connection, cmd: commands::CrudCommand) -> CrudResult {
    match cmd {
        commands::CrudCommand::ExecuteQuery { sql } => {
            match crud::handle_execute_query(conn, &sql) {
                Ok((columns, rows)) => CrudResult::QueryResult { columns, rows },
                Err(e) => CrudResult::Error(e),
            }
        }
        commands::CrudCommand::ExecuteMultiQuery { sql } => {
            let results = crud::handle_execute_multi_query(conn, &sql);
            CrudResult::MultiQueryResult { results }
        }
        commands::CrudCommand::SaveChanges { table, columns, rows, pk_columns, pending, deleted } => {
            match crud::handle_save_changes(conn, &table, &columns, &rows, &pk_columns, &pending, &deleted) {
                Ok((updates, deletes)) => CrudResult::ChangesSaved { updates, deletes },
                Err(e) => CrudResult::Error(e),
            }
        }
        commands::CrudCommand::InsertRow { table, columns, values } => {
            match crud::handle_insert_row(conn, &table, &columns, &values) {
                Ok(sql) => CrudResult::RowInserted { sql },
                Err(e) => CrudResult::Error(e),
            }
        }
        commands::CrudCommand::PasteRows { table, columns, data_rows, mode } => {
            match crud::handle_paste_rows(conn, &table, &columns, &data_rows, &mode) {
                Ok(inserted) => CrudResult::PasteComplete { inserted },
                Err(e) => CrudResult::Error(e),
            }
        }
        commands::CrudCommand::ImportCsv { table, sql_batch } => {
            match crud::handle_import_csv(conn, &table, &sql_batch) {
                Ok(()) => CrudResult::ImportComplete,
                Err(e) => CrudResult::Error(e),
            }
        }
    }
}

fn handle_load_cmd(conn: &mut rusqlite::Connection, cmd: commands::LoadCommand) -> LoadResult {
    match cmd {
        commands::LoadCommand::LoadTableData { table, page_size, offset } => {
            match data_loader::handle_load_table_data(conn, &table, page_size, offset) {
                Ok((pks, columns, rows, total)) => {
                    LoadResult::TableDataLoaded { table, pks, columns, rows, total }
                }
                Err(e) => LoadResult::Error(e),
            }
        }
        commands::LoadCommand::LoadRowCount { table } => {
            match data_loader::handle_load_row_count(conn, &table) {
                Ok(count) => LoadResult::RowCountLoaded { table, count },
                Err(e) => LoadResult::Error(e),
            }
        }
        commands::LoadCommand::LoadPrimaryKeys { table } => {
            match data_loader::handle_load_primary_keys(conn, &table) {
                Ok(pks) => LoadResult::PrimaryKeysLoaded { table, pks },
                Err(e) => LoadResult::Error(e),
            }
        }
    }
}

fn handle_browser_cmd(conn: &mut rusqlite::Connection, cmd: commands::BrowserCommand) -> BrowserResult {
    // CopyTables is special: we need to return the actual data for clipboard writes
    if let commands::BrowserCommand::CopyTables { ref tables } = cmd {
        match browser::collect_copy_data(conn, tables) {
            Ok(data) => return BrowserResult::CopyData(data),
            Err(e) => return BrowserResult::Error(e),
        }
    }
    match browser::handle_browser(conn, cmd) {
        Ok(event) => BrowserResult::Event(event),
        Err(e) => BrowserResult::Error(e),
    }
}

// ---------------------------------------------------------------------------
// Response → Event translation
// ---------------------------------------------------------------------------

fn translate_response(resp: InternalResp) -> SqliteEvent {
    match resp {
        InternalResp::Schema(sr) => match sr {
            SchemaResult::TablesLoaded { tables, views } => {
                SqliteEvent::Schema(events::SchemaEvent::TablesLoaded { tables, views })
            }
            SchemaResult::CreateTableDdl { table, ddl } => {
                SqliteEvent::Schema(events::SchemaEvent::CreateTableDdl { table, ddl })
            }
            SchemaResult::StructureLoaded { table, columns, indexes, foreign_keys } => {
                SqliteEvent::Schema(events::SchemaEvent::StructureLoaded {
                    table, columns, indexes, foreign_keys,
                })
            }
            SchemaResult::ForeignKeysLoaded { table, foreign_keys } => {
                SqliteEvent::Schema(events::SchemaEvent::ForeignKeysLoaded { table, foreign_keys })
            }
            SchemaResult::Error(e) => SqliteEvent::Error(e),
        },
        InternalResp::Crud(cr) => match cr {
            CrudResult::QueryResult { columns, rows } => {
                SqliteEvent::Crud(events::CrudEvent::QueryResult { columns, rows })
            }
            CrudResult::MultiQueryResult { results } => {
                SqliteEvent::Crud(events::CrudEvent::MultiQueryResult { results })
            }
            CrudResult::ChangesSaved { updates, deletes } => {
                SqliteEvent::Crud(events::CrudEvent::ChangesSaved { updates, deletes })
            }
            CrudResult::RowInserted { sql } => {
                SqliteEvent::Crud(events::CrudEvent::RowInserted { sql })
            }
            CrudResult::PasteComplete { inserted } => {
                SqliteEvent::Crud(events::CrudEvent::PasteComplete { inserted })
            }
            CrudResult::ImportComplete => SqliteEvent::Crud(events::CrudEvent::ImportComplete),
            CrudResult::Error(e) => SqliteEvent::Error(e),
        },
        InternalResp::Load(lr) => match lr {
            LoadResult::TableDataLoaded { table, pks, columns, rows, total } => {
                SqliteEvent::Load(events::LoadEvent::TableDataLoaded {
                    table, pks, columns, rows, total,
                })
            }
            LoadResult::RowCountLoaded { table, count } => {
                SqliteEvent::Load(events::LoadEvent::RowCountLoaded { table, count })
            }
            LoadResult::PrimaryKeysLoaded { table, pks } => {
                SqliteEvent::Load(events::LoadEvent::PrimaryKeysLoaded { table, pks })
            }
            LoadResult::Error(e) => SqliteEvent::Error(e),
        },
        InternalResp::Browser(br) => match br {
            BrowserResult::Event(event) => SqliteEvent::Browser(event),
            BrowserResult::CopyData(data) => SqliteEvent::Browser(events::BrowserEvent::CopyDataReady { data }),
            BrowserResult::Error(e) => SqliteEvent::Error(e),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn test_sqlite_command_is_send() {
        assert_send::<SqliteCommand>();
    }

    #[test]
    fn test_sqlite_event_is_send() {
        assert_send::<SqliteEvent>();
    }

    #[test]
    fn test_sqlite_service_is_send() {
        assert_send::<SqliteService>();
    }

    #[test]
    fn test_sqlite_service_in_mutex_is_sync() {
        assert_send_sync::<std::sync::Mutex<SqliteService>>();
    }

    // Integration tests using in-memory SQLite

    use anyhow::Result;
    use std::time::Duration;
    use voidb_core::TabManager;
    use voidb_core::shell_capabilities::TabInfo;

    struct MockTabManager;

    impl TabManager for MockTabManager {
        fn open(&self, _title: String, _plugin_id: String, _context: serde_json::Value) -> Result<()> { Ok(()) }
        fn close_current(&self) -> Result<()> { Ok(()) }
        fn close_tab(&self, _index: usize) -> Result<()> { Ok(()) }
        fn set_title(&self, _title: String) -> Result<()> { Ok(()) }
        fn request_render(&self) -> Result<()> { Ok(()) }
        fn list_tabs(&self) -> Result<Vec<TabInfo>> { Ok(vec![]) }
        fn switch_to(&self, _index: usize) -> Result<()> { Ok(()) }
        fn active_tab_index(&self) -> Result<usize> { Ok(0) }
        fn quit(&self) -> Result<()> { Ok(()) }
    }

    fn create_test_service() -> SqliteService {
        let tabs: Arc<dyn TabManager> = Arc::new(MockTabManager);
        SqliteService::new(
            SqliteConfig { path: ":memory:".into() },
            tabs,
        )
        .expect("Failed to create in-memory SqliteService")
    }

    #[tokio::test]
    async fn test_service_new_in_memory() {
        let _svc = create_test_service();
    }

    #[tokio::test]
    async fn test_list_tables() {
        let mut svc = create_test_service();

        // Create a table first
        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = svc.poll_event(); // consume the create result

        svc.send(SqliteCommand::Schema(SchemaCommand::ListTables));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = svc.poll_event();
        assert!(matches!(event, Some(SqliteEvent::Schema(SchemaEvent::TablesLoaded { .. }))));
    }

    #[tokio::test]
    async fn test_load_table_data() {
        let mut svc = create_test_service();

        // Create table and insert data
        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, value REAL)".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = svc.poll_event();

        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "INSERT INTO t VALUES (1, 'alice', 1.5)".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = svc.poll_event();

        svc.send(SqliteCommand::Load(LoadCommand::LoadTableData {
            table: "t".into(),
            page_size: 100,
            offset: 0,
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = svc.poll_event();
        match event {
            Some(SqliteEvent::Load(LoadEvent::TableDataLoaded { table, rows, total, .. })) => {
                assert_eq!(table, "t");
                assert_eq!(rows.len(), 1);
                assert_eq!(total, 1);
            }
            other => panic!("Expected TableDataLoaded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_execute_query() {
        let mut svc = create_test_service();

        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "SELECT 1 AS val, 'hello' AS msg".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = svc.poll_event();
        match event {
            Some(SqliteEvent::Crud(CrudEvent::QueryResult { columns, rows })) => {
                assert_eq!(columns.len(), 2);
                assert_eq!(rows.len(), 1);
            }
            other => panic!("Expected QueryResult, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_save_changes() {
        let mut svc = create_test_service();

        // Create table and insert data
        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = svc.poll_event();

        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "INSERT INTO t VALUES (1, 'alice')".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = svc.poll_event();

        // Save a change: update name from 'alice' to 'bob'
        use std::collections::HashMap;
        use voidb_core::{CellValue, ColumnInfo, Row};

        let columns = vec![
            ColumnInfo { name: "id".into(), data_type: "INTEGER".into(), nullable: false, is_primary_key: true, default_value: None, max_length: None, extra: String::new() },
            ColumnInfo { name: "name".into(), data_type: "TEXT".into(), nullable: true, is_primary_key: false, default_value: None, max_length: None, extra: String::new() },
        ];
        let rows = vec![Row { values: vec![CellValue::Int(1), CellValue::Text("alice".into())] }];
        let mut pending = HashMap::new();
        pending.insert((0, 1), CellValue::Text("bob".into()));

        svc.send(SqliteCommand::Crud(CrudCommand::SaveChanges {
            table: "t".into(),
            columns,
            rows,
            pk_columns: vec!["id".into()],
            pending,
            deleted: vec![],
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = svc.poll_event();
        match event {
            Some(SqliteEvent::Crud(CrudEvent::ChangesSaved { updates, deletes })) => {
                assert_eq!(updates, 1);
                assert_eq!(deletes, 0);
            }
            other => panic!("Expected ChangesSaved, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_insert_row() {
        let mut svc = create_test_service();

        svc.send(SqliteCommand::Crud(CrudCommand::ExecuteQuery {
            sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)".into(),
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = svc.poll_event();

        use voidb_core::{CellValue, ColumnInfo};
        let columns = vec![
            ColumnInfo { name: "id".into(), data_type: "INTEGER".into(), nullable: false, is_primary_key: true, default_value: None, max_length: None, extra: String::new() },
            ColumnInfo { name: "name".into(), data_type: "TEXT".into(), nullable: true, is_primary_key: false, default_value: None, max_length: None, extra: String::new() },
        ];

        svc.send(SqliteCommand::Crud(CrudCommand::InsertRow {
            table: "t".into(),
            columns,
            values: vec![CellValue::Int(99), CellValue::Text("new".into())],
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let event = svc.poll_event();
        assert!(matches!(event, Some(SqliteEvent::Crud(CrudEvent::RowInserted { .. }))));
    }
}
