//! AI analysis vendor adapters: the seam between Trove and the model servers.
//!
//! Trove owns the results — where they live, how they are scored, when they
//! are stale — and stays deliberately unopinionated about where they come
//! from. That seam is [`VendorAdapter`]: one trait with one concrete shape
//! per provider family (OpenAI, Anthropic, Gemini, DashScope). A future
//! adapter (a local multimodal model, an on-device Core ML / MediaPipe
//! implementation) implements the same trait and inherits the whole
//! post-processing and task stack.

use super::analysis::AiAnalysisRequest;

mod anthropic;
mod dashscope;
mod gemini;
mod openai;

pub use anthropic::AnthropicAdapter;
pub use dashscope::DashScopeAdapter;
pub use gemini::GeminiAdapter;
pub use openai::OpenAiAdapter;

use crate::error::Result;

/// Supported AI vendor identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VendorId {
    OpenAI,
    Anthropic,
    Gemini,
    DashScope,
}

impl VendorId {
    pub fn as_str(self) -> &'static str {
        match self {
            VendorId::OpenAI => "openai",
            VendorId::Anthropic => "anthropic",
            VendorId::Gemini => "gemini",
            VendorId::DashScope => "dashscope",
        }
    }

    /// The vendor's official API endpoint, in the shape the adapters'
    /// `new(base_url, ..)` expects — the prefix they append their operation
    /// path to (`/chat/completions`, `/v1/messages`, …). Settings use it to
    /// pre-fill the endpoint when the user picks a vendor; relays and local
    /// servers are typed over it afterwards.
    pub fn default_base_url(self) -> &'static str {
        match self {
            VendorId::OpenAI => "https://api.openai.com/v1",
            VendorId::Anthropic => "https://api.anthropic.com",
            VendorId::Gemini => "https://generativelanguage.googleapis.com",
            VendorId::DashScope => "https://dashscope.aliyuncs.com",
        }
    }
}

impl std::str::FromStr for VendorId {
    type Err = crate::error::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "openai" => Ok(VendorId::OpenAI),
            "anthropic" => Ok(VendorId::Anthropic),
            "gemini" => Ok(VendorId::Gemini),
            "dashscope" => Ok(VendorId::DashScope),
            other => Err(crate::error::Error::Validation(format!(
                "unknown AI vendor {other:?}"
            ))),
        }
    }
}

/// Discriminated error kind used by every vendor adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VendorErrorKind {
    /// Invalid or missing API key.
    Auth,
    /// Key valid but lacks permission for this model / feature.
    Permission,
    /// Quota exhausted / billing overrun.
    Quota,
    /// Transient network failure (DNS, connect, TLS).
    Network,
    /// Rate limited (429) but not quota.
    RateLimit,
    /// Request timed out or was aborted by the caller.
    Timeout,
    /// The server answered 2xx but the body was not the shape we need.
    InvalidResponse,
    /// The server refused the request (content filter, policy).
    Refused,
}

impl VendorErrorKind {
    /// Whether this kind is worth retrying without changing the request.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            VendorErrorKind::Network
                | VendorErrorKind::RateLimit
                | VendorErrorKind::Timeout
                | VendorErrorKind::InvalidResponse
        )
    }
}

/// An error raised by a vendor adapter when an analysis fails.
#[derive(Debug, thiserror::Error)]
#[error("AI vendor error ({kind:?}): {message}")]
pub struct VendorError {
    pub kind: VendorErrorKind,
    pub message: String,
    /// HTTP status when available.
    pub http_status: Option<u16>,
    /// Provider-specific error code (e.g. OpenAI's `invalid_api_key`).
    pub provider_code: Option<String>,
    /// Request id returned by the provider, for diagnostics.
    pub request_id: Option<String>,
}

/// A family of multimodal analysis models.
///
/// Implementations must be deterministic about identity: two adapters
/// producing different results must produce different [`VendorAdapter::id`]
/// strings, because that string is the `model_version` key every analysed
/// asset is filed under.
pub trait VendorAdapter: Send + Sync {
    /// Stable vendor identifier.
    fn vendor(&self) -> VendorId;

    /// Identity stored in `model_version`, e.g. `gpt-4o-mini-2024-07-18`.
    fn model_version(&self) -> &str;

    /// Analyse one asset. The result is free-form text + JSON; parsing and
    /// post-processing belong to the caller — see
    /// [`crate::ai::analysis::post_process`].
    fn analyze(
        &self,
        request: &AiAnalysisRequest,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<String, VendorError>;

    /// Lightweight credential / reachability check. Must not require vision
    /// payloads or structured output envelopes (test-connection).
    fn probe_connection(
        &self,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<(), VendorError>;
}

/// Build an adapter from a config descriptor.
pub fn build_adapter(
    vendor: VendorId,
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<Box<dyn VendorAdapter>> {
    match vendor {
        VendorId::OpenAI => Ok(Box::new(OpenAiAdapter::new(base_url, api_key, model)?)),
        VendorId::Anthropic => Ok(Box::new(AnthropicAdapter::new(base_url, api_key, model)?)),
        VendorId::Gemini => Ok(Box::new(GeminiAdapter::new(base_url, api_key, model)?)),
        VendorId::DashScope => Ok(Box::new(DashScopeAdapter::new(base_url, api_key, model)?)),
    }
}

/// Build the adapter named by the stored analysis configuration.
///
/// The `vendor` string is parsed here rather than at call sites so every
/// caller — the app's settings probe, the CLI, a future scheduler — agrees on
/// what an unknown vendor means (a clean error, not a silent OpenAI default).
pub fn build_from_config(
    config: &crate::config::AiAnalysisConfig,
) -> Result<Box<dyn VendorAdapter>> {
    let vendor: VendorId = config.vendor.parse()?;
    build_adapter(vendor, &config.base_url, &config.api_key, &config.model)
}
