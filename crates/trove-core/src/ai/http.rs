//! The HTTP plumbing the OpenAI-compatible providers share: reading a bounded
//! response body, and turning a server's error document into something worth
//! putting in a log line.
//!
//! Lifted out of `openai.rs` when the chat provider arrived and needed the
//! same helpers — the alternative was a second copy of the OpenAI error
//! shape, and two copies of a wire format is exactly the drift this module
//! exists to prevent. The request-building half deliberately stays in each
//! provider: `/embeddings` and `/chat/completions` disagree about everything
//! that matters there.

use serde::Deserialize;

use crate::error::{Error, Result};

/// Read a response body as text, refusing to buffer more than `limit` bytes.
/// A server answering with megabytes of prose is a bug, not a document.
pub(crate) fn read_body(
    response: &mut ureq::http::Response<ureq::Body>,
    limit: u64,
) -> Result<String> {
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))
}

/// Pull a human-readable message out of an error body, trying the OpenAI
/// error shape (`{"error":{"message":…}}`) before giving up.
pub(crate) fn parse_error(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Body {
        error: Option<Detail>,
    }
    #[derive(Deserialize)]
    struct Detail {
        message: Option<String>,
    }

    let body: Body = serde_json::from_str(text).ok()?;
    let message = body.error?.message?;
    Some(truncate(&message, 300))
}

/// Cut a diagnostic down to something a log line can carry.
pub(crate) fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(cap).collect();
        out.push('…');
        out
    }
}
