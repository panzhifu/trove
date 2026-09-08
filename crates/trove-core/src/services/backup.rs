//! Library backups: consistent snapshots of `library.db` via SQLite's
//! `VACUUM INTO` (safe even while other statements are running).
//!
//! Snapshots live in `<library root>/backups/` and roll on two axes:
//! auto-backup is throttled to one per day (taken at library open), and the
//! directory is pruned to the newest [`MAX_BACKUPS`] files after every write.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

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
}
