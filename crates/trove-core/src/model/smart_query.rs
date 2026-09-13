//! The saved-search condition tree: a small rule DSL evaluated against the
//! asset table. Serialization lives here; SQL compilation and evaluation in
//! `store::smart`.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// A condition node in a smart-collection query tree.
///
/// Serialized as `{ "op": "and"|"or"|"match", … }`; matches carry the field,
/// comparison operator and a JSON value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum SmartNode {
    And {
        children: Vec<SmartNode>,
    },
    Or {
        children: Vec<SmartNode>,
    },
    #[serde(rename = "match")]
    Match {
        field: SmartField,
        /// Comparison operator, serialized as `compare` (the `op` key is the
        /// internal tag, so a payload field cannot reuse it). Defaults to `==`
        /// when omitted.
        #[serde(rename = "compare", default)]
        op: SmartCompare,
        value: Json,
    },
}

/// The domain field a smart-collection condition tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartField {
    Kind,
    IsFavorite,
    Rating,
    Tag,
    Text,
    Extension,
    SizeBytes,
    /// The mined dominant color (`#rrggbb`), matched against the asset's
    /// `facts.visual.dominant_color` (stored under the flat key
    /// `dominant_color`).
    Color,
    /// The EXIF capture date, compared as a `YYYY-MM-DD` day.
    CapturedAt,
    /// The image aspect ratio (`width / height`).
    AspectRatio,
    /// Orientation derived from width vs height (landscape/portrait/square).
    Orientation,
}

/// Comparison operators for a smart-collection condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartCompare {
    #[default]
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}
