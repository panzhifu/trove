//! Whether the settings the user just changed actually reached the disk.
//!
//! Changing a setting is two facts, not one: the value in this run, and the
//! value that will be there after a restart. The first is held in memory and is
//! always right; the second needs a write to `config.json` (or a library's
//! `config.json`), and that write can fail — a full disk, a read-only home, a
//! permissions change, an antivirus holding the file. Until this module existed,
//! 23 call sites answered such a failure with `let _ =`, which means the user
//! saw the switch flip, was never told otherwise, and found the old value back
//! after the next launch. That is the same silence the task journal had before
//! P0, and the same reason: an error nobody reports is not an error that did not
//! happen, it is an error the user discovers alone, later.
//!
//! What it does *not* do is undo the in-memory change. Rolling a switch back
//! under the pointer reads as the application misbehaving, and the value the
//! user asked for is genuinely in force for this session; the honest statement is
//! "this is active now and may not survive", which is what the status bar says.
//!
//! The flag is process-global rather than per-window because two of the writers
//! (`plugins/builtin.rs`, the update check in `app/root.rs`) run with no window
//! and no controller to hang state on.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set while the most recent settings write failed.
static SETTINGS_WRITE_FAILED: AtomicBool = AtomicBool::new(false);

/// Report the outcome of one settings write.
///
/// Every writer goes through here so there is exactly one place that decides what
/// a failed write means: the flag flips to match the *last* answer rather than
/// latching — a library whose disk filled up and was cleaned out should not carry
/// a warning for the rest of the session while its writes are landing again — and
/// the reason is logged once, not on every subsequent failure, because a session
/// that keeps saving would otherwise repeat the same line dozens of times.
pub(crate) fn note<E: std::fmt::Display>(outcome: std::result::Result<(), E>, what: &'static str) {
    match outcome {
        Ok(()) => {
            if SETTINGS_WRITE_FAILED.swap(false, Ordering::Relaxed) {
                tracing::info!(what, "a settings write landed again");
            }
        }
        Err(error) => {
            if SETTINGS_WRITE_FAILED.swap(true, Ordering::Relaxed) {
                tracing::debug!(what, %error, "a settings write failed again");
            } else {
                tracing::warn!(what, %error, "settings did not reach the disk");
            }
        }
    }
}

/// Whether the last settings write failed, which is what the status bar shows.
pub(crate) fn degraded() -> bool {
    SETTINGS_WRITE_FAILED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{degraded, note};

    /// The flag tracks the last write, both ways, and a failure that is followed
    /// by a success stops warning.
    ///
    /// This is the only test that touches the flag, so no other case can race it.
    #[test]
    fn a_failed_write_warns_until_one_lands() {
        // Whatever the earlier tests left, a clean write must clear it.
        note(Ok::<(), &str>(()), "test");
        assert!(!degraded(), "a landing write clears the warning");

        note(Err::<(), &str>("disk full"), "test");
        assert!(degraded(), "a failed write warns");

        // A second failure keeps warning, and does not change the answer.
        note(Err::<(), &str>("disk full"), "test");
        assert!(degraded());

        note(Ok::<(), &str>(()), "test");
        assert!(!degraded(), "and the warning is not a latch");
    }
}
