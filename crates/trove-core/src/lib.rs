//! Trove core library — domain models, persistence and services for a local
//! asset library. Contains no UI code.

pub mod config;
pub mod error;
pub mod events;
pub mod layout;
pub mod library;
pub mod maintenance;
pub mod media;
pub mod model;
pub mod store;

pub use error::{Error, Result};
