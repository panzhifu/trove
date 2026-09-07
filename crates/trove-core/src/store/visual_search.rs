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
use std::path::Path;
use uuid::Uuid;

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
        let path = library_root.join("media").join(rel);
        let sig = VisualSignature::from_image(&path);
        if sig.phash != PHash(0) {
            let mut extra = asset.extra.clone();
            sig.apply_to_extra(&mut extra);
            assets::update_extra(conn, asset.id, &extra)?;
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

/// Search results with similarity scores.
#[derive(Debug, Clone)]
pub struct SimilarAsset {
    pub asset: Asset,
    /// Similarity score (0.0–1.0, higher = more similar).
    pub score: f32,
}

/// Find assets visually similar to the given image path.
///
/// Uses the stored pHash + color histogram signatures. Returns up to `limit`
/// results sorted by descending similarity (score ≥ 0.2).
pub fn search_by_image(
    conn: &rusqlite::Connection,
    query_path: &std::path::Path,
    limit: Option<u32>,
) -> Result<Vec<SimilarAsset>> {
    let query_sig = VisualSignature::from_image(query_path);
    if query_sig.phash == PHash(0) {
        return Ok(Vec::new());
    }

    collect_and_rank(conn, &query_sig, limit, None)
}

/// Find assets matching a specific hex color (e.g. "#ff8000").
///
/// Uses the stored color histogram. Returns up to `limit` results sorted by
/// descending color similarity.
pub fn search_by_color(
    conn: &rusqlite::Connection,
    hex: &str,
    limit: Option<u32>,
) -> Result<Vec<SimilarAsset>> {
    let rgb = match search::hex_to_rgb(hex) {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };

    // Build a synthetic histogram centered on the query color.
    let mut buckets = [0.0_f32; 4096];
    let r = (rgb[0] as usize) >> 4;
    let g = (rgb[1] as usize) >> 4;
    let b = (rgb[2] as usize) >> 4;
    let idx = ((r << 8) | (g << 4) | b) & 0xFFF;
    buckets[idx] = 1.0;
    let mut query_hist = ColorHistogram {
        buckets,
        total: 1.0,
    };
    query_hist.normalize();

    let query_sig = VisualSignature {
        phash: PHash(0),
        color_hist: query_hist,
    };

    collect_and_rank(conn, &query_sig, limit, Some(rgb))
}

/// Core: scan all image assets, compute similarity, rank, return top results.
fn collect_and_rank(
    conn: &rusqlite::Connection,
    query_sig: &VisualSignature,
    limit: Option<u32>,
    query_rgb: Option<[u8; 3]>,
) -> Result<Vec<SimilarAsset>> {
    // Fetch all image assets that have a visual signature.
    let rows_vec = rows::query_map(
        conn,
        &format!("SELECT {COLS} FROM assets WHERE kind = 'image' AND trashed_at IS NULL"),
        vec![],
        assets::asset_from_row,
    )?;

    let mut scored: Vec<SimilarAsset> = Vec::new();
    let min_threshold = 0.2_f32;

    for asset in rows_vec {
        // Parse the extra field as JSON to extract visual signature.
        let extra_json: serde_json::Value =
            serde_json::to_value(&asset.extra).unwrap_or(serde_json::Value::Null);
        let extra_map = extra_json.as_object();

        // Try to load the stored visual signature.
        if let Some(stored_sig) = extra_map.and_then(VisualSignature::from_extra) {
            let mut score = query_sig.similarity(&stored_sig);

            // Boost score if a specific color was queried and the asset's
            // dominant colors contain a close match.
            if let Some(qrgb) = query_rgb
                && let Some(colors) = extra_json
                    .pointer("/dominant_colors")
                    .and_then(|v| v.as_array())
            {
                for c in colors {
                    if let Some(hex_str) = c.as_str()
                        && let Some(crgb) = search::hex_to_rgb(hex_str)
                    {
                        let csim = search::color_similarity(search::rgb_distance(qrgb, crgb));
                        score = score.max(csim * 0.8);
                    }
                }
            }

            if score >= min_threshold {
                scored.push(SimilarAsset { asset, score });
            }
        }
    }

    // Sort by descending score.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Apply limit.
    let limit = limit.unwrap_or(50) as usize;
    scored.truncate(limit);

    Ok(scored)
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
            let path = library_root.join("media").join(rel);
            let sig = VisualSignature::from_image(&path);
            if sig.phash != PHash(0) {
                let mut extra = asset.extra.clone();
                sig.apply_to_extra(&mut extra);
                assets::update_extra(conn, asset.id, &extra)?;
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
        let mut extra_red: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut buckets = [0.0_f32; 4096];
        buckets[0xF00] = 1.0; // red bucket (#ff0000)
        let hist = ColorHistogram {
            buckets,
            total: 1.0,
        };
        extra_red.insert("visual_phash".into(), "0000000000000001".into());
        extra_red.insert("visual_color_hist".into(), hist.to_compact());
        // Convert to JSON values for storage.
        let extra_red_json: std::collections::BTreeMap<String, serde_json::Value> = extra_red
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        assets::update_extra(conn, red_id, &extra_red_json).unwrap();

        // Blue (#0000ff): r=0>>4=0, g=0>>4=0, b=255>>4=15 → bucket ((0<<8)|(0<<4)|15) = 0x00F
        let mut extra_blue: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut buckets = [0.0_f32; 4096];
        buckets[0x00F] = 1.0; // blue bucket (#0000ff)
        let hist = ColorHistogram {
            buckets,
            total: 1.0,
        };
        extra_blue.insert("visual_phash".into(), "0000000000000002".into());
        extra_blue.insert("visual_color_hist".into(), hist.to_compact());
        let extra_blue_json: std::collections::BTreeMap<String, serde_json::Value> = extra_blue
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        assets::update_extra(conn, blue_id, &extra_blue_json).unwrap();

        // Search for red.
        let results = search_by_color(conn, "#ff0000", None).unwrap();
        assert!(!results.is_empty(), "expected red results");
        // The red asset should rank higher than the blue one.
        assert_eq!(results[0].asset.id, red_id, "red should rank first");
    }
}
