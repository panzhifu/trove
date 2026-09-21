//! Asset store: insert / read / query / patch / lifecycle of assets.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use rusqlite::{Connection, types::Value};
use uuid::Uuid;

use super::rows::{self, bind_opt_int, bind_opt_str, bind_opt_ts};
use crate::error::{Error, Result};
use crate::model::{
    Asset, AssetFacts, AssetKind, AssetPatch, AssetQuery, Orientation, Origin, Page, UsageStatus,
    now,
};

/// Column list shared by every read; index order matches `asset_from_row`.
pub(crate) const COLS: &str = "id, origin, rel_path, file_name, ext, mime, size_bytes, content_hash, \
                    kind, width, height, duration_ms, captured_at, title, description, \
                    rating, is_favorite, source_url, extra, created_at, updated_at, trashed_at, \
                    usage_status, commercial_use";

/// Insert a fully-populated asset.
pub fn insert(conn: &Connection, asset: &Asset) -> Result<()> {
    rows::execute(
        conn,
        &format!(
            "INSERT INTO assets ({COLS}) VALUES \
             (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)"
        ),
        asset_values(asset),
    )?;
    Ok(())
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
pub fn count_by_content_hash(conn: &Connection, content_hash: &str) -> Result<u64> {
    Ok(rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE content_hash = ?1",
        vec![content_hash.to_string().into()],
    )? as u64)
}

/// Content columns of a *linked* asset after an in-place edit that wrote the
/// re-encoded result back to the file it links to: hash, size and geometry
/// move; the link columns do not (`rel_path` stays NULL, the origin stays
/// linked, the source path is untouched).
pub fn set_linked_media_columns(
    conn: &Connection,
    id: Uuid,
    content_hash: &str,
    size_bytes: u64,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<()> {
    rows::execute(
        conn,
        "UPDATE assets SET content_hash = ?1, size_bytes = ?2, width = ?3, height = ?4 \
         WHERE id = ?5",
        vec![
            Value::Text(content_hash.to_string()),
            Value::Integer(size_bytes as i64),
            rows::bind_opt_int(width.map(i64::from)),
            rows::bind_opt_int(height.map(i64::from)),
            rows::uuid(id).into(),
        ],
    )?;
    Ok(())
}

/// Distinct content hashes referenced by any record (live or trashed).
///
/// Used to decide what a blob or thumbnail on disk is still needed for, and
/// by the dedup pre-check ([`crate::media::precheck`]) to answer "does the
/// library already hold this content?" from memory instead of a query per
/// candidate file. Trashed rows count: their content still owns the files on
/// disk, and whether to re-import it is the commit's decision.
pub fn referenced_hashes(conn: &Connection) -> Result<Vec<String>> {
    rows::query_map(
        conn,
        "SELECT DISTINCT content_hash FROM assets WHERE content_hash IS NOT NULL",
        vec![],
        |row| row.get::<_, String>(0).map_err(Error::from),
    )
}

/// Find a live (not trashed) asset with the same content hash, if any.
pub fn find_by_content_hash(conn: &Connection, content_hash: &str) -> Result<Option<Asset>> {
    rows::query_one(
        conn,
        &format!("SELECT {COLS} FROM assets WHERE content_hash = ?1 AND trashed_at IS NULL"),
        vec![content_hash.to_string().into()],
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

/// Intersect a relevance-ranked id list (from the Tantivy index) with the
/// structured filters of `q`, preserving the rank order.
///
/// The candidate list drives the query and the filters only reject rows; see
/// [`WhereMode`] for why that has to be forced rather than left to the planner.
/// The query takes the candidate ids as an `IN (…)` list, whose entries are
/// served straight from the primary-key index, and returns the ids that
/// survived, in rank order.
pub(crate) fn rank_intersect(
    conn: &Connection,
    ranked: &[Uuid],
    q: &AssetQuery,
) -> Result<(u64, Vec<Uuid>)> {
    if ranked.is_empty() {
        return Ok((0, Vec::new()));
    }
    let (where_sql, args) = build_where(conn, q, WhereMode::Rejecting)?;

    let mut sql = String::from("SELECT id FROM assets");
    if !where_sql.is_empty() {
        sql.push(' ');
        sql.push_str(&where_sql);
        sql.push_str(" AND ");
    } else {
        sql.push_str(" WHERE ");
    }
    sql.push_str("id IN (");
    let mut all_args = args;
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
pub(crate) fn page_assets(ids: &[Uuid], q: &AssetQuery, conn: &Connection) -> Result<Vec<Asset>> {
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

pub(crate) fn tags_for_index(conn: &Connection, asset_id: Uuid) -> Result<String> {
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

/// List assets matching the structured filters of `q`.
///
/// Free text is not part of `q`; see [`AssetQuery`] and `build_where`.
pub fn query(conn: &Connection, q: &AssetQuery) -> Result<Page<Asset>> {
    let (where_sql, args) = build_where(conn, q, WhereMode::Driving)?;
    let total = rows::query_count(
        conn,
        &format!("SELECT COUNT(*) FROM assets {where_sql}"),
        args.clone(),
    )? as u64;
    let assets = query_items(conn, q, where_sql, args)?;
    Ok(Page::new(total, assets))
}

/// Like [`query`], but skips the exact COUNT: the returned total is a lower
/// bound (this page's item count). Rapid refreshes use it and overlay a
/// cached exact total, keeping the COUNT off the hot path.
pub fn query_without_count(conn: &Connection, q: &AssetQuery) -> Result<Page<Asset>> {
    let (where_sql, args) = build_where(conn, q, WhereMode::Driving)?;
    let assets = query_items(conn, q, where_sql, args)?;
    Ok(Page::new(assets.len() as u64, assets))
}

/// The paged item fetch shared by [`query`] and [`query_without_count`]:
/// order + limit are appended to `args`, so this must own them.
fn query_items(
    conn: &Connection,
    q: &AssetQuery,
    where_sql: String,
    mut args: Vec<Value>,
) -> Result<Vec<Asset>> {
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

    rows::query_map(conn, &sql, args, asset_from_row)
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
    if let Some(status) = patch.usage_status {
        sets.push(format!("usage_status = ?{}", args.len() + 1));
        args.push(usage_status_str(status).into());
    }
    if let Some(v) = patch.commercial_use {
        sets.push(format!("commercial_use = ?{}", args.len() + 1));
        args.push(v.map(|b| Value::Integer(b as i64)).unwrap_or(Value::Null));
    }
    if let Some(facts) = &patch.facts {
        sets.push(format!("extra = ?{}", args.len() + 1));
        args.push(serde_json::to_string(facts)?.into());
    }

    // The WHERE parameter comes after every SET value.
    let where_ix = args.len() + 1;
    let sql = format!(
        "UPDATE assets SET {} WHERE id = ?{where_ix}",
        sets.join(", ")
    );
    args.push(rows::uuid(id).into());
    rows::execute(conn, &sql, args)?;
    get(conn, id)
}

/// Replace the `extra` JSON column (used for visual signature backfill).
pub fn update_facts(conn: &Connection, id: Uuid, facts: &AssetFacts) -> Result<()> {
    let json = serde_json::to_string(facts)
        .map_err(|e| crate::Error::Db(format!("serialize extra: {e}")))?;
    rows::execute(
        conn,
        "UPDATE assets SET extra = ?1 WHERE id = ?2",
        vec![json.into(), rows::uuid(id).into()],
    )?;
    Ok(())
}

/// Overwrite an asset's relative blob path (used to link a placeholder
/// record from a metadata restore to a freshly staged blob).
pub fn set_rel_path(conn: &Connection, id: Uuid, rel_path: &str) -> Result<()> {
    rows::execute(
        conn,
        "UPDATE assets SET rel_path = ?1 WHERE id = ?2",
        vec![Value::Text(rel_path.to_string()), rows::uuid(id).into()],
    )?;
    Ok(())
}

/// Point an asset at a new content-addressed blob after an in-place edit:
/// new hash, path, size and dimensions, all in one statement. Identity
/// columns (title, tags, collections, timestamps) are untouched — an edit
/// rewrites what the asset *is*, not how it is organized.
pub fn set_media_columns(
    conn: &Connection,
    id: Uuid,
    content_hash: &str,
    rel_path: &str,
    size_bytes: u64,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<()> {
    rows::execute(
        conn,
        "UPDATE assets SET content_hash = ?1, rel_path = ?2, size_bytes = ?3, width = ?4, height = ?5 \
         WHERE id = ?6",
        vec![
            Value::Text(content_hash.to_string()),
            Value::Text(rel_path.to_string()),
            Value::Integer(size_bytes as i64),
            rows::bind_opt_int(width.map(i64::from)),
            rows::bind_opt_int(height.map(i64::from)),
            rows::uuid(id).into(),
        ],
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
    // The assets_delete trigger enqueues the doc for removal from the index.
    Ok(())
}

// -- duplicates ----------------------------------------------------------------

/// Perceptual-hash hamming distance below which two images count as
/// duplicates (of 64 bits; 8 ≈ ≥87% similar).
const DUPLICATE_PHASH_DISTANCE: u32 = 8;

/// A cluster of live images that look the same. Exact content duplicates
/// cannot occur among live assets — the importer deduplicates by content hash at
/// the record level — so a "duplicate" here is a re-encoded/resized variant
/// with a different hash but the same picture.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    /// Newest import first; the duplicate finder keeps one of these.
    pub assets: Vec<Asset>,
}

/// Group live image assets into near-duplicate clusters.
///
/// Needs visual signatures (the `visual_phash` fact, computed in background
/// after import and backfillable via maintenance); unsigned assets are
/// ignored. Clusters are returned largest first.
pub fn duplicate_groups(conn: &Connection) -> Result<Vec<DuplicateGroup>> {
    use crate::media::search::PHash;

    let rows_vec = rows::query_map(
        conn,
        &format!(
            "SELECT {COLS} FROM assets \
             WHERE kind = 'image' AND trashed_at IS NULL \
             ORDER BY created_at DESC, id ASC"
        ),
        vec![],
        asset_from_row,
    )?;

    // Only assets with a usable hash participate.
    let mut items: Vec<(Asset, PHash)> = Vec::new();
    for asset in rows_vec {
        let phash = asset
            .facts
            .visual
            .visual_phash
            .as_deref()
            .map(PHash::from_hex)
            .unwrap_or(PHash(0));
        if phash != PHash(0) {
            items.push((asset, phash));
        }
    }

    // Union-find over matching pairs. The pair scan is O(n²) hamming checks —
    // trivial for the libraries this desktop app targets.
    let n = items.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for a in 0..n {
        for b in (a + 1)..n {
            if items[a].1.hamming(items[b].1) <= DUPLICATE_PHASH_DISTANCE {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                if ra != rb {
                    parent[rb.max(ra)] = rb.min(ra);
                }
            }
        }
    }

    let mut clusters: std::collections::HashMap<usize, Vec<Asset>> =
        std::collections::HashMap::new();
    for (i, (asset, _)) in items.into_iter().enumerate() {
        clusters
            .entry(find(&mut parent, i))
            .or_default()
            .push(asset);
    }
    let mut groups: Vec<DuplicateGroup> = clusters
        .into_values()
        .filter(|members| members.len() > 1)
        .map(|assets| DuplicateGroup { assets })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.assets.len()));
    Ok(groups)
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
        content_hash: rows::opt_str(row, 7)?,
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
        facts: parse_facts(&rows::req_str(row, 18)?)?,
        created_at: rows::req_ts(row, 19)?,
        updated_at: rows::req_ts(row, 20)?,
        trashed_at: rows::opt_ts(row, 21)?,
        usage_status: parse_usage_status(&rows::req_str(row, 22)?)?,
        commercial_use: rows::opt_int(row, 23)?.map(|v| v != 0),
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
        bind_opt_str(a.content_hash.as_deref()),
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
        serde_json::to_string(&a.facts)
            .unwrap_or_else(|_| "{}".into())
            .into(),
        rows::ts(a.created_at).into(),
        rows::ts(a.updated_at).into(),
        bind_opt_ts(a.trashed_at),
        usage_status_str(a.usage_status).into(),
        a.commercial_use
            .map(|b| Value::Integer(b as i64))
            .unwrap_or(Value::Null),
    ]
}

/// Whether the column comparisons in a [`build_where`] clause may drive the
/// query through an index.
///
/// The two callers want opposite things here. The listing paths *are* their
/// filters, so `idx_assets_kind` driving a `kind = 'image'` listing is exactly
/// right. [`rank_intersect`] is the opposite case: it already holds an exact,
/// ordered candidate list, so the filters may only reject rows. Left to itself
/// the planner happily drives off a filter index instead — and the one every
/// search carries, `trashed_at IS NULL`, matches every live row, so instead of
/// 2000 index probes the intersection degenerates into a full scan. Measured on
/// a 100k library: 21 ms for 97 candidates and 36 ms for 2000, against 0.1 ms
/// and 5 ms once the id list drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WhereMode {
    /// Plain comparisons; the planner may pick any index, including one built
    /// for a filter column.
    Driving,
    /// Every indexable comparison is wrapped in SQLite's unary `+`, a
    /// documented no-op that makes the term ineligible as an index source. The
    /// semantics are unchanged — `+trashed_at IS NULL` still asks whether the
    /// column is null, and `+x = ?` still matches exactly what `x = ?` did.
    Rejecting,
}

impl WhereMode {
    /// The prefix that suppresses index use for one term, if this mode wants it.
    fn prefix(self) -> &'static str {
        match self {
            WhereMode::Driving => "",
            WhereMode::Rejecting => "+",
        }
    }
}

/// Build the `WHERE` clause for the structured filters of `q`.
///
/// Free text is *not* handled here. It is resolved by the Tantivy index into a
/// relevance-ranked id list, which callers intersect with this clause through
/// [`rank_intersect`]; see `AssetQuery`'s doc comment for why there is no
/// `LIKE` fallback. `mode` decides whether the comparisons may drive the query
/// through an index — see [`WhereMode`].
///
/// Conditions that are not plain column comparisons (`EXISTS (…)` subqueries
/// correlated on `assets.id`, `json_extract`, the orientation and aspect-ratio
/// `CASE`s, the `LOWER(ext)` comparison) are never index sources, so they need
/// no prefix.
pub(super) fn build_where(
    conn: &Connection,
    q: &AssetQuery,
    mode: WhereMode,
) -> Result<(String, Vec<Value>)> {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    let ni = mode.prefix();

    if let Some(kind) = q.kind {
        conds.push(format!("{ni}kind = ?{}", args.len() + 1));
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
        // The asset must carry every requested tag; a tag implicitly
        // includes its whole subtree (hierarchical tags).
        for tag in &q.tag_ids {
            let subtree = crate::store::tags::subtree_ids(conn, *tag)?;
            let (in_sql, mut in_args) = id_list("t.tag_id", &subtree, args.len());
            conds.push(format!(
                "EXISTS (SELECT 1 FROM asset_tag t WHERE t.asset_id = assets.id AND {in_sql})"
            ));
            args.append(&mut in_args);
        }
    }
    if let Some(fav) = q.is_favorite {
        conds.push(format!("{ni}is_favorite = ?{}", args.len() + 1));
        args.push(Value::Integer(fav as i64));
    }
    if let Some(prefix) = &q.source_path_prefix {
        conds.push(format!(
            "json_extract(assets.extra, '$.source_path') LIKE ?{} ESCAPE '\\'",
            args.len() + 1
        ));
        // Escape LIKE metacharacters so path separators and underscores in
        // real file names match literally.
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        args.push(Value::Text(format!("{escaped}%")));
    }
    if let Some(status) = q.usage_status {
        conds.push(format!("{ni}usage_status = ?{}", args.len() + 1));
        args.push(usage_status_str(status).into());
    }
    if let Some(clearance) = q.commercial_use {
        conds.push(format!("{ni}commercial_use = ?{}", args.len() + 1));
        args.push(Value::Integer(clearance as i64));
    }
    if let Some(orientation) = q.orientation {
        conds.push(format!(
            "CASE \
             WHEN width IS NULL OR height IS NULL OR width = 0 OR height = 0 THEN '' \
             WHEN width > height THEN 'landscape' \
             WHEN width < height THEN 'portrait' \
             ELSE 'square' END = ?{}",
            args.len() + 1
        ));
        args.push(
            match orientation {
                Orientation::Landscape => "landscape",
                Orientation::Portrait => "portrait",
                Orientation::Square => "square",
            }
            .to_string()
            .into(),
        );
    }
    if let Some(min_rating) = q.min_rating {
        // Unrated assets (NULL) fail the comparison naturally.
        conds.push(format!("{ni}rating >= ?{}", args.len() + 1));
        args.push(Value::Integer(min_rating as i64));
    }
    if let Some(aspect) = q.aspect {
        // Ratio band from the media preset. The CASE maps rows without
        // usable dimensions to -1.0, which falls outside every preset's
        // band, so they match nothing — same contract as the orientation
        // CASE above. `width * 1.0 / height` because SQLite divides two
        // INTEGERs as integers.
        let (lo, hi) = aspect.ratio_range();
        conds.push(format!(
            "CASE \
             WHEN width IS NULL OR height IS NULL OR width = 0 OR height = 0 THEN -1.0 \
             ELSE width * 1.0 / height END BETWEEN ?{} AND ?{}",
            args.len() + 1,
            args.len() + 2
        ));
        args.push(Value::Real(lo as f64));
        args.push(Value::Real(hi as f64));
    }
    if let Some(ext) = &q.ext {
        conds.push(format!("LOWER(ext) = LOWER(?{})", args.len() + 1));
        args.push(Value::Text(ext.clone()));
    }
    // Unconditional, so `where_sql` is never empty.
    if q.is_trashed {
        conds.push(format!("{ni}trashed_at IS NOT NULL"));
    } else {
        conds.push(format!("{ni}trashed_at IS NULL"));
    }

    // The placeholders are hand-numbered, so the clause has to end up using
    // exactly as many as it binds. Every condition bumps `args` by the same
    // count it writes, and a mismatch means one of them numbered itself wrong:
    // `assets::query` would then fail at bind time with "Wrong number of
    // parameters", or worse, bind a value to the wrong `?N`.
    debug_assert_eq!(
        highest_placeholder(&conds),
        args.len(),
        "clause binds {} values but numbers {} placeholders: {}",
        args.len(),
        highest_placeholder(&conds),
        conds.join(" AND ")
    );

    let where_sql = format!("WHERE {}", conds.join(" AND "));
    Ok((where_sql, args))
}

/// Distinct *direct* parent folders that live assets were imported from,
/// each with its live-asset count, sorted by path. No ancestor walk: the
/// panel shows one flat row per folder a file was actually dropped into,
/// nothing else.
pub fn source_folders(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let paths: Vec<String> = rows::query_map(
        conn,
        "SELECT json_extract(extra, '$.source_path') FROM assets \
         WHERE trashed_at IS NULL AND json_extract(extra, '$.source_path') IS NOT NULL",
        vec![],
        |row| row.get::<_, String>(0).map_err(Error::from),
    )?;
    // Count in place rather than through `entry(…).or_default()`: the parent
    // path is only materialised when it is new, so a 100k-row library pays for
    // its ~50 folders instead of one `String` per asset.
    let mut counts: std::collections::BTreeMap<String, u64> = Default::default();
    for path in paths {
        let Some(dir) = std::path::Path::new(&path).parent() else {
            continue;
        };
        let dir = dir.to_string_lossy();
        match counts.get_mut(dir.as_ref()) {
            Some(count) => *count += 1,
            None => {
                counts.insert(dir.into_owned(), 1);
            }
        }
    }
    Ok(counts.into_iter().collect())
}

/// `col IN (?, ?, …)` over a uuid list (each id one parameter), with the
/// placeholders numbered from `first` — the number of parameters the clause has
/// already used.
///
/// Numbering matters: every other condition numbers itself `?{args.len() + 1}`,
/// so a list that started again at `?1` collided with whatever came before it
/// (`kind = ?1` plus a tag list rendered two `?1`s, which is one SQLite
/// parameter but two bound values → "Wrong number of parameters"), and a second
/// tag list collided with the first.
///
/// An empty list renders `IN (NULL)`, which matches nothing. That is the honest
/// answer for, say, a tag whose row has since been deleted, and unlike the
/// naive `IN ()` it is valid SQL.
fn id_list(col: &str, ids: &[Uuid], first: usize) -> (String, Vec<Value>) {
    if ids.is_empty() {
        return (format!("{col} IN (NULL)"), Vec::new());
    }
    let placeholders: Vec<String> = (1..=ids.len())
        .map(|ix| format!("?{}", first + ix))
        .collect();
    (
        format!("{col} IN ({})", placeholders.join(",")),
        ids.iter().map(|id| rows::uuid(*id).into()).collect(),
    )
}

/// The highest `?N` index appearing in a condition list, or 0 when there is
/// none. Used by the guard below to check the hand-numbered placeholders.
fn highest_placeholder(conds: &[String]) -> usize {
    let mut highest = 0;
    for cond in conds {
        let bytes = cond.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'?' {
                i += 1;
                continue;
            }
            let digits = cond[i + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if let Ok(n) = digits.parse::<usize>() {
                highest = highest.max(n);
                i += 1 + digits.len();
            } else {
                i += 1;
            }
        }
    }
    highest
}

fn parse_facts(s: &str) -> Result<AssetFacts> {
    match serde_json::from_str(s) {
        Ok(facts) => Ok(facts),
        Err(_) if s.trim().is_empty() => Ok(AssetFacts::default()),
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
        AssetKind::Model => "model",
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
        "model" => AssetKind::Model,
        "" | "other" => AssetKind::Other,
        other => return Err(Error::Db(format!("bad kind {other}"))),
    })
}

/// Distinct file extensions of live assets, lowercased and sorted. Powers
/// the workspace format filter.
pub fn distinct_exts(conn: &Connection) -> Result<Vec<String>> {
    // `ext` stays bare in the query so `idx_assets_ext` can answer it —
    // including the ordering. Wrapping it (`DISTINCT LOWER(ext) … ORDER BY
    // LOWER(ext)`) makes the expression unindexable and turns a 4 µs index
    // walk into a 32 ms scan of every live row, on a query the workspace
    // toolbar runs once per frame. The case folding and de-duplication happen
    // here instead; `to_ascii_lowercase` matches SQLite's `LOWER`, which is
    // ASCII-only.
    let raw: Vec<String> = rows::query_map(
        conn,
        "SELECT DISTINCT ext FROM assets \
         WHERE trashed_at IS NULL AND ext != '' ORDER BY ext",
        vec![],
        |row| row.get::<_, String>(0).map_err(Error::from),
    )?;
    let mut exts: Vec<String> = raw.iter().map(|e| e.to_ascii_lowercase()).collect();
    exts.sort_unstable();
    exts.dedup();
    Ok(exts)
}

fn usage_status_str(status: UsageStatus) -> String {
    match status {
        UsageStatus::Unused => "unused",
        UsageStatus::Used => "used",
    }
    .to_string()
}

fn parse_usage_status(s: &str) -> Result<UsageStatus> {
    Ok(match s {
        "unused" => UsageStatus::Unused,
        "used" => UsageStatus::Used,
        other => return Err(Error::Db(format!("bad usage status {other}"))),
    })
}

/// The collect-import dedup key of a candidate file: its file name and size.
///
/// Loose on purpose. A different file that happens to share both is skipped
/// once, and the user can re-import it by hand — keying on the content hash
/// instead would be the re-read this exists to avoid. `None` when the file
/// cannot be stat'ed at all, which leaves the decision to the importer (it
/// reports such a file as a skip in due course).
pub fn known_key(path: &std::path::Path) -> Option<(String, u64)> {
    let name = path.file_name()?.to_str()?;
    let size = std::fs::metadata(path).ok()?.len();
    Some((name.to_string(), size))
}

/// Every key a *live* record holds, in one scan of `assets`.
///
/// Trashed rows are excluded, and that is the same rule the commit side
/// dedupes by ([`find_by_content_hash`] ignores them too): a file whose only
/// record is in the trash is a file the user can import again, and skipping
/// it here would leave no way back short of restoring the record.
///
/// A database that cannot answer yields an empty set, which reads as "nothing
/// is known yet" — the caller's next move is to offer the files to the
/// importer, which checks again. That is the safe direction to fail in.
pub fn known_keys(conn: &Connection) -> HashSet<(String, u64)> {
    let Ok(mut stmt) =
        conn.prepare("SELECT file_name, size_bytes FROM assets WHERE trashed_at IS NULL")
    else {
        return HashSet::new();
    };
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    });
    let mut known = HashSet::new();
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            known.insert(row);
        }
    }
    known
}

/// The subset of `paths` the library does not hold yet, in the order given.
///
/// The same rule the collect import skips on ([`known_key`]), asked *ahead* of
/// the job. Worth asking because the collect inbox keeps its files — they are
/// linked, not copied — so a wake-up over that directory is usually a
/// directory whose entire contents are already assets, and a job would report
/// nothing after paying for a scan and a progress notice.
pub fn unimported_paths(
    conn: &Connection,
    paths: &[std::path::PathBuf],
) -> Vec<std::path::PathBuf> {
    if paths.is_empty() {
        return Vec::new();
    }
    let known = known_keys(conn);
    paths
        .iter()
        .filter(|path| match known_key(path) {
            Some(key) => !known.contains(&key),
            None => true,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssetKind, test_asset};
    use crate::store::Store;
    use uuid::Uuid;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trove-known-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_the_library_already_holds_is_not_offered_again() {
        let store = Store::in_memory().unwrap();
        let dir = temp_dir("known");
        let held = dir.join("shot.png");
        std::fs::write(&held, b"0123456789").unwrap();
        let fresh = dir.join("new.png");
        std::fs::write(&fresh, b"xy").unwrap();

        let mut asset = test_asset("shot.png", AssetKind::Image, Uuid::new_v4());
        asset.size_bytes = 10;
        insert(store.conn(), &asset).unwrap();

        let unimported = unimported_paths(store.conn(), &[held.clone(), fresh.clone()]);
        assert_eq!(unimported, vec![fresh]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The key is name *and* size: a different file with the same name is a
    /// different asset, and dropping it would lose an import.
    #[test]
    fn the_same_name_with_a_different_size_is_still_new() {
        let store = Store::in_memory().unwrap();
        let dir = temp_dir("size");
        let other = dir.join("shot.png");
        std::fs::write(&other, b"a longer file than the one on record").unwrap();

        let mut asset = test_asset("shot.png", AssetKind::Image, Uuid::new_v4());
        asset.size_bytes = 10;
        insert(store.conn(), &asset).unwrap();

        assert_eq!(
            unimported_paths(store.conn(), std::slice::from_ref(&other)),
            vec![other]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_waiting_list_asks_the_database_nothing() {
        let store = Store::in_memory().unwrap();
        assert!(unimported_paths(store.conn(), &[]).is_empty());
    }

    /// A file that cannot be stat'ed stays a candidate: the importer is the
    /// one that reports it, and dropping it here would hide the reason.
    #[test]
    fn a_missing_file_stays_a_candidate() {
        let store = Store::in_memory().unwrap();
        let gone = std::path::PathBuf::from("/nonexistent/trove/shot.png");
        assert_eq!(
            unimported_paths(store.conn(), std::slice::from_ref(&gone)),
            vec![gone]
        );
    }
}
