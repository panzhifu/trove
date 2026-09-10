//! Library state and background work: the [`LibraryController`] every panel
//! reads, plus the import-job orchestration and folder watching.

pub mod clipboard;
mod controller;
pub mod gpu3d;
pub mod jobs;
pub mod video_player;
pub mod viewport3d;
pub mod watcher;

pub use controller::{GRID_PAGE_SIZE, ImportPhase, LibraryController, ViewMode};
