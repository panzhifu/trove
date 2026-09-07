//! Undo / redo operation log for metadata mutations.
//!
//! Every recorded [`Op`] carries enough before/after state to be applied and
//! inverted without recreating history. The stack lives inside
//! [`crate::library::Library`] (a `RefCell`, so the facade keeps its `&self`
//! signature); destructive operations that cannot be inverted — purge, empty
//! trash, imports, deleting a tag or collection — are intentionally **not**
//! recorded.

use std::cell::RefCell;

use rusqlite::Connection;
use uuid::Uuid;

use crate::error::Result;
use crate::model::AssetPatch;
use crate::store::{assets, collections, tags};

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// One invertible metadata mutation.
#[derive(Debug, Clone)]
pub enum Op {
    /// Restore every editable column of one asset from the inverse patch
    /// (both patches are fully populated, so undo and redo are symmetric).
    PatchAsset {
        id: Uuid,
        before: AssetPatch,
        after: AssetPatch,
    },
    /// Per-asset trash flags around a batch flip (`before`/`after` are
    /// parallel per-id states).
    SetTrashed {
        before: Vec<(Uuid, bool)>,
        after: Vec<(Uuid, bool)>,
    },
    /// Per-asset favorite flags around a batch flip.
    SetFavorite {
        before: Vec<(Uuid, bool)>,
        after: Vec<(Uuid, bool)>,
    },
    /// One batch title rewrite (multi-select rename).
    SetTitles {
        before: Vec<(Uuid, Option<String>)>,
        after: Vec<(Uuid, Option<String>)>,
    },
    /// Replace one asset's whole tag group.
    SetTags {
        asset: Uuid,
        before: Vec<Uuid>,
        after: Vec<Uuid>,
    },
    TagRename {
        id: Uuid,
        before: String,
        after: String,
    },
    TagColor {
        id: Uuid,
        before: Option<String>,
        after: Option<String>,
    },
    TagParent {
        id: Uuid,
        before: Option<Uuid>,
        after: Option<Uuid>,
    },
    CollectionRename {
        id: Uuid,
        before: String,
        after: String,
    },
    CollectionMove {
        id: Uuid,
        before: (Option<Uuid>, i64),
        after: (Option<Uuid>, i64),
    },
    /// Assets newly added to a collection (only the actual delta is recorded).
    MembershipAdd { collection: Uuid, added: Vec<Uuid> },
    /// Assets removed from a collection.
    MembershipRemove {
        collection: Uuid,
        removed: Vec<Uuid>,
    },
}

impl Op {
    /// Run the forward side of the operation.
    fn apply(&self, conn: &Connection) -> Result<()> {
        match self {
            Op::PatchAsset { id, after, .. } => {
                assets::update(conn, *id, after)?;
            }
            Op::SetTrashed { after, .. } => {
                for (id, trashed) in after {
                    assets::set_trashed(conn, *id, *trashed)?;
                }
            }
            Op::SetFavorite { after, .. } => {
                for (id, favorite) in after {
                    assets::update(
                        conn,
                        *id,
                        &AssetPatch {
                            is_favorite: Some(*favorite),
                            ..Default::default()
                        },
                    )?;
                }
            }
            Op::SetTitles { after, .. } => {
                for (id, title) in after {
                    assets::update(
                        conn,
                        *id,
                        &AssetPatch {
                            title: Some(title.clone()),
                            ..Default::default()
                        },
                    )?;
                }
            }
            Op::SetTags { asset, after, .. } => {
                tags::set_for_asset(conn, *asset, after)?;
            }
            Op::TagRename { id, after, .. } => {
                tags::rename(conn, *id, after)?;
            }
            Op::TagColor { id, after, .. } => {
                tags::set_color(conn, *id, after.as_deref())?;
            }
            Op::TagParent { id, after, .. } => {
                tags::move_to(conn, *id, *after)?;
            }
            Op::CollectionRename { id, after, .. } => {
                collections::rename(conn, *id, after)?;
            }
            Op::CollectionMove { id, after, .. } => {
                collections::move_to(conn, *id, after.0, after.1)?;
            }
            Op::MembershipAdd { collection, added } => {
                for id in added {
                    // Idempotent: re-adding an existing membership is a no-op.
                    collections::add_asset(conn, *collection, *id)?;
                }
            }
            Op::MembershipRemove {
                collection,
                removed,
            } => {
                for id in removed {
                    let _ = collections::remove_asset(conn, *collection, *id);
                }
            }
        }
        Ok(())
    }

    /// The operation that undoes this one.
    fn inverse(&self) -> Op {
        match self {
            Op::PatchAsset { id, before, after } => Op::PatchAsset {
                id: *id,
                before: after.clone(),
                after: before.clone(),
            },
            Op::SetTrashed { before, after } => Op::SetTrashed {
                before: after.clone(),
                after: before.clone(),
            },
            Op::SetFavorite { before, after } => Op::SetFavorite {
                before: after.clone(),
                after: before.clone(),
            },
            Op::SetTitles { before, after } => Op::SetTitles {
                before: after.clone(),
                after: before.clone(),
            },
            Op::SetTags {
                asset,
                before,
                after,
            } => Op::SetTags {
                asset: *asset,
                before: after.clone(),
                after: before.clone(),
            },
            Op::TagRename { id, before, after } => Op::TagRename {
                id: *id,
                before: after.clone(),
                after: before.clone(),
            },
            Op::TagColor { id, before, after } => Op::TagColor {
                id: *id,
                before: after.clone(),
                after: before.clone(),
            },
            Op::TagParent { id, before, after } => Op::TagParent {
                id: *id,
                before: *after,
                after: *before,
            },
            Op::CollectionRename { id, before, after } => Op::CollectionRename {
                id: *id,
                before: after.clone(),
                after: before.clone(),
            },
            Op::CollectionMove { id, before, after } => Op::CollectionMove {
                id: *id,
                before: *after,
                after: *before,
            },
            Op::MembershipAdd { collection, added } => Op::MembershipRemove {
                collection: *collection,
                removed: added.clone(),
            },
            Op::MembershipRemove {
                collection,
                removed,
            } => Op::MembershipAdd {
                collection: *collection,
                added: removed.clone(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Stack
// ---------------------------------------------------------------------------

/// Two-stack undo/redo log. `redo` is cleared whenever a new op is recorded
/// (standard linear-history semantics).
#[derive(Debug, Default, Clone)]
pub struct UndoStack {
    undo: Vec<Op>,
    redo: Vec<Op>,
}

impl UndoStack {
    pub fn record(&mut self, op: Op) {
        self.undo.push(op);
        self.redo.clear();
    }

    /// Undo the most recent op. Returns `false` when the stack is empty.
    pub fn undo(&mut self, conn: &Connection) -> Result<bool> {
        let Some(op) = self.undo.pop() else {
            return Ok(false);
        };
        let inverse = op.inverse();
        inverse.apply(conn)?;
        self.redo.push(op);
        Ok(true)
    }

    /// Redo the most recently undone op. Returns `false` when empty.
    pub fn redo(&mut self, conn: &Connection) -> Result<bool> {
        let Some(op) = self.redo.pop() else {
            return Ok(false);
        };
        op.apply(conn)?;
        self.undo.push(op);
        Ok(true)
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
}

/// Shared stack cell embedded in `Library`.
#[derive(Debug, Default, Clone)]
pub(crate) struct SharedUndoStack(RefCell<UndoStack>);

impl SharedUndoStack {
    pub fn record(&self, op: Op) {
        self.0.borrow_mut().record(op);
    }

    pub fn undo(&self, conn: &Connection) -> Result<bool> {
        self.0.borrow_mut().undo(conn)
    }

    pub fn redo(&self, conn: &Connection) -> Result<bool> {
        self.0.borrow_mut().redo(conn)
    }

    pub fn undo_len(&self) -> usize {
        self.0.borrow().undo_len()
    }

    pub fn redo_len(&self) -> usize {
        self.0.borrow().redo_len()
    }
}

/// Build the fully-populated inverse patch that restores every editable
/// column of `asset` to its current (pre-mutation) state.
pub(crate) fn restore_patch(asset: &crate::model::Asset) -> AssetPatch {
    AssetPatch {
        title: Some(asset.title.clone()),
        description: Some(asset.description.clone()),
        kind: Some(asset.kind),
        rating: Some(asset.rating),
        is_favorite: Some(asset.is_favorite),
        source_url: Some(asset.source_url.clone()),
        color_label: Some(asset.color_label.clone()),
        extra: Some(asset.extra.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::model::{Asset, AssetKind, NewCollection, NewTag, Origin, now};
    use crate::store::Store;

    fn sample_asset(name: &str, kind: AssetKind) -> Asset {
        let id = Uuid::new_v4();
        Asset {
            id,
            origin: Origin::Stored,
            rel_path: Some(format!("media/{}/{}", &id.to_string()[..2], name)),
            file_name: name.to_string(),
            ext: "png".into(),
            mime: "image/png".into(),
            size_bytes: 128,
            sha256: Some("a".repeat(64)),
            kind,
            width: Some(800),
            height: Some(600),
            duration_ms: None,
            captured_at: None,
            title: None,
            description: None,
            rating: None,
            is_favorite: false,
            source_url: None,
            color_label: None,
            extra: Default::default(),
            created_at: now(),
            updated_at: now(),
            trashed_at: None,
        }
    }

    #[test]
    fn undo_redo_roundtrips_patch_favorite_and_tags() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut stack = UndoStack::default();

        let mut a = sample_asset("a.png", AssetKind::Image);
        assets::insert(conn, &a).unwrap();
        a.title = None;

        // Forward: set a title.
        let patch = AssetPatch {
            title: Some(Some("hello".into())),
            ..Default::default()
        };
        stack.record(Op::PatchAsset {
            id: a.id,
            before: restore_patch(&a),
            after: patch,
        });
        stack.undo(conn).unwrap();
        assert_eq!(assets::get(conn, a.id).unwrap().unwrap().title, None);
        assert_eq!(stack.undo_len(), 0);
        stack.redo(conn).unwrap();
        assert_eq!(
            assets::get(conn, a.id).unwrap().unwrap().title,
            Some("hello".into())
        );

        // Forward: favorite flip.
        stack.record(Op::SetFavorite {
            before: vec![(a.id, false)],
            after: vec![(a.id, true)],
        });
        stack.undo(conn).unwrap();
        assert!(!assets::get(conn, a.id).unwrap().unwrap().is_favorite);
        stack.redo(conn).unwrap();
        assert!(assets::get(conn, a.id).unwrap().unwrap().is_favorite);

        // Forward: tag group replace.
        let t1 = tags::create(
            conn,
            &NewTag {
                name: "one".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        let t2 = tags::create(
            conn,
            &NewTag {
                name: "two".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        tags::add_to_asset(conn, a.id, t1.id).unwrap();
        stack.record(Op::SetTags {
            asset: a.id,
            before: vec![t1.id],
            after: vec![t2.id],
        });
        stack.undo(conn).unwrap();
        assert_eq!(tags::for_asset(conn, a.id).unwrap()[0].id, t1.id);
        stack.redo(conn).unwrap();
        assert_eq!(tags::for_asset(conn, a.id).unwrap()[0].id, t2.id);

        // Recording clears the redo branch (linear history).
        stack.record(Op::SetTags {
            asset: a.id,
            before: vec![t2.id],
            after: vec![],
        });
        assert_eq!(stack.redo_len(), 0);
        assert_eq!(stack.undo_len(), 4);
    }

    #[test]
    fn undo_redo_collection_membership_and_move() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut stack = UndoStack::default();

        let a = sample_asset("a.png", AssetKind::Image);
        assets::insert(conn, &a).unwrap();
        let c1 = collections::create(
            conn,
            &NewCollection {
                parent_id: None,
                name: "one".into(),
                position: 0,
            },
        )
        .unwrap();
        let c2 = collections::create(
            conn,
            &NewCollection {
                parent_id: None,
                name: "two".into(),
                position: 1,
            },
        )
        .unwrap();

        // Membership add / undo / redo (`record` only logs; the forward
        // mutation itself is applied first, exactly as the facade does).
        let op = Op::MembershipAdd {
            collection: c1.id,
            added: vec![a.id],
        };
        op.apply(conn).unwrap();
        stack.record(op);
        assert_eq!(collections::asset_ids(conn, c1.id).unwrap(), vec![a.id]);
        stack.undo(conn).unwrap();
        assert!(collections::asset_ids(conn, c1.id).unwrap().is_empty());
        stack.redo(conn).unwrap();
        assert_eq!(collections::asset_ids(conn, c1.id).unwrap(), vec![a.id]);

        // Move under another collection, then undo back to root.
        let op = Op::CollectionMove {
            id: c2.id,
            before: (None, 1),
            after: (Some(c1.id), 0),
        };
        op.apply(conn).unwrap();
        stack.record(op);
        assert_eq!(
            collections::get(conn, c2.id).unwrap().unwrap().parent_id,
            Some(c1.id)
        );
        stack.undo(conn).unwrap();
        assert_eq!(
            collections::get(conn, c2.id).unwrap().unwrap().parent_id,
            None
        );
    }

    #[test]
    fn library_facade_records_and_replays() {
        let lib = Library::open_in_memory(
            std::env::temp_dir().join(format!("trove-undo-{}", Uuid::new_v4())),
        )
        .unwrap();
        let conn = lib.store().conn();
        let a = sample_asset("a.png", AssetKind::Image);
        assets::insert(conn, &a).unwrap();

        // Recorded through the facade.
        lib.set_assets_favorite(&[a.id], true).unwrap();
        assert!(assets::get(conn, a.id).unwrap().unwrap().is_favorite);
        lib.undo().unwrap();
        assert!(!assets::get(conn, a.id).unwrap().unwrap().is_favorite);
        lib.redo().unwrap();
        assert!(assets::get(conn, a.id).unwrap().unwrap().is_favorite);

        // Tag rename through the facade keeps FTS in sync both ways.
        let tag = tags::create(
            conn,
            &NewTag {
                name: "beach".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();
        tags::add_to_asset(conn, a.id, tag.id).unwrap();
        lib.rename_tag(tag.id, "coastline").unwrap();
        assert_eq!(tags::get(conn, tag.id).unwrap().unwrap().name, "coastline");
        lib.undo().unwrap();
        assert_eq!(tags::get(conn, tag.id).unwrap().unwrap().name, "beach");

        // After undoing the rename, the earlier favorite flip is still
        // undoable; an empty stack undoes nothing (no error).
        assert!(lib.undo().unwrap());
        assert!(!assets::get(conn, a.id).unwrap().unwrap().is_favorite);
        assert!(!lib.undo().unwrap());
    }
}
