//! Library state and background work: the [`LibraryController`] every panel
//! reads, plus the import-job orchestration and folder watching.

pub mod clipboard;
mod controller;
pub mod jobs;
pub mod open_with;
pub mod watcher;

pub use controller::{GRID_PAGE_SIZE, ImportPhase, LibraryController, ViewMode};
