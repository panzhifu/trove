use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::Deserialize;

use crate::ai::analysis::{system_prompt, user_text_lines};
use crate::ai::http::read_body;
use crate::error::{Error, Result};

use super::{VendorAdapter, VendorError, VendorErrorKind, VendorId};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const BACKOFF: Duration = Duration::from_millis(1_500);
const MAX_BODY: u64 = 4 * 1024 * 1024;

pub struct AnthropicAdapter {
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicAdapter {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Result<Self> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::Validation("no base URL".into()));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::Validation(format!("must be http(s), got {base_url:?}")));
        }
        let model = model.trim();
        if model.is_empty() {
            return Err(Error::Validation("no model name".into()));
        }
        if api_key.trim().is_empty() {
            return Err(Error::Validation("Anthropic requires an API key".into()));
        }
        Ok(Self {
            base_url,
            api_key: api_key.trim().to_string(),
            model: model.to_string(),
        })
    }
}

impl VendorAdapter for AnthropicAdapter {
    fn vendor(&self) -> VendorId {
        VendorId::Anthropic
    }

    fn model_version(&self) -> &str {
        &self.model
    }

    fn analyze(
        &self,
        request: &crate::ai::analysis::AiAnalysisRequest,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        let system = system_prompt(request);
        let user_text = user_text_lines(request).join("\n");

        let mut content_parts: Vec<serde_json::Value> =
            vec![serde_json::json!({ "type": "text", "text": user_text })];
        if let Some(jpeg) = &request.thumbnail_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            content_parts.push(serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/jpeg", "data": b64 },
            }));
        }
        if let Some(jpeg) = &request.contact_sheet_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            content_parts.push(serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/jpeg", "data": b64 },
            }));
        }

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1024,
            "temperature": 0.2,
            "system": system,
            "messages": [{ "role": "user", "content": content_parts }],
            "tools": [{
                "name": "trove_asset_analysis",
                "description": "Describe the given asset.",
                "input_schema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "description": { "type": ["string", "null"] },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "rating": { "type": ["integer", "null"] },
                    },
                    "required": ["description", "tags", "rating"],
                },
            }],
            "tool_choice": { "type": "tool", "name": "trove_asset_analysis" },
        });

        self.request(&body, cancel)
    }

    fn probe_connection(&self, cancel: &AtomicBool) -> std::result::Result<(), VendorError> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "Reply with the single word OK." }],
        });
        self.request(&body, cancel).map(|_| ())
    }
}

impl AnthropicAdapter {
    fn request(
        &self,
        body: &serde_json::Value,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        let url = format!("{}/v1/messages", self.base_url);
        let body_str = body.to_string();
        let mut last_error = None;

        for attempt in 0..=2 {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(VendorError {
                    kind: VendorErrorKind::Timeout,
                    message: "cancelled".into(),
                    http_status: None,
                    provider_code: None,
                    request_id: None,
                });
            }
            if attempt > 0 {
                std::thread::sleep(BACKOFF * (1_u32 << (attempt - 1)));
            }

            let response = ureq::post(&url)
                .header("Content-Type", "application/json")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .config()
                .timeout_global(Some(REQUEST_TIMEOUT))
                .http_status_as_error(false)
                .build()
                .send(body_str.as_str());

            match response {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    let text = read_body(&mut response, MAX_BODY).map_err(|e| VendorError {
                        kind: VendorErrorKind::Network,
                        message: e.to_string(),
                        http_status: None,
                        provider_code: None,
                        request_id: None,
                    })?;
                    if (200..300).contains(&status) {
                        return extract_content(&text);
                    }
                    let err = VendorError {
                        kind: classify_http_error(status, &text),
                        message: parse_error(&text).unwrap_or_else(|| format!("HTTP {status}")),
                        http_status: Some(status),
                        provider_code: extract_code(&text),
                        request_id: None,
                    };
                    if !err.kind.is_transient() {
                        return Err(err);
                    }
                    last_error = Some(err);
                }
                Err(e) => {
                    last_error = Some(VendorError {
                        kind: VendorErrorKind::Network,
                        message: e.to_string(),
                        http_status: None,
                        provider_code: None,
                        request_id: None,
                    });
                }
            }
        }
        Err(last_error.expect("loop ran"))
    }
}

fn extract_content(body: &str) -> std::result::Result<String, VendorError> {
    #[derive(Deserialize)]
    struct Response {
        content: Vec<Block>,
    }
    #[derive(Deserialize)]
    struct Block {
        #[serde(rename = "type")]
        block_type: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        input: Option<serde_json::Value>,
    }

    let parsed: Response = serde_json::from_str(body).map_err(|e| VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: format!("unparseable: {e}"),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;

    for block in &parsed.content {
        if block.block_type == "tool_use" {
            if let Some(input) = &block.input {
                return Ok(serde_json::to_string_pretty(input).unwrap_or_default());
            }
        }
    }

    let text: String = parsed.content.iter().filter_map(|b| b.text.as_deref()).collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() {
        Err(VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message: "empty".into(),
            http_status: None,
            provider_code: None,
            request_id: None,
        })
    } else {
        Ok(text)
    }
}

fn classify_http_error(status: u16, body: &str) -> VendorErrorKind {
    match status {
        401 => VendorErrorKind::Auth,
        403 => VendorErrorKind::Permission,
        429 => {
            if body.to_lowercase().contains("quota") {
                VendorErrorKind::Quota
            } else {
                VendorErrorKind::RateLimit
            }
        }
        408 | 500..=599 => VendorErrorKind::Network,
        _ => VendorErrorKind::InvalidResponse,
    }
}

fn parse_error(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed.pointer("/error/message").and_then(|v| v.as_str()).map(str::to_string)
}

fn extract_code(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed.pointer("/error/code").and_then(|v| v.as_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> AnthropicAdapter {
        AnthropicAdapter::new("https://api.anthropic.com", "sk-ant-xxx", "claude-3-haiku-20240307").unwrap()
    }

    #[test]
    fn new_validates() {
        assert!(AnthropicAdapter::new("", "k", "m").is_err());
        assert!(AnthropicAdapter::new("https://x.com", "", "m").is_err());
        assert!(AnthropicAdapter::new("https://x.com", "k", "").is_err());
        assert!(AnthropicAdapter::new("ftp://x.com", "k", "m").is_err());
        assert_eq!(adapter().vendor(), VendorId::Anthropic);
    }

    #[test]
    fn extract_reads_tool_use() {
        let body = r#"{"content":[{"type":"tool_use","name":"trove_asset_analysis","input":{"description":"beach","tags":["ocean"],"rating":4}}]}"#;
        let text = extract_content(body).unwrap();
        assert!(text.contains("beach"));
    }

    #[test]
    fn extract_reads_text_fallback() {
        let body = r#"{"content":[{"type":"text","text":"{\"tags\":[\"beach\"]}"}]}"#;
        let text = extract_content(body).unwrap();
        assert!(text.contains("beach"));
    }

    #[test]
    fn extract_rejects_empty() {
        assert!(extract_content(r#"{"content":[]}"#).is_err());
    }
}
