//! One-off real-network verification of the local transcription path. Run
//! with `cargo test -p trove-core --test local_model_real -- --ignored
//! --nocapture`. Not part of the hermetic suite.

use std::sync::atomic::{AtomicBool, Ordering};

#[test]
#[ignore]
fn model_is_complete_on_disk() {
    let dir = trove_core::services::local_model::download(&AtomicBool::new(false), &|_, _| {})
        .expect("the download must succeed");
    println!("landed at {dir:?}");
}

/// Real inference on a known clip: candle-whisper's jfk.wav (16 kHz mono,
/// "And so my fellow Americans…"). The device selection happens inside the
/// load, so this runs on CUDA when the build carries the gpu feature and a
/// driver answers, CPU otherwise — the log names the choice.
#[test]
#[ignore]
fn transcribes_jfk_locally() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
    let audio = std::fs::read("/tmp/jfk.wav").expect("jfk.wav in /tmp");
    let config = trove_core::config::TranscriptionConfig {
        engine: trove_core::config::TranscriptionEngine::Local,
        ..Default::default()
    };
    let provider =
        trove_core::ai::transcribe::build_from_config(&config).expect("the model is on disk");
    println!("provider: {}", provider.model());
    let cancel = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let text = provider
        .transcribe(&audio, "jfk.wav", "audio/wav", None, None, &cancel)
        .expect("inference succeeds");
    println!("inference took {:?}", started.elapsed());
    println!("transcript: {text}");
    assert!(
        text.to_lowercase().contains("fellow americans"),
        "got: {text}"
    );
    assert!(!cancel.load(Ordering::Relaxed));
}
