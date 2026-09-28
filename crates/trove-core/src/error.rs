//! Error types for the Trove core.
//!
//! One error for the whole domain and persistence surface, so a call site can
//! propagate with `?` and only `match` when it actually needs to know *how* a
//! call failed. The variants name the categories the application reacts to
//! differently:
//!
//! - [`Error::Validation`] / [`Error::NotFound`] / [`Error::Conflict`] — the
//!   request was well-formed but refused, gone, or clashing.
//! - [`Error::Db`] / [`Error::Json`] / [`Error::Io`] — storage.
//! - [`Error::Network`] — an HTTP call failed.
//! - [`Error::External`] — another program or service Trove drove failed.
//! - [`Error::Unsupported`] — this build or this machine cannot do it.
//! - [`Error::Message`] — a failure with no category above; carrying the text
//!   is the whole point, so it is deliberately not more than a string.
//!
//! The subsystem-specific taxonomies stay where they are — `kwin::Failure`,
//! `screenshot::Error`, `ai::vendor::VendorError`, the 3D loaders' `String` —
//! because they answer questions their own callers ask (retry vs fall back vs
//! give up) and folding them into this enum would throw that away. They convert
//! at the boundary, into one of the variants above.

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

    /// An HTTP call failed: DNS, TLS, connection, timeout, or a response this
    /// build could not use.
    #[error("network error: {0}")]
    Network(String),

    /// Another program or service Trove drove failed — the OS file opener, a
    /// screenshot backend, KWin's D-Bus interface. `program` names it so a log
    /// line and a message both say *who* failed, not just what.
    #[error("{program}: {message}")]
    External { program: String, message: String },

    /// A capability this build or this machine does not have: no per-user font
    /// directory, a platform without the interface a call needs.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A failure with nothing but a message, for the cases no category above
    /// covers. Unlike the others this carries no meaning a caller can branch
    /// on; reach for a typed variant first.
    #[error("{0}")]
    Message(String),
}

/// `rusqlite` errors are turned into text at the boundary: the error enum is
/// `!Clone` and its variants carry prepared-statement state, none of which the
/// domain wants to hold.
impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Db(error.to_string())
    }
}

/// HTTP failures keep their own category so a caller can tell "offline" from
/// "the server said no" without parsing the message.
impl From<ureq::Error> for Error {
    fn from(error: ureq::Error) -> Self {
        Error::Network(error.to_string())
    }
}
