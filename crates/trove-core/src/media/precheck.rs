//! The cheap dedup pre-check: which of these paths the library already holds,
//! answered *without reading a byte*.
//!
//! ## Where this sits
//!
//! An import has three gates a file can fail, in increasing order of cost:
//!
//! 1. **This module** — nothing is read. A stat against the remembered
//!    `(path, size, mtime)` and a lookup in the library's name/size set.
//! 2. **The hash stage** — the file is read once, or its cheap sample is
//!    (see [`super::hash::fingerprint`]); the digest decides whether the
//!    content is already stored, and that decision is exact.
//! 3. **The commit** — the row is deduped against live assets by content
//!    hash, which is what makes gate 2's answer final.
//!
//! The gates are ordered so that the *cheap* and the *exact* check are not
//! competitors: this gate only ever drops a file it can prove is already
//! there, and everything it lets through is still deduped properly.
//!
//! ## Why it earns its keep
//!
//! The watch job re-offers whatever the embedder has not acknowledged, and
//! the collect inbox keeps its files forever (they are linked, not copied),
//! so its sweep runs over the whole of its history on every wake-up. Both
//! end up handing the same paths to the importer over and over. Without this
//! gate each of those repeats pays for a full read of the file; with it, a
//! repeat costs one `stat` plus one hash-map lookup.
//!
//! ## What it will not do
//!
//! It never *deletes* or repairs anything, and it never decides that a file
//! is new — "not known" is not an answer it gives. A file it cannot stat is
//! left to the pipeline, which reports it as a skip in due course.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::path::Path;

use rusqlite::Connection;

use super::hash_cache;
use crate::store::assets;

/// What the library holds, in the two shapes this gate asks in.
///
/// The name/size index is read up front: a cold cache asks it about every
/// candidate, and it is the cheap one (`(String, u64)` per live record). The
/// content-hash set is built **on the first cache hit** instead — a run over
/// files nothing is remembered about never asks it at all, and on a large
/// library it is the bigger of the two by an order of magnitude (a 64-byte
/// digest per record, versus a name and an integer). Lazily, the import that
/// actually needs it pays for it, and the one that does not never does.
pub struct Held<'a> {
    /// The library to ask.
    conn: &'a Connection,
    /// `(file name, size)` of every *live* record — the fallback, for a file
    /// nothing is remembered about yet.
    keys: HashSet<(String, u64)>,
    /// Every content hash any record references, live or trashed. Trashed
    /// rows are included on purpose: a digest that only a trashed asset
    /// references still has a thumbnail on disk, and re-importing that
    /// content is the commit-side dedup's decision to make, not this gate's.
    hashes: OnceCell<HashSet<String>>,
}

impl<'a> Held<'a> {
    /// Read the name/size index; the hash set comes when something asks.
    pub fn load(conn: &'a Connection) -> Self {
        Self {
            conn,
            keys: assets::known_keys(conn),
            hashes: OnceCell::new(),
        }
    }

    /// The content hashes the library references, read on first use.
    fn hashes(&self) -> &HashSet<String> {
        self.hashes.get_or_init(|| {
            assets::referenced_hashes(self.conn)
                .unwrap_or_default()
                .into_iter()
                .collect()
        })
    }

    /// Whether `path` is already in the library, decided without reading it.
    ///
    /// Two ways to know, and the order matters:
    ///
    /// - **The hash cache remembers this exact file.** The remembered digest
    ///   is the file's content (see [`hash_cache`]), so if any record
    ///   references it, the library already has this file — no read, and no
    ///   guessing. This branch is *authoritative*: when there is an entry,
    ///   nothing below it is consulted, so a file that changed in place is
    ///   never waved through by a stale name/size match.
    /// - **Nothing is remembered about the path, and a live record has its
    ///   name and size.** Loose on purpose (see [`assets::known_key`]): it is
    ///   what makes the first import after a cache loss cheap, at the price of
    ///   skipping a same-name same-size file the user edited. The
    ///   "nothing remembered" half is what keeps the price paid once instead
    ///   of forever: two consecutive imports of the same folder leave entries
    ///   behind, and from then on the exact branch above answers.
    ///
    /// An entry that exists but no longer *matches* (the file was rewritten:
    /// the mtime moved) is a **no**: the cache is telling us this file
    /// changed, and a changed file is one the pipeline has to look at.
    pub fn holds(&self, path: &Path, cache_root: &Path) -> bool {
        let stamp = hash_cache::stamp(path);
        if let Some((size, mtime)) = stamp {
            if let Some(hash) = hash_cache::lookup(cache_root, path, size, mtime) {
                return self.hashes().contains(&hash);
            }
            if hash_cache::knows_path(cache_root, path) {
                return false;
            }
        }
        match (stamp, path.file_name().and_then(|name| name.to_str())) {
            (Some((size, _)), Some(name)) => self.keys.contains(&(name.to_string(), size)),
            _ => false,
        }
    }

    /// Split `paths` into "already held" and "still to import", preserving
    /// the order of the latter. One pass, no I/O beyond the stat each path
    /// needs to build its key.
    pub fn partition(
        &self,
        paths: &[std::path::PathBuf],
        cache_root: &Path,
    ) -> (Vec<std::path::PathBuf>, u64) {
        let mut held = 0u64;
        let mut rest = Vec::with_capacity(paths.len());
        for path in paths {
            if self.holds(path, cache_root) {
                held += 1;
            } else {
                rest.push(path.clone());
            }
        }
        (rest, held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssetKind, test_asset};
    use crate::store::Store;
    use uuid::Uuid;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "trove-precheck-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_file_the_library_holds_by_name_and_size_is_skipped() {
        let store = Store::in_memory().unwrap();
        let root = temp_root("identity");
        let held = root.join("shot.png");
        std::fs::write(&held, b"0123456789").unwrap();
        let fresh = root.join("new.png");
        std::fs::write(&fresh, b"xy").unwrap();

        let mut asset = test_asset("shot.png", AssetKind::Image, Uuid::new_v4());
        asset.size_bytes = 10;
        assets::insert(store.conn(), &asset).unwrap();

        let held_set = Held::load(store.conn());
        assert!(held_set.holds(&held, &root));
        assert!(!held_set.holds(&fresh, &root));

        let (rest, count) = held_set.partition(&[held.clone(), fresh.clone()], &root);
        assert_eq!(rest, vec![fresh]);
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&root).ok();
    }

    /// The remembered digest is what makes the answer exact: when the cache
    /// knows this file, a name/size match against an unrelated record does
    /// not wave it through.
    #[test]
    fn a_remembered_file_is_judged_by_its_content_not_by_its_name() {
        let store = Store::in_memory().unwrap();
        let root = temp_root("remembered");

        // A record whose name and size match the candidate, but with a
        // different content hash.
        let file = root.join("photo.png");
        std::fs::write(&file, b"0123456789").unwrap();
        let (size, mtime) = hash_cache::stamp(&file).unwrap();
        let mut asset = test_asset("photo.png", AssetKind::Image, Uuid::new_v4());
        asset.size_bytes = size;
        asset.content_hash = Some("b".repeat(64));
        assets::insert(store.conn(), &asset).unwrap();

        let held = Held::load(store.conn());
        // Nothing remembered yet: the name/size fallback applies.
        assert!(held.holds(&file, &root));

        // The cache remembers this file as content the library does *not*
        // hold, so the fallback must not apply anymore.
        hash_cache::record(&root, &file, size, mtime, &"c".repeat(64), "sample");
        assert!(
            !held.holds(&file, &root),
            "a remembered file is judged by what it hashed to"
        );

        // And once that content is in the library, it is held again.
        hash_cache::record(&root, &file, size, mtime, &"b".repeat(64), "sample");
        assert!(held.holds(&file, &root));

        hash_cache::clear(&root);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A file that changed since the last import is not held, even though a
    /// live record has its name and size: once something *is* remembered
    /// about a path, the loose fallback stops applying to it.
    #[test]
    fn a_file_that_changed_since_it_was_remembered_is_not_held() {
        let store = Store::in_memory().unwrap();
        let root = temp_root("changed");
        let file = root.join("photo.png");
        std::fs::write(&file, b"0123456789").unwrap();

        let mut asset = test_asset("photo.png", AssetKind::Image, Uuid::new_v4());
        asset.size_bytes = 10;
        asset.content_hash = Some("d".repeat(64));
        assets::insert(store.conn(), &asset).unwrap();

        let held = Held::load(store.conn());
        // Nothing remembered yet: name and size are all we have, so it counts
        // as held (the price of the fallback, paid once).
        assert!(held.holds(&file, &root));

        // Now it is remembered as content the library holds...
        let (size, mtime) = hash_cache::stamp(&file).unwrap();
        hash_cache::record(&root, &file, size, mtime, &"d".repeat(64), "sample");
        assert!(held.holds(&file, &root));

        // ...and then it is rewritten. Same length, so only the mtime says so;
        // the entry stops matching, and the fallback must not step back in.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&file, b"0123456789").unwrap();
        assert_eq!(
            hash_cache::lookup(&root, &file, size, hash_cache::stamp(&file).unwrap().1),
            None
        );
        assert!(
            !held.holds(&file, &root),
            "a changed file has to reach the pipeline"
        );

        hash_cache::clear(&root);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A file that cannot be stat'ed is not "held": the pipeline reports it.
    #[test]
    fn an_unstattable_file_is_left_to_the_pipeline() {
        let store = Store::in_memory().unwrap();
        let root = temp_root("unstattable");
        let held = Held::load(store.conn());
        assert!(!held.holds(Path::new("/nonexistent/trove/shot.png"), &root));
        std::fs::remove_dir_all(&root).ok();
    }

    /// An empty library holds nothing, and the gate must not invent a "no"
    /// for a file whose content it cannot know.
    #[test]
    fn an_empty_library_holds_nothing() {
        let store = Store::in_memory().unwrap();
        let root = temp_root("empty");
        let file = root.join("a.png");
        std::fs::write(&file, b"x").unwrap();
        let held = Held::load(store.conn());
        assert!(!held.holds(&file, &root));
        let (rest, count) = held.partition(std::slice::from_ref(&file), &root);
        assert_eq!(rest, vec![file]);
        assert_eq!(count, 0);
        std::fs::remove_dir_all(&root).ok();
    }
}
