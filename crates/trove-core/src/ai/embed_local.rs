//! Local text embeddings: BAAI's BGE checkpoints running on this machine's
//! GPU (or CPU) through candle — a pure-Rust tensor stack, so nothing here
//! compiles or links C++.
//!
//! Two encoder families are supported, chosen by the checkpoint's
//! `model_type`: BERT (the `bge-*-v1.5` checkpoints, `bge-small-zh-v1.5` by
//! default) and XLM-RoBERTa (`bge-m3`, the multilingual checkpoint that is
//! strong in both Chinese and English). The weights are loaded once at
//! construction — construction happens inside the embedding job or the
//! probe's background task, never on the UI thread — and reused for every
//! batch of the run. The decode is the sentence-transformers recipe BGE was
//! trained for: the sequence output pooled at the `[CLS]` token, then
//! L2-normalized so cosine similarity ranks by direction. No instruction
//! prefix is prepended: v1.5 is the generation of BGE trained to retrieve
//! well *without* one, and the fingerprint text (title, description, tags)
//! and the query must go through the exact same pipeline to stay comparable.
//!
//! BGE is a text embedder: rows land in the text space, and there is no
//! image path — a multimodal (CLIP-style) vector store stays a cloud-engine
//! feature. The tokenizer and recipe are language-agnostic: English
//! fingerprints embed fine under the Chinese checkpoint, and `bge-m3` embeds
//! both at full strength.

use std::sync::{Arc, Mutex, OnceLock};

use candle_core::{DType, D, IndexOp as _, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{self, BertModel};
use candle_transformers::models::xlm_roberta::{self, XLMRobertaModel};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

use super::EmbeddingProvider;
use crate::config::local_embedding_identity;
use crate::error::{Error, Result};
use crate::model::EmbeddingSpace;

/// The process-wide cache of the one local embedder the settings point at.
/// Building a provider reads and parses the weights (a 2.27 GB torch pickle
/// for `bge-m3`) and uploads them to the device — seconds of work that a
/// search must not pay per query. Keyed by model id: a re-pick in the
/// settings replaces the entry, and the old weights drop with it.
static CACHE: OnceLock<Mutex<Option<(String, Arc<LocalBert>)>>> = OnceLock::new();

/// Build the local provider for `model`, reusing the cached instance when the
/// settings still point at the same checkpoint. See [`CACHE`].
pub fn build_cached(model: &str) -> Result<Arc<LocalBert>> {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut cached = cache.lock().unwrap();
    if let Some((cached_id, provider)) = cached.as_ref() {
        if cached_id == model {
            return Ok(provider.clone());
        }
    }
    let provider = Arc::new(build(model)?);
    *cached = Some((model.to_string(), provider.clone()));
    Ok(provider)
}

/// Texts per forward pass. The cloud client batches at 64, but a local BERT
/// holds the whole batch's activations in memory; 16 keeps a CPU run at a
/// few hundred megabytes while amortizing the per-pass overhead.
const LOCAL_BATCH: usize = 16;

/// Longest input the local embedder keeps. Every catalog checkpoint is a
/// sentence-size encoder; capping at 512 bounds memory for the one model
/// (`bge-m3`) whose window is thousands of tokens while never touching the
/// fingerprint texts, which are a title, a description and a tag list.
const LOCAL_MAX_TOKENS: usize = 512;

/// The two encoder families the catalog uses. They differ in the weights'
/// key prefix and in the order the attention mask and token-type ids are
/// passed, and nothing else — pooling is `[CLS]` + L2 for both.
enum Backend {
    Bert(BertModel),
    /// `bge-m3` and friends: XLM-RoBERTa, whose weights live under a
    /// `roberta.` prefix and which is loaded without a pooler.
    XlmRoberta(XLMRobertaModel),
}

/// Everything loaded once and held for the provider's lifetime.
struct Loaded {
    backend: Backend,
    tokenizer: Tokenizer,
    dim: usize,
    /// The device the model runs on; the input tensors have to be built on it.
    device: candle_core::Device,
}

pub struct LocalBert {
    loaded: Mutex<Loaded>,
    /// The identity this provider's vectors are stored under — the
    /// selected model's id, so a bigger checkpoint files its rows apart
    /// from the smaller one's.
    identity: String,
}

/// Build the local provider for `model` (a `services::embed_model`
/// catalog id): the model must already be on disk (the UI asks to
/// download it before a run starts), and loading happens here so a
/// broken install fails the run with a clear message instead of at first
/// use.
pub fn build(model: &str) -> Result<LocalBert> {
    let dir = match crate::services::embed_model::status(model) {
        crate::services::embed_model::ModelStatus::Ready { path } => path,
        crate::services::embed_model::ModelStatus::Missing => {
            return Err(Error::External {
                program: "embed-local".into(),
                message: format!("the local embedding model {model} is not downloaded yet"),
            });
        }
    };
    let loaded = load(&dir).map_err(|message| Error::External {
        program: "embed-local".into(),
        message,
    })?;
    Ok(LocalBert {
        loaded: Mutex::new(loaded),
        identity: local_embedding_identity(model),
    })
}

fn load(dir: &std::path::Path) -> std::result::Result<Loaded, String> {
    let config_text = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|e| format!("read config.json: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&config_text).map_err(|e| format!("parse config.json: {e}"))?;
    // `bge-m3` is XLM-RoBERTa; every other catalog checkpoint is BERT. The
    // type decides the loader and the weight-key prefix, nothing else.
    let model_type = value
        .get("model_type")
        .and_then(|v| v.as_str())
        .unwrap_or("bert");
    // The window is the model's own, capped so the one long-window checkpoint
    // cannot hold a huge activation budget for a one-line fingerprint.
    let max_length = value
        .get("max_position_embeddings")
        .and_then(|v| v.as_u64())
        .unwrap_or(LOCAL_MAX_TOKENS as u64)
        .min(LOCAL_MAX_TOKENS as u64) as usize;

    let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| format!("load tokenizer.json: {e}"))?;
    // Every input is cut to the model's window and every batch padded to its
    // own longest member, so the tensors are rectangular without padding the
    // whole library to the window.
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length,
            ..TruncationParams::default()
        }))
        .map_err(|e| format!("configure truncation: {e}"))?;
    tokenizer.with_padding(Some(PaddingParams::default()));

    let (device, backend) = super::local_device::select_device();
    tracing::info!(backend, "local: embedder device");
    // The weights format follows the upstream repo: a safetensors conversion
    // memory-maps (the fast path), a repo that never got one — bge-m3 and the
    // zh base/large checkpoints — ships the torch pickle, read through
    // candle's pth backend into the very same builder API.
    let safetensors = dir.join("model.safetensors");
    let vb = if safetensors.is_file() {
        unsafe {
            // Memory-mapping the weights: the file is the library's own managed
            // download, and the mapping is read-only for the process lifetime.
            VarBuilder::from_mmaped_safetensors(&[safetensors], DType::F32, &device)
        }
        .map_err(|e| format!("load model.safetensors: {e}"))?
    } else {
        VarBuilder::from_pth(dir.join("pytorch_model.bin"), DType::F32, &device)
            .map_err(|e| format!("load pytorch_model.bin: {e}"))?
    };

    let (backend, dim) = match model_type {
        "xlm-roberta" | "roberta" => {
            let config: xlm_roberta::Config = serde_json::from_value(value)
                .map_err(|e| format!("parse config.json (xlm-roberta): {e}"))?;
            let dim = config.hidden_size;
            // XLM-RoBERTa weights are usually stored under a `roberta.`
            // prefix, but a repo that re-exports the bare model has none;
            // detect it from the weights instead of guessing.
            let vb = if vb.contains_tensor("roberta.embeddings.word_embeddings.weight") {
                vb.pp("roberta")
            } else {
                vb
            };
            let model = XLMRobertaModel::new(&config, vb)
                .map_err(|e| format!("build the model: {e}"))?;
            (Backend::XlmRoberta(model), dim)
        }
        _ => {
            let config: bert::Config = serde_json::from_value(value)
                .map_err(|e| format!("parse config.json (bert): {e}"))?;
            let dim = config.hidden_size;
            let model =
                BertModel::load(vb, &config).map_err(|e| format!("build the model: {e}"))?;
            (Backend::Bert(model), dim)
        }
    };

    Ok(Loaded {
        backend,
        tokenizer,
        dim,
        device,
    })
}

impl Loaded {
    /// Pool and normalize one batch: `[batch, seq, hidden]` → one
    /// L2-normalized vector per row, read off the `[CLS]` slot.
    fn embed_batch(&self, texts: &[String]) -> candle_core::Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| candle_core::Error::Msg(format!("tokenize: {e}")))?;
        let len = encodings.first().map_or(0, |e| e.get_ids().len());
        if len == 0 {
            return Ok(Vec::new());
        }
        let input_ids = Tensor::new(
            encodings
                .iter()
                .map(|e| e.get_ids().to_vec())
                .collect::<Vec<_>>(),
            &self.device,
        )?;
        let token_type_ids = Tensor::new(
            encodings
                .iter()
                .map(|e| e.get_type_ids().to_vec())
                .collect::<Vec<_>>(),
            &self.device,
        )?;
        let attention_mask = Tensor::new(
            encodings
                .iter()
                .map(|e| e.get_attention_mask().to_vec())
                .collect::<Vec<_>>(),
            &self.device,
        )?;

        let sequence = match &self.backend {
            Backend::Bert(model) => {
                model.forward(&input_ids, &token_type_ids, Some(&attention_mask))?
            }
            // XLM-RoBERTa takes the attention mask up front and has no pooler;
            // token-type ids are all zeros for this family.
            Backend::XlmRoberta(model) => model.forward(
                &input_ids,
                &attention_mask,
                &token_type_ids,
                None,
                None,
                None,
            )?,
        };
        // `[CLS]` is slot 0 of every row — BGE's pooling slot — and the
        // L2 divide is what makes a dot product a cosine.
        let cls = sequence.i((.., 0))?;
        let l2_norm = cls.sqr()?.sum_keepdim(D::Minus1)?.sqrt()?;
        let vectors = cls
            .broadcast_div(&l2_norm)?
            .to_dtype(DType::F32)?
            .to_vec2()?;
        debug_assert_eq!(vectors.len(), texts.len());
        Ok(vectors)
    }
}

impl EmbeddingProvider for LocalBert {
    fn id(&self) -> &str {
        &self.identity
    }

    fn asset_space(&self) -> EmbeddingSpace {
        EmbeddingSpace::Text
    }

    fn dim(&self) -> Option<usize> {
        Some(self.loaded.lock().unwrap().dim)
    }

    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let loaded = self.loaded.lock().unwrap();
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(LOCAL_BATCH) {
            out.extend(loaded
                .embed_batch(chunk)
                .map_err(|error| Error::External {
                    program: "embed-local".into(),
                    message: error.to_string(),
                })?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity the provider reports is the one the config derives as
    /// the storage key: coverage, deletion and the comparability guard all
    /// query by this string, so the two ends must never drift — and the
    /// model id is part of it, so a bigger checkpoint files its rows apart
    /// from the smaller one's.
    #[test]
    fn identity_matches_the_config_storage_key() {
        assert_eq!(
            local_embedding_identity("bge-small-zh-v1.5"),
            "bge-small-zh-v1.5 (local)",
            "the stored id is part of the config contract"
        );
        assert_ne!(
            local_embedding_identity("bge-small-zh-v1.5"),
            local_embedding_identity("bge-large-zh-v1.5"),
            "different local models must not share a vector identity"
        );
    }
}
