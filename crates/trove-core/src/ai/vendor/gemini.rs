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

pub struct GeminiAdapter {
    base_url: String,
    api_key: String,
    model: String,
}

impl GeminiAdapter {
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
            return Err(Error::Validation("Gemini requires an API key".into()));
        }
        Ok(Self {
            base_url,
            api_key: api_key.trim().to_string(),
            model: model.to_string(),
        })
    }
}

impl VendorAdapter for GeminiAdapter {
    fn vendor(&self) -> VendorId {
        VendorId::Gemini
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

        let mut parts: Vec<serde_json::Value> = vec![serde_json::json!({ "text": user_text })];
        if let Some(jpeg) = &request.thumbnail_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            parts.push(serde_json::json!({
                "inline_data": { "mime_type": "image/jpeg", "data": b64 },
            }));
        }
        if let Some(jpeg) = &request.contact_sheet_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            parts.push(serde_json::json!({
                "inline_data": { "mime_type": "image/jpeg", "data": b64 },
            }));
        }

        let body = serde_json::json!({
            "system_instruction": { "parts": [{ "text": system }] },
            "contents": [{ "role": "user", "parts": parts }],
            "generationConfig": {
                "temperature": 0.2,
                "responseMimeType": "application/json",
                "responseSchema": {
                    "type": "OBJECT",
                    "properties": {
                        "description": { "type": ["STRING", "NULL"] },
                        "tags": { "type": "ARRAY", "items": { "type": "STRING" } },
                        "rating": { "type": ["INTEGER", "NULL"] },
                    },
                    "required": ["description", "tags", "rating"],
                },
            },
        });

        self.request(&body, cancel)
    }

    fn probe_connection(&self, cancel: &AtomicBool) -> std::result::Result<(), VendorError> {
        let body = serde_json::json!({
            "contents": [{ "role": "user", "parts": [{ "text": "Reply with the single word OK." }] }],
            "generationConfig": { "maxOutputTokens": 16 },
        });
        self.request(&body, cancel).map(|_| ())
    }
}

impl GeminiAdapter {
    fn request(
        &self,
        body: &serde_json::Value,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        let url = format!("{}/v1beta/models/{}:generateContent", self.base_url, self.model);
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
                .header("x-goog-api-key", &self.api_key)
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
        candidates: Vec<Candidate>,
        #[serde(default, rename = "promptFeedback")]
        prompt_feedback: Option<Feedback>,
    }
    #[derive(Deserialize, Debug)]
    struct Feedback {
        #[serde(default, rename = "blockReason")]
        block_reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct Candidate {
        content: Content,
        #[serde(default, rename = "finishReason")]
        finish_reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct Content {
        parts: Vec<Part>,
    }
    #[derive(Deserialize)]
    struct Part {
        #[serde(default)]
        text: Option<String>,
    }

    let parsed: Response = serde_json::from_str(body).map_err(|e| {
        VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message: format!("unparseable: {e}"),
            http_status: None,
            provider_code: None,
            request_id: None,
        }
    })?;

    if let Some(fb) = &parsed.prompt_feedback {
        if let Some(reason) = &fb.block_reason {
            if !reason.is_empty() {
                return Err(VendorError {
                    kind: VendorErrorKind::Refused,
                    message: format!("blocked: {reason}"),
                    http_status: None,
                    provider_code: None,
                    request_id: None,
                });
            }
        }
    }

    let candidate = parsed.candidates.first().ok_or(VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: "no candidates".into(),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;

    if let Some(ref reason) = candidate.finish_reason {
        if ["SAFETY", "BLOCKLIST", "PROHIBITED_CONTENT"].contains(&reason.as_str()) {
            return Err(VendorError {
                kind: VendorErrorKind::Refused,
                message: format!("refused: {reason}"),
                http_status: None,
                provider_code: None,
                request_id: None,
            });
        }
    }

    let text: String = candidate.content.parts.iter().filter_map(|p| p.text.as_deref()).collect::<Vec<_>>().join("\n");
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
        401 | 403 => VendorErrorKind::Auth,
        400 => {
            if body.to_lowercase().contains("api key") {
                VendorErrorKind::Auth
            } else {
                VendorErrorKind::InvalidResponse
            }
        }
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

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new("https://generativelanguage.googleapis.com", "AIza-xxx", "gemini-1.5-flash").unwrap()
    }

    #[test]
    fn new_validates() {
        assert!(GeminiAdapter::new("", "k", "m").is_err());
        assert!(GeminiAdapter::new("https://x.com", "", "m").is_err());
        assert!(GeminiAdapter::new("https://x.com", "k", "").is_err());
        assert_eq!(adapter().vendor(), VendorId::Gemini);
    }

    #[test]
    fn extract_reads_text() {
        let body = r#"{"candidates":[{"content":{"parts":[{"text":"{\"tags\":[\"beach\"]}"}]}}]}"#;
        assert!(extract_content(body).unwrap().contains("beach"));
    }

    #[test]
    fn extract_detects_block_reason() {
        let body = r#"{"promptFeedback":{"blockReason":"SAFETY"},"candidates":[]}"#;
        assert_eq!(extract_content(body).unwrap_err().kind, VendorErrorKind::Refused);
    }

    #[test]
    fn extract_detects_finish_reason() {
        let body = r#"{"candidates":[{"finishReason":"SAFETY","content":{"parts":[{"text":"ok"}]}}]}"#;
        assert_eq!(extract_content(body).unwrap_err().kind, VendorErrorKind::Refused);
    }
}
