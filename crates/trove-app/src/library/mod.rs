//! Library state and background work: the [`LibraryController`] every panel
//! reads, plus the import-job orchestration and folder watching.

mod controller;
pub mod jobs;
pub mod video_player;
pub mod watcher;

pub use controller::{GRID_PAGE_SIZE, ImportPhase, LibraryController, ViewMode};
