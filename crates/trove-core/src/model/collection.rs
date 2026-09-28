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
use uuid::Uuid;

use super::{Appearance, MAX_NAME_LEN, SavedQuery, SmartNode};

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
    /// The user's own glyph and accent for this folder, or nothing.
    #[serde(default)]
    pub appearance: Appearance,
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
    /// The condition tree. Typed, not a JSON blob: the tree a user saved and
    /// the tree the store compiles are the same value, so a stored rule cannot
    /// be one shape here and another where it is evaluated. It is serialized
    /// as JSON in the `query` column, which is a storage detail of the store.
    ///
    /// [`SavedQuery`] rather than a bare [`SmartNode`] because the read side
    /// meets trees written by other versions: a shape this build cannot parse
    /// is kept, not dropped, so its row still renames and exports.
    pub query: SavedQuery,
    /// The user's own glyph and accent for this folder, or nothing. Same
    /// column and same type as a plain [`Collection`]'s: the tree draws the two
    /// alike, and `color` — a free-form hex only this field's picker could
    /// produce — is what the named accent replaced.
    #[serde(default)]
    pub appearance: Appearance,
    /// Order among siblings within the same parent.
    pub position: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creating a container never sets its look: an appearance is something a user
/// adds to an existing folder, so it goes through `set_appearance` and the two
/// create paths stay about the row's substance.
#[derive(Debug, Clone)]
pub struct NewSmartCollection {
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub query: SmartNode,
    pub position: i64,
}

impl NewSmartCollection {
    /// Validate the name. The condition tree is typed, but *runnable* is a
    /// storage concern: the model layer cannot know which fields the current
    /// schema compiles, so that check stays where the tree is compiled
    /// (`store::smart::validate`) at every creation entry point.
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
