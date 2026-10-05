//! One-off timing probe of the local embedding path: model load (the cost
//! every search pays today, since the provider is rebuilt per query) versus
//! a single query embed. Run with `cargo test -p trove-core --test
//! embed_speed_real -- --ignored --nocapture`. `EMBED_SPEED_MODEL` names a
//! different catalog id.

#[test]
#[ignore = "loads real weights"]
fn load_and_query_timing() {
    let id = std::env::var("EMBED_SPEED_MODEL").unwrap_or_else(|_| "bge-m3".into());
    let config = trove_core::config::EmbeddingConfig {
        engine: trove_core::config::EmbeddingEngine::Local,
        local_model: Some(id.clone()),
        ..Default::default()
    };

    let start = std::time::Instant::now();
    let provider = trove_core::ai::embedding_provider(&config).expect("the model must load");
    println!("[{id}] build (load weights + CUDA): {:?}", start.elapsed());

    let start = std::time::Instant::now();
    let cached = trove_core::ai::embedding_provider(&config).expect("the cached model must load");
    println!("[{id}] rebuild (cache hit): {:?}", start.elapsed());
    assert!(
        std::sync::Arc::ptr_eq(&provider, &cached),
        "the cache must hand back the same provider"
    );

    for text in ["海边日落", "a cat sleeping on a warm keyboard"] {
        let start = std::time::Instant::now();
        let vectors = provider
            .embed_texts(&[text.to_string()])
            .expect("embed must succeed");
        println!(
            "[{id}] single query {:?}: {:?} (dim {})",
            text,
            start.elapsed(),
            vectors[0].len()
        );
    }

    // A small batch, the shape the asset backfill uses.
    let batch: Vec<String> = (0..32)
        .map(|i| format!("asset number {i} about travel and food"))
        .collect();
    let start = std::time::Instant::now();
    provider
        .embed_texts(&batch)
        .expect("batch embed must succeed");
    println!("[{id}] batch of 32: {:?}", start.elapsed());
}
