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

/// A saved-search tree as it is read back from storage.
///
/// The decoder is total on purpose. [`SmartNode`] cannot represent a tree a
/// foreign version wrote — an older `op` spelling, a shape this build's editor
/// no longer emits — and the row that carries one is still a folder the user
/// made, with a name, a parent and a look. So the raw JSON is kept in
/// [`SavedQuery::Foreign`] rather than either lying (an empty tree that would
/// silently match the whole library) or failing the whole row, which would
/// break renaming and appearance edits on a saved search that only its *rules*
/// are unreadable. Evaluating a foreign tree is a clean error; metadata work
/// is not affected.
///
/// Untagged serialization keeps the on-disk shape byte-identical to before for
/// both states, so existing libraries and exports need no migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SavedQuery {
    /// A tree this build can compile and evaluate.
    Node(SmartNode),
    /// A tree this build cannot name, kept verbatim.
    Foreign(Json),
}

impl SavedQuery {
    /// The parsed tree, or `None` for a shape this build cannot read.
    pub fn node(&self) -> Option<&SmartNode> {
        match self {
            Self::Node(node) => Some(node),
            Self::Foreign(_) => None,
        }
    }
}

impl From<SmartNode> for SavedQuery {
    fn from(node: SmartNode) -> Self {
        Self::Node(node)
    }
}

#[cfg(test)]
mod tests {
    use super::{SavedQuery, SmartField, SmartNode};

    /// A tree this build cannot name is kept verbatim, not dropped or reshaped
    /// into an empty tree — which would silently match the whole library.
    #[test]
    fn a_foreign_tree_round_trips_as_itself() {
        let raw = serde_json::json!({"op": "all", "children": []});
        let query: SavedQuery = serde_json::from_value(raw.clone()).unwrap();
        assert!(query.node().is_none(), "an old op spelling is not a node");
        assert_eq!(serde_json::to_value(&query).unwrap(), raw);
    }

    /// A tree this build knows parses as a node and survives a re-encode.
    ///
    /// The bytes may gain the defaulted `compare` on the way out — the stored
    /// shape always carried the operator, the input merely omitted it — so the
    /// assertion is on the decoded value, not on the exact text.
    #[test]
    fn a_known_tree_parses_and_round_trips() {
        let raw = serde_json::json!({"op": "match", "field": "kind", "value": "image"});
        let query: SavedQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(
            query.node(),
            Some(&SmartNode::Match {
                field: SmartField::Kind,
                op: super::SmartCompare::Eq,
                value: serde_json::json!("image"),
            })
        );
        let encoded = serde_json::to_value(&query).unwrap();
        let decoded: SavedQuery = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            decoded, query,
            "a known tree must survive its own re-encode"
        );
    }
}
