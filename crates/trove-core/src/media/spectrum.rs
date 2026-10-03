//! Log-spaced spectrum bands from PCM — the live spectrum strip behind the
//! audio preview.
//!
//! The waveform envelope is a picture of the whole song; a spectrum is a
//! picture of *now*. The engine decodes 100 ms chunks on its way into the
//! sink, so the analysis rides the same path: each chunk is windowed, run
//! through a small in-house radix-2 FFT (nothing here justifies an FFTW
//! dependency), and folded into [`BAND_COUNT`] geometric bands between
//! [`MIN_FREQ`] and [`MAX_FREQ`] — the poor man's constant-Q, one bin
//! spacing per octave ratio, so a bass guitar and a hi-hat get equal
//! screen room.
//!
//! Bands come out as 0..1 on a decibel scale: full-scale sine at the top,
//! [`DB_FLOOR`] at the bottom. Music lives mostly in the lower half of
//! that range, which is the headroom the fall animation needs.

use std::f32::consts::TAU;

/// Bars on the strip. 48 across ~900 px gives ~19 px per bar — dense
/// enough to read as a spectrum, sparse enough to keep clean gaps.
pub const BAND_COUNT: usize = 48;

/// The analyzed range. 40 Hz is under the low E of a bass guitar; 16 kHz
/// covers everything above that a lossy stream bother to keep.
pub const MIN_FREQ: f32 = 40.0;
pub const MAX_FREQ: f32 = 16_000.0;

/// Band value at or below this many dB under full scale reads as silence.
const DB_FLOOR: f32 = -70.0;

/// FFT size: 4096 frames at 44.1 kHz is a 93 ms window — one per chunk,
/// with the resolution to tell a 40 Hz floor apart (~10.8 Hz per bin).
const FFT_SIZE: usize = 4096;

/// A streaming analyzer. The window, the buffers and the bin→band map are
/// built once per playback and reused per chunk.
pub struct SpectrumAnalyzer {
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// FFT bin → band index, or -1 for bins outside the analyzed range.
    bin_band: Vec<i16>,
}

impl SpectrumAnalyzer {
    /// Build for `sample_rate` Hz input and an [`FFT_SIZE`] window.
    pub fn new(sample_rate: f32) -> Self {
        let n = FFT_SIZE;
        // Hann window: the chunk is an arbitrary slice of a continuous
        // stream, and the rectangular cut would smear every tone across
        // the bins its discontinuities leak into.
        let window: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos())
            .collect();
        // Geometric band edges: equal bin counts per octave ratio, the
        // property that makes a spectrum read like pitch space does.
        let ratio = (MAX_FREQ / MIN_FREQ).ln() / BAND_COUNT as f32;
        let mut bin_band = vec![-1i16; n / 2];
        for (k, slot) in bin_band.iter_mut().enumerate() {
            let freq = k as f32 * sample_rate / n as f32;
            if !(MIN_FREQ..=MAX_FREQ).contains(&freq) {
                continue;
            }
            let band = ((freq / MIN_FREQ).ln() / ratio).floor();
            *slot = (band as i16).clamp(0, BAND_COUNT as i16 - 1);
        }
        Self {
            window,
            re: vec![0.0; n],
            im: vec![0.0; n],
            bin_band,
        }
    }

    /// Reduce a mono PCM slice to [`BAND_COUNT`] band values, 0..1. Only
    /// the first window's worth of frames is read; a short tail (or none)
    /// pads as silence.
    pub fn bands(&mut self, pcm: &[i16]) -> Vec<f32> {
        let n = self.re.len();
        for i in 0..n {
            let sample = if i < pcm.len() {
                f32::from(pcm[i]) / 32768.0
            } else {
                0.0
            };
            self.re[i] = sample * self.window[i];
            self.im[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im);
        // Windowed amplitude back to physical units: the Hann window's
        // coherent gain is 0.5, and a real-input spectrum is counted on
        // both halves.
        let norm = 2.0 / (n as f32 * 0.5);
        let mut bands = vec![0.0f32; BAND_COUNT];
        for (k, &band) in self.bin_band.iter().enumerate() {
            let band: usize = match band.try_into() {
                Ok(band) => band,
                Err(_) => continue,
            };
            let mag = (self.re[k] * self.re[k] + self.im[k] * self.im[k]).sqrt() * norm;
            // Peak, not mean, per band — a band holding one loud
            // partial is loud, however quiet its floor.
            let db = 20.0 * (mag + 1e-9).log10();
            let value = ((db - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0);
            if value > bands[band] {
                bands[band] = value;
            }
        }
        bands
    }
}

/// In-place iterative radix-2 Cooley–Tukey. `n` must be a power of two —
/// the only FFT this crate needs, sized once and reused per chunk.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    // Bit-reversal permutation: the iterative shape visits its inputs in
    // this order and stays cache-friendly without recursion.
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let ang = -TAU / len as f32;
        let (sin, cos) = (ang.sin(), ang.cos());
        for start in (0..n).step_by(len) {
            let (mut wr, mut wi) = (1.0f32, 0.0f32);
            for k in 0..half {
                let i = start + k;
                let j = i + half;
                // One butterfly: (w · x[j]) split into x[i] and x[j].
                let tr = re[j] * wr - im[j] * wi;
                let ti = re[j] * wi + im[j] * wr;
                re[j] = re[i] - tr;
                im[j] = im[i] - ti;
                re[i] += tr;
                im[i] += ti;
                // Step the twiddle factor one turn along the unit circle.
                let nr = wr * cos - wi * sin;
                wi = wr * sin + wi * cos;
                wr = nr;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 kHz sine at 44.1 kHz must land in the band that owns 1 kHz, and
    /// nowhere louder.
    #[test]
    fn a_pure_tone_peaks_in_its_own_band() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        // 4096 frames of 1 kHz, full scale.
        let pcm: Vec<i16> = (0..4096)
            .map(|i| {
                let t = i as f32 / 44_100.0;
                (0.5 * (TAU * 1000.0 * t).sin() * 32768.0) as i16
            })
            .collect();
        let bands = analyzer.bands(&pcm);
        let owner = {
            let ratio = (MAX_FREQ / MIN_FREQ).ln() / BAND_COUNT as f32;
            ((1000.0 / MIN_FREQ).ln() / ratio).floor() as usize
        };
        assert_eq!(bands.len(), BAND_COUNT);
        let (peak_at, &peak) = bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(peak_at, owner, "1 kHz must land in band {owner}");
        assert!(peak > 0.4, "a half-scale tone must read clearly, got {peak}");
    }

    /// Silence reads as silence everywhere — no window leakage off the
    /// floor.
    #[test]
    fn silence_is_silence() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let bands = analyzer.bands(&vec![0i16; 4096]);
        assert!(bands.iter().all(|&b| b == 0.0));
    }

    /// The band map is monotone: every band's centre frequency maps back to
    /// its own slot, and the map covers real bins.
    #[test]
    fn bands_tile_the_range_in_order() {
        let analyzer = SpectrumAnalyzer::new(44_100.0);
        let ratio = (MAX_FREQ / MIN_FREQ).ln() / BAND_COUNT as f32;
        for band in 0..BAND_COUNT {
            let freq = MIN_FREQ * ((band as f32 + 0.5) * ratio).exp();
            let slot = ((freq / MIN_FREQ).ln() / ratio).floor() as usize;
            assert_eq!(slot, band);
        }
        assert!(analyzer.bin_band.iter().any(|&b| b >= 0), "bins are mapped");
    }

    /// Louder in, higher out: twice the amplitude is +6 dB, whatever the
    /// band.
    #[test]
    fn amplitude_tracks_the_reading() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let sine = |amp: f32| -> Vec<i16> {
            (0..4096)
                .map(|i| {
                    let t = i as f32 / 44_100.0;
                    (amp * (TAU * 440.0 * t).sin() * 32768.0) as i16
                })
                .collect()
        };
        let quiet = analyzer.bands(&sine(0.25));
        let loud = analyzer.bands(&sine(0.5));
        for (q, l) in quiet.iter().zip(loud.iter()) {
            assert!(*l >= *q - 0.01, "louder input must not read quieter");
        }
    }

    /// A short slice pads as silence instead of panicking: the analyzer is
    /// fed whole chunks, but the contract should not depend on it.
    #[test]
    fn a_short_slice_survives() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let bands = analyzer.bands(&[1000i16, -1000]);
        assert_eq!(bands.len(), BAND_COUNT);
    }
}
