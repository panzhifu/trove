//! Operational services that live above the store/media layers: database
//! backups, maintenance jobs, the local collect service, handing files to
//! external applications, and the release-update check.

pub mod backup;
pub mod collect;
pub mod font_manager;
pub mod maintenance;
pub mod open_external;
pub mod screenshot;
pub mod update;
