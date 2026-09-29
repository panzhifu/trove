//! Library backups: consistent snapshots of `library.db` via SQLite's
//! [`VACUUM INTO`](https://www.sqlite.org/lang_vacuum.html) (safe even while
//! other statements are running), and a restore that writes a snapshot back
//! through the same connection the library is open on.
//!
//! Snapshots live in `<library root>/backups/` and roll on two axes:
//! auto-backup is throttled to one per day (taken at library open), and the
//! directory is pruned to the newest [`MAX_BACKUPS`] files after every write.
//!
//! A snapshot is the **database only**. Blobs live beside it under the library
//! root, thumbnails and the text index in the cache directory, so a restore
//! brings back records — not files. Anything deleted from disk after the
//! snapshot was taken stays deleted, and a restored record pointing at it reads
//! as a missing file, which is the state the integrity check already reports.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::OpenFlags;

use crate::error::{Error, Result};

/// How many snapshot files to keep in `backups/`.
pub const MAX_BACKUPS: usize = 10;

/// Auto-backup cadence: a snapshot is taken at open only when the newest
/// existing one is older than this.
const AUTO_BACKUP_INTERVAL: chrono::Duration = chrono::Duration::hours(24);

/// The directory holding backup snapshots.
pub fn backups_dir(root: &Path) -> PathBuf {
    root.join("backups")
}

/// Existing snapshots, oldest first.
pub fn list_backups(root: &Path) -> Vec<PathBuf> {
    let dir = backups_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "db"))
        .collect();
    files.sort();
    files
}

/// Write a fresh snapshot and prune old ones. Returns the snapshot path.
pub fn create_backup(root: &Path, conn: &rusqlite::Connection) -> Result<PathBuf> {
    let dir = backups_dir(root);
    std::fs::create_dir_all(&dir)?;
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    // A same-second re-run would collide with the existing snapshot name;
    // disambiguate with a sequence suffix instead of failing.
    let mut path = dir.join(format!("library-{stamp}.db"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("library-{stamp}-{n}.db"));
    }
    conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])
        .map_err(|e| Error::Db(format!("backup failed: {e}")))?;
    prune_backups(root)?;
    Ok(path)
}

/// The schema version a snapshot carries, and whether it is a library database
/// at all.
///
/// Read-only, and the gate in front of [`restore_backup`]: a truncated file, a
/// path that is not a database, or a snapshot written by a *newer* Trove would
/// each otherwise be discovered only after the live library had been
/// overwritten — and the last of those leaves a library this build refuses to
/// open, which is the one outcome a rescue feature must not produce.
fn snapshot_version(snapshot: &Path) -> Result<i64> {
    let conn = rusqlite::Connection::open_with_flags(snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Error::Validation(format!("not a readable database: {e}")))?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| Error::Validation(format!("unreadable snapshot header: {e}")))?;
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='assets'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| Error::Validation(format!("snapshot has no catalog: {e}")))?;
    if tables == 0 {
        return Err(Error::Validation(
            "snapshot has no assets table — not a library snapshot".into(),
        ));
    }
    Ok(version)
}

/// Write a snapshot back over the live database, returning the snapshot taken
/// of the state being replaced.
///
/// Two choices here are load-bearing:
///
/// - **The destination is a connection of its own.** SQLite's online backup
///   wants `&mut` on the destination, and [`Store::conn`](crate::store::Store)
///   deliberately hands out `&Connection` (nothing outside the store may write).
///   A second connection to the same WAL database is the normal shape — the task
///   journal is already a third — and SQLite serialises the writes. The handle
///   the app is reading through keeps the pages it has until it is reopened,
///   which is why the caller reopens the library right after this returns.
/// - **The order.** What-is-current is snapshotted *before* a byte is
///   overwritten, and an unreadable-or-newer snapshot is refused before even
///   that. So every outcome leaves a library that opens, and a restore that
///   turned out to be the wrong snapshot is itself restorable from the path this
///   returns.
pub fn restore_backup(
    root: &Path,
    snapshot: &Path,
    conn: &rusqlite::Connection,
) -> Result<PathBuf> {
    if !snapshot.is_file() {
        return Err(Error::Validation(format!(
            "no snapshot at {}",
            snapshot.display()
        )));
    }
    let from = snapshot_version(snapshot)?;
    if from > crate::store::schema::SCHEMA_VERSION {
        return Err(Error::Validation(format!(
            "snapshot is at library schema v{from}, this build reads v{} only: \
             restoring it would leave the library unopenable",
            crate::store::schema::SCHEMA_VERSION
        )));
    }
    let before = create_backup(root, conn)?;
    let mut dst = rusqlite::Connection::open(root.join("library.db"))?;
    // The live handle is still open and may hold a read snapshot; a restore that
    // cannot get the write lock within this window reports failure rather than
    // hanging the UI thread — nothing has been overwritten in that case except
    // the extra snapshot `create_backup` just took.
    dst.execute_batch("PRAGMA busy_timeout=5000;")?;
    dst.restore(
        rusqlite::MAIN_DB,
        snapshot,
        None::<fn(rusqlite::backup::Progress)>,
    )
    .map_err(|e| Error::Db(format!("restore failed: {e}")))?;
    Ok(before)
}

/// Delete the oldest snapshots beyond [`MAX_BACKUPS`]. Returns how many were
/// removed.
pub fn prune_backups(root: &Path) -> Result<usize> {
    let files = list_backups(root);
    let overflow = files.len().saturating_sub(MAX_BACKUPS);
    for path in files.iter().take(overflow) {
        std::fs::remove_file(path)?;
    }
    Ok(overflow)
}

/// The auto-backup decision for library open: snapshot only when the newest
/// existing backup is stale. `Ok(None)` = a fresh snapshot already exists.
pub fn maybe_auto_backup_at(
    root: &Path,
    conn: &rusqlite::Connection,
    now: DateTime<Utc>,
) -> Result<Option<PathBuf>> {
    let stale = match list_backups(root).pop() {
        Some(newest) => {
            let age = now.signed_duration_since(backup_mtime(&newest)).num_hours();
            age >= AUTO_BACKUP_INTERVAL.num_hours()
        }
        None => true,
    };
    if stale {
        Ok(Some(create_backup(root, conn)?))
    } else {
        Ok(None)
    }
}

/// Best-effort auto-backup for `Library::open`: never fails the open.
pub fn maybe_auto_backup(root: &Path, conn: &rusqlite::Connection) {
    let _ = maybe_auto_backup_at(root, conn, Utc::now());
}

/// File-modification time of a snapshot, epoch 0 when unreadable (treats it
/// as ancient so a snapshot takes place).
fn backup_mtime(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_creates_prunes_and_throttles() {
        let root = std::env::temp_dir().join(format!("trove-backup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = rusqlite::Connection::open(root.join("library.db")).unwrap();
        conn.execute_batch("CREATE TABLE t(x)").unwrap();

        let first = create_backup(&root, &conn).unwrap();
        assert!(first.is_file());

        // Throttle: a fresh snapshot suppresses the auto path.
        assert!(
            maybe_auto_backup_at(&root, &conn, Utc::now())
                .unwrap()
                .is_none()
        );
        // A day later it fires.
        assert!(
            maybe_auto_backup_at(&root, &conn, Utc::now() + chrono::Duration::hours(25))
                .unwrap()
                .is_some()
        );

        // Pruning keeps only MAX_BACKUPS newest.
        for _ in 0..(MAX_BACKUPS + 3) {
            create_backup(&root, &conn).unwrap();
        }
        assert_eq!(list_backups(&root).len(), MAX_BACKUPS);

        // Windows refuses to delete files with open handles — close the
        // connection before removing the temp tree.
        drop(conn);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A library database for the restore tests: `library.db` under a temp root,
    /// with the table `snapshot_version` insists on and this build's schema
    /// version, and one table to carry observable rows in.
    fn restore_fixture(name: &str) -> (PathBuf, rusqlite::Connection) {
        let root =
            std::env::temp_dir().join(format!("trove-restore-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let conn = rusqlite::Connection::open(root.join("library.db")).unwrap();
        // The version is this build's, read from the schema rather than written
        // here: the refusal test below needs "one newer than now", and a literal
        // would quietly stop meaning that the next time the schema moves.
        let here = crate::store::schema::SCHEMA_VERSION;
        conn.execute_batch(&format!(
            "CREATE TABLE assets(id TEXT);
             PRAGMA user_version = {here};"
        ))
        .unwrap();
        (root, conn)
    }

    fn put(conn: &rusqlite::Connection, id: &str) {
        conn.execute("INSERT INTO assets(id) VALUES (?1)", [id])
            .unwrap();
    }

    fn ids(conn: &rusqlite::Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT id FROM assets ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        stmt.sort();
        stmt
    }

    /// The point of the whole function: a snapshot goes back in, and the state
    /// it replaced is not lost on the way — it is the path that came back, and
    /// it still holds the row the restore removed.
    #[test]
    fn restoring_returns_the_database_to_the_snapshot_and_keeps_what_it_replaced() {
        let (root, conn) = restore_fixture("roundtrip");
        put(&conn, "kept");
        let snapshot = create_backup(&root, &conn).unwrap();
        put(&conn, "gone");
        assert_eq!(ids(&conn), ["gone", "kept"]);

        let before = restore_backup(&root, &snapshot, &conn).unwrap();

        assert_eq!(
            ids(&conn),
            ["kept"],
            "the live database is the snapshot again"
        );
        let prior =
            rusqlite::Connection::open_with_flags(&before, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        assert_eq!(
            ids(&prior),
            ["gone", "kept"],
            "the returned snapshot is the state the restore overwrote"
        );
        drop(prior);
        drop(conn);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Refusals must be *pre-flight*: not a database, and a schema this build
    /// cannot read, each have to leave the library exactly as it was and the
    /// backups folder no bigger. A restore that failed halfway, or that wrote an
    /// unopenable file in, is worse than no rescue feature at all.
    #[test]
    fn a_bad_snapshot_is_refused_before_anything_is_written() {
        let (root, conn) = restore_fixture("refuse");
        put(&conn, "kept");
        let count = list_backups(&root).len();

        // Not a database.
        let junk = root.join("backups").join("not-a-snapshot.db");
        std::fs::create_dir_all(root.join("backups")).unwrap();
        std::fs::write(&junk, b"this is a listing of a folder, not sqlite").unwrap();
        assert!(restore_backup(&root, &junk, &conn).is_err());

        // A snapshot from a newer build.
        let newer = root.join("backups").join("library-from-the-future.db");
        {
            let future = rusqlite::Connection::open(&newer).unwrap();
            future
                .execute_batch(&format!(
                    "CREATE TABLE assets(id TEXT); PRAGMA user_version = {};",
                    crate::store::schema::SCHEMA_VERSION + 1
                ))
                .unwrap();
        }
        let error = restore_backup(&root, &newer, &conn).unwrap_err();
        assert!(
            error.to_string().contains("unopenable"),
            "the refusal should name the consequence: {error}"
        );

        // A path that is not there.
        assert!(restore_backup(&root, &root.join("nope.db"), &conn).is_err());

        assert_eq!(ids(&conn), ["kept"], "nothing moved in the live database");
        assert_eq!(
            list_backups(&root).len(),
            count + 2,
            "the only new files are the two snapshots the fixture itself wrote"
        );
        drop(conn);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
