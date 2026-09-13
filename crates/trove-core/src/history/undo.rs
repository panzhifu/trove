//! Undo / redo operation history for metadata mutations.
//!
//! Every recorded entry pairs an [`Op`] (enough before/after state to be
//! applied and inverted without recreating history) with an [`OpDesc`], a
//! human-facing description snapshot for the status bar. The stack lives
//! inside [`crate::library::Library`] (a `RefCell`, so the facade keeps its
//! `&self` signature) and is bounded — the oldest entries are evicted once
//! `cap` is reached. It is deliberately not persisted: destructive
//! operations that cannot be inverted — purge, empty trash, imports,
//! deleting a tag or collection — are never recorded.

use std::cell::RefCell;

use rusqlite::Connection;
use uuid::Uuid;

use crate::error::Result;
use crate::model::AssetPatch;
use crate::store::{assets, collections, tags};

/// How many steps are kept when no explicit cap is configured.
pub const DEFAULT_UNDO_CAP: usize = 20;

// ---------------------------------------------------------------------------
// Descriptions
// ---------------------------------------------------------------------------

/// What kind of mutation a history entry describes. One variant per
/// user-visible verb; the UI localizes via [`OpAction::key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpAction {
    /// Metadata patch on one asset.
    Edit,
    Trash,
    Restore,
    Favorite,
    Unfavorite,
    /// Batch title rewrite.
    Rename,
    /// Replace one asset's tag group.
    TagSet,
    TagRenamed,
    TagColored,
    TagMoved,
    CollectionRenamed,
    CollectionMoved,
    AddedToCollection,
    RemovedFromCollection,
}

impl OpAction {
    /// The i18n key the UI renders this action with.
    pub fn key(self) -> &'static str {
        match self {
            OpAction::Edit => "history.action.edit",
            OpAction::Trash => "history.action.trash",
            OpAction::Restore => "history.action.restore",
            OpAction::Favorite => "history.action.favorite",
            OpAction::Unfavorite => "history.action.unfavorite",
            OpAction::Rename => "history.action.rename",
            OpAction::TagSet => "history.action.tag_set",
            OpAction::TagRenamed => "history.action.tag_renamed",
            OpAction::TagColored => "history.action.tag_colored",
            OpAction::TagMoved => "history.action.tag_moved",
            OpAction::CollectionRenamed => "history.action.collection_renamed",
            OpAction::CollectionMoved => "history.action.collection_moved",
            OpAction::AddedToCollection => "history.action.added_to_collection",
            OpAction::RemovedFromCollection => "history.action.removed_from_collection",
        }
    }
}

/// Human-facing description of one recorded mutation, snapshotted at record
/// time (names may go stale later — acceptable for a history line).
///
/// `target` carries the affected object's name when exactly one object was
/// touched (an asset file name, a tag name, a collection name); `count` is
/// the number of touched objects. The UI localizes the count noun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpDesc {
    pub action: OpAction,
    pub target: Option<String>,
    pub count: usize,
}

impl OpDesc {
    pub fn new(action: OpAction, target: Option<String>, count: usize) -> Self {
        Self {
            action,
            target,
            count,
        }
    }

    /// A description for `count` objects without a name.
    pub fn counted(action: OpAction, count: usize) -> Self {
        Self {
            action,
            target: None,
            count,
        }
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// One invertible metadata mutation.
#[derive(Debug, Clone)]
pub enum Op {
    /// Restore every editable column of one asset from the inverse patch
    /// (both patches are fully populated, so undo and redo are symmetric).
    /// Patches are boxed: they dominate the enum's size otherwise.
    PatchAsset {
        id: Uuid,
        before: Box<AssetPatch>,
        after: Box<AssetPatch>,
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
                before: Box::new(after.as_ref().clone()),
                after: Box::new(before.as_ref().clone()),
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

/// One history entry: the invertible operation plus its description.
#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    op: Op,
    desc: OpDesc,
}

/// Two-stack undo/redo history. `redo` is cleared whenever a new op is
/// recorded (standard linear-history semantics); the oldest entries are
/// evicted once `cap` is reached.
#[derive(Debug, Clone)]
pub struct UndoStack {
    undo: Vec<Recorded>,
    redo: Vec<Recorded>,
    cap: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::with_cap(DEFAULT_UNDO_CAP)
    }
}

impl UndoStack {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            cap: cap.max(1),
        }
    }

    pub fn record(&mut self, op: Op, desc: OpDesc) {
        if self.cap <= self.undo.len() {
            self.undo.remove(0);
        }
        self.undo.push(Recorded { op, desc });
        self.redo.clear();
    }

    /// Undo the most recent op. Returns `false` when the history is empty.
    pub fn undo(&mut self, conn: &Connection) -> Result<bool> {
        let Some(entry) = self.undo.pop() else {
            return Ok(false);
        };
        let inverse = entry.op.inverse();
        inverse.apply(conn)?;
        self.redo.push(entry);
        Ok(true)
    }

    /// Redo the most recently undone op. Returns `false` when empty.
    pub fn redo(&mut self, conn: &Connection) -> Result<bool> {
        let Some(entry) = self.redo.pop() else {
            return Ok(false);
        };
        entry.op.apply(conn)?;
        self.undo.push(entry);
        Ok(true)
    }

    /// Undo up to `steps` entries in sequence, stopping early when the
    /// history runs out or an application fails (already-applied steps stay
    /// applied). Returns how many steps were undone.
    pub fn undo_steps(&mut self, steps: usize, conn: &Connection) -> Result<usize> {
        let mut done = 0;
        for _ in 0..steps {
            if !self.undo(conn)? {
                break;
            }
            done += 1;
        }
        Ok(done)
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Descriptions of the last `n` undoable entries, most recent first.
    pub fn undo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.undo
            .iter()
            .rev()
            .take(n)
            .map(|e| e.desc.clone())
            .collect()
    }

    /// Descriptions of the last `n` redoable entries, next-first.
    pub fn redo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.redo
            .iter()
            .rev()
            .take(n)
            .map(|e| e.desc.clone())
            .collect()
    }
}

/// Shared history cell embedded in `Library`.
#[derive(Debug, Clone)]
pub(crate) struct SharedUndoStack(RefCell<UndoStack>);

impl SharedUndoStack {
    pub fn with_cap(cap: usize) -> Self {
        Self(RefCell::new(UndoStack::with_cap(cap)))
    }

    pub fn record(&self, op: Op, desc: OpDesc) {
        self.0.borrow_mut().record(op, desc);
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

    pub fn undo_steps(&self, steps: usize, conn: &Connection) -> Result<usize> {
        self.0.borrow_mut().undo_steps(steps, conn)
    }

    pub fn undo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.0.borrow().undo_entries(n)
    }

    pub fn redo_entries(&self, n: usize) -> Vec<OpDesc> {
        self.0.borrow().redo_entries(n)
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
        usage_status: Some(asset.usage_status),
        commercial_use: Some(asset.commercial_use),
        facts: Some(asset.facts.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::model::{Asset, AssetKind, NewCollection, NewTag, Origin, UsageStatus, now};
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
            usage_status: UsageStatus::Unused,
            commercial_use: None,
            facts: Default::default(),
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
        stack.record(
            Op::PatchAsset {
                id: a.id,
                before: Box::new(restore_patch(&a)),
                after: Box::new(patch),
            },
            OpDesc::counted(OpAction::Edit, 1),
        );
        stack.undo(conn).unwrap();
        assert_eq!(assets::get(conn, a.id).unwrap().unwrap().title, None);
        assert_eq!(stack.undo_len(), 0);
        stack.redo(conn).unwrap();
        assert_eq!(
            assets::get(conn, a.id).unwrap().unwrap().title,
            Some("hello".into())
        );

        // Forward: favorite flip.
        stack.record(
            Op::SetFavorite {
                before: vec![(a.id, false)],
                after: vec![(a.id, true)],
            },
            OpDesc::counted(OpAction::Favorite, 1),
        );
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
        stack.record(
            Op::SetTags {
                asset: a.id,
                before: vec![t1.id],
                after: vec![t2.id],
            },
            OpDesc::counted(OpAction::TagSet, 1),
        );
        stack.undo(conn).unwrap();
        assert_eq!(tags::for_asset(conn, a.id).unwrap()[0].id, t1.id);
        stack.redo(conn).unwrap();
        assert_eq!(tags::for_asset(conn, a.id).unwrap()[0].id, t2.id);

        // Recording clears the redo branch (linear history).
        stack.record(
            Op::SetTags {
                asset: a.id,
                before: vec![t2.id],
                after: vec![],
            },
            OpDesc::counted(OpAction::TagSet, 1),
        );
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
        stack.record(op, OpDesc::counted(OpAction::Edit, 1));
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
        stack.record(op, OpDesc::counted(OpAction::Edit, 1));
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

        // Tag rename through the facade keeps the search index in sync both ways.
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

    #[test]
    fn cap_evicts_oldest_and_entries_describe() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let a = sample_asset("a.png", AssetKind::Image);
        assets::insert(conn, &a).unwrap();

        let mut stack = UndoStack::with_cap(3);
        for i in 0..5 {
            stack.record(
                Op::SetFavorite {
                    before: vec![(a.id, i % 2 == 0)],
                    after: vec![(a.id, i % 2 == 1)],
                },
                OpDesc::new(OpAction::Favorite, Some(format!("f{i}.png")), 1),
            );
        }
        // Only the newest three survive the cap.
        assert_eq!(stack.undo_len(), 3);
        let entries = stack.undo_entries(3);
        assert_eq!(entries[0].target.as_deref(), Some("f4.png"));
        assert_eq!(entries[2].target.as_deref(), Some("f2.png"));

        // Undoing flips favorite twice and the redo side describes next-first.
        assert_eq!(stack.undo_steps(2, conn).unwrap(), 2);
        assert_eq!(stack.redo_len(), 2);
        assert_eq!(stack.redo_entries(2)[0].target.as_deref(), Some("f3.png"));

        // undo_steps stops at the empty stack instead of erroring.
        assert_eq!(stack.undo_steps(10, conn).unwrap(), 1);
        assert_eq!(stack.undo_len(), 0);
    }
}
