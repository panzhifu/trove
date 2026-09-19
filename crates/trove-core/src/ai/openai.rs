//! The OpenAI-compatible embedding provider.
//!
//! Speaks `POST {base_url}/embeddings` with `{"model", "input"}` — the one
//! request shape every mainstream server accepts: OpenAI, Ollama
//! (`http://127.0.0.1:11434/v1`), LM Studio, vLLM, and the hosted proxies.
//! Batches ride one request each (bounded by [`REQUEST_BATCH`]), transient
//! failures retry with a pause, and the dimension is learned from the first
//! response rather than configured — local servers name models that all
//! differ.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use super::EmbeddingProvider;
use crate::config::EmbeddingConfig;
use crate::error::{Error, Result};
use crate::model::EmbeddingSpace;

/// Inputs per HTTP request. The OpenAI ceiling is 2048; staying well under
/// keeps a single oversized batch from being retried whole and spreads the
/// provider's rate limit across smaller, resumable units.
const REQUEST_BATCH: usize = 64;
/// Whole-request ceiling. An embeddings call is one round trip; 60 s is
/// generous even for a busy local server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Retries after the first attempt for transient failures (429, 5xx,
/// transport errors). A 4xx never retries — the server's answer will not
/// change.
const RETRIES: usize = 2;
/// Pause between attempts, doubled each retry.
const BACKOFF: Duration = Duration::from_millis(1_500);
/// Response body cap. 64 × 3072-dim floats of JSON is a few MB; 64 MB
/// covers any sane server and stops a runaway one.
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// An embeddings client for one configured endpoint + model.
pub struct OpenAICompatible {
    base_url: String,
    api_key: String,
    model: String,
    /// Learned from the first successful response (`None` until then) —
    /// servers vary the dimension per model and the model card is not
    /// always reachable.
    known_dim: OnceLock<usize>,
}

impl OpenAICompatible {
    /// Build a provider from the user's settings. Refuses half-configured
    /// settings up front rather than failing per request.
    pub fn new(config: &EmbeddingConfig) -> Result<Self> {
        let base_url = config.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::Validation(
                "embedding endpoint is not configured (no base URL)".into(),
            ));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::Validation(format!(
                "embedding base URL must be http(s), got {:?}",
                config.base_url
            )));
        }
        let model = config.model.trim().to_string();
        if model.is_empty() {
            return Err(Error::Validation(
                "embedding endpoint is not configured (no model name)".into(),
            ));
        }
        Ok(Self {
            base_url,
            api_key: config.api_key.trim().to_string(),
            model,
            known_dim: OnceLock::new(),
        })
    }

    /// One HTTP round trip for `inputs`, parsed and validated.
    fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.model, "input": inputs }).to_string();
        let url = format!("{}/embeddings", self.base_url);

        let mut last_error: Option<String> = None;
        for attempt in 0..=RETRIES {
            if attempt > 0 {
                std::thread::sleep(BACKOFF * (1_u32 << (attempt - 1)));
            }

            let mut request = ureq::post(&url).header("Content-Type", "application/json");
            // Local servers want no Authorization header at all; sending
            // `Bearer ` with an empty token is at best noise.
            if !self.api_key.is_empty() {
                request = request.header("Authorization", &format!("Bearer {}", self.api_key));
            }
            let response = request
                .config()
                .timeout_global(Some(REQUEST_TIMEOUT))
                .http_status_as_error(false)
                .build()
                .send(body.as_str());

            match response {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    let text = read_body(&mut response)?;
                    if (200..300).contains(&status) {
                        let vectors = parse_response(&text, inputs.len())?;
                        if let Some(dim) = vectors.first().map(Vec::len) {
                            let _ = self.known_dim.set(dim);
                        }
                        return Ok(vectors);
                    }
                    let detail = parse_error(&text).unwrap_or_else(|| format!("HTTP {status}"));
                    // A 4xx other than 429 is the server rejecting the
                    // request itself — retrying cannot help.
                    if status == 429 || status >= 500 {
                        last_error = Some(detail);
                        continue;
                    }
                    return Err(Error::Validation(format!(
                        "embedding endpoint refused the request: {detail}"
                    )));
                }
                Err(error) => {
                    // Transport-level failure (DNS, connect, timeout): worth
                    // another attempt unless the retries are spent.
                    last_error = Some(error.to_string());
                }
            }
        }
        Err(Error::Io(std::io::Error::other(format!(
            "embedding endpoint unreachable after {} attempts: {}",
            RETRIES + 1,
            last_error.unwrap_or_default()
        ))))
    }
}

impl EmbeddingProvider for OpenAICompatible {
    fn id(&self) -> &str {
        &self.model
    }

    fn asset_space(&self) -> EmbeddingSpace {
        EmbeddingSpace::Text
    }

    fn dim(&self) -> Option<usize> {
        self.known_dim.get().copied()
    }

    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(REQUEST_BATCH) {
            out.extend(self.request(chunk)?);
        }
        Ok(out)
    }
}

/// Read the response body as text under the byte cap.
fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<String> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))
}

/// Parse a successful `/embeddings` body: one vector per input, in input
/// order (the wire order is arbitrary — the `index` field is authoritative).
fn parse_response(text: &str, expected: usize) -> Result<Vec<Vec<f32>>> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        index: usize,
        embedding: Vec<f32>,
    }

    let parsed: Response = serde_json::from_str(text)
        .map_err(|e| Error::Validation(format!("embedding endpoint sent unparseable JSON: {e}")))?;

    if parsed.data.len() != expected {
        return Err(Error::Validation(format!(
            "embedding endpoint returned {} vectors for {expected} inputs",
            parsed.data.len()
        )));
    }
    let mut vectors: Vec<Option<Vec<f32>>> = (0..expected).map(|_| None).collect();
    for data in parsed.data {
        if data.index >= expected {
            return Err(Error::Validation(format!(
                "embedding endpoint sent an out-of-range index {}",
                data.index
            )));
        }
        if data.embedding.is_empty() {
            return Err(Error::Validation(
                "embedding endpoint sent an empty vector".into(),
            ));
        }
        vectors[data.index] = Some(data.embedding);
    }
    let vectors = vectors
        .into_iter()
        .collect::<Option<Vec<Vec<f32>>>>()
        .ok_or_else(|| Error::Validation("embedding endpoint skipped an input".into()))?;

    // One model, one dimension: a server that answers with mixed lengths is
    // broken in a way that would poison comparability.
    let dim = vectors[0].len();
    if vectors.iter().any(|v| v.len() != dim) {
        return Err(Error::Validation(
            "embedding endpoint returned mixed vector dimensions".into(),
        ));
    }
    Ok(vectors)
}

/// Pull a human-readable message out of an error body, trying the OpenAI
/// error shape before falling back to the raw text (truncated).
fn parse_error(text: &str) -> Option<String> {
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

fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(cap).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: &str, model: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            base_url: base_url.into(),
            api_key: String::new(),
            model: model.into(),
        }
    }

    #[test]
    fn new_validates_and_normalizes_the_endpoint() {
        assert!(OpenAICompatible::new(&config("", "m")).is_err());
        assert!(OpenAICompatible::new(&config("https://api.example.com/v1", "")).is_err());
        assert!(OpenAICompatible::new(&config("ftp://nope", "m")).is_err());

        let provider =
            OpenAICompatible::new(&config("https://api.example.com/v1///", " m3-small ")).unwrap();
        assert_eq!(provider.id(), "m3-small");
        assert_eq!(
            provider.dim(),
            None,
            "the dimension is learned, not configured"
        );
        assert_eq!(provider.asset_space(), EmbeddingSpace::Text);
        assert!(provider.known_dim.set(1536).is_ok());
        assert_eq!(provider.dim(), Some(1536));
    }

    #[test]
    fn parse_response_reorders_by_index_and_checks_shape() {
        let text = r#"{
            "object": "list",
            "data": [
                {"object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6]},
                {"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}
            ],
            "model": "m",
            "usage": {"prompt_tokens": 4, "total_tokens": 4}
        }"#;
        let vectors = parse_response(text, 2).unwrap();
        assert_eq!(
            vectors[0],
            vec![0.1, 0.2, 0.3],
            "input order, not wire order"
        );
        assert_eq!(vectors[1], vec![0.4, 0.5, 0.6]);

        // Wrong count, missing entry, bad index, empty vector, mixed dims.
        assert!(parse_response(text, 3).is_err());
        let missing = r#"{"data": [{"index": 1, "embedding": [1.0]}]}"#;
        assert!(parse_response(missing, 2).is_err());
        let out_of_range = r#"{"data": [{"index": 7, "embedding": [1.0]}]}"#;
        assert!(parse_response(out_of_range, 1).is_err());
        let empty = r#"{"data": [{"index": 0, "embedding": []}]}"#;
        assert!(parse_response(empty, 1).is_err());
        let mixed = r#"{"data": [
            {"index": 0, "embedding": [1.0, 2.0]},
            {"index": 1, "embedding": [1.0, 2.0, 3.0]}
        ]}"#;
        assert!(parse_response(mixed, 2).is_err());
        assert!(parse_response("not json", 1).is_err());
    }

    #[test]
    fn parse_error_reads_the_official_shape_then_gives_up() {
        let official =
            r#"{"error": {"message": "Model `nope` not found", "type": "invalid_request_error"}}"#;
        assert_eq!(
            parse_error(official).as_deref(),
            Some("Model `nope` not found")
        );
        assert_eq!(parse_error("<html>gateway timeout</html>"), None);
    }

    #[test]
    fn truncate_keeps_whole_characters() {
        assert_eq!(truncate("short", 300), "short");
        let long = "水".repeat(400);
        let cut = truncate(&long, 300);
        assert_eq!(cut.chars().count(), 301, "300 chars plus the ellipsis");
        assert!(cut.ends_with('…'));
    }
}
