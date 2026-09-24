//! Tag store: case-insensitively unique labels, nestable via `parent_id`
//! (a filter on a tag implicitly includes its whole subtree).

use chrono::Utc;
use rusqlite::Connection;
use uuid::Uuid;

use super::rows::{self, req_ts, req_uuid};
use crate::error::{Error, Result};
use crate::model::{NewTag, Tag};

/// Column list shared by every tag read; order matches `tag_from_row`.
const COLS: &str = "id, name, color, created_at, parent_id";

fn tag_from_row(row: &rusqlite::Row) -> Result<Tag> {
    Ok(Tag {
        id: req_uuid(row, 0)?,
        name: row.get::<_, String>(1)?,
        color: row.get::<_, Option<String>>(2)?,
        created_at: req_ts(row, 3)?,
        // UUIDs are stored as TEXT; parse after reading.
        parent_id: rows::opt_str(row, 4)?.and_then(|s| Uuid::parse_str(&s).ok()),
    })
}

/// Create a tag.
pub fn create(conn: &Connection, input: &NewTag) -> Result<Tag> {
    let id = Uuid::new_v4();
    let now = Utc::now();
    rows::execute(
        conn,
        "INSERT INTO tags (id, name, color, created_at, parent_id) VALUES (?1, ?2, ?3, ?4, ?5)",
        vec![
            rows::uuid(id).into(),
            input.name.trim().to_string().into(),
            rows::bind_opt_str(input.color.as_deref()),
            rows::ts(now).into(),
            input
                .parent_id
                .map(|u| rows::uuid(u).into())
                .unwrap_or(rusqlite::types::Value::Null),
        ],
    )?;
    Ok(Tag {
        id,
        name: input.name.trim().to_string(),
        color: input.color.clone(),
        parent_id: input.parent_id,
        created_at: now,
    })
}

/// Fetch a tag by id.
pub fn get(conn: &Connection, id: Uuid) -> Result<Option<Tag>> {
    rows::query_one(
        conn,
        &format!("SELECT {COLS} FROM tags WHERE id = ?1"),
        vec![rows::uuid(id).into()],
        tag_from_row,
    )
}

/// Find a tag by exact (case-insensitive) name.
pub fn get_by_name(conn: &Connection, name: &str) -> Result<Option<Tag>> {
    rows::query_one(
        conn,
        &format!("SELECT {COLS} FROM tags WHERE name = ?1 COLLATE NOCASE"),
        vec![name.trim().to_string().into()],
        tag_from_row,
    )
}

/// `id` plus every tag below it (recursive CTE). Filtering or counting a
/// tag always operates on this subtree.
pub fn subtree_ids(conn: &Connection, tag_id: Uuid) -> Result<Vec<Uuid>> {
    rows::query_map(
        conn,
        "WITH RECURSIVE sub(id) AS ( \
             SELECT id FROM tags WHERE id = ?1 \
             UNION ALL \
             SELECT t.id FROM tags t JOIN sub s ON t.parent_id = s.id \
         ) SELECT id FROM sub",
        vec![rows::uuid(tag_id).into()],
        |row| req_uuid(row, 0),
    )
}

/// Move a tag under `parent` (`None` = root level). Rejects moving a tag
/// into its own subtree (that would orphan the rest of the tree).
pub fn move_to(conn: &Connection, tag_id: Uuid, parent: Option<Uuid>) -> Result<()> {
    if Some(tag_id) == parent {
        return Err(Error::Validation("a tag cannot be its own parent".into()));
    }
    if let Some(pid) = parent
        && subtree_ids(conn, tag_id)?.contains(&pid)
    {
        return Err(Error::Validation(
            "cannot move a tag under its own descendant".into(),
        ));
    }
    rows::execute(
        conn,
        "UPDATE tags SET parent_id = ?1 WHERE id = ?2",
        vec![
            parent
                .map(|u| rows::uuid(u).into())
                .unwrap_or(rusqlite::types::Value::Null),
            rows::uuid(tag_id).into(),
        ],
    )?;
    Ok(())
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
    create(
        conn,
        &NewTag {
            name: name.into(),
            color: None,
            parent_id: None,
        },
    )
}

/// All tags ordered by name.
pub fn list(conn: &Connection) -> Result<Vec<Tag>> {
    rows::query_map(
        conn,
        &format!("SELECT {COLS} FROM tags ORDER BY name COLLATE NOCASE ASC"),
        vec![],
        tag_from_row,
    )
}

/// Tags attached to one asset, ordered by name.
pub fn for_asset(conn: &Connection, asset_id: Uuid) -> Result<Vec<Tag>> {
    rows::query_map(
        conn,
        "SELECT t.id, t.name, t.color, t.created_at, t.parent_id
         FROM tags t
         JOIN asset_tag at ON at.tag_id = t.id
         WHERE at.asset_id = ?1
         ORDER BY t.name COLLATE NOCASE ASC",
        vec![rows::uuid(asset_id).into()],
        tag_from_row,
    )
}

/// Number of assets carrying the tag or any of its descendants (matches
/// the hierarchical filter semantics).
pub fn count_assets(conn: &Connection, tag_id: Uuid) -> Result<u64> {
    let ids = subtree_ids(conn, tag_id)?;
    if ids.is_empty() {
        return Ok(0);
    }
    // Uuids are hex-only, so quoting is safe.
    let list = ids
        .iter()
        .map(|id| format!("'{}'", id))
        .collect::<Vec<_>>()
        .join(",");
    Ok(rows::query_count(
        conn,
        &format!(
            "SELECT COUNT(DISTINCT at.asset_id) FROM asset_tag at \
             WHERE at.tag_id IN ({list}) AND {}",
            super::sequences::hidden_beside("at.asset_id")
        ),
        vec![],
    )? as u64)
}

/// [`count_assets`] for *every* tag at once, as one map.
///
/// `count_assets` costs two queries per tag — a recursive subtree walk plus a
/// `COUNT(DISTINCT …)`. That is fine for a single tag and ruinous for a panel
/// that wants a number on every row: 30 tags meant 60 statements, and on a
/// 100k-asset library each one measured 1.9 ms. This asks the same question
/// once, letting a single recursive CTE expand every tag's subtree, and returns
/// a plain map for the caller to look up.
///
/// Same semantics as [`count_assets`], trashed assets included: a tag with no
/// assets (or no subtree, i.e. one whose row is gone) is simply absent from the
/// map, so callers should default missing ids to 0.
///
/// Both counts skip a sequence's hidden members, which is the one place they
/// part with "everything tagged": the sidebar number has to be the count of the
/// cards the tag's listing will actually show, or it disagrees with the view it
/// is a label for.
pub fn counts_by_tag(conn: &Connection) -> Result<std::collections::HashMap<Uuid, u64>> {
    let counts: Vec<(Uuid, u64)> = rows::query_map(
        conn,
        &format!(
            "WITH RECURSIVE sub(root, id) AS ( \
             SELECT id, id FROM tags \
             UNION ALL \
             SELECT s.root, t.id FROM tags t JOIN sub s ON t.parent_id = s.id \
             ) \
             SELECT sub.root, COUNT(DISTINCT at.asset_id) \
             FROM sub JOIN asset_tag at ON at.tag_id = sub.id \
             WHERE {} \
             GROUP BY sub.root",
            super::sequences::hidden_beside("at.asset_id")
        ),
        vec![],
        |row| Ok((req_uuid(row, 0)?, rows::int(row, 1)? as u64)),
    )?;
    Ok(counts.into_iter().collect())
}

/// Attach a tag to an asset (idempotent).
pub fn add_to_asset(conn: &Connection, asset_id: Uuid, tag_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "INSERT OR IGNORE INTO asset_tag (asset_id, tag_id) VALUES (?1, ?2)",
        vec![rows::uuid(asset_id).into(), rows::uuid(tag_id).into()],
    )?;
    Ok(())
}

/// Detach a tag from an asset.
pub fn remove_from_asset(conn: &Connection, asset_id: Uuid, tag_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM asset_tag WHERE asset_id = ?1 AND tag_id = ?2",
        vec![rows::uuid(asset_id).into(), rows::uuid(tag_id).into()],
    )?;
    Ok(())
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
    // The search index picks the batch up through the outbox triggers.
    Ok(())
}

/// Permanently delete a tag. Membership rows cascade.
pub fn delete(conn: &Connection, tag_id: Uuid) -> Result<()> {
    // The outbox triggers on the membership cascade keep the search index
    // current; nothing to collect here anymore.
    rows::execute(
        conn,
        "DELETE FROM tags WHERE id = ?1",
        vec![rows::uuid(tag_id).into()],
    )?;
    Ok(())
}

/// Rename a tag. Tag names are part of the search index, so every asset
/// carrying the tag is re-synced afterwards. Renaming onto an existing
/// (case-insensitive) name hits the unique constraint and fails.
pub fn rename(conn: &Connection, tag_id: Uuid, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Validation("tag name must not be empty".into()));
    }
    if let Some(existing) = get_by_name(conn, name)?
        && existing.id != tag_id
    {
        return Err(Error::Validation(format!("tag `{name}` already exists")));
    }
    rows::execute(
        conn,
        "UPDATE tags SET name = ?1 WHERE id = ?2",
        vec![name.to_string().into(), rows::uuid(tag_id).into()],
    )?;
    Ok(())
}

/// Set (or clear) the display color of a tag. Not part of the search index,
/// so no re-sync is needed. The color is normalized to lowercase `#rrggbb`.
pub fn set_color(conn: &Connection, tag_id: Uuid, color: Option<&str>) -> Result<()> {
    let color = color.map(super::smart::normalize_color).transpose()?;
    rows::execute(
        conn,
        "UPDATE tags SET color = ?1 WHERE id = ?2",
        vec![
            rows::bind_opt_str(color.as_deref()),
            rows::uuid(tag_id).into(),
        ],
    )?;
    Ok(())
}
