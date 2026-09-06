//! Smart-collection condition tree: compile a [`SmartNode`] tree into a SQL
//! `WHERE` fragment, and evaluate it live against the library.

use libsql::Value;
use serde_json::Value as Json;

use super::assets;
use super::rows;
use crate::error::{Error, Result};
use crate::model::{AssetKind, SmartCompare, SmartField, SmartNode};

/// Deserialize a stored JSON condition tree into a [`SmartNode`].
pub fn node_from_json(json: &Json) -> Result<SmartNode> {
    serde_json::from_value(json.clone())
        .map_err(|e| Error::Validation(format!("invalid condition tree: {e}")))
}

/// Compile a condition tree into a boolean `WHERE` fragment (no leading
/// `WHERE`) and its positional parameters. Used both at save time (validation)
/// and at query time (evaluation).
pub fn compile(node: &SmartNode) -> Result<(String, Vec<Value>)> {
    match node {
        SmartNode::And { children } => join(" AND ", children),
        SmartNode::Or { children } => join(" OR ", children),
        SmartNode::Match { field, op, value } => compile_match(*field, *op, value),
    }
}

fn join(sep: &str, children: &[SmartNode]) -> Result<(String, Vec<Value>)> {
    if children.is_empty() {
        return Err(Error::Validation("condition tree must not be empty".into()));
    }
    let mut parts = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    for child in children {
        let (sql, mut child_args) = compile(child)?;
        parts.push(format!("({sql})"));
        args.append(&mut child_args);
    }
    Ok((parts.join(sep), args))
}

fn compile_match(field: SmartField, op: SmartCompare, value: &Json) -> Result<(String, Vec<Value>)> {
    match field {
        SmartField::Kind => {
            require_eq_ne(op)?;
            let kind: AssetKind = serde_json::from_value(value.clone())
                .map_err(|_| Error::Validation("kind must be a valid asset kind".into()))?;
            Ok((format!("assets.kind {} ?", op_sql(op)), vec![kind_sql(kind).into()]))
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
            Ok((format!("assets.rating {} ?", op_sql(op)), vec![Value::Integer(n)]))
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
            Ok((
                format!(
                    "{operator}EXISTS (SELECT 1 FROM asset_tag at JOIN tags t ON t.id = at.tag_id \
                     WHERE at.asset_id = assets.id AND t.name = ? COLLATE NOCASE)"
                ),
                vec![s.into()],
            ))
        }
        SmartField::Text => {
            require_eq(op)?;
            let s = string_value(value, "text")?;
            let fts = assets::fts_query(&s);
            Ok((
                "assets.id IN (SELECT asset_id FROM asset_fts WHERE asset_fts MATCH ?)".into(),
                vec![fts.into()],
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
    }
}

/// Evaluate a condition tree against the live library (trashed assets are
/// excluded). Returns `(total_matching, matching_ids)`.
pub fn evaluate(
    conn: &libsql::Connection,
    node: &SmartNode,
    limit: Option<u32>,
    offset: u64,
) -> Result<(u64, Vec<uuid::Uuid>)> {
    let (expr, mut args) = compile(node)?;
    let where_sql = format!("WHERE trashed_at IS NULL AND ({expr})");

    let total = rows::query_count(
        conn,
        &format!("SELECT COUNT(DISTINCT assets.id) FROM assets {where_sql}"),
        args.clone(),
    )? as u64;

    let mut sql = format!(
        "SELECT DISTINCT assets.id FROM assets {where_sql} \
         ORDER BY assets.created_at DESC, assets.id ASC"
    );
    if let Some(limit) = limit {
        let limit = limit.min(1_000);
        sql.push_str(" LIMIT ? OFFSET ?");
        args.push(Value::Integer(limit as i64));
        args.push(Value::Integer(offset as i64));
    }

    let ids = rows::query_map(conn, &sql, args, |row| rows::req_uuid(row, 0))?;
    Ok((total, ids))
}

// -- small helpers -----------------------------------------------------------

fn require_eq_ne(op: SmartCompare) -> Result<()> {
    if matches!(op, SmartCompare::Eq | SmartCompare::Ne) {
        Ok(())
    } else {
        Err(Error::Validation("this field only supports == or !=".into()))
    }
}

fn require_eq(op: SmartCompare) -> Result<()> {
    if op == SmartCompare::Eq {
        Ok(())
    } else {
        Err(Error::Validation("this field only supports ==".into()))
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
fn normalize_color(v: &str) -> Result<String> {
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
        AssetKind::Other => "other",
    }
}