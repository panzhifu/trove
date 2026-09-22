//! Smart-collection store: persisted saved searches (name + JSON condition
//! tree). Evaluation lives in [`super::smart`].
//!
//! Smart collections nest: `parent_id` may reference a regular collection or
//! another smart collection, which one SQL FK cannot express — existence and
//! acyclicity are validated here. Deleting a parent (of either kind) removes
//! the smart subtree with it.

use chrono::Utc;
use rusqlite::{Connection, types::Value};
use uuid::Uuid;

use super::rows::{self, bind_opt_uuid, req_ts, req_uuid};
use crate::error::{Error, Result};
use crate::model::{Appearance, NewSmartCollection, SmartCollection};

/// Insert a smart collection, creating its id and timestamps.
pub fn create(conn: &Connection, input: &NewSmartCollection) -> Result<SmartCollection> {
    if let Some(parent) = input.parent_id
        && !parent_exists(conn, parent)?
    {
        return Err(Error::Validation(
            "smart collection parent does not exist".into(),
        ));
    }
    let id = Uuid::new_v4();
    let now = Utc::now();
    let query = serde_json::to_string(&input.query)?;
    rows::execute(
        conn,
        "INSERT INTO smart_collections (id, parent_id, name, query, position, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        vec![
            rows::uuid(id).into(),
            bind_opt_uuid(input.parent_id),
            input.name.trim().to_string().into(),
            query.into(),
            Value::Integer(input.position),
            rows::ts(now).into(),
        ],
    )?;
    Ok(SmartCollection {
        id,
        parent_id: input.parent_id,
        name: input.name.trim().to_string(),
        query: input.query.clone(),
        appearance: Appearance::default(),
        position: input.position,
        created_at: now,
        updated_at: now,
    })
}

pub fn get(conn: &Connection, id: Uuid) -> Result<Option<SmartCollection>> {
    rows::query_one(
        conn,
        "SELECT id, parent_id, name, query, position, created_at, updated_at, appearance
         FROM smart_collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
        collection_from_row,
    )
}

/// All smart collections in display order.
pub fn list(conn: &Connection) -> Result<Vec<SmartCollection>> {
    rows::query_map(
        conn,
        "SELECT id, parent_id, name, query, position, created_at, updated_at, appearance
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
///
/// The look of the folder is not part of this: rules and appearance are edited
/// in different places and rewriting one while saving the other would quietly
/// undo an edit made in between.
pub fn update_query(conn: &Connection, id: Uuid, query: &serde_json::Value) -> Result<()> {
    // Validate up front: an uncompilable tree must not land in the store.
    let node = super::smart::node_from_json(query)?;
    super::smart::compile(None, None, &node)?;
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

/// Set (or clear) a smart collection's own glyph and accent — see
/// [`super::collections::set_appearance`], which this mirrors.
pub fn set_appearance(conn: &Connection, id: Uuid, appearance: &Appearance) -> Result<()> {
    let changed = rows::execute(
        conn,
        "UPDATE smart_collections SET appearance = ?1, updated_at = ?2 WHERE id = ?3",
        vec![
            appearance
                .clone()
                .sanitized()
                .as_ref()
                .and_then(|a| a.to_storage())
                .map(Value::Text)
                .unwrap_or(Value::Null),
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    if changed == 0 {
        return Err(Error::NotFound("smart_collection"));
    }
    Ok(())
}

/// Move a smart collection under `new_parent` at `position`. The parent may
/// be a regular collection or another smart collection; cycles are refused.
pub fn move_to(conn: &Connection, id: Uuid, new_parent: Option<Uuid>, position: i64) -> Result<()> {
    if new_parent == Some(id) {
        return Err(Error::Validation(
            "a smart collection cannot be its own parent".into(),
        ));
    }
    if let Some(parent) = new_parent {
        if !parent_exists(conn, parent)? {
            return Err(Error::Validation(
                "smart collection parent does not exist".into(),
            ));
        }
        if is_descendant(conn, parent, id)? {
            return Err(Error::Validation(
                "cannot move a smart collection under its own descendant".into(),
            ));
        }
    }
    let changed = rows::execute(
        conn,
        "UPDATE smart_collections SET parent_id = ?1, position = ?2, updated_at = ?3 WHERE id = ?4",
        vec![
            bind_opt_uuid(new_parent),
            Value::Integer(position),
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    if changed == 0 {
        return Err(Error::NotFound("smart_collection"));
    }
    Ok(())
}

/// Delete a smart collection together with its smart descendants (the saved
/// searches only; assets are untouched).
pub fn delete(conn: &Connection, id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "WITH RECURSIVE doomed(id) AS (
             SELECT id FROM smart_collections WHERE id = ?1
             UNION
             SELECT sc.id FROM smart_collections sc JOIN doomed d ON sc.parent_id = d.id
         )
         DELETE FROM smart_collections WHERE id IN (SELECT id FROM doomed)",
        vec![rows::uuid(id).into()],
    )?;
    Ok(())
}

/// Delete every smart collection nested under the regular collection
/// `id` or under any of its descendant collections, plus their smart
/// descendants. Called when the collection itself is deleted so no smart
/// child is left with a dangling parent.
pub fn delete_under_collection(conn: &Connection, collection_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "WITH RECURSIVE
             colls(id) AS (
                 SELECT id FROM collections WHERE id = ?1
                 UNION
                 SELECT c.id FROM collections c JOIN colls p ON c.parent_id = p.id
             ),
             doomed(id) AS (
                 SELECT id FROM smart_collections WHERE parent_id IN (SELECT id FROM colls)
                 UNION
                 SELECT sc.id FROM smart_collections sc JOIN doomed d ON sc.parent_id = d.id
             )
         DELETE FROM smart_collections WHERE id IN (SELECT id FROM doomed)",
        vec![rows::uuid(collection_id).into()],
    )?;
    Ok(())
}

fn collection_from_row(row: &rusqlite::Row) -> Result<SmartCollection> {
    let query_json = rows::req_str(row, 3)?;
    let query = serde_json::from_str(&query_json)
        .map_err(|e| Error::Db(format!("smart_collections: bad query json: {e}")))?;
    Ok(SmartCollection {
        id: req_uuid(row, 0)?,
        parent_id: rows::opt_uuid(row, 1)?,
        name: rows::req_str(row, 2)?,
        query,
        position: rows::int(row, 4)?,
        created_at: req_ts(row, 5)?,
        updated_at: req_ts(row, 6)?,
        appearance: Appearance::from_storage(rows::opt_str(row, 7)?.as_deref()),
    })
}

// -- helpers -----------------------------------------------------------------

/// Whether `id` exists as a smart collection or a regular collection, i.e. is
/// a legal parent for a smart collection.
fn parent_exists(conn: &Connection, id: Uuid) -> Result<bool> {
    let n = rows::query_count(
        conn,
        "SELECT (SELECT COUNT(*) FROM smart_collections WHERE id = ?1)
              + (SELECT COUNT(*) FROM collections WHERE id = ?1)",
        vec![rows::uuid(id).into()],
    )?;
    Ok(n > 0)
}

/// Whether `candidate` is an ancestor of `id` along the smart-collection
/// parent chain. A chain that reaches a regular collection ends there —
/// collections can never have a smart-collection ancestor, so no cycle is
/// possible through them.
fn is_descendant(conn: &Connection, mut candidate: Uuid, id: Uuid) -> Result<bool> {
    let mut guard = 0usize;
    while guard < 10_000 {
        if candidate == id {
            return Ok(true);
        }
        let Some(parent) = smart_parent_of(conn, candidate)? else {
            return Ok(false);
        };
        candidate = parent;
        guard += 1;
    }
    Ok(true)
}

/// The raw `parent_id` of a smart collection, or `None` when the id is not a
/// smart collection (covers collection parents, ending the walk).
fn smart_parent_of(conn: &Connection, id: Uuid) -> Result<Option<Uuid>> {
    rows::query_one(
        conn,
        "SELECT parent_id FROM smart_collections WHERE id = ?1",
        vec![rows::uuid(id).into()],
        |row| match row.get::<_, Option<String>>(0)? {
            Some(v) => Ok(Some(rows::parse_uuid(&v)?)),
            None => Ok(None),
        },
    )
    .map(|opt| opt.flatten())
}
