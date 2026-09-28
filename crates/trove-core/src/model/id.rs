//! Typed identifiers for the places where two kinds of id sit side by side.
//!
//! Entity fields stay `Uuid` — wrapping every id in every struct would ripple
//! through the SQL layer for no local gain. What these types fix is narrower:
//! the four store calls that take two ids of different kinds as adjacent
//! arguments,
//!
//! - `tags::{add_to_asset, remove_from_asset}(asset, tag)`
//! - `collections::{add_asset, remove_asset}(collection, asset)`
//!
//! where both parameters were `Uuid` and nothing stopped a caller passing the
//! tag where the asset goes. [`AssetId`] and [`TagId`] are distinct types, so
//! that swap is a compile error — which is the whole point of a newtype here,
//! not the ability to recover the `Uuid` inside.
//!
//! Each wraps the `Uuid` publicly because the caller already holds one (from
//! `asset.id`) and the safety comes from the *type* it is wrapped in, not from
//! hiding the value. There is deliberately no `From<Uuid>`: an implicit
//! conversion is exactly the accident this prevents, so a call site spells the
//! kind out as `AssetId(asset.id)`.

use uuid::Uuid;

/// The id of an asset. See the module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AssetId(pub Uuid);

/// The id of a tag. See the module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TagId(pub Uuid);

/// The id of a collection. See the module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CollectionId(pub Uuid);

impl From<AssetId> for Uuid {
    fn from(id: AssetId) -> Uuid {
        id.0
    }
}

impl From<TagId> for Uuid {
    fn from(id: TagId) -> Uuid {
        id.0
    }
}

impl From<CollectionId> for Uuid {
    fn from(id: CollectionId) -> Uuid {
        id.0
    }
}
