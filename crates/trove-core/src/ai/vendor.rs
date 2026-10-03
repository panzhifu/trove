//! AI analysis vendor adapters: the seam between Trove and the model servers.
//!
//! Trove owns the results — where they live, how they are scored, when they
//! are stale — and stays deliberately unopinionated about where they come
//! from. That seam is [`VendorAdapter`]: one trait with one concrete shape
//! per API family. Most mainstream servers speak the OpenAI shape
//! (`/chat/completions` with `image_url` parts), so OpenAI, Moonshot, Zhipu,
//! Volcengine Ark, SiliconFlow and any OpenAI-compatible relay or local
//! server share one adapter; Anthropic and Gemini have their own. A future
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
///
/// Every variant after [`VendorId::DashScope`] speaks the OpenAI wire shape
/// and rides [`OpenAiAdapter`] — they differ only in the default endpoint
/// and the API-key realm, which is what [`VendorId::default_base_url`] and
/// the settings page carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VendorId {
    OpenAI,
    Anthropic,
    Gemini,
    DashScope,
    /// Moonshot AI (Kimi) — vision models `moonshot-v1-*-vision-preview`,
    /// `kimi-latest`.
    Moonshot,
    /// Zhipu AI (GLM) — vision models `glm-4v-plus`, `glm-4v-flash`.
    Zhipu,
    /// Volcengine Ark (Doubao) — vision models `doubao-*-vision-*`; the
    /// endpoint id is an inference access point the user creates.
    Volcengine,
    /// SiliconFlow — an aggregator hosting Qwen-VL, GLM-4V and friends
    /// behind one OpenAI-compatible key.
    SiliconFlow,
    /// DeepSeek — OpenAI-compatible. `deepseek-flash` takes images (the
    /// data-URL shape verified against their API); `deepseek-v4-pro` is
    /// text-only, so a run pinned to it degrades through the
    /// image-rejection path and the outcome says so.
    DeepSeek,
}

impl VendorId {
    pub fn as_str(self) -> &'static str {
        match self {
            VendorId::OpenAI => "openai",
            VendorId::Anthropic => "anthropic",
            VendorId::Gemini => "gemini",
            VendorId::DashScope => "dashscope",
            VendorId::Moonshot => "moonshot",
            VendorId::Zhipu => "zhipu",
            VendorId::Volcengine => "volcengine",
            VendorId::SiliconFlow => "siliconflow",
            VendorId::DeepSeek => "deepseek",
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
            VendorId::Moonshot => "https://api.moonshot.cn/v1",
            VendorId::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            VendorId::Volcengine => "https://ark.cn-beijing.volces.com/api/v3",
            VendorId::SiliconFlow => "https://api.siliconflow.cn/v1",
            VendorId::DeepSeek => "https://api.deepseek.com/v1",
        }
    }

    /// The vendor's known analysis models, for the settings page's
    /// quick-pick dropdown. Suggestions, not an allowlist: the model field
    /// stays free text, because relays, endpoint ids (`ep-…`), and models
    /// newer than this table all have to fit. Vision-capable models first —
    /// analysis sends images, so a text-only entry (DeepSeek's
    /// `deepseek-v4-pro`) degrades through the image-rejection path.
    pub fn models(self) -> &'static [&'static str] {
        match self {
            VendorId::OpenAI => &["gpt-4o-mini", "gpt-4o", "gpt-4.1-mini", "gpt-4.1"],
            VendorId::Anthropic => &["claude-sonnet-4-5", "claude-haiku-4-5", "claude-opus-4-1"],
            VendorId::Gemini => &["gemini-2.5-flash", "gemini-2.5-pro", "gemini-2.0-flash"],
            VendorId::DashScope => &["qwen-vl-max", "qwen-vl-plus"],
            VendorId::Moonshot => &[
                "moonshot-v1-8k-vision-preview",
                "moonshot-v1-32k-vision-preview",
                "kimi-latest",
            ],
            VendorId::Zhipu => &["glm-4v-plus", "glm-4v-flash"],
            VendorId::Volcengine => &["doubao-1.5-vision-pro-32k", "doubao-1.5-vision-lite"],
            VendorId::SiliconFlow => &[
                "Qwen/Qwen2.5-VL-72B-Instruct",
                "Qwen/Qwen2.5-VL-32B-Instruct",
                "deepseek-ai/deepseek-vl2",
            ],
            VendorId::DeepSeek => &["deepseek-flash", "deepseek-v4-pro"],
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
            "moonshot" | "moonshot-kimi" | "kimi" => Ok(VendorId::Moonshot),
            "zhipu" | "glm" => Ok(VendorId::Zhipu),
            "volcengine" | "ark" | "doubao" => Ok(VendorId::Volcengine),
            "siliconflow" => Ok(VendorId::SiliconFlow),
            "deepseek" => Ok(VendorId::DeepSeek),
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
        // One wire shape for every OpenAI-compatible server: the vendor id
        // only names whose key and endpoint the settings page pre-fills.
        VendorId::Moonshot
        | VendorId::Zhipu
        | VendorId::Volcengine
        | VendorId::SiliconFlow
        | VendorId::DeepSeek => Ok(Box::new(OpenAiAdapter::new_for(
            vendor, base_url, api_key, model,
        )?)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Every vendor id round-trips through its stored string and builds an
    /// adapter that answers with the same identity: the settings page and
    /// the analysis markers both key off these strings, so an id that fails
    /// either half would silently orphan a stored config or misfile a run.
    #[test]
    fn every_vendor_id_round_trips_and_builds_its_adapter() {
        let all = [
            VendorId::OpenAI,
            VendorId::Anthropic,
            VendorId::Gemini,
            VendorId::DashScope,
            VendorId::Moonshot,
            VendorId::Zhipu,
            VendorId::Volcengine,
            VendorId::SiliconFlow,
            VendorId::DeepSeek,
        ];
        for vendor in all {
            assert_eq!(
                VendorId::from_str(vendor.as_str()).unwrap(),
                vendor,
                "{} does not round-trip",
                vendor.as_str()
            );
            let url = vendor.default_base_url();
            assert!(
                url.starts_with("https://"),
                "{} has no official endpoint",
                vendor.as_str()
            );
            let adapter = build_adapter(vendor, url, "key", "model").expect("adapter builds");
            assert_eq!(
                adapter.vendor(),
                vendor,
                "the adapter reports the wrong vendor"
            );
        }
        assert!(VendorId::from_str("nope").is_err());
    }

    /// The OpenAI-shape vendors are aliases of one adapter. The aliases
    /// cover the names users actually type, and each pre-filled endpoint is
    /// the vendor's real API root — the settings page writes it straight
    /// into the config when a vendor is picked.
    #[test]
    fn openai_shaped_aliases_parse_and_prefill_their_endpoints() {
        assert_eq!(VendorId::from_str("kimi").unwrap(), VendorId::Moonshot);
        assert_eq!(VendorId::from_str("glm").unwrap(), VendorId::Zhipu);
        assert_eq!(VendorId::from_str("doubao").unwrap(), VendorId::Volcengine);
        assert_eq!(
            VendorId::from_str("ARK").unwrap(),
            VendorId::Volcengine,
            "ids parse case-insensitively"
        );
        assert_eq!(
            VendorId::from_str("moonshot").unwrap().default_base_url(),
            "https://api.moonshot.cn/v1"
        );
        assert_eq!(
            VendorId::from_str("zhipu").unwrap().default_base_url(),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(
            VendorId::from_str("volcengine").unwrap().default_base_url(),
            "https://ark.cn-beijing.volces.com/api/v3"
        );
        assert_eq!(
            VendorId::from_str("siliconflow")
                .unwrap()
                .default_base_url(),
            "https://api.siliconflow.cn/v1"
        );
        assert_eq!(
            VendorId::from_str("deepseek").unwrap().default_base_url(),
            "https://api.deepseek.com/v1"
        );
    }

    /// The settings quick-pick is built from these lists: a model entry
    /// must be a usable id (no surrounding whitespace, no duplicates that
    /// would make two menu rows check the same value).
    #[test]
    fn the_preset_model_lists_are_clean() {
        for vendor in [
            VendorId::OpenAI,
            VendorId::Anthropic,
            VendorId::Gemini,
            VendorId::DashScope,
            VendorId::Moonshot,
            VendorId::Zhipu,
            VendorId::Volcengine,
            VendorId::SiliconFlow,
            VendorId::DeepSeek,
        ] {
            let models = vendor.models();
            assert!(
                !models.is_empty(),
                "{} has no preset models",
                vendor.as_str()
            );
            let mut unique = std::collections::HashSet::new();
            for model in models {
                let trimmed = model.trim();
                assert_eq!(trimmed, *model, "{}: padded model id", vendor.as_str());
                assert!(!trimmed.is_empty(), "{}: empty model id", vendor.as_str());
                assert!(
                    unique.insert(*model),
                    "{}: duplicate model {model}",
                    vendor.as_str()
                );
            }
        }
    }
}
