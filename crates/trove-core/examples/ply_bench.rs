//! PLY load benchmark.
//!
//! Times the three stages a PLY asset goes through in trove, separately:
//!
//! * `read`   — `std::fs::read`, the whole file into one `Vec<u8>`
//! * `parse`  — `mesh::load_ply`, bytes into a `Mesh`
//! * `render` — `render3d::render`, the 512x384 card the library grid shows
//!   (supersample 2, exactly what `thumb::write_model_card` asks for)
//!
//! `--mem` instead reports the peak resident set reached while loading one
//! file, which is what the memory column of the comparison wants. Run that
//! with a single file per process: `VmHWM` is a high-water mark, so it never
//! comes back down.
//!
//! ```text
//! cargo run --release -p trove-core --example ply_bench -- --iters 3 FILE...
//! cargo run --release -p trove-core --example ply_bench -- --mem FILE
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use trove_core::media::{mesh, render3d};

/// The card size `thumb::write_model_card` renders at.
const CARD: (u32, u32) = (512, 384);
/// The supersample factor `write_model_card` passes.
const SUPERSAMPLE: u32 = 2;

struct Options {
    iters: usize,
    render: bool,
    mem: bool,
    json: bool,
    files: Vec<PathBuf>,
}

fn parse_args() -> Options {
    let mut options = Options {
        iters: 3,
        render: true,
        mem: false,
        json: false,
        files: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--iters" => {
                options.iters = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(3);
            }
            "--no-render" => options.render = false,
            "--mem" => options.mem = true,
            "--json" => options.json = true,
            _ => options.files.push(PathBuf::from(arg)),
        }
    }
    options
}

/// The peak resident set the process has reached so far, in bytes.
fn peak_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn median(values: &[Duration]) -> Duration {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn minimum(values: &[Duration]) -> Duration {
    *values.iter().min().expect("non-empty")
}

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

struct Report {
    name: String,
    bytes: u64,
    primitives: usize,
    point_cloud: bool,
    read: Duration,
    parse: Duration,
    render: Option<Duration>,
    /// Parse time of the second run, if any: a warm run right after the first
    /// shows whether the allocator or cache is doing the heavy lifting.
    parse_min: Duration,
}

fn measure(path: &Path, options: &Options) -> Result<Report, String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    let file_bytes = std::fs::metadata(path).map_err(|e| e.to_string())?.len();

    // Warm-up: pull the file into the page cache, fault in the allocator and
    // let the branch predictors settle, so the first timed run is not the one
    // paying for all of it.
    {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let warm = mesh::load_ply(&bytes)?;
        std::hint::black_box(&warm);
    }

    let mut reads = Vec::with_capacity(options.iters);
    let mut parses = Vec::with_capacity(options.iters);
    let mut renders = Vec::new();
    let mut primitives = 0usize;
    let mut point_cloud = false;

    for _ in 0..options.iters {
        let started = Instant::now();
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        reads.push(started.elapsed());

        let started = Instant::now();
        let model = mesh::load_ply(&bytes)?;
        parses.push(started.elapsed());
        primitives = model.primitive_count();
        point_cloud = model.is_point_cloud();

        if options.render {
            let started = Instant::now();
            let frame = render3d::render(
                &model,
                &render3d::Camera::default(),
                CARD.0,
                CARD.1,
                SUPERSAMPLE,
                1.0,
            );
            renders.push(started.elapsed());
            std::hint::black_box(&frame);
        }

        std::hint::black_box(&bytes);
    }

    Ok(Report {
        name,
        bytes: file_bytes,
        primitives,
        point_cloud,
        read: median(&reads),
        parse: median(&parses),
        parse_min: minimum(&parses),
        render: options.render.then(|| median(&renders)),
    })
}

fn main() {
    let options = parse_args();
    if options.files.is_empty() {
        eprintln!("usage: ply_bench [--iters N] [--no-render] [--mem] [--json] FILE...");
        std::process::exit(2);
    }

    if options.mem {
        // One file per process: `VmHWM` is monotonic, so it can only answer
        // for whatever ran first.
        let path = &options.files[0];
        let before = peak_rss().unwrap_or(0);
        match std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| mesh::load_ply(&bytes))
        {
            Ok(model) => {
                let after = peak_rss().unwrap_or(0);
                println!(
                    "{} peak_rss={} delta={} file={} primitives={} point_cloud={}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    after,
                    after.saturating_sub(before),
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                    model.primitive_count(),
                    model.is_point_cloud(),
                );
            }
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                std::process::exit(1);
            }
        }
        return;
    }

    let mut reports = Vec::new();
    for path in &options.files {
        match measure(path, &options) {
            Ok(report) => reports.push(report),
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                std::process::exit(1);
            }
        }
    }

    if options.json {
        println!("[");
        for (i, r) in reports.iter().enumerate() {
            let comma = if i + 1 == reports.len() { "" } else { "," };
            println!(
                "  {{\"file\":\"{}\",\"size_bytes\":{},\"primitives\":{},\"point_cloud\":{},\
                 \"read_ms\":{:.3},\"parse_ms\":{:.3},\"parse_min_ms\":{:.3},\"render_ms\":{}}}{}",
                r.name,
                r.bytes,
                r.primitives,
                r.point_cloud,
                millis(r.read),
                millis(r.parse),
                millis(r.parse_min),
                r.render
                    .map(millis)
                    .map(|v| format!("{v:.3}"))
                    .unwrap_or("null".into()),
                comma
            );
        }
        println!("]");
        return;
    }

    println!(
        "\n{:<28} {:>9} {:>11} {:>7}  {:>10} {:>10} {:>10} {:>10}",
        "file", "size", "primitives", "kind", "read", "parse", "render", "total"
    );
    println!("{}", "-".repeat(103));
    for r in &reports {
        let total = r.read + r.parse + r.render.unwrap_or_default();
        println!(
            "{:<28} {:>9} {:>11} {:>7}  {:>10} {:>10} {:>10} {:>10}",
            r.name,
            human_bytes(r.bytes),
            r.primitives,
            if r.point_cloud { "cloud" } else { "mesh" },
            format!("{:.1} ms", millis(r.read)),
            format!("{:.1} ms", millis(r.parse)),
            r.render
                .map(|d| format!("{:.1} ms", millis(d)))
                .unwrap_or_else(|| "-".into()),
            format!("{:.1} ms", millis(total)),
        );
    }
    println!(
        "\nmedian of {} runs, page cache warm; render = the 512x384 card at \
         supersample 2\n",
        options.iters
    );
}
