//! Schema introspection handler functions for the SQLite SyncWorker.
//!
//! These functions run on the worker thread with direct `&Connection` access.
//! They are called from the `handler_fn` in `mod.rs`.

use rusqlite::Connection;
use voidb_core::ColumnInfo;
use voidb_core::database::types::ForeignKeyInfo;

use super::events::IndexInfo;

/// (columns, indexes, foreign_keys)
type StructureResult = (Vec<ColumnInfo>, Vec<IndexInfo>, Vec<ForeignKeyInfo>);

/// List all tables and views in the database.
pub fn handle_list_tables(conn: &Connection) -> Result<(Vec<super::events::TableMeta>, Vec<String>), String> {
    let mut stmt = conn
        .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|e| format!("list_tables: {}", e))?;

    let entries: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("list_tables query: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    let mut tables = Vec::new();
    let mut views = Vec::new();

    for (name, obj_type) in entries {
        if obj_type == "view" {
            views.push(name);
        } else {
            let count: Option<u64> = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\"")),
                    [],
                    |r| r.get(0),
                )
                .ok();
            tables.push(super::events::TableMeta {
                name,
                table_type: "table".to_string(),
                rows: count,
            });
        }
    }

    Ok((tables, views))
}

/// Get the CREATE TABLE DDL for a table.
pub fn handle_show_create_table(conn: &Connection, table: &str) -> Result<String, String> {
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |r| r.get(0),
        )
        .map_err(|e| format!("show_create_table: {}", e))?;
    Ok(format!("{};", sql))
}

/// Load full structure info (columns + indexes + foreign keys).
pub fn handle_load_structure(
    conn: &Connection,
    table: &str,
) -> Result<StructureResult, String> {
    let schema = crate::sqlite_describe_table(conn, table)?;

    let indexes = schema
        .indexes
        .into_iter()
        .map(|idx| IndexInfo {
            name: idx.name,
            columns: idx.columns,
            unique: idx.unique,
            index_type: idx.index_type,
        })
        .collect();

    Ok((schema.columns, indexes, schema.foreign_keys))
}

/// Load foreign keys for a specific table.
pub fn handle_list_foreign_keys(conn: &Connection, table: &str) -> Result<Vec<ForeignKeyInfo>, String> {
    let schema = crate::sqlite_describe_table(conn, table)?;
    Ok(schema.foreign_keys)
}

/// Convert TableCopyData to a minimal TableSchema for clipboard operations.
///
/// Used by db_browser when writing copied table data to VoidbClipboard,
/// which requires a `TableSchema` struct.
pub fn table_copy_data_to_schema(td: &super::commands::TableCopyData) -> voidb_core::database::types::TableSchema {
    let columns: Vec<ColumnInfo> = td.column_names.iter().map(|name| {
        ColumnInfo {
            name: name.clone(),
            data_type: String::new(),
            nullable: true,
            is_primary_key: false,
            default_value: None,
            max_length: None,
            extra: String::new(),
        }
    }).collect();

    voidb_core::database::types::TableSchema {
        database: String::new(),
        table_name: td.table_name.clone(),
        columns,
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        engine: None,
        comment: None,
    }
}
