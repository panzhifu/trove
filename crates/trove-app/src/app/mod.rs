//! Window shell: the root view with its dock layout, the custom title bar,
//! app-wide actions, and interface-language resolution.

pub mod actions;
pub mod i18n;
pub mod title_bar;

mod root;

pub use root::AppView;
