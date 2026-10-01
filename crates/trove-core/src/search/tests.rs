use super::TextIndex;
use super::facts::FactTexts;
use super::{CANDIDATE_CAP, MAX_RANKED_POOL, expression};

/// A throwaway index directory, named so parallel tests never collide.
fn temp_index_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "trove-index-test-{}",
        crate::model::new_id().simple()
    ))
}

/// How much an uncapped ranked gather costs. Not a pass/fail test — it is
/// the measurement behind `pool_for`'s shape, kept so the next round can
/// re-take it instead of re-guessing it:
/// `cargo test -p trove-core bench_uncapped_gather -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_uncapped_gather() {
    use std::time::Instant;
    let idx = TextIndex::in_ram().unwrap();
    let writer = idx.writer().unwrap();
    for i in 0..100_000usize {
        idx.index_asset_text(
            &writer,
            &uuid::Uuid::new_v4().to_string(),
            &format!("photo-{i:06}-cat.jpg"),
            None,
            None,
            "",
            &FactTexts::default(),
            "",
        );
    }
    drop(writer);
    idx.commit().unwrap();
    println!("docs {}", idx.num_docs());
    for cap in [2_000usize, 20_000, 100_000] {
        let t = Instant::now();
        let hits = idx.search("cat", cap).unwrap();
        println!("cap {cap:6} -> {} hits in {:?}", hits.len(), t.elapsed());
    }
}

/// The ceiling decision, which is the only place `truncated` is decided for
/// a filtered search.
#[test]
fn a_filtered_gather_is_capped_only_above_the_ceiling() {
    // Below the ceiling the pool is the whole index, so a saturated length
    // is not a hidden match — it means the term matched everything.
    assert_eq!(TextIndex::gather_cap(0), (CANDIDATE_CAP, false));
    assert_eq!(TextIndex::gather_cap(12), (CANDIDATE_CAP, false));
    assert_eq!(TextIndex::gather_cap(50_000), (50_000, false));
    assert_eq!(
        TextIndex::gather_cap(MAX_RANKED_POOL as u64),
        (MAX_RANKED_POOL, false)
    );
    // Past it, the gather is narrower than what exists and says so.
    assert_eq!(
        TextIndex::gather_cap(MAX_RANKED_POOL as u64 + 1),
        (MAX_RANKED_POOL, true)
    );
}

/// The guarantee a filtered gather exists to keep: every asset that matches
/// the term *and* the filter is in the answer, including one the ranking
/// placed past the fast path's width.
///
/// The weak match is the point — 2004 documents repeat the word and one
/// mentions it once, so the favourite ranks last and a pool that stopped at
/// [`CANDIDATE_CAP`] would return a confident zero for `fav:yes`.
#[test]
fn a_filtered_search_finds_the_match_that_ranked_past_the_fast_path() {
    use crate::model::{AssetKind, AssetQuery};
    use crate::store::assets;

    let store = crate::store::Store::in_memory().unwrap();
    let conn = store.conn();
    let idx = TextIndex::in_ram().unwrap();
    let writer = idx.writer().unwrap();

    let mut strong = Vec::new();
    for i in 0..2004 {
        let mut a = crate::model::test_asset(
            &format!("kittens-kittens-{i:04}-kittens.png"),
            AssetKind::Image,
            crate::model::new_id(),
        );
        a.description = Some("kittens kittens".into());
        assets::insert(conn, &a).unwrap();
        idx.index_asset_text(
            &writer,
            &a.id.to_string(),
            &a.file_name,
            None,
            a.description.as_deref(),
            "",
            &FactTexts::default(),
            "",
        );
        strong.push(a.id);
    }
    // The one asset that both matches the term (weakly, so it ranks last)
    // and carries the filter.
    let mut last =
        crate::model::test_asset("photo-last.png", AssetKind::Image, crate::model::new_id());
    last.description = Some("kittens".into());
    last.is_favorite = true;
    assets::insert(conn, &last).unwrap();
    idx.index_asset_text(
        &writer,
        &last.id.to_string(),
        &last.file_name,
        None,
        last.description.as_deref(),
        "",
        &FactTexts::default(),
        "",
    );
    drop(writer);
    idx.commit().unwrap();
    assert!(!strong.is_empty());

    let expr = expression::parse("kittens").into_expression();
    let (pool, ran_out) = idx.pool_for("kittens", &expr, None, true).unwrap();
    assert_eq!(
        pool.len(),
        2005,
        "a filtered gather takes the term's whole result set, not a fixed width"
    );
    assert!(
        !ran_out,
        "nothing was left behind, so the total is a count and not a floor"
    );
    assert_eq!(
        pool.last(),
        Some(&last.id),
        "the setup lost: the weak match no longer ranks last, so this test proves nothing"
    );

    let q = AssetQuery {
        is_favorite: Some(true),
        ..AssetQuery::live()
    };
    let (total, kept) = assets::rank_intersect(conn, &pool, &q).unwrap();
    assert_eq!(kept, vec![last.id]);
    assert_eq!(total, 1, "the asset past the fast path was not found");
}

/// The in-RAM index accepts documents and serves them after commit.
#[test]
fn in_ram_roundtrip() {
    let idx = TextIndex::in_ram().unwrap();
    idx.index_asset_text(
        &idx.writer().unwrap(),
        "11111111-1111-1111-1111-111111111111",
        "flower.png",
        None,
        None,
        "",
        &FactTexts::default(),
        "",
    );
    idx.commit().unwrap();
    assert_eq!(idx.num_docs(), 1);
    assert!(!idx.search("flower", 10).unwrap().is_empty());
}

/// One document to index: `(asset_id, file_name, title, description, tags)`.
type Case = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
);

/// A small library: one asset per surface that carries a word nothing else
/// in the set carries, so a field qualifier's answer is unambiguous.
fn sample_index() -> TextIndex {
    let idx = TextIndex::in_ram().unwrap();
    let cases: [Case; 4] = [
        (
            "aaaa0000-0000-0000-0000-000000000001",
            "zong.png",
            None,
            None,
            "",
        ),
        (
            "aaaa0000-0000-0000-0000-000000000002",
            "b.png",
            Some("zong"),
            None,
            "",
        ),
        (
            "aaaa0000-0000-0000-0000-000000000003",
            "c.png",
            None,
            Some("zong"),
            "",
        ),
        (
            "aaaa0000-0000-0000-0000-000000000004",
            "d.png",
            None,
            None,
            "zong",
        ),
    ];
    {
        let writer = idx.writer().unwrap();
        for (id, name, title, desc, tags) in cases {
            idx.index_asset_text(
                &writer,
                id,
                name,
                title,
                desc,
                tags,
                &FactTexts::default(),
                "",
            );
        }
        // The writer must be released before the reader may see the commit.
        drop(writer);
    }
    idx.commit().unwrap();
    idx
}

fn ids_of(idx: &TextIndex, query: &str) -> Vec<String> {
    idx.search(query, 50)
        .unwrap()
        .into_iter()
        .map(|u| u.to_string())
        .collect()
}

fn ids_of_expr(idx: &TextIndex, query: &str) -> Vec<String> {
    let expr = crate::search::expression::parse(query).into_expression();
    idx.search_expression(&expr, 50)
        .unwrap()
        .into_iter()
        .map(|u| u.to_string())
        .collect()
}

/// The whole point of `is_plain`: a query with no new syntax in it must
/// rank exactly the way the splitter that predates this module did.
#[test]
fn a_plain_expression_answers_exactly_what_the_old_path_did() {
    let idx = sample_index();
    for query in ["zong", "b", "zong b", "png"] {
        assert_eq!(
            ids_of_expr(&idx, query),
            ids_of(&idx, query),
            "{query} diverged from the pre-expression path"
        );
    }
}

#[test]
fn a_field_qualifier_reaches_one_surface_only() {
    let idx = sample_index();
    // Unqualified, `zong` is found wherever it lives.
    assert_eq!(ids_of(&idx, "zong").len(), 4);
    // Qualified, it answers with the one asset that carries it there.
    assert_eq!(
        ids_of_expr(&idx, "name:zong"),
        vec!["aaaa0000-0000-0000-0000-000000000001"]
    );
    assert_eq!(
        ids_of_expr(&idx, "title:zong"),
        vec!["aaaa0000-0000-0000-0000-000000000002"]
    );
    assert_eq!(
        ids_of_expr(&idx, "tag:zong"),
        vec!["aaaa0000-0000-0000-0000-000000000004"]
    );
}

#[test]
fn a_bar_unions_two_groups_and_a_dash_subtracts() {
    let idx = sample_index();
    let union = ids_of_expr(&idx, "name:zong | tag:zong");
    assert_eq!(union.len(), 2, "{union:?}");
    assert!(union.contains(&"aaaa0000-0000-0000-0000-000000000001".to_string()));
    assert!(union.contains(&"aaaa0000-0000-0000-0000-000000000004".to_string()));

    // `png` is on all four; excluding the first-name asset leaves three.
    let minus = ids_of_expr(&idx, "png -name:zong");
    assert_eq!(minus.len(), 3, "{minus:?}");
    assert!(!minus.contains(&"aaaa0000-0000-0000-0000-000000000001".to_string()));
}

/// A leading dash is a typo's worth of distance from "show me everything",
/// so an exclusion with nothing to exclude from answers with nothing. The
/// box has never widened a match on syntax characters, and this is where
/// that property could have been lost.
#[test]
fn an_exclusion_without_something_positive_matches_nothing() {
    let idx = sample_index();
    assert_eq!(ids_of(&idx, "png").len(), 4);
    assert!(ids_of_expr(&idx, "-name:zong").is_empty());
    // With a positive term to anchor it, the same exclusion subtracts.
    let minus = ids_of_expr(&idx, "png -name:zong");
    assert_eq!(minus.len(), 3, "{minus:?}");
    assert!(!minus.contains(&"aaaa0000-0000-0000-0000-000000000001".to_string()));
}

/// The same shape on the AI-planned path, which reaches `BooleanQuery`
/// through a different door than the hand-typed one above.
///
/// The planner accepts a plan carrying one exclusion and no keywords — it
/// only rejects a plan where keywords, synonyms, exclusions, filters *and*
/// sort are all empty. Tantivy 0.26 already answers nothing for an
/// all-`MustNot` query (verified by disabling the guard and re-running this
/// assertion: it passes either way), so this pins the *decision*, not a live
/// wrong answer — it is what keeps the two paths agreeing if that engine
/// behaviour ever changes.
#[test]
fn a_plan_with_nothing_positive_matches_nothing() {
    use crate::ai::search_planner::AiSearchPlan;
    let idx = sample_index();
    assert_eq!(
        idx.search_plan(&AiSearchPlan::default(), 100)
            .unwrap()
            .len(),
        0
    );

    // A term that matches nothing, so an accidental "match all minus X"
    // anywhere in this path would show up as four rows rather than zero.
    let exclusions_only = AiSearchPlan {
        exclusions: vec!["nothing-carries-this".into()],
        ..Default::default()
    };
    assert_eq!(
        ids_of(&idx, "png").len(),
        4,
        "the sample library has four rows to accidentally return"
    );
    assert!(
        idx.search_plan(&exclusions_only, 100).unwrap().is_empty(),
        "an exclusions-only plan returned the whole library"
    );

    // With something positive to subtract from, the exclusion still applies.
    let positive = AiSearchPlan {
        keywords: vec!["png".into()],
        ..Default::default()
    };
    assert_eq!(idx.search_plan(&positive, 100).unwrap().len(), 4);
    let subtract = AiSearchPlan {
        keywords: vec!["png".into()],
        exclusions: vec!["png".into()],
        ..Default::default()
    };
    assert!(idx.search_plan(&subtract, 100).unwrap().is_empty());
}

#[test]
fn a_quoted_phrase_finds_the_substring_it_describes() {
    let idx = TextIndex::in_ram().unwrap();
    {
        let writer = idx.writer().unwrap();
        idx.index_asset_text(
            &writer,
            "bbbb0000-0000-0000-0000-000000000001",
            "summer 2024 beach.png",
            None,
            None,
            "",
            &FactTexts::default(),
            "",
        );
        idx.index_asset_text(
            &writer,
            "bbbb0000-0000-0000-0000-000000000002",
            "summer beach.png",
            None,
            None,
            "",
            &FactTexts::default(),
            "",
        );
        drop(writer);
    }
    idx.commit().unwrap();

    // Quoted, the words stay one span, so the grams of the whole phrase
    // have to be present: only the first file carries "summer 2024".
    let hits = ids_of_expr(&idx, "\"summer 2024\"");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0], "bbbb0000-0000-0000-0000-000000000001");
}

/// A metadata qualifier answers on its own surface and nowhere else. The
/// decoy carries the very same tokens in its description, so the
/// unqualified term finds both — and the qualified one only the facts.
/// This is the v3 fact surface (camera, artist, album, font), which
/// `extract_fact_texts` fills from the asset's mined metadata.
#[test]
fn a_camera_qualifier_answers_only_the_camera_surface() {
    let idx = TextIndex::in_ram().unwrap();
    let camera = "cccc0000-0000-0000-0000-000000000001";
    let decoy = "cccc0000-0000-0000-0000-000000000002";
    let artist = "cccc0000-0000-0000-0000-000000000003";
    let album = "cccc0000-0000-0000-0000-000000000004";
    let font = "cccc0000-0000-0000-0000-000000000005";
    {
        let writer = idx.writer().unwrap();
        idx.index_asset_text(
            &writer,
            camera,
            "canon-eos.png",
            None,
            None,
            "",
            &FactTexts {
                camera: "Canon EOS R5 ISO 400 f/2.8 1/60s".into(),
                ..Default::default()
            },
            "",
        );
        idx.index_asset_text(
            &writer,
            decoy,
            "decoy.png",
            None,
            Some("canon ryuichi async inter"),
            "",
            &FactTexts::default(),
            "",
        );
        idx.index_asset_text(
            &writer,
            artist,
            "artist.png",
            None,
            None,
            "",
            &FactTexts {
                artist: "Ryuichi Sakamoto".into(),
                ..Default::default()
            },
            "",
        );
        idx.index_asset_text(
            &writer,
            album,
            "album.png",
            None,
            None,
            "",
            &FactTexts {
                album: "Async".into(),
                ..Default::default()
            },
            "",
        );
        idx.index_asset_text(
            &writer,
            font,
            "font.png",
            None,
            None,
            "",
            &FactTexts {
                font: "Inter Bold 400".into(),
                ..Default::default()
            },
            "",
        );
        drop(writer);
    }
    idx.commit().unwrap();

    let ids = |query: &str| ids_of_expr(&idx, query);
    assert_eq!(ids("camera:canon"), vec![camera.to_string()]);
    assert_eq!(ids("make:canon"), vec![camera.to_string()], "alias");
    assert_eq!(ids("model:r5"), vec![camera.to_string()], "alias");
    assert_eq!(ids("camera:iso"), vec![camera.to_string()]);
    assert_eq!(ids("artist:ryuichi"), vec![artist.to_string()]);
    assert_eq!(ids("artist:sakamoto"), vec![artist.to_string()]);
    assert_eq!(ids("album:async"), vec![album.to_string()]);
    assert_eq!(ids("font:inter"), vec![font.to_string()]);
    assert_eq!(ids("family:inter"), vec![font.to_string()], "alias");
    // Unqualified, the token is found wherever it lives — the facts
    // surface included, and the description of the decoy beside it.
    let mut both = ids("canon");
    both.sort();
    assert_eq!(both, vec![camera.to_string(), decoy.to_string()]);
    let mut both = ids("ryuichi");
    both.sort();
    assert_eq!(both, vec![decoy.to_string(), artist.to_string()]);
    let mut both = ids("async");
    both.sort();
    assert_eq!(both, vec![decoy.to_string(), album.to_string()]);
    let mut both = ids("inter");
    both.sort();
    assert_eq!(both, vec![decoy.to_string(), font.to_string()]);
    // A fact token nothing else carries is found by the plain term:
    // the composite facts surface is one of the unqualified surfaces.
    assert_eq!(ids("iso"), vec![camera.to_string()]);
}

/// The v5 audio surface: the technical specs answer from the audio facts
/// field alone (`audio:` is the umbrella, `sample_rate:` / `channels:` /
/// `bit_depth:` / `bitrate:` are the scoped aliases), and the same tokens
/// in a description do not answer a qualified ask — while unqualified,
/// "48000" finds the audio asset through the composite facts surface.
/// The artist field rides on the same mined record but keeps its own
/// surface: an `audio:` ask does not read it.
#[test]
fn an_audio_qualifier_answers_only_the_audio_surface() {
    let idx = TextIndex::in_ram().unwrap();
    let audio = "eeee0000-0000-0000-0000-000000000001";
    let decoy = "eeee0000-0000-0000-0000-000000000002";
    {
        let writer = idx.writer().unwrap();
        idx.index_asset_text(
            &writer,
            audio,
            "take-04.wav",
            None,
            None,
            "",
            &FactTexts {
                artist: "Ryuichi Sakamoto".into(),
                audio: "48000 Hz 2c 24bit 320kbps".into(),
                ..Default::default()
            },
            "",
        );
        idx.index_asset_text(
            &writer,
            decoy,
            "decoy.png",
            None,
            Some("a 48000 Hz 2c 24bit 320kbps fantasy, specs quoted in prose"),
            "",
            &FactTexts::default(),
            "",
        );
        drop(writer);
    }
    idx.commit().unwrap();

    let ids = |query: &str| ids_of_expr(&idx, query);
    assert_eq!(ids("audio:48000"), vec![audio.to_string()]);
    assert_eq!(ids("sample_rate:48000"), vec![audio.to_string()], "alias");
    assert_eq!(ids("channels:2c"), vec![audio.to_string()]);
    assert_eq!(
        ids("bit_depth:24"),
        vec![audio.to_string()],
        "word-start prefix of 24bit"
    );
    assert_eq!(ids("bit_depth:24bit"), vec![audio.to_string()]);
    assert_eq!(
        ids("bitrate:320"),
        vec![audio.to_string()],
        "word-start prefix of 320kbps"
    );
    // The specs are quoted verbatim in the decoy's description; a scoped
    // ask still refuses it.
    assert!(ids("audio:48000").iter().all(|id| id != decoy));
    // The artist rides the same mined record but answers on its own
    // surface only — the audio surface does not see it.
    assert_eq!(ids("artist:ryuichi"), vec![audio.to_string()]);
    assert!(
        ids("audio:ryuichi").is_empty(),
        "audio: does not read the artist surface"
    );
    // Unqualified, the spec token finds both — the composite facts
    // surface and the description beside it.
    let mut both = ids("48000");
    both.sort();
    assert_eq!(both, vec![audio.to_string(), decoy.to_string()]);
}

/// Field-scoped pinyin: `tag:mao` answers from the tags' own pinyin
/// field, and the same syllable in a file name does not answer a tag ask
/// — nor the reverse. That is the v4 split: before it, pinyin lived only
/// on the four surfaces concatenated, so a qualified ask either missed
/// entirely or could be answered by the wrong surface.
#[test]
fn a_qualified_pinyin_stays_on_its_own_surface() {
    let idx = TextIndex::in_ram().unwrap();
    let named = "bbbb0000-0000-0000-0000-000000000001";
    let tagged = "bbbb0000-0000-0000-0000-000000000002";
    let catnamed = "bbbb0000-0000-0000-0000-000000000003";
    let titled = "bbbb0000-0000-0000-0000-000000000004";
    {
        let writer = idx.writer().unwrap();
        idx.index_asset_text(
            &writer,
            named,
            "照片.png",
            None,
            None,
            "",
            &FactTexts::default(),
            "",
        );
        idx.index_asset_text(
            &writer,
            tagged,
            "b.png",
            None,
            None,
            "猫",
            &FactTexts::default(),
            "",
        );
        idx.index_asset_text(
            &writer,
            catnamed,
            "猫.png",
            None,
            None,
            "",
            &FactTexts::default(),
            "",
        );
        idx.index_asset_text(
            &writer,
            titled,
            "c.png",
            Some("海边"),
            None,
            "",
            &FactTexts::default(),
            "",
        );
        drop(writer);
    }
    idx.commit().unwrap();

    let ids = |query: &str| ids_of_expr(&idx, query);
    // `tag:mao` answers from the tags' pinyin alone: the file name that
    // carries the very same syllable must not answer a tag ask.
    assert_eq!(ids("tag:mao"), vec![tagged.to_string()]);
    assert_eq!(ids("name:mao"), vec![catnamed.to_string()]);
    assert_eq!(ids("name:zhao"), vec![named.to_string()]);
    assert_eq!(ids("name:pian"), vec![named.to_string()]);
    assert_eq!(ids("title:hai"), vec![titled.to_string()]);
    // …and the converse: the title's pinyin does not answer a tag ask.
    assert!(ids("tag:hai").is_empty(), "{:?}", ids("tag:hai"));
    // Unqualified, the syllable is found on every surface that carries it.
    let mut both = ids("mao");
    both.sort();
    assert_eq!(both, vec![tagged.to_string(), catnamed.to_string()]);
}

/// A pool that comes back exactly full is gathered again, wider, when
/// structured filters are about to reject rows from it.
///
/// The cap is applied *before* the SQL narrowing, so every asset past the
/// cap is invisible to a filtered search — including one that matches both
/// the term and the filter, which is the answer the caller came for. The
/// counts here are what that looks like from the outside: `CANDIDATE_CAP +
/// 5` matching documents, and the two asks that differ only in whether
/// something will reject rows afterwards.
#[test]
fn a_saturated_pool_is_gathered_wider_only_when_it_will_be_filtered() {
    let count = CANDIDATE_CAP + 5;
    let idx = TextIndex::in_ram().unwrap();
    {
        let writer = idx.writer().unwrap();
        for _ in 0..count {
            idx.index_asset_text(
                &writer,
                &crate::model::new_id().to_string(),
                "flood.png",
                None,
                None,
                "",
                &FactTexts::default(),
                "",
            );
        }
        drop(writer);
    }
    idx.commit().unwrap();
    assert_eq!(idx.num_docs() as usize, count);

    let expr = expression::parse("flood").into_expression();
    let (pooled, ran_out) = idx.pool_for("flood", &expr, None, true).unwrap();
    assert!(
        pooled.len() > CANDIDATE_CAP,
        "a filtered ask reached past the cap and found {}",
        pooled.len()
    );
    assert!(
        !ran_out,
        "the library holds {count} and the pool saw all of them"
    );

    // With nothing to reject rows, the wider gather would buy nothing: the
    // caller shows a page out of this pool either way. What it must get is
    // the honest flag, so the number beside the grid reads as a floor.
    let (plain, ran_out) = idx.pool_for("flood", &expr, None, false).unwrap();
    assert_eq!(plain.len(), CANDIDATE_CAP);
    assert!(ran_out, "a saturated unfiltered pool says so");
}

/// A held writer lock is an error, not a panic — the desktop app and the
/// CLI are allowed to be open on the same library at the same time.
#[test]
fn open_refuses_a_held_writer_lock_without_panicking() {
    let dir = temp_index_dir();
    let owner = TextIndex::open(&dir).expect("the first open owns the index");
    assert!(owner.is_writable());

    let second = TextIndex::open(&dir);
    assert!(
        second.is_err(),
        "a second writable handle must be refused while the first holds the lock",
    );

    // ... and the read-only constructor is what the second process uses.
    let reader = TextIndex::open_read_only(&dir).expect("read-only open succeeds");
    assert!(!reader.is_writable());
    assert!(reader.commit().is_err(), "reads-only handle refuses writes");
    assert!(reader.search("anything", 10).unwrap().is_empty());

    drop(owner);
    drop(reader);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A read-only handle builds an index when there is none at all — the
/// state a CLI finds a freshly created library in — but never repairs one
/// that exists, because that directory may belong to another process.
#[test]
fn open_read_only_builds_only_an_absent_index() {
    let fresh = temp_index_dir();
    let idx = TextIndex::open_read_only(&fresh).unwrap();
    assert!(idx.is_writable(), "there was no index to conflict with");
    assert_eq!(idx.num_docs(), 0);
    drop(idx);
    assert!(fresh.join("meta.json").is_file(), "an index was created");
    let _ = std::fs::remove_dir_all(&fresh);

    let stale = temp_index_dir();
    drop(TextIndex::open(&stale).unwrap());
    std::fs::write(stale.join("trove-index-version"), "99").unwrap();
    let before = directory_entries(&stale);

    let reader = TextIndex::open_read_only(&stale).unwrap();
    assert!(!reader.is_writable());
    assert_eq!(reader.num_docs(), 0);
    assert_eq!(
        before,
        directory_entries(&stale),
        "a read-only open must leave an existing index untouched",
    );
    let _ = std::fs::remove_dir_all(&stale);
}

fn directory_entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The colour qualifier answers on three roads at once: the compact form
/// rides the trigram surface (`color:adobergb` against a name with spaces in
/// it), the profile's own words ride the word surface, and a quoted phrase
/// works because the colour words field carries positions. Unqualified text
/// deliberately does not reach the colour surfaces — `color:` is how you ask.
#[test]
fn a_color_qualifier_matches_word_gram_and_phrase() {
    let idx = TextIndex::in_ram().unwrap();
    {
        let writer = idx.writer().unwrap();
        idx.index_asset_text(
            &writer,
            "cccc0000-0000-0000-0000-000000000001",
            "wide.png",
            None,
            None,
            "",
            &super::facts::FactTexts {
                color: "Adobe RGB (1998)".into(),
                ..Default::default()
            },
            "",
        );
        drop(writer);
    }
    idx.commit().unwrap();

    assert_eq!(
        ids_of_expr(&idx, "color:adobergb"),
        vec!["cccc0000-0000-0000-0000-000000000001"],
        "the compact form matches across the name's spaces"
    );
    assert_eq!(
        ids_of_expr(&idx, "color:adobe"),
        vec!["cccc0000-0000-0000-0000-000000000001"],
    );
    assert_eq!(
        ids_of_expr(&idx, "color:\"adobe rgb\""),
        vec!["cccc0000-0000-0000-0000-000000000001"],
    );
    // A negative case has to sit beyond fuzzy reach: `color:srgb` would
    // legitimately hit the RGB words above through the same edit-distance
    // typo tolerance every word surface enjoys.
    assert!(ids_of_expr(&idx, "color:zebra").is_empty());
    assert!(
        ids_of(&idx, "adobergb").is_empty(),
        "technical metadata stays out of the unqualified surfaces"
    );
}

/// The body surface's end-to-end road: a real library, a real `.md` file,
/// the real drain (which is what passes the library root down to the
/// indexer), and a search that finds the file by what it *says* — the
/// unqualified path for knowledge-base queries, the `body:`/`content:`
/// qualifiers for scoped ones, a quoted phrase for word order. The 64 KiB
/// cap is asserted the honest way: a term past it does not match.
#[test]
fn a_text_body_is_indexed_from_the_file() {
    let root = std::env::temp_dir().join(format!(
        "trove-index-body-{}",
        crate::model::new_id().simple()
    ));
    let lib = crate::library::Library::open(&root, root.join("cache")).unwrap();
    let src = root.join("notes.md");
    std::fs::write(
        &src,
        b"# Lab notes\nThe quantum flux capacitor needs calibrating. The quick brown fox escapes.\n",
    )
    .unwrap();
    lib.import_into_store(std::slice::from_ref(&src), None)
        .unwrap();
    lib.drain_search_queue().unwrap();

    let idx = lib.text_index();
    assert_eq!(
        ids_of(idx, "quantum").len(),
        1,
        "an unqualified term reaches the body"
    );
    assert_eq!(
        ids_of_expr(idx, "body:calibrating"),
        ids_of_expr(idx, "content:calibrating"),
        "the two spellings name the same surface"
    );
    assert_eq!(ids_of_expr(idx, "body:calibrating").len(), 1);
    assert_eq!(
        ids_of_expr(idx, "body:\"quick brown\"").len(),
        1,
        "phrases match positions inside the body"
    );
    assert!(ids_of_expr(idx, "body:missingword").is_empty());

    // Past the cap: a 65 KiB file whose last line holds a unique word.
    let big = root.join("big.log");
    let mut body = "word ".repeat(14_000);
    body.push_str("tailmarkerword\n");
    std::fs::write(&big, body.as_bytes()).unwrap();
    lib.import_into_store(std::slice::from_ref(&big), None)
        .unwrap();
    lib.drain_search_queue().unwrap();
    assert!(
        ids_of(idx, "tailmarkerword").is_empty(),
        "a term beyond the 64 KiB index cap must not be promised"
    );
    assert_eq!(
        ids_of(idx, "word").len(),
        1,
        "the beginning stays searchable"
    );

    let _ = std::fs::remove_dir_all(&root);
}
