//! Times the per-render work that surrounds `justify_layout` during a
//! resize: SQL query, per-asset `is_file()` stats, and Cell rebuilding.
use std::time::Instant;
use trove_core::model::{Asset, AssetKind, AssetQuery, AssetSort};
use trove_core::store::{self, assets};

fn bench(label: &str, n_assets: usize, runs: usize) {
    let dir = std::env::temp_dir().join(format!("trove-relayout-{label}-{n_assets}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = store::Store::open(&dir.join("lib.db")).unwrap();
    let conn = store.conn();

    let mut batch = Vec::new();
    for i in 0..n_assets {
        let a = Asset {
            id: uuid::Uuid::new_v4(),
            origin: trove_core::model::Origin::Linked,
            file_name: format!("photo-seventeen-{i:06}.jpg"),
            ext: "jpg".into(),
            mime: "image/jpeg".into(),
            size_bytes: 1_234_567,
            sha256: Some(format!("{i:064x}")),
            kind: AssetKind::Image,
            width: Some(1600),
            height: Some(1067),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            trashed_at: None,
            rel_path: None,
            duration_ms: None,
            captured_at: None,
            title: None,
            description: None,
            rating: None,
            is_favorite: false,
            source_url: None,
            color_label: None,
            extra: Default::default(),
        };
        batch.push(a);
    }
    let t = Instant::now();
    for a in &batch {
        assets::insert(conn, a).unwrap();
    }
    println!(
        "n={n_assets:>6}  insert={:>7.2?} (setup)",
        t.elapsed() / runs.max(1) as u32
    );

    let q = AssetQuery {
        sort: AssetSort::CreatedAt,
        sort_desc: true,
        limit: Some(n_assets as u32),
        ..Default::default()
    };

    // Time the SQL query alone.
    let _ = assets::query(conn, &q).unwrap();
    let mut times = Vec::new();
    for _ in 0..runs {
        let t = Instant::now();
        let (total, list) = assets::query(conn, &q).unwrap();
        times.push(t.elapsed());
        std::hint::black_box((total, list.len()));
    }
    times.sort();
    println!(
        "  sql query (limit {n_assets}): median {:>8.2?}",
        times[times.len() / 2]
    );

    // Time one stat() per asset, like `thumb::abs_path(..).is_file()` per cell.
    let mut paths = Vec::new();
    for i in 0..n_assets {
        let p = dir.join(format!("thumb-{i:06}.jpg"));
        std::fs::write(&p, b"x").unwrap();
        paths.push(p);
    }
    let _ = paths.iter().all(|p| p.is_file());
    let mut times = Vec::new();
    for _ in 0..runs {
        let t = Instant::now();
        let mut hit = 0usize;
        for p in &paths {
            if p.is_file() {
                hit += 1;
            }
        }
        times.push(t.elapsed());
        std::hint::black_box(hit);
    }
    times.sort();
    println!(
        "  {} stat() calls:        median {:>8.2?}",
        n_assets,
        times[times.len() / 2]
    );

    // Time the Cell-style per-asset rebuild (two date formats + strings).
    let list = assets::query(conn, &q).unwrap().1;
    let mut times = Vec::new();
    for _ in 0..runs {
        let t = Instant::now();
        let mut bytes: usize = 0;
        for a in &list {
            let name = a.file_name.clone();
            let added = a.created_at.format("%Y-%m-%d %H:%M").to_string();
            let day = a.created_at.format("%Y-%m-%d").to_string();
            bytes += name.len() + added.len() + day.len();
        }
        times.push(t.elapsed());
        std::hint::black_box(bytes);
    }
    times.sort();
    println!(
        "  cell rebuild x{n_assets}:      median {:>8.2?}",
        times[times.len() / 2]
    );

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    for n in [200, 1_000, 5_000] {
        bench("bench", n, 20);
    }
}
