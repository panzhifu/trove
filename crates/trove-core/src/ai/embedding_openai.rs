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
use super::http::{parse_error, read_body};
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
    /// The endpoint is a multimodal (CLIP-style) embedder: assets are
    /// embedded from their image and a text query lands in the same space.
    multimodal: bool,
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
            multimodal: config.multimodal,
            known_dim: OnceLock::new(),
        })
    }

    /// One HTTP round trip for `input`, parsed and validated. `expected` is
    /// how many vectors the caller is waiting for, which is the input count.
    fn request(&self, input: serde_json::Value, expected: usize) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.model, "input": input }).to_string();
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
                    let text = read_body(&mut response, MAX_BODY)?;
                    if (200..300).contains(&status) {
                        let vectors = parse_response(&text, expected)?;
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
        // A multimodal endpoint files its **asset** rows under the image
        // space while its queries stay text: the two encoders are different,
        // but they were trained into one space, so the vectors are directly
        // comparable. That is the whole point of this mode.
        if self.multimodal {
            EmbeddingSpace::Image
        } else {
            EmbeddingSpace::Text
        }
    }

    fn dim(&self) -> Option<usize> {
        self.known_dim.get().copied()
    }

    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(REQUEST_BATCH) {
            let input = self.text_input(chunk);
            out.extend(self.request(input, chunk.len())?);
        }
        Ok(out)
    }

    fn embed_images(&self, paths: &[std::path::PathBuf]) -> Result<Vec<Vec<f32>>> {
        if !self.multimodal {
            return Err(Error::Validation(format!(
                "provider {} is text-only; turn on multimodal mode to embed images",
                self.id()
            )));
        }
        let mut out = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(REQUEST_BATCH) {
            let mut input = Vec::with_capacity(chunk.len());
            for path in chunk {
                input.push(serde_json::json!({ "image": image_data_uri(path)? }));
            }
            out.extend(self.request(serde_json::Value::Array(input), chunk.len())?);
        }
        Ok(out)
    }
}

impl OpenAICompatible {
    /// The `input` array for a batch of texts: bare strings for a plain
    /// embedding endpoint, modality-tagged objects for a multimodal one.
    fn text_input(&self, texts: &[String]) -> serde_json::Value {
        if self.multimodal {
            serde_json::Value::Array(
                texts
                    .iter()
                    .map(|text| serde_json::json!({ "text": text }))
                    .collect(),
            )
        } else {
            serde_json::Value::Array(texts.iter().map(|text| serde_json::json!(text)).collect())
        }
    }
}

/// A thumbnail as a `data:` URI — the inline form multimodal embedding APIs
/// accept. The bytes are already a small JPEG (see [`crate::media::thumb`]).
fn image_data_uri(path: &std::path::Path) -> Result<String> {
    use base64::Engine as _;
    let bytes = std::fs::read(path).map_err(|error| {
        Error::Io(std::io::Error::other(format!(
            "read image {}: {error}",
            path.display()
        )))
    })?;
    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/jpeg",
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::http::truncate;

    fn config(base_url: &str, model: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            base_url: base_url.into(),
            api_key: String::new(),
            model: model.into(),
            multimodal: false,
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

    fn multimodal_config(model: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: String::new(),
            model: model.into(),
            multimodal: true,
        }
    }

    #[test]
    fn a_multimodal_provider_files_assets_under_the_image_space() {
        let multi = OpenAICompatible::new(&multimodal_config("jina-clip-v2")).unwrap();
        assert_eq!(multi.asset_space(), EmbeddingSpace::Image);

        let plain = OpenAICompatible::new(&config("https://api.example.com/v1", "m")).unwrap();
        assert_eq!(plain.asset_space(), EmbeddingSpace::Text);
    }

    #[test]
    fn multimodal_input_labels_each_modality() {
        let plain = OpenAICompatible::new(&config("https://api.example.com/v1", "m")).unwrap();
        assert_eq!(
            plain.text_input(&["cat".to_string()]),
            serde_json::json!(["cat"]),
            "a plain endpoint takes bare strings"
        );

        let multi = OpenAICompatible::new(&multimodal_config("jina-clip-v2")).unwrap();
        assert_eq!(
            multi.text_input(&["猫".to_string()]),
            serde_json::json!([{ "text": "猫" }]),
            "a multimodal endpoint needs the modality tag"
        );
    }

    #[test]
    fn images_become_data_uris_with_the_right_mime() {
        let dir = std::env::temp_dir().join(format!("trove-embed-uri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let png = dir.join("a.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        assert!(
            image_data_uri(&png)
                .unwrap()
                .starts_with("data:image/png;base64,")
        );

        let jpg = dir.join("a.jpg");
        std::fs::write(&jpg, [0xFF, 0xD8, 0xFF]).unwrap();
        assert!(
            image_data_uri(&jpg)
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );

        // An unreadable path is an error, never a silent empty vector.
        assert!(image_data_uri(&dir.join("missing.png")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_a_multimodal_provider_accepts_images() {
        let plain = OpenAICompatible::new(&config("https://api.example.com/v1", "m")).unwrap();
        assert!(
            plain.embed_images(&[]).is_err(),
            "a text endpoint must refuse images even for an empty batch"
        );

        let multi = OpenAICompatible::new(&multimodal_config("jina-clip-v2")).unwrap();
        assert!(multi.embed_images(&[]).unwrap().is_empty());
    }
}
