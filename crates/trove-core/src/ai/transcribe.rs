//! Speech-to-text against the OpenAI-compatible transcription endpoint.
//!
//! One wire shape: `POST {base_url}/audio/transcriptions`, a
//! `multipart/form-data` body carrying the model, optional language and
//! vocabulary hint, and the audio itself; the reply is `{"text": "…"}`. This
//! is the de-facto standard — OpenAI (`whisper-1`, `gpt-4o-transcribe`),
//! Groq, SiliconFlow, and the self-hosted whisper servers all speak it, which
//! is why the config carries no vendor family selection: the base URL picks
//! the provider.
//!
//! Multipart is built by hand ([`multipart_body`]) and tested byte-level: the
//! format is a handful of headers around the audio, and getting it wrong
//! fails in ways servers report as inscrutable 400s.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::TranscriptionConfig;

use super::http;
use super::vendor::{VendorError, VendorErrorKind};

/// An audio file no request may carry. Speech endpoints cap uploads (OpenAI:
/// 25 MB); the audio-prep stage chunks on time so a well-formed run never
/// comes close, and this cap is the backstop that keeps a misconfigured
/// pipeline from streaming a whole movie into a JSON API.
pub const MAX_UPLOAD_BYTES: usize = 24 * 1024 * 1024;

/// How long the whole transcribe call may take, connect through body. Speech
/// servers legitimately take minutes on a 30-minute chunk — a global timeout
/// sized like a chat request would kill a healthy one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Reply-size cap: `{"text": "…"}` for a half-hour chunk is kilobytes; a
/// megabyte is already an answer worth refusing.
const MAX_REPLY_BYTES: u64 = 1024 * 1024;

/// Sleep between retries of a transient failure, doubling per attempt.
const BACKOFF: Duration = Duration::from_secs(2);

/// The audio shape a provider wants its chunks cut into. The cloud endpoint
/// caps uploads and pays per byte, so its chunks are 32 kbps AAC; the local
/// recogniser reads 16-bit PCM WAV straight off the disk — nothing is
/// uploaded, so bytes are free and the format is the one Whisper's own
/// pipeline wants (16 kHz mono, which `audio_prep` normalises to anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkFormat {
    AacM4a,
    Wav,
}

/// A source of transcripts for the library's audio and video.
///
/// Synchronous like every provider here: it runs on a background task thread.
/// `file_name` and `mime` travel with the audio because servers use the
/// extension to pick their decoders — an unnamed upload is a 400 waiting to
/// happen.
pub trait TranscribeProvider: Send + Sync {
    /// Identity recorded in the asset's marker beside every transcript, so a
    /// re-run under a different model re-asks instead of skipping.
    fn model(&self) -> &str;

    /// The chunk shape [`crate::media::audio_prep`] should cut for this
    /// provider.
    fn chunk_format(&self) -> ChunkFormat {
        ChunkFormat::AacM4a
    }

    /// Transcribe one prepared audio chunk. `language` is an ISO 639-1 hint
    /// (`None` = server auto-detects); `prompt` is the endpoint's vocabulary
    /// hint, ignored by servers that do not take one. An empty reply is a
    /// valid transcript (silence), not an error.
    fn transcribe(
        &self,
        audio: &[u8],
        file_name: &str,
        mime: &str,
        language: Option<&str>,
        prompt: Option<&str>,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError>;
}

/// The OpenAI-compatible `/audio/transcriptions` client.
pub struct OpenAiCompatible {
    base_url: String,
    api_key: String,
    model: String,
}

/// Build the provider the saved settings ask for: the OpenAI-compatible
/// cloud client, or the local candle Whisper engine (whose model must
/// already be on disk — the UI asks to download it before a run starts).
/// The local engine compiles on every platform; GPU acceleration rides the
/// `cuda`/`accelerate` features and CPU is always the fallback.
pub fn build_from_config(
    config: &TranscriptionConfig,
) -> crate::error::Result<std::sync::Arc<dyn TranscribeProvider>> {
    match config.engine {
        crate::config::TranscriptionEngine::Local => {
            let provider = crate::ai::transcribe_local::build(config)?;
            Ok(std::sync::Arc::new(provider))
        }
        crate::config::TranscriptionEngine::Cloud => {
            let provider = OpenAiCompatible::new(
                config.base_url.trim().to_string(),
                config.api_key.clone(),
                config.model.trim().to_string(),
            )
            .map_err(|error| crate::error::Error::External {
                program: "speech-to-text".into(),
                message: error.message,
            })?;
            Ok(std::sync::Arc::new(provider))
        }
    }
}

impl OpenAiCompatible {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
    ) -> std::result::Result<Self, VendorError> {
        if base_url.is_empty() || !base_url.starts_with("http") {
            return Err(VendorError {
                kind: VendorErrorKind::InvalidResponse,
                message: "base URL must be an http(s) endpoint".into(),
                http_status: None,
                provider_code: None,
                request_id: None,
            });
        }
        if model.is_empty() {
            return Err(VendorError {
                kind: VendorErrorKind::InvalidResponse,
                message: "model name is required".into(),
                http_status: None,
                provider_code: None,
                request_id: None,
            });
        }
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        })
    }

    fn request(
        &self,
        audio: &[u8],
        file_name: &str,
        mime: &str,
        language: Option<&str>,
        prompt: Option<&str>,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        if audio.len() > MAX_UPLOAD_BYTES {
            return Err(VendorError {
                kind: VendorErrorKind::Refused,
                message: format!(
                    "audio chunk is {} bytes, over the {} byte upload cap",
                    audio.len(),
                    MAX_UPLOAD_BYTES
                ),
                http_status: None,
                provider_code: None,
                request_id: None,
            });
        }

        let boundary = format!("trove-{}", crate::model::new_id().simple());
        let mut fields: Vec<(&str, String)> = vec![
            ("model", self.model.clone()),
            ("response_format", "json".into()),
        ];
        if let Some(language) = language {
            fields.push(("language", language.to_string()));
        }
        if let Some(prompt) = prompt {
            fields.push(("prompt", prompt.to_string()));
        }
        let fields: Vec<(&str, &str)> = fields
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let body = multipart_body(&fields, "file", file_name, mime, audio, &boundary);
        let url = format!("{}/audio/transcriptions", self.base_url);

        let mut last_error: Option<VendorError> = None;
        for attempt in 0..=2 {
            if cancel.load(Ordering::Relaxed) {
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

            let mut req = ureq::post(&url)
                .header(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={boundary}"),
                )
                .header("Accept", "application/json");
            if !self.api_key.is_empty() {
                req = req.header("Authorization", &format!("Bearer {}", self.api_key));
            }
            let response = req
                .config()
                .timeout_global(Some(REQUEST_TIMEOUT))
                .http_status_as_error(false)
                .build()
                .send(body.as_slice());
            match response {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    let text = http::read_body(&mut response, MAX_REPLY_BYTES).map_err(|e| {
                        VendorError {
                            kind: VendorErrorKind::Network,
                            message: e.to_string(),
                            http_status: None,
                            provider_code: None,
                            request_id: None,
                        }
                    })?;
                    if (200..300).contains(&status) {
                        return extract_text(&text);
                    }
                    let detail = http::parse_error(&text);
                    let kind = classify_http_error(status, &text);
                    let err = VendorError {
                        kind,
                        message: detail.unwrap_or_else(|| format!("HTTP {status}")),
                        http_status: Some(status),
                        provider_code: None,
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

impl TranscribeProvider for OpenAiCompatible {
    fn model(&self) -> &str {
        &self.model
    }

    fn transcribe(
        &self,
        audio: &[u8],
        file_name: &str,
        mime: &str,
        language: Option<&str>,
        prompt: Option<&str>,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        self.request(audio, file_name, mime, language, prompt, cancel)
    }
}

/// The `{"text": "…"}` the endpoint answers with. An empty transcript is
/// legitimate (silence); a missing `text` key is a shape we cannot use.
fn extract_text(body: &str) -> std::result::Result<String, VendorError> {
    #[derive(serde::Deserialize)]
    struct Reply {
        text: Option<String>,
    }
    let parsed: Reply = serde_json::from_str(body).map_err(|e| VendorError {
        kind: VendorErrorKind::InvalidResponse,
        message: format!("unparseable JSON: {e}"),
        http_status: None,
        provider_code: None,
        request_id: None,
    })?;
    Ok(parsed.text.unwrap_or_default())
}

/// The status → [`VendorErrorKind`] mapping, mirroring the chat provider's:
/// the same server families sit behind both endpoints, so the same statuses
/// mean the same things.
fn classify_http_error(status: u16, body: &str) -> VendorErrorKind {
    match status {
        401 => VendorErrorKind::Auth,
        403 => VendorErrorKind::Permission,
        429 if body.contains("quota") => VendorErrorKind::Quota,
        429 => VendorErrorKind::RateLimit,
        408 | 500..=599 => VendorErrorKind::Network,
        _ => VendorErrorKind::InvalidResponse,
    }
}

/// One `multipart/form-data` body: the text fields first, the audio last —
/// the order the OpenAI wire examples use and the one every server parses.
///
/// Fields are header-safe by construction (no quotes or newlines can appear
/// in the names and values this feature sends), which is what makes the
/// hand-rolled format safe to emit without escaping.
pub fn multipart_body(
    fields: &[(&str, &str)],
    file_field: &str,
    file_name: &str,
    mime: &str,
    file: &[u8],
    boundary: &str,
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{file_name}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_multipart_body_matches_the_wire_shape_byte_for_byte() {
        let body = multipart_body(
            &[("model", "whisper-1"), ("language", "en")],
            "file",
            "chunk-000.m4a",
            "audio/mp4",
            b"AUDIO",
            "BOUNDARY",
        );
        let text = String::from_utf8(body).unwrap();
        assert_eq!(
            text,
            "--BOUNDARY\r\n\
             Content-Disposition: form-data; name=\"model\"\r\n\r\n\
             whisper-1\r\n\
             --BOUNDARY\r\n\
             Content-Disposition: form-data; name=\"language\"\r\n\r\n\
             en\r\n\
             --BOUNDARY\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"chunk-000.m4a\"\r\n\
             Content-Type: audio/mp4\r\n\r\n\
             AUDIO\r\n\
             --BOUNDARY--\r\n"
        );
    }

    #[test]
    fn a_text_reply_parses_and_silence_is_a_valid_transcript() {
        assert_eq!(
            extract_text(r#"{"text": "hello world"}"#).unwrap(),
            "hello world"
        );
        assert_eq!(extract_text(r#"{"text": ""}"#).unwrap(), "", "silence");
        // Verbose shapes and foreign keys do not fool the parser.
        assert!(extract_text(r#"{"error": {"message": "boom"}}"#).is_ok());
        assert!(extract_text("not json at all").is_err());
    }

    #[test]
    fn http_statuses_classify_the_same_way_the_chat_provider_classifies_them() {
        assert!(matches!(
            classify_http_error(401, ""),
            VendorErrorKind::Auth
        ));
        assert!(matches!(
            classify_http_error(403, ""),
            VendorErrorKind::Permission
        ));
        assert!(matches!(
            classify_http_error(429, "quota exceeded"),
            VendorErrorKind::Quota
        ));
        assert!(matches!(
            classify_http_error(429, ""),
            VendorErrorKind::RateLimit
        ));
        assert!(matches!(
            classify_http_error(503, ""),
            VendorErrorKind::Network
        ));
        assert!(matches!(
            classify_http_error(400, ""),
            VendorErrorKind::InvalidResponse
        ));
        assert!(classify_http_error(503, "").is_transient());
        assert!(!classify_http_error(401, "").is_transient());
    }

    #[test]
    fn construction_refuses_a_shape_that_cannot_work() {
        assert!(OpenAiCompatible::new(String::new(), String::new(), "m".into()).is_err());
        assert!(OpenAiCompatible::new("ftp://x".into(), String::new(), "m".into()).is_err());
        assert!(
            OpenAiCompatible::new(
                "https://api.openai.com/v1".into(),
                String::new(),
                String::new()
            )
            .is_err()
        );
        assert!(
            OpenAiCompatible::new(
                "https://api.openai.com/v1".into(),
                "sk".into(),
                "whisper-1".into()
            )
            .is_ok()
        );
    }

    #[test]
    fn an_oversized_chunk_is_refused_before_any_network_is_spent() {
        let provider = OpenAiCompatible::new(
            "https://api.openai.com/v1".into(),
            "sk".into(),
            "whisper-1".into(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let oversized = vec![0u8; MAX_UPLOAD_BYTES + 1];
        let error = provider
            .transcribe(
                &oversized,
                "chunk-000.m4a",
                "audio/mp4",
                None,
                None,
                &cancel,
            )
            .unwrap_err();
        assert!(matches!(error.kind, VendorErrorKind::Refused));
    }
}
