//! Faceted search counts: how many assets carry each value of a categorical
//! dimension, computed against the current filter context.
//!
//! The UI shows these beside the grid so a user can see "PNG (42), JPG (18)"
//! before clicking a filter, rather than guessing from an empty dropdown.
//!
//! All counts come from SQLite, not the Tantivy index. The structured data
//! (kind, ext, rating, …) lives in columns the index does not replicate, and
//! the filtering pipeline already runs here — a `GROUP BY` reuses the same
//! WHERE clause the listing does, so facet counts and row counts agree.
//!
//! Two listing shapes, two counting strategies:
//!
//! - **Set listing** (plain browse, no search): the WHERE clause already
//!   describes the visible rows. One `SELECT … GROUP BY` per dimension counts
//!   them directly.
//!
//! - **Ranked listing** (search results, recent): the visible rows are an
//!   ordered id list. The facet query adds `id IN (…)` to the WHERE clause,
//!   so counts reflect only the assets the ranking returned.

use rusqlite::{Connection, types::Value};
use uuid::Uuid;

use super::assets::{WhereMode, build_where};
use super::rows;
use crate::error::{Error, Result};
use crate::model::AssetQuery;

/// One bucket of a facet: a value and how many assets carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetValue {
    /// The human-readable value (lower-cased for ext, kind name, tag name, …).
    pub value: String,
    /// How many assets in the current filter context carry this value.
    pub count: u64,
}

/// Facet counts across every browsable dimension.
///
/// Each field is a list of `(value, count)` pairs sorted by count descending.
/// Empty when no assets match the current filter, or when the dimension does
/// not apply (e.g. no tags in the library).
#[derive(Debug, Clone, Default)]
pub struct FacetCounts {
    pub kinds: Vec<FacetValue>,
    pub exts: Vec<FacetValue>,
    pub tags: Vec<FacetValue>,
    pub ratings: Vec<FacetValue>,
    pub favorites: Vec<FacetValue>,
    pub usage_statuses: Vec<FacetValue>,
    pub orientations: Vec<FacetValue>,
}

/// Compute facet counts for a set listing: the WHERE clause from `q` describes
/// the visible rows, and each dimension gets one `GROUP BY` over it.
pub fn compute_for_query(conn: &Connection, q: &AssetQuery) -> Result<FacetCounts> {
    let (where_sql, base_args) = build_where(conn, q, WhereMode::Driving)?;
    compute_with_where(conn, &where_sql, &base_args)
}

/// Compute facet counts for a ranked listing: the visible rows are the ids in
/// `ranked`, further narrowed by `q`'s filters.
pub fn compute_for_ranked(
    conn: &Connection,
    ranked: &[Uuid],
    q: &AssetQuery,
) -> Result<FacetCounts> {
    if ranked.is_empty() {
        return Ok(FacetCounts::default());
    }
    // Build a WHERE clause that restricts to the ranked ids AND the filters.
    // The ranked ids go first as a virtual-table join (same pattern as
    // `rank_intersect`), then the regular filters append.
    let (filter_where, filter_args) = build_where(conn, q, WhereMode::Driving)?;
    // Strip the leading "WHERE " from the filter clause so we can combine it
    // with the id restriction.
    let filter_body = filter_where.strip_prefix("WHERE ").unwrap_or(&filter_where);

    // Use json_each to pass the ranked ids as a single parameter, matching the
    // pattern `rank_intersect` uses. This avoids thousands of `?` placeholders.
    let ids_json = serde_json::to_string(&ranked.iter().map(|u| u.to_string()).collect::<Vec<_>>())
        .map_err(|e| Error::Db(format!("facet ids json: {e}")))?;

    let where_sql = if filter_body.is_empty() {
        "WHERE EXISTS (SELECT 1 FROM json_each(?1) WHERE value = assets.id)".to_string()
    } else {
        // Shift the filter placeholders past the ids_json parameter.
        let shifted = shift_placeholders(filter_body, 1)?;
        format!("WHERE EXISTS (SELECT 1 FROM json_each(?1) WHERE value = assets.id) AND {shifted}")
    };

    let mut args = vec![Value::Text(ids_json)];
    args.extend(filter_args);

    compute_with_where(conn, &where_sql, &args)
}

/// The shared counting core: given a WHERE clause and its arguments, run one
/// `GROUP BY` per facet dimension.
fn compute_with_where(
    conn: &Connection,
    where_sql: &str,
    base_args: &[Value],
) -> Result<FacetCounts> {
    let kinds = count_column(conn, "kind", where_sql, base_args)?;
    let exts = count_ext(conn, where_sql, base_args)?;
    let tags = count_tags(conn, where_sql, base_args)?;
    let ratings = count_column(conn, "rating", where_sql, base_args)?;
    let favorites = count_bool_column(conn, "is_favorite", where_sql, base_args)?;
    let usage_statuses = count_column(conn, "usage_status", where_sql, base_args)?;
    let orientations = count_orientation(conn, where_sql, base_args)?;

    Ok(FacetCounts {
        kinds: map_kind_labels(kinds),
        exts,
        tags,
        ratings: map_rating_labels(ratings),
        favorites: map_favorite_labels(favorites),
        usage_statuses: map_usage_status_labels(usage_statuses),
        orientations,
    })
}

// ---- per-dimension counters ------------------------------------------------

/// `SELECT col, COUNT(*) FROM assets {where} GROUP BY col`
fn count_column(
    conn: &Connection,
    column: &str,
    where_sql: &str,
    base_args: &[Value],
) -> Result<Vec<(String, u64)>> {
    let sql = format!("SELECT {column}, COUNT(*) FROM assets {where_sql} GROUP BY {column}");
    let args = base_args.to_vec();
    rows::query_map(conn, &sql, args, |row| {
        let value: String = row.get(0).unwrap_or_default();
        let count: i64 = row.get(1).unwrap_or(0);
        Ok((value, count.max(0) as u64))
    })
}

/// Extension counting uses `LOWER(ext)` to match the filter's comparison,
/// and skips empty extensions.
fn count_ext(conn: &Connection, where_sql: &str, base_args: &[Value]) -> Result<Vec<FacetValue>> {
    let sql = format!(
        "SELECT LOWER(ext), COUNT(*) FROM assets {where_sql} \
         GROUP BY LOWER(ext) HAVING LOWER(ext) != '' AND LOWER(ext) IS NOT NULL \
         ORDER BY COUNT(*) DESC"
    );
    let args = base_args.to_vec();
    let rows = rows::query_map(conn, &sql, args, |row| {
        let value: String = row.get(0).unwrap_or_default();
        let count: i64 = row.get(1).unwrap_or(0);
        Ok(FacetValue {
            value,
            count: count.max(0) as u64,
        })
    })?;
    Ok(rows)
}

/// Tag counting joins through `asset_tag` and `tags`, counting how many
/// assets in the current filter set carry each tag.
fn count_tags(conn: &Connection, where_sql: &str, base_args: &[Value]) -> Result<Vec<FacetValue>> {
    // The WHERE clause references `assets`, so the tag query wraps it as a
    // sub-select: find the assets that pass the filter, then count their tags.
    let sql = format!(
        "SELECT t.name, COUNT(DISTINCT a.id) \
         FROM asset_tag at2 \
         JOIN tags t ON t.id = at2.tag_id \
         JOIN assets a ON a.id = at2.asset_id \
         WHERE a.id IN (SELECT assets.id FROM assets {where_sql}) \
         GROUP BY t.id \
         ORDER BY COUNT(DISTINCT a.id) DESC"
    );
    let args = base_args.to_vec();
    rows::query_map(conn, &sql, args, |row| {
        let name: String = row.get(0).unwrap_or_default();
        let count: i64 = row.get(1).unwrap_or(0);
        Ok(FacetValue {
            value: name,
            count: count.max(0) as u64,
        })
    })
}

/// Boolean column: GROUP BY produces 0/1, mapped to yes/no labels.
fn count_bool_column(
    conn: &Connection,
    column: &str,
    where_sql: &str,
    base_args: &[Value],
) -> Result<Vec<(String, u64)>> {
    count_column(conn, column, where_sql, base_args)
}

/// Orientation is derived from width/height, so a CASE expression computes it.
fn count_orientation(
    conn: &Connection,
    where_sql: &str,
    base_args: &[Value],
) -> Result<Vec<FacetValue>> {
    let sql = format!(
        "SELECT CASE \
           WHEN width IS NULL OR height IS NULL OR width = 0 OR height = 0 THEN 'unknown' \
           WHEN width > height THEN 'landscape' \
           WHEN width < height THEN 'portrait' \
           ELSE 'square' \
         END, COUNT(*) \
         FROM assets {where_sql} \
         GROUP BY 1 \
         ORDER BY COUNT(*) DESC"
    );
    let args = base_args.to_vec();
    rows::query_map(conn, &sql, args, |row| {
        let value: String = row.get(0).unwrap_or_default();
        let count: i64 = row.get(1).unwrap_or(0);
        Ok(FacetValue {
            value,
            count: count.max(0) as u64,
        })
    })
}

// ---- label mapping ---------------------------------------------------------

/// Map raw `kind` column values to their display labels.
fn map_kind_labels(raw: Vec<(String, u64)>) -> Vec<FacetValue> {
    let mut out: Vec<FacetValue> = raw
        .into_iter()
        .filter(|(v, _)| !v.is_empty())
        .map(|(value, count)| {
            let label = match value.as_str() {
                "image" => "image",
                "video" => "video",
                "audio" => "audio",
                "document" => "document",
                "archive" => "archive",
                "font" => "font",
                "model" => "model",
                "other" => "other",
                _ => &value,
            };
            FacetValue {
                value: label.to_string(),
                count,
            }
        })
        .collect();
    out.sort_by_key(|facet| std::cmp::Reverse(facet.count));
    out
}

/// Map raw `rating` column values (stored as integers 0-5, NULL for unrated).
fn map_rating_labels(raw: Vec<(String, u64)>) -> Vec<FacetValue> {
    let mut out: Vec<FacetValue> = raw
        .into_iter()
        .map(|(value, count)| {
            let label = if value.is_empty() {
                "unrated".to_string()
            } else {
                format!("{value}★")
            };
            FacetValue {
                value: label,
                count,
            }
        })
        .collect();
    out.sort_by_key(|facet| std::cmp::Reverse(facet.count));
    out
}

/// Map boolean favorite values.
fn map_favorite_labels(raw: Vec<(String, u64)>) -> Vec<FacetValue> {
    raw.into_iter()
        .map(|(value, count)| {
            let label = if value == "1" { "favorite" } else { "normal" };
            FacetValue {
                value: label.to_string(),
                count,
            }
        })
        .collect()
}

/// Map raw `usage_status` column values.
fn map_usage_status_labels(raw: Vec<(String, u64)>) -> Vec<FacetValue> {
    let mut out: Vec<FacetValue> = raw
        .into_iter()
        .filter(|(v, _)| !v.is_empty())
        .map(|(value, count)| {
            let label = match value.as_str() {
                "unused" => "unused",
                "used" => "used",
                _ => &value,
            };
            FacetValue {
                value: label.to_string(),
                count,
            }
        })
        .collect();
    out.sort_by_key(|facet| std::cmp::Reverse(facet.count));
    out
}

/// Shift `?N` placeholders in a SQL fragment by `offset`, so the fragment can
/// be appended to a parameter list that already has `offset` entries bound.
fn shift_placeholders(sql: &str, offset: usize) -> Result<String> {
    if offset == 0 {
        return Ok(sql.to_string());
    }
    // Walk the string and rewrite every `?N` to `?(N+offset)`. A simple
    // character scan is enough: `?` followed by digits is a placeholder.
    let mut out = String::with_capacity(sql.len() + 16);
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '?' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                let num_str: String = chars[start..end].iter().collect();
                let n: usize = num_str
                    .parse()
                    .map_err(|_| Error::Db(format!("facet: bad placeholder ?{num_str}")))?;
                out.push_str(&format!("?{}", n + offset));
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssetKind, AssetQuery, test_asset};
    use crate::store::Store;
    use crate::store::assets;
    use std::collections::HashMap;

    fn seed_library() -> (Store, Vec<Uuid>) {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();

        let mut ids = Vec::new();
        // 3 PNG images, 2 JPG images, 1 video, 1 font
        for (name, kind, ext) in [
            ("a.png", AssetKind::Image, "png"),
            ("b.png", AssetKind::Image, "png"),
            ("c.png", AssetKind::Image, "png"),
            ("d.jpg", AssetKind::Image, "jpg"),
            ("e.jpg", AssetKind::Image, "jpg"),
            ("f.mp4", AssetKind::Video, "mp4"),
            ("g.ttf", AssetKind::Font, "ttf"),
        ] {
            let mut a = test_asset(name, kind, crate::model::new_id());
            a.ext = ext.to_string();
            assets::insert(conn, &a).unwrap();
            ids.push(a.id);
        }
        (store, ids)
    }

    #[test]
    fn facets_count_by_kind_and_ext() {
        let (store, _) = seed_library();
        let conn = store.conn();
        let q = AssetQuery::default();
        let facets = compute_for_query(conn, &q).unwrap();

        // 5 images, 1 video, 1 font
        let kind_map: HashMap<&str, u64> = facets
            .kinds
            .iter()
            .map(|fv| (fv.value.as_str(), fv.count))
            .collect();
        assert_eq!(kind_map.get("image"), Some(&5));
        assert_eq!(kind_map.get("video"), Some(&1));
        assert_eq!(kind_map.get("font"), Some(&1));

        // 3 png, 2 jpg, 1 mp4, 1 ttf
        let ext_map: HashMap<&str, u64> = facets
            .exts
            .iter()
            .map(|fv| (fv.value.as_str(), fv.count))
            .collect();
        assert_eq!(ext_map.get("png"), Some(&3));
        assert_eq!(ext_map.get("jpg"), Some(&2));
        assert_eq!(ext_map.get("mp4"), Some(&1));
        assert_eq!(ext_map.get("ttf"), Some(&1));
    }

    #[test]
    fn facets_respect_the_active_filters() {
        let (store, _) = seed_library();
        let conn = store.conn();
        // Filter to images only.
        let q = AssetQuery {
            kind: Some(AssetKind::Image),
            ..Default::default()
        };
        let facets = compute_for_query(conn, &q).unwrap();

        // Only images should be counted.
        assert_eq!(facets.kinds.len(), 1);
        assert_eq!(facets.kinds[0].value, "image");
        assert_eq!(facets.kinds[0].count, 5);

        // Extensions should only include image formats.
        let ext_map: HashMap<&str, u64> = facets
            .exts
            .iter()
            .map(|fv| (fv.value.as_str(), fv.count))
            .collect();
        assert_eq!(ext_map.get("png"), Some(&3));
        assert_eq!(ext_map.get("jpg"), Some(&2));
        assert!(!ext_map.contains_key("mp4"));
        assert!(!ext_map.contains_key("ttf"));
    }

    #[test]
    fn ranked_facets_count_only_the_ids_in_the_pool() {
        let (store, ids) = seed_library();
        let conn = store.conn();
        // Only the first 3 ids (all PNGs).
        let subset = ids[..3].to_vec();
        let q = AssetQuery::default();
        let facets = compute_for_ranked(conn, &subset, &q).unwrap();

        let ext_map: HashMap<&str, u64> = facets
            .exts
            .iter()
            .map(|fv| (fv.value.as_str(), fv.count))
            .collect();
        assert_eq!(ext_map.get("png"), Some(&3));
        assert!(!ext_map.contains_key("jpg"));
    }

    #[test]
    fn shift_placeholders_rewrites_parameter_numbers() {
        assert_eq!(shift_placeholders("?1 AND ?2", 3).unwrap(), "?4 AND ?5");
        assert_eq!(shift_placeholders("x = ?1", 0).unwrap(), "x = ?1");
        assert_eq!(
            shift_placeholders("?1 BETWEEN ?2 AND ?3", 10).unwrap(),
            "?11 BETWEEN ?12 AND ?13"
        );
    }
}
