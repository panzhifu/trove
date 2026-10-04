//! One-off real-network verification of the local embedding path: the full
//! download → completeness check → candle load → embed sequence against the
//! mirror list, pickle-weight entries included (bge-m3 and the zh base/large
//! checkpoints have no upstream safetensors; their `pytorch_model.bin` must
//! load through the pth backend). Run with `cargo test -p trove-core --test
//! embed_model_real -- --ignored --nocapture`. Not part of the hermetic
//! suite — the default model is ~2.3 GB. `EMBED_REAL_MODEL` names a
//! different catalog id (and `TROVE_DATA_DIR` redirects where it lands).

use std::sync::atomic::AtomicBool;

#[test]
#[ignore = "network, multi-GB"]
fn downloads_loads_and_embeds() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let id = std::env::var("EMBED_REAL_MODEL").unwrap_or_else(|_| "bge-m3".into());
    if matches!(
        trove_core::services::embed_model::status(&id),
        trove_core::services::embed_model::ModelStatus::Missing
    ) {
        let start = std::time::Instant::now();
        trove_core::services::embed_model::download(
            &id,
            &AtomicBool::new(false),
            &|received, total| {
                if received > 0 && total > 0 && received % (512 * 1024 * 1024) == 0 {
                    println!("{id}: {received} / {total} bytes");
                }
            },
        )
        .expect("the download must succeed");
        println!("{id}: downloaded in {:?}", start.elapsed());
    }
    let config = trove_core::config::EmbeddingConfig {
        engine: trove_core::config::EmbeddingEngine::Local,
        local_model: Some(id.clone()),
        ..Default::default()
    };
    let provider = trove_core::ai::embedding_provider(&config).expect("the model must load");
    let vectors = provider
        .embed_texts(&["你好，世界".to_string(), "hello world".to_string()])
        .expect("the model must embed");
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), vectors[1].len(), "one dim per model");
    assert!(
        vectors[0].iter().any(|v| *v != 0.0),
        "an all-zero vector means the run lied"
    );
    println!("{id}: dim = {}", vectors[0].len());
}
