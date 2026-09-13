//! Sorting a point stream into spatial order without holding it all in memory.
//!
//! The index has to be in [`crate::media::index::order`] order, and a cloud
//! that needs an index is by definition too large to sort in place. So the
//! stream is sorted in runs: as many points as fit a buffer are sorted and
//! spilled to a scratch file, and at the end the runs are merged back
//! together. That is the shape of any external sort; what makes it specific to
//! point clouds is the key, which is a space-filling curve rather than a
//! comparison.
//!
//! Runs are merged in groups of [`MERGE_FAN_IN`] rather than all at once: a
//! twenty-gigabyte cloud is a hundred runs or more, and a hundred open files
//! is not something to hand a user's machine.

use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::order::{Grid, spatial_key};

/// Runs merged in one pass: keeps the number of open files bounded whatever
/// the cloud's size, at the cost of reading the scratch file once more when
/// there are more runs than this.
pub const MERGE_FAN_IN: usize = 16;

/// Bytes of one spilled record: the key, the position, and the colour.
const RECORD_BYTES: usize = 8 + 12 + 12;

/// A point with its colour: what the sorter hands back in order.
pub type Point = ([f32; 3], [f32; 3]);

/// One point as the sorter carries it between sorting and writing out.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Record {
    key: u64,
    position: [f32; 3],
    color: [f32; 3],
}

impl Record {
    fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut bytes = [0u8; RECORD_BYTES];
        bytes[0..8].copy_from_slice(&self.key.to_le_bytes());
        for axis in 0..3 {
            let at = 8 + axis * 4;
            bytes[at..at + 4].copy_from_slice(&self.position[axis].to_le_bytes());
            let at = 20 + axis * 4;
            bytes[at..at + 4].copy_from_slice(&self.color[axis].to_le_bytes());
        }
        out.write_all(&bytes)
    }

    fn read(from: &[u8; RECORD_BYTES]) -> Self {
        let float = |at: usize| f32::from_le_bytes(from[at..at + 4].try_into().unwrap());
        Self {
            key: u64::from_le_bytes(from[0..8].try_into().unwrap()),
            position: [float(8), float(12), float(16)],
            color: [float(20), float(24), float(28)],
        }
    }
}

/// A sorted run living in the scratch file.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    offset: u64,
    points: u64,
}

/// The scratch file, deleted when the last user of it is done.
///
/// A sort of a twenty-gigabyte cloud leaves a scratch file of comparable size;
/// forgetting to remove it is not a detail.
struct Scratch {
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// Sorts points into spatial order, spilling to a scratch file as it fills.
pub struct SpatialSorter {
    grid: Grid,
    /// Points held before a run is written out.
    capacity: usize,
    buffer: Vec<Record>,
    /// Every run in the scratch file, oldest first.
    runs: Vec<Run>,
    /// Append point of the scratch file.
    appended: u64,
    /// Points pushed in total.
    total: u64,
    /// Whether any colour has been seen: a cloud is coloured or it is not.
    colors: bool,
    scratch: Scratch,
    writer: Option<BufWriter<File>>,
}

impl SpatialSorter {
    /// A sorter writing its scratch runs under `scratch` (created if missing).
    pub fn new(grid: Grid, capacity: usize, scratch: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        Ok(Self {
            grid,
            capacity: capacity.max(1),
            buffer: Vec::new(),
            runs: Vec::new(),
            appended: 0,
            total: 0,
            colors: false,
            scratch: Scratch {
                path: scratch.join(format!(
                    "trove-sort-{}.bin",
                    crate::model::new_id().simple()
                )),
            },
            writer: None,
        })
    }

    /// Points accepted so far.
    pub fn points(&self) -> u64 {
        self.total
    }

    /// Runs written to the scratch file: `0` when the cloud fitted the buffer,
    /// which is what a small file should see.
    pub fn spilled_runs(&self) -> usize {
        self.runs.len()
    }

    /// Whether the cloud has carried any colour so far.
    pub fn has_colors(&self) -> bool {
        self.colors
    }

    /// Add a batch of points, in whatever order the file holds them.
    ///
    /// `colors` is either empty or parallel to `points`.
    pub fn push(&mut self, points: &[[f32; 3]], colors: &[[f32; 3]]) -> Result<(), String> {
        let has_colors = colors.len() == points.len() && !colors.is_empty();
        self.colors |= has_colors;
        for (index, position) in points.iter().enumerate() {
            self.buffer.push(Record {
                key: spatial_key(&self.grid, *position),
                position: *position,
                color: if has_colors { colors[index] } else { [0.0; 3] },
            });
            self.total += 1;
            if self.buffer.len() >= self.capacity {
                self.spill()?;
            }
        }
        Ok(())
    }

    /// Sort what is buffered and append it to the scratch file as a run.
    fn spill(&mut self) -> Result<(), String> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // Stable, so points sharing a cell keep their file order and the index
        // of a given file is reproducible.
        self.buffer.sort_by_key(|record| record.key);
        if self.writer.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.scratch.path)
                .map_err(|e| e.to_string())?;
            self.writer = Some(BufWriter::new(file));
        }
        let offset = self.appended;
        let points = self.buffer.len() as u64;
        {
            let writer = self.writer.as_mut().expect("just opened");
            for record in &self.buffer {
                record.write(writer).map_err(|e| e.to_string())?;
            }
        }
        self.appended += points * RECORD_BYTES as u64;
        self.runs.push(Run { offset, points });
        self.buffer.clear();
        Ok(())
    }

    /// The points in spatial order.
    ///
    /// Nothing touches the disk for a cloud that fitted the buffer, and the
    /// points come out in the same order either way — which is the property
    /// the tests pin.
    pub fn finish(mut self) -> Result<SortedPoints, String> {
        if self.runs.is_empty() {
            self.buffer.sort_by_key(|record| record.key);
            return Ok(SortedPoints {
                source: Source::Memory {
                    records: std::mem::take(&mut self.buffer),
                    at: 0,
                },
                run_count: 0,
            });
        }
        self.spill()?;
        if let Some(mut writer) = self.writer.take() {
            writer.flush().map_err(|e| e.to_string())?;
            // Closed here rather than at the end of the function: the merge
            // opens its own readers on the same file.
            drop(writer);
        }

        // Merge down to a fan-in the final reader can open at once.
        let mut runs = std::mem::take(&mut self.runs);
        while runs.len() > MERGE_FAN_IN {
            let mut merged = Vec::with_capacity(runs.len().div_ceil(MERGE_FAN_IN));
            for group in runs.chunks(MERGE_FAN_IN) {
                if group.len() == 1 {
                    merged.push(group[0]);
                    continue;
                }
                let mut cursors = [const { None }; MERGE_FAN_IN];
                let mut count = 0;
                for run in group {
                    cursors[count] = Some(Cursor::open(&self.scratch.path, *run)?);
                    count += 1;
                }
                let (offset, points) = self.merge_into_scratch(&mut cursors[..count])?;
                merged.push(Run { offset, points });
            }
            runs = merged;
        }

        let mut cursors = Vec::with_capacity(runs.len());
        for run in &runs {
            cursors.push(Cursor::open(&self.scratch.path, *run)?);
        }
        let run_count = runs.len();
        Ok(SortedPoints {
            source: Source::Merged {
                cursors,
                heap: BinaryHeap::new(),
                primed: false,
                _scratch: self.scratch,
            },
            run_count,
        })
    }

    /// Merge a group of cursors into a new run at the end of the scratch file.
    fn merge_into_scratch(&mut self, cursors: &mut [Option<Cursor>]) -> Result<(u64, u64), String> {
        let offset = self.appended;
        let mut points = 0u64;
        let mut heap = BinaryHeap::new();
        for (index, cursor) in cursors.iter_mut().enumerate() {
            if let Some(head) = cursor.as_ref().and_then(|cursor| cursor.head) {
                heap.push(Head {
                    key: head.key,
                    cursor: index,
                });
            }
        }
        let mut writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.scratch.path)
                .map_err(|e| e.to_string())?,
        );
        while let Some(entry) = heap.pop() {
            let cursor = cursors[entry.cursor]
                .as_mut()
                .ok_or("a run vanished mid-merge")?;
            let record = cursor.head.ok_or("a run ran out from under the heap")?;
            record.write(&mut writer).map_err(|e| e.to_string())?;
            points += 1;
            cursor.advance()?;
            if let Some(head) = cursor.head {
                heap.push(Head {
                    key: head.key,
                    cursor: entry.cursor,
                });
            }
        }
        writer.flush().map_err(|e| e.to_string())?;
        self.appended += points * RECORD_BYTES as u64;
        Ok((offset, points))
    }
}

/// The head of one run, for the merge heap. Ordered so the smallest key comes
/// out first.
#[derive(PartialEq, Eq)]
struct Head {
    key: u64,
    cursor: usize,
}

impl Ord for Head {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.key.cmp(&self.key)
    }
}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A sequential reader over one run, always holding its next record.
struct Cursor {
    reader: BufReader<File>,
    /// The next record to emit, or `None` at the end of the run.
    head: Option<Record>,
    left: u64,
}

impl Cursor {
    fn open(path: &Path, run: Run) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(run.offset))
            .map_err(|e| e.to_string())?;
        let mut cursor = Self {
            reader: BufReader::new(file),
            head: None,
            left: run.points,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    /// Read the next record into `head`.
    fn advance(&mut self) -> Result<(), String> {
        if self.left == 0 {
            self.head = None;
            return Ok(());
        }
        let mut bytes = [0u8; RECORD_BYTES];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|e| e.to_string())?;
        self.left -= 1;
        self.head = Some(Record::read(&bytes));
        Ok(())
    }
}

/// Points in spatial order, from memory or from the merged scratch file.
pub struct SortedPoints {
    source: Source,
    run_count: usize,
}

enum Source {
    Memory {
        records: Vec<Record>,
        at: usize,
    },
    Merged {
        cursors: Vec<Cursor>,
        heap: BinaryHeap<Head>,
        primed: bool,
        /// Owns the scratch file until the merge is done with it.
        _scratch: Scratch,
    },
}

impl SortedPoints {
    /// Runs the cloud was spilled as; `0` when it fitted in memory.
    pub fn spilled_runs(&self) -> usize {
        self.run_count
    }

    /// The next point in spatial order.
    pub fn next_point(&mut self) -> Result<Option<Point>, String> {
        match &mut self.source {
            Source::Memory { records, at } => match records.get(*at) {
                Some(record) => {
                    *at += 1;
                    Ok(Some((record.position, record.color)))
                }
                None => Ok(None),
            },
            Source::Merged {
                cursors,
                heap,
                primed,
                ..
            } => {
                if !*primed {
                    *primed = true;
                    for (index, cursor) in cursors.iter().enumerate() {
                        if let Some(head) = cursor.head {
                            heap.push(Head {
                                key: head.key,
                                cursor: index,
                            });
                        }
                    }
                }
                let Some(entry) = heap.pop() else {
                    return Ok(None);
                };
                let cursor = &mut cursors[entry.cursor];
                let record = cursor.head.ok_or("a run ran out from under the heap")?;
                cursor.advance()?;
                if let Some(head) = cursor.head {
                    heap.push(Head {
                        key: head.key,
                        cursor: entry.cursor,
                    });
                }
                Ok(Some((record.position, record.color)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::formats::types::Bounds;
    use crate::media::index::order::{Grid, sort_spatially};

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("trove-sort-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn grid_for(points: &[[f32; 3]]) -> Grid {
        let mut bounds = Bounds::empty();
        for point in points {
            bounds.extend(*point);
        }
        Grid::covering(bounds, 12)
    }

    /// A cloud walked in a scrambled order, with a colour that identifies each
    /// point so a lost or doubled point shows up.
    fn scrambled(count: usize) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
        let mut points = Vec::with_capacity(count);
        let mut colors = Vec::with_capacity(count);
        let mut state = 987654321u64;
        for index in 0..count {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let unit = |shift: u32| ((state >> shift) & 0xffff) as f32 / 65535.0 * 10.0;
            points.push([unit(0), unit(16), unit(32)]);
            colors.push([index as f32, (index % 251) as f32, 0.0]);
        }
        (points, colors)
    }

    /// A cloud small enough to fit the buffer is sorted in memory and never
    /// touches the disk.
    #[test]
    fn a_cloud_that_fits_the_buffer_never_spills() {
        let (points, colors) = scrambled(500);
        let mut sorter =
            SpatialSorter::new(grid_for(&points), 1_000, &scratch_dir("fits")).unwrap();
        sorter.push(&points, &colors).unwrap();
        let mut sorted = sorter.finish().unwrap();
        assert_eq!(sorted.spilled_runs(), 0, "nothing should have been spilled");

        let mut got = Vec::new();
        while let Some((position, _)) = sorted.next_point().unwrap() {
            got.push(position);
        }
        let expected = sort_spatially(points, colors, 12);
        assert_eq!(got, expected.points);
    }

    /// Past the buffer the cloud goes through the scratch file, and comes out
    /// in exactly the order the in-memory sort would have produced — including
    /// through the grouping pass, which is why the buffer here is tiny.
    #[test]
    fn spilling_produces_the_same_order_as_sorting_in_memory() {
        let (points, colors) = scrambled(4_000);
        // A capacity of 37 points over 4000 points is 109 runs: more than the
        // fan-in, so the merge itself has to be merged.
        let mut sorter = SpatialSorter::new(grid_for(&points), 37, &scratch_dir("spills")).unwrap();
        for batch in 0..40 {
            let from = batch * 100;
            let to = (from + 100).min(points.len());
            sorter.push(&points[from..to], &colors[from..to]).unwrap();
        }
        assert!(sorter.spilled_runs() > MERGE_FAN_IN);

        let mut sorted = sorter.finish().unwrap();
        // The final merge reads at most a fan-in of runs, however many the
        // spill produced: that is what keeps the number of open files bounded.
        assert!(
            sorted.spilled_runs() <= MERGE_FAN_IN,
            "{}",
            sorted.spilled_runs()
        );
        let mut got = Vec::new();
        let mut got_colors = Vec::new();
        while let Some((position, color)) = sorted.next_point().unwrap() {
            got.push(position);
            got_colors.push(color);
        }

        let expected = sort_spatially(points, colors, 12);
        assert_eq!(got.len(), expected.points.len(), "every point came through");
        assert_eq!(got, expected.points, "and in the same order");
        assert_eq!(got_colors, expected.colors, "with its own colour");
    }

    /// Points and their colours stay paired across the spill, which is the
    /// mistake a merge makes when it carries one array and not the other.
    #[test]
    fn spilled_points_keep_their_own_colour() {
        let (points, colors) = scrambled(1_000);
        let mut sorter =
            SpatialSorter::new(grid_for(&points), 64, &scratch_dir("colours")).unwrap();
        sorter.push(&points, &colors).unwrap();
        let mut sorted = sorter.finish().unwrap();
        let mut seen = 0;
        while let Some((position, color)) = sorted.next_point().unwrap() {
            let index = color[0] as usize;
            assert_eq!(position, points[index], "colour {index} lost its point");
            seen += 1;
        }
        assert_eq!(seen, points.len());
    }

    /// A cloud with no colours comes back with none, rather than with zeros
    /// that the writer would take for real colours.
    #[test]
    fn an_uncoloured_cloud_reports_no_colours() {
        let (points, _) = scrambled(300);
        let mut sorter = SpatialSorter::new(grid_for(&points), 50, &scratch_dir("plain")).unwrap();
        sorter.push(&points, &[]).unwrap();
        assert!(!sorter.has_colors());
        let mut sorted = sorter.finish().unwrap();
        let mut count = 0;
        while sorted.next_point().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, points.len());
    }

    /// The scratch file is a build artefact, and a large one: it is gone once
    /// the sorted points have been consumed.
    #[test]
    fn the_scratch_file_is_removed_when_the_sort_is_done() {
        let dir = scratch_dir("cleanup");
        std::fs::remove_dir_all(&dir).ok();
        let (points, colors) = scrambled(400);
        {
            let mut sorter = SpatialSorter::new(grid_for(&points), 32, &dir).unwrap();
            sorter.push(&points, &colors).unwrap();
            // Every run lives in one file, not one file per run.
            assert_eq!(
                std::fs::read_dir(&dir).unwrap().count(),
                1,
                "the runs share a scratch file"
            );
            let mut sorted = sorter.finish().unwrap();
            while sorted.next_point().unwrap().is_some() {}
        }
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "the scratch file is removed with the sorted points"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
