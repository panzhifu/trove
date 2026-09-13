//! Spatial ordering: what turns a point cloud on disk into a file that can be
//! read one region at a time.
//!
//! A PLY body is in file order, and for a scan file order has nothing to do
//! with where the points are: two records next to each other can be a
//! kilometre apart in the model. Reading a *region* cheaply needs the
//! opposite, so an index sorts the points along a space-filling curve. This is
//! Morton (Z-order), the same choice Potree, the EPT format and 3D Tiles make;
//! Nimbus uses Hilbert and reports that it reduces the spatial jumps Morton
//! occasionally makes. Everything here goes through [`spatial_key`], so
//! swapping the curve in is one function rather than a rewrite.
//!
//! Ordering on its own does not bound memory — the sort has to spill to disk
//! for a twenty-gigabyte file — but it is the thing the spill sorts *by*, and
//! it is where the locality either exists or does not.

use crate::media::formats::types::Bounds;

/// Bits of resolution per axis. 21 keeps three interleaved codes inside a
/// `u64` (63 bits), which is 2M cells per axis — a 20 GB scan of a building
/// has millimetres of detail left at that resolution.
pub const MAX_BITS: u32 = 21;

/// The cube-to-grid mapping of one cloud.
///
/// Quantising to a grid is what makes the curve usable at all: a continuous
/// position has no meaningful Morton code, a cell does. The grid covers the
/// cloud's bounding cube and nothing else, so the resolution the cloud is
/// indexed at adapts to its size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    bits: u32,
    /// Model-space corner of the cube.
    origin: [f32; 3],
    /// Model units per cell.
    step: f32,
}

impl Grid {
    /// A grid of `2^bits` cells a side covering `bounds`.
    pub fn covering(bounds: Bounds, bits: u32) -> Self {
        let bits = bits.clamp(1, MAX_BITS);
        let size = bounds.size();
        // A cube, so a cell is the same size on every axis and the curve's
        // locality is not skewed by the model's proportions.
        let side = size[0].max(size[1]).max(size[2]).max(1e-12);
        let centre = bounds.center();
        let cells = (1u64 << bits) as f32;
        Self {
            bits,
            origin: [
                centre[0] - side * 0.5,
                centre[1] - side * 0.5,
                centre[2] - side * 0.5,
            ],
            step: side / cells,
        }
    }

    /// A grid from its raw parts, as an index file stores them.
    pub fn from_parts(bits: u32, origin: [f32; 3], step: f32) -> Self {
        Self {
            bits: bits.clamp(1, MAX_BITS),
            origin,
            step,
        }
    }

    /// The corner of the cube, in model space.
    pub fn origin(&self) -> [f32; 3] {
        self.origin
    }

    /// Model units per cell.
    pub fn step(&self) -> f32 {
        self.step
    }

    /// Bits per axis.
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Cells per axis.
    pub fn resolution(&self) -> u32 {
        1 << self.bits
    }

    /// Model space → cell coordinates, clamped into the cube.
    pub fn quantise(&self, point: [f32; 3]) -> [u32; 3] {
        let last = self.resolution() - 1;
        let mut cell = [0u32; 3];
        for axis in 0..3 {
            let raw = ((point[axis] - self.origin[axis]) / self.step).floor();
            cell[axis] = if raw.is_finite() {
                (raw.clamp(0.0, last as f32)) as u32
            } else {
                0
            };
        }
        cell
    }

    /// Cell coordinates → the centre of that cell, in model space.
    pub fn dequantise(&self, cell: [u32; 3]) -> [f32; 3] {
        let mut point = [0.0f32; 3];
        for axis in 0..3 {
            point[axis] = self.origin[axis] + (cell[axis] as f32 + 0.5) * self.step;
        }
        point
    }

    /// The most a quantised point can move: half a cell diagonal. This is the
    /// error the index admits, and the one the renderer's framing has to
    /// tolerate.
    pub fn max_error(&self) -> f32 {
        self.step * 0.5 * 3.0f32.sqrt()
    }
}

/// One byte, spread to every third bit: bit `i` of the byte lands on bit `3i`.
///
/// The spread is done a byte at a time through this table rather than with the
/// usual chain of magic masks. Three lookups per axis are as fast, and — the
/// reason to prefer it — the table's contents are obvious at a glance, where a
/// mistyped mask is a silent loss of the model's upper bits.
const SPREAD_BYTE: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut byte = 0usize;
    while byte < 256 {
        let mut bit = 0;
        let mut spread = 0u64;
        while bit < 8 {
            if byte & (1 << bit) != 0 {
                spread |= 1 << (bit * 3);
            }
            bit += 1;
        }
        table[byte] = spread;
        byte += 1;
    }
    table
};

/// `value` with two zero bits between each of its bits: the spread of one
/// axis. Bits above [`MAX_BITS`] are ignored.
fn spread(value: u32) -> u64 {
    // 21 bits is three bytes: 8 + 8 + 5. Each byte's spread starts 24 bits
    // above the previous one, which is the same as 8 input bits × 3.
    SPREAD_BYTE[(value & 0xff) as usize]
        | SPREAD_BYTE[((value >> 8) & 0xff) as usize] << 24
        | SPREAD_BYTE[((value >> 16) & 0x1f) as usize] << 48
}

/// The Morton code of a cell: where it sits along the Z-order curve.
///
/// The three axes are interleaved bit by bit, so the high bits of the result
/// are the high bits of all three coordinates — which is what makes a range of
/// codes a *box* in space rather than a stripe.
pub fn morton_code(cell: [u32; 3]) -> u64 {
    spread(cell[0]) | spread(cell[1]) << 1 | spread(cell[2]) << 2
}

/// The spatial key of a model-space point, for sorting.
pub fn spatial_key(grid: &Grid, point: [f32; 3]) -> u64 {
    morton_code(grid.quantise(point))
}

/// A run of spatially adjacent points: the unit an index reader fetches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    /// Bounds of the points in the run, in model space.
    pub bounds: Bounds,
    /// First point in the run, as an offset into the ordered arrays.
    pub start: usize,
    /// Points in the run.
    pub len: usize,
}

impl Chunk {
    /// One past the last point.
    pub fn end(&self) -> usize {
        self.start + self.len
    }
}

/// Points and their colours in spatial order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpatialOrder {
    pub points: Vec<[f32; 3]>,
    /// Parallel to `points`; empty when the cloud carries no colours.
    pub colors: Vec<[f32; 3]>,
    /// The grid the order was built on, so a reader can quantise as it wrote.
    pub grid: Option<Grid>,
}

/// Sort a cloud into the order an index reads it in.
///
/// This is the in-memory form, for a cloud that fits; the spilling version
/// that a twenty-gigabyte file needs sorts by the same key, run by run.
pub fn sort_spatially(points: Vec<[f32; 3]>, colors: Vec<[f32; 3]>, bits: u32) -> SpatialOrder {
    if points.is_empty() {
        return SpatialOrder::default();
    }
    let mut bounds = Bounds::empty();
    for point in &points {
        bounds.extend(*point);
    }
    let grid = Grid::covering(bounds, bits);

    let has_colors = colors.len() == points.len();
    let mut order: Vec<usize> = (0..points.len()).collect();
    // A stable sort keeps points that quantise to the same cell in the order
    // they arrived, which makes the result reproducible for a given file.
    order.sort_by_key(|index| spatial_key(&grid, points[*index]));

    let mut sorted_points = Vec::with_capacity(points.len());
    let mut sorted_colors = if has_colors {
        Vec::with_capacity(colors.len())
    } else {
        Vec::new()
    };
    for index in order {
        sorted_points.push(points[index]);
        if has_colors {
            sorted_colors.push(colors[index]);
        }
    }
    SpatialOrder {
        points: sorted_points,
        colors: sorted_colors,
        grid: Some(grid),
    }
}

/// Split ordered points into runs of at most `max_points`, each with the
/// bounds of what it holds.
///
/// A run is what a renderer streams: its bounds are the box the LOD culling
/// tests, and its length is how much has to be read to draw it. Runs are taken
/// along the curve, so a run is a compact region of the model.
pub fn chunked(points: &[[f32; 3]], max_points: usize) -> Vec<Chunk> {
    let max_points = max_points.max(1);
    let mut chunks = Vec::with_capacity(points.len().div_ceil(max_points));
    for start in (0..points.len()).step_by(max_points) {
        let len = max_points.min(points.len() - start);
        let mut bounds = Bounds::empty();
        for point in &points[start..start + len] {
            bounds.extend(*point);
        }
        chunks.push(Chunk { bounds, start, len });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The obviously-correct version the fast one is checked against.
    fn morton_naive(cell: [u32; 3]) -> u64 {
        let mut code = 0u64;
        for bit in 0..MAX_BITS {
            for (axis, value) in cell.iter().enumerate() {
                if value & (1 << bit) != 0 {
                    code |= 1 << (bit * 3 + axis as u32);
                }
            }
        }
        code
    }

    /// The magic-number spread is only worth its opacity if it agrees with the
    /// bit-at-a-time version, on every bit it claims to carry.
    #[test]
    fn morton_matches_the_naive_interleave() {
        let edge = (1u32 << MAX_BITS) - 1;
        let mut cells = vec![
            [0, 0, 0],
            [edge, 0, 0],
            [0, edge, 0],
            [0, 0, edge],
            [edge, edge, edge],
            [1, 2, 3],
            [1 << 20, 1 << 10, 1],
            [0x1f_ffff, 0x0f_0f0f, 0x12_3456],
            [12345, 6789, 1011],
        ];
        // A spread of bits, so a wrong mask shows up somewhere.
        for bit in 0..MAX_BITS {
            cells.push([1 << bit, 1 << (bit / 2), 1 << (bit % 7)]);
            cells.push([edge >> bit, edge & !((1 << bit) - 1), bit * 7919]);
        }
        for cell in cells {
            assert_eq!(morton_code(cell), morton_naive(cell), "{cell:?}");
        }
    }

    /// The interleave is a bijection: every cell of the grid gets its own code
    /// and no code is left out. (Note what is *not* asserted: that consecutive
    /// codes are neighbours. That is a property of the Hilbert curve; Morton
    /// jumps between sub-cubes, and those jumps are exactly what the curve's
    /// critics point at.)
    #[test]
    fn the_codes_visit_every_cell_exactly_once() {
        for bits in 1..=4u32 {
            let side = 1u32 << bits;
            let mut codes = Vec::new();
            for x in 0..side {
                for y in 0..side {
                    for z in 0..side {
                        codes.push(morton_code([x, y, z]));
                    }
                }
            }
            let total = 1usize << (bits * 3);
            assert_eq!(codes.len(), total);
            codes.sort_unstable();
            codes.dedup();
            assert_eq!(codes.len(), total, "bits {bits}: codes are not unique");
            assert_eq!(codes[0], 0);
            assert_eq!(*codes.last().unwrap(), total as u64 - 1);
        }
    }

    /// What makes a *range* of codes a *box*: cells sharing the top `3k` bits
    /// of their code are exactly one sub-cube of side `2^(bits-k)`. This is
    /// the property the index's runs are read by, and the one a wrong mask
    /// breaks.
    #[test]
    fn a_shared_code_prefix_is_a_sub_cube() {
        let bits = 4u32;
        let side = 1u32 << bits;
        for k in 1..=bits {
            let sub = 1u32 << (bits - k);
            let mut groups: std::collections::HashMap<u64, Vec<[u32; 3]>> = Default::default();
            for x in 0..side {
                for y in 0..side {
                    for z in 0..side {
                        let code = morton_code([x, y, z]);
                        groups
                            .entry(code >> (3 * (bits - k)))
                            .or_default()
                            .push([x, y, z]);
                    }
                }
            }
            assert_eq!(
                groups.len(),
                1 << (3 * k),
                "k {k}: the wrong number of sub-cubes"
            );
            for cells in groups.values() {
                assert_eq!(cells.len(), (sub * sub * sub) as usize, "k {k}");
                for (axis, _) in cells[0].iter().enumerate() {
                    let min = cells.iter().map(|cell| cell[axis]).min().unwrap();
                    let max = cells.iter().map(|cell| cell[axis]).max().unwrap();
                    assert_eq!(max - min + 1, sub, "k {k}: not a cube on axis {axis}");
                    // Aligned to the sub-cube grid, not just the right size.
                    assert_eq!(min % sub, 0, "k {k}: offset on axis {axis}");
                }
            }
        }
    }

    /// Quantising a cloud loses at most half a cell, and the cell size follows
    /// the model: a small scan is not indexed at the resolution of a city.
    #[test]
    fn the_grid_round_trips_within_its_error() {
        let bounds = Bounds {
            min: [-2.0, -1.0, -0.5],
            max: [2.0, 1.0, 0.5],
        };
        let grid = Grid::covering(bounds, 12);
        assert_eq!(grid.resolution(), 4096);
        let mut worst = 0.0f32;
        for index in 0..1_000 {
            let t = index as f32 * 0.017;
            let point = [t.sin() * 2.0, (t * 1.7).cos(), (t * 0.3).sin() * 0.5];
            let restored = grid.dequantise(grid.quantise(point));
            for axis in 0..3 {
                worst = worst.max((restored[axis] - point[axis]).abs());
            }
        }
        assert!(worst <= grid.max_error() + 1e-6, "worst {worst}");

        // A grid of the same bit count over a bigger model has bigger cells.
        let big = Grid::covering(
            Bounds {
                min: [-200.0, -200.0, -200.0],
                max: [200.0, 200.0, 200.0],
            },
            12,
        );
        assert!(big.max_error() > grid.max_error() * 50.0);
    }

    /// Points outside the bounds the grid was built from still index, clamped
    /// to the edge cell rather than wrapping to the far side.
    #[test]
    fn points_outside_the_grid_clamp_to_the_edge() {
        let grid = Grid::covering(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            8,
        );
        assert_eq!(grid.quantise([2.0, -5.0, f32::NAN]), [255, 0, 0]);
    }

    /// Mean distance between consecutive points — the measure of whether a run
    /// of records is a region.
    fn mean_step(points: &[[f32; 3]]) -> f32 {
        let total: f32 = points
            .windows(2)
            .map(|pair| {
                let d = [
                    pair[1][0] - pair[0][0],
                    pair[1][1] - pair[0][1],
                    pair[1][2] - pair[0][2],
                ];
                (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
            })
            .sum();
        total / (points.len() - 1) as f32
    }

    /// Ordering along the curve is what makes a run of points a *region*.
    /// The fixture is a regular grid walked in a scrambled order — what a scan
    /// file looks like: every point is a neighbour of something, and file order
    /// says nothing about what.
    #[test]
    fn spatial_order_makes_neighbours_out_of_consecutive_records() {
        let side = 32u32;
        let spacing = 10.0f32 / (side - 1) as f32;
        let mut cells: Vec<[u32; 3]> = Vec::new();
        for x in 0..side {
            for y in 0..side {
                for z in 0..side {
                    cells.push([x, y, z]);
                }
            }
        }
        // A deterministic scramble (a linear congruential walk over the grid).
        let mut state = 12345u64;
        for index in (1..cells.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            cells.swap(index, (state >> 33) as usize % (index + 1));
        }
        let points: Vec<[f32; 3]> = cells
            .iter()
            .map(|cell| {
                [
                    cell[0] as f32 * spacing,
                    cell[1] as f32 * spacing,
                    cell[2] as f32 * spacing,
                ]
            })
            .collect();

        let before = mean_step(&points);
        let sorted = sort_spatially(points, Vec::new(), 12);
        let after = mean_step(&sorted.points);

        assert!(
            after * 10.0 < before,
            "spatial order should be much tighter: {before} -> {after}"
        );
        // And tight in absolute terms: consecutive records are within a couple
        // of grid steps, which is what makes a short run a compact region.
        assert!(
            after < spacing * 3.0,
            "consecutive points are {after} apart, a grid step is {spacing}"
        );
    }

    /// Every point survives the sort and keeps its own colour — the two arrays
    /// must not come apart.
    #[test]
    fn sorting_keeps_points_and_colours_together() {
        let mut points = Vec::new();
        let mut colors = Vec::new();
        for index in 0..500 {
            let t = index as f32 * 0.1;
            points.push([(t * 3.0).sin(), (t * 5.0).cos(), (t * 7.0).sin()]);
            // The colour encodes the point's identity, so a mismatched pairing
            // is visible.
            colors.push([index as f32, 0.0, 0.0]);
        }
        let sorted = sort_spatially(points.clone(), colors, 8);
        assert_eq!(sorted.points.len(), points.len());
        assert_eq!(sorted.colors.len(), points.len());
        for (point, color) in sorted.points.iter().zip(&sorted.colors) {
            let original = points[color[0] as usize];
            assert_eq!(*point, original, "colour {} lost its point", color[0]);
        }
        // And the order really changed.
        assert_ne!(sorted.points, points);
    }

    /// Runs tile the ordered cloud, hold at most the requested number of
    /// points, and their bounds really do contain them.
    #[test]
    fn chunks_tile_the_cloud_and_bound_their_own_points() {
        let mut points = Vec::new();
        for index in 0..1_000 {
            let t = index as f32 * 0.07;
            points.push([t.sin() * 4.0, (t * 1.3).cos() * 2.0, (t * 0.7).sin()]);
        }
        let sorted = sort_spatially(points, Vec::new(), 9);
        let chunks = chunked(&sorted.points, 128);
        assert_eq!(chunks.len(), 1_000usize.div_ceil(128));
        assert_eq!(chunks[0].start, 0);
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].end(), pair[1].start, "runs must tile the order");
            assert!(pair[0].len <= 128);
        }
        assert_eq!(chunks.last().unwrap().end(), 1_000);

        for chunk in &chunks {
            for point in &sorted.points[chunk.start..chunk.end()] {
                for axis in 0..3 {
                    assert!(
                        point[axis] >= chunk.bounds.min[axis] - 1e-6
                            && point[axis] <= chunk.bounds.max[axis] + 1e-6,
                        "{point:?} outside {:?}",
                        chunk.bounds
                    );
                }
            }
        }
    }

    /// An empty cloud sorts to nothing rather than to a panic.
    #[test]
    fn an_empty_cloud_is_not_an_error() {
        let sorted = sort_spatially(Vec::new(), Vec::new(), 8);
        assert!(sorted.points.is_empty() && sorted.colors.is_empty());
        assert!(chunked(&[], 64).is_empty());
    }
}
