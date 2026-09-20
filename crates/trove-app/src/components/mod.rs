//! Reusable, asset-type-specific building blocks shared across panels.

// The screenshot picker overlay is Linux-only: it freezes a compositor frame
// and lists the compositor's windows, and elsewhere the platform's own region
// picker is the whole interaction (see `app::root::take_screenshot`).
#[cfg(target_os = "linux")]
pub(crate) mod capture_pick;
pub(crate) mod preview;
