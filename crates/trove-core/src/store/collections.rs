//! Collection store: the user's folder tree and its many-to-many asset
//! membership.

use chrono::Utc;
use libsql::{Connection, Value};
use uuid::Uuid;

use super::rows::{self, bind_opt_uuid, int, req_str, req_ts, req_uuid};
use crate::error::{Error, Result};
use crate::model::{Collection, NewCollection};

/// Insert a collection, creating its id and timestamps.
pub fn create(conn: &Connection, input: &NewCollection) -> Result<Collection> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    rows::execute(
        conn,
        "INSERT INTO collections (id, parent_id, name, position, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        vec![
            rows::uuid(id).into(),
            bind_opt_uuid(input.parent_id),
            input.name.trim().to_string().into(),
            Value::Integer(input.position),
            rows::ts(now).into(),
        ],
    )?;
    Ok(Collection {
        id,
        parent_id: input.parent_id,
        name: input.name.trim().to_string(),
        position: input.position,
        created_at: now,
        updated_at: now,
    })
}

pub fn get(conn: &Connection, id: Uuid) -> Result<Option<Collection>> {
    rows::query_one(
        conn,
        "SELECT id, parent_id, name, position, created_at, updated_at
         FROM collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
        collection_from_row,
    )
}

/// Top-level collections ordered by position.
pub fn roots(conn: &Connection) -> Result<Vec<Collection>> {
    children_of(conn, None)
}

/// Every collection in the library (any depth), parents before children.
pub fn list(conn: &Connection) -> Result<Vec<Collection>> {
    rows::query_map(
        conn,
        "SELECT id, parent_id, name, position, created_at, updated_at
         FROM collections
         ORDER BY created_at ASC, position ASC, name ASC",
        vec![],
        collection_from_row,
    )
}

/// Direct children of `parent` (or of the library root when `None`), ordered
/// by position then name.
pub fn children_of(conn: &Connection, parent: Option<Uuid>) -> Result<Vec<Collection>> {
    let (sql, params) = match parent {
        Some(id) => (
            "SELECT id, parent_id, name, position, created_at, updated_at
             FROM collections WHERE parent_id = ?1
             ORDER BY position ASC, name ASC",
            vec![rows::uuid(id).into()],
        ),
        None => (
            "SELECT id, parent_id, name, position, created_at, updated_at
             FROM collections WHERE parent_id IS NULL
             ORDER BY position ASC, name ASC",
            vec![],
        ),
    };
    rows::query_map(conn, sql, params, collection_from_row)
}

/// Find (or create) the top-level collection named `name` (exact, trimmed
/// match). Used by auto-import to bucket files by source folder / month / etc.
pub fn ensure_root_named(conn: &Connection, name: &str) -> Result<Collection> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Validation(
            "collection name must not be empty".into(),
        ));
    }
    let existing = children_of(conn, None)?;
    let existing_len = existing.len();
    if let Some(c) = existing.iter().find(|c| c.name == name) {
        return Ok(c.clone());
    }
    create(
        conn,
        &NewCollection {
            parent_id: None,
            name: name.to_string(),
            position: existing_len as i64,
        },
    )
}

/// Rename a collection.
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
        "UPDATE collections SET name = ?1, updated_at = ?2 WHERE id = ?3",
        vec![
            name.to_string().into(),
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    if changed == 0 {
        return Err(Error::NotFound("collection"));
    }
    Ok(())
}

/// Move a collection under `new_parent` at `position`, refusing cycles.
pub fn move_to(conn: &Connection, id: Uuid, new_parent: Option<Uuid>, position: i64) -> Result<()> {
    if new_parent == Some(id) {
        return Err(Error::Validation(
            "a collection cannot be its own parent".into(),
        ));
    }
    if let Some(parent) = new_parent
        && is_descendant(conn, parent, id)?
    {
        return Err(Error::Validation(
            "cannot move a collection under its own descendant".into(),
        ));
    }
    rows::execute(
        conn,
        "UPDATE collections SET parent_id = ?1, position = ?2, updated_at = ?3 WHERE id = ?4",
        vec![
            bind_opt_uuid(new_parent),
            Value::Integer(position),
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    Ok(())
}

/// Delete a collection. The DB cascades to children and to the
/// `asset_collection` membership rows; the assets themselves are kept.
pub fn delete(conn: &Connection, id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
    )?;
    Ok(())
}

// -- asset membership --------------------------------------------------------

/// Attach an asset to a collection. Re-attaching is a no-op that keeps the
/// existing position.
pub fn add_asset(conn: &Connection, collection_id: Uuid, asset_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "INSERT OR IGNORE INTO asset_collection (asset_id, collection_id, position)
         VALUES (?1, ?2,
             (SELECT COALESCE(MAX(position) + 1, 0) FROM asset_collection WHERE collection_id = ?2))",
        vec![rows::uuid(asset_id).into(), rows::uuid(collection_id).into()],
    )?;
    Ok(())
}

/// Detach an asset from a collection.
pub fn remove_asset(conn: &Connection, collection_id: Uuid, asset_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM asset_collection WHERE collection_id = ?1 AND asset_id = ?2",
        vec![
            rows::uuid(collection_id).into(),
            rows::uuid(asset_id).into(),
        ],
    )?;
    Ok(())
}

/// Asset ids inside a collection in membership order.
pub fn asset_ids(conn: &Connection, collection_id: Uuid) -> Result<Vec<Uuid>> {
    rows::query_map(
        conn,
        "SELECT asset_id FROM asset_collection
         WHERE collection_id = ?1 ORDER BY position ASC",
        vec![rows::uuid(collection_id).into()],
        |row| req_uuid(row, 0),
    )
}

/// Number of assets directly in a collection (excluding nested collections).
pub fn count_assets(conn: &Connection, collection_id: Uuid) -> Result<u64> {
    Ok(rows::query_count(
        conn,
        "SELECT COUNT(*) FROM asset_collection WHERE collection_id = ?1",
        vec![rows::uuid(collection_id).into()],
    )? as u64)
}

// -- helpers -----------------------------------------------------------------

fn collection_from_row(row: &libsql::Row) -> Result<Collection> {
    Ok(Collection {
        id: req_uuid(row, 0)?,
        parent_id: {
            let s: Option<String> = row.get::<Option<String>>(1)?;
            match s {
                Some(v) => Some(rows::parse_uuid(&v)?),
                None => None,
            }
        },
        name: req_str(row, 2)?,
        position: int(row, 3)?,
        created_at: req_ts(row, 4)?,
        updated_at: req_ts(row, 5)?,
    })
}

/// Whether `candidate` is an ancestor of `id` (walking parents up).
fn is_descendant(conn: &Connection, mut candidate: Uuid, id: Uuid) -> Result<bool> {
    let mut guard = 0usize;
    while guard < 10_000 {
        if candidate == id {
            return Ok(true);
        }
        let Some(parent) = parent_of(conn, candidate)? else {
            return Ok(false);
        };
        candidate = parent;
        guard += 1;
    }
    Ok(true)
}

fn parent_of(conn: &Connection, id: Uuid) -> Result<Option<Uuid>> {
    rows::query_one(
        conn,
        "SELECT parent_id FROM collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
        |row| match row.get::<Option<String>>(0)? {
            Some(v) => Ok(Some(rows::parse_uuid(&v)?)),
            None => Ok(None),
        },
    )
    .map(|opt| opt.flatten())
}
