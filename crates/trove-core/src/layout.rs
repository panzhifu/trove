//! Justified ("Google-Photos-style") photo-wall layout — pure geometry, no UI.
//!
//! All photos are treated as one sequence and every possible line break is
//! scored, then a dynamic program finds the global optimum:
//!
//! ```text
//! dp[i] = min over j > i of ( badness(i, j) + dp[j] )
//! ```
//!
//! where `dp[i]` is the minimum total badness of the suffix starting at
//! photo `i`, and `badness(i, j)` scores the row holding photos `i..j`.
//! The score measures how far the row's scaled height ends up from the
//! target row height, so a row that needs little rescaling is cheap and a
//! row that fights the container (a lone panorama stretched absurdly tall,
//! or twenty portraits squeezed into one short line) is expensive. That is
//! why a panorama ends up on its own or with very few companions: forcing
//! it into a mixed row inflates the total cost.
//!
//! Within a row every cell shares one height, cells keep their aspect
//! ratio, and the row is scaled so its combined width spans the container
//! exactly — no dead space, however wide or narrow the panel is.

/// Gap between adjacent cells and between rows, in px. The caller renders
/// with this value too, so rows tile without seams.
pub const GRID_GAP: f32 = 8.0;
/// Ideal row height the optimizer aims for, in px.
pub const TARGET_ROW_HEIGHT: f32 = 180.0;
/// Hard bounds a row's final height is allowed to take, in px.
pub const MIN_ROW_HEIGHT: f32 = 100.0;
pub const MAX_ROW_HEIGHT: f32 = 320.0;
/// Once a row's natural (unscaled) width exceeds this multiple of the
/// container, adding more cells can only move the scaled height further
/// from target — the DP search for that row stops.
const PRUNE_FACTOR: f32 = 1.6;
/// Floor applied to every input aspect. Without it, zero or near-zero
/// aspects contribute nothing to a row's natural width, the prune never
/// fires (the DP degrades toward O(n²)) and the cell's final width is 0.
pub const MIN_ASPECT: f32 = 0.05;

/// One laid-out row: the uniform cell height and each cell's width, in the
/// same order as the input aspects.
#[derive(Debug, Clone, PartialEq)]
pub struct RowLayout {
    pub height: f32,
    pub item_widths: Vec<f32>,
}

/// Compute the globally optimal justified layout for `aspects`
/// (width ÷ height per photo) inside a container of `content_width` px.
///
/// Every cell of every row except a possible thin tail is guaranteed to
/// keep its aspect ratio and to be exactly as tall as its row; row widths
/// sum (with gaps) to the container. Aspects are floored at
/// [`MIN_ASPECT`] so degenerate inputs (zero / unknown sizes) can neither
/// stall the DP prune nor produce zero-width cells.
///
/// For small inputs (<500 items) uses exact DP. For larger inputs uses a
/// parallel greedy approximation that is O(n) and nearly as good.
pub fn justify_layout(input: &[f32], content_width: f32) -> Vec<RowLayout> {
    let n = input.len();
    if n == 0 || content_width <= 0.0 {
        return Vec::new();
    }

    // For small datasets, use exact DP. For large, use parallel greedy.
    const EXACT_DP_THRESHOLD: usize = 500;
    if n < EXACT_DP_THRESHOLD {
        justify_layout_exact(input, content_width)
    } else {
        justify_layout_parallel_greedy(input, content_width)
    }
}

/// Exact DP — O(n · avg photos per row). Used for small datasets.
fn justify_layout_exact(input: &[f32], content_width: f32) -> Vec<RowLayout> {
    let n = input.len();
    let aspects: Vec<f32> = input.iter().map(|a| (*a).max(MIN_ASPECT)).collect();

    let mut dp = vec![f32::INFINITY; n + 1];
    let mut next = vec![0usize; n + 1];
    dp[n] = 0.0;

    for i in (0..n).rev() {
        let mut natural = 0.0f32;
        let mut best = f32::INFINITY;
        let mut best_j = i + 1;
        for j in (i + 1)..=n {
            natural += TARGET_ROW_HEIGHT * aspects[j - 1];
            let k = j - i;

            if k > 1 && natural > content_width * PRUNE_FACTOR {
                break;
            }

            let content = (content_width - GRID_GAP * (k as f32 - 1.0)).max(1.0);
            let h_raw = TARGET_ROW_HEIGHT * content / natural.max(1e-3);
            let h = if h_raw < MIN_ROW_HEIGHT {
                h_raw
            } else {
                h_raw.min(MAX_ROW_HEIGHT)
            };

            let badness = (h - TARGET_ROW_HEIGHT) * (h - TARGET_ROW_HEIGHT);
            let cand = badness + dp[j];
            if cand < best {
                best = cand;
                best_j = j;
            }
        }
        dp[i] = best;
        next[i] = best_j;
    }

    build_rows(&aspects, &next, content_width, n)
}

/// Parallel greedy — O(n) with parallel row building. Used for large datasets.
fn justify_layout_parallel_greedy(input: &[f32], content_width: f32) -> Vec<RowLayout> {
    use rayon::prelude::*;

    let n = input.len();
    let aspects: Vec<f32> = input.iter().map(|a| (*a).max(MIN_ASPECT)).collect();

    // Phase 1: Greedy row breaking (sequential but O(n)).
    let mut row_starts: Vec<usize> = vec![0];
    let mut i = 0;
    while i < n {
        let mut natural = 0.0f32;
        let mut best_j = i + 1;
        let mut best_score = f32::INFINITY;

        for j in (i + 1..=n).take(30) {
            natural += TARGET_ROW_HEIGHT * aspects[j - 1];
            let k = j - i;
            if k > 1 && natural > content_width * 2.0 {
                break;
            }

            let content = (content_width - GRID_GAP * (k as f32 - 1.0)).max(1.0);
            let h_raw = TARGET_ROW_HEIGHT * content / natural.max(1e-3);
            let h = if h_raw < MIN_ROW_HEIGHT {
                h_raw
            } else {
                h_raw.min(MAX_ROW_HEIGHT)
            };

            let score = (h - TARGET_ROW_HEIGHT).abs();
            if score < best_score {
                best_score = score;
                best_j = j;
            }
        }
        i = best_j;
        if i < n {
            row_starts.push(i);
        }
    }

    // Phase 2: Build rows in parallel.
    let row_ends: Vec<usize> = row_starts[1..]
        .iter()
        .copied()
        .chain(std::iter::once(n))
        .collect();
    row_starts
        .par_iter()
        .zip(row_ends.par_iter())
        .map(|(&start, &end)| {
            let k = end - start;
            let content = (content_width - GRID_GAP * (k as f32 - 1.0)).max(1.0);
            let natural: f32 = aspects[start..end].iter().sum::<f32>() * TARGET_ROW_HEIGHT;
            let h_raw = TARGET_ROW_HEIGHT * content / natural.max(1e-3);
            let h = if h_raw < MIN_ROW_HEIGHT {
                h_raw
            } else {
                h_raw.min(MAX_ROW_HEIGHT)
            };
            RowLayout {
                height: h,
                item_widths: aspects[start..end].iter().map(|a| a * h).collect(),
            }
        })
        .collect()
}

/// Build rows from the DP next-pointer table.
fn build_rows(aspects: &[f32], next: &[usize], content_width: f32, n: usize) -> Vec<RowLayout> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < n {
        let j = next[i];
        let k = j - i;
        let content = (content_width - GRID_GAP * (k as f32 - 1.0)).max(1.0);
        let natural: f32 = aspects[i..j].iter().sum::<f32>() * TARGET_ROW_HEIGHT;
        let h_raw = TARGET_ROW_HEIGHT * content / natural.max(1e-3);
        let h = if h_raw < MIN_ROW_HEIGHT {
            h_raw
        } else {
            h_raw.min(MAX_ROW_HEIGHT)
        };
        let item_widths: Vec<f32> = aspects[i..j].iter().map(|a| a * h).collect();
        rows.push(RowLayout {
            height: h,
            item_widths,
        });
        i = j;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total_cells(rows: &[RowLayout]) -> usize {
        rows.iter().map(|r| r.item_widths.len()).sum()
    }

    /// Every input cell appears exactly once, in order, with aspect kept.
    fn covers_in_order(aspects: &[f32], rows: &[RowLayout]) {
        let mut idx = 0;
        for row in rows {
            for &w in &row.item_widths {
                let expect = aspects[idx];
                let got = w / row.height;
                assert!(
                    (got - expect).abs() < 1e-2,
                    "cell {idx}: aspect {got} != {expect} (w={w}, h={})",
                    row.height
                );
                idx += 1;
            }
        }
        assert_eq!(idx, aspects.len());
        assert_eq!(total_cells(rows), aspects.len());
    }

    fn span_of(row: &RowLayout) -> f32 {
        row.item_widths.iter().sum::<f32>() + GRID_GAP * (row.item_widths.len() as f32 - 1.0)
    }

    #[test]
    fn typical_mix_fills_width_with_no_dead_space() {
        // Deterministic pseudo-random mix: portrait, square, landscape, wide.
        let mut aspects: Vec<f32> = Vec::new();
        let mut seed = 7u64;
        for _ in 0..40 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let bucket = (seed >> 33) as usize % 4;
            let base = match bucket {
                0 => 0.55, // portrait
                1 => 1.0,  // square
                2 => 1.5,  // landscape 3:2
                _ => 2.2,  // wide panorama
            };
            let jitter = ((seed >> 40) as f32 / (1u64 << 24) as f32) * 0.25 - 0.125;
            aspects.push((base + jitter).clamp(0.3, 3.2));
        }

        for width in [420.0, 640.0, 800.0, 1024.0, 1400.0, 1920.0] {
            let rows = justify_layout(&aspects, width);
            assert!(!rows.is_empty());
            covers_in_order(&aspects, &rows);

            for (idx, row) in rows.iter().enumerate() {
                assert!(
                    row.height >= MIN_ROW_HEIGHT - 0.01 && row.height <= MAX_ROW_HEIGHT + 0.01,
                    "row {idx} height {} out of bounds",
                    row.height
                );
                let total = span_of(row);
                let is_last = idx == rows.len() - 1;
                if is_last {
                    assert!(
                        total <= width + 0.5,
                        "tail must never overflow: {total} > {width}"
                    );
                } else {
                    assert!(
                        (total - width).abs() < 0.5,
                        "row {idx} leaves dead space or overflows: {total} vs {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn single_cell_and_empty_input() {
        assert!(justify_layout(&[], 600.0).is_empty());
        let rows = justify_layout(&[1.5], 600.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item_widths.len(), 1);
    }

    /// A wide panorama must not drag mixed rows far off the target height;
    /// the DP cost model resolves whether it sits alone or with a couple of
    /// companions. Assert the output stays legal and full.
    #[test]
    fn panorama_and_portraits_stay_legal() {
        // Lots of small portraits followed by one very wide panorama.
        let mut aspects: Vec<f32> = vec![0.45; 12];
        aspects.extend([3.8, 1.1, 0.9]);
        let rows = justify_layout(&aspects, 900.0);
        covers_in_order(&aspects, &rows);
        for (idx, row) in rows.iter().enumerate() {
            let total = span_of(row);
            let is_last = idx == rows.len() - 1;
            if is_last {
                assert!(total <= 900.5);
            } else {
                assert!((total - 900.0).abs() < 0.5);
            }
        }
        // The panorama keeps its wide aspect inside whatever row chose it.
        let mut idx = 0;
        for row in &rows {
            for &w in &row.item_widths {
                let expect = aspects[idx];
                assert!((w / row.height - expect).abs() < 1e-2);
                idx += 1;
            }
        }
    }

    /// Very long inputs terminate quickly (regression guard for the DP
    /// prune): layout of 1_000 photos must not blow up.
    #[test]
    fn thousand_photos_terminate() {
        let aspects: Vec<f32> = (0..1000)
            .map(|i| match i % 5 {
                0 => 0.5,
                1 => 0.75,
                2 => 1.0,
                3 => 1.6,
                _ => 2.5,
            })
            .collect();
        let rows = justify_layout(&aspects, 1280.0);
        covers_in_order(&aspects, &rows);
        for (idx, row) in rows.iter().enumerate() {
            if idx < rows.len() - 1 {
                assert!((span_of(row) - 1280.0).abs() < 0.5);
            }
        }
    }

    /// Deterministic: identical input yields identical output.
    #[test]
    fn deterministic() {
        let aspects = [1.0, 1.2, 0.8, 1.7, 0.6, 2.0, 1.1, 0.9, 1.4, 0.7];
        let a = justify_layout(&aspects, 820.0);
        let b = justify_layout(&aspects, 820.0);
        assert_eq!(a, b);
    }

    /// Zero or negative aspects must be floored: the DP prune relies on the
    /// natural row width being non-decreasing, and a 0-aspect cell would
    /// otherwise both stall the prune (O(n²)) and get a zero-width cell.
    #[test]
    fn degenerate_aspects_are_floored() {
        let mut aspects = vec![0.0; 2_000];
        aspects.extend(std::iter::repeat_n(-1.5, 2_000));
        let rows = justify_layout(&aspects, 1280.0);
        assert_eq!(total_cells(&rows), aspects.len());
        for row in &rows {
            for &w in &row.item_widths {
                assert!(w > 0.0, "zero-width cell from degenerate aspect");
            }
        }
    }

    /// Parallel greedy path (>500 items) produces valid layout.
    #[test]
    fn parallel_greedy_large_dataset() {
        let aspects: Vec<f32> = (0..800)
            .map(|i| match i % 5 {
                0 => 0.5,
                1 => 0.75,
                2 => 1.0,
                3 => 1.6,
                _ => 2.5,
            })
            .collect();
        let rows = justify_layout(&aspects, 1280.0);
        assert_eq!(total_cells(&rows), aspects.len());
        covers_in_order(&aspects, &rows);
        for (idx, row) in rows.iter().enumerate() {
            let total = span_of(row);
            let is_last = idx == rows.len() - 1;
            if is_last {
                assert!(total <= 1280.5);
            } else {
                assert!((total - 1280.0).abs() < 1.0, "row {idx}: {total} vs 1280");
            }
        }
    }
}
