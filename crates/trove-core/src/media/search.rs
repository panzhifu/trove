//! Visual similarity search: perceptual hashing (pHash) and color matching.
//!
//! Two signals compose "search by image":
//!   1. Color palette similarity (cosine distance over quantized RGB buckets)
//!   2. Perceptual hash hamming distance (DCT-based pHash, 64-bit)
//!
//! The combined score ranks results. Both signals are cheap to compute at
//! import time and cheap to query at search time (no ML model needed).

use std::path::Path;

use image;

// ---------------------------------------------------------------------------
// Color histogram (16 bins per channel = 4096 buckets)
// ---------------------------------------------------------------------------

/// A 4096-bucket quantized color histogram (16 levels per channel).
/// Used for fast color-palette similarity between images.
#[derive(Debug, Clone)]
pub struct ColorHistogram {
    /// Flat 16×16×16 buffer, row-major: ((r * 16) + g) * 16 + b.
    pub buckets: [f32; 4096],
    /// Total pixels (for normalization).
    pub total: f32,
}

impl Default for ColorHistogram {
    fn default() -> Self {
        Self {
            buckets: [0.0_f32; 4096],
            total: 0.0,
        }
    }
}

impl ColorHistogram {
    /// Build a histogram from a pre-decoded RGB image.
    pub fn from_rgb(image: &image::RgbImage) -> Self {
        let mut buckets = [0.0_f32; 4096];
        let mut total = 0.0_f32;

        // Downsample to ~128px for speed (signature doesn't need high res).
        let (w, h) = image.dimensions();
        let max_dim = w.max(h);
        let scaled = if max_dim > 128 {
            let scale = 128.0 / max_dim as f32;
            let nw = (w as f32 * scale).max(1.0) as u32;
            let nh = (h as f32 * scale).max(1.0) as u32;
            image::imageops::resize(image, nw, nh, image::imageops::FilterType::Triangle)
        } else {
            image.clone()
        };

        for px in scaled.pixels() {
            let r = (px[0] as usize) >> 4;
            let g = (px[1] as usize) >> 4;
            let b = (px[2] as usize) >> 4;
            let idx = ((r << 8) | (g << 4) | b) & 0xFFF;
            buckets[idx] += 1.0;
            total += 1.0;
        }

        let mut hist = Self { buckets, total };
        if total > 0.0 {
            hist.normalize();
        }
        hist
    }

    /// Build a histogram from an image file.
    pub fn from_image(path: &Path) -> Self {
        match crate::media::hdr::open_for_display(path) {
            Ok(img) => Self::from_rgb(&img.to_rgb8()),
            _ => Self::default(),
        }
    }

    /// Cosine similarity to another histogram (1.0 = identical, 0.0 = unrelated).
    /// Both histograms must be L2-normalized (call `normalize()` after construction).
    pub fn cosine_similarity(&self, other: &Self) -> f32 {
        if self.total == 0.0 || other.total == 0.0 {
            return 0.0;
        }
        let mut dot = 0.0_f32;
        for i in 0..4096 {
            dot += self.buckets[i] * other.buckets[i];
        }
        dot.clamp(0.0, 1.0)
    }

    /// L2-normalize the histogram in place (required before cosine_similarity).
    pub fn normalize(&mut self) {
        let norm = self
            .buckets
            .iter()
            .map(|v| v * v)
            .sum::<f32>()
            .sqrt()
            .max(1e-9);
        for v in &mut self.buckets {
            *v /= norm;
        }
        self.total = 1.0;
    }

    /// Serialize to a compact hex string (4096 × 2-bit quantization → 1024 bytes).
    pub fn to_compact(&self) -> String {
        // Quantize each f32 bucket to 2 bits (0–3) for compact storage.
        self.buckets
            .chunks(4)
            .map(|chunk| {
                let mut byte = 0_u8;
                for (i, v) in chunk.iter().enumerate() {
                    let q = (v * 3.0).min(3.0) as u8; // 0–3
                    byte |= q << (i * 2);
                }
                byte
            })
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Deserialize from compact hex string. The histogram is L2-normalized.
    pub fn from_compact(s: &str) -> Self {
        let mut buckets = [0.0_f32; 4096];
        let mut idx = 0;
        for chunk in s.as_bytes().chunks(2) {
            if chunk.len() == 2
                && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(chunk).unwrap_or("00"), 16)
            {
                for i in 0..4 {
                    if idx < 4096 {
                        buckets[idx] = ((byte >> (i * 2)) & 0x03) as f32 / 3.0;
                        idx += 1;
                    }
                }
            }
        }
        let total = if idx > 0 { 1.0 } else { 0.0 };
        let mut hist = Self { buckets, total };
        if total > 0.0 {
            hist.normalize();
        }
        hist
    }
}

// ---------------------------------------------------------------------------
// Perceptual hash (DCT-based pHash, 64-bit)
// ---------------------------------------------------------------------------

/// A 64-bit perceptual hash. Hamming distance measures visual similarity:
/// 0 = identical, <10 = very similar, >20 = likely unrelated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PHash(pub u64);

impl PHash {
    /// Compute pHash from a pre-decoded grayscale image (9×8 or larger).
    pub fn from_gray(image: &image::GrayImage) -> Self {
        let hash = if image.width() >= 9 && image.height() >= 8 {
            let small = image::imageops::resize(image, 9, 8, image::imageops::FilterType::Triangle);
            Self::dhash(&small)
        } else {
            0
        };
        Self(hash)
    }

    /// Compute pHash from an image file. Returns a zero hash for undecodable files.
    pub fn from_image(path: &Path) -> Self {
        match crate::media::hdr::open_for_display(path) {
            Ok(img) => {
                let gray = image::imageops::grayscale(&img);
                Self::from_gray(&gray)
            }
            _ => Self(0),
        }
    }

    /// Difference hash: compare adjacent pixels in an 9×8 grid (64 bits).
    /// Robust to resizing, recoloring, and minor edits.
    fn dhash(img: &image::GrayImage) -> u64 {
        let (w, h) = img.dimensions();
        if w < 9 || h < 8 {
            return 0;
        }
        // Resize to 9×8 for a 64-bit hash (8 rows × 8 comparisons per row).
        let small = image::imageops::resize(img, 9, 8, image::imageops::FilterType::Triangle);
        let mut hash = 0_u64;
        for y in 0..8 {
            for x in 0..8 {
                let left = small.get_pixel(x, y)[0];
                let right = small.get_pixel(x + 1, y)[0];
                if right > left {
                    hash |= 1 << (y * 8 + x);
                }
            }
        }
        hash
    }

    /// Hamming distance to another hash (number of differing bits).
    pub fn hamming(self, other: Self) -> u32 {
        (self.0 ^ other.0).count_ones()
    }

    /// Similarity score (1.0 = identical, →0 = dissimilar).
    pub fn similarity(self, other: Self) -> f32 {
        let d = self.hamming(other);
        1.0 - (d as f32 / 64.0)
    }

    /// Serialize to hex.
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }

    /// Deserialize from hex.
    pub fn from_hex(s: &str) -> Self {
        Self(u64::from_str_radix(s, 16).unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------
// Combined visual signature (computed once at import time)
// ---------------------------------------------------------------------------

/// The full visual signature of an image, computed at import time and stored
/// in the asset's `extra` JSON. Powers "search by image".
#[derive(Debug, Clone)]
pub struct VisualSignature {
    /// 64-bit perceptual hash.
    pub phash: PHash,
    /// Compact color histogram.
    pub color_hist: ColorHistogram,
}

impl VisualSignature {
    /// Compute the full visual signature from a decoded RGB image.
    /// Use this when you already have the image decoded to avoid double decode.
    pub fn from_rgb(image: &image::RgbImage) -> Self {
        // Downsample once to a working size for both algorithms.
        let (w, h) = image.dimensions();
        let max_dim = w.max(h);
        // pHash needs only 9×8; histogram ~128px. Use a common working size.
        let working = if max_dim > 256 {
            let scale = 256.0 / max_dim as f32;
            let nw = (w as f32 * scale).max(9.0) as u32;
            let nh = (h as f32 * scale).max(9.0) as u32;
            image::imageops::resize(image, nw, nh, image::imageops::FilterType::Triangle)
        } else {
            image.clone()
        };

        // Single decode → compute both from the same buffer.
        let gray = image::imageops::grayscale(&working);
        let phash = PHash::from_gray(&gray);
        let color_hist = ColorHistogram::from_rgb(&working);

        Self { phash, color_hist }
    }

    /// Compute the full visual signature from an image file.
    pub fn from_image(path: &Path) -> Self {
        match crate::media::hdr::open_for_display(path) {
            Ok(img) => Self::from_rgb(&img.to_rgb8()),
            _ => Self {
                phash: PHash(0),
                color_hist: ColorHistogram::default(),
            },
        }
    }

    /// Combined similarity to another signature.
    /// Returns 0.0–1.0 (higher = more similar).
    pub fn similarity(&self, other: &Self) -> f32 {
        let phash_sim = if self.phash == PHash(0) || other.phash == PHash(0) {
            // One side has no pHash (e.g. color-only search). Use color only.
            -1.0 // sentinel: signal to use color only
        } else {
            self.phash.similarity(other.phash)
        };
        let color_sim = self.color_hist.cosine_similarity(&other.color_hist);
        if phash_sim < 0.0 {
            // pHash not available on one side; use color similarity alone.
            color_sim
        } else {
            // Weighted combination: pHash captures structure, color captures palette.
            phash_sim * 0.6 + color_sim * 0.4
        }
    }

    /// Store in the asset's typed facts (persisted in the `extra` column).
    pub fn apply_to_facts(&self, facts: &mut crate::model::AssetFacts) {
        facts.visual.visual_phash = Some(self.phash.to_hex());
        facts.visual.visual_color_hist = Some(self.color_hist.to_compact());
    }

    /// Load from the asset's typed facts (parsed from the `extra` column).
    pub fn from_facts(facts: &crate::model::AssetFacts) -> Option<Self> {
        let phash = facts.visual.visual_phash.as_deref().map(PHash::from_hex)?;
        let color_hist = facts
            .visual
            .visual_color_hist
            .as_deref()
            .map(ColorHistogram::from_compact)?;
        Some(Self { phash, color_hist })
    }
}

// ---------------------------------------------------------------------------
// Color hex search (standalone)
// ---------------------------------------------------------------------------

/// Parse a `#rrggbb` hex string into an `[u8; 3]`.
pub fn hex_to_rgb(hex: &str) -> Option<[u8; 3]> {
    let s = hex.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Euclidean distance between two RGB colors (0–441.67).
pub fn rgb_distance(a: [u8; 3], b: [u8; 3]) -> f32 {
    let dr = a[0] as f32 - b[0] as f32;
    let dg = a[1] as f32 - b[1] as f32;
    let db = a[2] as f32 - b[2] as f32;
    (dr * dr + dg * dg + db * db).sqrt()
}

/// Similarity score from a color distance (1.0 = identical, 0.0 = opposite).
pub fn color_similarity(distance: f32) -> f32 {
    // Max possible distance in RGB is sqrt(255² × 3) ≈ 441.67.
    (1.0 - distance / 441.67).max(0.0)
}

// ---------------------------------------------------------------------------
// Colour matching in the space the question is asked in
// ---------------------------------------------------------------------------

/// A colour where it sorts like the eye sorts it: hue in degrees (0–360),
/// saturation and lightness in 0..=1.
///
/// RGB distance — what [`color_similarity`] measures — is the wrong space for a
/// "find me the red ones" question: two reds of different brightness are far
/// apart in RGB and adjacent in hue, and a grey has no hue at all but still
/// scores as if it did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsl {
    pub h: f32,
    pub s: f32,
    pub l: f32,
}

/// Convert an 8-bit RGB to HSL. Pure black and pure white land at `s = 0` with
/// an undefined hue, which is reported as 0 rather than as a noise value — the
/// caller decides that a hueless colour matches on lightness alone.
pub fn rgb_to_hsl(rgb: [u8; 3]) -> Hsl {
    let r = rgb[0] as f32 / 255.0;
    let g = rgb[1] as f32 / 255.0;
    let b = rgb[2] as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta == 0.0 {
        return Hsl {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }
    // A dark colour's saturation is relative to how little light there is to
    // carry it, a bright one's to how little room is left.
    let denominator = 1.0 - (2.0 * lightness - 1.0).abs();
    let saturation = if denominator > 0.0 {
        delta / denominator
    } else {
        0.0
    };
    let hue = if max == r {
        ((g - b) / delta) % 6.0
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    let mut hue = hue * 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    Hsl {
        h: hue,
        s: saturation,
        l: lightness,
    }
}

/// Below this saturation a colour has no hue worth matching on. The rule is
/// asymmetric, and both halves matter: a grey *ask* ("dark", "pale") is answered
/// by lightness alone, while a grey *candidate* never answers a coloured ask —
/// without the floor, a near-black pixel with 3 % red in it reads as "red" and a
/// colour filter fills up with blacks.
pub const CHROMA_FLOOR: f32 = 0.15;

/// How wide a colour counts as a match.
///
/// Built from the 0–100 "similarity" the interface asks for: 0 is the loosest
/// box (55° of hue, half the saturation and lightness range), 100 the tightest
/// (10°, 0.1, 0.1). The endpoints and the linear interpolation between them are
/// the shape a colour filter has to have — a slider that changed only a score
/// threshold while the box stayed fixed would let the user widen *nothing*.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColourMatch {
    pub hue_span: f32,
    pub sat_delta: f32,
    pub light_delta: f32,
    pub chroma_floor: f32,
}

impl ColourMatch {
    /// The box for a 0–100 similarity ask. Values outside are clamped, so the
    /// caller can pass a slider through unmodified.
    pub fn from_similarity(similarity: f32) -> Self {
        let t = similarity.clamp(0.0, 100.0) / 100.0;
        Self {
            hue_span: 55.0 + (10.0 - 55.0) * t,
            sat_delta: 0.5 + (0.1 - 0.5) * t,
            light_delta: 0.42 + (0.1 - 0.42) * t,
            chroma_floor: CHROMA_FLOOR,
        }
    }

    /// How well `candidate` answers a request for `query`, from 0.0 (outside the
    /// box) to 1.0 (the same colour).
    ///
    /// The worst axis decides the score: a red with exactly the right hue but a
    /// lightness at the box edge is not "a close match", it is a miss with one
    /// axis saved. Averaging the axes would hide that and rank dark maroons
    /// beside bright scarlet.
    pub fn score(&self, query: Hsl, candidate: Hsl) -> f32 {
        let lightness = self.axis(query.l - candidate.l, self.light_delta);
        if query.s < self.chroma_floor {
            // The ask has no hue at all — it is "dark", "pale", "grey" — so hue
            // cannot answer it and lightness is the whole question.
            return lightness;
        }
        if candidate.s < self.chroma_floor {
            // And the converse, which is the one that reads as a bug if you get
            // it wrong: a grey is not a shade of red however exactly its
            // lightness lines up, so it never answers a coloured ask.
            return 0.0;
        }
        let delta = (query.h - candidate.h).abs() % 360.0;
        // Hue is a circle: 350° and 10° are 20° apart, not 340°.
        let hue_delta = delta.min(360.0 - delta);
        self.axis(hue_delta, self.hue_span)
            .min(self.axis(query.s - candidate.s, self.sat_delta))
            .min(lightness)
    }

    /// One axis's share of the score: how much of the half-width is left.
    fn axis(&self, delta: f32, half_width: f32) -> f32 {
        (1.0 - delta.abs() / half_width).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_histogram_similarity() {
        let mut hist1 = ColorHistogram::from_compact(&"00".repeat(1024));
        let hist2 = ColorHistogram::from_compact(&"00".repeat(1024));
        // Both zero histograms → 0 similarity (no signal).
        assert_eq!(hist1.cosine_similarity(&hist2), 0.0);

        // Same histogram → 1.0.
        hist1.total = 1.0;
        hist1.buckets[0] = 1.0;
        let hist3 = hist1.clone();
        assert!(hist1.cosine_similarity(&hist3) > 0.99);
    }

    #[test]
    fn phash_hamming() {
        let a = PHash(0x0000_0000_0000_0000);
        let b = PHash(0x0000_0000_0000_0001);
        assert_eq!(a.hamming(b), 1);

        let c = PHash(0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(a.hamming(c), 64);
        assert!(a.similarity(b) > a.similarity(c));
    }

    #[test]
    fn hex_to_rgb_roundtrip() {
        assert_eq!(hex_to_rgb("#ff8000"), Some([255, 128, 0]));
        assert_eq!(hex_to_rgb("ff8000"), Some([255, 128, 0]));
        assert_eq!(hex_to_rgb("#zzz"), None);
    }

    #[test]
    fn phash_hex_roundtrip() {
        let h = PHash(0x1234_5678_9ABC_DEF0);
        assert_eq!(PHash::from_hex(&h.to_hex()), h);
    }

    #[test]
    fn visual_signature_facts_roundtrip() {
        let mut facts = crate::model::AssetFacts::default();
        let sig = VisualSignature {
            phash: PHash(0xABCD),
            color_hist: ColorHistogram::from_compact(&"00".repeat(1024)),
        };
        sig.apply_to_facts(&mut facts);
        assert!(facts.visual.visual_phash.is_some());
        assert!(facts.visual.visual_color_hist.is_some());
        let loaded = VisualSignature::from_facts(&facts).unwrap();
        assert_eq!(loaded.phash, sig.phash);
    }

    #[test]
    fn rgb_to_hsl_lands_where_the_eye_lands() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        let red = rgb_to_hsl([255, 0, 0]);
        assert!(
            near(red.h, 0.0) && near(red.s, 1.0) && near(red.l, 0.5),
            "{red:?}"
        );
        // Same hue, different brightness: RGB distance calls these far apart,
        // the hue says they are the same colour, and that is the point.
        let dark_red = rgb_to_hsl([128, 0, 0]);
        assert!(
            near(dark_red.h, 0.0) && near(dark_red.l, 0.25),
            "{dark_red:?}"
        );
        let green = rgb_to_hsl([0, 255, 0]);
        assert!(near(green.h, 120.0), "{green:?}");
        let blue = rgb_to_hsl([0, 0, 255]);
        assert!(near(blue.h, 240.0), "{blue:?}");
        for grey in [[0u8, 0, 0], [128, 128, 128], [255, 255, 255]] {
            let h = rgb_to_hsl(grey);
            assert_eq!(h.s, 0.0, "{grey:?} has no saturation to speak of");
        }
    }

    #[test]
    fn a_tighter_similarity_narrows_the_box_on_every_axis() {
        let loose = ColourMatch::from_similarity(0.0);
        let tight = ColourMatch::from_similarity(100.0);
        assert!((loose.hue_span - 55.0).abs() < 0.01, "{loose:?}");
        assert!((tight.hue_span - 10.0).abs() < 0.01, "{tight:?}");
        assert!((tight.sat_delta - 0.1).abs() < 0.01);
        assert!((tight.light_delta - 0.1).abs() < 0.01);
        assert!(tight.hue_span < loose.hue_span);
        assert!(tight.sat_delta < loose.sat_delta);
        assert!(tight.light_delta < loose.light_delta);
        // The slider's ends are the contract; a value off the rail is clamped
        // rather than becoming a negative box that matches nothing at all.
        assert_eq!(
            ColourMatch::from_similarity(-50.0),
            ColourMatch::from_similarity(0.0)
        );
        assert_eq!(
            ColourMatch::from_similarity(999.0),
            ColourMatch::from_similarity(100.0)
        );
    }

    #[test]
    fn colour_scores_are_worst_axis_not_average_and_wrap_on_hue() {
        let red = rgb_to_hsl([255, 0, 0]);
        let match_loose = ColourMatch::from_similarity(0.0);
        // Same colour, both ends of the rail.
        assert!((match_loose.score(red, red) - 1.0).abs() < 1e-6);
        assert!((ColourMatch::from_similarity(100.0).score(red, red) - 1.0).abs() < 1e-6);
        // Hue is a circle: these two sit ~2.5° either side of 0°, so a naive
        // |a - b| would call them 355° apart and reject them.
        let wrap_a = rgb_to_hsl([255, 0, 10]);
        let wrap_b = rgb_to_hsl([255, 10, 0]);
        assert!(
            match_loose.score(wrap_a, wrap_b) > 0.0,
            "adjacent hues across 0° must match: {wrap_a:?} {wrap_b:?}"
        );
        // Green is 120° from red: outside even the loosest box.
        assert_eq!(match_loose.score(red, rgb_to_hsl([0, 255, 0])), 0.0);
        // Worst axis, not average: right hue and saturation, lightness past the
        // edge, is a miss rather than a third of a match.
        let box_tight = ColourMatch::from_similarity(100.0);
        let dark_red = rgb_to_hsl([128, 0, 0]);
        assert_eq!(
            box_tight.score(red, dark_red),
            0.0,
            "{red:?} vs {dark_red:?}"
        );
        // A neutral is not a shade of anything: a grey never answers a coloured
        // ask, however exactly its lightness lines up.
        let grey = rgb_to_hsl([128, 128, 128]);
        assert_eq!(
            match_loose.score(red, grey),
            0.0,
            "a grey answered a red query: {grey:?}"
        );
        // The converse does hold: a *grey ask* is a question about lightness,
        // and a saturated colour at that brightness is a plausible answer.
        assert!(
            match_loose.score(grey, red) > 0.0,
            "nothing answered a mid-grey ask"
        );
        assert_eq!(
            match_loose.score(red, rgb_to_hsl([255, 255, 255])),
            0.0,
            "white is too far from red's lightness even for the loosest box"
        );
    }
}
