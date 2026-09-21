//! The persistent hash cache: what a file hashed to, remembered across runs.
//!
//! ## Why
//!
//! Hashing is the one cost an import cannot avoid — a linked import reads the
//! file, and that read *is* the import. The watch job makes it worse by
//! design: a file it offered once and was not acknowledged comes back on the
//! next sweep, and the sweep runs over the whole root for as long as the
//! process lives. Without a memory, every one of those re-offers pays for the
//! same read again.
//!
//! This module is that memory. It answers one question — "what did this file
//! hash to?" — from a key that costs a `stat` to build, and it survives a
//! restart, because a library that is watched all day should only pay the
//! first time.
//!
//! ## What it keys on
//!
//! A file's identity here is `(path, size, mtime)` with the mtime in
//! nanoseconds. That is the same assumption every build system makes: a file
//! whose length and modification time are both unchanged has not been
//! rewritten. The assumption can be broken deliberately (write, then restore
//! both), which is why the cache is consulted as a *pre-check* — the entry is
//! a reason to skip work, never a reason to overwrite a hash measured now.
//!
//! ## Two indexes
//!
//! - **by path**: this exact file hashed to this digest → the whole read is
//!   skipped.
//! - **by fingerprint**: content whose cheap sample
//!   ([`super::hash::fingerprint`]) looks like this hashed to this digest →
//!   the read is skipped for a file we have never seen but whose sample
//!   matches one we have. That is what turns "drop the same folder twice
//!   under two names" and "the inbox sweep re-offers a file" into a stat.
//!
//! Both live in one JSON file inside the library's cache root, next to the
//! thumbnails — travel with the library, not with the database, because the
//! digests in it are also the thumbnail file names.
//!
//! ## Failure mode
//!
//! Every operation degrades to "cache miss". An unreadable or corrupt file
//! reads as empty; a failed write is logged and ignored. The cost of being
//! wrong here is one file being hashed again, never a wrong hash being
//! recorded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// Where the cache lives inside the cache root.
const FILE_NAME: &str = "hash-cache.json";

/// Schema of the file itself. Bumped when the layout changes, at which point
/// an older file is discarded rather than migrated: everything in it can be
/// rebuilt by hashing.
const VERSION: u32 = 1;

/// Entries kept per index. A library of a million files would otherwise
/// rewrite a hundred megabytes of JSON on every import; the cap keeps the
/// file a few megabytes and the *recently used* half of a big library
/// cached. Entries beyond it are dropped least-recently-used first.
const MAX_ENTRIES: usize = 200_000;

/// Handles for the loaded caches, one per cache root. A process opens one
/// library at a time, so this is one in practice; keyed by root so a library
/// swap (and every test) reloads instead of mixing two libraries' hashes.
fn registry() -> &'static Mutex<Option<Loaded>> {
    static REGISTRY: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// The loaded state of one cache root.
struct Loaded {
    root: PathBuf,
    /// path → what it hashed to.
    by_path: HashMap<String, Entry>,
    /// (size, fingerprint) → digest.
    by_fingerprint: HashMap<(u64, String), String>,
    /// Changed since the last [`flush`].
    dirty: bool,
    /// Monotone counter, stamped on every hit, so eviction can drop the
    /// entries nobody has asked for lately.
    tick: u64,
}

/// One file's remembered hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// The file this describes.
    path: String,
    /// File size at the time of the hash.
    size: u64,
    /// Modification time, nanoseconds since the epoch.
    mtime: i64,
    /// The BLAKE3 content hash.
    hash: String,
    /// The cheap sample of the same content (see [`super::hash::fingerprint`]).
    fingerprint: String,
    /// Last lookup, for eviction.
    #[serde(default)]
    used: u64,
}

/// On-disk shape.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    /// One record per known file. A list rather than a map: arbitrary string
    /// keys are legal JSON, but a list of records is what a human can read
    /// and diff.
    entries: Vec<Entry>,
    /// `(size, fingerprint) → hash`, flattened the same way.
    fingerprints: Vec<StoredFingerprint>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredFingerprint {
    size: u64,
    fingerprint: String,
    hash: String,
}

/// A file's `(size, mtime_ns)`, the cheap key this cache is built on. `None`
/// when the file cannot be stat'ed or its timestamp is not representable —
/// in which case the caller must hash it.
pub fn stamp(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((meta.len(), mtime.as_nanos() as i64))
}

/// What this file hashed to, when nothing about it has changed since.
///
/// `size` and `mtime` come from [`stamp`]. A hit means the bytes are the same
/// bytes that were hashed, so the caller can record `hash` without reading
/// the file at all.
pub fn lookup(cache_root: &Path, path: &Path, size: u64, mtime: i64) -> Option<String> {
    let key = path.to_string_lossy().into_owned();
    with(cache_root, |cache| {
        let entry = cache.by_path.get_mut(&key)?;
        if entry.size != size || entry.mtime != mtime {
            return None;
        }
        cache.tick += 1;
        entry.used = cache.tick;
        Some(entry.hash.clone())
    })
    .flatten()
}
/// The digest some *other* file with this sample hashed to, if any. The
/// caller is expected to have used [`super::hash::fingerprint`].
pub fn hash_for_fingerprint(cache_root: &Path, size: u64, fingerprint: &str) -> Option<String> {
    with(cache_root, |cache| {
        cache
            .by_fingerprint
            .get(&(size, fingerprint.to_string()))
            .cloned()
    })
    .flatten()
}

/// Whether anything is remembered about this path *at all* — whether or not
/// the file still matches what was remembered.
///
/// The difference from [`lookup`] is the difference between "we know what
/// this file is" and "we know what this file *was*". A caller that only has a
/// loose fallback (a name-and-size match, say) needs the second question: an
/// entry that no longer matches means the file changed, and a fallback must
/// not then swallow it.
pub fn knows_path(cache_root: &Path, path: &Path) -> bool {
    let key = path.to_string_lossy().into_owned();
    with(cache_root, |cache| cache.by_path.contains_key(&key)).unwrap_or(false)
}

/// Remember what a file hashed to. Called once per newly read file.
///
/// An empty `hash` is ignored — a caller that failed to hash must not leave a
/// "known" entry that would skip the file forever. An empty `fingerprint` is
/// allowed and means "this file needs no sample entry": the caller is telling
/// us its sample was already the whole file, so indexing the sample next to
/// the path would only store the same digest twice.
pub fn record(
    cache_root: &Path,
    path: &Path,
    size: u64,
    mtime: i64,
    hash: &str,
    fingerprint: &str,
) {
    if hash.is_empty() {
        return;
    }
    with(cache_root, |cache| {
        cache.tick += 1;
        let used = cache.tick;
        let key = path.to_string_lossy().into_owned();
        cache.by_path.insert(
            key.clone(),
            Entry {
                path: key,
                size,
                mtime,
                hash: hash.to_string(),
                fingerprint: fingerprint.to_string(),
                used,
            },
        );
        if !fingerprint.is_empty() {
            cache
                .by_fingerprint
                .insert((size, fingerprint.to_string()), hash.to_string());
        }
        cache.dirty = true;
    });
}

/// Write the cache back, if anything changed since the last call. Called at
/// the end of an import run rather than per file: the file is rewritten
/// whole, and one write per job keeps that off the hot path.
pub fn flush(cache_root: &Path) {
    with(cache_root, persist);
}

/// Write one loaded cache to its own root, when it has pending changes.
fn persist(cache: &mut Loaded) {
    if !cache.dirty {
        return;
    }
    let stored = cache.to_stored();
    cache.dirty = false;
    let path = cache.root.join(FILE_NAME);
    match write_atomically(&path, &stored) {
        Ok(()) => tracing::debug!(entries = stored.entries.len(), "hash cache written"),
        Err(error) => tracing::warn!(%error, "could not write the hash cache"),
    }
}

/// Drop everything remembered for this root, on disk and in memory. For
/// tests, and for a caller that knows the whole file set was rewritten.
pub fn clear(cache_root: &Path) {
    with(cache_root, |cache| {
        cache.by_path.clear();
        cache.by_fingerprint.clear();
        cache.dirty = true;
    });
    let _ = std::fs::remove_file(cache_root.join(FILE_NAME));
}

/// How many files this root remembers. Diagnostics and tests.
pub fn len(cache_root: &Path) -> usize {
    with(cache_root, |cache| cache.by_path.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// Run `f` against the loaded cache for `cache_root`, loading (or reloading,
/// if the root changed) first. `None` only if the lock was poisoned, in which
/// case the caller treats it as a miss.
///
/// A root change *persists* what the outgoing cache still had pending rather
/// than dropping it on the floor: one process opens one library at a time in
/// practice, but tests — and any future caller holding two libraries — would
/// otherwise lose every entry recorded since the last flush, silently.
fn with<T>(cache_root: &Path, f: impl FnOnce(&mut Loaded) -> T) -> Option<T> {
    let mut guard = registry().lock().unwrap_or_else(|e| e.into_inner());
    let stale = guard
        .as_ref()
        .is_none_or(|loaded| loaded.root != cache_root);
    if stale {
        if let Some(mut outgoing) = guard.take() {
            persist(&mut outgoing);
        }
        *guard = load(cache_root);
    }
    guard.as_mut().map(f)
}

/// Read the file for `cache_root`, or start empty.
fn load(cache_root: &Path) -> Option<Loaded> {
    let path = cache_root.join(FILE_NAME);
    let stored: Stored = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "hash cache unreadable; starting empty");
            Stored::default()
        }),
        Err(_) => Stored::default(),
    };
    if stored.version != VERSION {
        // A cache from another layout: everything in it is rebuildable, so
        // discarding it is cheaper than migrating it.
        return Some(Loaded::empty(cache_root.to_path_buf()));
    }

    let mut cache = Loaded::empty(cache_root.to_path_buf());
    for entry in stored.entries {
        cache.tick += 1;
        cache.tick = cache.tick.max(entry.used);
        cache.by_path.insert(entry.path.clone(), entry);
    }
    for row in stored.fingerprints {
        cache
            .by_fingerprint
            .insert((row.size, row.fingerprint), row.hash);
    }
    cache.dirty = false;
    Some(cache)
}

impl Loaded {
    fn empty(root: PathBuf) -> Self {
        Self {
            root,
            by_path: HashMap::new(),
            by_fingerprint: HashMap::new(),
            dirty: false,
            tick: 0,
        }
    }

    /// The cache as it will be written, capped to the newest [`MAX_ENTRIES`]
    /// files.
    ///
    /// The fingerprint index is written as the samples of the files that
    /// survived the cap, so the two indexes never disagree about what is
    /// known: a sample whose file was evicted is a sample nobody can act on.
    fn to_stored(&self) -> Stored {
        let mut entries: Vec<Entry> = self.by_path.values().cloned().collect();
        if entries.len() > MAX_ENTRIES {
            entries.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.used));
            entries.truncate(MAX_ENTRIES);
        }
        let live: std::collections::HashSet<(u64, &str)> = entries
            .iter()
            .map(|entry| (entry.size, entry.fingerprint.as_str()))
            .collect();
        let mut fingerprints: Vec<StoredFingerprint> = self
            .by_fingerprint
            .iter()
            .filter(|((size, fingerprint), _)| live.contains(&(*size, fingerprint.as_str())))
            .map(|((size, fingerprint), hash)| StoredFingerprint {
                size: *size,
                fingerprint: fingerprint.clone(),
                hash: hash.clone(),
            })
            .collect();
        fingerprints.sort_unstable_by_key(|row| std::cmp::Reverse(row.size));
        fingerprints.truncate(MAX_ENTRIES);
        Stored {
            version: VERSION,
            entries,
            fingerprints,
        }
    }
}

/// Write `stored` to `path` through a temporary sibling and a rename, so a
/// crash mid-write cannot leave a half-written cache behind.
fn write_atomically(path: &Path, stored: &Stored) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(stored)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "trove-hashcache-{name}-{}-{}",
            std::process::id(),
            crate::model::new_id().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A recorded file is answered from the stat alone, and only while its
    /// size and mtime both still match.
    #[test]
    fn a_recorded_file_is_remembered_until_it_changes() {
        let root = temp_root("roundtrip");
        let file = root.join("photo.bin");
        std::fs::write(&file, b"first").unwrap();
        let (size, mtime) = stamp(&file).unwrap();

        assert_eq!(lookup(&root, &file, size, mtime), None, "nothing recorded");

        record(&root, &file, size, mtime, "digest-a", "sample-a");
        assert_eq!(
            lookup(&root, &file, size, mtime).as_deref(),
            Some("digest-a")
        );

        // A rewrite moves the mtime, so the entry stops matching.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&file, b"second").unwrap();
        let (size2, mtime2) = stamp(&file).unwrap();
        assert_eq!(lookup(&root, &file, size2, mtime2), None);

        // A change that keeps the mtime but not the size is rejected too.
        assert_eq!(lookup(&root, &file, size2 + 1, mtime2), None);

        std::fs::remove_dir_all(&root).ok();
    }

    /// The cache survives a flush and a reload: that is the whole point of it
    /// being a file rather than a map.
    #[test]
    fn the_cache_survives_a_reload() {
        let root = temp_root("persist");
        let file = root.join("clip.bin");
        std::fs::write(&file, b"payload").unwrap();
        let (size, mtime) = stamp(&file).unwrap();
        record(&root, &file, size, mtime, "digest-b", "sample-b");
        flush(&root);
        assert!(root.join(FILE_NAME).is_file());

        // Point the registry at a different root, then come back: the state
        // has to be re-read from disk, not kept in memory.
        let other = temp_root("persist-other");
        assert_eq!(len(&other), 0);
        assert_eq!(
            lookup(&root, &file, size, mtime).as_deref(),
            Some("digest-b")
        );
        assert_eq!(len(&root), 1);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// The second index answers for a file we have never seen, which is what
    /// makes a re-dropped folder (or a re-offered inbox file) cheap.
    #[test]
    fn a_fingerprint_match_answers_for_an_unseen_path() {
        let root = temp_root("fingerprint");
        let seen = root.join("original.bin");
        std::fs::write(&seen, b"same content").unwrap();
        let (size, mtime) = stamp(&seen).unwrap();
        record(&root, &seen, size, mtime, "digest-c", "sample-c");

        assert_eq!(
            hash_for_fingerprint(&root, size, "sample-c").as_deref(),
            Some("digest-c")
        );
        // A different sample or a different length is a miss.
        assert_eq!(hash_for_fingerprint(&root, size, "sample-other"), None);
        assert_eq!(hash_for_fingerprint(&root, size + 1, "sample-c"), None);
        // A different path is a miss on the path index even with a match on
        // the fingerprint: the two indexes answer different questions.
        let unseen = root.join("copy.bin");
        assert_eq!(lookup(&root, &unseen, size, mtime), None);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A corrupt or foreign cache reads as empty instead of failing: the cost
    /// of losing it is one file being hashed twice.
    #[test]
    fn a_corrupt_cache_reads_as_empty() {
        let root = temp_root("corrupt");
        let file = root.join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let (size, mtime) = stamp(&file).unwrap();
        record(&root, &file, size, mtime, "digest-d", "sample-d");
        flush(&root);

        std::fs::write(root.join(FILE_NAME), b"{ not json").unwrap();
        // A different root forces a reload of this one.
        let other = temp_root("corrupt-other");
        assert_eq!(len(&other), 0);
        assert_eq!(lookup(&root, &file, size, mtime), None);

        // A cache written by another layout is discarded, not misread.
        std::fs::write(
            root.join(FILE_NAME),
            br#"{"version":99,"entries":[],"fingerprints":[]}"#,
        )
        .unwrap();
        assert_eq!(len(&other), 0);
        assert_eq!(len(&root), 0);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// An empty digest is never recorded: a caller that failed to hash must
    /// not leave a "known" entry that would skip the file forever.
    #[test]
    fn an_empty_digest_is_not_recorded() {
        let root = temp_root("empty");
        let file = root.join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let (size, mtime) = stamp(&file).unwrap();
        record(&root, &file, size, mtime, "", "sample-e");
        assert_eq!(lookup(&root, &file, size, mtime), None);
        assert_eq!(hash_for_fingerprint(&root, size, "sample-e"), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stamp_refuses_directories_and_missing_files() {
        let root = temp_root("stamp");
        assert_eq!(stamp(&root), None, "a directory has no content hash");
        assert_eq!(stamp(&root.join("gone.bin")), None);
        let file = root.join("real.bin");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(stamp(&file).unwrap().0, 1);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Eviction keeps the newest entries when a cache overflows its cap.
    #[test]
    fn a_cache_over_its_cap_keeps_the_newest_entries() {
        let root = temp_root("cap");
        let mut cache = Loaded::empty(root.clone());
        let over = MAX_ENTRIES + 10;
        for i in 0..over {
            cache.tick += 1;
            let digest = format!("{i:064x}");
            cache.by_path.insert(
                format!("/fake/{i}.bin"),
                Entry {
                    path: format!("/fake/{i}.bin"),
                    size: i as u64,
                    mtime: 0,
                    hash: digest.clone(),
                    fingerprint: digest.clone(),
                    used: cache.tick,
                },
            );
            cache
                .by_fingerprint
                .insert((i as u64, digest.clone()), digest);
        }
        let stored = cache.to_stored();
        assert_eq!(stored.entries.len(), MAX_ENTRIES);
        // The newest survived, the oldest did not.
        assert!(
            stored
                .entries
                .iter()
                .any(|e| e.path == format!("/fake/{}.bin", over - 1))
        );
        assert!(!stored.entries.iter().any(|e| e.path == "/fake/0.bin"));
        // The fingerprint index never describes more than the files known.
        assert_eq!(stored.fingerprints.len(), MAX_ENTRIES);
        std::fs::remove_dir_all(&root).ok();
    }
}
