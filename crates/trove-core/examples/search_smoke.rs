//! End-to-end smoke for the Tantivy-backed full-text search.
//!
//! Unlike the unit tests (which use an in-RAM index), this drives the real
//! path: an on-disk [`Library`], so the index lives under
//! `<root>/search_index`, the `search_queue` triggers feed it, and every
//! query goes through the `Library::search_assets` facade. It also checks
//! the two self-repair paths (wiped index, stale version file).
//!
//!     cargo run -p trove-core --example search_smoke [--micro [N]] [--scale N] [--profile [N]]
//!
//! Exits non-zero if any expectation fails.

use std::path::PathBuf;
use std::time::Instant;

use chrono::Utc;
use trove_core::library::Library;
use trove_core::model::{
    Asset, AssetKind, AssetPatch, AssetQuery, NewTag, Orientation, Origin, UsageStatus,
};
use trove_core::store::{assets, tags};
use uuid::Uuid;

fn asset(name: &str, ext: &str, kind: AssetKind, title: Option<&str>, desc: Option<&str>) -> Asset {
    let id = Uuid::new_v4();
    Asset {
        id,
        origin: Origin::Stored,
        rel_path: Some(format!("media/{}/{}", &id.to_string()[..2], name)),
        file_name: name.to_string(),
        ext: ext.to_string(),
        mime: format!("application/{ext}"),
        size_bytes: 128,
        content_hash: Some(format!("{:0>64}", id.simple())),
        kind,
        width: None,
        height: None,
        duration_ms: None,
        captured_at: None,
        title: title.map(str::to_string),
        description: desc.map(str::to_string),
        rating: None,
        is_favorite: false,
        source_url: None,
        usage_status: UsageStatus::Unused,
        commercial_use: None,
        facts: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        trashed_at: None,
    }
}

fn tmp_root(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trove-search-smoke-{label}-{}", Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One query through the facade: total count plus the matching asset ids.
fn query(lib: &Library, q: &str, kind: Option<AssetKind>) -> (u64, Vec<Uuid>) {
    let page = lib
        .search_assets(
            q,
            &AssetQuery {
                kind,
                limit: Some(20),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("search {q:?} failed: {e}"));
    (page.total, page.items.iter().map(|a| a.id).collect())
}

fn hits(lib: &Library, q: &str) -> u64 {
    query(lib, q, None).0
}

/// Sorted ids, so order-insensitive comparisons read cleanly.
fn ids(lib: &Library, q: &str) -> Vec<Uuid> {
    let mut v = query(lib, q, None).1;
    v.sort();
    v
}

struct Report {
    failed: u32,
    passed: u32,
}

impl Report {
    fn check<T: std::fmt::Debug + PartialEq>(&mut self, case: &str, got: T, want: T) {
        if got == want {
            self.passed += 1;
            println!("  ok    {case:<46} -> {got:?}");
        } else {
            self.failed += 1;
            println!("  FAIL  {case:<46} -> got {got:?}, want {want:?}");
        }
    }

    fn section(&self, name: &str) {
        println!("\n== {name} ==");
    }
}

/// Populate the library and return the ids the assertions need.
struct Fixture {
    photo: Uuid,
    cat: Uuid,
    report: Uuid,
    cpp: Uuid,
    /// Only source of the word "ephemeral" is its title, so retitling it
    /// proves the stale document left the index.
    scratch: Uuid,
    tag_landscape: Uuid,
}

fn build(lib: &Library) -> Fixture {
    let conn = lib.store().conn();

    let mut photo = asset(
        "sunset-beach.jpg",
        "jpg",
        AssetKind::Image,
        Some("Sunset over the beach"),
        None,
    );
    photo.width = Some(1920);
    photo.height = Some(1080);

    let cat = asset(
        "花园里的猫.png",
        "png",
        AssetKind::Image,
        Some("A cat in the garden 花园里的猫"),
        None,
    );

    let mut report = asset(
        "quarterly-report.pdf",
        "pdf",
        AssetKind::Document,
        None,
        Some("Quarterly notes about sunset revenue"),
    );
    report.size_bytes = 4096;

    let cpp = asset(
        "notes.txt",
        "txt",
        AssetKind::Document,
        Some("C++ tips and (tricks)"),
        None,
    );

    let scratch = asset(
        "scratch.md",
        "md",
        AssetKind::Document,
        Some("Ephemeral keyword"),
        None,
    );

    for a in [&photo, &cat, &report, &cpp, &scratch] {
        assets::insert(conn, a).unwrap();
    }

    let tag_landscape = tags::create(
        conn,
        &NewTag {
            name: "landscape".into(),
            color: None,
            parent_id: None,
        },
    )
    .unwrap()
    .id;
    tags::add_to_asset(conn, photo.id, tag_landscape).unwrap();

    Fixture {
        photo: photo.id,
        cat: cat.id,
        report: report.id,
        cpp: cpp.id,
        scratch: scratch.id,
        tag_landscape,
    }
}

/// Attribute the cost of the index build: how much is the row-by-row SQLite
/// write, how much is the outbox delete, and how much is Tantivy itself.
///
/// Every measurement runs on its own library, freshly populated in one
/// transaction and starting from a full queue, so no experiment leaves state
/// that distorts the next one.
fn micro(n: usize) {
    let n = n.max(1);
    // The row-by-row experiments cost ~4 ms/row of fsync, so they only run at
    // the small default; the batch sweep below is the part that scales.
    let slow = n <= 2_500;
    println!(
        "trove search micro — {n} rows{}\n",
        if slow {
            ""
        } else {
            " (fsync-bound rows skipped)"
        }
    );
    let per = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / n as f64;
    let line = |label: &str, d: std::time::Duration| {
        println!(
            "  {label:<31} {:>9.1} ms   ({:.3} ms/row)",
            d.as_secs_f64() * 1000.0,
            per(d)
        );
    };
    let mut roots: Vec<PathBuf> = Vec::new();

    // -- the row write path -------------------------------------------------
    if slow {
        let root = tmp_root("micro-insert-auto");
        roots.push(root.clone());
        let lib = Library::open(&root, root.join("cache")).unwrap();
        let t = Instant::now();
        insert_rows(&lib, n, 0);
        line("insert rows, autocommit", t.elapsed());
        drop(lib);
    }

    let root = tmp_root("micro");
    roots.push(root.clone());
    let lib = Library::open(&root, root.join("cache")).unwrap();
    let t = Instant::now();
    insert_tx(&lib, n);
    line("insert rows, 1 transaction", t.elapsed());

    let conn = lib.store().conn();
    let ids = asset_ids(conn);
    assert_eq!(ids.len(), n);
    assert_eq!(queue_len(conn), n as i64, "one insert == one queued row");

    // -- emptying the outbox ------------------------------------------------
    // All start from the same full queue and end with it empty.
    if slow {
        let t = Instant::now();
        for id in &ids {
            conn.execute("DELETE FROM search_queue WHERE asset_id = ?1", [id])
                .unwrap();
        }
        line("queue DELETE, autocommit per row", t.elapsed());
        assert_eq!(queue_len(conn), 0);
    }

    refill_queue(conn);
    let t = Instant::now();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn
            .prepare("DELETE FROM search_queue WHERE asset_id = ?1")
            .unwrap();
        for id in &ids {
            stmt.execute([id]).unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    line("queue DELETE, 1 tx, per row", t.elapsed());
    assert_eq!(queue_len(conn), 0);

    refill_queue(conn);
    let rowids = queue_rowids(conn);
    let t = Instant::now();
    conn.execute_batch("BEGIN").unwrap();
    delete_rowids(conn, &rowids);
    conn.execute_batch("COMMIT").unwrap();
    line("queue DELETE, 1 tx, 1 statement", t.elapsed());
    assert_eq!(queue_len(conn), 0);
    drop(lib);

    // -- building the index -------------------------------------------------
    // Tantivy alone: no queue traffic at all. This is the floor the drain can
    // reach.
    let root = tmp_root("micro-index");
    roots.push(root.clone());
    let lib = Library::open(&root, root.join("cache")).unwrap();
    insert_tx(&lib, n);
    let conn = lib.store().conn();
    let t = Instant::now();
    for id in asset_ids(conn) {
        lib.text_index()
            .index_asset(conn, Uuid::parse_str(&id).unwrap())
            .unwrap();
    }
    lib.text_index().commit().unwrap();
    line("index only (no queue traffic)", t.elapsed());
    drop(lib);

    // The drain as it was written before the batching change: one
    // autocommitting DELETE per row.
    let mut before_docs = None;
    if slow {
        let root = tmp_root("micro-drain-old");
        roots.push(root.clone());
        let lib = Library::open(&root, root.join("cache")).unwrap();
        insert_tx(&lib, n);
        let t = Instant::now();
        drain_row_by_row(lib.store().conn(), lib.text_index()).unwrap();
        line("drain [before, per-row DELETE]", t.elapsed());
        assert_eq!(queue_len(lib.store().conn()), 0);
        before_docs = Some(lib.text_index().num_docs());
        drop(lib);
    }

    // The drain as it is now.
    let root = tmp_root("micro-drain-new");
    roots.push(root.clone());
    let lib = Library::open(&root, root.join("cache")).unwrap();
    insert_tx(&lib, n);
    let t = Instant::now();
    trove_core::search::drain(lib.store().conn(), lib.text_index()).unwrap();
    line("drain [after, batched DELETE]", t.elapsed());
    assert_eq!(queue_len(lib.store().conn()), 0);
    let docs = lib.text_index().num_docs();
    drop(lib);

    // Sweep the batch size. Batching the deletes removed the fsync-per-row; what
    // is left is Tantivy's commit, paid once per batch, so the batch size is
    // what is left to tune.
    for batch in [500usize, 2_000, 8_000] {
        if batch > n {
            continue;
        }
        let root = tmp_root(&format!("micro-drain-batch-{batch}"));
        roots.push(root.clone());
        let lib = Library::open(&root, root.join("cache")).unwrap();
        insert_tx(&lib, n);
        let t = Instant::now();
        drain_batched(lib.store().conn(), lib.text_index(), batch).unwrap();
        line(&format!("drain, batch={batch}"), t.elapsed());
        assert_eq!(queue_len(lib.store().conn()), 0);
        assert_eq!(lib.text_index().num_docs(), docs);
        drop(lib);
    }

    if let Some(expected) = before_docs {
        assert_eq!(docs, expected, "the old and new drains index the same docs");
    }
    assert_eq!(docs, n as u64, "every live asset is indexed exactly once");
    println!("  docs indexed                  {docs}");

    for r in roots {
        let _ = std::fs::remove_dir_all(r);
    }
}

/// Insert `n` synthetic assets inside one transaction (the autocommit path is
/// measured separately).
fn insert_tx(lib: &Library, n: usize) {
    let conn = lib.store().conn();
    conn.execute_batch("BEGIN").unwrap();
    insert_rows(lib, n, 0);
    conn.execute_batch("COMMIT").unwrap();
}

fn asset_ids(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = conn.prepare("SELECT id FROM assets").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn queue_rowids(conn: &rusqlite::Connection) -> Vec<i64> {
    let mut stmt = conn.prepare("SELECT rowid FROM search_queue").unwrap();
    stmt.query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn queue_len(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM search_queue", [], |r| r.get(0))
        .unwrap()
}

/// Empty the outbox and re-enqueue every asset, so the next experiment starts
/// from exactly one pending row per asset.
fn refill_queue(conn: &rusqlite::Connection) {
    conn.execute("DELETE FROM search_queue", []).unwrap();
    conn.execute(
        "INSERT INTO search_queue(asset_id, deleted) SELECT id, 0 FROM assets",
        [],
    )
    .unwrap();
}

/// `DELETE … WHERE rowid IN (…)` as one statement — what `search::drain` does
/// for a whole batch.
fn delete_rowids(conn: &rusqlite::Connection, rowids: &[i64]) {
    let mut sql = String::from("DELETE FROM search_queue WHERE rowid IN (");
    for i in 0..rowids.len() {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
    sql.push(')');
    conn.prepare(&sql)
        .unwrap()
        .execute(rusqlite::params_from_iter(rowids.iter().copied()))
        .unwrap();
}

/// `search::drain` with the batch size as a parameter, so the bench can sweep
/// it. Same algorithm as the real one.
fn drain_batched(
    conn: &rusqlite::Connection,
    index: &trove_core::search::TextIndex,
    batch: usize,
) -> trove_core::error::Result<()> {
    loop {
        let pending: Vec<(i64, String, bool)> = {
            let mut stmt = conn
                .prepare("SELECT rowid, asset_id, deleted FROM search_queue LIMIT ?1")
                .unwrap();
            stmt.query_map([batch as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
        };
        if pending.is_empty() {
            break;
        }
        let full = pending.len() == batch;
        let mut actions: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        for (_, id, deleted) in &pending {
            actions
                .entry(id.clone())
                .and_modify(|d| *d &= *deleted)
                .or_insert(*deleted);
        }
        for (id, deleted) in &actions {
            let id = Uuid::parse_str(id).unwrap();
            if *deleted {
                index.remove_asset(id).unwrap();
            } else {
                index.index_asset(conn, id)?;
            }
        }
        index.commit()?;
        let tx = conn.unchecked_transaction().unwrap();
        delete_rowids(
            &tx,
            &pending
                .iter()
                .map(|(rowid, _, _)| *rowid)
                .collect::<Vec<_>>(),
        );
        tx.commit().unwrap();
        if !full {
            break;
        }
    }
    Ok(())
}

/// The drain as it was written before the batching change — one autocommitting
/// DELETE per row, Tantivy commit at the end of each batch. Kept as the
/// "before" side of the comparison.
fn drain_row_by_row(
    conn: &rusqlite::Connection,
    index: &trove_core::search::TextIndex,
) -> trove_core::error::Result<()> {
    loop {
        let pending: Vec<(String, bool)> = {
            let mut stmt = conn
                .prepare("SELECT asset_id, deleted FROM search_queue LIMIT 500")
                .unwrap();
            stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
        };
        if pending.is_empty() {
            break;
        }
        let full = pending.len() == 500;
        for (id, deleted) in &pending {
            let id = Uuid::parse_str(id).unwrap();
            if *deleted {
                index.remove_asset(id).unwrap();
            } else {
                index.index_asset(conn, id)?;
            }
        }
        for (id, _) in &pending {
            conn.execute("DELETE FROM search_queue WHERE asset_id = ?1", [id])
                .unwrap();
        }
        index.commit()?;
        if !full {
            break;
        }
    }
    Ok(())
}

fn insert_rows(lib: &Library, n: usize, offset: usize) {
    let conn = lib.store().conn();
    for i in 0..n {
        let idx = i + offset;
        let name = if idx.is_multiple_of(7) {
            format!("风景-{idx}.jpg")
        } else {
            format!("photo-{idx:06}.jpg")
        };
        let title = match idx % 7 {
            0 => Some(format!("花园里的猫 {idx}")),
            3 => Some(format!("Sunset over the beach {idx}")),
            _ => None,
        };
        let mut a = asset(&name, "jpg", AssetKind::Image, title.as_deref(), None);
        if idx.is_multiple_of(5) {
            a.description = Some(format!("trip notes about sunset revenue {idx}"));
        }
        // Shape for the listing filters: a folder tree, real dimensions (so
        // orientation is decided by the same CASE the SQL uses), a rating and a
        // favourite flag. The source path is a full file path (the directories
        // panel groups by its parent), spread over 50 folders.
        a.facts.source_path = Some(format!(
            "/photos/2026/09/roll-{:02}/IMG_{idx:06}.jpg",
            idx % 50
        ));
        let (w, h) = match idx % 3 {
            0 => (1920, 1080),
            1 => (1080, 1920),
            _ => (1000, 1000),
        };
        a.width = Some(w);
        a.height = Some(h);
        if idx.is_multiple_of(4) {
            a.rating = Some((idx % 5) as u8);
        }
        a.is_favorite = idx.is_multiple_of(11);
        assets::insert(conn, &a).unwrap();
    }
}

/// Split a query's latency into the part Tantivy owns and the part the SQL
/// intersection owns, per query shape, at whatever scale is asked for.
///
/// The two halves have completely different fixes, so measuring them together
/// says nothing: the index side cares about how many documents a term touches,
/// the SQL side about how many ids came back to be filtered.
fn profile(n: usize) {
    const ROUNDS: usize = 5;
    let root = tmp_root("profile");
    let lib = Library::open(&root, root.join("cache")).unwrap();
    insert_tx(&lib, n);
    lib.drain_search_queue().unwrap();

    let queries = [
        "sunset",
        "sunset revenue",
        "beacho",
        "photo-0001",
        "花园",
        "猫",
        "mao",
        "hyldm",
        "notes tra",
    ];
    println!("trove query profile — {n} docs, best of {ROUNDS}\n");
    println!(
        "  {:<16} {:>8} {:>10} {:>11} {:>11} {:>9}",
        "query", "matches", "candidates", "index ms", "full ms", "sql ms"
    );
    let mut sum_index = 0.0f64;
    let mut sum_full = 0.0f64;
    for q in queries {
        let mut best_index = f64::MAX;
        let mut best_full = f64::MAX;
        let mut candidates = 0usize;
        let mut matches = 0u64;
        for _ in 0..ROUNDS {
            let t = Instant::now();
            let ids = lib
                .text_index()
                .search(q, trove_core::search::CANDIDATE_CAP)
                .unwrap();
            best_index = best_index.min(t.elapsed().as_secs_f64() * 1000.0);
            candidates = ids.len();

            let t = Instant::now();
            let (total, _) = query(&lib, q, None);
            best_full = best_full.min(t.elapsed().as_secs_f64() * 1000.0);
            matches = total;
        }
        let sql = (best_full - best_index).max(0.0);
        sum_index += best_index;
        sum_full += best_full;
        println!(
            "  {q:<16} {matches:>8} {candidates:>10} {best_index:>11.2} {best_full:>11.2} {sql:>9.2}"
        );
    }
    println!(
        "\n  totals                {:>10} {sum_index:>11.2} {sum_full:>11.2} {:>9.2}",
        "",
        (sum_full - sum_index).max(0.0)
    );

    // Why the intersection is slow, and which fix actually helps.
    //
    // The internal `rank=` timing is ~20 ms even for 97 candidates, and
    // `EXPLAIN QUERY PLAN` shows the reason: the planner drives off
    // `idx_assets_trashed`, whose `trashed_at IS NULL` arm matches every live
    // row. The `id IN (…)` list — the only selective term — is evaluated as a
    // filter over a near-full scan, so the step costs the same whether 97 or
    // 2000 candidates come back.
    //
    // Each candidate below re-runs the same intersection with the same real
    // condition, so the plans and timings are directly comparable.
    let conn = lib.store().conn();
    let ids = lib
        .text_index()
        .search("sunset", trove_core::search::CANDIDATE_CAP)
        .unwrap();

    fn plan_of(conn: &rusqlite::Connection, sql: &str, args: &[rusqlite::types::Value]) -> String {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .and_then(|mut s| {
                let rows = s.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    r.get::<_, String>(3)
                })?;
                Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>().join("; "))
            })
            .unwrap_or_else(|e| format!("<{e}>"))
    }

    fn bench(
        conn: &rusqlite::Connection,
        sql: &str,
        args: &[rusqlite::types::Value],
    ) -> (f64, usize) {
        let mut best = f64::MAX;
        let mut rows = 0;
        for _ in 0..5 {
            let t = Instant::now();
            let found: Vec<String> = {
                let mut stmt = conn.prepare(sql).unwrap();
                stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    r.get::<_, String>(0)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
            };
            rows = found.len();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        (best, rows)
    }

    let uuid_args = |ids: &[Uuid]| -> Vec<rusqlite::types::Value> {
        ids.iter()
            .map(|u| rusqlite::types::Value::Text(u.to_string()))
            .collect()
    };
    let marks = |n: usize| std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",");
    let values_rows = |n: usize| std::iter::repeat_n("(?)", n).collect::<Vec<_>>().join(",");

    println!(
        "\n  rank_intersect plan candidates (candidates = {})",
        ids.len()
    );
    println!("    {:<34} {:>8}  plan", "variant", "ms");

    for stats in [false, true] {
        if stats {
            conn.execute_batch("ANALYZE").unwrap();
            println!("    -- with sqlite_stat1 (ANALYZE) --");
        } else {
            println!("    -- no stats (as shipped) --");
        }
        for n in [97usize, ids.len()] {
            if n == 0 {
                continue;
            }
            let subset = &ids[..n.min(ids.len())];
            let args = uuid_args(subset);
            let live = format!(
                "SELECT id FROM assets WHERE trashed_at IS NULL AND id IN ({})",
                marks(n)
            );
            let mut variants: Vec<(&str, String, &[rusqlite::types::Value])> = vec![
                ("IN list (current)", live.clone(), &args),
                (
                    "IN list + INDEXED BY pk",
                    format!(
                        "SELECT id FROM assets INDEXED BY sqlite_autoindex_assets_1 \
                         WHERE trashed_at IS NULL AND id IN ({})",
                        marks(n)
                    ),
                    &args,
                ),
                (
                    "IN list + unary + on trashed",
                    format!(
                        "SELECT id FROM assets WHERE +trashed_at IS NULL AND id IN ({})",
                        marks(n)
                    ),
                    &args,
                ),
                (
                    "IN list, selective kind cond",
                    format!(
                        "SELECT id FROM assets WHERE trashed_at IS NULL AND kind = 'image' \
                         AND id IN ({})",
                        marks(n)
                    ),
                    &args,
                ),
                (
                    "CTE + CROSS JOIN (fixed order)",
                    format!(
                        "WITH cand(id) AS MATERIALIZED (VALUES {}) \
                         SELECT a.id FROM cand c CROSS JOIN assets a \
                         WHERE a.id = c.id AND a.trashed_at IS NULL",
                        values_rows(n)
                    ),
                    &args,
                ),
            ];
            if stats && n == ids.len() {
                variants.push((
                    "IN list, drop trashed cond",
                    { format!("SELECT id FROM assets WHERE id IN ({})", marks(n)) },
                    &args,
                ));
            }
            println!("    n={n}");
            for (label, sql, args) in &variants {
                let plan = plan_of(conn, sql, args);
                let (ms, rows) = bench(conn, sql, args);
                println!("    {label:<34} {ms:>8.2}  rows={rows}  {plan}");
            }
        }
    }

    // The listing path — the grid with filters and no search word — has its own
    // expensive shapes. These conditions either cannot use an index at all
    // (`json_extract`, the orientation CASE) or have one that barely narrows, so
    // each is a scan of `assets` plus per-row work. `count+page` is the same
    // query with the exact COUNT on top, which is what the grid asks for.
    println!("\n  listing filters (no text, {n} assets, limit 20)");
    println!(
        "    {:<34} {:>9} {:>9} {:>11}",
        "filter", "matches", "page ms", "count+page"
    );
    let base = || AssetQuery {
        limit: Some(20),
        ..Default::default()
    };
    let shapes: [(&str, AssetQuery); 7] = [
        ("none", base()),
        (
            "kind = image",
            AssetQuery {
                kind: Some(AssetKind::Image),
                ..base()
            },
        ),
        (
            "is_favorite",
            AssetQuery {
                is_favorite: Some(true),
                ..base()
            },
        ),
        (
            "rating >= 3",
            AssetQuery {
                min_rating: Some(3),
                ..base()
            },
        ),
        (
            "orientation = landscape",
            AssetQuery {
                orientation: Some(Orientation::Landscape),
                ..base()
            },
        ),
        (
            "ext = jpg",
            AssetQuery {
                ext: Some("jpg".into()),
                ..base()
            },
        ),
        (
            "folder prefix (1/5 of rows)",
            AssetQuery {
                source_path_prefix: Some("/photos/2026/09/roll-1".into()),
                ..base()
            },
        ),
    ];
    for (label, q) in &shapes {
        let t = Instant::now();
        let page = assets::query_without_count(conn, q).unwrap();
        let page_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let full = assets::query(conn, q).unwrap();
        let full_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!(
            "    {label:<34} {:>9} {page_ms:>9.2} {full_ms:>11.2}",
            full.total
        );
        assert_eq!(
            page.total,
            full.total.min(20),
            "{label}: the uncounted page must agree with the counted one"
        );
    }

    // Why the folder filter is the one that hurts: `json_extract` is an
    // expression over the `extra` JSON, so no index can drive it and every live
    // row pays a JSON parse.
    let folder_sql = "SELECT id FROM assets WHERE trashed_at IS NULL \
                      AND json_extract(assets.extra, '$.source_path') LIKE ?1 ESCAPE '\\'";
    let folder_arg = [rusqlite::types::Value::Text(
        "/photos/2026/09/roll-1%".into(),
    )];
    println!(
        "    folder plan: {}",
        plan_of(conn, folder_sql, &folder_arg)
    );

    // Where the listing COUNT's time goes. Every listing carries
    // `trashed_at IS NULL`, so the question is whether the planner can answer
    // the whole clause out of one index, or has to visit the main table (or
    // parse the `extra` JSON) once per live row. The bounded rows show what a
    // `LIMIT`-capped count would cost — that is the lever that would take the
    // COUNT off the UI thread's critical path entirely.
    println!("\n  listing COUNT anatomy ({n} assets)");
    let folder_like =
        "json_extract(assets.extra, '$.source_path') LIKE '/photos/2026/09/roll-1%' ESCAPE '\\'";
    let counts: [(&str, String); 7] = [
        (
            "trashed only",
            "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL".into(),
        ),
        (
            "trashed + kind",
            "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL AND kind = 'image'".into(),
        ),
        (
            "trashed + kind, bounded 1001",
            "SELECT COUNT(*) FROM (SELECT 1 FROM assets \
             WHERE trashed_at IS NULL AND kind = 'image' LIMIT 1001)"
                .into(),
        ),
        (
            "kind only (no trash guard)",
            "SELECT COUNT(*) FROM assets WHERE kind = 'image'".into(),
        ),
        (
            "trashed + LOWER(ext)",
            "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL AND LOWER(ext) = LOWER('jpg')"
                .into(),
        ),
        (
            "trashed + folder",
            format!("SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL AND {folder_like}"),
        ),
        (
            "trashed + folder, bounded 1001",
            format!(
                "SELECT COUNT(*) FROM (SELECT 1 FROM assets \
                 WHERE trashed_at IS NULL AND {folder_like} LIMIT 1001)"
            ),
        ),
    ];
    for (label, sql) in &counts {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let found: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(found);
        }
        println!(
            "    {label:<34} {best:>9.2} ms   {}",
            plan_of(conn, sql, &[])
        );
    }

    // Two panels read the database straight from `render`, so these are
    // per-frame costs rather than per-view-change ones. Whoever fixes them has
    // to know what they cost.
    println!("\n  per-render panel queries ({n} assets)");
    let t = Instant::now();
    let folders = assets::source_folders(conn).unwrap();
    println!(
        "    source_folders (FoldersPanel::render)  {:>9.2} ms   {} folders",
        t.elapsed().as_secs_f64() * 1000.0,
        folders.len()
    );

    // Where `source_folders` spends its time: the scan (a `json_extract` per
    // live row) versus the Rust tail (one `String` per asset, then
    // `Path::parent` and a map insert). Only the tail is ours to remove
    // without a schema change.
    let t = Instant::now();
    let mut bytes = 0usize;
    {
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(extra, '$.source_path') FROM assets \
                 WHERE trashed_at IS NULL AND json_extract(extra, '$.source_path') IS NOT NULL",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
        {
            bytes += row.len();
        }
    }
    let sql_only = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "    source_folders: SQL rows only          {:>9.2} ms   ({bytes} bytes read)",
        sql_only
    );

    const TAG_N: usize = 30;
    let tag_ids: Vec<Uuid> = (0..TAG_N)
        .map(|i| {
            tags::create(
                conn,
                &NewTag {
                    name: format!("bench-tag-{i:02}"),
                    color: None,
                    parent_id: None,
                },
            )
            .unwrap()
            .id
        })
        .collect();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn
            .prepare("INSERT INTO asset_tag (asset_id, tag_id) VALUES (?1, ?2)")
            .unwrap();
        for (i, id) in asset_ids(conn).iter().enumerate() {
            stmt.execute(rusqlite::params![id, tag_ids[i % TAG_N].to_string()])
                .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();

    let t = Instant::now();
    let total: u64 = tag_ids
        .iter()
        .map(|id| tags::count_assets(conn, *id).unwrap())
        .sum();
    let all_tags = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "    {TAG_N} tags: count_assets × {TAG_N}            {all_tags:>9.2} ms   ({:.3} ms/tag, {total} hits, TagsPanel::render)",
        all_tags / TAG_N as f64
    );

    // The batched replacement: one recursive CTE groups every tag's subtree at
    // once, so the panel reads the whole column in a single round trip instead
    // of N+1. It has to agree with `count_assets` exactly, or a row would show
    // a number the grid then contradicts.
    let t = Instant::now();
    let batched = tags::counts_by_tag(conn).unwrap();
    let batched_ms = t.elapsed().as_secs_f64() * 1000.0;
    let batched_total: u64 = batched.values().sum();
    println!(
        "    {TAG_N} tags: counts_by_tag (1 query)       {batched_ms:>9.2} ms   ({} rows, {batched_total} hits)",
        batched.len()
    );
    for id in &tag_ids {
        assert_eq!(
            batched.get(id).copied().unwrap_or(0),
            tags::count_assets(conn, *id).unwrap(),
            "counts_by_tag must agree with count_assets per tag"
        );
    }

    // The workspace toolbar is rebuilt on every frame and some of its pieces
    // read the store (and, in one case, the config *file*) right there. These
    // are the costs a per-frame caller would pay for each.
    println!("\n  per-render toolbar reads ({n} assets)");
    let t = Instant::now();
    let exts = assets::distinct_exts(conn).unwrap();
    println!(
        "    distinct_exts (format filter)          {:>9.3} ms   {} exts",
        t.elapsed().as_secs_f64() * 1000.0,
        exts.len()
    );
    // `distinct_exts` lowercases inside the query, which is what stops the
    // plain `idx_assets_ext` from covering it. Three rewrites, same answer:
    // the first two keep the case handling in the query, the last moves it to
    // Rust (which then has to de-duplicate too).
    //
    // Measured twice, because the planner's choice here depends on
    // `sqlite_stat1`: the run above has already ANALYZEd, and a shipped
    // library has not.
    let ext_variants: [(&str, &str); 3] = [
        (
            "LOWER(ext) DISTINCT (current)",
            "SELECT DISTINCT LOWER(ext) FROM assets \
             WHERE trashed_at IS NULL AND ext != '' ORDER BY LOWER(ext)",
        ),
        (
            "GROUP BY LOWER(ext)",
            "SELECT LOWER(ext) FROM assets \
             WHERE trashed_at IS NULL AND ext != '' GROUP BY LOWER(ext) ORDER BY 1",
        ),
        (
            "DISTINCT ext (lowercase in Rust)",
            "SELECT DISTINCT ext FROM assets \
             WHERE trashed_at IS NULL AND ext != '' ORDER BY ext",
        ),
    ];
    for round in ["no stats (as shipped)", "with sqlite_stat1 (ANALYZE)"] {
        if round.starts_with("no") {
            let _ = conn.execute_batch("DELETE FROM sqlite_stat1");
        } else {
            conn.execute_batch("ANALYZE").unwrap();
        }
        println!("      -- {round} --");
        for (label, sql) in ext_variants {
            let mut best = f64::MAX;
            let mut n = 0usize;
            for _ in 0..5 {
                let t = Instant::now();
                let mut stmt = conn.prepare(sql).unwrap();
                let got = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .count();
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
                n = got;
            }
            println!(
                "      {label:<32} {best:>9.3} ms   rows={n}   {}",
                plan_of(conn, sql, &[])
            );
        }
    }
    // The variants above run against a library where every asset shares one
    // extension, which is the cheapest possible case for an index-ordered
    // `DISTINCT` (one table lookup, and SQLite can seek past the whole run of
    // equal keys). Re-check with a realistic spread, in a scratch table so the
    // rest of the profile keeps its shape.
    conn.execute_batch(
        "CREATE TEMP TABLE ext_probe (ext TEXT NOT NULL, trashed_at TEXT);
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 100000)
         INSERT INTO ext_probe
         SELECT CASE i % 12
                  WHEN 0 THEN 'jpg'  WHEN 1 THEN 'png'  WHEN 2 THEN 'gif'
                  WHEN 3 THEN 'webp' WHEN 4 THEN 'bmp'  WHEN 5 THEN 'tiff'
                  WHEN 6 THEN 'svg'  WHEN 7 THEN 'avif' WHEN 8 THEN 'heic'
                  WHEN 9 THEN 'jxl'  WHEN 10 THEN 'psd' ELSE 'nef'
                END,
                NULL
         FROM c;
         CREATE INDEX ext_probe_ext ON ext_probe(ext);",
    )
    .unwrap();
    println!("      -- 100k rows / 12 distinct exts (scratch table) --");
    // The plan has to be captured next to the timing: `plan_of` reads the
    // schema as it is *now*, so asking for it after the third index exists
    // would report that index for all three rows.
    let probe = |sql: &str| -> (f64, usize, String) {
        let mut best = f64::MAX;
        let mut n = 0usize;
        for _ in 0..5 {
            let t = Instant::now();
            let mut stmt = conn.prepare(sql).unwrap();
            let got = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .count();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            n = got;
        }
        (best, n, plan_of(conn, sql, &[]))
    };
    let current_sql = "SELECT DISTINCT LOWER(ext) FROM ext_probe \
                       WHERE trashed_at IS NULL AND ext != '' ORDER BY LOWER(ext)";
    let bare_sql = "SELECT DISTINCT ext FROM ext_probe \
                    WHERE trashed_at IS NULL AND ext != '' ORDER BY ext";
    let current = probe(current_sql);
    let bare = probe(bare_sql);
    // The shipped schema has no such index; this is the migration candidate.
    conn.execute_batch("CREATE INDEX ext_probe_live ON ext_probe(ext) WHERE trashed_at IS NULL")
        .unwrap();
    let live = probe(bare_sql);
    for (label, (ms, n, plan)) in [
        ("LOWER(ext) DISTINCT (current)", current),
        ("DISTINCT ext (lowercase in Rust)", bare),
        ("DISTINCT ext + live-only index", live),
    ] {
        println!("      {label:<32} {ms:>9.3} ms   rows={n}   {plan}");
    }
    conn.execute_batch("DROP TABLE ext_probe").unwrap();

    let t = Instant::now();
    let all = tags::list(conn).unwrap();
    println!(
        "    tags::list (tag filter)                {:>9.3} ms   {} tags",
        t.elapsed().as_secs_f64() * 1000.0,
        all.len()
    );
    let ids20: Vec<Uuid> = asset_ids(conn)
        .into_iter()
        .take(20)
        .filter_map(|s| Uuid::parse_str(&s).ok())
        .collect();
    let t = Instant::now();
    let listed = assets::by_ids(conn, &ids20).unwrap();
    println!(
        "    by_ids × 20 (selection toolbar)        {:>9.3} ms   {} rows",
        t.elapsed().as_secs_f64() * 1000.0,
        listed.len()
    );
    // Config reads are file I/O plus a JSON parse. `AppConfig::load()` is
    // called from the toolbar on every frame; the font sample text already has
    // a TTL cache for exactly this reason, the toolbar does not.
    let t = Instant::now();
    const CFG_ITERS: usize = 200;
    for _ in 0..CFG_ITERS {
        std::hint::black_box(trove_core::config::AppConfig::load().filter_tools());
    }
    println!(
        "    AppConfig::load() + filter_tools       {:>9.3} ms/次  ({} iters)",
        t.elapsed().as_secs_f64() * 1000.0 / CFG_ITERS as f64,
        CFG_ITERS
    );

    // What a schema migration could buy for the listing path. Every listing
    // clause carries `trashed_at IS NULL`, but the single-column indexes cover
    // the filter column *alone* — so the planner either scans `assets` or
    // probes an index that matches every live row anyway. A *partial* index on
    // the filter column, restricted to the live set, is the shape that matches
    // how trove actually queries. Created here only to measure, then dropped.
    println!("\n  listing with live-only partial indexes ({n} assets)");
    println!(
        "    {:<30} {:>9} {:>13}",
        "filter", "matches", "page / count ms"
    );
    let probe: [(&str, AssetQuery); 4] = [
        ("none (trashed only)", base()),
        (
            "kind = image",
            AssetQuery {
                kind: Some(AssetKind::Image),
                ..base()
            },
        ),
        (
            "is_favorite",
            AssetQuery {
                is_favorite: Some(true),
                ..base()
            },
        ),
        (
            "rating >= 3",
            AssetQuery {
                min_rating: Some(3),
                ..base()
            },
        ),
    ];
    let measure = |q: &AssetQuery| -> (u64, f64, f64) {
        let t = Instant::now();
        let page = assets::query_without_count(conn, q).unwrap();
        let page_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let full = assets::query(conn, q).unwrap();
        let full_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(
            page.total,
            full.total.min(20),
            "the uncounted page must agree with the counted one"
        );
        (full.total, page_ms, full_ms)
    };
    let before: Vec<(u64, f64, f64)> = probe.iter().map(|(_, q)| measure(q)).collect();
    // Does the planner want stats? Without `sqlite_stat1` it has to guess from
    // index structure alone, which is what makes the rank path need its `+`
    // prefixes. Timed here because it is a write-time cost, paid once, and the
    // partial indexes below are invisible to the planner without it.
    let t = Instant::now();
    conn.execute_batch("ANALYZE").unwrap();
    println!(
        "    ANALYZE on {n} assets: {:.2} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );
    conn.execute_batch(
        "BEGIN; \
         CREATE INDEX bench_live_kind     ON assets(kind)        WHERE trashed_at IS NULL; \
         CREATE INDEX bench_live_favorite ON assets(is_favorite) WHERE trashed_at IS NULL; \
         CREATE INDEX bench_live_rating   ON assets(rating)      WHERE trashed_at IS NULL; \
         COMMIT;",
    )
    .unwrap();
    conn.execute_batch("ANALYZE").unwrap();
    let after: Vec<(u64, f64, f64)> = probe.iter().map(|(_, q)| measure(q)).collect();
    for (ix, (label, _)) in probe.iter().enumerate() {
        let (total, page_before, full_before) = before[ix];
        let (_, page_after, full_after) = after[ix];
        println!(
            "    {label:<30} {total:>9}  {page_before:>6.2}→{page_after:<6.2} {full_before:>7.2}→{full_after:<6.2}"
        );
    }
    for (label, sql) in [
        (
            "live + kind",
            "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL AND kind = 'image'",
        ),
        (
            "live + favorite",
            "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL AND is_favorite = 1",
        ),
    ] {
        println!("    {label:<30} plan: {}", plan_of(conn, sql, &[]));
    }
    conn.execute_batch(
        "DROP INDEX bench_live_kind; \
         DROP INDEX bench_live_favorite; \
         DROP INDEX bench_live_rating;",
    )
    .unwrap();

    let _ = std::fs::remove_dir_all(&root);
}

fn scale(n: usize) {
    println!("trove search scale run — {n} assets");
    let root = tmp_root("scale");

    let t0 = Instant::now();
    let lib = Library::open(&root, root.join("cache")).unwrap();
    // One transaction, like the import job's batching — the row-by-row SQL cost
    // is what `--micro` measures; here the drain is the subject.
    insert_tx(&lib, n);
    let insert = t0.elapsed();

    let t1 = Instant::now();
    lib.drain_search_queue().unwrap();
    let build = t1.elapsed();

    let queries = [
        "sunset",
        "sunset revenue",
        "beacho",
        "photo-0001",
        "花园",
        "猫",
        "mao",
        "hyldm",
        "notes tra",
    ];
    // One warm-up round (drains the queue's tail, warms the searcher).
    for q in queries {
        let _ = query(&lib, q, None);
    }
    let mut worst = 0.0f64;
    let mut total = 0.0f64;
    let mut runs = 0u32;
    let mut last_totals = Vec::new();
    for _ in 0..10 {
        last_totals.clear();
        for q in queries {
            let t = Instant::now();
            let (hits, _) = query(&lib, q, None);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            worst = worst.max(ms);
            total += ms;
            runs += 1;
            last_totals.push(hits);
        }
    }

    let bytes = dir_size(&root.join("search_index"));
    println!(
        "  insert {n} rows (1 tx)  {:>10.1} ms",
        insert.as_secs_f64() * 1000.0
    );
    println!(
        "  index build (drain)     {:>10.1} ms   ({:.0} docs/s)",
        build.as_secs_f64() * 1000.0,
        n as f64 / build.as_secs_f64()
    );
    println!(
        "  query latency           avg {:>7.2} ms   worst {:>7.2} ms   ({} queries)",
        total / runs as f64,
        worst,
        runs
    );
    println!(
        "  index size on disk      {:>10.1} MB",
        bytes as f64 / 1_048_576.0
    );
    println!("  hits per query          {last_totals:?}");
    let _ = std::fs::remove_dir_all(&root);
}

fn dir_size(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let md = e.metadata();
                    match md {
                        Ok(m) if m.is_file() => m.len(),
                        _ => 0,
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(pos) = args.iter().position(|a| a == "--scale") {
        let n = args
            .get(pos + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000);
        scale(n);
        return;
    }
    if let Some(pos) = args.iter().position(|a| a == "--profile") {
        let n = args
            .get(pos + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000);
        profile(n);
        return;
    }
    if let Some(pos) = args.iter().position(|a| a == "--micro") {
        let n = args
            .get(pos + 1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(2_000);
        micro(n);
        return;
    }
    smoke();
}

fn smoke() {
    let root = tmp_root("main");
    let mut r = Report {
        failed: 0,
        passed: 0,
    };
    let mut lib = Library::open(&root, root.join("cache")).unwrap();
    let fx = build(&lib);

    println!("trove full-text search smoke — root {}", root.display());

    // --- term / ranking -----------------------------------------------------
    r.section("terms and ranking");
    r.check(r#""sunset" (word, 2 docs)"#, hits(&lib, "sunset"), 2);
    r.check(
        r#""sunset" resolves to photo + report"#,
        ids(&lib, "sunset"),
        {
            let mut v = vec![fx.photo, fx.report];
            v.sort();
            v
        },
    );
    r.check(r#""sunset beach" (AND)"#, hits(&lib, "sunset beach"), 1);
    r.check(
        r#""sunset revenue" (AND, spans fields)"#,
        hits(&lib, "sunset revenue"),
        1,
    );
    r.check(r#""rainy" (absent)"#, hits(&lib, "rainy"), 0);
    r.check(
        r#""  SUNSET  " (case + padding)"#,
        hits(&lib, "  SUNSET  "),
        2,
    );

    // --- substring (gram field) --------------------------------------------
    r.section("substring matching");
    r.check(r#""unse" (infix both docs)"#, hits(&lib, "unse"), 2);
    r.check(r#""each" (infix)"#, hits(&lib, "each"), 1);
    r.check(
        r#""arterly-rep" (infix, file name)"#,
        hits(&lib, "arterly-rep"),
        1,
    );

    // --- fuzzy (typo tolerance) --------------------------------------------
    r.section("typo tolerance");
    r.check(r#""beacho" (1 edit from beach)"#, hits(&lib, "beacho"), 1);
    r.check(r#""beacch""#, hits(&lib, "beacch"), 1);

    // --- CJK ----------------------------------------------------------------
    r.section("CJK");
    r.check(r#""猫" (single hanzi)"#, ids(&lib, "猫"), vec![fx.cat]);
    r.check(r#""园里" (two hanzi)"#, hits(&lib, "园里"), 1);
    r.check(
        r#""花园里" (three hanzi, in text)"#,
        hits(&lib, "花园里"),
        1,
    );
    r.check(r#""里的猫" (hanzi tail)"#, hits(&lib, "里的猫"), 1);
    r.check(
        r#""花园的" (hanzi gap, not a substring)"#,
        hits(&lib, "花园的"),
        0,
    );
    r.check(r#""花猫" (non-adjacent hanzi)"#, hits(&lib, "花猫"), 0);

    // --- pinyin -------------------------------------------------------------
    r.section("pinyin");
    r.check(r#""mao" (full syllable)"#, ids(&lib, "mao"), vec![fx.cat]);
    r.check(r#""hua" (single syllable)"#, hits(&lib, "hua"), 1);
    r.check(
        r#""hua yuan" (two syllables, AND)"#,
        hits(&lib, "hua yuan"),
        1,
    );
    r.check(r#""hyldm" (full initials)"#, hits(&lib, "hyldm"), 1);
    r.check(r#""hyl" (initials prefix)"#, hits(&lib, "hyl"), 1);
    // Known limitation, not a bug to fix here: the pinyin field holds one
    // token per syllable, so a run-together query has no token to match.
    r.check(
        r#""huayuan" (concatenated - NOT supported)"#,
        hits(&lib, "huayuan"),
        0,
    );
    r.check(
        r#""huayuanlide" (concatenated - NOT supported)"#,
        hits(&lib, "huayuanlide"),
        0,
    );

    // --- tags ---------------------------------------------------------------
    r.section("tags");
    r.check(r#""landscape" (tag name)"#, hits(&lib, "landscape"), 1);
    r.check(r#""lands" (tag prefix)"#, hits(&lib, "lands"), 1);
    r.check(
        r#""landscape sunset" (tag AND word)"#,
        hits(&lib, "landscape sunset"),
        1,
    );
    r.check(
        r#""landscape revenue" (tag AND other doc)"#,
        hits(&lib, "landscape revenue"),
        0,
    );

    // --- literal syntax characters -----------------------------------------
    r.section("syntax characters stay literal");
    for q in [
        "c*",
        "\"c*\"",
        "NEAR",
        "c AND tips",
        "c OR (tips)",
        "\"",
        "*",
        "--",
    ] {
        r.check(&format!("{q:?} matches nothing"), hits(&lib, q), 0);
    }
    r.check(r#""tips" still finds the doc"#, hits(&lib, "tips"), 1);
    r.check(r#""c++" literal plus signs"#, hits(&lib, "c++"), 1);

    // --- filter interaction -------------------------------------------------
    r.section("filters");
    r.check(
        r#""sunset" + kind=Image"#,
        query(&lib, "sunset", Some(AssetKind::Image)).0,
        1,
    );
    r.check(
        r#""sunset" + kind=Document"#,
        query(&lib, "sunset", Some(AssetKind::Document)).0,
        1,
    );
    r.check(
        r#""sunset" + kind=Audio"#,
        query(&lib, "sunset", Some(AssetKind::Audio)).0,
        0,
    );
    r.check("blank query is empty", hits(&lib, "   "), 0);

    // --- mutation stays in sync (outbox triggers + drain) -------------------
    r.section("mutations stay in sync");
    assets::set_trashed(lib.store().conn(), fx.photo, true).unwrap();
    r.check(
        r#""sunset" after trashing the photo"#,
        hits(&lib, "sunset"),
        1,
    );
    assets::set_trashed(lib.store().conn(), fx.photo, false).unwrap();
    r.check(r#""sunset" after restore"#, hits(&lib, "sunset"), 2);

    tags::rename(lib.store().conn(), fx.tag_landscape, "seaside").unwrap();
    r.check(
        r#""landscape" after tag rename"#,
        hits(&lib, "landscape"),
        0,
    );
    r.check(r#""seaside" after tag rename"#, hits(&lib, "seaside"), 1);
    tags::rename(lib.store().conn(), fx.tag_landscape, "landscape").unwrap();

    let retitle = AssetPatch {
        title: Some(Some("Replaced keyword".into())),
        ..Default::default()
    };
    assets::update(lib.store().conn(), fx.scratch, &retitle).unwrap();
    r.check(
        r#""ephemeral" gone after retitling"#,
        hits(&lib, "ephemeral"),
        0,
    );
    r.check(
        r#""replaced" indexed after retitling"#,
        hits(&lib, "replaced"),
        1,
    );

    let retitle = AssetPatch {
        title: Some(Some("Winter mountains".into())),
        ..Default::default()
    };
    assets::update(lib.store().conn(), fx.report, &retitle).unwrap();
    r.check(
        r#""mountains" after title edit"#,
        hits(&lib, "mountains"),
        1,
    );
    r.check(
        r#""sunset" survives in the description"#,
        hits(&lib, "sunset"),
        2,
    );
    let clear = AssetPatch {
        title: Some(None),
        ..Default::default()
    };
    assets::update(lib.store().conn(), fx.report, &clear).unwrap();

    assets::delete(lib.store().conn(), fx.cpp).unwrap();
    r.check(r#""tips" after purging the doc"#, hits(&lib, "tips"), 0);

    // --- persistence --------------------------------------------------------
    r.section("on-disk index");
    drop(lib);
    lib = Library::open(&root, root.join("cache")).unwrap();
    r.check(r#""sunset" survives a reopen"#, hits(&lib, "sunset"), 2);
    r.check(r#""猫" survives a reopen"#, hits(&lib, "猫"), 1);
    r.check(
        "index dir exists on disk",
        root.join("search_index").is_dir(),
        true,
    );
    r.check(
        "version file written",
        std::fs::read_to_string(root.join("search_index/trove-index-version"))
            .unwrap_or_default()
            .trim()
            .to_string(),
        "2".to_string(),
    );

    // --- self repair --------------------------------------------------------
    r.section("self repair");
    drop(lib);
    std::fs::remove_dir_all(root.join("search_index")).unwrap();
    lib = Library::open(&root, root.join("cache")).unwrap();
    r.check(
        r#"wiped index rebuilds ("sunset")"#,
        hits(&lib, "sunset"),
        2,
    );
    r.check(r#"wiped index rebuilds ("猫")"#, hits(&lib, "猫"), 1);

    drop(lib);
    std::fs::write(root.join("search_index/trove-index-version"), "1").unwrap();
    lib = Library::open(&root, root.join("cache")).unwrap();
    r.check(
        r#"stale version file rebuilds ("sunset")"#,
        hits(&lib, "sunset"),
        2,
    );

    // --- empty library ------------------------------------------------------
    r.section("empty library");
    let empty_root = tmp_root("empty");
    let empty = Library::open(&empty_root, empty_root.join("cache")).unwrap();
    r.check("no assets -> no hits", hits(&empty, "sunset"), 0);
    drop(empty);

    println!("\n{} passed, {} failed", r.passed, r.failed);
    std::process::exit(if r.failed == 0 { 0 } else { 1 });
}
