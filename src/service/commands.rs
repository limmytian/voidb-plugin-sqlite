//! SQLite service command types.
//!
//! Commands are sent from the TUI plugin to the SyncWorker via
//! `SqliteService::send()`. Each domain (schema, CRUD, data loading,
//! browser) has its own sub-enum for organization.
//!
//! Unlike MySQL, SQLite has no `database` parameter — a single database
//! per file.

use std::collections::HashMap;

use voidb_core::{CellValue, ColumnInfo, Row};

/// Top-level command enum for the SQLite service.
#[derive(Debug)]
pub enum SqliteCommand {
    /// Schema introspection operations.
    Schema(SchemaCommand),

    /// CRUD operations (query, execute, save, insert).
    Crud(CrudCommand),

    /// Data loading operations (pagination, row count, PKs).
    Load(LoadCommand),

    /// Browser operations (DDL, table management).
    Browser(BrowserCommand),
}

/// Schema introspection commands.
#[derive(Debug)]
pub enum SchemaCommand {
    /// List all tables and views.
    ListTables,

    /// Get the CREATE TABLE DDL for a table.
    ShowCreateTable { table: String },

    /// Load full structure info (columns + indexes + foreign keys).
    LoadStructure { table: String },

    /// Load foreign keys for a specific table.
    ListForeignKeys { table: String },
}

/// CRUD commands (query execution and data mutation).
#[derive(Debug)]
pub enum CrudCommand {
    /// Execute a single SQL query and return results.
    ExecuteQuery { sql: String },

    /// Execute multiple SQL statements sequentially.
    ExecuteMultiQuery { sql: String },

    /// Save pending cell edits and row deletions.
    SaveChanges {
        table: String,
        columns: Vec<ColumnInfo>,
        rows: Vec<Row>,
        pk_columns: Vec<String>,
        pending: HashMap<(usize, usize), CellValue>,
        deleted: Vec<usize>,
    },

    /// Insert a new row into a table.
    InsertRow {
        table: String,
        columns: Vec<ColumnInfo>,
        values: Vec<CellValue>,
    },

    /// Paste rows into a table.
    PasteRows {
        table: String,
        columns: Vec<ColumnInfo>,
        data_rows: Vec<Row>,
        mode: PasteMode,
    },

    /// Import CSV data via pre-built SQL batch.
    ImportCsv { table: String, sql_batch: String },
}

/// Data loading commands (pagination, counts, PKs).
#[derive(Debug)]
pub enum LoadCommand {
    /// Load a page of table data.
    LoadTableData {
        table: String,
        page_size: usize,
        offset: u64,
    },

    /// Load the total row count for a table.
    LoadRowCount { table: String },

    /// Load primary key column names for a table.
    LoadPrimaryKeys { table: String },
}

/// Browser / DDL management commands.
#[derive(Debug)]
pub enum BrowserCommand {
    /// Load all tables for browser display.
    LoadTables,

    /// Load preview data for a table.
    LoadPreview { table: String },

    /// Copy table structures and data to clipboard.
    CopyTables { tables: Vec<String> },

    /// Paste previously copied tables.
    PasteTables { table_data: Vec<TableCopyData> },

    /// Execute raw SQL in browser context.
    ExecuteSql { sql: String },

    /// Load columns for the schema editor.
    LoadColumnsForEditor { table: String },

    /// Drop a table.
    DropTable { table: String },

    /// Rename a table.
    RenameTable { old_name: String, new_name: String },

    /// Duplicate a table (structure + data).
    DuplicateTable { source: String, target: String },
}

/// Paste mode for row paste operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteMode {
    /// Insert rows without clearing existing data.
    InsertOnly,
    /// Delete existing rows before inserting.
    Overwrite,
}

/// Data needed for a table copy/paste operation.
#[derive(Debug, Clone)]
pub struct TableCopyData {
    /// Name of the table.
    pub table_name: String,
    /// CREATE TABLE SQL statement.
    pub create_sql: String,
    /// Row data.
    pub rows: Vec<Row>,
    /// Column names in order.
    pub column_names: Vec<String>,
}
