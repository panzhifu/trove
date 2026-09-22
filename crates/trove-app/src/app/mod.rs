//! Window shell: the root view with its dock layout, the custom title bar,
//! app-wide actions, interface-language resolution, and the appearance
//! (light/dark + theme) plumbing.

pub mod actions;
pub mod dock_skin;
pub mod i18n;
pub mod theme;
pub mod title_bar;

mod capture;
pub mod library_manager;
mod root;
mod status_bar;
mod task_panel;
mod tray;

pub use library_manager::LibraryManagerView;
pub use root::AppView;
pub(crate) use root::run_update_check;
// The screenshot picker overlay hands a highlighted window back here, since
// it is the main window that has to run the capture. Both the overlay and
// this entry point are Linux-only: the picker freezes a compositor frame and
// lists the compositor's windows, and elsewhere the platform's own region
// picker is the whole interaction (see `take_screenshot`).
#[cfg(target_os = "linux")]
pub(crate) use capture::capture_picked_window;
