//! Window shell: the root view with its dock layout, the custom title bar,
//! app-wide actions, interface-language resolution, and the appearance
//! (light/dark + theme) plumbing.

pub mod actions;
pub mod dock_skin;
pub mod i18n;
pub mod theme;
pub mod title_bar;

pub mod library_manager;
mod root;
mod tray;

pub use library_manager::LibraryManagerView;
pub use root::AppView;
pub(crate) use root::run_update_check;
// The screenshot picker overlay hands a highlighted window back here, since
// it is the main window that has to run the capture.
pub(crate) use root::capture_picked_window;
