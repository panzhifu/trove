use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::ai::analysis::{system_prompt, user_text_lines};
use crate::ai::http::read_body;
use crate::error::{Error, Result};

use super::{VendorAdapter, VendorError, VendorErrorKind, VendorId};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const BACKOFF: Duration = Duration::from_millis(1_500);
const MAX_BODY: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuredOutputMode {
    JsonSchema,
    JsonObject,
    Text,
}

pub struct OpenAiAdapter {
    base_url: String,
    api_key: String,
    model: String,
    mode: OnceLock<StructuredOutputMode>,
}

impl OpenAiAdapter {
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
        Ok(Self {
            base_url,
            api_key: api_key.trim().to_string(),
            model: model.to_string(),
            mode: OnceLock::new(),
        })
    }

    fn build_user_content(
        request: &crate::ai::analysis::AiAnalysisRequest,
        text: &str,
    ) -> serde_json::Value {
        let mut parts: Vec<serde_json::Value> =
            vec![serde_json::json!({ "type": "text", "text": text })];

        if let Some(jpeg) = &request.thumbnail_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:image/jpeg;base64,{b64}"), "detail": "low" },
            }));
        }
        if let Some(jpeg) = &request.contact_sheet_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:image/jpeg;base64,{b64}"), "detail": "low" },
            }));
        }

        if parts.len() == 1 {
            serde_json::Value::String(text.to_string())
        } else {
            serde_json::Value::Array(parts)
        }
    }

    fn response_format(mode: StructuredOutputMode, language: &str) -> serde_json::Value {
        match mode {
            StructuredOutputMode::JsonSchema => serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "trove_asset_analysis",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "description": { "type": ["string", "null"], "description": format!("Description in {language}, or null.") },
                            "tags": { "type": "array", "items": { "type": "string" }, "description": format!("Keywords in {language}.") },
                            "rating": { "type": ["integer", "null"], "description": "Aesthetic score 1-5, or null." },
                        },
                        "required": ["description", "tags", "rating"],
                    },
                },
            }),
            StructuredOutputMode::JsonObject => serde_json::json!({ "type": "json_object" }),
            StructuredOutputMode::Text => serde_json::json!({ "type": "text" }),
        }
    }
}

impl VendorAdapter for OpenAiAdapter {
    fn vendor(&self) -> VendorId {
        VendorId::OpenAI
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
        let user_content = Self::build_user_content(request, &user_text);

        let modes: Vec<StructuredOutputMode> = match self.mode.get() {
            Some(m) => vec![*m],
            None => vec![
                StructuredOutputMode::JsonSchema,
                StructuredOutputMode::JsonObject,
                StructuredOutputMode::Text,
            ],
        };

        let mut last_error: Option<VendorError> = None;
        for mode in &modes {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(VendorError {
                    kind: VendorErrorKind::Timeout,
                    message: "cancelled".into(),
                    http_status: None,
                    provider_code: None,
                    request_id: None,
                });
            }

            let mut body = serde_json::json!({
                "model": self.model,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user_content },
                ],
                "temperature": 0.2,
            });
            if *mode != StructuredOutputMode::Text {
                body["response_format"] = Self::response_format(*mode, &request.language);
            }

            match self.request(&body, cancel) {
                Ok(text) => {
                    let _ = self.mode.set(*mode);
                    return Ok(text);
                }
                Err(err) if err.kind.is_transient() || is_format_rejection(&err) => {
                    last_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.expect("loop ran"))
    }

    fn probe_connection(&self, cancel: &AtomicBool) -> std::result::Result<(), VendorError> {
        self.analyze(
            &crate::ai::analysis::AiAnalysisRequest {
                asset_id: uuid::Uuid::nil(),
                display_name: "probe".into(),
                file_name: "probe".into(),
                mime: "text/plain".into(),
                media_type: crate::ai::analysis::MediaType::Other,
                thumbnail_jpeg: None,
                contact_sheet_jpeg: None,
                language: "en".into(),
                enabled_fields: crate::ai::analysis::AiAnalysisFields::default(),
                existing_tag_names: vec![],
                vocabulary: vec![],
                metadata_lines: vec![],
                settings: crate::ai::analysis::AiAnalysisSettings::default(),
            },
            cancel,
        )?;
        Ok(())
    }
}

impl OpenAiAdapter {
    fn request(
        &self,
        body: &serde_json::Value,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body_str = body.to_string();

        let mut last_error: Option<VendorError> = None;
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

            let mut req = ureq::post(&url).header("Content-Type", "application/json");
            if !self.api_key.is_empty() {
                req = req.header("Authorization", &format!("Bearer {}", self.api_key));
            }
            let response = req
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
                        return extract_content(&text, &self.model);
                    }
                    let detail = parse_error(&text);
                    let kind = classify_http_error(status, &text);
                    let err = VendorError {
                        kind,
                        message: detail.unwrap_or_else(|| format!("HTTP {status}")),
                        http_status: Some(status),
                        provider_code: extract_code(&text),
                        request_id: None,
                    };
                    if !kind.is_transient() {
                        return Err(err);
                    }
                    last_error = Some(err);
                }
                Err(error) => {
                    last_error = Some(VendorError {
                        kind: VendorErrorKind::Network,
                        message: error.to_string(),
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

fn extract_content(body: &str, _model: &str) -> std::result::Result<String, VendorError> {
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
        #[serde(default)]
        refusal: Option<String>,
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

    let parsed: Response = serde_json::from_str(body).map_err(|e| VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: format!("unparseable JSON: {e}"),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;

    let choice = parsed.choices.first().ok_or(VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: "no choices".into(),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;

    if let Some(refusal) = &choice.message.refusal {
        if !refusal.trim().is_empty() {
            return Err(VendorError {
                kind: VendorErrorKind::Refused,
                message: refusal.trim().to_string(),
                http_status: None,
                provider_code: None,
                request_id: None,
            });
        }
    }

    match &choice.message.content {
        Content::Text(text) if !text.trim().is_empty() => Ok(text.clone()),
        Content::Parts(parts) => {
            let text: String = parts.iter().filter_map(|p| p.text.as_deref()).collect::<Vec<_>>().join("\n");
            if text.trim().is_empty() {
                Err(VendorError {
                    kind: VendorErrorKind::InvalidResponse,
                    message: "empty reply".into(),
                    http_status: None,
                    provider_code: None,
                    request_id: None,
                })
            } else {
                Ok(text)
            }
        }
        Content::Null | Content::Text(_) => Err(VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message: "empty reply".into(),
            http_status: None,
            provider_code: None,
            request_id: None,
        }),
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

fn is_format_rejection(err: &VendorError) -> bool {
    let Some(status) = err.http_status else { return false };
    if status != 400 && status != 422 {
        return false;
    }
    let lower = err.message.to_lowercase();
    let fmt = lower.contains("response_format") || lower.contains("json_schema") || lower.contains("json_object");
    let rejected = lower.contains("unsupported") || lower.contains("not supported") || lower.contains("does not support");
    fmt && rejected
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
    use crate::ai::analysis::{AiAnalysisFields, AiAnalysisRequest, AiAnalysisSettings, MediaType};

    fn sample_request() -> AiAnalysisRequest {
        AiAnalysisRequest {
            asset_id: uuid::Uuid::new_v4(),
            display_name: "sunset.png".into(),
            file_name: "sunset.png".into(),
            mime: "image/png".into(),
            media_type: MediaType::Image,
            thumbnail_jpeg: Some(vec![0xFF, 0xD8, 0xFF]),
            contact_sheet_jpeg: None,
            language: "en".into(),
            enabled_fields: AiAnalysisFields::default(),
            existing_tag_names: vec!["beach".into()],
            vocabulary: vec!["beach".into(), "sunset".into()],
            metadata_lines: vec!["dimensions: 4000x3000 (landscape)".into()],
            settings: AiAnalysisSettings::default(),
        }
    }

    #[test]
    fn new_validates() {
        assert!(OpenAiAdapter::new("", "k", "m").is_err());
        assert!(OpenAiAdapter::new("https://x.com", "k", "").is_err());
        assert!(OpenAiAdapter::new("ftp://x.com", "k", "m").is_err());
    }

    #[test]
    fn user_content_text_only() {
        let req = AiAnalysisRequest {
            thumbnail_jpeg: None,
            ..sample_request()
        };
        let content = OpenAiAdapter::build_user_content(&req, "metadata");
        assert_eq!(content.as_str(), Some("metadata"));
    }

    #[test]
    fn user_content_with_thumbnail() {
        let req = sample_request();
        let content = OpenAiAdapter::build_user_content(&req, "metadata");
        let arr = content.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn response_format_schema_has_strict() {
        let fmt = OpenAiAdapter::response_format(StructuredOutputMode::JsonSchema, "en");
        assert_eq!(fmt["json_schema"]["name"], "trove_asset_analysis");
    }

    #[test]
    fn extract_content_reads_text() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"{\"tags\":[\"beach\"]}"}}]}"#;
        assert_eq!(extract_content(body, "m").unwrap(), "{\"tags\":[\"beach\"]}");
    }

    #[test]
    fn extract_content_reads_parts() {
        let body = r#"{"choices":[{"message":{"content":[{"type":"text","text":"hello"}]}}]}"#;
        assert_eq!(extract_content(body, "m").unwrap(), "hello");
    }

    #[test]
    fn extract_content_rejects_empty() {
        assert!(extract_content(r#"{"choices":[{"message":{"content":"   "}}]}"#, "m").is_err());
    }

    #[test]
    fn extract_content_rejects_refusal() {
        let body = r#"{"choices":[{"message":{"content": null, "refusal":"harmful"}}]}"#;
        let err = extract_content(body, "m").unwrap_err();
        assert_eq!(err.kind, VendorErrorKind::Refused);
    }

    #[test]
    fn format_rejection_detection() {
        assert!(is_format_rejection(&VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message: "response_format json_schema unsupported".into(),
            http_status: Some(400),
            provider_code: None,
            request_id: None,
        }));
        assert!(!is_format_rejection(&VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message: "model not found".into(),
            http_status: Some(400),
            provider_code: None,
            request_id: None,
        }));
    }
}
