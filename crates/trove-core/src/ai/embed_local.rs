//! Local text embeddings: BAAI's `bge-small-zh-v1.5` running on this
//! machine's GPU (or CPU) through candle — a pure-Rust tensor stack, so
//! nothing here compiles or links C++.
//!
//! The weights are loaded once at construction — construction happens inside
//! the embedding job or the probe's background task, never on the UI
//! thread — and reused for every batch of the run. The decode is the
//! sentence-transformers recipe BGE was trained for: BERT's sequence output,
//! pooled at the `[CLS]` token, then L2-normalized so cosine similarity
//! ranks by direction. No instruction prefix is prepended: v1.5 is the
//! generation of BGE trained to retrieve well *without* one, and the
//! fingerprint text (title, description, tags) and the query must go through
//! the exact same pipeline to stay comparable.
//!
//! BGE is a text embedder: rows land in the text space, and there is no
//! image path — a multimodal (CLIP-style) vector store stays a cloud-engine
//! feature. The model is BAAI's Chinese-tuned small checkpoint, but the
//! tokenizer and recipe are language-agnostic: English fingerprints embed
//! fine, which is what a multilingual library needs.

use std::sync::Mutex;

use candle_core::{DType, D, IndexOp as _, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

use super::EmbeddingProvider;
use crate::config::LOCAL_EMBEDDING_MODEL;
use crate::error::{Error, Result};
use crate::model::EmbeddingSpace;

/// Texts per forward pass. The cloud client batches at 64, but a local BERT
/// holds the whole batch's activations in memory; 16 keeps a CPU run at a
/// few hundred megabytes while amortizing the per-pass overhead.
const LOCAL_BATCH: usize = 16;

/// Everything loaded once and held for the provider's lifetime.
struct Loaded {
    model: BertModel,
    tokenizer: Tokenizer,
    dim: usize,
}

pub struct LocalBert {
    loaded: Mutex<Loaded>,
}

/// Build the local provider: the model must already be on disk (the UI asks
/// to download it before a run starts), and loading happens here so a
/// broken install fails the run with a clear message instead of at first
/// use.
pub fn build() -> Result<LocalBert> {
    let dir = match crate::services::embed_model::status() {
        crate::services::embed_model::ModelStatus::Ready { path } => path,
        crate::services::embed_model::ModelStatus::Missing => {
            return Err(Error::External {
                program: "embed-local".into(),
                message: "the local embedding model is not downloaded yet".into(),
            });
        }
    };
    let loaded = load(&dir).map_err(|message| Error::External {
        program: "embed-local".into(),
        message,
    })?;
    Ok(LocalBert {
        loaded: Mutex::new(loaded),
    })
}

fn load(dir: &std::path::Path) -> std::result::Result<Loaded, String> {
    let config: Config = serde_json::from_str(
        &std::fs::read_to_string(dir.join("config.json"))
            .map_err(|e| format!("read config.json: {e}"))?,
    )
    .map_err(|e| format!("parse config.json: {e}"))?;
    let dim = config.hidden_size;
    let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| format!("load tokenizer.json: {e}"))?;
    // Every input is cut to the model's window and every batch padded to its
    // own longest member, so the tensors are rectangular without padding the
    // whole library to 512 tokens.
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: config.max_position_embeddings,
            ..TruncationParams::default()
        }))
        .map_err(|e| format!("configure truncation: {e}"))?;
    tokenizer.with_padding(Some(PaddingParams::default()));

    let (device, backend) = super::local_device::select_device();
    tracing::info!(backend, "local: embedder device");
    let vb = unsafe {
        // Memory-mapping the weights: the file is the library's own managed
        // download, and the mapping is read-only for the process lifetime.
        VarBuilder::from_mmaped_safetensors(
            &[dir.join("model.safetensors")],
            DType::F32,
            &device,
        )
    }
    .map_err(|e| format!("load model.safetensors: {e}"))?;
    let model = BertModel::load(vb, &config).map_err(|e| format!("build the model: {e}"))?;

    Ok(Loaded {
        model,
        tokenizer,
        dim,
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
            &self.model.device,
        )?;
        let token_type_ids = Tensor::new(
            encodings
                .iter()
                .map(|e| e.get_type_ids().to_vec())
                .collect::<Vec<_>>(),
            &self.model.device,
        )?;
        let attention_mask = Tensor::new(
            encodings
                .iter()
                .map(|e| e.get_attention_mask().to_vec())
                .collect::<Vec<_>>(),
            &self.model.device,
        )?;

        let sequence = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;
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
        LOCAL_EMBEDDING_MODEL
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

    /// The identity the provider reports is the one the config names as the
    /// storage key: coverage, deletion and the comparability guard all query
    /// by this string, so the two ends must never drift.
    #[test]
    fn identity_matches_the_config_storage_key() {
        assert_eq!(
            LOCAL_EMBEDDING_MODEL,
            "bge-small-zh-v1.5 (local)",
            "the stored id is part of the config contract"
        );
    }
}
