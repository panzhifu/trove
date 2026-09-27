//! Process-wide logging, shared by every Trove binary.
//!
//! `tracing` events go to stderr and — for the desktop app — to a log file.
//! Installed first thing in `main`, before any component can emit. Two sinks:
//!
//! - **stderr** — the terminal the binary was launched from;
//! - **`<state>/trove/logs/trove.log`** — survives launcher starts where no
//!   terminal is attached (the desktop app). Appended across runs; rotated
//!   to `trove.log.old` once it grows past [`MAX_LOG_BYTES`].
//!
//! The filter comes from `RUST_LOG` (standard `tracing` directives, e.g.
//! `RUST_LOG=trove=debug,xcap=warn`); without it every crate logs at
//! [`LoggingOptions::default_level`] and up. Every crate in the workspace
//! logs through the same global subscriber, and so does any dependency
//! using the `tracing` facade (gpui-pre's ashpd/zbus, for example).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Rotate the log file once it grows past this (one `.old` is kept).
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// What a binary wants from the logging setup. The two Trove binaries
/// disagree on the stderr floor, on whether a script's output should carry
/// module targets, and on whether a background log file is wanted at all —
/// the differences live here as data so the wiring stays in one place.
pub struct LoggingOptions {
    /// The stderr level when `RUST_LOG` is unset: `"info"` for the desktop
    /// app, `"warn"` (or `"error"` behind `--quiet`) for the CLI.
    pub default_level: &'static str,
    /// Append to the rotating file under `paths::logs_dir()`. The desktop
    /// app turns this on — it launches without a terminal; the CLI keeps
    /// its output script-friendly and skips it.
    pub file_sink: bool,
    /// Print the event target (module path) on stderr lines. Off for the
    /// CLI, whose lines belong to a script's stderr.
    pub targets: bool,
}

impl LoggingOptions {
    /// The desktop app: stderr at `info`, the rotating log file on.
    pub fn app() -> Self {
        Self {
            default_level: "info",
            file_sink: true,
            targets: true,
        }
    }

    /// The CLI: stderr at `default_level`, no file sink, no targets.
    pub fn cli(default_level: &'static str) -> Self {
        Self {
            default_level,
            file_sink: false,
            targets: false,
        }
    }
}

/// Install the global subscriber. The global can only be set once per
/// process; a second call is a no-op with a note on stderr.
pub fn init(options: LoggingOptions) {
    let filter = match std::env::var_os("RUST_LOG") {
        None => EnvFilter::new(options.default_level),
        Some(spec) => EnvFilter::try_new(spec.to_string_lossy()).unwrap_or_else(|err| {
            eprintln!("[logging] invalid RUST_LOG ({err}); falling back to the default");
            EnvFilter::new(options.default_level)
        }),
    };

    // Terminal sink. `stdout` stays untouched; stderr is what terminals and
    // journald capture by default.
    let stderr = tracing_subscriber::fmt::layer()
        .with_target(options.targets)
        .with_writer(std::io::stderr);
    // File sink, when the config directory is writable. `try_clone` per event
    // keeps the writer 'static without locking; one dup(2) per event is
    // cheap at this log volume.
    let opened = options.file_sink.then(open_log_file).flatten();
    let log_path = opened.as_ref().map(|(_, path)| path.display().to_string());
    let file_layer = opened.map(|(file, _)| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || file.try_clone().expect("clone trove log file"))
    });

    let subscriber = tracing_subscriber::registry()
        .with(stderr)
        .with(file_layer)
        .with(filter);

    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("[logging] a global subscriber is already installed; keeping it");
        return;
    }
    tracing::info!(
        file = log_path.as_deref().unwrap_or("<stderr only>"),
        "logging initialized (set RUST_LOG to adjust verbosity)"
    );
}

/// Open the log file for appending, rotating a bloated one aside first.
/// `None` = logging stays on stderr only (e.g. a read-only state directory).
fn open_log_file() -> Option<(File, PathBuf)> {
    let dir = crate::paths::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("trove.log");
    rotate_bloated(&path);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some((file, path))
}

/// Move `trove.log` to `trove.log.old` once it passes [`MAX_LOG_BYTES`]; the
/// old file is overwritten. Best effort — failure just keeps appending.
fn rotate_bloated(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let old = path.with_extension("log.old");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(path, old);
}
