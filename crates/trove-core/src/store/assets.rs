//! Asset store: insert / read / query / patch / lifecycle of assets.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::Utc;
use rusqlite::{Connection, types::Value};
use serde_json::Value as Json;
use uuid::Uuid;

use super::rows::{self, bind_opt_int, bind_opt_str, bind_opt_ts};
use crate::error::{Error, Result};
use crate::model::{Asset, AssetKind, AssetPatch, AssetQuery, Origin, now};

/// Column list shared by every read; index order matches `asset_from_row`.
pub(crate) const COLS: &str = "id, origin, rel_path, file_name, ext, mime, size_bytes, sha256, \
                    kind, width, height, duration_ms, captured_at, title, description, \
                    rating, is_favorite, source_url, extra, created_at, updated_at, trashed_at";

/// Insert a fully-populated asset.
pub fn insert(conn: &Connection, asset: &Asset) -> Result<()> {
    rows::execute(
        conn,
        &format!(
            "INSERT INTO assets ({COLS}) VALUES \
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)"
        ),
        asset_values(asset),
    )?;
    fts_insert(conn, asset)
}

pub fn get(conn: &Connection, id: Uuid) -> Result<Option<Asset>> {
    rows::query_one(
        conn,
        &format!("SELECT {COLS} FROM assets WHERE id = ?1"),
        vec![rows::uuid(id).into()],
        asset_from_row,
    )
}

/// Number of records (live or trashed) referencing a content hash.
pub fn count_by_sha256(conn: &Connection, sha256: &str) -> Result<u64> {
    Ok(rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE sha256 = ?1",
        vec![sha256.to_string().into()],
    )? as u64)
}

/// Distinct content hashes referenced by any record (live or trashed).
pub fn referenced_shas(conn: &Connection) -> Result<Vec<String>> {
    rows::query_map(
        conn,
        "SELECT DISTINCT sha256 FROM assets WHERE sha256 IS NOT NULL",
        vec![],
        |row| row.get::<_, String>(0).map_err(Error::from),
    )
}

/// Find a live (not trashed) asset with the same content hash, if any.
pub fn find_by_sha256(conn: &Connection, sha256: &str) -> Result<Option<Asset>> {
    rows::query_one(
        conn,
        &format!("SELECT {COLS} FROM assets WHERE sha256 = ?1 AND trashed_at IS NULL"),
        vec![sha256.to_string().into()],
        asset_from_row,
    )
}

// -- full-text search --------------------------------------------------------

/// Materialise assets by id, preserving the input order. Missing ids are
/// skipped; used by search and smart collections to order by rank.
pub fn by_ids(conn: &Connection, ids: &[Uuid]) -> Result<Vec<Asset>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = format!("SELECT {COLS} FROM assets WHERE id IN (");
    let mut args: Vec<Value> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
        args.push(rows::uuid(*id).into());
    }
    sql.push(')');
    let found: HashMap<Uuid, Asset> = rows::query_map(conn, &sql, args, asset_from_row)?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    Ok(ids.iter().filter_map(|id| found.get(id).cloned()).collect())
}

/// Full-text search over the FTS index.
///
/// `text` picks matching ids ranked by BM25; the other dimensions of `q`
/// (kind / collection / tags / favorite / trash) further filter that set, and
/// `q.limit` / `q.offset` page the result. Order is by relevance, not recency.
/// Returns `(total_matching, page)`.
pub fn search(conn: &Connection, text: &str, q: &AssetQuery) -> Result<(u64, Vec<Asset>)> {
    let text = text.trim();
    if text.is_empty() {
        return Ok((0, Vec::new()));
    }
    let fts = fts_query(text);

    // Rank first (cheap: ids only), then apply the compound filters to the
    // ranked subset, preserving order.
    let ranked = rows::query_map(
        conn,
        "SELECT asset_id FROM asset_fts WHERE asset_fts MATCH ?1 \
         ORDER BY bm25(asset_fts)",
        vec![fts.into()],
        |row| rows::req_uuid(row, 0),
    )?;

    // Search already applies the text filter via the FTS index, so drop the
    // LIKE clause from the compound filters.
    let mut filtered = q.clone();
    filtered.text = None;
    let (where_sql, args) = build_where(&filtered);

    // Exclude assets the compound filters reject by intersecting with the
    // ranked id set, then slice.
    let (total, ids) = rank_intersect(conn, &ranked, &where_sql, &args)?;
    let page = page_assets(&ids, q, conn)?;
    Ok((total, page))
}

/// Build the WHERE for the ranked id set: `WHERE <filters> AND id IN (…)`.
fn rank_intersect(
    conn: &Connection,
    ranked: &[Uuid],
    where_sql: &str,
    args: &[Value],
) -> Result<(u64, Vec<Uuid>)> {
    if ranked.is_empty() {
        return Ok((0, Vec::new()));
    }
    let mut sql = String::from("SELECT id FROM assets");
    sql.push(' ');
    sql.push_str(where_sql); // "" or "WHERE conds"
    if !where_sql.is_empty() {
        sql.push_str(" AND ");
    } else {
        sql.push_str(" WHERE ");
    }
    sql.push_str("id IN (");
    let mut all_args = args.to_vec();
    for (i, id) in ranked.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
        all_args.push(rows::uuid(*id).into());
    }
    sql.push(')');

    let live: HashSet<Uuid> = rows::query_map(conn, &sql, all_args, |row| rows::req_uuid(row, 0))?
        .into_iter()
        .collect();

    let filtered: Vec<Uuid> = ranked
        .iter()
        .filter(|id| live.contains(id))
        .copied()
        .collect();
    let total = filtered.len() as u64;
    Ok((total, filtered))
}

/// Slice a full ranked id list per `q.limit`/`q.offset` and materialise.
fn page_assets(ids: &[Uuid], q: &AssetQuery, conn: &Connection) -> Result<Vec<Asset>> {
    let start = q.offset as usize;
    let end = q
        .limit
        .map(|l| start.saturating_add(l as usize))
        .unwrap_or(ids.len());
    let end = end.min(ids.len());
    if start >= ids.len() {
        return Ok(Vec::new());
    }
    by_ids(conn, &ids[start..end])
}

/// Escape user text into an FTS5 query where every whitespace-separated term
/// becomes an ANDed prefix phrase, so operators like `"` `*` `NEAR` are treated
/// as literal characters, never as query syntax.
///
/// - `"sunset beach"` → `"sunset"* "beach"*` : both terms must match (FTS5
///   joins terms with implicit AND).
/// - Each phrase gets a trailing `*`, so `sunset` also matches `sunsets` and
///   `sunsetsky` (prefix match within a token).
pub(crate) fn fts_query(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    cleaned
        .split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

// -- FTS index maintenance ---------------------------------------------------

/// Insert a row into the FTS index mirroring the asset's searchable text
/// (file name, title, description, and the names of all attached tags).
pub(crate) fn fts_insert(conn: &Connection, asset: &Asset) -> Result<()> {
    let tags = tags_for_fts(conn, asset.id)?;
    rows::execute(
        conn,
        "INSERT INTO asset_fts(asset_id, file_name, title, description, tags) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        vec![
            rows::uuid(asset.id).into(),
            asset.file_name.clone().into(),
            nullable_or_empty(asset.title.as_deref()),
            nullable_or_empty(asset.description.as_deref()),
            Value::Text(tags),
        ],
    )?;
    Ok(())
}

/// Comma-joined names of every tag attached to `asset_id` (empty if none).
/// FTS5 indexed columns reject NULL, hence the default.
fn tags_for_fts(conn: &Connection, asset_id: Uuid) -> Result<String> {
    let names: Vec<String> = rows::query_map(
        conn,
        "SELECT t.name FROM tags t \
         JOIN asset_tag at ON at.tag_id = t.id \
         WHERE at.asset_id = ?1 ORDER BY t.name",
        vec![rows::uuid(asset_id).into()],
        |row| row.get::<_, String>(0).map_err(Error::from),
    )?;
    Ok(names.join(", "))
}

/// Rewrite an asset's FTS row from current state (asset row + attached tags).
/// Used after updates to title/description and after tag membership changes.
/// A missing asset row simply leaves the index entry removed.
pub(crate) fn fts_sync(conn: &Connection, asset_id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM asset_fts WHERE asset_id = ?1",
        vec![rows::uuid(asset_id).into()],
    )?;
    if let Some(asset) = get(conn, asset_id)? {
        fts_insert(conn, &asset)?;
    }
    Ok(())
}

/// FTS5 indexed columns reject NULL; map `None` to the empty string.
fn nullable_or_empty(v: Option<&str>) -> Value {
    Value::Text(v.unwrap_or("").to_string())
}

/// List assets matching `query`. Returns `(total_matching, page)`.
pub fn query(conn: &Connection, q: &AssetQuery) -> Result<(u64, Vec<Asset>)> {
    let (where_sql, mut args) = build_where(q);
    let total = rows::query_count(
        conn,
        &format!("SELECT COUNT(*) FROM assets {where_sql}"),
        args.clone(),
    )? as u64;

    let mut sql = format!("SELECT {COLS} FROM assets {where_sql}");
    let order_col = match q.sort {
        crate::model::AssetSort::CreatedAt => "created_at",
        crate::model::AssetSort::Name => "file_name COLLATE NOCASE",
        crate::model::AssetSort::SizeBytes => "size_bytes",
        crate::model::AssetSort::Rating => "rating",
    };
    let dir = if q.sort_desc { "DESC" } else { "ASC" };
    sql.push_str(&format!(" ORDER BY {order_col} {dir}, id ASC"));
    if let Some(limit) = q.limit {
        let limit = limit.min(1_000);
        sql.push_str(" LIMIT ? OFFSET ?");
        args.push(Value::Integer(limit as i64));
        args.push(Value::Integer(q.offset as i64));
    }

    let assets = rows::query_map(conn, &sql, args, asset_from_row)?;
    Ok((total, assets))
}

/// Apply a partial patch. `None` fields leave the column untouched.
pub fn update(conn: &Connection, id: Uuid, patch: &AssetPatch) -> Result<Option<Asset>> {
    if get(conn, id)?.is_none() {
        return Ok(None);
    }

    // Every assignment appends its value to `args`; the placeholder number is
    // one past the current length, so parameter order matches SQL order.
    let mut sets: Vec<String> = Vec::new();
    let mut args: Vec<Value> = Vec::new();

    sets.push(format!("updated_at = ?{}", args.len() + 1));
    args.push(rows::ts(now()).into());

    if let Some(v) = &patch.title {
        sets.push(format!("title = ?{}", args.len() + 1));
        args.push(bind_opt_str(v.as_deref()));
    }
    if let Some(v) = &patch.description {
        sets.push(format!("description = ?{}", args.len() + 1));
        args.push(bind_opt_str(v.as_deref()));
    }
    if let Some(kind) = patch.kind {
        sets.push(format!("kind = ?{}", args.len() + 1));
        args.push(kind_str(kind).into());
    }
    if let Some(rating) = &patch.rating {
        sets.push(format!("rating = ?{}", args.len() + 1));
        args.push(bind_opt_int(rating.map(|r| r as i64)));
    }
    if let Some(fav) = patch.is_favorite {
        sets.push(format!("is_favorite = ?{}", args.len() + 1));
        args.push(Value::Integer(fav as i64));
    }
    if let Some(v) = &patch.source_url {
        sets.push(format!("source_url = ?{}", args.len() + 1));
        args.push(bind_opt_str(v.as_deref()));
    }
    if let Some(extra) = &patch.extra {
        sets.push(format!("extra = ?{}", args.len() + 1));
        args.push(serde_json::to_string(extra)?.into());
    }

    // The WHERE parameter comes after every SET value.
    let where_ix = args.len() + 1;
    let sql = format!(
        "UPDATE assets SET {} WHERE id = ?{where_ix}",
        sets.join(", ")
    );
    args.push(rows::uuid(id).into());
    rows::execute(conn, &sql, args)?;

    // Mirror title/description changes into the search index.
    let updated = get(conn, id)?;
    if let Some(a) = &updated {
        fts_sync(conn, a.id)?;
    }
    Ok(updated)
}

/// Replace the `extra` JSON column (used for visual signature backfill).
pub fn update_extra(
    conn: &Connection,
    id: Uuid,
    extra: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let json = serde_json::to_string(extra)
        .map_err(|e| crate::Error::Db(format!("serialize extra: {e}")))?;
    rows::execute(
        conn,
        "UPDATE assets SET extra = ?1 WHERE id = ?2",
        vec![json.into(), rows::uuid(id).into()],
    )?;
    Ok(())
}

/// Move an asset into (or out of) the trash.
pub fn set_trashed(conn: &Connection, id: Uuid, trashed: bool) -> Result<bool> {
    let changed = rows::execute(
        conn,
        "UPDATE assets SET trashed_at = ?1, updated_at = ?2 WHERE id = ?3",
        vec![
            if trashed {
                bind_opt_ts(Some(Utc::now()))
            } else {
                Value::Null
            },
            rows::ts(Utc::now()).into(),
            rows::uuid(id).into(),
        ],
    )?;
    Ok(changed > 0)
}

/// Permanently delete an asset. Cascades remove membership and tag rows.
pub fn delete(conn: &Connection, id: Uuid) -> Result<()> {
    rows::execute(
        conn,
        "DELETE FROM assets WHERE id = ?1",
        vec![rows::uuid(id).into()],
    )?;
    rows::execute(
        conn,
        "DELETE FROM asset_fts WHERE asset_id = ?1",
        vec![rows::uuid(id).into()],
    )?;
    Ok(())
}

// -- row mapping -------------------------------------------------------------

pub(crate) fn asset_from_row(row: &rusqlite::Row) -> Result<Asset> {
    Ok(Asset {
        id: rows::req_uuid(row, 0)?,
        origin: match rows::req_str(row, 1)?.as_str() {
            "stored" => Origin::Stored,
            "linked" => Origin::Linked,
            other => return Err(Error::Db(format!("bad origin {other}"))),
        },
        rel_path: rows::opt_str(row, 2)?,
        file_name: rows::req_str(row, 3)?,
        ext: rows::req_str(row, 4)?,
        mime: rows::req_str(row, 5)?,
        size_bytes: {
            let v = rows::int(row, 6)?;
            v.max(0) as u64
        },
        sha256: rows::opt_str(row, 7)?,
        kind: parse_kind(&rows::req_str(row, 8)?)?,
        width: rows::opt_int(row, 9)?.map(|v| v as u32),
        height: rows::opt_int(row, 10)?.map(|v| v as u32),
        duration_ms: rows::opt_int(row, 11)?.map(|v| v as u64),
        captured_at: rows::opt_ts(row, 12)?,
        title: rows::opt_str(row, 13)?,
        description: rows::opt_str(row, 14)?,
        rating: rows::opt_int(row, 15)?.map(|v| v as u8),
        is_favorite: rows::boolean(row, 16)?,
        source_url: rows::opt_str(row, 17)?,
        extra: parse_extra(&rows::req_str(row, 18)?)?,
        created_at: rows::req_ts(row, 19)?,
        updated_at: rows::req_ts(row, 20)?,
        trashed_at: rows::opt_ts(row, 21)?,
    })
}

fn asset_values(a: &Asset) -> Vec<Value> {
    vec![
        rows::uuid(a.id).into(),
        match a.origin {
            Origin::Stored => "stored".to_string().into(),
            Origin::Linked => "linked".to_string().into(),
        },
        bind_opt_str(a.rel_path.as_deref()),
        a.file_name.clone().into(),
        a.ext.clone().into(),
        a.mime.clone().into(),
        Value::Integer(a.size_bytes as i64),
        bind_opt_str(a.sha256.as_deref()),
        kind_str(a.kind).into(),
        a.width
            .map(|v| Value::Integer(v as i64))
            .unwrap_or(Value::Null),
        a.height
            .map(|v| Value::Integer(v as i64))
            .unwrap_or(Value::Null),
        a.duration_ms
            .map(|v| Value::Integer(v as i64))
            .unwrap_or(Value::Null),
        bind_opt_ts(a.captured_at),
        bind_opt_str(a.title.as_deref()),
        bind_opt_str(a.description.as_deref()),
        a.rating
            .map(|v| Value::Integer(v as i64))
            .unwrap_or(Value::Null),
        Value::Integer(a.is_favorite as i64),
        bind_opt_str(a.source_url.as_deref()),
        serde_json::to_string(&a.extra)
            .unwrap_or_else(|_| "{}".into())
            .into(),
        rows::ts(a.created_at).into(),
        rows::ts(a.updated_at).into(),
        bind_opt_ts(a.trashed_at),
    ]
}

fn build_where(q: &AssetQuery) -> (String, Vec<Value>) {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Value> = Vec::new();

    if let Some(text) = q.text.as_deref() {
        let text = text.trim();
        if !text.is_empty() {
            // Same searchable surface as the FTS index: file name, title,
            // description, and attached tag names.
            conds.push(
                "(file_name LIKE ? OR title LIKE ? OR description LIKE ? \
                 OR EXISTS (SELECT 1 FROM asset_tag ft \
                            JOIN tags t ON t.id = ft.tag_id \
                            WHERE ft.asset_id = assets.id AND t.name LIKE ?))"
                    .into(),
            );
            let like = format!("%{}%", text.to_lowercase());
            args.push(Value::Text(like.clone()));
            args.push(Value::Text(like.clone()));
            args.push(Value::Text(like.clone()));
            args.push(Value::Text(like));
        }
    }
    if let Some(kind) = q.kind {
        conds.push(format!("kind = ?{}", args.len() + 1));
        args.push(kind_str(kind).into());
    }
    if let Some(cid) = q.collection_id {
        conds.push(format!(
            "EXISTS (SELECT 1 FROM asset_collection ac \
             WHERE ac.asset_id = assets.id AND ac.collection_id = ?{})",
            args.len() + 1
        ));
        args.push(rows::uuid(cid).into());
    }
    if !q.tag_ids.is_empty() {
        // The asset must carry every requested tag.
        for tag in &q.tag_ids {
            let ix = args.len() + 1;
            conds.push(format!(
                "EXISTS (SELECT 1 FROM asset_tag t WHERE t.asset_id = assets.id AND t.tag_id = ?{ix})"
            ));
            args.push(rows::uuid(*tag).into());
        }
    }
    if let Some(fav) = q.is_favorite {
        conds.push(format!("is_favorite = ?{}", args.len() + 1));
        args.push(Value::Integer(fav as i64));
    }
    if q.is_trashed {
        conds.push("trashed_at IS NOT NULL".into());
    } else {
        conds.push("trashed_at IS NULL".into());
    }

    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    (where_sql, args)
}

fn parse_extra(s: &str) -> Result<BTreeMap<String, Json>> {
    match serde_json::from_str(s) {
        Ok(map) => Ok(map),
        Err(_) if s.trim().is_empty() => Ok(BTreeMap::new()),
        Err(e) => Err(Error::Db(format!("bad extra json: {e}"))),
    }
}

fn kind_str(kind: AssetKind) -> String {
    match kind {
        AssetKind::Image => "image",
        AssetKind::Video => "video",
        AssetKind::Audio => "audio",
        AssetKind::Document => "document",
        AssetKind::Archive => "archive",
        AssetKind::Font => "font",
        AssetKind::Other => "other",
    }
    .to_string()
}

fn parse_kind(s: &str) -> Result<AssetKind> {
    Ok(match s {
        "image" => AssetKind::Image,
        "video" => AssetKind::Video,
        "audio" => AssetKind::Audio,
        "document" => AssetKind::Document,
        "archive" => AssetKind::Archive,
        "font" => AssetKind::Font,
        "" | "other" => AssetKind::Other,
        other => return Err(Error::Db(format!("bad kind {other}"))),
    })
}

// ---------------------------------------------------------------------------
// CLIP embeddings (schema v4 `embedding` BLOB column)
//
// All embedding SQL lives here; `media::clip` only does model inference.
// The vector deliberately stays OUT of the `Asset` struct so ordinary list
// queries never drag a half-KB BLOB per row.
// ---------------------------------------------------------------------------

/// Store (or replace) the embedding BLOB of one asset.
pub fn set_embedding(conn: &Connection, asset_id: Uuid, bytes: &[u8]) -> Result<()> {
    rows::execute(
        conn,
        "UPDATE assets SET embedding = ?1 WHERE id = ?2",
        vec![Value::Blob(bytes.to_vec()), rows::uuid(asset_id).into()],
    )?;
    Ok(())
}

/// `true` when the asset already carries an embedding.
pub fn has_embedding(conn: &Connection, asset_id: Uuid) -> Result<bool> {
    let n = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE id = ?1 AND embedding IS NOT NULL",
        vec![rows::uuid(asset_id).into()],
    )?;
    Ok(n > 0)
}

/// Live images that have no embedding yet: `(id, rel_path)`.
pub fn images_missing_embedding(conn: &Connection) -> Result<Vec<(Uuid, Option<String>)>> {
    rows::query_map(
        conn,
        "SELECT id, rel_path FROM assets
         WHERE kind = 'image' AND trashed_at IS NULL AND embedding IS NULL",
        vec![],
        |row| Ok((rows::req_uuid(row, 0)?, rows::opt_str(row, 1)?)),
    )
}

/// Every stored embedding, fetched in chunks of `chunk` rows so a large
/// library does not materialise the whole column at once.
pub fn all_embeddings(conn: &Connection, chunk: usize) -> Result<Vec<(Uuid, Vec<u8>)>> {
    let chunk = chunk.max(1);
    let mut out = Vec::new();
    let mut offset: i64 = 0;
    loop {
        let batch = rows::query_map(
            conn,
            "SELECT id, embedding FROM assets
             WHERE embedding IS NOT NULL AND trashed_at IS NULL
             LIMIT ? OFFSET ?",
            vec![Value::Integer(chunk as i64), Value::Integer(offset)],
            |row| Ok((rows::req_uuid(row, 0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        let n = batch.len() as i64;
        out.extend(batch);
        if n < chunk as i64 {
            return Ok(out);
        }
        offset += n;
    }
}

/// `(embedded_images, total_live_images)` — coverage for the settings page.
pub fn embedding_counts(conn: &Connection) -> Result<(u64, u64)> {
    let total = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE kind = 'image' AND trashed_at IS NULL",
        vec![],
    )? as u64;
    let embedded = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE kind = 'image' AND trashed_at IS NULL
         AND embedding IS NOT NULL",
        vec![],
    )? as u64;
    Ok((embedded, total))
}
