//! Operational services that live above the store/media layers: database
//! backups, the full-backup archive, maintenance jobs, the local collect
//! service, handing files to external applications, and the release-update
//! check.

pub mod archive;
pub mod backup;
pub mod collect;
pub mod font_manager;
#[cfg(target_os = "linux")]
pub mod kwin;
#[cfg(target_os = "linux")]
pub mod kwin_script;
pub mod maintenance;
pub mod open_external;
pub mod screenshot;
pub mod storage;
pub mod update;
pub mod xmp;
