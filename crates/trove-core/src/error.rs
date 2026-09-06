//! Error types for the Trove core.

use thiserror::Error;

/// Result alias used across the core crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error for domain and persistence operations.
#[derive(Debug, Error)]
pub enum Error {
    /// A value failed domain validation (bad input, name too long, ...).
    #[error("invalid value: {0}")]
    Validation(String),

    /// The requested record does not exist.
    #[error("not found: {0}")]
    NotFound(&'static str),

    /// A uniqueness or referential constraint was violated.
    #[error("constraint violation: {0}")]
    Conflict(String),

    /// Underlying database failure.
    #[error("database error: {0}")]
    Db(String),

    /// Serde (de)serialization failure — persisted data, JSON fields, ...
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// Filesystem / I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
