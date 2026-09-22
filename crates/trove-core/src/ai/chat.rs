//! The OpenAI-compatible chat provider: the model that *reads* an asset.
//!
//! Same server shape as [`super::openai`], different path — `POST
//! {base_url}/chat/completions` with `{"model", "messages"}` — and a
//! different job. An embedding turns text into a vector; this answers a
//! question about the asset, which is what the automatic tagger asks.
//!
//! The one structural difference is that a request may carry an image. A
//! vision model receives the asset's thumbnail as a data URI in the same
//! message as the text, the shape every OpenAI-compatible multimodal server
//! accepts. A text-only model rejects such a request; the tagger notices and
//! degrades to text rather than failing the run.

use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;

use super::http::{parse_error, read_body};
use crate::config::ChatConfig;
use crate::error::{Error, Result};

/// Whole-request ceiling. A chat model composing a short tag list is slower
/// than an embedding call and a vision model is slower still — this bounds
/// one asset, not the run.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Retries after the first attempt for transient failures (429, 5xx,
/// transport errors). Matches the embedding provider's policy: a 4xx is the
/// server answering, not the network failing, so it never retries.
const RETRIES: usize = 2;
const BACKOFF: Duration = Duration::from_millis(1_500);
/// Body cap. A tag list is a few hundred bytes; megabytes would be a broken
/// server.
const MAX_BODY: u64 = 4 * 1024 * 1024;
/// Output ceiling. The prompt asks for a short JSON array, and a model that
/// ignores that should be cut off rather than billed for an essay.
const MAX_TOKENS: u32 = 512;
/// Sampling temperature. Tagging wants the obvious answer rather than a
/// creative one, and a low temperature is also what makes two runs agree.
const TEMPERATURE: f32 = 0.2;

/// One request: the standing instruction, the asset, and its thumbnail when
/// there is one.
pub struct ChatRequest<'a> {
    /// System message — the role, the rules and the tag vocabulary. Built
    /// once per run and identical for every asset in it.
    pub system: &'a str,
    /// The asset, as text.
    pub user: &'a str,
    /// Thumbnail bytes (JPEG). `None` is a text-only request.
    pub image: Option<&'a [u8]>,
}

/// A model that answers prompts about an asset.
///
/// Synchronous for the same reason [`super::EmbeddingProvider`] is: it runs
/// on the tagger's worker threads, and an async runtime here would be a
/// second one in the process for no gain.
pub trait ChatProvider: Send + Sync {
    /// Identity of the model. Stored beside every tag it produced, so a later
    /// run can tell whose work it is looking at.
    fn id(&self) -> &str;

    /// Answer one prompt. The reply is free text; parsing it belongs to the
    /// caller — see [`super::tagging`].
    fn complete(&self, request: &ChatRequest<'_>) -> Result<String>;
}

/// `POST {base_url}/chat/completions`, OpenAI-compatible.
pub struct OpenAIChat {
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAIChat {
    pub fn new(config: &ChatConfig) -> Result<Self> {
        let base_url = config.base_url.trim().trim_end_matches('/');
        let model = config.model.trim();
        if base_url.is_empty() {
            return Err(Error::Validation("chat endpoint base URL is empty".into()));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::Validation(format!(
                "chat endpoint base URL must be http(s), not '{base_url}'"
            )));
        }
        if model.is_empty() {
            return Err(Error::Validation("chat model name is empty".into()));
        }
        Ok(Self {
            base_url: base_url.to_string(),
            api_key: config.api_key.trim().to_string(),
            model: model.to_string(),
        })
    }

    /// The JSON body for one request.
    fn body(&self, request: &ChatRequest<'_>) -> String {
        let user_content = match request.image {
            Some(bytes) => serde_json::json!([
                { "type": "text", "text": request.user },
                { "type": "image_url", "image_url": { "url": data_uri(bytes) } },
            ]),
            None => serde_json::Value::String(request.user.to_string()),
        };
        serde_json::json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": user_content },
            ],
            "temperature": TEMPERATURE,
            "max_tokens": MAX_TOKENS,
        })
        .to_string()
    }

    fn request(&self, request: &ChatRequest<'_>) -> Result<String> {
        let body = self.body(request);
        let url = format!("{}/chat/completions", self.base_url);

        let mut last_error: Option<String> = None;
        for attempt in 0..=RETRIES {
            if attempt > 0 {
                std::thread::sleep(BACKOFF * (1_u32 << (attempt - 1)));
            }

            let mut outbound = ureq::post(&url).header("Content-Type", "application/json");
            // Local servers want no Authorization header at all; sending
            // `Bearer ` with an empty token is at best noise.
            if !self.api_key.is_empty() {
                outbound = outbound.header("Authorization", &format!("Bearer {}", self.api_key));
            }
            let response = outbound
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
                        return parse_response(&text);
                    }
                    let detail = parse_error(&text).unwrap_or_else(|| format!("HTTP {status}"));
                    if status == 429 || status >= 500 {
                        last_error = Some(detail);
                        continue;
                    }
                    return Err(Error::Validation(format!(
                        "chat endpoint refused the request: {detail}"
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
            "chat endpoint unreachable after {} attempts: {}",
            RETRIES + 1,
            last_error.unwrap_or_default()
        ))))
    }
}

impl ChatProvider for OpenAIChat {
    fn id(&self) -> &str {
        &self.model
    }

    fn complete(&self, request: &ChatRequest<'_>) -> Result<String> {
        self.request(request)
    }
}

/// A thumbnail as a data URI — the form a multimodal message takes.
fn data_uri(bytes: &[u8]) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Pull the assistant's text out of a `/chat/completions` body.
///
/// `content` is a plain string on every server worth supporting, but the
/// multimodal shape allows an array of parts, so a server answering that way
/// gets its text parts joined instead of an error.
fn parse_response(text: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Response {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: Message,
    }
    #[derive(Deserialize)]
    struct Message {
        content: Content,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Content {
        Text(String),
        Parts(Vec<Part>),
        Null,
    }
    #[derive(Deserialize)]
    struct Part {
        #[serde(default)]
        text: Option<String>,
    }

    let parsed: Response = serde_json::from_str(text)
        .map_err(|e| Error::Validation(format!("chat endpoint sent unparseable JSON: {e}")))?;
    let choice = parsed
        .choices
        .first()
        .ok_or_else(|| Error::Validation("chat endpoint returned no choices".into()))?;
    let content = match &choice.message.content {
        Content::Text(text) => text.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
        Content::Null => String::new(),
    };
    if content.trim().is_empty() {
        return Err(Error::Validation(
            "chat endpoint returned an empty reply".into(),
        ));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: &str, model: &str) -> ChatConfig {
        ChatConfig {
            base_url: base_url.into(),
            model: model.into(),
            ..ChatConfig::default()
        }
    }

    fn provider() -> OpenAIChat {
        OpenAIChat::new(&config("https://api.example.com/v1///", " gpt-4o-mini ")).unwrap()
    }

    #[test]
    fn new_validates_and_normalizes_the_endpoint() {
        assert!(OpenAIChat::new(&config("", "m")).is_err());
        assert!(OpenAIChat::new(&config("https://api.example.com/v1", "")).is_err());
        assert!(OpenAIChat::new(&config("ftp://nope", "m")).is_err());

        let provider = provider();
        assert_eq!(provider.id(), "gpt-4o-mini", "trimmed, not as typed");
    }

    #[test]
    fn a_request_without_an_image_sends_plain_text() {
        let body: serde_json::Value = serde_json::from_str(&provider().body(&ChatRequest {
            system: "sys",
            user: "asset",
            image: None,
        }))
        .unwrap();
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["content"], "asset");
        assert_eq!(body["model"], "gpt-4o-mini");
    }

    #[test]
    fn a_request_with_an_image_sends_a_data_uri_part() {
        let body: serde_json::Value = serde_json::from_str(&provider().body(&ChatRequest {
            system: "sys",
            user: "asset",
            image: Some(&[0xff, 0xd8, 0xff]),
        }))
        .unwrap();
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "text part then image part");
        assert_eq!(parts[0]["text"], "asset");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], data_uri(&[0xff, 0xd8, 0xff]));
    }

    #[test]
    fn a_data_uri_is_standard_base64_under_a_jpeg_type() {
        // `/9j/` is the base64 of FF D8 FF, the JPEG magic — a wrong alphabet
        // or a missing padding character shows up right here.
        assert_eq!(data_uri(&[0xff, 0xd8, 0xff]), "data:image/jpeg;base64,/9j/");
    }

    #[test]
    fn parse_response_reads_a_string_content() {
        let text =
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"[\"cat\"]"}}]}"#;
        assert_eq!(parse_response(text).unwrap(), "[\"cat\"]");
    }

    #[test]
    fn parse_response_joins_multimodal_parts() {
        let text = r#"{"choices":[{"message":{"content":[
            {"type":"text","text":"[\"cat\","},
            {"type":"image_url","image_url":{"url":"data:…"}},
            {"type":"text","text":"\"tabby\"]"}
        ]}}]}"#;
        assert_eq!(parse_response(text).unwrap(), "[\"cat\",\n\"tabby\"]");
    }

    #[test]
    fn parse_response_rejects_the_shapes_that_mean_nothing() {
        assert!(parse_response("not json").is_err());
        assert!(parse_response(r#"{"choices":[]}"#).is_err());
        assert!(
            parse_response(r#"{"choices":[{"message":{"content":"   "}}]}"#).is_err(),
            "an empty reply must not read as 'no tags'"
        );
    }
}
