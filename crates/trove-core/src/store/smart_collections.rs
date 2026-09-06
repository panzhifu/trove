//! Smart-collection store: persisted saved searches (name + JSON condition
//! tree). Evaluation lives in [`super::smart`].

use chrono::Utc;
use libsql::{Connection, Value};
use uuid::Uuid;

use super::rows::{self, bind_opt_str, req_ts, req_uuid};
use crate::error::{Error, Result};
use crate::model::{NewSmartCollection, SmartCollection};

/// Insert a smart collection, creating its id and timestamps.
pub fn create(conn: &Connection, input: &NewSmartCollection) -> Result<SmartCollection> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    let query = serde_json::to_string(&input.query)?;
    rows::execute(
        conn,
        "INSERT INTO smart_collections (id, name, query, color, position, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        vec![
            rows::uuid(id).into(),
            input.name.trim().to_string().into(),
            query.into(),
            bind_opt_str(input.color.as_deref()),
            Value::Integer(input.position),
            rows::ts(now).into(),
        ],
    )?;
    Ok(SmartCollection {
        id,
        name: input.name.trim().to_string(),
        query: input.query.clone(),
        color: input.color.clone(),
        position: input.position,
        created_at: now,
        updated_at: now,
    })
}

pub fn get(conn: &Connection, id: Uuid) -> Result<Option<SmartCollection>> {
    rows::query_one(
        conn,
        "SELECT id, name, query, color, position, created_at, updated_at
         FROM smart_collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
        collection_from_row,
    )
}

/// All smart collections in display order.
pub fn list(conn: &Connection) -> Result<Vec<SmartCollection>> {
    rows::query_map(
        conn,
        "SELECT id, name, query, color, position, created_at, updated_at
         FROM smart_collections ORDER BY position ASC, created_at ASC",
        vec![],
        collection_from_row,
    )
}

/// Rename a smart collection.
pub fn rename(conn: &Connection, id: Uuid, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Validation("name must not be empty".into()));
    }
    if name.len() > crate::model::MAX_NAME_LEN {
        return Err(Error::Validation("name too long".into()));
    }
    let changed = rows::execute(
        conn,
        "UPDATE smart_collections SET name = ?1, updated_at = ?2 WHERE id = ?3",
        vec![
            name.to_string().into(),
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    if changed == 0 {
        return Err(Error::NotFound("smart_collection"));
    }
    Ok(())
}

/// Replace the stored condition tree of a smart collection. The tree is
/// validated by compiling it before the row is touched.
pub fn update_query(conn: &Connection, id: Uuid, query: &serde_json::Value) -> Result<()> {
    // Validate up front: an uncompilable tree must not land in the store.
    let node = super::smart::node_from_json(query)?;
    super::smart::compile(&node)?;
    let changed = rows::execute(
        conn,
        "UPDATE smart_collections SET query = ?1, updated_at = ?2 WHERE id = ?3",
        vec![
            serde_json::to_string(query)
                .map_err(|e| Error::Db(format!("serialize query: {e}")))?
                .into(),
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    if changed == 0 {
        return Err(Error::NotFound("smart_collection"));
    }
    Ok(())
}

/// Delete a smart collection (just the saved search; assets are untouched).
pub fn delete(conn: &Connection, id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM smart_collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
    )?;
    Ok(())
}

fn collection_from_row(row: &libsql::Row) -> Result<SmartCollection> {
    let query_json = rows::req_str(row, 2)?;
    let query = serde_json::from_str(&query_json).map_err(|e| {
        Error::Db(format!("smart_collections: bad query json: {e}"))
    })?;
    Ok(SmartCollection {
        id: req_uuid(row, 0)?,
        name: rows::req_str(row, 1)?,
        query,
        color: rows::opt_str(row, 3)?,
        position: rows::int(row, 4)?,
        created_at: req_ts(row, 5)?,
        updated_at: req_ts(row, 6)?,
    })
}