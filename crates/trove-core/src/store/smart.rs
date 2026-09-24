//! Smart-collection condition tree: compile a [`SmartNode`] tree into a SQL
//! `WHERE` fragment, and evaluate it live against the library.

use rusqlite::types::Value;
use serde_json::Value as Json;
use uuid::Uuid;

use super::rows;

/// How many text-condition candidates one Tantivy lookup may contribute to
/// a smart rule (they become an `id IN (json_each)` narrowing).
const TEXT_CANDIDATE_CAP: usize = 500;
use crate::error::{Error, Result};
use crate::model::{AssetKind, AssetQuery, Page, SmartCompare, SmartField, SmartNode};

/// Deserialize a stored JSON condition tree into a [`SmartNode`].
pub fn node_from_json(json: &Json) -> Result<SmartNode> {
    serde_json::from_value(json.clone())
        .map_err(|e| Error::Validation(format!("invalid condition tree: {e}")))
}

/// Parse and compile a serialized condition tree, checking it is runnable
/// against the current schema. Creation entry points (facade, rules dialog)
/// call this instead of the model layer, which carries no storage concerns.
pub fn validate_json(query: &Json) -> Result<()> {
    compile(None, None, &node_from_json(query)?).map(|_| ())
}

/// Compile a condition tree into a boolean `WHERE` fragment (no leading
/// `WHERE`) and its positional parameters. Used both at save time (validation)
/// and at query time (evaluation).
/// `conn = None` compiles a validation-only shape (tag matches fall back to
/// name equality); query evaluation passes the connection so tag matches
/// can expand to the whole subtree.
pub fn compile(
    conn: Option<&rusqlite::Connection>,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
) -> Result<(String, Vec<Value>)> {
    match node {
        SmartNode::And { children } => join(conn, text, " AND ", children),
        SmartNode::Or { children } => join(conn, text, " OR ", children),
        SmartNode::Match { field, op, value } => compile_match(conn, text, *field, *op, value),
    }
}

fn join(
    conn: Option<&rusqlite::Connection>,
    text: Option<&crate::search::TextIndex>,
    sep: &str,
    children: &[SmartNode],
) -> Result<(String, Vec<Value>)> {
    if children.is_empty() {
        return Err(Error::Validation("condition tree must not be empty".into()));
    }
    let mut parts = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    for child in children {
        let (sql, mut child_args) = compile(conn, text, child)?;
        parts.push(format!("({sql})"));
        args.append(&mut child_args);
    }
    Ok((parts.join(sep), args))
}

fn compile_match(
    conn: Option<&rusqlite::Connection>,
    text: Option<&crate::search::TextIndex>,
    field: SmartField,
    op: SmartCompare,
    value: &Json,
) -> Result<(String, Vec<Value>)> {
    match field {
        SmartField::Kind => {
            require_eq_ne(op)?;
            let kind: AssetKind = serde_json::from_value(value.clone())
                .map_err(|_| Error::Validation("kind must be a valid asset kind".into()))?;
            Ok((
                format!("assets.kind {} ?", op_sql(op)),
                vec![Value::from(kind_sql(kind).to_string())],
            ))
        }
        SmartField::IsFavorite => {
            require_eq_ne(op)?;
            let b = value
                .as_bool()
                .ok_or_else(|| Error::Validation("is_favorite must be a boolean".into()))?;
            Ok((
                format!("assets.is_favorite {} ?", op_sql(op)),
                vec![Value::Integer(b as i64)],
            ))
        }
        SmartField::Rating => {
            let n = number_value(value, "rating")?;
            Ok((
                format!("assets.rating {} ?", op_sql(op)),
                vec![Value::Integer(n)],
            ))
        }
        SmartField::SizeBytes => {
            let n = number_value(value, "size_bytes")?;
            Ok((
                format!("assets.size_bytes {} ?", op_sql(op)),
                vec![Value::Integer(n)],
            ))
        }
        SmartField::Extension => {
            require_eq_ne(op)?;
            let s = string_value(value, "extension")?;
            Ok((format!("assets.ext {} ?", op_sql(op)), vec![s.into()]))
        }
        SmartField::Tag => {
            let s = string_value(value, "tag")?;
            let operator = if op == SmartCompare::Eq { "" } else { "NOT " };
            // Hierarchical: matching a tag includes its whole subtree. With
            // no connection (validation) fall back to name equality.
            let ids: Vec<Uuid> = match conn {
                Some(conn) => super::tags::get_by_name(conn, &s)?
                    .and_then(|t| super::tags::subtree_ids(conn, t.id).ok())
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            if ids.is_empty() {
                Ok((
                    format!(
                        "{operator}EXISTS (SELECT 1 FROM asset_tag at JOIN tags t ON t.id = at.tag_id \
                         WHERE at.asset_id = assets.id AND t.name = ? COLLATE NOCASE)"
                    ),
                    vec![s.into()],
                ))
            } else {
                // Anonymous `?` placeholders: SQLite numbers them after the
                // largest explicit index compiled so far, matching the order
                // the args are appended by `join`.
                let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let args: Vec<Value> = ids
                    .into_iter()
                    .map(|id| Value::Text(id.to_string()))
                    .collect();
                Ok((
                    format!(
                        "{operator}EXISTS (SELECT 1 FROM asset_tag at \
                         WHERE at.asset_id = assets.id AND at.tag_id IN ({placeholders}))"
                    ),
                    args,
                ))
            }
        }
        SmartField::Text => {
            require_eq_ne(op)?;
            let s = string_value(value, "text")?;
            // Candidates come from the Tantivy index (words, typo-tolerant
            // fuzzy, gram substrings, pinyin); the SQL fragment narrows to
            // those ids. Without an index (compile-time validation) the
            // candidate list is empty but the fragment still compiles.
            let candidates = match text {
                Some(idx) => idx.search(&s, TEXT_CANDIDATE_CAP)?,
                None => Vec::new(),
            };
            let json = serde_json::to_string(&candidates)
                .map_err(|e| Error::Validation(format!("text candidates: {e}")))?;
            // `contains` matches the ids that hit the query, `not contains`
            // everything else. The empty candidate set is correct for both
            // without special-casing: the subquery returns no rows, so
            // `IN` is false and `NOT IN` true for every asset.
            let verb = if op == SmartCompare::Eq {
                "IN"
            } else {
                "NOT IN"
            };
            Ok((
                format!("assets.id {verb} (SELECT value FROM json_each(?))"),
                vec![Value::from(json)],
            ))
        }
        SmartField::Color => {
            require_eq_ne(op)?;
            let s = normalize_color(&string_value(value, "color")?)?;
            // Dominant color is mined into the `extra` JSON as lowercase hex.
            Ok((
                format!(
                    "lower(json_extract(assets.extra, '$.dominant_color')) {} ?",
                    op_sql(op)
                ),
                vec![s.into()],
            ))
        }
        SmartField::CapturedAt => {
            // `captured_at` stores an RFC 3339 timestamp; comparing the
            // first ten characters (the calendar day) against a `YYYY-MM-DD`
            // value works lexicographically for all six operators.
            let s = string_value(value, "captured_at")?;
            if !valid_date(&s) {
                return Err(Error::Validation(
                    "captured_at must be a YYYY-MM-DD date".into(),
                ));
            }
            Ok((
                format!("substr(assets.captured_at, 1, 10) {} ?", op_sql(op)),
                vec![s.into()],
            ))
        }
        SmartField::AspectRatio => {
            let v = value
                .as_f64()
                .ok_or_else(|| Error::Validation("aspect_ratio must be a number".into()))?;
            // NULL dimensions (non-images) yield NULL and never match.
            Ok((
                format!(
                    "(assets.width * 1.0 / NULLIF(assets.height, 0)) {} ?",
                    op_sql(op)
                ),
                vec![Value::Real(v)],
            ))
        }
        SmartField::Orientation => {
            require_eq_ne(op)?;
            let s = string_value(value, "orientation")?;
            let cond = match s.as_str() {
                "landscape" => "assets.width > assets.height",
                "portrait" => "assets.width < assets.height",
                "square" => "assets.width = assets.height",
                _ => {
                    return Err(Error::Validation(
                        "orientation must be landscape, portrait or square".into(),
                    ));
                }
            };
            let sql = if op == SmartCompare::Eq {
                cond.to_string()
            } else {
                format!("NOT ({cond})")
            };
            Ok((sql, vec![]))
        }
    }
}

/// The narrowing applied on top of a rule tree: the grid filters plus the page
/// window.
///
/// Bundled into one value because these always travel together — through the
/// `evaluate*` entry points and the store's `BrowseContext` — and because five
/// positional arguments made every call site unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmartPage {
    /// Restrict to one asset kind (the toolbar's type filter).
    pub kind: Option<AssetKind>,
    /// Restrict to favorites only (the toolbar's star filter).
    pub favorite: Option<bool>,
    /// `Some(n)` enables paging.
    pub limit: Option<u32>,
    /// Rows to skip; ignored when `limit` is `None`.
    pub offset: u64,
    /// The remaining grid filters — orientation, aspect band, resolution band,
    /// minimum rating, extension, and whatever the search box's qualifier
    /// grammar stated.
    ///
    /// They belong *inside* the statement, not after it: applied to the rows a
    /// page already returned, they shrink the page without shrinking the COUNT,
    /// so the header reports a collection the list can never deliver and the
    /// grid keeps re-running the query at the bottom of a scroll that no longer
    /// advances. Only the predicates `build_where` can express are honoured;
    /// a caller that sets a container field (collection, tag, folder) has to
    /// mean it, because this does apply one.
    pub filters: Option<AssetQuery>,
}

/// Evaluate a condition tree against the live library (trashed assets are
/// excluded). Returns the matching ids as a page.
pub fn evaluate(
    conn: &rusqlite::Connection,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
    limit: Option<u32>,
    offset: u64,
) -> Result<Page<Uuid>> {
    evaluate_filtered(
        conn,
        text,
        node,
        SmartPage {
            limit,
            offset,
            ..Default::default()
        },
    )
}

/// Like [`evaluate`], with the extra grid filters of [`SmartPage`] AND-ed
/// onto the tree — the toolbar filters compose with smart collections the
/// same way they compose with plain views.
pub fn evaluate_filtered(
    conn: &rusqlite::Connection,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
    page: SmartPage,
) -> Result<Page<Uuid>> {
    evaluate_counted(conn, text, node, page, true)
}

/// [`evaluate_filtered`] without the exact COUNT: the returned total is a
/// lower bound (this page's id count). Rapid refreshes use it and overlay a
/// cached exact total, keeping the COUNT off the hot path.
pub fn evaluate_filtered_without_count(
    conn: &rusqlite::Connection,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
    page: SmartPage,
) -> Result<Page<Uuid>> {
    evaluate_counted(conn, text, node, page, false)
}

/// The rule tree plus the narrowing on top of it, as a `WHERE` clause and its
/// parameters. Shared by [`evaluate_counted`] and [`count`] so the page and the
/// total are computed over one row set.
fn rule_where(
    conn: &rusqlite::Connection,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
    kind: Option<AssetKind>,
    favorite: Option<bool>,
    filters: Option<&AssetQuery>,
) -> Result<(String, Vec<Value>)> {
    let (tree, mut args) = compile(Some(conn), text, node)?;
    // Parenthesize the tree before appending: a compiled `or` group is a
    // bare `a OR b`, and `a OR b AND kind = ?` would let the AND bind to
    // only the last branch.
    let mut expr = format!("({tree})");
    if let Some(kind) = kind {
        expr.push_str(" AND assets.kind = ?");
        args.push(Value::from(kind_sql(kind).to_string()));
    }
    if let Some(favorite) = favorite {
        expr.push_str(" AND assets.is_favorite = ?");
        args.push(Value::Integer(favorite as i64));
    }
    // The remaining grid filters belong in here rather than after the page is
    // materialized: applied to rows already fetched, they shrink the page but
    // not the COUNT, so the header promises a set the list cannot deliver and
    // paging never reaches its own end. `base` shifts this clause's
    // hand-numbered placeholders past every parameter bound above it.
    if let Some(filters) = filters {
        let (fragment, mut fragment_args) = super::assets::where_fragment(
            conn,
            filters,
            super::assets::WhereMode::Rejecting,
            args.len(),
        )?;
        expr.push_str(&format!(" AND ({fragment})"));
        args.append(&mut fragment_args);
    }
    Ok((format!("WHERE trashed_at IS NULL AND ({expr})"), args))
}

/// How many live assets a rule tree selects, without fetching them. The browse
/// session asks this when the view opens, not on every page.
pub fn count(
    conn: &rusqlite::Connection,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
    kind: Option<AssetKind>,
    favorite: Option<bool>,
    filters: Option<&AssetQuery>,
) -> Result<u64> {
    let (where_sql, args) = rule_where(conn, text, node, kind, favorite, filters)?;
    Ok(rows::query_count(
        conn,
        &format!("SELECT COUNT(DISTINCT assets.id) FROM assets {where_sql}"),
        args,
    )? as u64)
}

fn evaluate_counted(
    conn: &rusqlite::Connection,
    text: Option<&crate::search::TextIndex>,
    node: &SmartNode,
    page: SmartPage,
    count_rows: bool,
) -> Result<Page<Uuid>> {
    let SmartPage {
        kind,
        favorite,
        limit,
        offset,
        filters,
    } = page;
    let (where_sql, mut args) = rule_where(conn, text, node, kind, favorite, filters.as_ref())?;

    // The COUNT never carries the page limit/offset, so snapshot the
    // where-clause args before they gain the paging ones.
    let count_args = args.clone();
    let mut sql = format!(
        "SELECT DISTINCT assets.id FROM assets {where_sql} \
         ORDER BY assets.created_at DESC, assets.id ASC"
    );
    if let Some(limit) = super::assets::checked_limit(limit)? {
        sql.push_str(" LIMIT ? OFFSET ?");
        args.push(Value::Integer(limit as i64));
        args.push(Value::Integer(offset as i64));
    }
    let ids = rows::query_map(conn, &sql, args, |row| rows::req_uuid(row, 0))?;

    let total = if count_rows {
        rows::query_count(
            conn,
            &format!("SELECT COUNT(DISTINCT assets.id) FROM assets {where_sql}"),
            count_args,
        )? as u64
    } else {
        ids.len() as u64
    };
    Ok(Page::new(total, ids))
}

// -- small helpers -----------------------------------------------------------

fn require_eq_ne(op: SmartCompare) -> Result<()> {
    if matches!(op, SmartCompare::Eq | SmartCompare::Ne) {
        Ok(())
    } else {
        Err(Error::Validation(
            "this field only supports == or !=".into(),
        ))
    }
}

fn string_value(v: &Json, what: &str) -> Result<String> {
    v.as_str()
        .map(ToString::to_string)
        .ok_or_else(|| Error::Validation(format!("{what} must be a string")))
}

fn number_value(v: &Json, what: &str) -> Result<i64> {
    v.as_i64()
        .ok_or_else(|| Error::Validation(format!("{what} must be an integer")))
}

/// Accept `#rrggbb` (case-insensitive, `#` optional) and normalize to lowercase
/// with the `#` prefix, matching what mining stores in `extra.dominant_color`.
pub(crate) fn normalize_color(v: &str) -> Result<String> {
    let mut s = v.trim().to_lowercase();
    if !s.starts_with('#') {
        s.insert(0, '#');
    }
    if s.len() == 7 && s[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(s)
    } else {
        Err(Error::Validation("color must be a #rrggbb value".into()))
    }
}

/// `YYYY-MM-DD` calendar date check (shape only; month/day ranges are not
/// validated beyond the digit layout — SQLite comparisons are string-based).
/// Public: the rule editor reuses it for live validation.
pub fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                true
            } else {
                c.is_ascii_digit()
            }
        })
}

fn op_sql(op: SmartCompare) -> &'static str {
    match op {
        SmartCompare::Eq => "=",
        SmartCompare::Ne => "<>",
        SmartCompare::Gt => ">",
        SmartCompare::Gte => ">=",
        SmartCompare::Lt => "<",
        SmartCompare::Lte => "<=",
    }
}

fn kind_sql(kind: AssetKind) -> &'static str {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_asset;
    use crate::store::{Store, assets};

    fn node_text(op: SmartCompare, text: &str) -> SmartNode {
        SmartNode::Match {
            field: SmartField::Text,
            op,
            value: serde_json::json!(text),
        }
    }

    #[test]
    fn text_not_contains_matches_assets_outside_the_candidate_set() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut img = test_asset("sunset.png", AssetKind::Image, Uuid::new_v4());
        img.title = Some("golden sunset".into());
        assets::insert(conn, &img).unwrap();
        let doc = test_asset("tax.txt", AssetKind::Document, Uuid::new_v4());
        assets::insert(conn, &doc).unwrap();

        let idx = crate::search::TextIndex::in_ram().unwrap();
        idx.index_asset(conn, img.id).unwrap();
        idx.commit().unwrap();

        // contains: only the asset whose indexed text hits the query.
        let page = evaluate(
            conn,
            Some(&idx),
            &node_text(SmartCompare::Eq, "sunset"),
            None,
            0,
        )
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0], img.id);

        // not contains: everything else — including assets with no indexed
        // text at all, since none of them match the query.
        let page = evaluate(
            conn,
            Some(&idx),
            &node_text(SmartCompare::Ne, "sunset"),
            None,
            0,
        )
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0], doc.id);

        // Without an index the candidate set is empty: contains matches
        // nothing and not-contains degrades to every asset.
        let page = evaluate(conn, None, &node_text(SmartCompare::Eq, "sunset"), None, 0).unwrap();
        assert_eq!(page.total, 0);
        let page = evaluate(conn, None, &node_text(SmartCompare::Ne, "sunset"), None, 0).unwrap();
        assert_eq!(page.total, 2);
    }

    #[test]
    fn text_rejects_comparison_operators() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let outcome = evaluate(conn, None, &node_text(SmartCompare::Lt, "x"), None, 0);
        assert!(outcome.is_err());
    }
}
