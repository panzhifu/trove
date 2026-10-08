//! Local speech-to-text: OpenAI's Whisper running on this machine's CPU
//! through candle — a pure-Rust tensor stack, so nothing here compiles or
//! links C++.
//!
//! The recogniser is the multilingual base checkpoint (`openai/whisper-base`
//! through the mirrors in [`crate::services::local_model`]). The weights are
//! loaded once at construction — construction happens inside the
//! transcription job, on a background thread, where a few seconds of model
//! load and a broken install both belong — and reused for every chunk of
//! the run.
//!
//! The decode follows the reference protocol in the shape candle's own
//! example uses: each ≤30 s mel window is encoded once, the prompt
//! (`<|startoftranscript|>`, language, `<|transcribe|>`, `<|notimestamps|>`)
//! is fed through the KV-cached decoder in one pass, and tokens are then
//! generated greedily until `<|endoftext|>`. No-timestamps mode is what
//! keeps this small — the timestamp pairing rules are the hairiest part of
//! the protocol, and a library transcript does not need them. Two quality
//! gates from the reference remain: a window is thrown away when its
//! average log probability is too low (mumbled or empty audio), or when its
//! text compresses too well (the hallucination loop whisper falls into on
//! silence); failing windows retry up the reference's temperature ladder
//! before being given up on.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use candle_core::{Device, IndexOp as _};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{self, audio, model::Whisper};
use tokenizers::Tokenizer;

use crate::ai::transcribe::{ChunkFormat, TranscribeProvider};
use crate::ai::vendor::{VendorError, VendorErrorKind};
use crate::config::TranscriptionConfig;

/// Identity recorded beside every transcript.
const MODEL_LABEL: &str = "whisper-base (local)";

/// Retry temperatures for a window that failed its quality gates, from the
/// reference implementation.
const TEMPERATURES: [f64; 6] = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];

/// Everything loaded once and held for the provider's lifetime.
pub struct Loaded {
    model: Whisper,
    tokenizer: Tokenizer,
    device: Device,
    config: whisper::Config,
    /// The mel filter bank: the same 64 KB the candle example ships.
    filters: Vec<f32>,
    sot: u32,
    transcribe: u32,
    eot: u32,
    no_timestamps: u32,
    no_speech: Vec<u32>,
    suppressed: Vec<u32>,
}

pub struct LocalWhisper {
    loaded: Mutex<Loaded>,
    /// The language hint saved in the settings, used when a run passes none.
    language: Option<String>,
}

impl LocalWhisper {
    pub fn new(loaded: Loaded, language: Option<String>) -> Self {
        Self {
            loaded: Mutex::new(loaded),
            language,
        }
    }
}

/// Build the local provider: the model must already be on disk (the UI asks
/// to download it before a run starts), and loading happens here so a
/// broken install fails the run with a clear message instead of at first
/// use.
pub fn build(config: &TranscriptionConfig) -> crate::error::Result<LocalWhisper> {
    let dir = match crate::services::local_model::status(&crate::services::local_model::WHISPER) {
        crate::services::local_model::ModelStatus::Ready { path } => path,
        crate::services::local_model::ModelStatus::Missing => {
            return Err(crate::error::Error::External {
                program: "whisper-local".into(),
                message: "the local transcription model is not downloaded yet".into(),
            });
        }
    };
    let loaded = load(&dir).map_err(|message| crate::error::Error::External {
        program: "whisper-local".into(),
        message,
    })?;
    Ok(LocalWhisper::new(loaded, config.language.clone()))
}

/// The tensor device for this build: CUDA first with a CPU fallback on
/// Linux, plain CPU elsewhere — the shared policy lives in
/// [`super::local_device`].
fn select_device() -> (candle_core::Device, &'static str) {
    super::local_device::select_device()
}

fn load(dir: &Path) -> std::result::Result<Loaded, String> {
    let config: whisper::Config = serde_json::from_str(
        &std::fs::read_to_string(dir.join("config.json"))
            .map_err(|e| format!("read config.json: {e}"))?,
    )
    .map_err(|e| format!("parse config.json: {e}"))?;
    let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| format!("load tokenizer.json: {e}"))?;
    let (device, backend) = select_device();
    tracing::info!(backend, "local: recogniser device");
    let vb = unsafe {
        // Memory-mapping the weights: the file is the library's own managed
        // download, and the mapping is read-only for the process lifetime.
        VarBuilder::from_mmaped_safetensors(
            &[dir.join("model.safetensors")],
            whisper::DTYPE,
            &device,
        )
    }
    .map_err(|e| format!("load model.safetensors: {e}"))?;
    let model = Whisper::load(&vb, config.clone()).map_err(|e| format!("build the model: {e}"))?;

    // The mel filter bank: num_mel_bins filters over N_FFT/2 + 1 slots, in
    // the `mel_80` tensor of the demo's safetensors file.
    let filter_tensors =
        candle_core::safetensors::load(dir.join("mel_filters.safetensors"), &device)
            .map_err(|e| format!("load mel_filters.safetensors: {e}"))?;
    let filters: Vec<f32> = filter_tensors
        .get("mel_80")
        .ok_or("mel_filters.safetensors lacks the mel_80 tensor")?
        .to_vec2::<f32>()
        .map_err(|e| format!("read the mel_80 tensor: {e}"))?
        .into_iter()
        .flatten()
        .collect();
    let expected = config.num_mel_bins * (whisper::N_FFT / 2 + 1);
    if filters.len() != expected {
        return Err(format!(
            "the mel filter bank holds {} values, expected {expected}",
            filters.len()
        ));
    }

    let special = |token: &str| {
        tokenizer
            .token_to_id(token)
            .ok_or_else(|| format!("the tokenizer lacks {token}"))
    };
    let sot = special(whisper::SOT_TOKEN)?;
    let transcribe = special(whisper::TRANSCRIBE_TOKEN)?;
    let eot = special(whisper::EOT_TOKEN)?;
    let no_timestamps = special(whisper::NO_TIMESTAMPS_TOKEN)?;
    let no_speech = whisper::NO_SPEECH_TOKENS
        .iter()
        .filter_map(|token| tokenizer.token_to_id(token))
        .collect();
    let suppressed = config.suppress_tokens.clone();

    Ok(Loaded {
        model,
        tokenizer,
        device,
        config,
        filters,
        sot,
        transcribe,
        eot,
        no_timestamps,
        no_speech,
        suppressed,
    })
}

impl TranscribeProvider for LocalWhisper {
    fn model(&self) -> &str {
        MODEL_LABEL
    }

    fn chunk_format(&self) -> ChunkFormat {
        ChunkFormat::Wav
    }

    fn transcribe(
        &self,
        audio: &[u8],
        _file_name: &str,
        _mime: &str,
        language: Option<&str>,
        _prompt: Option<&str>,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let (samples, sample_rate) = pcm_from_wav(audio).map_err(|message| VendorError {
            kind: VendorErrorKind::InvalidResponse,
            message,
            http_status: None,
            provider_code: None,
            request_id: None,
        })?;
        // A per-run hint wins over the saved preference; both fall back to
        // detection on the first window.
        let language = language
            .map(str::to_string)
            .or_else(|| self.language.clone());
        let mut guard = self
            .loaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.transcribe(&samples, sample_rate, language.as_deref(), cancel)
    }
}

impl Loaded {
    /// Transcribe one chunk: mel over the whole recording, then 30 s
    /// windows.
    fn transcribe(
        &mut self,
        samples: &[f32],
        sample_rate: u32,
        language: Option<&str>,
        cancel: &AtomicBool,
    ) -> std::result::Result<String, VendorError> {
        if sample_rate as usize != whisper::SAMPLE_RATE {
            return Err(VendorError {
                kind: VendorErrorKind::InvalidResponse,
                message: format!("the audio chunk is {sample_rate} Hz, Whisper needs 16 kHz"),
                http_status: None,
                provider_code: None,
                request_id: None,
            });
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }

        // One mel window per 30 s of audio, computed just in time: the
        // whole-chunk mel cost a minute of uncancellable FFT up front and
        // ~115 MB of f32; a window costs a fraction of either, and the
        // cancel flag gets checked between windows. pcm_to_mel pads each
        // window past 3000 frames, so the slice below always yields the
        // full 3000-frame geometry the encoder was trained on — the tail
        // window rides on zero padding, exactly like a short file does.
        let window_samples = whisper::N_FRAMES * whisper::HOP_LENGTH;
        let mut texts: Vec<String> = Vec::new();
        // A saved hint resolves immediately; detection happens on the first
        // window and only when no hint was given.
        let hinted = language.and_then(|code| self.language_token(code));
        let mut detected: Option<u32> = None;
        let mut seek = 0usize;
        while seek < samples.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            let width = window_samples.min(samples.len() - seek);
            let mel = audio::pcm_to_mel(&self.config, &samples[seek..seek + width], &self.filters);
            seek += width;
            let frames = mel.len() / self.config.num_mel_bins;
            let window = candle_core::Tensor::from_vec(
                mel,
                (self.config.num_mel_bins, frames),
                &self.device,
            )
            .map_err(local_error)?
            .narrow(1, 0, whisper::N_FRAMES.min(frames))
            .map_err(local_error)?
            .unsqueeze(0)
            .map_err(local_error)?;

            // The language settles on the first window and holds for the
            // run: one detection pass, not one per segment.
            let language_token = match hinted {
                Some(token) => token,
                None => match detected {
                    Some(token) => token,
                    None => {
                        let token = self.detect_language(&window).map_err(local_error)?;
                        tracing::debug!(
                            token,
                            name = ?self.tokenizer.id_to_token(token),
                            "local: language detected"
                        );
                        detected = Some(token);
                        token
                    }
                },
            };

            let Some(segment) = self
                .decode_with_fallback(&window, language_token, cancel)
                .map_err(local_error)?
            else {
                continue;
            };
            if segment.text.trim().is_empty() {
                continue;
            }
            // The reference skips a window whose no-speech probability is
            // high while its log probability is low: padded silence decoded
            // into whisper's favourite ghost words.
            if segment.no_speech_prob > 0.6 && segment.avg_logprob < -1.0 {
                continue;
            }
            texts.push(segment.text.trim().to_string());
        }
        Ok(join_segments(texts))
    }

    /// `<|startoftranscript|>`, the language, then the transcription task
    /// with timestamps off.
    fn initial_prompt(&self, language_token: u32) -> Vec<u32> {
        vec![
            self.sot,
            language_token,
            self.transcribe,
            self.no_timestamps,
        ]
    }

    fn language_token(&self, code: &str) -> Option<u32> {
        self.tokenizer.token_to_id(&format!("<|{code}|>"))
    }

    /// Which language this window is in: one decoder step after the SOT,
    /// argmax over the language-token block of the vocabulary. The block
    /// sits between the SOT and the transcribe token, spelled `<|en|>`,
    /// `<|zh|>`, … — a two-or-three letter code is what separates a language
    /// from `<|translate|>` and the other specials.
    fn detect_language(&mut self, window: &candle_core::Tensor) -> candle_core::Result<u32> {
        let candidates: Vec<u32> = (self.sot + 1..self.transcribe)
            .filter(|id| {
                self.tokenizer
                    .id_to_token(*id)
                    .and_then(|token| {
                        token
                            .strip_prefix("<|")
                            .and_then(|t| t.strip_suffix("|>"))
                            .map(str::to_string)
                    })
                    .is_some_and(|inner| {
                        (2..=3).contains(&inner.chars().count())
                            && inner.chars().all(|c| c.is_ascii_alphabetic())
                    })
            })
            .collect();
        if candidates.is_empty() {
            return Err(candle_core::Error::Msg(
                "the tokenizer has no language tokens; this checkpoint is not multilingual".into(),
            ));
        }

        self.model.reset_kv_cache();
        let prompt = candle_core::Tensor::new(&[self.sot][..], &self.device)?.unsqueeze(0)?;
        let xa = self.model.encoder.forward(window, true)?;
        let logits = self.model.decoder.forward(&prompt, &xa, true)?;
        let logits = self.model.decoder.final_linear(&logits)?;
        let last = logits.i((0, 0))?.to_vec1::<f32>()?;
        let best = candidates
            .into_iter()
            .max_by(|a, b| {
                last[*a as usize]
                    .partial_cmp(&last[*b as usize])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty");
        Ok(best)
    }

    /// Decode one window, retrying up the temperature ladder while the
    /// quality gates say the answer is worse than silence. `None` = the
    /// window reads as silence.
    fn decode_with_fallback(
        &mut self,
        window: &candle_core::Tensor,
        language_token: u32,
        cancel: &AtomicBool,
    ) -> candle_core::Result<Option<Segment>> {
        let mut fallback: Option<Segment> = None;
        for temperature in TEMPERATURES {
            // A ladder pass on the CPU runs to a minute; the cancel flag
            // must win between passes, not after the whole ladder.
            if cancel.load(Ordering::Relaxed) {
                return Err(candle_core::Error::Msg(CANCELLED.into()));
            }
            let segment = self.decode(window, language_token, temperature, cancel)?;
            tracing::debug!(
                temperature,
                no_speech = segment.no_speech_prob,
                avg_logprob = segment.avg_logprob,
                compression = segment.compression_ratio,
                chars = segment.text.chars().count(),
                "local: window decoded"
            );
            let gates_ok = segment.compression_ratio <= 2.4 && segment.avg_logprob >= -1.0;
            if gates_ok {
                return Ok(Some(segment));
            }
            if segment.no_speech_prob > 0.6 {
                // Past its no-speech threshold this window is silence to the
                // reference — returned as-is rather than retried.
                return Ok(None);
            }
            fallback = Some(segment);
        }
        Ok(fallback)
    }

    /// One greedy decode pass over one window.
    ///
    /// candle's whisper decoder carries no self-attention KV cache — only
    /// the cross-attention to the encoded audio is cached — so the whole
    /// token sequence is re-fed every step, exactly as the reference
    /// example does. The cache is built on the first pass (`flush`) and
    /// reused for the rest of the window.
    fn decode(
        &mut self,
        window: &candle_core::Tensor,
        language_token: u32,
        temperature: f64,
        cancel: &AtomicBool,
    ) -> candle_core::Result<Segment> {
        let mut tokens: Vec<u32> = self.initial_prompt(language_token);
        let xa = self.model.encoder.forward(window, true)?;

        let mut sum_logprob = 0f64;
        let mut no_speech_prob = 0f64;
        let mut generated: Vec<u32> = Vec::new();
        let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15 ^ (temperature.to_bits()));
        let max_generated = self.config.max_target_positions / 2;

        for step in 0..max_generated {
            let tokens_t =
                candle_core::Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;
            let ys = self.model.decoder.forward(&tokens_t, &xa, step == 0)?;
            let (_, seq_len, _) = ys.dims3()?;
            let logits = self
                .model
                .decoder
                .final_linear(&ys.i((..1, seq_len - 1..))?)?
                .i((0, 0))?
                .to_vec1::<f32>()?;
            if step == 0 {
                // The reference reads the no-speech probability off the
                // first generated position's distribution.
                let first_probs = softmax(&logits);
                no_speech_prob = self
                    .no_speech
                    .iter()
                    .filter_map(|id| first_probs.get(*id as usize).copied())
                    .sum();
            }
            let last = suppress(logits, &self.suppressed);
            let probs = softmax(&last);
            let token = if temperature > 0.0 {
                sample(&probs, &mut rng)
            } else {
                argmax(&last)
            };
            if token == self.eot {
                break;
            }
            generated.push(token);
            sum_logprob += probs[token as usize].max(1e-10).ln();
            tokens.push(token);
            // Full-sequence re-feeding makes each step a forward pass over
            // the whole context; on the CPU that is seconds, so the cancel
            // flag gets polled every few steps instead of every one.
            if generated.len().is_multiple_of(16) && cancel.load(Ordering::Relaxed) {
                return Err(candle_core::Error::Msg(CANCELLED.into()));
            }
        }

        let text = self
            .tokenizer
            .decode(&generated, true)
            .map_err(|e| candle_core::Error::Msg(format!("decode tokens: {e}")))?;
        let compression_ratio = compression_ratio(&text);
        Ok(Segment {
            text,
            avg_logprob: sum_logprob / generated.len().max(1) as f64,
            compression_ratio,
            no_speech_prob,
        })
    }
}

/// Blank out the tokens the checkpoint itself asks to suppress — special
/// tokens beyond the ones the prompt already names.
fn suppress(mut logits: Vec<f32>, suppressed: &[u32]) -> Vec<f32> {
    for &id in suppressed {
        if (id as usize) < logits.len() {
            logits[id as usize] = -1.0e10;
        }
    }
    logits
}

/// One window's output plus the gates that decide whether to keep it.
struct Segment {
    text: String,
    avg_logprob: f64,
    compression_ratio: f64,
    no_speech_prob: f64,
}

/// The marker a cancelled decode carries through candle's error channel;
/// `local_error` maps it back to the cancellation the task layer expects.
const CANCELLED: &str = "cancelled";

fn local_error(error: candle_core::Error) -> VendorError {
    let message = error.to_string();
    let kind = if message.contains(CANCELLED) {
        VendorErrorKind::Timeout
    } else {
        VendorErrorKind::InvalidResponse
    };
    VendorError {
        kind,
        message,
        http_status: None,
        provider_code: None,
        request_id: None,
    }
}

fn cancelled() -> VendorError {
    VendorError {
        kind: VendorErrorKind::Timeout,
        message: "cancelled".into(),
        http_status: None,
        provider_code: None,
        request_id: None,
    }
}

/// The 16-bit PCM the chunks arrive as, to the f32 the model wants.
/// Stereo is folded to mono; the header is parsed rather than assumed —
/// ffmpeg wrote it, but asserting beats trusting.
fn pcm_from_wav(bytes: &[u8]) -> std::result::Result<(Vec<f32>, u32), String> {
    let riff = bytes.get(..4).ok_or("empty audio chunk")?;
    if riff != b"RIFF" {
        return Err("not a RIFF file".into());
    }
    if bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a WAVE container".into());
    }

    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut pcm: Option<Vec<i16>> = None;
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body = offset + 8;
        let end = (body + size).min(bytes.len());
        match id {
            b"fmt " => {
                let fmt = bytes.get(body..body + 16).ok_or("truncated fmt chunk")?;
                let format = u16::from_le_bytes(fmt[0..2].try_into().unwrap());
                if format != 1 {
                    return Err(format!("unsupported WAV format tag {format} (need PCM)"));
                }
                channels = u16::from_le_bytes(fmt[2..4].try_into().unwrap());
                sample_rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap());
                if bits != 16 {
                    return Err(format!("unsupported WAV bit depth {bits} (need 16)"));
                }
            }
            b"data" => {
                let data = &bytes[body..end];
                pcm = Some(
                    data.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| i16::from_le_bytes(*pair))
                        .collect(),
                );
            }
            _ => {}
        }
        offset = body + size + (size & 1); // chunks are word-aligned
    }

    let pcm = pcm.ok_or("the WAV carries no data chunk")?;
    let samples = match channels {
        1 => pcm
            .iter()
            .map(|s| f32::from(*s) / 32768.0)
            .collect::<Vec<f32>>(),
        2 => pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (f32::from(pair[0]) + f32::from(pair[1])) / 2.0 / 32768.0)
            .collect::<Vec<f32>>(),
        channels => return Err(format!("unsupported WAV channel count {channels}")),
    };
    if sample_rate == 0 {
        return Err("the WAV declares no sample rate".into());
    }
    Ok((samples, sample_rate))
}

/// One maximally simple generator so a retried window is not a deterministic
/// copy of the greedy answer. xorshift64*, seeded from the temperature.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// The probability distribution the model is proposing, on the CPU: subtract
/// the max for the exponent's sake, exponentiate, normalize.
fn softmax(logits: &[f32]) -> Vec<f64> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f64> = logits
        .iter()
        .map(|l| (*l as f64 - max as f64).exp())
        .collect();
    let sum: f64 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

fn argmax(logits: &[f32]) -> u32 {
    let mut best = 0usize;
    for (index, value) in logits.iter().enumerate() {
        if *value > logits[best] {
            best = index;
        }
    }
    best as u32
}

/// Sample one index from the temperature-flattened distribution.
fn sample(probs: &[f64], rng: &mut Rng) -> u32 {
    let total: f64 = probs.iter().sum();
    if total <= 0.0 {
        return argmax(&[]);
    }
    let mut pick = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64 * total;
    for (index, probability) in probs.iter().enumerate() {
        pick -= probability;
        if pick <= 0.0 {
            return index as u32;
        }
    }
    (probs.len() - 1) as u32
}

/// The reference's hallucination detector: how many copies of the text a
/// gzip pass needs. A loop like "the the the the…" compresses to almost
/// nothing, and the ratio explodes.
fn compression_ratio(text: &str) -> f64 {
    use std::io::Write as _;
    let raw = text.as_bytes();
    if raw.is_empty() {
        return 1.0;
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    if encoder.write_all(raw).is_err() {
        return 1.0;
    }
    match encoder.finish() {
        Ok(compressed) if !compressed.is_empty() => raw.len() as f64 / compressed.len() as f64,
        _ => 1.0,
    }
}

/// Windows join into one transcript. A space between segments is right for
/// space-delimited scripts; CJK on either side of the join reads better
/// without one.
fn join_segments(texts: Vec<String>) -> String {
    let mut joined = String::new();
    for text in texts {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if let (Some(last), Some(first)) = (joined.chars().next_back(), text.chars().next()) {
            let cjk = |c: char| {
                ('\u{4E00}'..='\u{9FFF}').contains(&c)
                    || ('\u{3000}'..='\u{303F}').contains(&c)
                    || ('\u{3040}'..='\u{30FF}').contains(&c)
            };
            // A space belongs between scripts; two CJK runs touch without
            // one.
            if !joined.is_empty() && !(cjk(last) && cjk(first)) {
                joined.push(' ');
            }
        }
        joined.push_str(text);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence in, empty transcript out — but the WAV must parse first, and
    /// its sample rate must surface: the provider refuses anything that is
    /// not the 16 kHz audio_prep normalises to.
    #[test]
    fn the_probe_wav_parses_to_16k_silence() {
        let wav = crate::media::audio_prep::probe_wav();
        let (samples, rate) = pcm_from_wav(&wav).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 16_000);
        assert!(samples.iter().all(|s| *s == 0.0), "silence");
    }

    /// A stereo file folds to mono by averaging, and a wrong bit depth or
    /// format tag is refused rather than silently misread.
    #[test]
    fn stereo_folds_and_nonsense_is_refused() {
        // 16 kHz stereo, two frames: a one-second header would overstate it,
        // so the sizes here are exact.
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + 8).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&(16_000u32 * 4).to_le_bytes()); // byte rate
        wav.extend_from_slice(&4u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&32767i16.to_le_bytes());
        wav.extend_from_slice(&(-32767i16).to_le_bytes());

        let (samples, rate) = pcm_from_wav(&wav).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(
            samples.len(),
            1,
            "one stereo frame folds to one mono sample"
        );
        assert!((samples[0] - 0.0).abs() < 1e-6, "the pair averages out");

        // Not WAV at all.
        assert!(pcm_from_wav(b"not a wav").is_err());
    }

    /// The hallucination gate: a repeated loop compresses to almost nothing
    /// and must trip the ratio; ordinary prose does not.
    #[test]
    fn compression_ratio_exposes_a_loop() {
        let looped = "the quick brown fox ".repeat(40);
        assert!(compression_ratio(&looped) > 10.0, "a loop compresses hard");
        let prose = "A short transcript of an ordinary recording, saying something once.";
        assert!(compression_ratio(prose) < 5.0, "prose does not");
        assert!((compression_ratio("") - 1.0).abs() < f64::EPSILON);
    }

    /// Segments join without spaces across CJK and with them across scripts.
    #[test]
    fn joins_respect_the_scripts() {
        let joined = join_segments(vec!["今天天气".into(), "很好".into(), "hello world".into()]);
        assert_eq!(joined, "今天天气很好 hello world");
    }
}
