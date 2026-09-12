//! Times `justify_layout` at realistic collection sizes, both code paths.
use trove_core::layout::justify_layout;

fn mixed_aspects(n: usize) -> Vec<f32> {
    let mut aspects = Vec::with_capacity(n);
    let mut seed = 7u64;
    for _ in 0..n {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bucket = (seed >> 33) as usize % 5;
        let base = match bucket {
            0 => 0.55,  // portrait
            1 => 1.0,   // square
            2 => 1.5,   // landscape
            3 => 2.2,   // wide
            _ => 0.707, // document (A-series)
        };
        let jitter = ((seed >> 40) as f32 / (1u64 << 24) as f32) * 0.25 - 0.125;
        aspects.push((base + jitter).clamp(0.05, 3.2));
    }
    aspects
}

fn bench(n: usize, content_width: f32) {
    let aspects = mixed_aspects(n);
    // Warm once (rayon pool spin-up, caches), then time 20 runs.
    let _ = justify_layout(&aspects, content_width);
    let mut times = Vec::new();
    for _ in 0..20 {
        let t = std::time::Instant::now();
        let rows = justify_layout(&aspects, content_width);
        times.push(t.elapsed());
        std::hint::black_box(&rows);
    }
    times.sort();
    let med = times[times.len() / 2];
    let worst = times[times.len() - 1];
    println!("n={n:>6}  median={med:>10.2?}  worst={worst:>10.2?}");
}

fn main() {
    for &n in &[200, 499, 500, 1_000, 5_000, 20_000, 100_000] {
        bench(n, 1440.0);
    }
}
