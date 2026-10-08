//! CRUD operation handler functions for the SQLite SyncWorker.
//!
//! These functions run on the worker thread with direct `&Connection` access.

use std::collections::HashMap;

use rusqlite::Connection;
use voidb_core::{CellValue, ColumnInfo, Row};

use super::commands::PasteMode;
use super::events::StatementResult;
use super::value_convert::{build_pk_where, decode_sqlite_value, extract_columns, quote_sqlite_value};

/// Execute a single SQL query and return columns + rows.
pub fn handle_execute_query(conn: &Connection, sql: &str) -> Result<(Vec<ColumnInfo>, Vec<Row>), String> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("Prepare: {}", e))?;

    let columns = extract_columns(&stmt);
    let col_count = columns.len();

    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            let values: Vec<CellValue> = (0..col_count)
                .map(|i| decode_sqlite_value(row, i))
                .collect();
            Ok(Row { values })
        })
        .map_err(|e| format!("Query: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok((columns, rows))
}

/// Execute multiple SQL statements sequentially.
pub fn handle_execute_multi_query(conn: &Connection, sql: &str) -> Vec<StatementResult> {
    let statements = voidb_core::sql_split::split_statements(sql);
    let mut results = Vec::new();

    for stmt_sql in &statements {
        let trimmed = stmt_sql.trim();
        if trimmed.is_empty() {
            continue;
        }

        let upper = trimmed.to_uppercase();
        let is_select = upper.starts_with("SELECT")
            || upper.starts_with("PRAGMA")
            || upper.starts_with("EXPLAIN")
            || upper.starts_with("VALUES");

        if is_select {
            match handle_execute_query(conn, trimmed) {
                Ok((columns, rows)) => {
                    results.push(StatementResult::Select { columns, rows });
                }
                Err(e) => {
                    results.push(StatementResult::Error(e));
                }
            }
        } else {
            match conn.execute(trimmed, []) {
                Ok(affected) => {
                    results.push(StatementResult::Affected(affected as u64));
                }
                Err(e) => {
                    results.push(StatementResult::Error(e.to_string()));
                }
            }
        }
    }

    if results.is_empty() {
        results.push(StatementResult::Empty);
    }

    results
}

/// Save pending cell edits and row deletions.
pub fn handle_save_changes(
    conn: &Connection,
    table: &str,
    columns: &[ColumnInfo],
    rows: &[Row],
    pk_columns: &[String],
    pending: &HashMap<(usize, usize), CellValue>,
    deleted: &[usize],
) -> Result<(usize, usize), String> {
    let qtable = format!("\"{}\"", table.replace('"', "\"\""));

    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| format!("Begin transaction: {}", e))?;

    // Apply updates
    let mut update_count = 0;
    // Group pending edits by row index
    let mut row_edits: HashMap<usize, Vec<(usize, &CellValue)>> = HashMap::new();
    for (&(row_idx, col_idx), value) in pending {
        row_edits.entry(row_idx).or_default().push((col_idx, value));
    }

    for (row_idx, edits) in &row_edits {
        if deleted.contains(row_idx) {
            continue;
        }
        let row_values = &rows[*row_idx].values;
        let where_clause = build_pk_where(pk_columns, row_values, columns)
            .inspect_err(|_| {
                let _ = conn.execute_batch("ROLLBACK");
            })?;

        let set_parts: Vec<String> = edits
            .iter()
            .map(|(col_idx, value)| {
                let col_name = &columns[*col_idx].name;
                format!(
                    "\"{}\" = {}",
                    col_name.replace('"', "\"\""),
                    quote_sqlite_value(value)
                )
            })
            .collect();

        let sql = format!(
            "UPDATE {} SET {} WHERE {}",
            qtable,
            set_parts.join(", "),
            where_clause
        );
        conn.execute(&sql, []).map_err(|e| {
            let _ = conn.execute_batch("ROLLBACK");
            format!("Update row {}: {}", row_idx, e)
        })?;
        update_count += 1;
    }

    // Apply deletes
    let mut delete_count = 0;
    for &row_idx in deleted {
        let row_values = &rows[row_idx].values;
        let where_clause = build_pk_where(pk_columns, row_values, columns)
            .inspect_err(|_| {
                let _ = conn.execute_batch("ROLLBACK");
            })?;
        let sql = format!("DELETE FROM {} WHERE {}", qtable, where_clause);
        conn.execute(&sql, []).map_err(|e| {
            let _ = conn.execute_batch("ROLLBACK");
            format!("Delete row {}: {}", row_idx, e)
        })?;
        delete_count += 1;
    }

    conn.execute_batch("COMMIT")
        .map_err(|e| format!("Commit: {}", e))?;

    Ok((update_count, delete_count))
}

/// Insert a new row into a table.
pub fn handle_insert_row(
    conn: &Connection,
    table: &str,
    columns: &[ColumnInfo],
    values: &[CellValue],
) -> Result<String, String> {
    let qtable = format!("\"{}\"", table.replace('"', "\"\""));
    let col_names: Vec<String> = columns
        .iter()
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect();
    let val_strs: Vec<String> = values.iter().map(quote_sqlite_value).collect();

    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        qtable,
        col_names.join(", "),
        val_strs.join(", ")
    );

    conn.execute(&sql, [])
        .map_err(|e| format!("Insert: {}", e))?;

    Ok(sql)
}

/// Paste rows into a table.
pub fn handle_paste_rows(
    conn: &Connection,
    table: &str,
    columns: &[ColumnInfo],
    data_rows: &[Row],
    mode: &PasteMode,
) -> Result<usize, String> {
    let qtable = format!("\"{}\"", table.replace('"', "\"\""));

    conn.execute_batch("BEGIN TRANSACTION")
        .map_err(|e| format!("Begin transaction: {}", e))?;

    if *mode == PasteMode::Overwrite {
        conn.execute(&format!("DELETE FROM {}", qtable), [])
            .map_err(|e| {
                let _ = conn.execute_batch("ROLLBACK");
                format!("Delete existing rows: {}", e)
            })?;
    }

    let col_names: Vec<String> = columns
        .iter()
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect();

    let mut inserted = 0;
    for row in data_rows {
        let val_strs: Vec<String> = row.values.iter().map(quote_sqlite_value).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            qtable,
            col_names.join(", "),
            val_strs.join(", ")
        );
        conn.execute(&sql, []).map_err(|e| {
            let _ = conn.execute_batch("ROLLBACK");
            format!("Insert row: {}", e)
        })?;
        inserted += 1;
    }

    conn.execute_batch("COMMIT")
        .map_err(|e| format!("Commit: {}", e))?;

    Ok(inserted)
}

/// Import CSV data via pre-built SQL batch.
pub fn handle_import_csv(conn: &Connection, _table: &str, sql_batch: &str) -> Result<(), String> {
    conn.execute_batch(sql_batch)
        .map_err(|e| format!("Import CSV: {}", e))
}
