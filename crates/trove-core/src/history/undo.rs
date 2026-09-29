//! Undo / redo operation history for metadata mutations.
//!
//! Every recorded entry pairs an [`Op`] (enough before/after state to be
//! applied and inverted without recreating history) with an [`OpDesc`], a
//! human-facing description snapshot for the status bar.
//!
//! **The history is a table — `undo_log`, in the library's own database** — not
//! a stack in RAM. One row per applied mutation, ordered by `seq`, with
//! `undone_at` marking the rows that have since been undone. That single column
//! replaces what used to be two `Vec`s: the newest applied row is the next undo,
//! the newest undone row is the next redo. So a mutation recorded before a
//! restart is still undoable after it, which is the point of the table.
//!
//! What does *not* cross a restart is the redo branch: [`UndoHistory::open_session`]
//! drops the undone rows when a library is opened. Undoing something you decided
//! against is a decision you can make again; silently re-applying a change you
//! already backed out of, in a session that has no memory of why, is not
//! something to hand back without asking.
//!
//! Destructive operations that cannot be inverted — purge, empty trash, imports,
//! deleting a tag or collection — are never recorded, and neither is any
//! operation whose undo would need a *file*: the twelve [`Op`] variants are all
//! database state.
//!
//! Rows are dropped rather than guessed at. A row written by a newer build (an
//! unknown `action` slug) or one whose payload no longer parses is deleted at
//! open with a `tracing::warn!` naming its `seq` — the alternative is an undo
//! that half-applies, or a library that fails to open because of its own history.

use rusqlite::{Connection, params};
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

    /// The stable identifier written to `undo_log.action`.
    ///
    /// Separate from [`OpAction::key`] on purpose: that one is a translation
    /// lookup key and renames with the UI, while this one is on disk in rows a
    /// future build has to read. [`OpAction::from_slug`] is the only way back,
    /// and a slug it does not recognise is a row this build cannot describe —
    /// which is dropped at open rather than guessed at.
    pub fn slug(self) -> &'static str {
        match self {
            OpAction::Edit => "edit",
            OpAction::Trash => "trash",
            OpAction::Restore => "restore",
            OpAction::Favorite => "favorite",
            OpAction::Unfavorite => "unfavorite",
            OpAction::Rename => "rename",
            OpAction::TagSet => "tag_set",
            OpAction::TagRenamed => "tag_renamed",
            OpAction::TagColored => "tag_colored",
            OpAction::TagMoved => "tag_moved",
            OpAction::CollectionRenamed => "collection_renamed",
            OpAction::CollectionMoved => "collection_moved",
            OpAction::AddedToCollection => "added_to_collection",
            OpAction::RemovedFromCollection => "removed_from_collection",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "edit" => Some(OpAction::Edit),
            "trash" => Some(OpAction::Trash),
            "restore" => Some(OpAction::Restore),
            "favorite" => Some(OpAction::Favorite),
            "unfavorite" => Some(OpAction::Unfavorite),
            "rename" => Some(OpAction::Rename),
            "tag_set" => Some(OpAction::TagSet),
            "tag_renamed" => Some(OpAction::TagRenamed),
            "tag_colored" => Some(OpAction::TagColored),
            "tag_moved" => Some(OpAction::TagMoved),
            "collection_renamed" => Some(OpAction::CollectionRenamed),
            "collection_moved" => Some(OpAction::CollectionMoved),
            "added_to_collection" => Some(OpAction::AddedToCollection),
            "removed_from_collection" => Some(OpAction::RemovedFromCollection),
            _ => None,
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

/// One asset's two sides of a batch mutation. The id travels *with* both the
/// before and the after value, so a recorded batch cannot describe one set of
/// assets on the way in and a different set on the way back. Two parallel
/// `Vec<(Uuid, T)>` allow exactly that: append to one, forget the other, and
/// undo rewrites a row the forward half never touched — without an error.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Flip<T> {
    pub id: Uuid,
    pub before: T,
    pub after: T,
}

impl<T: Clone> Flip<T> {
    /// The same asset with its two sides exchanged. Turning every flip in a
    /// recorded batch is what undo runs, and its inverse is what redo runs.
    fn swapped(&self) -> Self {
        Self {
            id: self.id,
            before: self.after.clone(),
            after: self.before.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// One invertible metadata mutation.
///
/// Serialized into `undo_log.op` as JSON, so every payload type it names
/// ([`AssetPatch`] and what that holds) is `Serialize`/`Deserialize` too. Field
/// *names* are now on disk: renaming one leaves the old rows unparsable, and the
/// open-time rule for those is drop, not guess.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Op {
    /// Restore every editable column of one asset from the inverse patch
    /// (both patches are fully populated, so undo and redo are symmetric).
    /// Patches are boxed: they dominate the enum's size otherwise.
    PatchAsset {
        id: Uuid,
        before: Box<AssetPatch>,
        after: Box<AssetPatch>,
    },
    /// Per-asset trash flags around a batch flip.
    SetTrashed { flips: Vec<Flip<bool>> },
    /// Per-asset favorite flags around a batch flip.
    SetFavorite { flips: Vec<Flip<bool>> },
    /// One batch title rewrite (multi-select rename).
    SetTitles { flips: Vec<Flip<Option<String>>> },
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
            Op::SetTrashed { flips } => {
                for flip in flips {
                    assets::set_trashed(conn, flip.id, flip.after)?;
                }
            }
            Op::SetFavorite { flips } => {
                for flip in flips {
                    assets::update(
                        conn,
                        flip.id,
                        &AssetPatch {
                            is_favorite: Some(flip.after),
                            ..Default::default()
                        },
                    )?;
                }
            }
            Op::SetTitles { flips } => {
                for flip in flips {
                    assets::update(
                        conn,
                        flip.id,
                        &AssetPatch {
                            title: Some(flip.after.clone()),
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
                    collections::add_asset(
                        conn,
                        crate::model::CollectionId(*collection),
                        crate::model::AssetId(*id),
                    )?;
                }
            }
            Op::MembershipRemove {
                collection,
                removed,
            } => {
                for id in removed {
                    let _ = collections::remove_asset(
                        conn,
                        crate::model::CollectionId(*collection),
                        crate::model::AssetId(*id),
                    );
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
            Op::SetTrashed { flips } => Op::SetTrashed {
                flips: flips.iter().map(Flip::swapped).collect(),
            },
            Op::SetFavorite { flips } => Op::SetFavorite {
                flips: flips.iter().map(Flip::swapped).collect(),
            },
            Op::SetTitles { flips } => Op::SetTitles {
                flips: flips.iter().map(Flip::swapped).collect(),
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

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// The library's mutation history, kept in its own `undo_log` table.
///
/// Stateless apart from the cap: every method reads and writes through the
/// connection it is handed, which is also what lets [`UndoHistory::record`] run
/// inside the mutation's transaction. Holding no state is the reason the history
/// survives a restart — there is nothing in memory to lose.
#[derive(Debug, Clone, Copy)]
pub struct UndoHistory {
    cap: usize,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self::with_cap(DEFAULT_UNDO_CAP)
    }
}

/// One row of `undo_log`, read back for undo and redo.
struct Entry {
    seq: i64,
    op: Op,
}

impl UndoHistory {
    pub fn with_cap(cap: usize) -> Self {
        Self { cap: cap.max(1) }
    }

    /// Record one mutation as the newest step of history.
    ///
    /// **Run this on the transaction that made the mutation**, not after it
    /// committed: a change whose undo row did not land is a change that can
    /// never be taken back, and nothing on screen would say so. The library's
    /// mutation methods therefore open the transaction, apply, and record.
    ///
    /// Recording discards the undone rows first — the linear history the two
    /// stacks used to keep by clearing `redo`, now expressed as one `DELETE`.
    pub fn record(&self, conn: &Connection, op: Op, desc: &OpDesc) -> Result<()> {
        conn.execute("DELETE FROM undo_log WHERE undone_at IS NOT NULL", [])?;
        conn.execute(
            "INSERT INTO undo_log (action, target, count, op, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                desc.action.slug(),
                desc.target,
                desc.count as i64,
                serde_json::to_string(&op)?,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        self.prune(conn)
    }

    /// Apply the most recent step that is currently in effect, and mark it
    /// undone. Returns `false` when nothing is left to undo.
    ///
    /// The inverse and the `undone_at` stamp share one transaction: an inverse
    /// that fails halfway must leave the row in place so the user can retry it,
    /// and an inverse that succeeds must not be undoable twice.
    pub fn undo(&self, conn: &Connection) -> Result<bool> {
        let Some(entry) = self.newest(conn, true)? else {
            return Ok(false);
        };
        let inverse = entry.op.inverse();
        apply_atomic(conn, |tx| {
            inverse.apply(tx)?;
            tx.execute(
                "UPDATE undo_log SET undone_at = ?2 WHERE seq = ?1",
                params![entry.seq, chrono::Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    /// Re-apply the most recently undone step. Returns `false` when there is
    /// none — which, since [`Self::open_session`] clears undone rows, means
    /// nothing has been undone *in this session*.
    pub fn redo(&self, conn: &Connection) -> Result<bool> {
        let Some(entry) = self.newest(conn, false)? else {
            return Ok(false);
        };
        let forward = entry.op.clone();
        apply_atomic(conn, |tx| {
            forward.apply(tx)?;
            tx.execute(
                "UPDATE undo_log SET undone_at = NULL WHERE seq = ?1",
                params![entry.seq],
            )?;
            Ok(())
        })?;
        Ok(true)
    }

    /// Undo up to `steps` entries in sequence, stopping early when the history
    /// runs out or a step fails (the ones already applied stay applied). Returns
    /// how many were undone.
    pub fn undo_steps(&self, steps: usize, conn: &Connection) -> Result<usize> {
        let mut done = 0;
        while done < steps && self.undo(conn)? {
            done += 1;
        }
        Ok(done)
    }

    /// Prepare the history for a new session: drop the rows a previous session
    /// left undone, and drop the rows this build cannot read.
    ///
    /// Called once at library open. Both deletions are the same shape of
    /// decision — a row that cannot be *described* cannot be offered, and a row
    /// that is already inverted belongs to a decision the user has taken back —
    /// but only the second one is a loss, so it is the one that is logged.
    pub fn open_session(&self, conn: &Connection) -> Result<()> {
        let dropped = conn.execute("DELETE FROM undo_log WHERE undone_at IS NOT NULL", [])?;
        if dropped > 0 {
            tracing::debug!(
                dropped,
                "undo history: steps undone before this session were not carried over"
            );
        }
        self.readable_rows(conn)?;
        Ok(())
    }

    /// Steps available to undo, and the newest `n` of their descriptions.
    pub fn undo_len(&self, conn: &Connection) -> usize {
        self.count(conn, true).unwrap_or(0)
    }

    /// Steps available to redo — undone this session, see [`Self::redo`].
    pub fn redo_len(&self, conn: &Connection) -> usize {
        self.count(conn, false).unwrap_or(0)
    }

    pub fn undo_entries(&self, conn: &Connection, n: usize) -> Vec<OpDesc> {
        self.descriptions(conn, n, true).unwrap_or_default()
    }

    pub fn redo_entries(&self, conn: &Connection, n: usize) -> Vec<OpDesc> {
        self.descriptions(conn, n, false).unwrap_or_default()
    }

    /// The next row to act on, decoded.
    ///
    /// The two sides read in opposite orders, and that is the whole difference
    /// between a history and a stack of leftovers. Undo takes the **newest** row
    /// still in effect; redo takes the **oldest** undone one, because undo walked
    /// backwards through `seq` and redo has to walk the same steps forwards —
    /// newest-first there would re-apply the last undo before the one before it
    /// and leave the database in a state that never existed.
    fn newest(&self, conn: &Connection, applied: bool) -> Result<Option<Entry>> {
        let (side, order) = if applied {
            ("IS NULL", "DESC")
        } else {
            ("IS NOT NULL", "ASC")
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT seq, op FROM undo_log WHERE undone_at {side} ORDER BY seq {order} LIMIT 1"
        ))?;
        let found: Option<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .next()
            .and_then(|r| r.ok());
        let Some((seq, blob)) = found else {
            return Ok(None);
        };
        match serde_json::from_str::<Op>(&blob) {
            Ok(op) => Ok(Some(Entry { seq, op })),
            Err(error) => {
                // `open_session` clears unreadable rows, so reaching here means
                // the table changed underneath this session. Leaving the row is
                // the honest answer: deleting it would take undo history away
                // from a user who is watching the panel.
                Err(crate::Error::Validation(format!(
                    "undo step {seq} does not parse: {error}"
                )))
            }
        }
    }

    fn count(&self, conn: &Connection, applied: bool) -> Result<usize> {
        let side = if applied { "IS NULL" } else { "IS NOT NULL" };
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM undo_log WHERE undone_at {side}"),
            [],
            |row| row.get(0),
        )?;
        Ok(n as usize)
    }

    fn descriptions(&self, conn: &Connection, n: usize, applied: bool) -> Result<Vec<OpDesc>> {
        // Undo entries are newest-first (what happened most recently is the line
        // at the top); redo entries are next-first, so the list reads in the
        // order the keys will replay them — see [`UndoHistory::newest`].
        let (side, order) = if applied {
            ("IS NULL", "DESC")
        } else {
            ("IS NOT NULL", "ASC")
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT action, target, count FROM undo_log WHERE undone_at {side} \
             ORDER BY seq {order} LIMIT ?1"
        ))?;
        let rows = stmt.query_map([n as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        // Newest first, which is the order the status bar lists them in.
        let mut out = Vec::new();
        for row in rows {
            let (slug, target, count) = row?;
            // Unreachable after `open_session` cleaned house, and the query has
            // to return *something* for the row: describe it by its count under
            // the most generic verb rather than dropping it silently, so the
            // number in the header still matches the list.
            let action = OpAction::from_slug(&slug).unwrap_or(OpAction::Edit);
            out.push(OpDesc::new(action, target, count as usize));
        }
        Ok(out)
    }

    /// Delete everything unreadable, then keep only the newest `cap` rows.
    fn readable_rows(&self, conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare("SELECT seq, action, op FROM undo_log ORDER BY seq ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut dead: Vec<i64> = Vec::new();
        for row in rows {
            let (seq, slug, blob) = row?;
            let parses = serde_json::from_str::<Op>(&blob).is_ok();
            if !parses || OpAction::from_slug(&slug).is_none() {
                dead.push(seq);
            }
        }
        drop(stmt);
        for seq in &dead {
            tracing::warn!(
                seq,
                "undo history: dropping a step this build cannot read (a newer library, or a changed Op shape)"
            );
            conn.execute("DELETE FROM undo_log WHERE seq = ?1", params![seq])?;
        }
        Ok(())
    }

    fn prune(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "DELETE FROM undo_log WHERE seq NOT IN \
             (SELECT seq FROM undo_log ORDER BY seq DESC LIMIT ?1)",
            params![self.cap as i64],
        )?;
        Ok(())
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

/// Apply one database step atomically: the closure runs inside a
/// transaction that commits on success and rolls back on error, so a
/// half-applied op can never leak into the store.
/// Run one database step inside a transaction of its own: commit on success,
/// roll back on error, and hand the caller's value out on the success path only.
///
/// The generic is what lets a mutation method return what the store reported
/// (`changed`, a count) while the undo row rides in the same transaction — the
/// pair is the contract, and a step that returns before its row does would make
/// an un-recorded mutation look exactly like a recorded one.
pub(crate) fn apply_atomic<T>(
    conn: &Connection,
    step: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    // `unchecked_transaction`: the store hands out a shared `&Connection`, so
    // the checked `&mut`-based API is not reachable here. There is no outer
    // transaction on these paths to conflict with — the mutation methods that
    // wrap themselves in this helper do their recording inside the closure.
    let tx = conn.unchecked_transaction()?;
    let value = match step(&tx) {
        Ok(value) => value,
        Err(error) => {
            let _ = tx.rollback();
            return Err(error);
        }
    };
    tx.commit().map_err(crate::error::Error::from)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::model::{
        Asset, AssetKind, AssetLocation, AssetSeed, NewCollection, NewTag, Placement, UsageStatus,
        now,
    };
    use crate::store::Store;

    fn sample_asset(name: &str, kind: AssetKind) -> Asset {
        let id = Uuid::new_v4();
        Asset::from_seed(AssetSeed {
            id,
            location: AssetLocation::Stored {
                rel_path: format!("media/{}/{}", &id.to_string()[..2], name),
            },
            file_name: name.to_string(),
            ext: "png".into(),
            mime: "image/png".into(),
            size_bytes: 128,
            content_hash: Some(crate::model::ContentHash::from_hasher("a".repeat(64))),
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
            placement: Placement::Live,
        })
    }

    /// Undo runs its inverse inside a transaction, so an inverse that cannot
    /// apply (here: the old tag name has been taken again) rolls back whole
    /// and stays on the stack — the entry is neither lost nor half-applied,
    /// and the chain behind it is intact.
    #[test]
    fn a_failing_undo_stays_on_the_stack_unchanged() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let stack = UndoHistory::default();

        let tag = NewTag {
            name: "a".into(),
            color: None,
            parent_id: None,
        };
        let created = tags::create(conn, &tag).unwrap().id;
        // Forward, recorded the way the library records it: a → x.
        tags::rename(conn, created, "x").unwrap();
        stack
            .record(
                conn,
                Op::TagRename {
                    id: created,
                    before: "a".into(),
                    after: "x".into(),
                },
                &OpDesc::new(OpAction::TagRenamed, Some("a".into()), 1),
            )
            .unwrap();
        // Then someone else takes the name "a" again.
        tags::create(
            conn,
            &NewTag {
                name: "a".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap();

        assert!(stack.undo(conn).is_err(), "the rename back must conflict");
        assert_eq!(stack.undo_len(conn), 1, "the entry is retriable, not lost");
        assert_eq!(stack.redo_len(conn), 0);
        // And the store shows no half of it: the tag is still "x".
        let tags = tags::list(conn).unwrap();
        assert_eq!(tags.iter().filter(|t| t.name == "x").count(), 1);
    }

    #[test]
    fn undo_redo_roundtrips_patch_favorite_and_tags() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let stack = UndoHistory::default();

        let mut a = sample_asset("a.png", AssetKind::Image);
        assets::insert(conn, &a).unwrap();
        a.title = None;

        // Forward: set a title.
        let patch = AssetPatch {
            title: Some(Some("hello".into())),
            ..Default::default()
        };
        stack
            .record(
                conn,
                Op::PatchAsset {
                    id: a.id,
                    before: Box::new(restore_patch(&a)),
                    after: Box::new(patch),
                },
                &OpDesc::counted(OpAction::Edit, 1),
            )
            .unwrap();
        stack.undo(conn).unwrap();
        assert_eq!(assets::get(conn, a.id).unwrap().unwrap().title, None);
        assert_eq!(stack.undo_len(conn), 0);
        stack.redo(conn).unwrap();
        assert_eq!(
            assets::get(conn, a.id).unwrap().unwrap().title,
            Some("hello".into())
        );

        // Forward: favorite flip.
        stack
            .record(
                conn,
                Op::SetFavorite {
                    flips: vec![Flip {
                        id: a.id,
                        before: false,
                        after: true,
                    }],
                },
                &OpDesc::counted(OpAction::Favorite, 1),
            )
            .unwrap();
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
        tags::add_to_asset(
            conn,
            crate::model::AssetId(a.id),
            crate::model::TagId(t1.id),
        )
        .unwrap();
        stack
            .record(
                conn,
                Op::SetTags {
                    asset: a.id,
                    before: vec![t1.id],
                    after: vec![t2.id],
                },
                &OpDesc::counted(OpAction::TagSet, 1),
            )
            .unwrap();
        stack.undo(conn).unwrap();
        assert_eq!(tags::for_asset(conn, a.id).unwrap()[0].id, t1.id);
        stack.redo(conn).unwrap();
        assert_eq!(tags::for_asset(conn, a.id).unwrap()[0].id, t2.id);

        // Recording clears the redo branch (linear history).
        stack
            .record(
                conn,
                Op::SetTags {
                    asset: a.id,
                    before: vec![t2.id],
                    after: vec![],
                },
                &OpDesc::counted(OpAction::TagSet, 1),
            )
            .unwrap();
        assert_eq!(stack.redo_len(conn), 0);
        assert_eq!(stack.undo_len(conn), 4);
    }

    /// A batch flip pairs every asset with *its own* before value, so undoing a
    /// mixed selection puts each row back on the side it came from instead of
    /// flattening the group onto one state.
    #[test]
    fn a_batch_flip_undoes_each_asset_to_its_own_side() {
        fn is_trashed(conn: &Connection, id: Uuid) -> bool {
            assets::get(conn, id)
                .unwrap()
                .unwrap()
                .placement()
                .is_trashed()
        }

        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let stack = UndoHistory::default();

        // Two of three are already in the trash; the batch empties it.
        let a = {
            let mut asset = sample_asset("a.png", AssetKind::Image);
            asset.set_placement(Placement::Trashed(now()));
            asset
        };
        let b = sample_asset("b.png", AssetKind::Image);
        let c = {
            let mut asset = sample_asset("c.png", AssetKind::Image);
            asset.set_placement(Placement::Trashed(now()));
            asset
        };
        for asset in [&a, &b, &c] {
            assets::insert(conn, asset).unwrap();
        }

        let op = Op::SetTrashed {
            flips: vec![
                Flip {
                    id: a.id,
                    before: true,
                    after: false,
                },
                Flip {
                    id: b.id,
                    before: false,
                    after: false,
                },
                Flip {
                    id: c.id,
                    before: true,
                    after: false,
                },
            ],
        };
        // Forward is the caller's move, recorded here so the stack describes
        // what actually happened to the library.
        op.clone().apply(conn).unwrap();
        stack
            .record(conn, op, &OpDesc::counted(OpAction::Restore, 3))
            .unwrap();
        assert!(
            !is_trashed(conn, a.id) && !is_trashed(conn, b.id) && !is_trashed(conn, c.id),
            "the forward half empties the trash"
        );

        stack.undo(conn).unwrap();
        assert!(is_trashed(conn, a.id), "a came from the trash");
        assert!(
            !is_trashed(conn, b.id),
            "b was live on the way in, so undo leaves it live"
        );
        assert!(is_trashed(conn, c.id), "c came from the trash");

        stack.redo(conn).unwrap();
        for id in [a.id, b.id, c.id] {
            assert!(!is_trashed(conn, id), "redo takes the whole batch");
        }
    }

    #[test]
    fn undo_redo_collection_membership_and_move() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let stack = UndoHistory::default();

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
        stack
            .record(conn, op, &OpDesc::counted(OpAction::Edit, 1))
            .unwrap();
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
        stack
            .record(conn, op, &OpDesc::counted(OpAction::Edit, 1))
            .unwrap();
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
        tags::add_to_asset(
            conn,
            crate::model::AssetId(a.id),
            crate::model::TagId(tag.id),
        )
        .unwrap();
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

        let stack = UndoHistory::with_cap(3);
        for i in 0..5 {
            stack
                .record(
                    conn,
                    Op::SetFavorite {
                        flips: vec![Flip {
                            id: a.id,
                            before: i % 2 == 0,
                            after: i % 2 == 1,
                        }],
                    },
                    &OpDesc::new(OpAction::Favorite, Some(format!("f{i}.png")), 1),
                )
                .unwrap();
        }
        // Only the newest three survive the cap.
        assert_eq!(stack.undo_len(conn), 3);
        let entries = stack.undo_entries(conn, 3);
        assert_eq!(entries[0].target.as_deref(), Some("f4.png"));
        assert_eq!(entries[2].target.as_deref(), Some("f2.png"));

        // Undoing flips favorite twice and the redo side describes next-first.
        assert_eq!(stack.undo_steps(2, conn).unwrap(), 2);
        assert_eq!(stack.redo_len(conn), 2);
        assert_eq!(
            stack.redo_entries(conn, 2)[0].target.as_deref(),
            Some("f3.png")
        );

        // undo_steps stops at the empty stack instead of erroring.
        assert_eq!(stack.undo_steps(10, conn).unwrap(), 1);
        assert_eq!(stack.undo_len(conn), 0);
    }

    /// A new step throws the undone ones away.
    ///
    /// This is the `redo.clear()` of the two-stack history, and it is still the
    /// right rule now that the history is a table: branching history would let a
    /// redo land *beside* a later change rather than before it, and the database
    /// would end up in a state that never happened.
    #[test]
    fn a_new_step_discards_the_undone_ones() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let tag = tags::create(
            conn,
            &NewTag {
                name: "keep".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap()
        .id;
        let stack = UndoHistory::default();
        let rename = |before: &str, after: &str| Op::TagRename {
            id: tag,
            before: before.into(),
            after: after.into(),
        };
        let desc = OpDesc::new(OpAction::TagRenamed, Some("keep".into()), 1);

        stack.record(conn, rename("a", "b"), &desc).unwrap();
        stack.record(conn, rename("b", "c"), &desc).unwrap();
        assert!(stack.undo(conn).unwrap(), "the newest step is undoable");
        assert_eq!(stack.redo_len(conn), 1);

        // Recording past an undone step closes that branch.
        stack.record(conn, rename("c", "d"), &desc).unwrap();
        assert_eq!(stack.redo_len(conn), 0, "the redo side is gone");
        assert_eq!(stack.undo_len(conn), 2);
    }

    /// What a restart carries over, and what it deliberately does not.
    #[test]
    fn a_restart_carries_undo_but_not_redo() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let tag = tags::create(
            conn,
            &NewTag {
                name: "one".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap()
        .id;
        let stack = UndoHistory::default();
        let desc = OpDesc::new(OpAction::TagRenamed, Some("one".into()), 1);
        stack
            .record(
                conn,
                Op::TagRename {
                    id: tag,
                    before: "one".into(),
                    after: "two".into(),
                },
                &desc,
            )
            .unwrap();
        tags::rename(conn, tag, "two").unwrap();
        assert!(stack.undo(conn).unwrap());
        assert_eq!(stack.redo_len(conn), 1, "undoable in this session");

        // "Reopening" is a fresh history object over the same rows, then the
        // open-time pass.
        let stack = UndoHistory::default();
        stack.open_session(conn).unwrap();
        assert_eq!(stack.undo_len(conn), 0, "the applied side is empty again");
        assert_eq!(
            stack.redo_len(conn),
            0,
            "a step the previous session backed out of is not offered back"
        );
        // And the database says the same: the tag is back to its own name.
        assert_eq!(tags::get(conn, tag).unwrap().unwrap().name, "one");
    }

    /// A row this build cannot read is dropped at open, loudly, and the rest of
    /// the history survives.
    ///
    /// Two ways to get there and one rule for both: the `action` slug is not one
    /// this build knows (a row written by a newer Trove), or `op` is not a `Op`
    /// any more. Guessing either one means applying a mutation nobody described —
    /// and the alternative, refusing to open, would make the history table able to
    /// brick a library.
    #[test]
    fn an_unreadable_step_is_dropped_at_open_rather_than_guessed() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let tag = tags::create(
            conn,
            &NewTag {
                name: "fine".into(),
                color: None,
                parent_id: None,
            },
        )
        .unwrap()
        .id;
        let desc = OpDesc::new(OpAction::TagRenamed, Some("fine".into()), 1);
        let good = Op::TagRename {
            id: tag,
            before: "fine".into(),
            after: "better".into(),
        };
        UndoHistory::default().record(conn, good, &desc).unwrap();
        for (action, op) in [
            ("renamed_by_a_future_build", "{\"x\":1}"),
            ("edit", "not even json"),
        ] {
            conn.execute(
                "INSERT INTO undo_log (action, target, count, op, created_at) \
                 VALUES (?1, NULL, 1, ?2, 'then')",
                params![action, op],
            )
            .unwrap();
        }
        assert_eq!(
            crate::store::rows::query_count(conn, "SELECT COUNT(*) FROM undo_log", vec![]).unwrap(),
            3
        );

        UndoHistory::default().open_session(conn).unwrap();
        let left = UndoHistory::default();
        assert_eq!(left.undo_len(conn), 1, "the readable step is still there");
        assert_eq!(
            left.undo_entries(conn, 5)[0].action,
            OpAction::TagRenamed,
            "and it is still described by its own action"
        );
    }
}
