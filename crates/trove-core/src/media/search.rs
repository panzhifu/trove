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
        match image::open(path) {
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
        match image::open(path) {
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
        match image::open(path) {
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

    /// Store in the asset's `extra` map (BTreeMap form, JSON values).
    pub fn apply_to_extra(
        &self,
        extra: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        extra.insert(
            "visual_phash".into(),
            serde_json::Value::String(self.phash.to_hex()),
        );
        extra.insert(
            "visual_color_hist".into(),
            serde_json::Value::String(self.color_hist.to_compact()),
        );
    }

    /// Load from a JSON object (parsed from the `extra` column).
    pub fn from_extra(extra: &serde_json::Map<String, serde_json::Value>) -> Option<Self> {
        let phash = extra
            .get("visual_phash")
            .and_then(|v| v.as_str())
            .map(PHash::from_hex)?;
        let color_hist = extra
            .get("visual_color_hist")
            .and_then(|v| v.as_str())
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
    fn visual_signature_extra_roundtrip() {
        let mut extra = std::collections::BTreeMap::new();
        let sig = VisualSignature {
            phash: PHash(0xABCD),
            color_hist: ColorHistogram::from_compact(&"00".repeat(1024)),
        };
        sig.apply_to_extra(&mut extra);
        assert!(extra.contains_key("visual_phash"));
        assert!(extra.contains_key("visual_color_hist"));
        // Convert BTreeMap to serde_json::Map for from_extra.
        let extra_json: serde_json::Map<String, serde_json::Value> =
            extra.into_iter().map(|(k, v)| (k, v)).collect();
        let loaded = VisualSignature::from_extra(&extra_json).unwrap();
        assert_eq!(loaded.phash, sig.phash);
    }
}
