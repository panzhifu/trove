//! History: what the user did and what they used recently.
//!
//! Two independent records live here:
//! - [`undo`] — the operation-level undo/redo log for metadata mutations
//!   (in-memory, per library, bounded);
//! - [`app`] — app-level recent-use records (picked colours, recently opened
//!   libraries) persisted next to the config. These are *history*, not
//!   settings: high-frequency writes with their own lifecycle, deliberately
//!   kept out of `AppConfig`.

pub mod app;
pub mod undo;

pub use app::AppHistory;
pub use undo::{DEFAULT_UNDO_CAP, Op, OpAction, OpDesc};
