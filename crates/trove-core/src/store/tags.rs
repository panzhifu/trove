//! Tag store: flat, case-insensitively unique labels and their assets.

use chrono::Utc;
use libsql::Connection;
use uuid::Uuid;

use super::assets;
use super::rows::{self, req_ts, req_uuid};
use crate::error::{Error, Result};
use crate::model::{NewTag, Tag};

fn tag_from_row(row: &libsql::Row) -> Result<Tag> {
    Ok(Tag {
        id: req_uuid(row, 0)?,
        name: row.get::<String>(1)?,
        color: row.get::<Option<String>>(2)?,
        created_at: req_ts(row, 3)?,
    })
}

/// Create a tag.
pub fn create(conn: &Connection, input: &NewTag) -> Result<Tag> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    rows::execute(
        conn,
        "INSERT INTO tags (id, name, color, created_at) VALUES (?1, ?2, ?3, ?4)",
        vec![
            rows::uuid(id).into(),
            input.name.trim().to_string().into(),
            rows::bind_opt_str(input.color.as_deref()),
            rows::ts(now).into(),
        ],
    )?;
    Ok(Tag {
        id,
        name: input.name.trim().to_string(),
        color: input.color.clone(),
        created_at: now,
    })
}

/// Fetch a tag by id.
pub fn get(conn: &Connection, id: Uuid) -> Result<Option<Tag>> {
    rows::query_one(
        conn,
        "SELECT id, name, color, created_at FROM tags WHERE id = ?1",
        vec![rows::uuid(id).into()],
        tag_from_row,
    )
}

/// Find a tag by exact (case-insensitive) name.
pub fn get_by_name(conn: &Connection, name: &str) -> Result<Option<Tag>> {
    rows::query_one(
        conn,
        "SELECT id, name, color, created_at FROM tags WHERE name = ?1 COLLATE NOCASE",
        vec![name.trim().to_string().into()],
        tag_from_row,
    )
}

/// Find or create a tag by name, preserving the caller's casing for display.
pub fn ensure_named(conn: &Connection, name: &str) -> Result<Tag> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Validation("tag name must not be empty".into()));
    }
    if let Some(tag) = get_by_name(conn, name)? {
        return Ok(tag);
    }
    create(conn, &NewTag { name: name.into(), color: None })
}

/// All tags ordered by name.
pub fn list(conn: &Connection) -> Result<Vec<Tag>> {
    rows::query_map(
        conn,
        "SELECT id, name, color, created_at FROM tags ORDER BY name COLLATE NOCASE ASC",
        vec![],
        tag_from_row,
    )
}

/// Tags attached to one asset, ordered by name.
pub fn for_asset(conn: &Connection, asset_id: Uuid) -> Result<Vec<Tag>> {
    rows::query_map(
        conn,
        "SELECT t.id, t.name, t.color, t.created_at
         FROM tags t
         JOIN asset_tag at ON at.tag_id = t.id
         WHERE at.asset_id = ?1
         ORDER BY t.name COLLATE NOCASE ASC",
        vec![rows::uuid(asset_id).into()],
        tag_from_row,
    )
}

/// Number of assets carrying a tag.
pub fn count_assets(conn: &Connection, tag_id: Uuid) -> Result<u64> {
    Ok(rows::query_count(
        conn,
        "SELECT COUNT(*) FROM asset_tag WHERE tag_id = ?1",
        vec![rows::uuid(tag_id).into()],
    )? as u64)
}

/// Attach a tag to an asset (idempotent).
pub fn add_to_asset(conn: &Connection, asset_id: Uuid, tag_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "INSERT OR IGNORE INTO asset_tag (asset_id, tag_id) VALUES (?1, ?2)",
        vec![rows::uuid(asset_id).into(), rows::uuid(tag_id).into()],
    )?;
    // Tag names are part of the searchable text; refresh the index entry.
    assets::fts_sync(conn, asset_id)
}

/// Detach a tag from an asset.
pub fn remove_from_asset(conn: &Connection, asset_id: Uuid, tag_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM asset_tag WHERE asset_id = ?1 AND tag_id = ?2",
        vec![rows::uuid(asset_id).into(), rows::uuid(tag_id).into()],
    )?;
    assets::fts_sync(conn, asset_id)
}

/// Replace the tag set of an asset with `tag_ids`.
pub fn set_for_asset(conn: &Connection, asset_id: Uuid, tag_ids: &[Uuid]) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM asset_tag WHERE asset_id = ?1",
        vec![rows::uuid(asset_id).into()],
    )?;
    for tag_id in tag_ids {
        rows::execute(
            conn,
            "INSERT OR IGNORE INTO asset_tag (asset_id, tag_id) VALUES (?1, ?2)",
            vec![rows::uuid(asset_id).into(), rows::uuid(*tag_id).into()],
        )?;
    }
    // One sync for the whole batch, after all membership rows are written.
    assets::fts_sync(conn, asset_id)
}

/// Permanently delete a tag. Membership rows cascade.
pub fn delete(conn: &Connection, tag_id: Uuid) -> Result<()> {
    // Collect affected assets first: the cascade below removes the
    // membership rows we would need to find them afterwards.
    let affected: Vec<Uuid> = rows::query_map(
        conn,
        "SELECT asset_id FROM asset_tag WHERE tag_id = ?1",
        vec![rows::uuid(tag_id).into()],
        |row| req_uuid(row, 0),
    )?;
    rows::execute(
        conn,
        "DELETE FROM tags WHERE id = ?1",
        vec![rows::uuid(tag_id).into()],
    )?;
    for asset_id in affected {
        assets::fts_sync(conn, asset_id)?;
    }
    Ok(())
}
