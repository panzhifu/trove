//! The fetch machinery behind the local model downloads: a mirror list,
//! streamed file pulls, and size probes.
//!
//! candle is an inference engine — it ships no weights — so every local
//! model (the transcriber's Whisper, the embedder's BGE) is fetched from a
//! HuggingFace-style resolve URL at the user's ask. The mirrors are tried in
//! order: hf-mirror first, because it is the path that works where
//! huggingface.co does not, and where it works both serve identical bytes.
//! A wrinkle the mirror order alone cannot hide: hf-mirror redirects the
//! large checkpoints to HuggingFace's own CDN, whose connect/stream
//! behaviour on constrained networks swings between fast and dead — so
//! every file also gets retries, a bounded connect timeout, and a log line
//! on every failure (the download must never fail silently again).
//!
//! Every pull lands in a `.part` sibling the caller renames into place, so a
//! killed run never leaves a half file pretending to be whole; files already
//! present are kept, which is what makes a re-run after a failure resume
//! rather than restart.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};

/// The download mirrors, tried in order.
pub(crate) const MIRRORS: [&str; 2] = ["https://hf-mirror.com", "https://huggingface.co"];

/// How long a connect (socket + TLS, across redirects) may take before the
/// attempt is declared dead. The redirected CDN stalls instead of refusing,
/// and without this a stalled request hangs for the global timeout — an
/// hour of a silent progress bar.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Attempts per file before the download gives up. Each attempt restarts
/// the file (the `.part` is overwritten); a mid-stream CDN reset on a
/// multi-GB checkpoint is routine, and one retry is often all it takes.
pub(crate) const FETCH_ATTEMPTS: usize = 3;

/// Hard ceiling on one response body. A runaway server must not write the
/// disk forever (the global timeout is the other guard), but it has to sit
/// comfortably above the largest catalog checkpoint.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Stream one model file to `dest` through the mirrors, returning the bytes
/// written. `report` is called per chunk with the newly received bytes.
pub(crate) fn fetch_file(
    repo: &str,
    file: &str,
    dest: &Path,
    cancel: &AtomicBool,
    mut report: impl FnMut(u64),
) -> Result<u64> {
    let mut last_error: Option<Error> = None;
    for mirror in MIRRORS {
        match fetch_file_from(mirror, repo, file, dest, cancel, &mut report) {
            Ok(written) => return Ok(written),
            Err(error) if cancel.load(Ordering::Relaxed) => return Err(error),
            Err(error) => {
                tracing::warn!(mirror, file, %error, "model fetch: mirror failed, trying the next");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::External {
        program: "model-fetch".into(),
        message: "no mirror answered".into(),
    }))
}

fn fetch_file_from(
    mirror: &str,
    repo: &str,
    file: &str,
    dest: &Path,
    cancel: &AtomicBool,
    report: &mut impl FnMut(u64),
) -> Result<u64> {
    let url = format!("{mirror}/{repo}/resolve/main/{file}");
    let response = ureq::get(&url)
        .config()
        .timeout_global(Some(Duration::from_secs(60 * 60)))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| Error::External {
            program: "model-fetch".into(),
            message: e.to_string(),
        })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::External {
            program: "model-fetch".into(),
            message: format!("HTTP {status} for {url}"),
        });
    }
    // Bounds a runaway response, nothing more — and it must sit well above
    // the largest real checkpoint: the cap used to be 2 GiB, and bge-m3's
    // 2.11 GiB `pytorch_model.bin` silently died at exactly that offset (the
    // truncated stream reads as a clean EOF, the size check below the caller
    // rejects it, and the download can never succeed).
    let mut reader = response.into_body().into_reader().take(MAX_RESPONSE_BYTES);
    let mut out = fs::File::create(dest)?;
    let mut buf = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(dest);
            return Err(Error::External {
                program: "model-fetch".into(),
                message: "cancelled".into(),
            });
        }
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        out.write_all(&buf[..read])?;
        written += read as u64;
        report(read as u64);
    }
    out.sync_all()?;
    Ok(written)
}

/// Ask a mirror how big `file` is. A server that does not advertise a
/// length is fine — the progress bar loses its total, not the download.
pub(crate) fn content_length(repo: &str, file: &str, cancel: &AtomicBool) -> Result<Option<u64>> {
    for mirror in MIRRORS {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let url = format!("{mirror}/{repo}/resolve/main/{file}");
        let probe = ureq::get(&url)
            .config()
            .timeout_global(Some(Duration::from_secs(60)))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .call();
        let response = match probe {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(mirror, file, %error, "size probe: mirror failed, trying the next");
                continue;
            }
        };
        if !(200..300).contains(&response.status().as_u16()) {
            tracing::warn!(
                mirror,
                file,
                status = response.status().as_u16(),
                "size probe: mirror answered with an error status, trying the next"
            );
            continue;
        }
        return Ok(response
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok()));
    }
    Err(Error::External {
        program: "model-fetch".into(),
        message: "no mirror answered the size probe".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// A bogus repo answers 404 on every mirror, so the fetch names the
    /// last HTTP status rather than an opaque transport error — the message
    /// the settings page surfaces when a model's upstream disappears.
    #[test]
    #[ignore = "network"]
    fn a_missing_file_reports_the_http_status() {
        let cancel = AtomicBool::new(false);
        let error = fetch_file(
            "trove/nonexistent-repo",
            "model.safetensors",
            &std::env::temp_dir().join("trove-fetch-test"),
            &cancel,
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("HTTP"), "{error}");
    }

    /// Real-network probe of the exact stack the app's download rides: the
    /// silent size probe first, then a cancel-limited stream to measure the
    /// body throughput a multi-GB checkpoint would see. Diagnoses "download
    /// fails with no log lines" — the probe is the one failure path that
    /// never logs. Run with `cargo test -p trove-core --lib
    /// model_fetch::tests::the_bge_m3_weights_probe -- --ignored --nocapture`.
    #[test]
    #[ignore = "network"]
    fn the_bge_m3_weights_probe() {
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        match content_length("BAAI/bge-m3", "pytorch_model.bin", &cancel) {
            Ok(size) => println!("content_length: Ok({size:?}) in {:?}", start.elapsed()),
            Err(error) => {
                println!("content_length: FAILED in {:?}: {error}", start.elapsed());
                return;
            }
        }

        let dest = std::env::temp_dir().join("trove-fetch-probe.part");
        let cancel = Arc::new(AtomicBool::new(false));
        let seen = Instant::now();
        let secs: u64 = std::env::var("TROVE_PROBE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let stopper_cancel = cancel.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            stopper_cancel.store(true, Ordering::Relaxed);
        });
        let progress = Arc::new(AtomicU64::new(0));
        let first_byte = Arc::new(Mutex::new(None::<f64>));
        let progress_probe = progress.clone();
        let first_probe = first_byte.clone();
        let started = seen;
        let result = fetch_file(
            "BAAI/bge-m3",
            "pytorch_model.bin",
            &dest,
            &cancel,
            &move |_| {
                first_probe
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| started.elapsed().as_secs_f64());
                progress_probe.fetch_add(1, Ordering::Relaxed);
            },
        );
        stopper.join().unwrap();
        let chunks = progress.load(Ordering::Relaxed);
        let first = first_byte.lock().unwrap().to_owned();
        let bytes = chunks * 64 * 1024;
        println!("first byte after: {first:?}; chunks: {chunks}");
        let _ = fs::remove_file(&dest);
        println!("fetch outcome: {result:?}");
        println!(
            "fetched {bytes} bytes in {:?} = {:.0} KB/s",
            seen.elapsed(),
            bytes as f64 / seen.elapsed().as_secs_f64() / 1024.0
        );
    }
}
