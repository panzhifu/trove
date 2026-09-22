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

pub struct DashScopeAdapter {
    base_url: String,
    api_key: String,
    model: String,
}

impl DashScopeAdapter {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Result<Self> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::Validation("no base URL".into()));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Error::Validation(format!(
                "must be http(s), got {base_url:?}"
            )));
        }
        let model = model.trim();
        if model.is_empty() {
            return Err(Error::Validation("no model name".into()));
        }
        if api_key.trim().is_empty() {
            return Err(Error::Validation("DashScope requires an API key".into()));
        }
        Ok(Self {
            base_url,
            api_key: api_key.trim().to_string(),
            model: model.to_string(),
        })
    }
}

impl VendorAdapter for DashScopeAdapter {
    fn vendor(&self) -> VendorId {
        VendorId::DashScope
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

        let mut content: Vec<serde_json::Value> = vec![serde_json::json!({ "text": user_text })];
        if let Some(jpeg) = &request.thumbnail_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            content.push(serde_json::json!({ "image": format!("data:image/jpeg;base64,{b64}") }));
        }
        if let Some(jpeg) = &request.contact_sheet_jpeg {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
            content.push(serde_json::json!({ "image": format!("data:image/jpeg;base64,{b64}") }));
        }

        let body = serde_json::json!({
            "model": self.model,
            "input": {
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": content },
                ],
            },
            "parameters": {
                "result_format": "message",
                "response_format": { "type": "json_object" },
                "temperature": 0.2,
            },
        });

        self.request(&body, cancel)
    }

    fn probe_connection(&self, cancel: &AtomicBool) -> std::result::Result<(), VendorError> {
        let body = serde_json::json!({
            "model": self.model,
            "input": {
                "messages": [{ "role": "user", "content": [{ "text": "Reply with the single word OK." }] }],
            },
        });
        self.request(&body, cancel).map(|_| ())
    }
}

impl DashScopeAdapter {
    fn request(
        &self,
        body: &serde_json::Value,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        let url = format!(
            "{}/api/v1/services/aigc/multimodal-generation/generation",
            self.base_url
        );
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
                .header("Authorization", &format!("Bearer {}", self.api_key))
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
        output: Output,
    }
    #[derive(Deserialize)]
    struct Output {
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
    }
    #[derive(Deserialize)]
    struct Part {
        #[serde(default)]
        text: Option<String>,
    }

    let parsed: Response = serde_json::from_str(body).map_err(|e| VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: format!("unparseable: {e}"),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;

    let choice = parsed.output.choices.first().ok_or(VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: "no choices".into(),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;

    match &choice.message.content {
        Content::Text(text) if !text.trim().is_empty() => Ok(text.clone()),
        Content::Parts(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
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
        Content::Text(_) => Err(VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message: "empty".into(),
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

fn parse_error(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn extract_code(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .get("code")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> DashScopeAdapter {
        DashScopeAdapter::new("https://dashscope.aliyuncs.com", "sk-xxx", "qwen-vl-plus").unwrap()
    }

    #[test]
    fn new_validates() {
        assert!(DashScopeAdapter::new("", "k", "m").is_err());
        assert!(DashScopeAdapter::new("https://x.com", "", "m").is_err());
        assert!(DashScopeAdapter::new("https://x.com", "k", "").is_err());
        assert_eq!(adapter().vendor(), VendorId::DashScope);
    }

    #[test]
    fn extract_reads_text() {
        let body = r#"{"output":{"choices":[{"message":{"content":"{\"tags\":[\"beach\"]}"}}]}}"#;
        assert!(extract_content(body).unwrap().contains("beach"));
    }

    #[test]
    fn extract_reads_parts() {
        let body = r#"{"output":{"choices":[{"message":{"content":[{"text":"{\"tags\":[\"beach\"]}"}]}}]}}"#;
        assert!(extract_content(body).unwrap().contains("beach"));
    }

    #[test]
    fn extract_rejects_empty() {
        assert!(extract_content(r#"{"output":{"choices":[]}}"#).is_err());
    }
}
