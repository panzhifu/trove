//! Head-to-head benchmark against Serpent's large-library fixture.
//!
//! ```text
//! cargo run --release -p trove-core --example serpent_parity_bench -- \
//!     bench/work/lib-20k 5
//! ```
//!
//! The argument is a Serpent fixture directory — the output of
//! `npm run large-library:generate` inside `reference/Serpent` — which holds
//! both `.serpent/library.db` (its library database) and `Assets/` (the files
//! those rows describe). Two phases run against it, and they are deliberately
//! *not* the same library:
//!
//! **Ingest** imports `Assets/` through the real job and times it. This is the
//! honest end-to-end number, and it is also where the two applications differ
//! in kind rather than in speed: the fixture's images and videos are copies of
//! a small pool, so ~20 000 files carry a few hundred distinct contents and
//! Trove's content-hash dedup collapses them. Both counts are reported.
//!
//! **Query** runs against a *mirrored* library: every row of Serpent's own
//! database translated into a Trove `Asset` (same names, sizes, dimensions,
//! ratings, tags, collection memberships and timestamps) and inserted
//! directly. That is what Serpent's benchmark does for itself — its fixture
//! generator writes rows, it never imports — so mirroring is the only shape
//! where both sides answer the same question: what does this metadata corpus
//! cost to page, sort, filter and search. Timing the ingest library instead
//! would compare 20 000 rows against a few hundred.
//!
//! Metric names match Serpent's `comprehensive-perf-bench` keys so the two
//! reports pair up row for row. Each is a median over `rounds` timed rounds
//! after one discarded warm-up, with min and max printed beside it — the
//! convention every bench in this directory follows, and the reason
//! [`import_bench`] states: this filesystem is btrfs and a single round drifts
//! by two or three times. The last line is one `PARITY_JSON` document so
//! `bench/aggregate.mjs` can merge it with Serpent's `PERF_BENCH_JSON`.
//!
//! [`import_bench`]: crate::import_bench

use std::collections::HashMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use trove_core::layout::justify_layout;
use trove_core::library::Library;
use trove_core::media::probe;
use trove_core::model::{
    Asset, AssetFacts, AssetKind, AssetLocation, AssetQuery, AssetSeed, AssetSort,
    MAX_DESCRIPTION_LEN, NewCollection, NewTag, UsageStatus,
};
use trove_core::store::{BrowseContext, assets, collections, tags};

/// A connection of this benchmark's own to the library's database.
///
/// The store's handle is private to the crate, and it should stay that way: one
/// `&Connection` shared across layers is what let any caller write anywhere.
/// WAL supports a second reader/writer over the same file, which is what a
/// harness that issues raw SQL — inserts, queue drains, row counts — actually
/// wants, since it then measures the same file the app writes.
fn raw_conn(db: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open the library database")
}

use trove_core::tasks::import::{ImportOptions, ImportSource};
use trove_core::tasks::{TaskKind, TaskManager};
use uuid::Uuid;

/// The width the grid lays its rows out into, matching `layout_bench`.
const CONTENT_WIDTH: f32 = 1440.0;
/// The page size both applications are asked for, matching Serpent's bench.
const PAGE: u32 = 50;
/// The deep scroll position both applications are asked for.
const DEEP_OFFSET: u64 = 10_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let fixture: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| usage("a Serpent fixture directory is required"));
    let rounds: usize = args
        .next()
        .filter(|a| !a.starts_with("--"))
        .and_then(|a| a.parse().ok())
        .unwrap_or(5);
    let rebuild = args.any(|a| a == "--rebuild");

    let serpent_db = fixture.join(".serpent/library.db");
    if !serpent_db.exists() {
        usage(&format!("no Serpent database at {}", serpent_db.display()));
    }
    let declared = read_manifest_asset_count(&fixture.join(".serpent/large-library-fixture.json"));

    println!("=== Trove parity bench ===");
    println!(
        "fixture        : {} ({} assets declared)",
        fixture.display(),
        declared
    );
    println!("rounds         : {rounds} timed after 1 warm-up");
    println!(
        "stage pool     : up to {} threads",
        trove_core::media::import::stage_thread_ceiling()
    );
    println!();

    let mut json = serde_json::Map::new();
    json.insert("suite".into(), serde_json::json!("trove-parity"));
    json.insert("assets".into(), serde_json::json!(declared));

    if std::env::var("PARITY_SKIP_INGEST").is_err() {
        run_ingest(&fixture, &serpent_db, &mut json);
    }
    run_query(&fixture, &serpent_db, rounds, rebuild, &mut json);

    println!("PARITY_JSON {}", serde_json::Value::Object(json));
}

fn usage(why: &str) -> ! {
    eprintln!("usage: serpent_parity_bench <serpent-fixture-dir> [rounds] [--rebuild]\n{why}");
    std::process::exit(2);
}

// ---------------------------------------------------------------------------
// Phase 1 — ingest the fixture's own files through the real import job
// ---------------------------------------------------------------------------

fn run_ingest(
    fixture: &Path,
    serpent_db: &Path,
    json: &mut serde_json::Map<String, serde_json::Value>,
) {
    let root = fixture.with_extension("trove-native");
    let cache = root.join("cache");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let assets_dir = fixture.join("Assets");

    let options = ImportOptions {
        data_root: root.clone(),
        cache_root: cache.clone(),
        storage: trove_core::media::import::ImportStorage::Link,
        source: ImportSource::Paths {
            paths: vec![assets_dir.clone()],
            into_collection: None,
        },
    };

    let files = walk_count(&assets_dir);
    let bytes = walk_bytes(&assets_dir);
    println!(
        "--- ingest: {files} files, {:.1} GiB ---",
        bytes as f64 / (1 << 30) as f64
    );

    let started = Instant::now();
    let manager = TaskManager::new();
    let (_, rx) = manager
        .start(TaskKind::Import, "parity-ingest", move |ctx| {
            trove_core::tasks::import::run(&options, ctx)
        })
        .expect("the import job starts");
    let outcome = rx.recv().expect("the import job finishes");
    let elapsed = started.elapsed();

    let report = &outcome.report;
    let reused = report.imported.iter().filter(|item| item.reused).count();
    let first_skips: Vec<String> = report
        .skipped
        .iter()
        .take(3)
        .map(|s| format!("{} — {}", s.path.display(), s.reason))
        .collect();

    let lib = Library::open(&root, &cache).unwrap();
    let rows = count_rows(&lib);
    let index_before = lib.text_index().num_docs();
    let index_start = Instant::now();
    lib.drain_search_queue().unwrap();
    let index_secs = index_start.elapsed().as_secs_f64();
    let index_docs = lib.text_index().num_docs();
    drop(lib);

    let thumbs = walk_count(&cache.join("thumbs"));
    let secs = elapsed.as_secs_f64();
    println!(
        "  import          : {:.1} s   ({:.0} files/s, {:.0} MiB/s)",
        secs,
        files as f64 / secs,
        bytes as f64 / (1 << 20) as f64 / secs
    );
    println!(
        "  outcome         : {} inserted, {} deduped onto an existing row, {} already held, {} skipped",
        report.imported_count() - reused,
        reused,
        report.already_imported,
        report.skipped_count()
    );
    for line in &first_skips {
        println!("    skip            : {line}");
    }
    println!(
        "  asset rows      : {rows}   (Serpent keeps {} for the same tree)",
        count_serpent_rows(serpent_db)
    );
    println!("  thumbnails      : {thumbs}");
    println!(
        "  text index      : {index_secs:.1} s   ({index_docs} docs, {index_before} already present)"
    );
    println!(
        "  library on disk : {:.1} MiB",
        dir_bytes(&root) as f64 / (1 << 20) as f64
    );
    println!();

    insert(json, "ingestFiles", files as f64);
    insert(json, "ingestBytes", bytes as f64);
    insert(json, "ingestSeconds", round(secs, 2));
    insert(json, "ingestFilesPerSecond", round(files as f64 / secs, 1));
    insert(
        json,
        "ingestMiBPerSecond",
        round(bytes as f64 / (1 << 20) as f64 / secs, 1),
    );
    insert(json, "ingestAssetRows", rows as f64);
    insert(json, "ingestThumbnails", thumbs as f64);
    insert(json, "ingestIndexBuildSeconds", round(index_secs, 2));
}

// ---------------------------------------------------------------------------
// Phase 2 — query a mirror of Serpent's own rows
// ---------------------------------------------------------------------------

fn run_query(
    fixture: &Path,
    serpent_db: &Path,
    rounds: usize,
    rebuild: bool,
    json: &mut serde_json::Map<String, serde_json::Value>,
) {
    let root = fixture.with_extension("trove-mirror");
    let cache = root.join("cache");

    if let Some(steps) = build_mirror(fixture, serpent_db, &root, rebuild) {
        println!("--- mirror build ---");
        for (label, secs) in &steps {
            println!("  {label:<15}: {:>8.1} ms", secs * 1000.0);
        }
        println!();
    }

    // A cold open is one of Serpent's own metrics, so measure it before the
    // library the queries run against exists.
    let open_start = Instant::now();
    let probe_lib = Library::open(&root, &cache).unwrap();
    let open_ms = open_start.elapsed().as_secs_f64() * 1000.0;
    drop(probe_lib);

    let lib = Library::open(&root, &cache).unwrap();
    let conn = &raw_conn(&lib.db_path());
    let text = lib.text_index();
    let total = count_rows(&lib);

    let (folder_leaf, folder_root) = folder_prefixes(fixture);
    let collection_id = collections::list(conn)
        .ok()
        .and_then(|v| v.first().map(|c| c.id));
    let sample_id = assets::query(
        conn,
        &AssetQuery {
            limit: Some(PAGE),
            sort_desc: true,
            ..AssetQuery::live()
        },
    )
    .ok()
    .and_then(|p| p.items.first().map(|a| a.id));

    println!("--- query: {total} mirrored assets, median of {rounds} ---");
    let mut report: Vec<Row> = Vec::new();
    report.push(Row {
        label: "openLibraryMs".into(),
        median: round(open_ms, 2),
        min: round(open_ms, 2),
        max: round(open_ms, 2),
    });

    measure(&mut report, "allBrowseFirstPageMs", rounds, || {
        run(conn, text, &browse(|_| {}));
    });
    measure(&mut report, "allBrowseFirstPageNoCountMs", rounds, || {
        browse(|_| {})
            .run_without_count(conn, text, Some(PAGE), None)
            .unwrap();
    });
    measure(&mut report, "browseFirstPageAscMs", rounds, || {
        // The unindexed direction, kept as its own row: this is what the
        // struct default measures, and it is five to eight times the cost.
        run(conn, text, &BrowseContext::default());
    });
    measure(&mut report, "deepOffsetPageMs", rounds, || {
        run(conn, text, &browse(|c| c.offset = DEEP_OFFSET));
    });
    measure(&mut report, "deepOffsetPageNoCountMs", rounds, || {
        browse(|c| c.offset = DEEP_OFFSET)
            .run_without_count(conn, text, Some(PAGE), None)
            .unwrap();
    });
    measure(&mut report, "exactCountOnlyMs", rounds, || {
        assets::count(conn, &AssetQuery::live()).unwrap();
    });
    measure(&mut report, "browseSessionOpenMs", rounds, || {
        browse(|_| {}).snapshot(conn, text, None, true).unwrap();
    });
    measure(&mut report, "browseSessionPageMs", rounds, || {
        let session = browse(|_| {}).snapshot(conn, text, None, true).unwrap();
        session
            .page(conn, text, DEEP_OFFSET as usize, Some(PAGE as usize))
            .unwrap();
    });
    measure(&mut report, "scrollTwentyWindowsMs", rounds, || {
        let session = browse(|_| {}).snapshot(conn, text, None, true).unwrap();
        let mut offset = 0usize;
        for _ in 0..20 {
            session
                .page(conn, text, offset, Some(PAGE as usize))
                .unwrap();
            offset += PAGE as usize;
        }
    });
    if let Some(cid) = collection_id {
        measure(&mut report, "collectionSwitchMs", rounds, || {
            run(conn, text, &browse(|c| c.collection = Some(cid)));
        });
        measure(&mut report, "collectionSwitchNoCountMs", rounds, || {
            browse(|c| c.collection = Some(cid))
                .run_without_count(conn, text, Some(PAGE), None)
                .unwrap();
        });
    }
    measure(&mut report, "folderSwitchMs", rounds, || {
        run(
            conn,
            text,
            &browse(|c| c.folder = Some(folder_leaf.clone())),
        );
    });
    measure(&mut report, "folderSwitchNoCountMs", rounds, || {
        browse(|c| c.folder = Some(folder_leaf.clone()))
            .run_without_count(conn, text, Some(PAGE), None)
            .unwrap();
    });
    measure(&mut report, "folderSwitchRecursiveMs", rounds, || {
        run(
            conn,
            text,
            &browse(|c| c.folder = Some(folder_root.clone())),
        );
    });
    measure(
        &mut report,
        "folderSwitchRecursiveNoCountMs",
        rounds,
        || {
            browse(|c| c.folder = Some(folder_root.clone()))
                .run_without_count(conn, text, Some(PAGE), None)
                .unwrap();
        },
    );
    measure(&mut report, "searchFixedTokenMs", rounds, || {
        run(conn, text, &browse(|c| c.search = "asset".into()));
    });
    measure(&mut report, "searchNeedleMs", rounds, || {
        run(
            conn,
            text,
            &browse(|c| c.search = "serpent-large-library-needle".into()),
        );
    });
    measure(&mut report, "indexOnlySearchMs", rounds, || {
        text.search("asset", trove_core::search::CANDIDATE_CAP)
            .unwrap();
    });
    for (label, sort, desc) in [
        ("sortNameAscMs", AssetSort::Name, false),
        ("sortCreatedAtDescMs", AssetSort::CreatedAt, true),
        ("sortUpdatedAtDescMs", AssetSort::UpdatedAt, true),
        ("sortByteSizeDescMs", AssetSort::SizeBytes, true),
        ("sortRatingDescMs", AssetSort::Rating, true),
    ] {
        measure(&mut report, label, rounds, move || {
            run(
                conn,
                text,
                &BrowseContext {
                    sort,
                    sort_desc: desc,
                    ..BrowseContext::default()
                },
            );
        });
    }
    measure(&mut report, "filterRatingMs", rounds, || {
        run(conn, text, &browse(|c| c.min_rating = Some(3)));
    });
    measure(&mut report, "filterKindImageMs", rounds, || {
        run(conn, text, &browse(|c| c.kind = Some(AssetKind::Image)));
    });
    measure(&mut report, "filterKindImageNoCountMs", rounds, || {
        browse(|c| c.kind = Some(AssetKind::Image))
            .run_without_count(conn, text, Some(PAGE), None)
            .unwrap();
    });
    if let Some(id) = sample_id {
        measure(&mut report, "inspectorMetadataMs", rounds, || {
            let rows = assets::by_ids(conn, &[id]).unwrap();
            collections::for_asset(conn, rows[0].id).unwrap();
            tags::for_asset(conn, rows[0].id).unwrap();
        });
    }
    measure(&mut report, "sidebarListFoldersMs", rounds, || {
        assets::source_folders(conn).unwrap();
    });
    measure(&mut report, "sidebarListCollectionsMs", rounds, || {
        collections::list(conn).unwrap();
    });
    // Serpent's `layoutOnly` is one call returning the geometry of the whole
    // listing, so this gathers every aspect — paged, because the store caps one
    // window — and then runs the same justify pass the grid does.
    measure(&mut report, "layoutOnlyMs", rounds, || {
        let mut aspects = Vec::with_capacity(total);
        let mut offset = 0u64;
        while offset < total as u64 {
            let page = browse(|c| c.offset = offset)
                .run_without_count(conn, text, Some(assets::MAX_PAGE), None)
                .unwrap();
            if page.items.is_empty() {
                break;
            }
            offset += page.items.len() as u64;
            for asset in &page.items {
                aspects.push(match (asset.width, asset.height) {
                    (Some(w), Some(h)) if h > 0 => w as f32 / h as f32,
                    _ => 1.0,
                });
            }
        }
        let rows = justify_layout(&aspects, CONTENT_WIDTH);
        black_box(rows.iter().map(|row| row.height).sum::<f32>());
    });
    // The floor under the row above: the same geometry pass reading only what
    // geometry needs. `layoutOnlyMs` pages the whole library through the store,
    // so it pays a full `Asset` per row — `extra` deserialized into `AssetFacts`
    // included — for two integers. One statement and two columns is what the
    // pass actually costs; the gap between the two is row materialisation, not
    // layout, and the app never pays it because it fetches a 200-row window at a
    // time (`workspace::next_window`) rather than the whole listing.
    measure(&mut report, "layoutGeometryFloorMs", rounds, || {
        let mut aspects = Vec::with_capacity(total);
        {
            let mut stmt = conn
                .prepare(
                    "SELECT width, height FROM assets WHERE trashed_at IS NULL \
                     ORDER BY created_at DESC, id ASC",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, Option<u32>>(0)?, row.get::<_, Option<u32>>(1)?))
                })
                .unwrap();
            for row in rows {
                let (width, height) = row.unwrap();
                aspects.push(match (width, height) {
                    (Some(w), Some(h)) if h > 0 => w as f32 / h as f32,
                    _ => 1.0,
                });
            }
        }
        let rows = justify_layout(&aspects, CONTENT_WIDTH);
        black_box(rows.iter().map(|row| row.height).sum::<f32>());
    });

    println!(
        "  {:<28}{:>10}{:>10}{:>10}",
        "metric", "median", "min", "max"
    );
    for row in &report {
        println!(
            "  {:<28}{:>10.2}{:>10.2}{:>10.2}",
            row.label, row.median, row.min, row.max
        );
    }
    // --- where does the time actually go? -----------------------------------
    //
    // Every listing metric above runs through `store::assets`. This block runs
    // the same statement two more ways: on a bare connection with the same
    // pragmas, and on one with none. The gap between `store` and `bare` is
    // Trove's own read path; the gap between `bare` and Serpent's engine-level
    // cost is SQLite's.
    {
        let sql = "SELECT id, origin, rel_path, file_name, ext, mime, size_bytes, content_hash, \
                   kind, width, height, duration_ms, captured_at, title, description, \
                   rating, is_favorite, source_url, extra, created_at, updated_at, trashed_at, \
                   usage_status, commercial_use FROM assets WHERE trashed_at IS NULL \
                   ORDER BY created_at DESC, id ASC LIMIT ?1 OFFSET ?2";
        let bare = Connection::open(root.join("library.db")).unwrap();
        let sql_first = "SELECT id, origin, rel_path, file_name, ext, mime, size_bytes, content_hash, \
                   kind, width, height, duration_ms, captured_at, title, description, \
                   rating, is_favorite, source_url, extra, created_at, updated_at, trashed_at, \
                   usage_status, commercial_use FROM assets WHERE trashed_at IS NULL \
                   ORDER BY created_at DESC, id ASC LIMIT ?1 OFFSET ?2";
        let timed = |conn: &Connection, offset: i64| -> f64 {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let mut stmt = conn.prepare_cached(sql).unwrap();
                let n = stmt
                    .query_map(rusqlite::params![50i64, offset], |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .count();
                let e = t.elapsed().as_secs_f64() * 1000.0;
                if n != 50 {
                    panic!("the floor query returned {n} rows at offset {offset}");
                }
                best = best.min(e);
            }
            best
        };
        let bare_first = timed(&bare, 0);
        let bare_deep = timed(&bare, DEEP_OFFSET as i64);
        let store_count = {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                assets::count(conn, &AssetQuery::live()).unwrap();
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            best
        };
        let store_first = {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                assets::query_without_count(
                    conn,
                    &AssetQuery {
                        limit: Some(PAGE),
                        sort_desc: true,
                        ..AssetQuery::live()
                    },
                )
                .unwrap();
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            best
        };
        // Split the store's own cost three ways: the COUNT alone, the same
        // 24-column read with a trivial mapper, and that read plus a JSON parse
        // of `extra` (what `parse_facts` does per row).
        let probe =
            |sql: &str, args: &[&dyn rusqlite::types::ToSql], json: bool, cols: usize| -> f64 {
                let mut best = f64::MAX;
                for _ in 0..5 {
                    let t = Instant::now();
                    let mut stmt = bare.prepare_cached(sql).unwrap();
                    let n = stmt
                        .query_map(rusqlite::params_from_iter(args.iter().copied()), |row| {
                            let mut kept = 0usize;
                            for i in 0..cols {
                                if row.get_ref(i).is_ok() {
                                    kept += 1;
                                }
                            }
                            if json {
                                let text: String = row.get(18)?;
                                let parsed: serde_json::Value =
                                    serde_json::from_str(&text).unwrap_or_default();
                                kept += usize::from(parsed.is_object());
                            }
                            Ok(kept)
                        })
                        .unwrap()
                        .count();
                    let e = t.elapsed().as_secs_f64() * 1000.0;
                    if n == 0 {
                        panic!("probe returned no rows");
                    }
                    best = best.min(e);
                }
                best
            };
        let count_floor = probe(
            "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL",
            &[],
            false,
            1,
        );
        let read_floor = probe(sql_first, &[&50i64, &0i64], false, 24);
        let json_floor = probe(sql_first, &[&50i64, &0i64], true, 24);
        // Same read, but deserializing `extra` into the real typed model —
        // `#[serde(flatten)]` across five sub-structs is the one step the
        // generic `serde_json::Value` probe above cannot reproduce.
        let typed_floor = {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let mut stmt = bare.prepare_cached(sql_first).unwrap();
                let n = stmt
                    .query_map(rusqlite::params![50i64, 0i64], |row| {
                        let text: String = row.get(18)?;
                        let facts: AssetFacts = serde_json::from_str(&text).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                18,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                        // Any field touches the whole parsed value; what the
                        // probe measures is the deserialisation, not which key
                        // it reads. `source_path` is no longer readable from
                        // outside the crate now that a record's location is
                        // read through `Asset::location`.
                        Ok(usize::from(facts.photo.make.is_some()))
                    })
                    .unwrap()
                    .count();
                let e = t.elapsed().as_secs_f64() * 1000.0;
                if n == 0 {
                    panic!("typed probe returned no rows");
                }
                best = best.min(e);
            }
            best
        };
        let extra_len: f64 = {
            let sum: i64 = bare
                .query_row(
                    "SELECT SUM(length(extra)) FROM assets WHERE trashed_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            sum as f64 / 20000.0
        };
        println!(
            "  COUNT floor          : {count_floor:.2} ms   (store::count {store_count:.2} ms)"
        );
        println!(
            "  24-col read floor    : {read_floor:.2} ms   + JSON parse {json_floor:.2} ms   (store read {store_first:.2} ms)"
        );
        println!("  extra column size    : {extra_len:.0} bytes/row average");
        insert(json, "countFloorMs", round(count_floor, 2));
        insert(json, "readFloorMs", round(read_floor, 2));
        insert(json, "readFloorWithJsonMs", round(json_floor, 2));
        // Same statement, same mapper, but with the pragmas `Store::open` puts
        // on every library connection: this is the last difference left between
        // the floor and the store path.
        let store_pragmas = {
            let p = Connection::open(root.join("library.db")).unwrap();
            p.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-16000; PRAGMA foreign_keys=ON;")
                .unwrap();
            p.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let mut stmt = p.prepare_cached(sql_first).unwrap();
                let n = stmt
                    .query_map(rusqlite::params![50i64, 0i64], |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .count();
                let e = t.elapsed().as_secs_f64() * 1000.0;
                if n != 50 {
                    panic!("pragma probe returned {n}");
                }
                best = best.min(e);
            }
            best
        };
        println!("  extra -> typed facts : {typed_floor:.2} ms for the same 50 rows");
        // Trove's own connection, the hand-written statement: this separates
        // "the connection is slow" from "the SQL Trove builds is different".
        let store_conn_bare_sql = {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                let mut stmt = conn.prepare_cached(sql_first).unwrap();
                let n = stmt
                    .query_map(rusqlite::params![50i64, 0i64], |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .count();
                let e = t.elapsed().as_secs_f64() * 1000.0;
                if n != 50 {
                    panic!("store-connection probe returned {n}");
                }
                best = best.min(e);
            }
            best
        };
        println!("  floor + store pragmas: {store_pragmas:.2} ms for the same 50 rows");
        println!("  same SQL, Trove conn : {store_conn_bare_sql:.2} ms for the same 50 rows");
        insert(json, "typedFactsFloorMs", round(typed_floor, 2));
        insert(json, "floorWithStorePragmasMs", round(store_pragmas, 2));
        insert(json, "sameSqlTroveConnMs", round(store_conn_bare_sql, 2));
        insert(json, "extraBytesPerRow", round(extra_len, 1));
        println!(
            "  sql floor, bare conn   : {bare_first:.2} ms first page / {bare_deep:.2} ms at offset {DEEP_OFFSET}"
        );
        println!("  same query via store   : {store_first:.2} ms first page");
        insert(json, "sqlFloorFirstPageMs", round(bare_first, 2));
        insert(json, "sqlFloorDeepPageMs", round(bare_deep, 2));
        insert(json, "storeReadFirstPageMs", round(store_first, 2));
    }

    let db_bytes = dir_bytes(&root);
    println!(
        "  mirror database : {:.1} MiB",
        db_bytes as f64 / (1 << 20) as f64
    );
    println!();

    for row in &report {
        insert(json, &row.label, row.median);
        json.insert(format!("{}Min", row.label), serde_json::json!(row.min));
        json.insert(format!("{}Max", row.label), serde_json::json!(row.max));
    }
    insert(json, "mirrorDbBytes", db_bytes as f64);
    insert(json, "mirrorTotalRows", total as f64);
}

/// A browse with the sort direction the grid actually asks for.
///
/// `BrowseContext`'s and `AssetQuery`'s `sort_desc` is a plain `bool`, so its
/// default is *ascending* — and the library's ordered partial indexes are
/// `(col DESC, id ASC)`, which an ascending sort with an ascending tiebreaker
/// cannot use at all (SQLite answers it with a temp B-tree over the whole
/// live set). Serpent's bench asks for `created_at: desc`, so every row here
/// has to as well or the two sides are not measuring the same order.
fn browse(over: impl FnOnce(&mut BrowseContext)) -> BrowseContext {
    let mut ctx = BrowseContext {
        sort_desc: true,
        ..BrowseContext::default()
    };
    over(&mut ctx);
    ctx
}

fn run(conn: &Connection, text: &trove_core::search::TextIndex, ctx: &BrowseContext) {
    ctx.run(conn, text, Some(PAGE), None).unwrap();
}

struct Row {
    label: String,
    median: f64,
    min: f64,
    max: f64,
}

fn measure(report: &mut Vec<Row>, label: &str, rounds: usize, mut f: impl FnMut()) {
    f();
    let mut times: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        f();
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report.push(Row {
        label: label.into(),
        median: round(times[times.len() / 2], 2),
        min: round(times[0], 2),
        max: round(times[times.len() - 1], 2),
    });
}

/// Translate Serpent's rows into Trove's, in its own database-shaped order.
/// Returns `None` when a current mirror already exists on disk, so repeat runs
/// go straight to querying.
fn build_mirror(
    fixture: &Path,
    serpent_db: &Path,
    root: &Path,
    rebuild: bool,
) -> Option<Vec<(String, f64)>> {
    let cache = root.join("cache");
    let marker = root.join("mirror-built");
    let want = count_serpent_rows(serpent_db);
    if !rebuild
        && marker.exists()
        && let Ok(lib) = Library::open(root, &cache)
        && count_rows(&lib) == want
    {
        return None;
    }
    std::fs::remove_dir_all(root).ok();
    std::fs::create_dir_all(root).unwrap();

    let mut steps: Vec<(String, f64)> = Vec::new();
    let assets_root = fixture.join("Assets").display().to_string();

    let lib = Library::open(root, &cache).unwrap();
    let src = Connection::open_with_flags(serpent_db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

    // Serpent's asset ids are already UUIDs and are kept, so only the two
    // container tables need an old→new map.
    let t = Instant::now();
    let mut collection_map: HashMap<Uuid, Uuid> = HashMap::new();
    let mut tag_map: HashMap<Uuid, Uuid> = HashMap::new();
    {
        let conn = &raw_conn(&lib.db_path());
        for (id, name, position) in triples(
            &src,
            "SELECT collection_id, name, position FROM collections ORDER BY position",
        ) {
            let created = collections::create(
                conn,
                &NewCollection {
                    parent_id: None,
                    name,
                    position,
                },
            )
            .unwrap();
            collection_map.insert(Uuid::parse_str(&id).unwrap(), created.id);
        }
        for (id, name) in pairs(&src, "SELECT tag_id, name FROM tags ORDER BY name") {
            let created = tags::create(
                conn,
                &NewTag {
                    name,
                    color: None,
                    parent_id: None,
                },
            )
            .unwrap();
            tag_map.insert(Uuid::parse_str(&id).unwrap(), created.id);
        }
    }
    steps.push(("containers".into(), t.elapsed().as_secs_f64()));

    let t = Instant::now();
    let mut mirrored = 0usize;
    {
        let conn = &raw_conn(&lib.db_path());
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut stmt = src
            .prepare(
                "SELECT a.asset_id, a.relative_file_path, a.created_at, a.updated_at,
                        r.original_filename, r.byte_size,
                        m.rating, m.favorite, m.description,
                        art.width, art.height, art.duration_ms
                   FROM assets a
                   JOIN revisions r ON r.revision_id = a.current_revision_id
                   LEFT JOIN asset_metadata m ON m.asset_id = a.asset_id
                   LEFT JOIN revision_artifacts art
                        ON art.revision_id = a.current_revision_id
                       AND art.artifact_role = 'technical-metadata'
                  ORDER BY a.rowid",
            )
            .unwrap();
        let out = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                ))
            })
            .unwrap();
        for row in out {
            let (
                id,
                rel,
                created_at,
                updated_at,
                file_name,
                size,
                rating,
                favorite,
                description,
                width,
                height,
                duration_ms,
            ) = row.unwrap();
            let ext = probe::normalize_ext(file_name.rsplit('.').next().unwrap_or(""));
            let probed = probe::probe(&ext);
            let asset = Asset::from_seed(AssetSeed {
                id: Uuid::parse_str(&id).unwrap(),
                // Serpent's rows are all external files: the mirror links to
                // the path it is assembled from below, which is what
                // `AssetLocation::Linked` *is* -- the path and the state are one
                // value now, so a mirrored row cannot claim one without the
                // other.
                location: AssetLocation::Linked {
                    source_path: format!("{assets_root}/{rel}"),
                },
                file_name,
                ext,
                mime: probed.mime,
                size_bytes: size.max(0) as u64,
                // A stand-in digest, not a measurement: the fixture's own
                // `content_fingerprint` column is NULL and nothing here reads
                // the bytes. It only has to be stable and per-row unique.
                content_hash: Some(blake3::hash(rel.as_bytes()).to_hex().to_string()),
                kind: probed.kind,
                width: width.map(|v| v.max(0) as u32),
                height: height.map(|v| v.max(0) as u32),
                duration_ms: duration_ms.map(|v| v.max(0) as u64),
                captured_at: None,
                title: None,
                description: description.map(|d| d.chars().take(MAX_DESCRIPTION_LEN).collect()),
                // Serpent's unrated is 0; Trove's is NULL.
                rating: rating.filter(|v| *v > 0).map(|v| v as u8),
                is_favorite: favorite.unwrap_or(0) == 1,
                source_url: None,
                usage_status: UsageStatus::default(),
                commercial_use: None,
                facts: AssetFacts::default(),
                created_at: parse_time(&created_at),
                updated_at: parse_time(&updated_at),
                placement: trove_core::model::Placement::Live,
            });
            assets::insert(conn, &asset).unwrap();
            mirrored += 1;
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    steps.push(("asset rows".into(), t.elapsed().as_secs_f64()));

    let t = Instant::now();
    {
        let conn = &raw_conn(&lib.db_path());
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut stmt = src
            .prepare("SELECT collection_id, asset_id FROM collection_assets ORDER BY collection_id, position")
            .unwrap();
        let out = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for pair in out {
            let (collection_id, asset_id) = pair.unwrap();
            let (Ok(old_collection), Ok(asset_id)) =
                (Uuid::parse_str(&collection_id), Uuid::parse_str(&asset_id))
            else {
                continue;
            };
            let Some(new_collection) = collection_map.get(&old_collection).copied() else {
                continue;
            };
            collections::add_asset(conn, new_collection, asset_id).ok();
        }
        let mut stmt = src
            .prepare("SELECT asset_id, tag_id FROM human_asset_tags")
            .unwrap();
        let out = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for pair in out {
            let (asset_id, tag_id) = pair.unwrap();
            let (Ok(asset_id), Ok(tag_id)) = (Uuid::parse_str(&asset_id), Uuid::parse_str(&tag_id))
            else {
                continue;
            };
            let Some(new_tag) = tag_map.get(&tag_id).copied() else {
                continue;
            };
            tags::add_to_asset(conn, asset_id, new_tag).ok();
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    steps.push(("memberships".into(), t.elapsed().as_secs_f64()));

    let t = Instant::now();
    lib.drain_search_queue().unwrap();
    steps.push(("text index".into(), t.elapsed().as_secs_f64()));
    drop(lib);

    // Re-open so the store's statistics pass describes a full table; a query
    // phase timed against statistics gathered while it was empty measures the
    // planner guessing, not the library.
    let t = Instant::now();
    let reopened = Library::open(root, &cache).unwrap();
    steps.push(("reopen+statistics".into(), t.elapsed().as_secs_f64()));
    let docs = reopened.text_index().num_docs();
    drop(reopened);

    println!("  mirrored {mirrored} assets, {docs} index docs");
    std::fs::write(&marker, format!("{mirrored}\n")).unwrap();
    Some(steps)
}

// --- small helpers ---------------------------------------------------------

fn insert(json: &mut serde_json::Map<String, serde_json::Value>, key: &str, value: f64) {
    json.insert(key.into(), serde_json::json!(value));
}

fn parse_time(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .or_else(|_| DateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f%:z"))
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// `(one leaf folder, one root folder)` as source-path prefixes: the fixture
/// writes `Assets/Root-nn/Child-nn/asset-nnnnn.ext`, so a leaf is one folder's
/// own assets and a root is that folder plus its ten children — the same
/// non-recursive / recursive pair Serpent's bench asks for.
fn folder_prefixes(fixture: &Path) -> (String, String) {
    let base = fixture.join("Assets");
    let leaf = walk_first_file(&base)
        .and_then(|p| p.parent().map(|d| d.display().to_string()))
        .unwrap_or_else(|| base.display().to_string());
    (leaf, base.join("Root-00").display().to_string())
}

fn count_rows(lib: &Library) -> usize {
    raw_conn(&lib.db_path())
        .query_row("SELECT count(*) FROM assets", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as usize
}

fn count_serpent_rows(db: &Path) -> usize {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| c.query_row("SELECT count(*) FROM assets", [], |r| r.get::<_, i64>(0)))
        .unwrap_or(0) as usize
}

fn read_manifest_asset_count(manifest: &Path) -> usize {
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| {
            v.get("assetCount")
                .and_then(|n| n.as_u64())
                .map(|n| n as usize)
        })
        .unwrap_or(0)
}

fn triples(src: &Connection, sql: &str) -> Vec<(String, String, i64)> {
    let mut stmt = src.prepare(sql).unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

fn pairs(src: &Connection, sql: &str) -> Vec<(String, String)> {
    let mut stmt = src.prepare(sql).unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

fn round(value: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    (value * p).round() / p
}

fn walk_first_file(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn walk_count(dir: &Path) -> usize {
    let mut n = 0usize;
    walk(dir, &mut |_, _| n += 1);
    n
}

fn walk_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    walk(dir, &mut |_, size| total += size);
    total
}

fn dir_bytes(dir: &Path) -> u64 {
    if dir.exists() { walk_bytes(dir) } else { 0 }
}

fn walk(dir: &Path, visit: &mut dyn FnMut(&Path, u64)) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(md) = entry.metadata() else { continue };
            if md.is_dir() {
                stack.push(path);
            } else if md.is_file() {
                visit(&path, md.len());
            }
        }
    }
}
