//! Operational services that live above the store/media layers: database
//! backups, maintenance jobs, the local collect service and handing files
//! to external applications.

pub mod backup;
pub mod collect;
pub mod font_manager;
pub mod maintenance;
pub mod open_with;
pub mod screenshot;
