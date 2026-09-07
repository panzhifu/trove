//! SQL helpers shared by the stores.
//!
//! A thin synchronous wrapper over [`rusqlite`]; the store stays
//! single-threaded for the UI.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, Row, types::Value};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Run an `INSERT`/`UPDATE`/`DELETE`/DDL statement, returning rows changed.
pub fn execute(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<u64> {
    conn.execute(sql, rusqlite::params_from_iter(params))
        .map(|n| n as u64)
        .map_err(Error::from)
}

/// Run a `SELECT`, mapping every row to a value.
pub fn query_map<T>(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    mut map: impl FnMut(&Row) -> Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql).map_err(Error::from)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params)).map_err(Error::from)?;
    let mut out = Vec::new();
    loop {
        match rows.next().map_err(Error::from)? {
            Some(row) => out.push(map(&row)?),
            None => break,
        }
    }
    Ok(out)
}

/// Run a `SELECT` expecting at most one row.
pub fn query_one<T>(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    map: impl FnOnce(&Row) -> Result<T>,
) -> Result<Option<T>> {
    let mut stmt = conn.prepare(sql).map_err(Error::from)?;
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(Error::from)?;
    match rows.next().map_err(Error::from)? {
        Some(row) => map(&row).map(Some),
        None => Ok(None),
    }
}

/// Run a `SELECT` whose first column is a single integer (`COUNT(*)`, ...).
pub fn query_count(conn: &Connection, sql: &str, params: Vec<Value>) -> Result<i64> {
    Ok(query_one(conn, sql, params, |row| int(row, 0))?.unwrap_or(0))
}

/// Read a nullable `TEXT` column as `Option<String>`.
pub fn opt_str(row: &Row, ix: usize) -> Result<Option<String>> {
    row.get::<_, Option<String>>(ix).map_err(Error::from)
}

/// Read a non-null `TEXT` column.
pub fn req_str(row: &Row, ix: usize) -> Result<String> {
    row.get::<_, String>(ix).map_err(Error::from)
}

/// Read an integer column (`INTEGER`/`BOOLEAN`) as `i64`.
pub fn int(row: &Row, ix: usize) -> Result<i64> {
    row.get::<_, i64>(ix).map_err(Error::from)
}

/// Read an optional integer column.
pub fn opt_int(row: &Row, ix: usize) -> Result<Option<i64>> {
    row.get::<_, Option<i64>>(ix).map_err(Error::from)
}

/// Read a boolean column.
pub fn boolean(row: &Row, ix: usize) -> Result<bool> {
    Ok(int(row, ix)? != 0)
}

/// Read an optional `TEXT` column holding an RFC 3339 timestamp.
pub fn opt_ts(row: &Row, ix: usize) -> Result<Option<DateTime<Utc>>> {
    opt_str(row, ix)?.map(|s| parse_ts(&s)).transpose()
}

/// Read a required `TEXT` column holding an RFC 3339 timestamp.
pub fn req_ts(row: &Row, ix: usize) -> Result<DateTime<Utc>> {
    parse_ts(&req_str(row, ix)?)
}

/// Read a required `TEXT` primary key holding a UUID.
pub fn req_uuid(row: &Row, ix: usize) -> Result<Uuid> {
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

/// Map a rusqlite error onto the crate error type.
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}
