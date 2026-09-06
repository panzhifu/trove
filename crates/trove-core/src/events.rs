//! Change events produced by the service layer.
//!
//! The UI (or any observer) subscribes to these to refresh its view after a
//! domain mutation. They describe *what happened*, never how to render it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LibraryEvent {
    AssetCreated { id: Uuid },
    AssetUpdated { id: Uuid },
    AssetTrashed { id: Uuid },
    AssetRestored { id: Uuid },
    AssetDeleted { id: Uuid },

    CollectionCreated { id: Uuid },
    CollectionUpdated { id: Uuid },
    CollectionDeleted { id: Uuid },

    TagCreated { id: Uuid },
    TagDeleted { id: Uuid },
}
