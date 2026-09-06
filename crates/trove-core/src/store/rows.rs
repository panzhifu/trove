//! SQL helpers shared by the stores.
//!
//! libsql's public connection API is async even for a local file; these
//! helpers run each call to completion on the current thread with `pollster`
//! so the store stays synchronous for the (single-threaded) application.

use chrono::{DateTime, Utc};
use libsql::{Connection, Row, Value};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Run an `INSERT`/`UPDATE`/`DELETE`/DDL statement, returning rows changed.
pub fn execute(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<u64> {
    pollster::block_on(conn.execute(sql, params)).map_err(Error::from)
}

/// Run a batch of statements atomically (migrations).
pub fn execute_transactional_batch(conn: &Connection, sql: &str) -> Result<()> {
    let _ = pollster::block_on(conn.execute_transactional_batch(sql)).map_err(Error::from)?;
    Ok(())
}

/// Run `f` inside a `BEGIN … COMMIT` transaction on `conn`. On error the work
/// is rolled back and the error returned. Statements run after.
pub fn transaction<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    execute(conn, "BEGIN", vec![])?;
    match f(conn) {
        Ok(v) => {
            execute(conn, "COMMIT", vec![])?;
            Ok(v)
        }
        Err(e) => {
            let _ = execute(conn, "ROLLBACK", vec![]);
            Err(e)
        }
    }
}

/// Run a `SELECT`, mapping every row to a value **inside** the fetch loop.
///
/// libsql `Row::get` reads lazily from the underlying statement, which is
/// only valid while the row is current — so rows must be materialized before
/// the next `next()` advances the cursor.
pub fn query_map<T>(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    mut map: impl FnMut(&Row) -> Result<T>,
) -> Result<Vec<T>> {
    let mut rows = pollster::block_on(conn.query(sql, params)).map_err(Error::from)?;
    let mut out = Vec::new();
    while let Some(row) = pollster::block_on(rows.next()).map_err(Error::from)? {
        out.push(map(&row)?);
    }
    Ok(out)
}

/// Run a `SELECT` expecting at most one row, materialized immediately.
pub fn query_one<T>(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    map: impl FnOnce(&Row) -> Result<T>,
) -> Result<Option<T>> {
    let mut rows = pollster::block_on(conn.query(sql, params)).map_err(Error::from)?;
    match pollster::block_on(rows.next()).map_err(Error::from)? {
        Some(row) => map(&row).map(Some),
        None => Ok(None),
    }
}

/// Run a `SELECT` whose first column is a single integer (`COUNT(*)`, ...).
pub fn query_count(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<i64> {
    Ok(query_one(conn, sql, params, |row| int(row, 0))?
        .unwrap_or(0))
}

/// Read a nullable `TEXT` column as `Option<String>`.
pub fn opt_str(row: &Row, ix: i32) -> Result<Option<String>> {
    Ok(row.get::<Option<String>>(ix)?)
}

/// Read a non-null `TEXT` column.
pub fn req_str(row: &Row, ix: i32) -> Result<String> {
    row.get::<String>(ix).map_err(Error::from)
}

/// Read an integer column (`INTEGER`/`BOOLEAN`) as `i64`.
pub fn int(row: &Row, ix: i32) -> Result<i64> {
    row.get::<i64>(ix).map_err(Error::from)
}

/// Read an optional integer column.
pub fn opt_int(row: &Row, ix: i32) -> Result<Option<i64>> {
    Ok(row.get::<Option<i64>>(ix)?)
}

/// Read a boolean column.
pub fn boolean(row: &Row, ix: i32) -> Result<bool> {
    row.get::<bool>(ix).map_err(Error::from)
}

/// Read an optional `TEXT` column holding an RFC 3339 timestamp.
pub fn opt_ts(row: &Row, ix: i32) -> Result<Option<DateTime<Utc>>> {
    opt_str(row, ix)?.map(|s| parse_ts(&s)).transpose()
}

/// Read a required `TEXT` column holding an RFC 3339 timestamp.
pub fn req_ts(row: &Row, ix: i32) -> Result<DateTime<Utc>> {
    parse_ts(&req_str(row, ix)?)
}

/// Read a required `TEXT` primary key holding a UUID.
pub fn req_uuid(row: &Row, ix: i32) -> Result<Uuid> {
    parse_uuid(&req_str(row, ix)?)
}

/// Serialize a timestamp for storage (RFC 3339, UTC).
pub fn ts(v: DateTime<Utc>) -> String {
    v.to_rfc3339()
}

/// Serialize a UUID for storage.
pub fn uuid(v: Uuid) -> String {
    v.to_string()
}

/// Bind an optional string.
pub fn bind_opt_str(v: Option<&str>) -> Value {
    match v {
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    }
}

/// Bind an optional timestamp.
pub fn bind_opt_ts(v: Option<DateTime<Utc>>) -> Value {
    bind_opt_str(v.map(ts).as_deref())
}

/// Bind an optional UUID.
pub fn bind_opt_uuid(v: Option<Uuid>) -> Value {
    bind_opt_str(v.map(uuid).as_deref())
}

/// Bind an optional integer.
pub fn bind_opt_int(v: Option<i64>) -> Value {
    match v {
        Some(i) => Value::Integer(i),
        None => Value::Null,
    }
}

pub fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| Error::Validation(format!("bad timestamp {s:?}: {e}")))
}

pub fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| Error::Validation(format!("bad uuid {s:?}: {e}")))
}

/// Map a libsql error onto the crate error type.
impl From<libsql::Error> for Error {
    fn from(e: libsql::Error) -> Self {
        Error::Db(e.to_string())
    }
}
