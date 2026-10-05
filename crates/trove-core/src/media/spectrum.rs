//! Log-spaced spectrum bands from PCM — the live spectrum strip behind the
//! audio preview.
//!
//! The waveform envelope is a picture of the whole song; a spectrum is a
//! picture of *now*. The engine decodes 100 ms chunks on its way into the
//! sink, so the analysis rides the same path, folded into [`BAND_COUNT`]
//! geometric bands between [`MIN_FREQ`] and [`MAX_FREQ`] — the poor man's
//! CQT, one bin spacing per octave ratio, so a bass guitar and a hi-hat
//! get equal screen room.
//!
//! ## Two windows, one spectrum
//!
//! A single FFT cannot serve both ends of the range: 4096 points gives
//! 10.8 Hz bins, which merges the first two frets of a bass guitar into
//! one bar, while a window long enough to separate them would smear every
//! hi-hat across half a second. So the analyzer is two FFTs stitched at
//! band [`CROSSOVER_BAND`]: the low bands read a [`LOW_FFT_SIZE`] window
//! (2.7 Hz bins, held in a rolling history the caller maintains), the high
//! bands a [`HIGH_FFT_SIZE`] one. Long windows for pitch, short windows
//! for transients — the trade a constant-Q transform exists to make.
//!
//! Bands come out as 0..1 on a decibel scale with a display tilt and a
//! triangular smoothing pass: full-scale sine at the top, [`DB_FLOOR`] at
//! the bottom.

use std::f32::consts::TAU;

/// Bars on the strip. 48 across ~900 px gives ~19 px per bar — dense
/// enough to read as a spectrum, sparse enough to keep clean gaps.
pub const BAND_COUNT: usize = 48;

/// The analyzed range. 40 Hz is under the low E of a bass guitar; 16 kHz
/// covers everything above that a lossy stream bothers to keep.
pub const MIN_FREQ: f32 = 40.0;
pub const MAX_FREQ: f32 = 16_000.0;

/// The long window, for the low bands: 371 ms at 44.1 kHz, 2.7 Hz per bin.
/// That is what lets a 41 Hz E1 and a 47 Hz F1 land in different bars.
pub const LOW_FFT_SIZE: usize = 16_384;

/// The short window, for everything above the crossover: 93 ms at 44.1
/// kHz, 10.8 Hz per bin — plenty where a band is hundreds of Hz wide, and
/// fast enough to keep transients off the smear.
pub const HIGH_FFT_SIZE: usize = 4_096;

/// Bands below this index come from the long window, the rest from the
/// short one. Band 18 centers near 350 Hz: below it, pitch separation is
/// the point; above it, timing.
const CROSSOVER_BAND: usize = 18;

/// Band value at or below this many dB under full scale reads as silence.
const DB_FLOOR: f32 = -70.0;

/// Display tilt, +3 dB per octave referenced at [`TILT_REF_HZ`]. Deliberately
/// *not* A-weighting: the full psychoacoustic curve pulls 40 Hz down 35 dB
/// and the bass bars would never leave the floor. This is the gentler
/// balance analyzers actually display with — pink-ish masters read flat.
const TILT_DB_PER_OCT: f32 = 3.0;
const TILT_REF_HZ: f32 = 100.0;
const TILT_MAX_DB: f32 = 24.0;

/// One-third-octave smoothing over ±2 bands (~±0.35 octave at this band
/// spacing): adjacent bars jitter against each other for no reason a ear
/// can hear, and a triangular pass costs nothing.
const SMOOTH_KERNEL: [f32; 5] = [0.25, 0.5, 1.0, 0.5, 0.25];

/// A streaming analyzer. The windows, buffers and bin→band maps are built
/// once per playback and reused per chunk; the caller keeps the rolling
/// PCM history (see [`SpectrumAnalyzer::bands`]).
pub struct SpectrumAnalyzer {
    low: Stage,
    high: Stage,
    /// Display tilt per band, precomputed from the band's centre frequency.
    tilt: Vec<f32>,
}

struct Stage {
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// FFT bin → absolute band index, -1 for bins outside this stage's
    /// slice of the range.
    bin_band: Vec<i16>,
}

impl Stage {
    fn new(sample_rate: f32, n: usize, band_range: std::ops::Range<usize>) -> Self {
        // Hann window: the slice is an arbitrary cut of a continuous
        // stream, and a rectangular cut would smear every tone across the
        // bins its discontinuities leak into.
        let window: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos())
            .collect();
        let ratio = (MAX_FREQ / MIN_FREQ).ln() / BAND_COUNT as f32;
        let mut bin_band = vec![-1i16; n / 2];
        for (k, slot) in bin_band.iter_mut().enumerate() {
            let freq = k as f32 * sample_rate / n as f32;
            if !(MIN_FREQ..=MAX_FREQ).contains(&freq) {
                continue;
            }
            let band = ((freq / MIN_FREQ).ln() / ratio).floor() as usize;
            if band_range.contains(&band) {
                *slot = band as i16;
            }
        }
        Self {
            window,
            re: vec![0.0; n],
            im: vec![0.0; n],
            bin_band,
        }
    }

    /// Window the tail of `pcm`, transform, and fold the peaks into `db`
    /// (dB under full scale, per band). Only the last `n` frames are read;
    /// whatever the tail lacks pads as silence.
    fn analyze(&mut self, pcm: &[i16], db: &mut [f32]) {
        let n = self.re.len();
        let start = pcm.len().saturating_sub(n);
        for i in 0..n {
            let sample = if start + i < pcm.len() {
                f32::from(pcm[start + i]) / 32768.0
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
        for (k, &band) in self.bin_band.iter().enumerate() {
            let band: usize = match band.try_into() {
                Ok(band) => band,
                Err(_) => continue,
            };
            let mag = (self.re[k] * self.re[k] + self.im[k] * self.im[k]).sqrt() * norm;
            // Peak, not mean, per band — a band holding one loud
            // partial is loud, however quiet its floor.
            let reading = 20.0 * (mag + 1e-9).log10();
            if reading > db[band] {
                db[band] = reading;
            }
        }
    }
}

impl SpectrumAnalyzer {
    /// Build for `sample_rate` Hz input.
    pub fn new(sample_rate: f32) -> Self {
        let ratio = (MAX_FREQ / MIN_FREQ).ln() / BAND_COUNT as f32;
        let tilt = (0..BAND_COUNT)
            .map(|band| {
                let centre = MIN_FREQ * ((band as f32 + 0.5) * ratio).exp();
                (TILT_DB_PER_OCT * (centre / TILT_REF_HZ).log2()).clamp(0.0, TILT_MAX_DB)
            })
            .collect();
        Self {
            low: Stage::new(sample_rate, LOW_FFT_SIZE, 0..CROSSOVER_BAND),
            high: Stage::new(sample_rate, HIGH_FFT_SIZE, CROSSOVER_BAND..BAND_COUNT),
            tilt,
        }
    }

    /// Reduce recent mono PCM to [`BAND_COUNT`] band values, 0..1. `tail` is
    /// the rolling history, oldest first; the long window reads its last
    /// [`LOW_FFT_SIZE`] frames, the short one its last [`HIGH_FFT_SIZE`]. A
    /// tail shorter than the window pads as silence — a fresh playback
    /// fills it over the first few chunks.
    pub fn bands(&mut self, tail: &[i16]) -> Vec<f32> {
        // Per-band peaks in dB from both windows, then tilt, floor and the
        // smoothing pass.
        let mut db = vec![f32::NEG_INFINITY; BAND_COUNT];
        self.low.analyze(tail, &mut db);
        self.high.analyze(tail, &mut db);
        let mut bands: Vec<f32> = db
            .iter()
            .zip(&self.tilt)
            .map(|(&reading, &tilt)| {
                if reading == f32::NEG_INFINITY {
                    0.0
                } else {
                    ((reading + tilt - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0)
                }
            })
            .collect();
        // Triangular smoothing across the log-spaced neighbors. Peaks are
        // taken per band before this pass, so a band holding one loud
        // partial still leads its neighborhood.
        let source = bands.clone();
        for (k, slot) in bands.iter_mut().enumerate() {
            let mut acc = 0.0;
            let mut weight = 0.0;
            for (d, &w) in SMOOTH_KERNEL.iter().enumerate() {
                let j = k as isize + d as isize - 2;
                if j < 0 || j >= source.len() as isize {
                    continue;
                }
                acc += source[j as usize] * w;
                weight += w;
            }
            *slot = acc / weight;
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
        assert!(
            peak > 0.4,
            "a half-scale tone must read clearly, got {peak}"
        );
    }

    /// The two-window stitching pays for itself at the bottom: 41 Hz and 47
    /// Hz are five frets' worth of nothing apart, and a single 4096-point
    /// window maps both into band 0 (bins 3.8 and 4.4, band edge at
    /// 45.1 Hz). The long window's 2.7 Hz bins put them in separate bars.
    #[test]
    fn two_adjacent_bass_notes_split_across_bands() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let pcm: Vec<i16> = (0..LOW_FFT_SIZE)
            .map(|i| {
                let t = i as f32 / 44_100.0;
                // Two 0.4-amplitude sines; the sum stays under full scale.
                let a = 0.4 * (TAU * 41.2 * t).sin();
                let b = 0.4 * (TAU * 47.0 * t).sin();
                ((a + b) * 32768.0) as i16
            })
            .collect();
        let bands = analyzer.bands(&pcm);
        assert!(
            bands[0] > 0.15 && bands[1] > 0.15,
            "E1 and F1 must read in their own bars, got {:?}",
            &bands[0..3]
        );
    }

    /// Silence reads as silence everywhere — no window leakage off the
    /// floor.
    #[test]
    fn silence_is_silence() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let bands = analyzer.bands(&vec![0i16; LOW_FFT_SIZE]);
        assert!(bands.iter().all(|&b| b == 0.0));
    }

    /// The band map is monotone: every band's centre frequency maps back to
    /// its own slot in whichever stage owns it.
    #[test]
    fn bands_tile_the_range_in_order() {
        let analyzer = SpectrumAnalyzer::new(44_100.0);
        let ratio = (MAX_FREQ / MIN_FREQ).ln() / BAND_COUNT as f32;
        for band in 0..BAND_COUNT {
            let freq = MIN_FREQ * ((band as f32 + 0.5) * ratio).exp();
            let slot = ((freq / MIN_FREQ).ln() / ratio).floor() as usize;
            assert_eq!(slot, band);
            let owner = if band < CROSSOVER_BAND {
                &analyzer.low
            } else {
                &analyzer.high
            };
            assert!(
                owner.bin_band.contains(&(band as i16)),
                "band {band} must have bins in its stage"
            );
        }
    }

    /// Louder in, higher out: twice the amplitude is +6 dB, whatever the
    /// band.
    #[test]
    fn amplitude_tracks_the_reading() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let sine = |amp: f32| -> Vec<i16> {
            (0..HIGH_FFT_SIZE)
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
    /// fed a rolling history, which starts short on a fresh playback.
    #[test]
    fn a_short_slice_survives() {
        let mut analyzer = SpectrumAnalyzer::new(44_100.0);
        let bands = analyzer.bands(&[1000i16, -1000]);
        assert_eq!(bands.len(), BAND_COUNT);
    }
}
