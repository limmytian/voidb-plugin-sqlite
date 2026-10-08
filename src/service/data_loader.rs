//! Paginated data loading handler functions for the SQLite SyncWorker.

use rusqlite::Connection;
use voidb_core::{ColumnInfo, Row};
use voidb_core::CellValue;

use super::value_convert::{decode_sqlite_value, extract_columns};

/// (pks, columns, rows, total_count)
type TableDataResult = (Vec<String>, Vec<ColumnInfo>, Vec<Row>, u64);

/// Load a page of table data with PKs and total count.
pub fn handle_load_table_data(
    conn: &Connection,
    table: &str,
    page_size: usize,
    offset: u64,
) -> Result<TableDataResult, String> {
    let qtable = format!("\"{}\"", table.replace('"', "\"\""));

    // Get PKs
    let pks = handle_load_primary_keys(conn, table)?;

    // Get total count
    let total = handle_load_row_count(conn, table)?;

    // Query page
    let sql = format!(
        "SELECT * FROM {} LIMIT {} OFFSET {}",
        qtable, page_size, offset
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {}", e))?;
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

    Ok((pks, columns, rows, total))
}

/// Load the total row count for a table.
pub fn handle_load_row_count(conn: &Connection, table: &str) -> Result<u64, String> {
    let qtable = format!("\"{}\"", table.replace('"', "\"\""));
    let count: u64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {}", qtable), [], |r| {
            r.get(0)
        })
        .map_err(|e| format!("count_rows: {}", e))?;
    Ok(count)
}

/// Load primary key column names for a table.
pub fn handle_load_primary_keys(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let schema = crate::sqlite_describe_table(conn, table)?;
    Ok(schema
        .columns
        .into_iter()
        .filter(|c| c.is_primary_key)
        .map(|c| c.name)
        .collect())
}
