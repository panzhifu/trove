//! Browsed-view dispatch: one description of "what the workspace is showing"
//! compiled to the matching paged query.
//!
//! The workspace panel can browse five mutually exclusive views — recently
//! viewed, full-text search, a smart collection, the trash, and the plain
//! (collection / tag / folder filtered) listing. Which view wins and how the
//! grid filters compose with it is a domain rule, so it lives here next to
//! the queries; the UI only builds the [`BrowseContext`] and renders the
//! [`Page`] that comes back.

use rusqlite::Connection;
use uuid::Uuid;

use super::{assets, smart, smart_collections, view_history};
use crate::error::{Error, Result};
use crate::model::{AspectPreset, Asset, AssetKind, AssetQuery, AssetSort, Orientation, Page};
use crate::search::vector::{self, QueryVector, VECTOR_CANDIDATE_CAP, VectorIndex};

/// How the workspace grid is currently browsing the library.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrowseContext {
    /// The browsed collection (`None` = all assets).
    pub collection: Option<Uuid>,
    /// Browse the trash instead of a collection.
    pub in_trash: bool,
    /// Browse the recently-viewed history.
    pub in_recent: bool,
    /// Browse the live results of this smart collection.
    pub smart: Option<Uuid>,
    /// Only assets carrying this tag.
    pub tag: Option<Uuid>,
    /// Only assets imported from this source-path prefix.
    pub folder: Option<String>,
    /// Active full-text search term. Overrides the other views when set.
    pub search: String,
    /// The search term's embedding when the caller has one — the vector leg
    /// of the hybrid ranking, scored against the index passed to
    /// [`Self::run`].
    ///
    /// Hybrid ranking is opt-in *by data*, never by a flag: with no vector
    /// here, no index, a vector left over from a different term, or a
    /// model/space that disagrees with the index, the text ranking stands
    /// alone — exactly the pre-hybrid behaviour.
    pub vector: Option<QueryVector>,
    /// Grid filters (compose with every view except the trash, which hides
    /// the filter controls and ignores them entirely).
    pub kind: Option<AssetKind>,
    pub is_favorite: bool,
    pub orientation: Option<Orientation>,
    /// Media aspect-ratio preset the dimensions must fall into. Composes
    /// with `orientation` (a 2.35:1 cover is also a landscape).
    pub aspect: Option<AspectPreset>,
    /// Minimum star rating (unrated assets match nothing).
    pub min_rating: Option<u8>,
    pub ext: Option<String>,
    /// Listing sort (ignored by the live search, which sorts by
    /// relevance).
    pub sort: AssetSort,
    pub sort_desc: bool,
}

impl BrowseContext {
    /// Run the paged query for this view. `limit` caps the page (the grid's
    /// pagination cursor); the recent view treats it as an id cap too.
    ///
    /// `vector` is the in-memory index holding the stored embeddings — the
    /// second leg of a hybrid search. It is only consulted when
    /// [`Self::vector`] carries a vector for the *current* term; see that
    /// field for what "no hybrid" means.
    pub fn run(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        limit: Option<u32>,
        vector: Option<&VectorIndex>,
    ) -> Result<Page<Asset>> {
        self.run_counted(conn, text, limit, vector, true)
    }

    /// Like [`run`](Self::run), but skips the exact COUNT where the view
    /// supports it: the returned total is a lower bound. Rapid refreshes
    /// use this and overlay a cached exact total (see the workspace's data
    /// pass); the recent view has no COUNT to skip and the search view
    /// always counts (both are cheap or user-initiated).
    pub fn run_without_count(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        limit: Option<u32>,
        vector: Option<&VectorIndex>,
    ) -> Result<Page<Asset>> {
        self.run_counted(conn, text, limit, vector, false)
    }

    fn run_counted(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        limit: Option<u32>,
        vector: Option<&VectorIndex>,
        count: bool,
    ) -> Result<Page<Asset>> {
        // Every paged browse feeds the query metrics; past the slow threshold
        // the note itself logs the warn.
        let started = std::time::Instant::now();
        let result = self.run_counted_inner(conn, text, limit, vector, count);
        crate::metrics::note_query(started.elapsed());
        result
    }

    /// The vector leg of a hybrid search, fused with the text ranking.
    ///
    /// `None` means "there is nothing to fuse", and the caller then uses the
    /// text ranking as it stands. Every reason to decline is a data mismatch
    /// rather than an error: no query vector, no index, a vector computed for
    /// a term the user has already typed past, or a model/space that
    /// disagrees with the index — the same comparability contract the store
    /// enforces on the write side.
    fn fused_candidates(
        &self,
        conn: &Connection,
        index: Option<&VectorIndex>,
        text_ranked: &[Uuid],
    ) -> Result<Option<Vec<Uuid>>> {
        let (Some(query), Some(index)) = (self.vector.as_ref(), index) else {
            return Ok(None);
        };
        if query.text != self.search.trim() || query.vector.is_empty() {
            return Ok(None);
        }
        if index.model() != query.model || index.space() != query.space {
            return Ok(None);
        }
        // A failure here must not sink the search: the text ranking is
        // already a complete answer, so a broken vector leg degrades to it.
        let hits = match index.search(conn, &query.vector, VECTOR_CANDIDATE_CAP) {
            Ok(hits) => hits,
            Err(error) => {
                tracing::warn!(%error, "vector leg of a hybrid search failed; text ranking stands");
                return Ok(None);
            }
        };
        if hits.is_empty() {
            return Ok(None);
        }
        crate::metrics::note_vector_search();
        let ranked: Vec<Uuid> = hits.into_iter().map(|m| m.asset_id).collect();
        Ok(Some(vector::reciprocal_rank_fusion(text_ranked, &ranked)))
    }

    fn run_counted_inner(
        &self,
        conn: &Connection,
        text: &crate::search::TextIndex,
        limit: Option<u32>,
        vector: Option<&VectorIndex>,
        count: bool,
    ) -> Result<Page<Asset>> {
        let search_active = !self.in_trash && !self.in_recent && !self.search.trim().is_empty();

        if self.in_recent {
            // Recently viewed: ids ordered by last view time, materialized in
            // that order (missing / trashed ids drop out of the query).
            // History is capped, so one page covers it all.
            let ids = view_history::recent_ids(conn, limit.map(|l| l as usize).unwrap_or(200))?;
            let total = ids.len() as u64;
            let items = assets::by_ids(conn, &ids)?;
            Ok(Page::new(total, items))
        } else if search_active {
            // Ranked candidates come from the Tantivy index (words, typo
            // tolerance, gram substrings, pinyin); the compound grid filters
            // stay in SQL and narrow the ranked set, preserving rank order.
            let text_ranked = text.search(&self.search, crate::search::CANDIDATE_CAP)?;
            // A configured embedding endpoint turns the same term into a
            // second ranking, and the two are fused rather than one replacing
            // the other: an asset both legs like outranks either leg's
            // favourite. Nothing to fuse with ⇒ the text ranking stands.
            let candidates = match self.fused_candidates(conn, vector, &text_ranked)? {
                Some(fused) => fused,
                None => text_ranked,
            };
            let q = AssetQuery {
                collection_id: self.collection,
                tag_ids: self.tag.map(|t| vec![t]).unwrap_or_default(),
                kind: self.kind,
                is_favorite: self.is_favorite.then_some(true),
                orientation: self.orientation,
                aspect: self.aspect,
                min_rating: self.min_rating,
                ext: self.ext.clone(),
                source_path_prefix: self.folder.clone(),
                is_trashed: false,
                limit,
                ..Default::default()
            };
            let (total, ids) = assets::rank_intersect(conn, &candidates, &q)?;
            let page = assets::page_assets(&ids, &q, conn)?;
            Ok(Page::new(total, page))
        } else if let Some(sid) = self.smart {
            let Some(sc) = smart_collections::get(conn, sid)? else {
                return Err(Error::NotFound("smart_collection"));
            };
            let node = smart::node_from_json(&sc.query)?;
            let page = smart::SmartPage {
                kind: self.kind,
                favorite: self.is_favorite.then_some(true),
                limit,
                offset: 0,
            };
            let ids = if count {
                smart::evaluate_filtered(conn, Some(text), &node, page)?
            } else {
                smart::evaluate_filtered_without_count(conn, Some(text), &node, page)?
            };
            let items = assets::by_ids(conn, &ids.items)?;
            // `evaluate_filtered` carries only kind/favorite; the remaining
            // grid filters apply in-memory (the smart result set is a page).
            let items: Vec<Asset> = items
                .into_iter()
                .filter(|a| {
                    self.orientation
                        .is_none_or(|o| orientation_of(a) == Some(o))
                        && self.aspect.is_none_or(|p| aspect_matches(a, p))
                        && self
                            .min_rating
                            .is_none_or(|r| a.rating.is_some_and(|v| v >= r))
                        && self
                            .ext
                            .as_ref()
                            .is_none_or(|e| a.ext.eq_ignore_ascii_case(e))
                })
                .collect();
            Ok(Page::new(ids.total, items))
        } else {
            let q = AssetQuery {
                collection_id: if self.in_trash { None } else { self.collection },
                tag_ids: if self.in_trash {
                    Vec::new()
                } else {
                    self.tag.map(|t| vec![t]).unwrap_or_default()
                },
                kind: if self.in_trash { None } else { self.kind },
                is_favorite: (!self.in_trash && self.is_favorite).then_some(true),
                orientation: if self.in_trash {
                    None
                } else {
                    self.orientation
                },
                aspect: if self.in_trash { None } else { self.aspect },
                min_rating: if self.in_trash { None } else { self.min_rating },
                ext: if self.in_trash {
                    None
                } else {
                    self.ext.clone()
                },
                source_path_prefix: if self.in_trash {
                    None
                } else {
                    self.folder.clone()
                },
                is_trashed: self.in_trash,
                sort: self.sort,
                sort_desc: self.sort_desc,
                limit,
                ..Default::default()
            };
            if count {
                assets::query(conn, &q)
            } else {
                assets::query_without_count(conn, &q)
            }
        }
    }
}

/// Orientation of one asset row, mirroring the SQL CASE in
/// `assets::build_where`.
fn orientation_of(a: &Asset) -> Option<Orientation> {
    match (a.width, a.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            if w > h {
                Some(Orientation::Landscape)
            } else if w < h {
                Some(Orientation::Portrait)
            } else {
                Some(Orientation::Square)
            }
        }
        _ => None,
    }
}

/// Aspect-preset test for one asset row, mirroring the SQL ratio-band CASE
/// in `assets::build_where`: rows without usable dimensions match nothing.
fn aspect_matches(a: &Asset, preset: AspectPreset) -> bool {
    match (a.width, a.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let ratio = w as f32 / h as f32;
            let (lo, hi) = preset.ratio_range();
            ratio >= lo && ratio <= hi
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_asset;
    use crate::store::Store;

    #[test]
    fn hybrid_search_fuses_a_vector_leg_into_the_text_ranking() {
        use crate::model::{EmbeddingSpace, NewEmbedding};
        use crate::store::embeddings;

        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut shot = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        shot.title = Some("sunset shot".into());
        assets::insert(conn, &shot).unwrap();
        let mut beach = test_asset("b.png", AssetKind::Image, Uuid::new_v4());
        beach.title = Some("beach walk".into());
        assets::insert(conn, &beach).unwrap();

        let idx = crate::search::TextIndex::in_ram().unwrap();
        idx.index_asset(conn, shot.id).unwrap();
        idx.index_asset(conn, beach.id).unwrap();
        idx.commit().unwrap();

        // Only `beach` carries a vector, and it points straight at the query
        // — a hit the text leg cannot see on its own.
        embeddings::upsert(
            conn,
            &NewEmbedding {
                asset_id: beach.id,
                model: "test-model".into(),
                space: EmbeddingSpace::Text,
                vector: vec![1.0, 0.0],
                source_hash: "h".into(),
            },
        )
        .unwrap();
        let index = VectorIndex::new("test-model", EmbeddingSpace::Text);
        let query = |text: &str, model: &str| QueryVector {
            text: text.into(),
            model: model.into(),
            space: EmbeddingSpace::Text,
            vector: vec![1.0, 0.0],
        };
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext {
                search: "sunset".into(),
                ..Default::default()
            };
            mutate(&mut c);
            c
        };

        // Text alone: only the asset whose title matches.
        let page = ctx(&|_| {}).run(conn, &idx, None, Some(&index)).unwrap();
        assert_eq!(page.total, 1, "no query vector ⇒ the text ranking stands");
        assert_eq!(page.items[0].id, shot.id);

        // With the term's vector, the second leg pulls its own hit in.
        let page = ctx(&|c: &mut BrowseContext| {
            c.vector = Some(query("sunset", "test-model"));
        })
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 2, "the vector leg contributes its own hit");
        let ids: Vec<Uuid> = page.items.iter().map(|a| a.id).collect();
        assert!(ids.contains(&shot.id) && ids.contains(&beach.id), "{ids:?}");

        // A vector for a term the user has typed past is ignored …
        let page = ctx(&|c: &mut BrowseContext| {
            c.vector = Some(query("bicycle", "test-model"));
        })
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 1, "a stale query vector must not fuse");

        // … and so is one from another model (the comparability contract).
        let page = ctx(&|c: &mut BrowseContext| {
            c.vector = Some(query("sunset", "other-model"));
        })
        .run(conn, &idx, None, Some(&index))
        .unwrap();
        assert_eq!(page.total, 1, "a model mismatch must not fuse");
    }

    #[test]
    fn dispatches_plain_search_smart_trash_and_recent() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut img = test_asset("a.png", AssetKind::Image, Uuid::new_v4());
        img.title = Some("sunset shot".into());
        assets::insert(conn, &img).unwrap();
        let doc = test_asset("b.txt", AssetKind::Document, Uuid::new_v4());
        assets::insert(conn, &doc).unwrap();

        let idx = crate::search::TextIndex::in_ram().unwrap();
        idx.index_asset(conn, img.id).unwrap();
        idx.commit().unwrap();
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext::default();
            mutate(&mut c);
            c
        };

        // Plain: everything, newest first.
        let page = ctx(&|_| {}).run(conn, &idx, None, None).unwrap();
        assert_eq!(page.total, 2);

        // Search overrides the plain view.
        let page = ctx(&|c: &mut BrowseContext| c.search = "sunset".into())
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // Smart collection wins over the search text.
        let sc = smart_collections::create(
            conn,
            &crate::model::NewSmartCollection {
                parent_id: None,
                name: "docs".into(),
                query: serde_json::json!({
                    "op": "match", "field": "kind", "value": "document"
                }),
                color: None,
                position: 0,
            },
        )
        .unwrap();
        // The live search overrides the smart collection (the controller
        // clears the smart selection when a search starts, so this pairing
        // resolves to the search view — matching the UI contract).
        let page = ctx(&|c: &mut BrowseContext| {
            c.search = "sunset".into();
            c.smart = Some(sc.id);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // With no search text, the smart collection drives the view.
        let page = ctx(&|c: &mut BrowseContext| c.smart = Some(sc.id))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, doc.id);

        // Trash ignores the grid filters entirely.
        assets::set_trashed(conn, img.id, true).unwrap();
        let page = ctx(&|c: &mut BrowseContext| {
            c.in_trash = true;
            c.is_favorite = true;
            c.kind = Some(AssetKind::Font);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        eprintln!(
            "trash page: total={} items={:?}",
            page.total,
            page.items
                .iter()
                .map(|a| (a.file_name.as_str(), a.trashed_at.is_some()))
                .collect::<Vec<_>>()
        );
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, img.id);

        // Orientation / min-rating / extension filters (plain view).
        let mut square = test_asset("c.png", AssetKind::Image, Uuid::new_v4());
        (square.width, square.height) = (Some(64), Some(64));
        assets::insert(conn, &square).unwrap();
        let mut rated = test_asset("d.png", AssetKind::Image, Uuid::new_v4());
        rated.rating = Some(4);
        rated.ext = "jpg".into();
        assets::insert(conn, &rated).unwrap();

        let page = ctx(&|c: &mut BrowseContext| c.orientation = Some(Orientation::Square))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![square.id]
        );

        let page = ctx(&|c: &mut BrowseContext| c.min_rating = Some(4))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![rated.id]
        );

        let page = ctx(&|c: &mut BrowseContext| c.ext = Some("JPG".into()))
            .run(conn, &idx, None, None)
            .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![rated.id]
        );

        // Recent view orders by last view (the timestamps are RFC 3339
        // strings; separate the records so their order is unambiguous).
        view_history::record(conn, doc.id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        view_history::record(conn, img.id).unwrap();
        let page = ctx(&|c: &mut BrowseContext| c.in_recent = true)
            .run(conn, &idx, None, None)
            .unwrap();
        // img was trashed above: readers hide trashed rows, so only doc shows.
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![doc.id]
        );
    }

    #[test]
    fn aspect_preset_filters_plain_and_smart_views() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let idx = crate::search::TextIndex::in_ram().unwrap();
        let ctx = |mutate: &dyn Fn(&mut BrowseContext)| {
            let mut c = BrowseContext::default();
            mutate(&mut c);
            c
        };
        // One asset per preset band, plus a no-dimensions row that must
        // match nothing.
        let sizes: &[(&str, Option<(u32, u32)>)] = &[
            ("cover.png", Some((900, 383))),      // 2.3499… → WechatCover
            ("wide.png", Some((1920, 1080))),     // 1.777…  → VideoWide
            ("vertical.png", Some((1080, 1920))), // 0.5625 → VideoVertical
            ("photo.png", Some((640, 480))),      // 1.333…  → PhotoLandscape
            ("portrait.png", Some((480, 640))),   // 0.75    → PhotoPortrait
            ("square.png", Some((64, 64))),       // 1.0     → Square
            ("nodims.png", None),
        ];
        let mut ids = std::collections::HashMap::new();
        for (name, dims) in sizes {
            let mut a = test_asset(name, AssetKind::Image, Uuid::new_v4());
            (a.width, a.height) = dims
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None));
            assets::insert(conn, &a).unwrap();
            ids.insert(name.to_string(), a.id);
            idx.index_asset(conn, a.id).unwrap();
        }
        idx.commit().unwrap();

        let expected =
            |names: &[&str]| -> Vec<Uuid> { names.iter().map(|n| ids[*n]).collect::<Vec<_>>() };
        for (preset, names) in [
            (AspectPreset::WechatCover, vec!["cover.png"]),
            (AspectPreset::VideoWide, vec!["wide.png"]),
            (AspectPreset::VideoVertical, vec!["vertical.png"]),
            (AspectPreset::PhotoLandscape, vec!["photo.png"]),
            (AspectPreset::PhotoPortrait, vec!["portrait.png"]),
            (AspectPreset::Square, vec!["square.png"]),
        ] {
            let want = expected(&names);
            // Plain view: the SQL ratio-band CASE.
            let page = ctx(&|c: &mut BrowseContext| c.aspect = Some(preset))
                .run(conn, &idx, None, None)
                .unwrap();
            assert_eq!(
                page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
                want,
                "{preset:?} via SQL"
            );
            // Smart view: the in-memory mirror.
            let sc = smart_collections::create(
                conn,
                &crate::model::NewSmartCollection {
                    parent_id: None,
                    name: format!("smart-{preset:?}"),
                    query: serde_json::json!({
                        "op": "match", "field": "kind", "value": "image"
                    }),
                    color: None,
                    position: 0,
                },
            )
            .unwrap();
            let page = ctx(&|c: &mut BrowseContext| {
                c.smart = Some(sc.id);
                c.aspect = Some(preset);
            })
            .run(conn, &idx, None, None)
            .unwrap();
            assert_eq!(
                page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
                want,
                "{preset:?} in memory"
            );
        }

        // The presets compose with the orientation filter.
        let page = ctx(&|c: &mut BrowseContext| {
            c.aspect = Some(AspectPreset::WechatCover);
            c.orientation = Some(Orientation::Landscape);
        })
        .run(conn, &idx, None, None)
        .unwrap();
        assert_eq!(
            page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
            expected(&["cover.png"])
        );
    }
}
