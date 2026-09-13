//! Tags: case-insensitively unique labels that may nest.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::MAX_NAME_LEN;

/// A case-insensitively unique descriptive label. Tags may nest
/// (`parent_id`): filtering by a tag implicitly includes its whole subtree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tag {
    pub id: Uuid,
    pub name: String,
    pub color: Option<String>,
    pub parent_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTag {
    pub name: String,
    pub color: Option<String>,
    pub parent_id: Option<Uuid>,
}

impl NewTag {
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(crate::error::Error::Validation(
                "tag name must not be empty".into(),
            ));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(crate::error::Error::Validation(format!(
                "tag name exceeds {MAX_NAME_LEN} characters"
            )));
        }
        Ok(())
    }
}
