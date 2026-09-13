//! Build a spatial index for a point-cloud PLY, and report what it cost.
//!
//! ```text
//! cargo run --release --example index_build -- cloud.ply [out.trovecloud]
//! ```
//!
//! The index is what makes a twenty-gigabyte cloud openable: it orders the
//! points along a space-filling curve and writes them chunk by chunk, so a
//! renderer can read a *region* instead of the file. Building it costs one
//! pass over the source (and temporary scratch space of a similar size);
//! afterwards the file is never read whole again.
//!
//! Release, not debug: the sort is the only CPU-bound part and it is worth
//! roughly an order of magnitude here.

use std::path::PathBuf;
use std::time::Instant;

use trove_core::media::index::{CloudIndex, IndexConfig, build_ply_index};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next().map(PathBuf::from) else {
        eprintln!("usage: index_build <cloud.ply> [out.trovecloud]");
        std::process::exit(2);
    };
    let out = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| input.with_extension("trovecloud"));
    let scratch = std::env::temp_dir().join("trove-index-scratch");

    let source_bytes = std::fs::metadata(&input).map(|m| m.len()).unwrap_or(0);
    println!(
        "source   {} ({:.1} MiB)",
        input.display(),
        source_bytes as f64 / (1 << 20) as f64
    );

    let started = Instant::now();
    let summary = match build_ply_index(&input, IndexConfig::default(), &scratch, &out) {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("index build failed: {error}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed();

    let points = summary.points.max(1);
    println!(
        "index    {} ({:.1} MiB, {:.0}% of the source, {:.1} bytes/point)",
        out.display(),
        summary.bytes as f64 / (1 << 20) as f64,
        summary.bytes as f64 / source_bytes.max(1) as f64 * 100.0,
        summary.bytes as f64 / points as f64
    );
    println!(
        "         {} points in {} chunks, colours {}, {} sorted runs spilled",
        summary.points, summary.chunks, summary.has_colors, summary.spilled_runs
    );
    println!(
        "build    {:.1} s ({:.1} Mpoints/s), scratch cleaned up",
        elapsed.as_secs_f64(),
        points as f64 / elapsed.as_secs_f64() / 1e6
    );

    // Read it back the way a renderer would: the header, then one chunk.
    let opened = Instant::now();
    match CloudIndex::open(&out) {
        Ok(index) => {
            let bounds = index.bounds();
            println!(
                "reopen   {:.1} ms, {} points, {} chunks, extent {:?} .. {:?}",
                opened.elapsed().as_secs_f64() * 1000.0,
                index.point_count(),
                index.chunk_count(),
                bounds.min,
                bounds.max
            );
            let middle = index.chunk_count() / 2;
            if let Some(chunk) = index.chunk(middle) {
                let read = Instant::now();
                match index.read_chunk(middle) {
                    Ok(batch) => println!(
                        "one chunk {}: {} points in {:.2} ms, box {:?} .. {:?}",
                        middle,
                        batch.points.len(),
                        read.elapsed().as_secs_f64() * 1000.0,
                        chunk.bounds.min,
                        chunk.bounds.max
                    ),
                    Err(error) => eprintln!("chunk read failed: {error}"),
                }
            }
        }
        Err(error) => eprintln!("index open failed: {error}"),
    }
}
