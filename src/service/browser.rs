//! Browser service handler functions for the SQLite SyncWorker.
//!
//! All functions execute on the worker thread with `&Connection` access.
//! They handle table management operations for the db_browser plugin.

use rusqlite::Connection;

use voidb_core::{ColumnInfo, Row};

use super::commands::{BrowserCommand, TableCopyData};
use super::events::{BrowserEvent, TableMeta};
use super::value_convert::{decode_sqlite_value, quote_sqlite_value};

/// Handle a browser command. Returns an event or error string.
pub fn handle_browser(
    conn: &Connection,
    cmd: BrowserCommand,
) -> Result<BrowserEvent, String> {
    match cmd {
        BrowserCommand::LoadTables => handle_load_tables(conn),
        BrowserCommand::LoadPreview { table } => handle_load_preview(conn, &table),
        BrowserCommand::CopyTables { tables } => handle_copy_tables(conn, &tables),
        BrowserCommand::PasteTables { table_data } => handle_paste_tables(conn, &table_data),
        BrowserCommand::ExecuteSql { sql } => handle_execute_sql(conn, &sql),
        BrowserCommand::LoadColumnsForEditor { table } => handle_load_columns_for_editor(conn, &table),
        BrowserCommand::DropTable { table } => handle_drop_table(conn, &table),
        BrowserCommand::RenameTable { old_name, new_name } => handle_rename_table(conn, &old_name, &new_name),
        BrowserCommand::DuplicateTable { source, target } => handle_duplicate_table(conn, &source, &target),
    }
}

/// Load all tables and views for the browser tree.
pub fn handle_load_tables(conn: &Connection) -> Result<BrowserEvent, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(|e| format!("Query failed: {}", e))?;

    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    let mut tables = Vec::new();
    for name in names {
        let count: Option<u64> = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\"")),
                [],
                |r| r.get(0),
            )
            .ok();
        tables.push(TableMeta {
            name,
            table_type: "table".into(),
            rows: count,
        });
    }

    let mut view_stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='view' ORDER BY name")
        .map_err(|e| format!("View query failed: {}", e))?;
    let views: Vec<String> = view_stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("View query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(BrowserEvent::TablesLoaded { tables, views })
}

/// Load preview data for a table (first 50 rows + columns + DDL + row count).
pub fn handle_load_preview(conn: &Connection, table: &str) -> Result<BrowserEvent, String> {
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));

    // Get columns via describe
    let schema = crate::sqlite_describe_table(conn, table)?;
    let columns: Vec<ColumnInfo> = schema.columns;

    // Get row count
    let row_count: u64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {}", quoted), [], |r| r.get(0))
        .map_err(|e| format!("Count failed: {}", e))?;

    // Get first 50 rows
    let sql = format!("SELECT * FROM {} LIMIT 50", quoted);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare failed: {}", e))?;
    let col_count = stmt.column_count();
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            let values: Vec<voidb_core::CellValue> = (0..col_count)
                .map(|i| decode_sqlite_value(row, i))
                .collect();
            Ok(Row { values })
        })
        .map_err(|e| format!("Query failed: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    // Get DDL
    let ddl = crate::sqlite_generate_create_table(&crate::sqlite_describe_table(conn, table)?);

    Ok(BrowserEvent::PreviewLoaded {
        table: table.to_string(),
        columns,
        rows,
        row_count,
        ddl,
    })
}

/// Copy table structures and data for clipboard operations.
///
/// Returns the copied data as `BrowserEvent::CopyDone` (the actual clipboard
/// write happens in the facade layer, not here on the worker thread).
pub fn handle_copy_tables(
    _conn: &Connection,
    _tables: &[String],
) -> Result<BrowserEvent, String> {
    // The actual data collection is done here; the facade will write to clipboard.
    // We return CopyDone — the facade handles clipboard writes separately.
    // But we need to return the data. The current BrowserEvent::CopyDone has no data.
    // So we use a separate internal response path via BrowserResult::CopyData.
    // This is handled in mod.rs handle_browser_cmd differently.

    // Actually, looking at the architecture: the browser handler just returns events.
    // For copy, we need to return the data to the facade. We'll handle this by
    // returning CopyDone and having a separate internal path in mod.rs that
    // collects the data and writes to clipboard.
    //
    // For now, we implement the data collection logic here and return it
    // via a special path. See mod.rs for the CopyTablesResult handling.
    Ok(BrowserEvent::CopyDone)
}

/// Internal: collect table data for a copy operation.
/// Called from mod.rs to get the actual data for clipboard.
pub fn collect_copy_data(
    conn: &Connection,
    tables: &[String],
) -> Result<Vec<TableCopyData>, String> {
    let mut result = Vec::new();
    for table in tables {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));

        // Get CREATE TABLE DDL
        let schema = crate::sqlite_describe_table(conn, table)?;
        let create_sql = crate::sqlite_generate_create_table(&schema);

        // Get column names
        let column_names: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();

        // Get all rows
        let sql = format!("SELECT * FROM {}", quoted);
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("SELECT from '{}' failed: {}", table, e))?;
        let col_count = stmt.column_count();
        let rows: Vec<Row> = stmt
            .query_map([], |row| {
                let values: Vec<voidb_core::CellValue> = (0..col_count)
                    .map(|i| decode_sqlite_value(row, i))
                    .collect();
                Ok(Row { values })
            })
            .map_err(|e| format!("Query '{}' failed: {}", table, e))?
            .filter_map(|r| r.ok())
            .collect();

        result.push(TableCopyData {
            table_name: table.clone(),
            create_sql,
            rows,
            column_names,
        });
    }
    Ok(result)
}

/// Paste previously copied tables into the database.
pub fn handle_paste_tables(
    conn: &Connection,
    table_data: &[TableCopyData],
) -> Result<BrowserEvent, String> {
    for td in table_data {
        // Execute the CREATE TABLE DDL
        conn.execute_batch(&td.create_sql)
            .map_err(|e| format!("CREATE TABLE '{}' failed: {}", td.table_name, e))?;

        // Insert rows if any
        if !td.rows.is_empty() && !td.column_names.is_empty() {
            let col_names: Vec<String> = td
                .column_names
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect();
            let cols_str = col_names.join(", ");

            for chunk in td.rows.chunks(500) {
                let mut values_parts = Vec::new();
                for row in chunk {
                    let vals: Vec<String> = row.values.iter().map(quote_sqlite_value).collect();
                    values_parts.push(format!("({})", vals.join(", ")));
                }
                let insert_sql = format!(
                    "INSERT INTO \"{}\" ({}) VALUES {}",
                    td.table_name.replace('"', "\"\""),
                    cols_str,
                    values_parts.join(", ")
                );
                conn.execute_batch(&insert_sql)
                    .map_err(|e| format!("INSERT into '{}' failed: {}", td.table_name, e))?;
            }
        }
    }
    Ok(BrowserEvent::PasteDone)
}

/// Execute raw SQL in the browser context.
pub fn handle_execute_sql(conn: &Connection, sql: &str) -> Result<BrowserEvent, String> {
    let stmts: Vec<&str> = sql.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    for stmt in &stmts {
        conn.execute_batch(stmt)
            .map_err(|e| format!("Failed: {}", e))?;
    }
    Ok(BrowserEvent::SqlExecuted {
        result: format!("OK ({} statements)", stmts.len()),
    })
}

/// Load columns for the schema editor.
pub fn handle_load_columns_for_editor(
    conn: &Connection,
    table: &str,
) -> Result<BrowserEvent, String> {
    let schema = crate::sqlite_describe_table(conn, table)?;
    Ok(BrowserEvent::ColumnsForEditorLoaded {
        table: table.to_string(),
        columns: schema.columns,
    })
}

/// Drop a table.
pub fn handle_drop_table(conn: &Connection, table: &str) -> Result<BrowserEvent, String> {
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS \"{}\"",
        table.replace('"', "\"\"")
    ))
    .map_err(|e| format!("Drop failed: {}", e))?;
    Ok(BrowserEvent::TableDropped {
        table: table.to_string(),
    })
}

/// Rename a table.
pub fn handle_rename_table(
    conn: &Connection,
    old_name: &str,
    new_name: &str,
) -> Result<BrowserEvent, String> {
    conn.execute_batch(&format!(
        "ALTER TABLE \"{}\" RENAME TO \"{}\"",
        old_name.replace('"', "\"\""),
        new_name.replace('"', "\"\"")
    ))
    .map_err(|e| format!("Rename failed: {}", e))?;
    Ok(BrowserEvent::TableRenamed {
        old_name: old_name.to_string(),
        new_name: new_name.to_string(),
    })
}

/// Duplicate a table (structure + data).
pub fn handle_duplicate_table(
    conn: &Connection,
    source: &str,
    target: &str,
) -> Result<BrowserEvent, String> {
    conn.execute_batch(&format!(
        "CREATE TABLE \"{}\" AS SELECT * FROM \"{}\"",
        target.replace('"', "\"\""),
        source.replace('"', "\"\"")
    ))
    .map_err(|e| format!("Duplicate failed: {}", e))?;
    Ok(BrowserEvent::TableDuplicated {
        source: source.to_string(),
        target: target.to_string(),
    })
}
