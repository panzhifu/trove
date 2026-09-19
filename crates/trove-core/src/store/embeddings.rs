//! Persistence for AI embeddings: the `asset_embeddings` table.
//!
//! A vector is stored L2-normalized as little-endian f32 in a BLOB, so the
//! search side is a plain dot product — see [`crate::search`]'s vector index.
//! The table is keyed `(asset_id, model, space)`; deleting an asset deletes
//! its vectors with it (CASCADE), and `source_hash` lets a backfill skip
//! rows whose input has not changed.

use super::assets;
use super::rows;
use crate::error::{Error, Result};
use crate::model::{Asset, EmbeddingSpace, NewEmbedding, normalized};
use std::collections::HashMap;
use uuid::Uuid;

/// Bind a `&str` as a TEXT parameter (`Value` implements `From<String>`,
/// not `From<&str>`).
fn text(s: &str) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(s.to_string())
}

/// Store (or replace) one embedding. The vector is validated and
/// L2-normalized on the way in, so what sits in the BLOB is always a unit
/// vector regardless of what the provider returned.
pub fn upsert(conn: &rusqlite::Connection, embedding: &NewEmbedding) -> Result<()> {
    embedding.validate()?;
    let vector = normalized(&embedding.vector)?;

    rows::execute(
        conn,
        "INSERT INTO asset_embeddings
             (asset_id, model, space, dim, source_hash, vector, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(asset_id, model, space) DO UPDATE SET
             dim = excluded.dim,
             source_hash = excluded.source_hash,
             vector = excluded.vector,
             updated_at = excluded.updated_at",
        vec![
            rows::uuid(embedding.asset_id).into(),
            text(&embedding.model),
            text(embedding.space.as_str()),
            (embedding.vector.len() as i64).into(),
            text(&embedding.source_hash),
            rusqlite::types::Value::Blob(pack(&vector)),
            rows::ts(crate::model::now()).into(),
        ],
    )?;
    Ok(())
}

/// Load one stored vector, already decoded back to f32 (still normalized).
pub fn get(
    conn: &rusqlite::Connection,
    asset_id: Uuid,
    model: &str,
    space: EmbeddingSpace,
) -> Result<Option<Vec<f32>>> {
    rows::query_one(
        conn,
        "SELECT vector FROM asset_embeddings
         WHERE asset_id = ?1 AND model = ?2 AND space = ?3",
        vec![
            rows::uuid(asset_id).into(),
            text(model),
            text(space.as_str()),
        ],
        |row| Ok(unpack(&blob(row, 0)?)),
    )
}

/// Drop one asset's vector for a model+space. Returns rows removed.
pub fn delete(
    conn: &rusqlite::Connection,
    asset_id: Uuid,
    model: &str,
    space: EmbeddingSpace,
) -> Result<u64> {
    rows::execute(
        conn,
        "DELETE FROM asset_embeddings
         WHERE asset_id = ?1 AND model = ?2 AND space = ?3",
        vec![
            rows::uuid(asset_id).into(),
            text(model),
            text(space.as_str()),
        ],
    )
}

/// Drop every vector of one model, across both spaces — the "the provider
/// changed, start over" button. Returns rows removed.
pub fn delete_model(conn: &rusqlite::Connection, model: &str) -> Result<u64> {
    rows::execute(
        conn,
        "DELETE FROM asset_embeddings WHERE model = ?1",
        vec![text(model)],
    )
}

/// `(embedded_live, total_live)` — how many non-trashed assets carry a
/// vector of this model+space, out of all non-trashed assets. The settings
/// page renders it as a coverage fraction, like the visual-signature counts.
pub fn coverage(
    conn: &rusqlite::Connection,
    model: &str,
    space: EmbeddingSpace,
) -> Result<(u64, u64)> {
    let embedded = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM asset_embeddings e
         JOIN assets a ON a.id = e.asset_id
         WHERE e.model = ?1 AND e.space = ?2 AND a.trashed_at IS NULL",
        vec![text(model), text(space.as_str())],
    )? as u64;
    let total = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL",
        vec![],
    )? as u64;
    Ok((embedded, total))
}

/// Every live asset paired with the `source_hash` currently stored for it
/// under `model`/`space` (`None` = not embedded yet). The backfill
/// recomputes each asset's fingerprint on its own thread and compares, so
/// the SQL side only has to hand out what is stored.
pub fn embeddable_assets(
    conn: &rusqlite::Connection,
    model: &str,
    space: EmbeddingSpace,
) -> Result<Vec<(Asset, Option<String>)>> {
    let stored: HashMap<Uuid, String> = rows::query_map(
        conn,
        "SELECT asset_id, source_hash FROM asset_embeddings
         WHERE model = ?1 AND space = ?2",
        vec![text(model), text(space.as_str())],
        |row| Ok((rows::req_uuid(row, 0)?, rows::req_str(row, 1)?)),
    )?
    .into_iter()
    .collect();

    let live = assets::query(
        conn,
        &crate::model::AssetQuery {
            is_trashed: false,
            ..Default::default()
        },
    )?;
    Ok(live
        .items
        .into_iter()
        .map(|asset| {
            let hash = stored.get(&asset.id).cloned();
            (asset, hash)
        })
        .collect())
}

/// Every stored vector of one model+space, decoded — the raw material the
/// in-memory search index loads from. Trashed assets keep their rows (trash
/// is reversible), so the snapshot may hold a few assets a search will then
/// filter out; deletion, which does cascade, drifts the
/// [`fingerprint`] and triggers a reload.
pub fn snapshot(
    conn: &rusqlite::Connection,
    model: &str,
    space: EmbeddingSpace,
) -> Result<Vec<(Uuid, Vec<f32>)>> {
    rows::query_map(
        conn,
        "SELECT asset_id, vector FROM asset_embeddings
         WHERE model = ?1 AND space = ?2",
        vec![text(model), text(space.as_str())],
        |row| Ok((rows::req_uuid(row, 0)?, unpack(&blob(row, 1)?))),
    )
}

/// `(row_count, last_updated_at)` for one model+space — the cheap staleness
/// probe the in-memory index runs before every search. Any change to the
/// table's contents (an upsert, a CASCADE delete) moves at least one half.
pub fn fingerprint(
    conn: &rusqlite::Connection,
    model: &str,
    space: EmbeddingSpace,
) -> Result<(u64, String)> {
    Ok(rows::query_one(
        conn,
        "SELECT COUNT(*), COALESCE(MAX(updated_at), '')
         FROM asset_embeddings WHERE model = ?1 AND space = ?2",
        vec![text(model), text(space.as_str())],
        |row| Ok((rows::int(row, 0)? as u64, rows::req_str(row, 1)?)),
    )?
    .unwrap_or((0, String::new())))
}

/// Pack f32s as little-endian bytes. Endianness is pinned to LE (not
/// native) so a library file copied across architectures reads back
/// identically.
fn pack(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Decode a packed BLOB back to f32s. A trailing partial float (a corrupted
/// or hand-edited BLOB) is dropped rather than guessed at; the search side
/// then filters the mismatched length out naturally.
fn unpack(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

fn blob(row: &rusqlite::Row, ix: usize) -> Result<Vec<u8>> {
    row.get::<_, Vec<u8>>(ix).map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_asset;
    use crate::store::Store;
    use crate::store::assets;

    fn emb(asset_id: Uuid, vector: Vec<f32>) -> NewEmbedding {
        NewEmbedding {
            asset_id,
            model: "test-model".into(),
            space: EmbeddingSpace::Text,
            vector,
            source_hash: "hash-1".into(),
        }
    }

    #[test]
    fn upsert_normalizes_and_get_roundtrips() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &asset).unwrap();

        upsert(conn, &emb(asset.id, vec![3.0, 4.0])).unwrap();
        let got = get(conn, asset.id, "test-model", EmbeddingSpace::Text)
            .unwrap()
            .expect("row exists");
        // [3, 4] normalized is [0.6, 0.8]; float storage rounds a hair.
        assert!(
            (got[0] - 0.6).abs() < 1e-6 && (got[1] - 0.8).abs() < 1e-6,
            "{got:?}"
        );

        // The provider's raw vector was not mutated by the write path.
        let e = emb(asset.id, vec![3.0, 4.0]);
        upsert(conn, &e).unwrap();
        assert_eq!(e.vector, vec![3.0, 4.0]);
    }

    #[test]
    fn upsert_replaces_the_same_key() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &asset).unwrap();

        upsert(conn, &emb(asset.id, vec![1.0, 0.0])).unwrap();
        upsert(
            conn,
            &NewEmbedding {
                vector: vec![0.0, 1.0],
                source_hash: "hash-2".into(),
                ..emb(asset.id, vec![0.0, 1.0])
            },
        )
        .unwrap();
        let got = get(conn, asset.id, "test-model", EmbeddingSpace::Text)
            .unwrap()
            .unwrap();
        assert!(got[1] > 0.99, "second write won: {got:?}");
        let (count, _) = fingerprint(conn, "test-model", EmbeddingSpace::Text).unwrap();
        assert_eq!(count, 1, "one row, not two");
    }

    #[test]
    fn spaces_and_models_are_independent_keys() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &asset).unwrap();

        upsert(conn, &emb(asset.id, vec![1.0, 0.0])).unwrap();
        upsert(
            conn,
            &NewEmbedding {
                space: EmbeddingSpace::Image,
                ..emb(asset.id, vec![0.0, 1.0])
            },
        )
        .unwrap();
        upsert(
            conn,
            &NewEmbedding {
                model: "other-model".into(),
                ..emb(asset.id, vec![0.5, 0.5])
            },
        )
        .unwrap();

        assert_eq!(
            fingerprint(conn, "test-model", EmbeddingSpace::Text)
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            fingerprint(conn, "test-model", EmbeddingSpace::Image)
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            fingerprint(conn, "other-model", EmbeddingSpace::Text)
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            fingerprint(conn, "missing-model", EmbeddingSpace::Text).unwrap(),
            (0, String::new())
        );

        // Deleting one model leaves the others.
        assert_eq!(delete_model(conn, "test-model").unwrap(), 2);
        assert!(
            get(conn, asset.id, "test-model", EmbeddingSpace::Text)
                .unwrap()
                .is_none()
        );
        assert!(
            get(conn, asset.id, "other-model", EmbeddingSpace::Text)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn deleting_the_asset_cascades() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &asset).unwrap();
        upsert(conn, &emb(asset.id, vec![1.0])).unwrap();

        assets::delete(conn, asset.id).unwrap();
        assert!(
            get(conn, asset.id, "test-model", EmbeddingSpace::Text)
                .unwrap()
                .is_none(),
            "the embedding row follows its asset out"
        );
    }

    #[test]
    fn coverage_counts_live_assets_only() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let a = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        let b = test_asset("b.png", crate::model::AssetKind::Image, Uuid::new_v4());
        let c = test_asset("c.png", crate::model::AssetKind::Image, Uuid::new_v4());
        for asset in [&a, &b, &c] {
            assets::insert(conn, asset).unwrap();
        }
        upsert(conn, &emb(a.id, vec![1.0])).unwrap();

        assert_eq!(
            coverage(conn, "test-model", EmbeddingSpace::Text).unwrap(),
            (1, 3)
        );

        // Trashing hides the asset from both sides of the fraction.
        assets::set_trashed(conn, a.id, true).unwrap();
        assert_eq!(
            coverage(conn, "test-model", EmbeddingSpace::Text).unwrap(),
            (0, 2)
        );
        assets::set_trashed(conn, b.id, true).unwrap();
        assert_eq!(
            coverage(conn, "test-model", EmbeddingSpace::Text).unwrap(),
            (0, 1)
        );
    }

    #[test]
    fn embeddable_assets_hands_out_stored_hashes() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let a = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        let b = test_asset("b.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &a).unwrap();
        assets::insert(conn, &b).unwrap();
        upsert(conn, &emb(a.id, vec![1.0])).unwrap();

        let rows = embeddable_assets(conn, "test-model", EmbeddingSpace::Text).unwrap();
        assert_eq!(rows.len(), 2, "every live asset is a backfill candidate");
        let a_row = rows.iter().find(|(asset, _)| asset.id == a.id).unwrap();
        assert_eq!(
            a_row.1.as_deref(),
            Some("hash-1"),
            "stored hash comes along"
        );
        let b_row = rows.iter().find(|(asset, _)| asset.id == b.id).unwrap();
        assert_eq!(b_row.1, None, "not yet embedded");

        // A trashed asset is not a candidate.
        assets::set_trashed(conn, b.id, true).unwrap();
        let rows = embeddable_assets(conn, "test-model", EmbeddingSpace::Text).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn fingerprint_tracks_changes() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &asset).unwrap();

        let before = fingerprint(conn, "test-model", EmbeddingSpace::Text).unwrap();
        assert_eq!(before, (0, String::new()));

        upsert(conn, &emb(asset.id, vec![1.0])).unwrap();
        let after = fingerprint(conn, "test-model", EmbeddingSpace::Text).unwrap();
        assert_eq!(after.0, 1);
        assert!(!after.1.is_empty(), "updated_at is set");

        // A delete drifts the fingerprint too (CASCADE, not an explicit delete).
        assets::delete(conn, asset.id).unwrap();
        let gone = fingerprint(conn, "test-model", EmbeddingSpace::Text).unwrap();
        assert_ne!(gone, after);
    }

    #[test]
    fn invalid_vectors_are_refused() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let asset = test_asset("a.png", crate::model::AssetKind::Image, Uuid::new_v4());
        assets::insert(conn, &asset).unwrap();

        assert!(upsert(conn, &emb(asset.id, vec![0.0, 0.0])).is_err());
        assert!(upsert(conn, &emb(asset.id, vec![f32::NAN])).is_err());
        assert!(upsert(conn, &emb(asset.id, Vec::new())).is_err());
        assert!(
            get(conn, asset.id, "test-model", EmbeddingSpace::Text)
                .unwrap()
                .is_none(),
            "nothing was written by the failed attempts"
        );
    }

    #[test]
    fn pack_roundtrips_through_the_blob() {
        let vector = vec![0.25_f32, -1.5, 3.0e10, f32::MIN_POSITIVE];
        assert_eq!(unpack(&pack(&vector)), vector);
        // A trailing byte is dropped, not hallucinated into a float.
        let mut bytes = pack(&vector);
        bytes.push(0xAB);
        assert_eq!(unpack(&bytes), vector);
    }
}
