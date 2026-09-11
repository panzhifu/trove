//! Window shell: the root view with its dock layout, the custom title bar,
//! app-wide actions, interface-language resolution, and the appearance
//! (light/dark + theme) plumbing.

pub mod actions;
pub mod i18n;
pub mod theme;
pub mod title_bar;

mod root;

pub use root::AppView;
