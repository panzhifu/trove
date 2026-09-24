//! Visual similarity search: find images similar to a query image or color.
//!
//! Two search modes:
//! - [`search_by_image`] — find assets visually similar to a given image
//!   (using perceptual hash + color histogram from the stored signatures).
//! - [`search_by_color`] — find assets matching a specific hex color.

use super::assets::{self, COLS};
use super::rows;
use crate::error::Result;
use crate::media::search::{self, ColorHistogram, PHash, VisualSignature};
use crate::model::Asset;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

/// The four `extra` keys a similarity scan needs, and nothing else.
///
/// [`crate::model::AssetFacts`] is the typed view of the whole blob, which is
/// right for one asset and far too much work for fifty thousand of them: its
/// flattened sub-structs and its catch-all map of unrecognised keys cost a
/// parse of every field per row. The keys are flat here because the facts
/// struct flattens them onto one JSON map (`model/facts.rs`) — `$.visual_phash`,
/// not `$.visual.visual_phash`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Signature {
    #[serde(rename = "visual_phash")]
    phash: Option<String>,
    #[serde(rename = "visual_color_hist")]
    hist: Option<String>,
    #[serde(rename = "dominant_color")]
    dominant: Option<String>,
    #[serde(rename = "dominant_colors")]
    dominants: Option<Vec<String>>,
}

impl Signature {
    /// The stored signature, or `None` when the row has not been mined yet.
    fn as_visual_signature(&self) -> Option<VisualSignature> {
        let phash = self.phash.as_deref().map(PHash::from_hex)?;
        let color_hist = self.hist.as_deref().map(ColorHistogram::from_compact)?;
        Some(VisualSignature { phash, color_hist })
    }

    /// The colours to compare a picked colour against: the palette when the
    /// miner found several, the single dominant one otherwise.
    fn dominant_colors(&self) -> Vec<&str> {
        match &self.dominants {
            Some(list) if !list.is_empty() => list.iter().map(String::as_str).collect(),
            _ => self.dominant.iter().map(String::as_str).collect(),
        }
    }
}

/// Compute and store visual signature for a single asset (background task).
pub fn compute_and_store_signature(
    store: &crate::store::Store,
    library_root: &Path,
    asset_id: Uuid,
) -> Result<bool> {
    let conn = store.conn();
    let assets_list = rows::query_map(
        conn,
        &format!("SELECT {COLS} FROM assets WHERE id = ?1"),
        vec![rows::uuid(asset_id).into()],
        assets::asset_from_row,
    )?;

    let asset = match assets_list.into_iter().next() {
        Some(a) => a,
        None => return Ok(false),
    };

    if asset.kind != crate::model::AssetKind::Image {
        return Ok(false);
    }

    if let Some(ref rel) = asset.rel_path {
        let path = library_root.join(rel);
        let sig = VisualSignature::from_image(&path);
        if sig.phash != PHash(0) {
            let mut facts = asset.facts.clone();
            sig.apply_to_facts(&mut facts);
            assets::update_facts(conn, asset.id, &facts)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Batch compute visual signatures for assets (background processing).
/// Returns the number of assets updated.
pub fn compute_signatures_batch(
    store: &crate::store::Store,
    library_root: &Path,
    asset_ids: &[Uuid],
) -> Result<u64> {
    let mut updated = 0_u64;
    for id in asset_ids {
        if let Ok(true) = compute_and_store_signature(store, library_root, *id) {
            updated += 1
        }
    }
    Ok(updated)
}

/// Get IDs of image assets that don't have a visual signature yet.
pub fn assets_needing_signature(store: &crate::store::Store) -> Result<Vec<Uuid>> {
    let conn = store.conn();
    rows::query_map(
        conn,
        "SELECT id FROM assets WHERE kind = 'image' AND trashed_at IS NULL \
             AND (extra NOT LIKE '%visual_phash%' OR extra IS NULL)",
        vec![],
        |row| rows::req_uuid(row, 0),
    )
}

/// `(signed_images, total_live_images)` — coverage for the settings page.
/// A signature is present when `extra` carries the `visual_phash` key.
pub fn signature_counts(conn: &rusqlite::Connection) -> Result<(u64, u64)> {
    let total = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE kind = 'image' AND trashed_at IS NULL",
        vec![],
    )? as u64;
    let signed = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE kind = 'image' AND trashed_at IS NULL
         AND extra LIKE '%visual_phash%'",
        vec![],
    )? as u64;
    Ok((signed, total))
}

/// Search results with similarity scores.
#[derive(Debug, Clone)]
pub struct SimilarAsset {
    pub asset: Asset,
    /// Similarity score (0.0–1.0, higher = more similar).
    pub score: f32,
}

/// Below this, a stored signature is not similar enough to a query image to be
/// worth showing. The histogram's own scale, not the colour box's.
const MIN_SIGNATURE_SIMILARITY: f32 = 0.2;

/// Find assets visually similar to the given image path.
///
/// Uses the stored pHash + color histogram signatures. Returns up to `limit`
/// results sorted by descending similarity (score ≥ [`MIN_SIGNATURE_SIMILARITY`]).
pub fn search_by_image(
    conn: &rusqlite::Connection,
    query_path: &std::path::Path,
    limit: Option<u32>,
) -> Result<Vec<SimilarAsset>> {
    let query_sig = VisualSignature::from_image(query_path);
    if query_sig.phash == PHash(0) {
        return Ok(Vec::new());
    }
    scan_and_rank(conn, limit, |signature| {
        let stored = signature.as_visual_signature()?;
        let score = query_sig.similarity(&stored);
        (score >= MIN_SIGNATURE_SIMILARITY).then_some(score)
    })
}

/// Find assets whose recorded palette answers a request for a specific hex
/// colour (e.g. `"#ff8000"`).
///
/// `similarity` is the interface's 0–100 ask: how tight the colour box is
/// ([`search::ColourMatch`]). It is *not* a score threshold — a wide box and a
/// narrow one are different questions, and each ranks its own answers.
///
/// This route scores the recorded dominant colours in HSL and does not decode a
/// colour histogram. Decoding one costs a 4096-float rebuild per row, and a
/// colour query asks something the palette already answers.
///
/// One case still needs the histogram: an asset whose signature was backfilled
/// by [`compute_and_store_signature`], which writes the hash and the histogram
/// but no palette. Those rows fall back to the old cosine, so a library mined
/// before the palette existed still answers a colour click.
pub fn search_by_color(
    conn: &rusqlite::Connection,
    hex: &str,
    similarity: f32,
    limit: Option<u32>,
) -> Result<Vec<SimilarAsset>> {
    let rgb = match search::hex_to_rgb(hex) {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let query = search::rgb_to_hsl(rgb);
    let box_of = search::ColourMatch::from_similarity(similarity);
    // Built once for the fallback rows: a histogram with all its weight in the
    // query colour's bucket.
    let mut buckets = [0.0_f32; 4096];
    buckets[((((rgb[0] as usize) >> 4) << 8)
        | (((rgb[1] as usize) >> 4) << 4)
        | ((rgb[2] as usize) >> 4))
        & 0xFFF] = 1.0;
    let mut query_hist = ColorHistogram {
        buckets,
        total: 1.0,
    };
    query_hist.normalize();
    let query_sig = VisualSignature {
        phash: PHash(0),
        color_hist: query_hist,
    };

    scan_and_rank(conn, limit, |signature| {
        let palette = signature.dominant_colors();
        if palette.is_empty() {
            return signature.as_visual_signature().and_then(|stored| {
                let score = query_sig.similarity(&stored);
                (score >= MIN_SIGNATURE_SIMILARITY).then_some(score)
            });
        }
        let best = palette
            .iter()
            .filter_map(|hex| search::hex_to_rgb(hex))
            .map(|rgb| box_of.score(query, search::rgb_to_hsl(rgb)))
            .fold(0.0_f32, f32::max);
        (best > 0.0).then_some(best)
    })
}

/// The shared skeleton of both similarity routes: read every live image's
/// recorded facts, ask `score` about them, keep what it ranks, cut to `limit`,
/// and only then fetch the records.
///
/// The scan reads `(id, extra)` and nothing else. It used to materialize a full
/// [`Asset`] per image — twenty-four columns, the timestamps parsed, the whole
/// `AssetFacts` struct with its flattened sub-structs and its catch-all map of
/// unknown keys — for every image in the library, then throw all but fifty of
/// them away. The record for a winner is fetched by id at the end, once.
fn scan_and_rank(
    conn: &rusqlite::Connection,
    limit: Option<u32>,
    mut score: impl FnMut(&Signature) -> Option<f32>,
) -> Result<Vec<SimilarAsset>> {
    let candidates: Vec<(Uuid, Signature)> = rows::query_map(
        conn,
        // No `extra LIKE '%visual_phash%'` predicate: it is a substring test on
        // a JSON blob, so it can match the string inside a *value* and miss the
        // point, and the row's JSON is parsed two lines below anyway.
        "SELECT id, extra FROM assets WHERE kind = 'image' AND trashed_at IS NULL",
        vec![],
        |row| {
            let id = rows::req_uuid(row, 0)?;
            let text = rows::req_str(row, 1)?;
            Ok((id, serde_json::from_str(&text).unwrap_or_default()))
        },
    )?;

    let mut scored: Vec<(Uuid, f32)> = Vec::new();
    for (id, signature) in &candidates {
        if let Some(value) = score(signature) {
            scored.push((*id, value));
        }
    }

    // Sort by descending score.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Apply limit.
    let limit = limit.unwrap_or(50) as usize;
    scored.truncate(limit);

    // Materialize only what is going on screen, in rank order.
    let ids: Vec<Uuid> = scored.iter().map(|(id, _)| *id).collect();
    let by_id: HashMap<Uuid, Asset> = assets::by_ids(conn, &ids)?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    Ok(scored
        .iter()
        .filter_map(|(id, score)| {
            by_id.get(id).map(|asset| SimilarAsset {
                asset: asset.clone(),
                score: *score,
            })
        })
        .collect())
}

/// Backfill visual signatures for existing image assets that don't have one.
/// Returns the number of assets updated.
/// `library_root` is the root path of the library (where `media/` lives).
pub fn backfill_signatures(
    store: &crate::store::Store,
    library_root: &std::path::Path,
) -> Result<u64> {
    let conn = store.conn();
    let rows_vec = rows::query_map(
        conn,
        &format!(
            "SELECT {COLS} FROM assets WHERE kind = 'image' AND trashed_at IS NULL \
             AND (extra NOT LIKE '%visual_phash%' OR extra IS NULL)"
        ),
        vec![],
        assets::asset_from_row,
    )?;

    let mut updated = 0_u64;
    for asset in rows_vec {
        if let Some(ref rel) = asset.rel_path {
            let path = library_root.join(rel);
            let sig = VisualSignature::from_image(&path);
            if sig.phash != PHash(0) {
                let mut facts = asset.facts.clone();
                sig.apply_to_facts(&mut facts);
                assets::update_facts(conn, asset.id, &facts)?;
                updated += 1;
            }
        }
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use uuid::Uuid;

    #[test]
    fn search_by_color_finds_similar() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();

        // Insert two assets: one red, one blue.
        let red_id = Uuid::new_v4();
        let blue_id = Uuid::new_v4();

        assets::insert(
            conn,
            &crate::model::test_asset("red.png", crate::model::AssetKind::Image, red_id),
        )
        .unwrap();
        assets::insert(
            conn,
            &crate::model::test_asset("blue.png", crate::model::AssetKind::Image, blue_id),
        )
        .unwrap();

        // Manually set color histograms.
        // Red (#ff0000): r=255>>4=15, g=0>>4=0, b=0>>4=0 → bucket ((15<<8)|(0<<4)|0) = 0xF00
        let mut buckets = [0.0_f32; 4096];
        buckets[0xF00] = 1.0; // red bucket (#ff0000)
        let hist = ColorHistogram {
            buckets,
            total: 1.0,
        };
        let red_facts = crate::model::AssetFacts {
            visual: crate::model::VisualFacts {
                visual_phash: Some("0000000000000001".into()),
                visual_color_hist: Some(hist.to_compact()),
                ..Default::default()
            },
            ..Default::default()
        };
        assets::update_facts(conn, red_id, &red_facts).unwrap();

        // Blue (#0000ff): r=0>>4=0, g=0>>4=0, b=255>>4=15 → bucket ((0<<8)|(0<<4)|15) = 0x00F
        let mut buckets = [0.0_f32; 4096];
        buckets[0x00F] = 1.0; // blue bucket (#0000ff)
        let hist = ColorHistogram {
            buckets,
            total: 1.0,
        };
        let blue_facts = crate::model::AssetFacts {
            visual: crate::model::VisualFacts {
                visual_phash: Some("0000000000000002".into()),
                visual_color_hist: Some(hist.to_compact()),
                ..Default::default()
            },
            ..Default::default()
        };
        assets::update_facts(conn, blue_id, &blue_facts).unwrap();

        // Search for red.
        let results = search_by_color(conn, "#ff0000", 50.0, None).unwrap();
        assert!(!results.is_empty(), "expected red results");
        // The red asset should rank higher than the blue one.
        assert_eq!(results[0].asset.id, red_id, "red should rank first");
    }

    /// The similarity rail is a question width, not a score threshold: sliding
    /// it toward "exact" removes answers rather than only reordering them, and
    /// a grey never answers a red ask at any width.
    #[test]
    fn a_colour_search_narrows_as_the_slider_tightens() {
        let store = crate::store::Store::in_memory().unwrap();
        let conn = store.conn();
        let mut by_id: HashMap<Uuid, &'static str> = HashMap::new();
        for (name, hex) in [
            ("scarlet.png", "#ff0000"),
            // Same hue and saturation, a quarter darker.
            ("maroon.png", "#800000"),
            ("azure.png", "#0000ff"),
            ("slate.png", "#808080"),
        ] {
            let asset =
                crate::model::test_asset(name, crate::model::AssetKind::Image, Uuid::new_v4());
            assets::insert(conn, &asset).unwrap();
            let facts = crate::model::AssetFacts {
                visual: crate::model::VisualFacts {
                    dominant_color: Some(hex.into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            assets::update_facts(conn, asset.id, &facts).unwrap();
            by_id.insert(asset.id, name);
        }
        let hits = |similarity: f32| -> Vec<&'static str> {
            search_by_color(conn, "#ff0000", similarity, None)
                .unwrap()
                .iter()
                .map(|hit| by_id[&hit.asset.id])
                .collect()
        };

        assert_eq!(
            hits(0.0),
            vec!["scarlet.png", "maroon.png"],
            "the loosest box takes the same hue at any of these lightnesses"
        );
        assert_eq!(
            hits(100.0),
            vec!["scarlet.png"],
            "the tightest box asks for red and takes nothing else"
        );
    }

    /// The cost of one colour search against a library full of signatures.
    ///
    /// Not a pass/fail test — it is the measurement behind `scan_and_rank`'s
    /// shape. `cargo test --release -p trove-core bench_colour_search -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_colour_search_over_a_full_library() {
        use std::time::Instant;
        let store = crate::store::Store::in_memory().unwrap();
        let conn = store.conn();
        let n = 50_000usize;
        let t = Instant::now();
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..n {
            // A real compact histogram: 1024 hex characters, as mining writes it.
            let hist: String = (0..1024)
                .map(|c| ((i * (c + 7)) % 16) as u8 as char)
                .map(|c| if c.is_ascii_hexdigit() { c } else { 'a' })
                .collect();
            conn.execute(
                "INSERT INTO assets (id, kind, file_name, created_at, updated_at, extra)
                 VALUES (?1, 'image', ?2, '2026-01-01T00:00:00+00:00',
                         '2026-01-01T00:00:00+00:00', ?3)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    format!("a{i}.png"),
                    format!(
                        "{{\"visual_phash\":\"{i:016x}\",\"visual_color_hist\":\"{hist}\",\
                         \"dominant_color\":\"#ff8000\",\"dominant_colors\":[\"#ff8000\"]}}"
                    )
                ],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
        println!("seeded {n} signed images in {:?}", t.elapsed());
        let t = Instant::now();
        let hits = search_by_color(conn, "#ff8000", 50.0, Some(50)).unwrap();
        println!(
            "search_by_color over {} signed rows: {:?} ({} hits)",
            n,
            t.elapsed(),
            hits.len()
        );
        let t = Instant::now();
        let hits = search_by_color(conn, "#ff8000", 50.0, Some(50)).unwrap();
        println!("second run: {:?} ({} hits)", t.elapsed(), hits.len());
    }
}
