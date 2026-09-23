//! Building an index from a PLY point cloud, in one pass over the file.
//!
//! This is the seam between the format reader and the index: the reader yields
//! batches in file order, the index sorts them in spatial order and writes
//! them out compactly. Nothing here holds the cloud in memory — that is the
//! point of the exercise, since the files this exists for are the ones that
//! would not fit.

use std::path::Path;

use super::file::{BatchSource, IndexConfig, IndexSummary, PointBatch, build_index};
use super::order::Grid;
use crate::media::formats::streaming::PointStreamer;

/// Points read from the file per batch.
const BATCH: usize = 64 * 1024;

/// Records sampled to find the cloud's extent before the real read.
///
/// The grid the order is built on has to cover the whole cloud, and a strided
/// sample of a few thousand records gives that without a pass over twenty
/// gigabytes.
const EXTENT_SAMPLES: usize = 4096;

/// A PLY body read as batches, in file order.
struct PlyBatches {
    streamer: PointStreamer,
    done: bool,
}

impl BatchSource for PlyBatches {
    fn next_batch(&mut self) -> Result<Option<PointBatch>, String> {
        if self.done {
            return Ok(None);
        }
        match self.streamer.next_chunk(BATCH)? {
            Some(chunk) => {
                self.done = chunk.is_last;
                Ok(Some(PointBatch {
                    points: chunk.positions,
                    colors: chunk.colors,
                    intensities: chunk.intensities,
                    classes: chunk.classes,
                }))
            }
            None => {
                self.done = true;
                Ok(None)
            }
        }
    }
}

/// Build an index for a binary PLY point cloud, sampling the file for the
/// grid's extent.
///
/// Fails for an ASCII body, whose records are not a fixed size: nothing can be
/// sampled without reading the file. Use [`build_ply_index_with_grid`] with a
/// grid derived from a pass of your own in that case.
pub fn build_ply_index(
    path: &Path,
    config: IndexConfig,
    scratch: &Path,
    out: &Path,
) -> Result<IndexSummary, String> {
    let (mut streamer, _) = PointStreamer::open(path)?;
    if streamer.declares_faces() {
        return Err("the PLY declares faces, so it is a mesh rather than a point cloud".into());
    }
    let bounds = streamer.sample_bounds(EXTENT_SAMPLES).ok_or(
        "the PLY body cannot be sampled for the index's grid: its records are not a fixed size \
         (an ASCII body has to be read once to find the cloud's extent)",
    )?;
    // Sampling is a side look: the reader is still at the start of the body.
    let grid = Grid::covering(bounds, config.grid_bits);
    build_index(
        &mut PlyBatches {
            streamer,
            done: false,
        },
        grid,
        config,
        scratch,
        out,
    )
}

/// [`build_ply_index`] with the grid supplied by the caller.
pub fn build_ply_index_with_grid(
    path: &Path,
    grid: Grid,
    config: IndexConfig,
    scratch: &Path,
    out: &Path,
) -> Result<IndexSummary, String> {
    let (streamer, _) = PointStreamer::open(path)?;
    if streamer.declares_faces() {
        return Err("the PLY declares faces, so it is a mesh rather than a point cloud".into());
    }
    build_index(
        &mut PlyBatches {
            streamer,
            done: false,
        },
        grid,
        config,
        scratch,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::formats::types::Bounds;
    use crate::media::index::CloudIndex;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("trove-ply-index-{}-{name}", std::process::id()))
    }

    /// A binary PLY cloud of `count` points on a scrambled grid, with
    /// `uchar` colours — the shape and the layout a scan file has.
    fn write_cloud(
        path: &Path,
        count: usize,
        coloured: bool,
        channels: bool,
    ) -> (Vec<[f32; 3]>, Bounds) {
        let side = (count as f64).cbrt().ceil() as usize;
        let spacing = 10.0f32 / side as f32;
        let mut cells: Vec<(usize, usize, usize)> = Vec::new();
        for x in 0..side {
            for y in 0..side {
                for z in 0..side {
                    cells.push((x, y, z));
                }
            }
        }
        let mut state = 5_197u64;
        for index in (1..cells.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            cells.swap(index, (state >> 33) as usize % (index + 1));
        }
        let mut points = Vec::with_capacity(count);
        let mut bounds = Bounds::empty();
        for cell in cells.iter().take(count) {
            let point = [
                cell.0 as f32 * spacing,
                cell.1 as f32 * spacing,
                cell.2 as f32 * spacing,
            ];
            bounds.extend(point);
            points.push(point);
        }

        let mut file = Vec::new();
        file.extend_from_slice(
            format!(
                "ply\nformat binary_little_endian 1.0\nelement vertex {}\n\
                 property float x\nproperty float y\nproperty float z\n",
                points.len()
            )
            .as_bytes(),
        );
        if coloured {
            file.extend_from_slice(
                b"property uchar red\nproperty uchar green\nproperty uchar blue\n",
            );
        }
        if channels {
            file.extend_from_slice(b"property float intensity\nproperty uchar classification\n");
        }
        file.extend_from_slice(b"end_header\n");
        for (index, point) in points.iter().enumerate() {
            for value in point {
                file.extend_from_slice(&value.to_le_bytes());
            }
            if coloured {
                let ramp = (index % 256) as u8;
                file.extend_from_slice(&[ramp, 64, 200]);
            }
            if channels {
                file.extend_from_slice(&point[1].to_le_bytes());
                file.push(class_of(point[1], spacing));
            }
        }
        std::fs::write(path, &file).unwrap();
        (points, bounds)
    }

    /// The class a scan fixture gives a point: a function of its own position,
    /// so a reader can check what came back against the point it landed on and
    /// not against the record it was written in — the index reorders the cloud,
    /// which is the entire point of it.
    fn class_of(y: f32, spacing: f32) -> u8 {
        ((y / spacing).round() as usize % 23) as u8
    }

    /// The whole upgrade on a real file: a scan with return strength and
    /// classification goes in, and both come back through the streamer, the
    /// spilled sort, the reordered file and the chunk reader — with the header
    /// able to say what the cloud spans before a single point is read.
    #[test]
    fn a_scans_channels_survive_the_index_and_its_new_order() {
        let dir = temp("channels");
        std::fs::create_dir_all(&dir).unwrap();
        let ply = dir.join("scan.ply");
        let (points, _) = write_cloud(&ply, 4_000, true, true);
        let spacing = 10.0f32 / (points.len() as f64).cbrt().ceil() as f32;
        let out = dir.join("scan.trovecloud");

        let summary = build_ply_index(
            &ply,
            IndexConfig {
                points_per_chunk: 500,
                sort_capacity: 300,
                ..Default::default()
            },
            &dir,
            &out,
        )
        .expect("a scan indexes");
        assert!(summary.channels.colors, "the fixture has colours");
        assert!(summary.channels.intensity, "and intensity");
        assert!(summary.channels.class, "and a classification");
        assert_eq!(
            summary.channels.record_bytes(),
            14,
            "6 + 3 + 4 + 1 bytes a point"
        );

        let index = CloudIndex::open(&out).expect("the index opens");
        // What the two fields span, from the header alone: this is what lets the
        // viewport paint the first chunk the way the last one will look.
        let (low, high) = index.intensity_range().expect("a range");
        assert!(low.abs() < 1e-3, "{low}");
        assert!((high - 10.0 * (1.0 - 1.0 / 16.0)).abs() < 0.7, "{high}");
        assert_eq!(index.class_count(), Some(16), "the fixture uses 16 classes");

        let mut seen = 0usize;
        for chunk in 0..index.chunk_count() {
            let batch = index.read_chunk(chunk).expect("the chunk reads");
            assert_eq!(batch.intensities.len(), batch.points.len());
            assert_eq!(batch.classes.len(), batch.points.len());
            for (point, (intensity, class)) in batch
                .points
                .iter()
                .zip(batch.intensities.iter().zip(&batch.classes))
            {
                // The intensity was written as the y coordinate, so a point
                // that kept its own says so here — within what a chunk-local
                // 16-bit fraction can move it.
                assert!(
                    (intensity - point[1]).abs() < 1e-3,
                    "{point:?} carries {intensity}"
                );
                assert_eq!(
                    *class,
                    class_of(point[1], spacing),
                    "{point:?} was painted with another point's class"
                );
                seen += 1;
            }
        }
        assert_eq!(seen, points.len(), "and none of them went missing");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole path, on a real file: a PLY goes in, a compact index comes
    /// out, and reading it back gives the cloud in spatial order.
    #[test]
    fn a_ply_becomes_an_index_and_reads_back_in_spatial_order() {
        let dir = temp("build");
        std::fs::create_dir_all(&dir).unwrap();
        let ply = dir.join("cloud.ply");
        let (points, bounds) = write_cloud(&ply, 4_000, true, false);
        let out = dir.join("cloud.trovecloud");

        let config = IndexConfig {
            points_per_chunk: 500,
            // Small enough that the sort has to spill: the path a big file
            // takes, exercised on a small one.
            sort_capacity: 300,
            ..Default::default()
        };
        let summary = build_ply_index(&ply, config, &dir, &out).expect("index builds");
        assert_eq!(summary.points, points.len() as u64);
        assert!(summary.channels.colors);
        assert!(summary.spilled_runs > 1, "{}", summary.spilled_runs);
        assert!(summary.chunks > 1);
        // The index is far smaller than the file it came from: 9 bytes a point
        // against 15.
        let file_bytes = std::fs::metadata(&ply).unwrap().len();
        assert!(
            summary.bytes < file_bytes,
            "index {} vs file {file_bytes}",
            summary.bytes
        );

        let index = CloudIndex::open(&out).expect("index opens");
        assert_eq!(index.point_count(), points.len() as u64);
        assert!(index.has_colors());
        // The extent is the cloud's own, so a camera framed on it is right.
        for axis in 0..3 {
            assert!((index.bounds().min[axis] - bounds.min[axis]).abs() < 1e-3);
            assert!((index.bounds().max[axis] - bounds.max[axis]).abs() < 1e-3);
        }

        // Every point is present exactly once, and consecutive records are
        // neighbours: compare the decoded cloud against the source as a set by
        // checking that each decoded point is one of the source points, within
        // its chunk's quantisation.
        // What the index is for: the file's order *is* the spatial order, so a
        // run of records is a region of the model. Compared point by point
        // against the in-memory sort, within what a chunk-local 16-bit
        // fraction carries.
        let in_memory = crate::media::index::order::sort_spatially(points.clone(), Vec::new(), 21);
        let mut decoded_points: Vec<[f32; 3]> = Vec::with_capacity(points.len());
        for chunk in 0..index.chunk_count() {
            let batch = index.read_chunk(chunk).unwrap();
            assert_eq!(batch.colors.len(), batch.points.len());
            decoded_points.extend(batch.points);
        }
        assert_eq!(decoded_points.len(), points.len());
        let mut worst = 0.0f32;
        for (got, want) in decoded_points.iter().zip(&in_memory.points) {
            for axis in 0..3 {
                worst = worst.max((got[axis] - want[axis]).abs());
            }
        }
        assert!(worst < 1e-3, "the stored order drifted by {worst}");

        // And the order is *local*: a mean step of about a cell, against the
        // several cells a scrambled file spends. (A single step can be much
        // larger — Morton jumps when it steps between sub-cubes, which is the
        // flaw Hilbert fixes — so the mean is the measure, not the maximum.)
        let spacing = 10.0f32 / (points.len() as f64).cbrt().ceil() as f32;
        let mean_step = |list: &[[f32; 3]]| {
            let total: f32 = list
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
            total / (list.len() - 1) as f32
        };
        let stored_step = mean_step(&decoded_points);
        let source_step = mean_step(&points);
        assert!(
            stored_step < spacing * 2.0,
            "a mean step of {stored_step} against a {spacing} grid"
        );
        assert!(
            stored_step * 4.0 < source_step,
            "the index barely helped: {source_step} -> {stored_step}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The index survives a cloud with no colours, and reports that it has
    /// none rather than inventing an alpha channel.
    #[test]
    fn an_uncoloured_ply_indexes_without_colours() {
        let dir = temp("plain");
        std::fs::create_dir_all(&dir).unwrap();
        let ply = dir.join("plain.ply");
        write_cloud(&ply, 900, false, false);
        let out = dir.join("plain.trovecloud");
        let summary = build_ply_index(&ply, IndexConfig::default(), &dir, &out).expect("builds");
        assert!(!summary.channels.colors);
        let index = CloudIndex::open(&out).expect("opens");
        assert!(!index.has_colors());
        assert!(index.read_chunk(0).unwrap().colors.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An ASCII body cannot be sampled for its extent, and says so instead of
    /// guessing; the caller can still supply a grid of their own.
    #[test]
    fn an_ascii_body_is_refused_without_a_grid_but_works_with_one() {
        let dir = temp("ascii");
        std::fs::create_dir_all(&dir).unwrap();
        let ply = dir.join("ascii.ply");
        let mut text = String::from(
            "ply\nformat ascii 1.0\nelement vertex 4\n\
             property float x\nproperty float y\nproperty float z\nend_header\n",
        );
        for point in [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ] {
            text.push_str(&format!("{} {} {}\n", point[0], point[1], point[2]));
        }
        std::fs::write(&ply, text).unwrap();

        let out = dir.join("ascii.trovecloud");
        let error = build_ply_index(&ply, IndexConfig::default(), &dir, &out)
            .expect_err("refused without a grid");
        assert!(error.contains("sampled"), "{error}");

        let grid = Grid::covering(
            Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            12,
        );
        let summary = build_ply_index_with_grid(&ply, grid, IndexConfig::default(), &dir, &out)
            .expect("builds with a grid");
        assert_eq!(summary.points, 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A mesh PLY is not a cloud, and the index says so rather than indexing
    /// its vertices as if they were points.
    #[test]
    fn a_mesh_ply_is_refused() {
        let dir = temp("mesh");
        std::fs::create_dir_all(&dir).unwrap();
        let ply = dir.join("mesh.ply");
        let text = "ply\nformat ascii 1.0\nelement vertex 3\n\
                    property float x\nproperty float y\nproperty float z\n\
                    element face 1\nproperty list uchar int vertex_indices\n\
                    end_header\n0 0 0\n1 0 0\n0 1 0\n3 0 1 2\n";
        std::fs::write(&ply, text).unwrap();
        let out = dir.join("mesh.trovecloud");
        let error = build_ply_index(&ply, IndexConfig::default(), &dir, &out)
            .expect_err("a mesh is refused");
        assert!(error.contains("faces"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
