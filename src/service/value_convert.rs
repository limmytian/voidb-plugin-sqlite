//! Centralized SQLite value conversion utilities.
//!
//! Consolidates `decode_sqlite_value` (from lib.rs) and SQL generation
//! helpers into one module. These functions run on the SyncWorker thread
//! inside handler_fn.

use voidb_core::{CellValue, ColumnInfo};

/// Decode a rusqlite value to CellValue by probing value_ref() for type.
///
/// Uses `value_ref()` to inspect the actual SQLite storage type,
/// providing accurate type detection regardless of column affinity.
pub fn decode_sqlite_value(row: &rusqlite::Row, idx: usize) -> CellValue {
    use rusqlite::types::ValueRef;

    match row.get_ref(idx) {
        Ok(val) => match val {
            ValueRef::Null => CellValue::Null,
            ValueRef::Integer(i) => CellValue::Int(i),
            ValueRef::Real(f) => CellValue::Float(f),
            ValueRef::Text(bytes) => {
                CellValue::Text(String::from_utf8_lossy(bytes).into_owned())
            }
            ValueRef::Blob(bytes) => CellValue::Blob(bytes.to_vec()),
        },
        Err(_) => CellValue::Null,
    }
}

/// SQL-quote a CellValue for embedding in generated SQL statements.
///
/// Produces a SQL literal: NULL, numeric, or single-quoted string
/// with escaped inner quotes.
pub fn quote_sqlite_value(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".into(),
        CellValue::Int(i) => i.to_string(),
        CellValue::Float(f) => f.to_string(),
        CellValue::Bool(b) => if *b { "1" } else { "0" }.into(),
        CellValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
        CellValue::Date(s) | CellValue::Time(s) | CellValue::DateTime(s) => {
            format!("'{}'", s.replace('\'', "''"))
        }
        CellValue::Blob(b) => format!("X'{}'", hex::encode(b)),
        CellValue::Json(s) => format!("'{}'", s.replace('\'', "''")),
        CellValue::Uuid(s) | CellValue::Unknown(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

/// Build a WHERE clause from primary key columns and a data row.
///
/// Produces expressions like `"id" = 42 AND "name" = 'foo'`.
/// NULL PKs are handled with `IS NULL`.
///
/// # Errors
///
/// Returns an error if `pk_columns` is empty or a PK column is not
/// found in the column list.
pub fn build_pk_where(
    pk_columns: &[String],
    row: &[CellValue],
    columns: &[ColumnInfo],
) -> Result<String, String> {
    if pk_columns.is_empty() {
        return Err("No primary key -- cannot identify row".to_string());
    }

    let mut conditions = Vec::new();
    for pk in pk_columns {
        let idx = columns
            .iter()
            .position(|c| c.name == *pk)
            .ok_or_else(|| format!("PK column '{}' not found", pk))?;

        let val = &row[idx];
        if matches!(val, CellValue::Null) {
            conditions.push(format!("\"{}\" IS NULL", pk));
        } else {
            conditions.push(format!("\"{}\" = {}", pk, quote_sqlite_value(val)));
        }
    }

    Ok(conditions.join(" AND "))
}

/// Extract column metadata from a prepared rusqlite Statement.
pub fn extract_columns(stmt: &rusqlite::Statement) -> Vec<ColumnInfo> {
    (0..stmt.column_count())
        .map(|i| ColumnInfo {
            name: stmt.column_name(i).unwrap_or("?").to_string(),
            data_type: String::new(),
            nullable: true,
            is_primary_key: false,
            default_value: None,
            max_length: None,
            extra: String::new(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_sqlite_value_null() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER)").unwrap();
        conn.execute("INSERT INTO t VALUES (NULL)", []).unwrap();
        let mut stmt = conn.prepare("SELECT a FROM t").unwrap();
        let val = stmt
            .query_row([], |row| Ok(decode_sqlite_value(row, 0)))
            .unwrap();
        assert_eq!(val, CellValue::Null);
    }

    #[test]
    fn test_decode_sqlite_value_integer() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER)").unwrap();
        conn.execute("INSERT INTO t VALUES (42)", []).unwrap();
        let mut stmt = conn.prepare("SELECT a FROM t").unwrap();
        let val = stmt
            .query_row([], |row| Ok(decode_sqlite_value(row, 0)))
            .unwrap();
        assert_eq!(val, CellValue::Int(42));
    }

    #[test]
    fn test_decode_sqlite_value_text() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES ('hello')", []).unwrap();
        let mut stmt = conn.prepare("SELECT a FROM t").unwrap();
        let val = stmt
            .query_row([], |row| Ok(decode_sqlite_value(row, 0)))
            .unwrap();
        assert_eq!(val, CellValue::Text("hello".into()));
    }

    #[test]
    fn test_quote_sqlite_value_null() {
        assert_eq!(quote_sqlite_value(&CellValue::Null), "NULL");
    }

    #[test]
    fn test_quote_sqlite_value_int() {
        assert_eq!(quote_sqlite_value(&CellValue::Int(42)), "42");
    }

    #[test]
    fn test_quote_sqlite_value_text_with_quotes() {
        assert_eq!(
            quote_sqlite_value(&CellValue::Text("it's".into())),
            "'it''s'"
        );
    }

    #[test]
    fn test_quote_sqlite_value_float() {
        let result = quote_sqlite_value(&CellValue::Float(2.5));
        assert!(result.starts_with("2.5"));
    }

    #[test]
    fn test_quote_sqlite_value_blob() {
        assert_eq!(
            quote_sqlite_value(&CellValue::Blob(vec![0xDE, 0xAD])),
            "X'dead'"
        );
    }

    #[test]
    fn test_quote_sqlite_value_bool() {
        assert_eq!(quote_sqlite_value(&CellValue::Bool(true)), "1");
        assert_eq!(quote_sqlite_value(&CellValue::Bool(false)), "0");
    }

    #[test]
    fn test_quote_sqlite_value_datetime() {
        assert_eq!(
            quote_sqlite_value(&CellValue::DateTime("2024-01-15 10:30:00".into())),
            "'2024-01-15 10:30:00'"
        );
    }

    #[test]
    fn test_build_pk_where_single() {
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "INTEGER".into(),
            nullable: false,
            is_primary_key: true,
            default_value: None,
            max_length: None,
            extra: String::new(),
        }];
        let row = vec![CellValue::Int(42)];
        let result = build_pk_where(&["id".into()], &row, &columns).unwrap();
        assert_eq!(result, "\"id\" = 42");
    }

    #[test]
    fn test_build_pk_where_composite() {
        let columns = vec![
            ColumnInfo {
                name: "a".into(),
                data_type: "INTEGER".into(),
                nullable: false,
                is_primary_key: true,
                default_value: None,
                max_length: None,
                extra: String::new(),
            },
            ColumnInfo {
                name: "b".into(),
                data_type: "TEXT".into(),
                nullable: false,
                is_primary_key: true,
                default_value: None,
                max_length: None,
                extra: String::new(),
            },
        ];
        let row = vec![CellValue::Int(1), CellValue::Text("x".into())];
        let result = build_pk_where(&["a".into(), "b".into()], &row, &columns).unwrap();
        assert_eq!(result, "\"a\" = 1 AND \"b\" = 'x'");
    }

    #[test]
    fn test_build_pk_where_empty_pks() {
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "INTEGER".into(),
            nullable: false,
            is_primary_key: true,
            default_value: None,
            max_length: None,
            extra: String::new(),
        }];
        let row = vec![CellValue::Int(1)];
        let result = build_pk_where(&[], &row, &columns);
        assert!(result.is_err());
    }
}
