mod agent_session;
mod cli_plugin;
mod capabilities;
mod config;
mod connection;
pub mod service;

use async_trait::async_trait;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

use voidb_core::connection::{ConnectionConfig, DatabaseType};
use voidb_core::database::types::*;
use voidb_core::database::DatabaseAdapter;
use voidb_core::error::VoidbError;

pub use cli_plugin::create_sqlite_cli_plugin;
pub use agent_session::SqliteAgentSessionFactory;
pub use capabilities::{invoke_sqlite_capability, sqlite_capabilities};
pub use config::SqliteConfig;
pub use connection::SqliteConnectionProvider;
pub use service::{SqliteCommand, SqliteEvent, SqliteService};

/// Query table schema from a SQLite database using PRAGMA statements.
/// Usable from any thread that has a `rusqlite::Connection`.
pub fn sqlite_describe_table(conn: &Connection, table: &str) -> Result<TableSchema, String> {
    let qtable = table.replace('"', "\"\"");

    // Columns via PRAGMA table_info
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{}\")", qtable))
        .map_err(|e| format!("PRAGMA table_info failed: {}", e))?;
    let columns: Vec<ColumnInfo> = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let data_type: String = row.get(2)?;
            let notnull: bool = row.get::<_, i32>(3)? != 0;
            let default_value: Option<String> = row.get(4)?;
            let pk: i32 = row.get(5)?;
            Ok(ColumnInfo {
                name,
                data_type,
                nullable: !notnull,
                is_primary_key: pk > 0,
                default_value,
                max_length: None,
                extra: if pk > 0 { "PRIMARY KEY".to_string() } else { String::new() },
            })
        })
        .map_err(|e| format!("PRAGMA table_info query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    // Indexes via PRAGMA index_list + index_info
    let mut indexes = Vec::new();
    let mut idx_stmt = conn
        .prepare(&format!("PRAGMA index_list(\"{}\")", qtable))
        .map_err(|e| format!("PRAGMA index_list failed: {}", e))?;
    let idx_list: Vec<(String, bool)> = idx_stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let unique: bool = row.get::<_, i32>(2)? != 0;
            Ok((name, unique))
        })
        .map_err(|e| format!("index_list query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    for (idx_name, unique) in idx_list {
        let sql = format!("PRAGMA index_info(\"{}\")", idx_name.replace('"', "\"\""));
        let mut info_stmt = conn.prepare(&sql).map_err(|e| format!("index_info failed: {}", e))?;
        let cols: Vec<String> = info_stmt
            .query_map([], |row| row.get::<_, String>(2))
            .map_err(|e| format!("index_info query failed: {}", e))?
            .filter_map(|r| r.ok())
            .collect();
        indexes.push(IndexInfo {
            name: idx_name,
            columns: cols,
            unique,
            index_type: "BTREE".to_string(),
        });
    }

    // Foreign keys via PRAGMA foreign_key_list
    let mut fk_stmt = conn
        .prepare(&format!("PRAGMA foreign_key_list(\"{}\")", qtable))
        .map_err(|e| format!("PRAGMA foreign_key_list failed: {}", e))?;
    let fk_rows: Vec<(i32, String, String, String, String, String)> = fk_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i32>(0)?,       // id
                row.get::<_, String>(2)?,     // table
                row.get::<_, String>(3)?,     // from
                row.get::<_, String>(4)?,     // to
                row.get::<_, String>(5)?,     // on_update
                row.get::<_, String>(6)?,     // on_delete
            ))
        })
        .map_err(|e| format!("foreign_key_list query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    let mut fk_map: std::collections::HashMap<i32, ForeignKeyInfo> = std::collections::HashMap::new();
    for (id, ref_table, from_col, to_col, on_update, on_delete) in fk_rows {
        let entry = fk_map.entry(id).or_insert_with(|| ForeignKeyInfo {
            name: format!("fk_{}_{}", table, id),
            columns: Vec::new(),
            referenced_table: ref_table,
            referenced_columns: Vec::new(),
            on_update: on_update.clone(),
            on_delete: on_delete.clone(),
        });
        entry.columns.push(from_col);
        entry.referenced_columns.push(to_col);
    }
    let foreign_keys: Vec<ForeignKeyInfo> = fk_map.into_values().collect();

    Ok(TableSchema {
        database: String::new(),
        table_name: table.to_string(),
        columns,
        indexes,
        foreign_keys,
        engine: None,
        comment: None,
    })
}

/// Generate CREATE TABLE SQL for SQLite from a TableSchema.
pub fn sqlite_generate_create_table(schema: &TableSchema) -> String {
    let mut parts = Vec::new();
    let mut pk_cols: Vec<&str> = Vec::new();

    for col in &schema.columns {
        let mut def = format!("\"{}\" {}", col.name.replace('"', "\"\""), col.data_type);
        if !col.nullable {
            def.push_str(" NOT NULL");
        }
        if let Some(ref dv) = col.default_value {
            def.push_str(&format!(" DEFAULT {}", dv));
        }
        if col.is_primary_key {
            pk_cols.push(&col.name);
        }
        parts.push(def);
    }

    if !pk_cols.is_empty() {
        let cols_str = pk_cols
            .iter()
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("PRIMARY KEY ({})", cols_str));
    }

    for fk in &schema.foreign_keys {
        let from = fk.columns.iter().map(|c| format!("\"{}\"", c.replace('"', "\"\""))).collect::<Vec<_>>().join(", ");
        let to = fk.referenced_columns.iter().map(|c| format!("\"{}\"", c.replace('"', "\"\""))).collect::<Vec<_>>().join(", ");
        let mut fk_sql = format!(
            "FOREIGN KEY ({}) REFERENCES \"{}\" ({})",
            from,
            fk.referenced_table.replace('"', "\"\""),
            to
        );
        if !fk.on_update.is_empty() && fk.on_update != "NO ACTION" {
            fk_sql.push_str(&format!(" ON UPDATE {}", fk.on_update));
        }
        if !fk.on_delete.is_empty() && fk.on_delete != "NO ACTION" {
            fk_sql.push_str(&format!(" ON DELETE {}", fk.on_delete));
        }
        parts.push(fk_sql);
    }

    format!(
        "CREATE TABLE \"{}\" (\n  {}\n)",
        schema.table_name.replace('"', "\"\""),
        parts.join(",\n  ")
    )
}

/// Decode a rusqlite value to CellValue.
pub fn decode_sqlite_value(row: &rusqlite::Row, idx: usize) -> CellValue {
    if let Ok(v) = row.get::<_, Option<i64>>(idx) {
        match v {
            Some(i) => CellValue::Int(i),
            None => CellValue::Null,
        }
    } else if let Ok(v) = row.get::<_, Option<f64>>(idx) {
        match v {
            Some(f) => CellValue::Float(f),
            None => CellValue::Null,
        }
    } else if let Ok(v) = row.get::<_, Option<String>>(idx) {
        match v {
            Some(s) => CellValue::Text(s),
            None => CellValue::Null,
        }
    } else if let Ok(v) = row.get::<_, Option<Vec<u8>>>(idx) {
        match v {
            Some(b) => CellValue::Blob(b),
            None => CellValue::Null,
        }
    } else {
        CellValue::Null
    }
}

pub struct SqliteAdapter {
    connection_id: String,
    conn: Arc<Mutex<Connection>>,
}

#[async_trait]
impl DatabaseAdapter for SqliteAdapter {
    async fn connect(config: &ConnectionConfig) -> Result<Box<dyn DatabaseAdapter>, VoidbError> {
        use crate::config::SqliteConfig;

        let sqlite_config: SqliteConfig = config.plugin_config
            .as_ref()
            .ok_or_else(|| VoidbError::Connection("Missing plugin_config".to_string()))
            .and_then(|pc| {
                serde_json::from_value(pc.clone())
                    .map_err(|e| VoidbError::Connection(format!("Invalid SQLite config: {}", e)))
            })?;

        let path = sqlite_config.path.clone();
        let conn = Connection::open(&path)
            .map_err(|e| VoidbError::Connection(format!("Failed to open SQLite database '{}': {}", path, e)))?;

        // Enable foreign keys
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| VoidbError::Connection(format!("Failed to enable foreign keys: {}", e)))?;

        Ok(Box::new(SqliteAdapter {
            connection_id: config.name.clone(),
            conn: Arc::new(Mutex::new(conn)),
        }))
    }

    async fn ping(&self) -> Result<(), VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        conn.execute_batch("SELECT 1")
            .map_err(|e| VoidbError::Connection(format!("Ping failed: {}", e)))?;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), VoidbError> {
        Ok(())
    }

    fn db_type(&self) -> DatabaseType { DatabaseType::SQLite }
    fn connection_id(&self) -> String { self.connection_id.clone() }

    async fn list_databases(&self) -> Result<Vec<String>, VoidbError> {
        // SQLite has a single database per file; return "main"
        Ok(vec!["main".to_string()])
    }

    async fn list_tables(&self, _database: &str) -> Result<Vec<TableInfo>, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ).map_err(|e| VoidbError::Query(format!("list_tables: {}", e)))?;

        let tables: Vec<TableInfo> = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            Ok(TableInfo {
                name,
                table_type: "TABLE".to_string(),
                row_count: None,
                comment: None,
            })
        })
        .map_err(|e| VoidbError::Query(format!("list_tables query: {}", e)))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(tables)
    }

    async fn list_views(&self, _database: &str) -> Result<Vec<ViewInfo>, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='view' ORDER BY name"
        ).map_err(|e| VoidbError::Query(format!("list_views: {}", e)))?;

        let views: Vec<ViewInfo> = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            Ok(ViewInfo {
                name,
                definition: None,
            })
        })
        .map_err(|e| VoidbError::Query(format!("list_views query: {}", e)))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(views)
    }

    async fn describe_table(&self, _database: &str, table: &str) -> Result<TableSchema, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        sqlite_describe_table(&conn, table).map_err(VoidbError::Query)
    }

    async fn list_indexes(&self, _database: &str, table: &str) -> Result<Vec<IndexInfo>, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let schema = sqlite_describe_table(&conn, table).map_err(VoidbError::Query)?;
        Ok(schema.indexes)
    }

    async fn list_foreign_keys(&self, _database: &str, table: &str) -> Result<Vec<ForeignKeyInfo>, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let schema = sqlite_describe_table(&conn, table).map_err(VoidbError::Query)?;
        Ok(schema.foreign_keys)
    }

    async fn query_rows(&self, sql: &str, _params: &[QueryParam], offset: u64, limit: u64) -> Result<QueryResult, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let paginated = format!("{} LIMIT {} OFFSET {}", sql.trim_end_matches(';'), limit, offset);
        let mut stmt = conn.prepare(&paginated)
            .map_err(|e| VoidbError::Query(format!("Prepare: {}", e)))?;

        let col_count = stmt.column_count();
        let columns: Vec<ColumnInfo> = (0..col_count)
            .map(|i| ColumnInfo {
                name: stmt.column_name(i).unwrap_or("?").to_string(),
                data_type: String::new(),
                nullable: true,
                is_primary_key: false,
                default_value: None,
                max_length: None,
                extra: String::new(),
            })
            .collect();

        let rows: Vec<Row> = stmt.query_map([], |row| {
            let values: Vec<CellValue> = (0..col_count)
                .map(|i| decode_sqlite_value(row, i))
                .collect();
            Ok(Row { values })
        })
        .map_err(|e| VoidbError::Query(format!("Query: {}", e)))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(QueryResult {
            columns,
            rows,
            rows_affected: None,
            execution_time: std::time::Duration::ZERO,
        })
    }

    async fn execute(&self, sql: &str, _params: &[QueryParam]) -> Result<ExecuteResult, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let affected = conn.execute(sql, [])
            .map_err(|e| VoidbError::Query(format!("Execute: {}", e)))? as u64;
        Ok(ExecuteResult {
            rows_affected: affected,
            last_insert_id: Some(conn.last_insert_rowid()),
        })
    }

    async fn execute_query(&self, sql: &str, _database: Option<&str>) -> Result<QueryResult, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let trimmed = sql.trim();
        let upper = trimmed.to_uppercase();

        let is_select = upper.starts_with("SELECT")
            || upper.starts_with("PRAGMA")
            || upper.starts_with("EXPLAIN")
            || upper.starts_with("VALUES");

        if is_select {
            let mut stmt = conn.prepare(trimmed)
                .map_err(|e| VoidbError::Query(format!("Prepare: {}", e)))?;

            let col_count = stmt.column_count();
            let columns: Vec<ColumnInfo> = (0..col_count)
                .map(|i| ColumnInfo {
                    name: stmt.column_name(i).unwrap_or("?").to_string(),
                    data_type: String::new(),
                    nullable: true,
                    is_primary_key: false,
                    default_value: None,
                    max_length: None,
                    extra: String::new(),
                })
                .collect();

            let rows: Vec<Row> = stmt.query_map([], |row| {
                let values: Vec<CellValue> = (0..col_count)
                    .map(|i| decode_sqlite_value(row, i))
                    .collect();
                Ok(Row { values })
            })
            .map_err(|e| VoidbError::Query(format!("Query: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();

            Ok(QueryResult {
                columns,
                rows,
                rows_affected: None,
                execution_time: std::time::Duration::ZERO,
            })
        } else {
            let affected = conn.execute(trimmed, [])
                .map_err(|e| VoidbError::Query(format!("Execute: {}", e)))? as u64;
            Ok(QueryResult {
                columns: vec![ColumnInfo {
                    name: "rows_affected".to_string(),
                    data_type: "INTEGER".to_string(),
                    nullable: false,
                    is_primary_key: false,
                    default_value: None,
                    max_length: None,
                    extra: String::new(),
                }],
                rows: vec![Row {
                    values: vec![CellValue::Int(affected as i64)],
                }],
                rows_affected: Some(affected),
                execution_time: std::time::Duration::ZERO,
            })
        }
    }

    async fn count_rows(&self, _database: &str, table: &str, filter: Option<&str>) -> Result<u64, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let sql = if let Some(f) = filter {
            format!("SELECT COUNT(*) FROM {} WHERE {}", quoted, f)
        } else {
            format!("SELECT COUNT(*) FROM {}", quoted)
        };
        let count: u64 = conn.query_row(&sql, [], |r| r.get(0))
            .map_err(|e| VoidbError::Query(format!("count_rows: {}", e)))?;
        Ok(count)
    }

    async fn get_create_table_sql(&self, _database: &str, table: &str) -> Result<String, VoidbError> {
        let conn = self.conn.lock().map_err(|e| VoidbError::Other(format!("Lock error: {}", e)))?;
        let sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        ).map_err(|e| VoidbError::Query(format!("get_create_table_sql: {}", e)))?;
        Ok(format!("{};", sql))
    }

    fn generate_create_table_sql(&self, schema: &TableSchema) -> String {
        sqlite_generate_create_table(schema)
    }

    fn quote_identifier(&self, name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}
