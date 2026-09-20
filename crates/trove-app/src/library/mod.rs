//! Library state and background work: the [`LibraryController`] every panel
//! reads, plus the import-job orchestration and folder watching.

pub mod clipboard;
mod controller;
pub mod jobs;

pub use controller::{
    AiProbe, GRID_PAGE_SIZE, ImportPhase, LibraryController, SelectionSource, ViewMode,
};
