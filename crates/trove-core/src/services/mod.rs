//! Operational services that live above the store/media layers: database
//! backups, the repository package, maintenance jobs, migration from other
//! asset managers, the local collect service, handing files to external
//! applications, and the release-update check.

pub mod backup;
pub mod collect;
pub mod embed_model;
pub mod font_manager;
#[cfg(target_os = "linux")]
pub mod kwin;
#[cfg(target_os = "linux")]
pub mod kwin_script;
pub mod local_model;
pub mod maintenance;
pub mod migrate;
mod model_fetch;
pub mod open_external;
pub mod repo_package;
pub mod screenshot;
pub mod storage;
pub mod update;
pub mod xmp;
