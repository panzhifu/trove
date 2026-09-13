//! The two container types that organize assets: the manual [`Collection`]
//! (membership stored as rows) and the [`SmartCollection`] (membership
//! computed live from a rule tree). They share the tree/renaming/ordering
//! semantics and are colocated for that reason, but stay separate types: a
//! collection's members are materialized rows while a smart collection's are
//! the result of a predicate, which leaks into membership operations
//! (add/remove, counts, export) and the storage layout (join table vs query
//! column).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use super::MAX_NAME_LEN;

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// A user-organized folder. Collections form a tree (`parent_id`) while the
/// membership of assets is many-to-many, so one asset can live in several
/// collections without duplicating its file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    /// Order among siblings within the same parent.
    pub position: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewCollection {
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub position: i64,
}

impl NewCollection {
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        validate_name("collection name", &self.name)
    }
}

// ---------------------------------------------------------------------------
// Smart (saved-search) collection
// ---------------------------------------------------------------------------

/// A saved search: a named condition tree re-evaluated live against the
/// library whenever it is opened. Smart collections may nest (`parent_id`
/// may reference another smart collection or a regular collection), though
/// regular collections can never live under a smart one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartCollection {
    pub id: Uuid,
    /// Parent smart collection or collection; `None` = top level. The
    /// referenced table is not known to the schema (no SQL FK), so callers
    /// resolve it against both trees.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    pub name: String,
    /// The condition tree (`SmartNode`), serialized as JSON.
    pub query: Json,
    pub color: Option<String>,
    /// Order among siblings within the same parent.
    pub position: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSmartCollection {
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub query: Json,
    pub color: Option<String>,
    pub position: i64,
}

impl NewSmartCollection {
    /// Validate the name. The condition tree is *not* checked here — the
    /// model layer carries no storage concerns, so runnability is validated
    /// where the tree is compiled (`store::smart::validate_json`) at every
    /// creation entry point.
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        validate_name("smart collection name", &self.name)
    }
}

fn validate_name(what: &str, name: &str) -> Result<(), crate::error::Error> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::error::Error::Validation(format!(
            "{what} must not be empty"
        )));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(crate::error::Error::Validation(format!(
            "{what} exceeds {MAX_NAME_LEN} characters"
        )));
    }
    Ok(())
}
