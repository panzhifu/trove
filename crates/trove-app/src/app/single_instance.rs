//! Single-instance lock: one desktop session per user.
//!
//! Two Trove processes opening the same library is the accident this module
//! exists to prevent — SQLite tolerates it (the CLI reads concurrently), but
//! the desktop app owns the writer lock, the tray, and the watch tasks, and a
//! second copy of all three is how a user ends up with two diverging views
//! and a stale thumbnail cache. The lock is an advisory `flock` (std file
//! locking, so Windows is covered too) on a per-user file in the state
//! directory: it is held for the process lifetime and released by exit, and a
//! second process that cannot take it prints a line to stderr — visible from
//! a terminal, and from a launcher the worst case is "nothing happened"
//! rather than two libraries diverging — and exits.

use std::fs::OpenOptions;
use std::path::PathBuf;

/// The held lock. Dropping it releases; the process keeps it alive for as
/// long as it runs.
pub struct InstanceLock {
    _file: std::fs::File,
}

/// Try to become the single instance. `Some(lock)` when this process holds
/// the lock; `None` when another Trove already does — the caller should exit.
pub fn acquire() -> Option<InstanceLock> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    if file.try_lock().is_err() {
        eprintln!("Trove is already running for this user.");
        return None;
    }
    Some(InstanceLock { _file: file })
}

/// Per user, because a shared machine's second account must be able to run
/// its own Trove. The state directory is already per-user by construction;
/// the name is fixed inside it.
fn lock_path() -> PathBuf {
    trove_core::paths::state_dir().join("app-instance.lock")
}
