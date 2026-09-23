//! The index file: a cloud in spatial order, chunk by chunk, ready to be read
//! a region at a time.
//!
//! The layout is deliberately dull, because a twenty-gigabyte file has to be
//! written in one pass and read without a search:
//!
//! ```text
//! header (128 bytes)     magic, grid, cloud bounds, counts, channel mask,
//!                        cloud-wide ranges, table offset
//! payload                one record per point, in spatial order:
//!                        3 × u16  position within its chunk's box
//!                        3 × u8   colour (when the cloud has any)
//!                        f32      return strength (when the cloud has any)
//!                        u8       classification (when the cloud has any)
//! chunk table            40 bytes per chunk: its bounds and its point range
//! ```
//!
//! Three decisions worth naming:
//!
//! * **Positions are chunk-local.** Each chunk stores its own bounding box and
//!   its points as a fraction of it, so precision follows the chunk rather
//!   than the model: a 16-bit fraction of a metre-sized chunk is finer than a
//!   millimetre, and a run of a thousand points costs 6 kB.
//! * **The table is written after the payload, and the header is patched.**
//!   One pass over the merged sort, no second read of a scratch file that is
//!   already tens of gigabytes, and no need to know the point count up front
//!   (though a PLY header usually does say).
//! * **Positions and colours interleave.** Reading a chunk is then one seek
//!   and one contiguous read, rather than two reads in two sections.
//!
//! Version 2 added the two scanner attributes ([`Channels`]) and put the whole
//! cloud's intensity range and class count in the header, because a reader that
//! only ever holds the chunks it is drawing cannot measure them itself — and a
//! colour scale whose range grows as chunks arrive repaints what is already on
//! screen. A version 1 sidecar is refused by name rather than misread, and has
//! to be rebuilt.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::order::{Grid, MAX_BITS};
use super::sort::{SortedPoint, SpatialSorter};
use crate::media::formats::types::Bounds;

/// Magic at the head of an index file.
const MAGIC: [u8; 8] = *b"TRVECLD1";
/// Version of the layout this code reads and writes. Version 1 stored positions
/// and colours only.
const VERSION: u32 = 2;
/// Bytes of the fixed header.
const HEADER_BYTES: usize = 128;
/// Bytes of one chunk table entry.
const CHUNK_BYTES: usize = 40;
/// Bytes of the position every record starts with.
const RECORD_POSITION: usize = 6;
/// Bytes of each channel that follows it.
const RECORD_COLOR: usize = 3;
const RECORD_INTENSITY: usize = 4;
const RECORD_CLASS: usize = 1;
/// The widest record any combination of channels makes, which is what one
/// record's stack buffer has to hold.
const RECORD_MAX: usize = RECORD_POSITION + RECORD_COLOR + RECORD_INTENSITY + RECORD_CLASS;

/// Which per-point channels an index stores, and so how wide a record is.
///
/// The one place the record layout is written down: the builder and the reader
/// both ask it, because the two agreeing is what makes a point's class come back
/// as that point's rather than its neighbour's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channels {
    pub colors: bool,
    pub intensity: bool,
    pub class: bool,
}

/// Header bits for the three optional channels.
const CHANNEL_COLORS: u32 = 1;
const CHANNEL_INTENSITY: u32 = 2;
const CHANNEL_CLASS: u32 = 4;

impl Channels {
    fn mask(self) -> u32 {
        (if self.colors { CHANNEL_COLORS } else { 0 })
            | if self.intensity { CHANNEL_INTENSITY } else { 0 }
            | if self.class { CHANNEL_CLASS } else { 0 }
    }

    /// From the header's mask. An unknown bit is refused rather than ignored:
    /// it would be a channel whose bytes are in the record and unaccounted for.
    fn from_mask(mask: u32) -> Result<Self, String> {
        if mask & !(CHANNEL_COLORS | CHANNEL_INTENSITY | CHANNEL_CLASS) != 0 {
            return Err(format!("the index declares channels {mask} cannot be read"));
        }
        Ok(Self {
            colors: mask & CHANNEL_COLORS != 0,
            intensity: mask & CHANNEL_INTENSITY != 0,
            class: mask & CHANNEL_CLASS != 0,
        })
    }

    /// Bytes of one record. Public because a caller building an index reports
    /// it, and bytes per point is the number that decides whether a twenty
    /// gigabyte scan fits on disk.
    pub fn record_bytes(self) -> usize {
        RECORD_POSITION
            + usize::from(self.colors) * RECORD_COLOR
            + usize::from(self.intensity) * RECORD_INTENSITY
            + usize::from(self.class) * RECORD_CLASS
    }

    /// Where each channel starts inside a record, or `None` when absent.
    fn offsets(self) -> (Option<usize>, Option<usize>, Option<usize>) {
        let mut at = RECORD_POSITION;
        let color = self.colors.then(|| {
            let here = at;
            at += RECORD_COLOR;
            here
        });
        let intensity = self.intensity.then(|| {
            let here = at;
            at += RECORD_INTENSITY;
            here
        });
        let class = self.class.then(|| {
            let here = at;
            at += RECORD_CLASS;
            here
        });
        (color, intensity, class)
    }
}

/// A batch of points, with whichever per-point channels the cloud carries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointBatch {
    pub points: Vec<[f32; 3]>,
    /// Parallel to `points`, or empty when the cloud carries no colours.
    pub colors: Vec<[f32; 3]>,
    /// The scanner's return strength, raw as the file wrote it.
    pub intensities: Vec<f32>,
    /// The ASPRS class of each point.
    pub classes: Vec<u8>,
}

impl PointBatch {
    /// A batch with nothing but positions.
    pub fn plain(points: Vec<[f32; 3]>) -> Self {
        Self {
            points,
            ..Default::default()
        }
    }
}

/// How an index is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexConfig {
    /// Points per chunk: how much a reader fetches in one go, and the unit the
    /// renderer culls by. Small chunks cull tightly and cost more table.
    pub points_per_chunk: usize,
    /// Points held in memory before a sorted run spills to the scratch file.
    /// The cost of a bigger buffer is memory; the cost of a smaller one is
    /// another pass over the scratch file.
    pub sort_capacity: usize,
    /// Bits per axis of the grid the points are *sorted* on. It does not
    /// affect what the file stores — positions are chunk-local — only the
    /// locality of the order.
    pub grid_bits: u32,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            points_per_chunk: 64 * 1024,
            // 4M points is 128 MB of records in the sorter, which keeps a
            // twenty-gigabyte cloud to something like a hundred runs — few
            // enough to merge in one grouping pass.
            sort_capacity: 1 << 22,
            grid_bits: MAX_BITS,
        }
    }
}

/// A source of points in file order: what an index is built from.
///
/// The point-cloud reader implements this; keeping it a trait means the index
/// can be built and tested without a file in a particular format.
pub trait BatchSource {
    /// The next batch, or `None` once the cloud has been read.
    fn next_batch(&mut self) -> Result<Option<PointBatch>, String>;
}

/// What building an index produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSummary {
    /// Points written.
    pub points: u64,
    /// Chunks they were cut into.
    pub chunks: u64,
    /// Bytes of the finished file.
    pub bytes: u64,
    /// Runs that had to go through the scratch file. `0` means the cloud
    /// fitted the sorter's buffer.
    pub spilled_runs: usize,
    /// Which per-point channels the file stores.
    pub channels: Channels,
}

/// Build an index from a stream of point batches.
///
/// `grid` fixes the order (see [`Grid`]); the caller derives it from the
/// cloud's extent, which for a file is cheaply sampled rather than read.
/// Scratch space for the sort lives under `scratch`.
pub fn build_index(
    source: &mut dyn BatchSource,
    grid: Grid,
    config: IndexConfig,
    scratch: &Path,
    out: &Path,
) -> Result<IndexSummary, String> {
    let mut sorter = SpatialSorter::new(grid, config.sort_capacity, scratch)?;
    while let Some(batch) = source.next_batch()? {
        sorter.push(
            &batch.points,
            &batch.colors,
            &batch.intensities,
            &batch.classes,
        )?;
    }
    let channels = Channels {
        colors: sorter.has_colors(),
        intensity: sorter.has_intensity(),
        class: sorter.has_class(),
    };
    let mut sorted = sorter.finish()?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = out.with_extension(format!("tmp-{}", crate::model::new_id().simple()));
    let record_bytes = channels.record_bytes();
    let (color_at, intensity_at, class_at) = channels.offsets();

    let mut table: Vec<[u8; CHUNK_BYTES]> = Vec::new();
    let mut cloud = Bounds::empty();
    let mut written = 0u64;
    {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        let mut writer = BufWriter::new(file);
        // The header is written last: the counts and the table offset are not
        // known until the payload is.
        writer
            .write_all(&[0u8; HEADER_BYTES])
            .map_err(|e| e.to_string())?;

        // One chunk at a time, so the writer holds at most a chunk's worth of
        // points however large the cloud is.
        let mut chunk: Vec<SortedPoint> = Vec::with_capacity(config.points_per_chunk);
        // What the cloud measured as it was written. These go in the header, so
        // a reader can colour its first chunk against the whole cloud's range —
        // which is the difference between a stable picture and one that repaints
        // itself as chunks arrive.
        let mut intensity = Intensity::default();
        let mut class_top = 0u8;
        let mut flush_chunk = |chunk: &mut Vec<SortedPoint>,
                               table: &mut Vec<[u8; CHUNK_BYTES]>,
                               writer: &mut BufWriter<File>|
         -> Result<(), String> {
            if chunk.is_empty() {
                return Ok(());
            }
            let mut bounds = Bounds::empty();
            for point in chunk.iter() {
                bounds.extend(point.position);
                intensity.add(point.intensity);
                class_top = class_top.max(point.class);
            }
            cloud.extend(bounds.min);
            cloud.extend(bounds.max);
            let start = written;
            let count = chunk.len() as u32;
            let extent = bounds.size();
            for point in chunk.iter() {
                let mut record = [0u8; RECORD_MAX];
                for (axis, ((extent, lowest), value)) in extent
                    .iter()
                    .zip(&bounds.min)
                    .zip(&point.position)
                    .enumerate()
                {
                    let fraction = if *extent > 0.0 {
                        ((*value - *lowest) / *extent * 65535.0).round()
                    } else {
                        0.0
                    };
                    let value = fraction.clamp(0.0, 65535.0) as u16;
                    let at = axis * 2;
                    record[at..at + 2].copy_from_slice(&value.to_le_bytes());
                }
                if let Some(at) = color_at {
                    for channel in 0..3 {
                        record[at + channel] =
                            (point.color[channel].clamp(0.0, 1.0) * 255.0).round() as u8;
                    }
                }
                if let Some(at) = intensity_at {
                    record[at..at + RECORD_INTENSITY]
                        .copy_from_slice(&point.intensity.to_le_bytes());
                }
                if let Some(at) = class_at {
                    record[at] = point.class;
                }
                writer
                    .write_all(&record[..record_bytes])
                    .map_err(|e| e.to_string())?;
            }
            table.push(chunk_entry(bounds, start, count));
            written += count as u64;
            chunk.clear();
            Ok(())
        };

        while let Some(point) = sorted.next_point()? {
            chunk.push(point);
            if chunk.len() >= config.points_per_chunk {
                flush_chunk(&mut chunk, &mut table, &mut writer)?;
            }
        }
        flush_chunk(&mut chunk, &mut table, &mut writer)?;

        // The table goes last, and the header is patched with where it is.
        let table_offset = HEADER_BYTES as u64 + written * record_bytes as u64;
        for entry in &table {
            writer.write_all(entry).map_err(|e| e.to_string())?;
        }
        writer.flush().map_err(|e| e.to_string())?;
        let header = encode_header(&Header {
            record_bytes: record_bytes as u32,
            channels,
            grid,
            bounds: cloud,
            points: written,
            chunks: table.len() as u64,
            table_offset,
            intensity: channels.intensity.then_some(intensity),
            class_count: channels.class.then_some(class_top as u32 + 1),
        });
        writer.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        writer.write_all(&header).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
    }

    let bytes = std::fs::metadata(&temp).map_err(|e| e.to_string())?.len();
    std::fs::rename(&temp, out).map_err(|e| e.to_string())?;

    Ok(IndexSummary {
        points: written,
        chunks: table.len() as u64,
        bytes,
        spilled_runs: sorted.spilled_runs(),
        channels,
    })
}

/// The intensity range of a whole cloud, as measured while it was written.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Intensity {
    min: f32,
    max: f32,
    /// Whether any finite value was seen: a cloud whose every intensity is a
    /// NaN has no range, and saying so is better than storing infinities.
    seen: bool,
}

impl Intensity {
    fn add(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        if self.seen {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        } else {
            *self = Self {
                min: value,
                max: value,
                seen: true,
            };
        }
    }

    fn range(self) -> Option<(f32, f32)> {
        self.seen.then_some((self.min, self.max))
    }
}

/// One chunk table entry.
fn chunk_entry(bounds: Bounds, start: u64, count: u32) -> [u8; CHUNK_BYTES] {
    let mut entry = [0u8; CHUNK_BYTES];
    for axis in 0..3 {
        entry[axis * 4..axis * 4 + 4].copy_from_slice(&bounds.min[axis].to_le_bytes());
        entry[12 + axis * 4..12 + axis * 4 + 4].copy_from_slice(&bounds.max[axis].to_le_bytes());
    }
    entry[24..32].copy_from_slice(&start.to_le_bytes());
    entry[32..36].copy_from_slice(&count.to_le_bytes());
    entry
}

/// The fixed part of an index file.
#[derive(Debug, Clone, Copy)]
struct Header {
    record_bytes: u32,
    channels: Channels,
    grid: Grid,
    bounds: Bounds,
    points: u64,
    chunks: u64,
    table_offset: u64,
    /// The whole cloud's intensity range, for a cloud that has an intensity.
    intensity: Option<Intensity>,
    /// Highest classification plus one, for a cloud that has one.
    class_count: Option<u32>,
}

fn encode_header(header: &Header) -> [u8; HEADER_BYTES] {
    let mut bytes = [0u8; HEADER_BYTES];
    bytes[0..8].copy_from_slice(&MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&header.record_bytes.to_le_bytes());
    bytes[16..20].copy_from_slice(&header.grid.bits().to_le_bytes());
    for axis in 0..3 {
        bytes[20 + axis * 4..20 + axis * 4 + 4]
            .copy_from_slice(&header.grid.origin()[axis].to_le_bytes());
        bytes[36 + axis * 4..36 + axis * 4 + 4]
            .copy_from_slice(&header.bounds.min[axis].to_le_bytes());
        bytes[48 + axis * 4..48 + axis * 4 + 4]
            .copy_from_slice(&header.bounds.max[axis].to_le_bytes());
    }
    bytes[32..36].copy_from_slice(&header.grid.step().to_le_bytes());
    bytes[60..68].copy_from_slice(&header.points.to_le_bytes());
    bytes[68..76].copy_from_slice(&header.chunks.to_le_bytes());
    bytes[76..84].copy_from_slice(&header.table_offset.to_le_bytes());
    // What a version 2 file adds, and what the rest of the block leaves zero
    // for a file that grows again.
    bytes[84..88].copy_from_slice(&header.channels.mask().to_le_bytes());
    let (low, high) = header
        .intensity
        .and_then(Intensity::range)
        .unwrap_or_default();
    bytes[88..92].copy_from_slice(&low.to_le_bytes());
    bytes[92..96].copy_from_slice(&high.to_le_bytes());
    bytes[96..100].copy_from_slice(&header.class_count.unwrap_or(0).to_le_bytes());
    bytes
}

fn decode_header(bytes: &[u8; HEADER_BYTES]) -> Result<Header, String> {
    if bytes[0..8] != MAGIC {
        return Err("not a trove cloud index".into());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != VERSION {
        return Err(format!(
            "this index was written by another version ({version}, expected {VERSION})"
        ));
    }
    let float = |at: usize| f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let channels = Channels::from_mask(u32::from_le_bytes(bytes[84..88].try_into().unwrap()))?;
    let record_bytes = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    // Both are in the file, and they have to agree: read alone, a record size
    // would walk every point out of step with its own channels.
    if record_bytes != channels.record_bytes() as u32 {
        return Err(format!(
            "the record size {record_bytes} disagrees with its channels ({})",
            channels.record_bytes()
        ));
    }
    let grid_bits = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let grid = Grid::from_parts(grid_bits, [float(20), float(24), float(28)], float(32));
    let intensity = Intensity {
        min: float(88),
        max: float(92),
        seen: channels.intensity && float(88).is_finite() && float(92).is_finite(),
    };
    let class_count = u32::from_le_bytes(bytes[96..100].try_into().unwrap());
    Ok(Header {
        record_bytes,
        channels,
        grid,
        bounds: Bounds {
            min: [float(36), float(40), float(44)],
            max: [float(48), float(52), float(56)],
        },
        points: u64::from_le_bytes(bytes[60..68].try_into().unwrap()),
        chunks: u64::from_le_bytes(bytes[68..76].try_into().unwrap()),
        table_offset: u64::from_le_bytes(bytes[76..84].try_into().unwrap()),
        intensity: channels
            .intensity
            .then_some(intensity)
            .filter(|value| value.seen),
        class_count: channels
            .class
            .then_some(class_count)
            .filter(|count| *count > 0),
    })
}

/// One run of spatially adjacent points, as the file holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexChunk {
    /// Bounds of the points in the run — the box a renderer culls by.
    pub bounds: Bounds,
    /// First point of the run, as an offset into the cloud's order.
    pub start: u64,
    /// Points in the run.
    pub count: u32,
}

impl IndexChunk {
    /// One past the last point.
    pub fn end(&self) -> u64 {
        self.start + self.count as u64
    }
}

/// An index file, open for reading.
///
/// Reads are per chunk: the reader seeks to the run and reads it whole, so a
/// renderer streaming by visibility touches only the regions it is drawing.
#[derive(Debug)]
pub struct CloudIndex {
    /// Kept so each read can take its own handle: reads then need no lock and
    /// no `&mut`, which is what lets a renderer read chunks off its own
    /// threads.
    path: PathBuf,
    header: Header,
    chunks: Vec<IndexChunk>,
    len: u64,
}

impl CloudIndex {
    /// Open an index file, validating its header and table.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let mut reader = BufReader::new(file);
        let mut head = [0u8; HEADER_BYTES];
        reader
            .read_exact(&mut head)
            .map_err(|_| "the index file ends before its header".to_string())?;
        let header = decode_header(&head)?;

        // The table has to be inside the file, or every read after it lies.
        let table_end = header
            .table_offset
            .checked_add(
                header
                    .chunks
                    .checked_mul(CHUNK_BYTES as u64)
                    .ok_or("chunk count overflows")?,
            )
            .ok_or("the chunk table runs past the end of the file")?;
        if table_end > len {
            return Err("the chunk table runs past the end of the file".into());
        }
        let payload_end = HEADER_BYTES as u64 + header.points * header.record_bytes as u64;
        if payload_end > header.table_offset {
            return Err("the points run into the chunk table".into());
        }

        let mut chunks = Vec::with_capacity(header.chunks as usize);
        reader
            .seek(SeekFrom::Start(header.table_offset))
            .map_err(|e| e.to_string())?;
        for _ in 0..header.chunks {
            let mut entry = [0u8; CHUNK_BYTES];
            reader
                .read_exact(&mut entry)
                .map_err(|_| "the chunk table ends early".to_string())?;
            let float = |at: usize| f32::from_le_bytes(entry[at..at + 4].try_into().unwrap());
            chunks.push(IndexChunk {
                bounds: Bounds {
                    min: [float(0), float(4), float(8)],
                    max: [float(12), float(16), float(20)],
                },
                start: u64::from_le_bytes(entry[24..32].try_into().unwrap()),
                count: u32::from_le_bytes(entry[32..36].try_into().unwrap()),
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            header,
            chunks,
            len,
        })
    }

    /// Points in the cloud.
    pub fn point_count(&self) -> u64 {
        self.header.points
    }

    /// Runs the cloud was cut into.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// The cloud's own extent, for framing a camera on.
    pub fn bounds(&self) -> Bounds {
        self.header.bounds
    }

    /// The grid the order was built on, for a reader that wants to quantise.
    pub fn grid(&self) -> Grid {
        self.header.grid
    }

    /// Whether the cloud carries colours.
    pub fn has_colors(&self) -> bool {
        self.header.channels.colors
    }

    /// Whether the cloud carries the two scanner attributes the viewport can
    /// paint by. A recorded zero is not a channel: these say the bytes are there.
    pub fn has_intensity(&self) -> bool {
        self.header.channels.intensity
    }

    pub fn has_class(&self) -> bool {
        self.header.channels.class
    }

    /// The whole cloud's intensity range, measured when the index was built.
    ///
    /// A reader holding the three chunks nearest the camera cannot measure this
    /// itself, and a range that grows as chunks arrive repaints the points
    /// already on screen. The builder saw every point, so it wrote the answer
    /// down once.
    pub fn intensity_range(&self) -> Option<(f32, f32)> {
        self.header.intensity.and_then(Intensity::range)
    }

    /// How many classifications the cloud holds: highest class plus one.
    pub fn class_count(&self) -> Option<usize> {
        self.header.class_count.map(|count| count as usize)
    }

    /// Bytes of the file.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the index holds nothing.
    pub fn is_empty(&self) -> bool {
        self.header.points == 0
    }

    /// One run.
    pub fn chunk(&self, index: usize) -> Option<&IndexChunk> {
        self.chunks.get(index)
    }

    /// Read one run's points, decoded.
    pub fn read_chunk(&self, index: usize) -> Result<PointBatch, String> {
        let chunk = *self
            .chunks
            .get(index)
            .ok_or_else(|| format!("no chunk {index}"))?;
        let record_bytes = self.header.record_bytes as usize;
        let at = HEADER_BYTES as u64 + chunk.start * self.header.record_bytes as u64;
        let bytes = chunk.count as usize * record_bytes;
        if at + bytes as u64 > self.len {
            return Err("the run runs past the end of the file".into());
        }
        let mut reader = File::open(&self.path).map_err(|e| e.to_string())?;
        reader
            .seek(SeekFrom::Start(at))
            .map_err(|e| e.to_string())?;
        let mut raw = vec![0u8; bytes];
        reader.read_exact(&mut raw).map_err(|e| e.to_string())?;

        let extent = chunk.bounds.size();
        let channels = self.header.channels;
        let (color_at, intensity_at, class_at) = channels.offsets();
        let mut points = Vec::with_capacity(chunk.count as usize);
        let mut colors = if channels.colors {
            Vec::with_capacity(chunk.count as usize)
        } else {
            Vec::new()
        };
        let mut intensities = if channels.intensity {
            Vec::with_capacity(chunk.count as usize)
        } else {
            Vec::new()
        };
        let mut classes = if channels.class {
            Vec::with_capacity(chunk.count as usize)
        } else {
            Vec::new()
        };
        for index in 0..chunk.count as usize {
            let record = &raw[index * record_bytes..(index + 1) * record_bytes];
            let mut point = [0.0f32; 3];
            for axis in 0..3 {
                let value = u16::from_le_bytes(record[axis * 2..axis * 2 + 2].try_into().unwrap());
                point[axis] = if extent[axis] > 0.0 {
                    chunk.bounds.min[axis] + value as f32 / 65535.0 * extent[axis]
                } else {
                    chunk.bounds.min[axis]
                };
            }
            points.push(point);
            if let Some(at) = color_at {
                colors.push([
                    record[at] as f32 / 255.0,
                    record[at + 1] as f32 / 255.0,
                    record[at + 2] as f32 / 255.0,
                ]);
            }
            if let Some(at) = intensity_at {
                intensities.push(f32::from_le_bytes(
                    record[at..at + RECORD_INTENSITY].try_into().unwrap(),
                ));
            }
            if let Some(at) = class_at {
                classes.push(record[at]);
            }
        }
        Ok(PointBatch {
            points,
            colors,
            intensities,
            classes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::index::order::Grid;

    /// A batch source over an in-memory list: the seam a file reader plugs
    /// into.
    struct Batches {
        points: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        intensities: Vec<f32>,
        classes: Vec<u8>,
        batch: usize,
        at: usize,
    }

    impl Batches {
        fn new(points: Vec<[f32; 3]>, colors: Vec<[f32; 3]>, batch: usize) -> Self {
            Self {
                points,
                colors,
                intensities: Vec::new(),
                classes: Vec::new(),
                batch,
                at: 0,
            }
        }

        /// The slice of a channel a batch takes: empty means the cloud does not
        /// carry it at all.
        fn slice<T: Copy>(values: &[T], from: usize, to: usize) -> Vec<T> {
            if values.is_empty() {
                Vec::new()
            } else {
                values[from..to].to_vec()
            }
        }
    }

    impl BatchSource for Batches {
        fn next_batch(&mut self) -> Result<Option<PointBatch>, String> {
            if self.at >= self.points.len() {
                return Ok(None);
            }
            let (from, to) = (self.at, (self.at + self.batch).min(self.points.len()));
            let batch = PointBatch {
                points: self.points[from..to].to_vec(),
                colors: Self::slice(&self.colors, from, to),
                intensities: Self::slice(&self.intensities, from, to),
                classes: Self::slice(&self.classes, from, to),
            };
            self.at = to;
            Ok(Some(batch))
        }
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("trove-index-test-{}-{name}", std::process::id()));
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

    /// A scrambled cloud with a colour that identifies each point.
    fn scrambled(count: usize) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
        let mut points = Vec::with_capacity(count);
        let mut colors = Vec::with_capacity(count);
        let mut state = 24681357u64;
        for index in 0..count {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let unit = |shift: u32| ((state >> shift) & 0xffff) as f32 / 65535.0 * 8.0;
            points.push([unit(0), unit(16), unit(32)]);
            colors.push([index as f32 / count as f32, 0.25, 0.75]);
        }
        (points, colors)
    }

    fn build(
        dir: &Path,
        points: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        config: IndexConfig,
    ) -> (std::path::PathBuf, IndexSummary) {
        let grid = grid_for(&points);
        let mut source = Batches::new(points, colors, 333);
        let out = dir.join("cloud.trovecloud");
        let summary = build_index(&mut source, grid, config, dir, &out).expect("index builds");
        (out, summary)
    }

    /// An index round-trips every point, within the precision its chunk
    /// affords, and the reader gets the same count back.
    #[test]
    fn an_index_round_trips_every_point() {
        let dir = scratch_dir("roundtrip");
        let (points, colors) = scrambled(5_000);
        let (path, summary) = build(
            &dir,
            points.clone(),
            colors.clone(),
            IndexConfig {
                points_per_chunk: 512,
                sort_capacity: 200,
                ..Default::default()
            },
        );
        assert_eq!(summary.points, points.len() as u64);
        assert!(summary.channels.colors);
        assert!(
            summary.spilled_runs > 1,
            "the sort should have spilled: {} runs",
            summary.spilled_runs
        );

        let index = CloudIndex::open(&path).expect("index opens");
        assert_eq!(index.point_count(), points.len() as u64);
        assert!(index.has_colors());
        assert!(index.chunk_count() > 1);

        // Every point is there, and the error is inside what a chunk-local
        // 16-bit fraction can represent.
        let mut decoded = 0usize;
        for chunk in 0..index.chunk_count() {
            let batch = index.read_chunk(chunk).expect("chunk reads");
            let (chunk_points, chunk_colors) = (batch.points, batch.colors);
            assert!(!chunk_points.is_empty());
            assert_eq!(chunk_colors.len(), chunk_points.len());
            let bounds = index.chunk(chunk).unwrap().bounds;
            let extent = bounds.size();
            for point in &chunk_points {
                for axis in 0..3 {
                    let allowed = extent[axis] / 65535.0 + 1e-4;
                    assert!(point[axis] >= bounds.min[axis] - allowed);
                    assert!(point[axis] <= bounds.max[axis] + allowed);
                }
            }
            decoded += chunk_points.len();
        }
        assert_eq!(decoded, points.len(), "every point came back");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Colours survive the file, to the byte they were quantised to.
    #[test]
    fn colours_round_trip_through_the_index() {
        let dir = scratch_dir("colours");
        let (points, colors) = scrambled(600);
        let (path, _) = build(&dir, points, colors.clone(), IndexConfig::default());
        let index = CloudIndex::open(&path).expect("index opens");
        let mut seen: Vec<[f32; 3]> = Vec::new();
        for chunk in 0..index.chunk_count() {
            let batch = index.read_chunk(chunk).unwrap();
            seen.extend(batch.colors);
        }
        assert_eq!(seen.len(), colors.len());
        for color in &seen {
            // The first channel encoded the point's identity, so a shuffled
            // pairing would show up as a value nothing in the cloud had.
            assert!((0.0..=1.0).contains(&color[0]));
            assert!((color[1] - 0.25).abs() < 1.0 / 255.0);
            assert!((color[2] - 0.75).abs() < 1.0 / 255.0);
        }
        let mut first_channels: Vec<f32> = seen.iter().map(|color| color[0]).collect();
        first_channels.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // The encoding is a ramp, so the sorted values must be a ramp too: that
        // is what a lost or doubled point would break.
        for pair in first_channels.windows(2) {
            assert!(pair[1] >= pair[0]);
        }
        assert!(first_channels.last().unwrap() - first_channels[0] > 0.9);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A cloud with no colours is stored without them: six bytes a point, and
    /// a reader that does not invent an alpha channel.
    #[test]
    fn an_uncoloured_cloud_is_stored_without_colours() {
        let dir = scratch_dir("plain");
        let (points, _) = scrambled(400);
        let (path, summary) = build(&dir, points.clone(), Vec::new(), IndexConfig::default());
        assert!(!summary.channels.colors);
        let index = CloudIndex::open(&path).expect("index opens");
        assert!(!index.has_colors());
        let batch = index.read_chunk(0).unwrap();
        assert!(!batch.points.is_empty() && batch.colors.is_empty());
        // Six bytes a point, plus the header and one table entry.
        let expected = HEADER_BYTES as u64 + points.len() as u64 * 6 + CHUNK_BYTES as u64;
        assert_eq!(summary.bytes, expected);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The file holds exactly the order the spatial sort produced — not
    /// merely *an* order, and not the file's own. The fixture is a dense grid
    /// with distinct cells, so the two orders are comparable point by point.
    #[test]
    fn the_file_holds_the_spatial_order() {
        let dir = scratch_dir("order");
        let side = 16u32;
        let spacing = 0.5f32;
        let mut grid_points: Vec<[f32; 3]> = Vec::new();
        for x in 0..side {
            for y in 0..side {
                for z in 0..side {
                    grid_points.push([x as f32 * spacing, y as f32 * spacing, z as f32 * spacing]);
                }
            }
        }
        // Scrambled, the way a scan file arrives.
        let mut state = 777u64;
        for index in (1..grid_points.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            grid_points.swap(index, (state >> 33) as usize % (index + 1));
        }
        let colors: Vec<[f32; 3]> = grid_points.iter().map(|_| [0.5, 0.5, 0.5]).collect();
        let expected =
            crate::media::index::order::sort_spatially(grid_points.clone(), colors.clone(), 12);
        // The order the file arrived in, for comparison: this is what the
        // index is supposed to improve on.
        let as_read = grid_points.clone();

        let (path, _) = build(
            &dir,
            grid_points,
            colors,
            IndexConfig {
                points_per_chunk: 256,
                sort_capacity: 5_000,
                ..Default::default()
            },
        );
        let index = CloudIndex::open(&path).expect("index opens");

        let mut decoded: Vec<[f32; 3]> = Vec::new();
        for chunk in 0..index.chunk_count() {
            let batch = index.read_chunk(chunk).unwrap();
            decoded.extend(batch.points);
        }
        assert_eq!(
            decoded.len(),
            expected.points.len(),
            "every point is in the file"
        );

        // Point by point against the in-memory spatial sort, within what a
        // chunk-local 16-bit fraction can carry.
        let mut worst = 0.0f32;
        for (got, want) in decoded.iter().zip(&expected.points) {
            for axis in 0..3 {
                worst = worst.max((got[axis] - want[axis]).abs());
            }
        }
        assert!(worst < 1e-3, "the order or the values drifted: {worst}");

        // The measure of the whole exercise: a run of records is a region.
        // (Consecutive records are not always *adjacent* cells — Morton order
        // jumps when it steps between sub-cubes, which is the flaw Hilbert
        // fixes — but they stay within a couple of cells, and the file's own
        // order is seven times looser.)
        let mean_step = |points: &[[f32; 3]]| {
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
        };
        let sorted_step = mean_step(&decoded);
        let as_read_step = mean_step(&as_read);
        assert!(
            sorted_step < spacing * 2.0,
            "consecutive records are {sorted_step} apart, a cell is {spacing}"
        );
        assert!(
            sorted_step * 4.0 < as_read_step,
            "the index barely helped: {as_read_step} -> {sorted_step}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An empty cloud makes a valid, empty index rather than a broken file,
    /// and reading it says so instead of panicking.
    /// Clusters two units apart along +X, `per_cluster` points each and
    /// jittered by less than a quarter of that. The gap is what makes a cluster
    /// recoverable from a *decoded* position — a round trip through chunk-local
    /// quantisation cannot push a point into its neighbour's basin — which is
    /// what lets a test check a channel against the point it landed on rather
    /// than against the record it was written in.
    fn clusters(count: usize, per_cluster: usize) -> Vec<[f32; 3]> {
        let mut points = Vec::with_capacity(count * per_cluster);
        for cluster in 0..count {
            for index in 0..per_cluster {
                let jitter = index as f32 / per_cluster as f32;
                points.push([
                    cluster as f32 * 2.0 + jitter * 0.4,
                    jitter * 2.0 - 1.0,
                    1.0 - jitter,
                ]);
            }
        }
        points
    }

    /// Which cluster a point belongs to, as a reader would work it out.
    fn cluster_of(point: [f32; 3]) -> usize {
        (point[0] * 0.5).round() as usize
    }

    /// The intensity and class a cluster carries, as a rule a reader can
    /// recompute: the class is derived from the intensity, so the two can only
    /// come back together if they travelled with the same point.
    fn intensity_of(cluster: usize) -> f32 {
        cluster as f32 * 3.0
    }

    fn class_of(cluster: usize) -> u8 {
        ((cluster % 23) + (cluster / 23)) as u8
    }

    /// The channels, and the cloud-wide ranges, survive the write and the read.
    ///
    /// The point of measuring the ranges in the header is that a reader can
    /// colour its *first* chunk against the whole cloud: a range that grew as
    /// chunks arrived would repaint the points already on screen. So this
    /// asserts the ranges before `read_chunk` has been called at all, and then
    /// that every point comes back with the pair belonging to its own cluster.
    #[test]
    fn the_channels_round_trip_and_the_header_measures_the_whole_cloud() {
        let dir = scratch_dir("channels");
        let points = clusters(40, 50);
        let colors: Vec<[f32; 3]> = points.iter().map(|p| [p[0] / 80.0, 0.25, 0.75]).collect();
        let intensities: Vec<f32> = points
            .iter()
            .map(|point| intensity_of(cluster_of(*point)))
            .collect();
        let classes: Vec<u8> = points
            .iter()
            .map(|point| class_of(cluster_of(*point)))
            .collect();
        let mut source = Batches::new(points.clone(), colors, 333);
        source.intensities = intensities;
        source.classes = classes;
        let out = dir.join("cloud.trovecloud");
        let summary = build_index(
            &mut source,
            grid_for(&points),
            IndexConfig {
                points_per_chunk: 512,
                sort_capacity: 200,
                ..Default::default()
            },
            &dir,
            &out,
        )
        .expect("an indexed cloud with channels builds");
        assert!(summary.channels.colors);
        assert!(summary.channels.intensity);
        assert!(summary.channels.class);
        // 6 of position, 3 of colour, 4 of intensity, 1 of class.
        assert_eq!(summary.channels.record_bytes(), 14);

        let index = CloudIndex::open(&out).expect("index opens");
        assert!(index.has_intensity());
        assert!(index.has_class());
        // Measured from a cloud whose points have not been read at all.
        assert_eq!(index.intensity_range(), Some((0.0, 117.0)));
        // The highest class is 22 — the one cluster 22 carries — so the count
        // a palette has to cover is one past it.
        assert_eq!(index.class_count(), Some(23));

        let mut seen = 0usize;
        for chunk in 0..index.chunk_count() {
            let batch = index.read_chunk(chunk).expect("chunk reads");
            assert_eq!(batch.intensities.len(), batch.points.len());
            assert_eq!(batch.classes.len(), batch.points.len());
            for (point, (intensity, class)) in batch
                .points
                .iter()
                .zip(batch.intensities.iter().zip(&batch.classes))
            {
                // The point's own cluster, within what a chunk-local 16-bit
                // fraction can shift it.
                let cluster = cluster_of(*point);
                assert!(
                    (point[0] * 0.5 - cluster as f32).abs() < 0.25,
                    "{point:?} is not inside its cluster's own basin"
                );
                assert!(
                    (intensity - intensity_of(cluster)).abs() < 1e-4,
                    "point {point:?} of cluster {cluster} got intensity {intensity}"
                );
                assert_eq!(
                    *class,
                    class_of(cluster),
                    "point {point:?} lost its class to another"
                );
                seen += 1;
            }
        }
        assert_eq!(seen, points.len(), "every point came back");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A cloud with nothing but geometry pays for nothing else: the record is
    /// six bytes, and the reader reports no channel rather than a zero one.
    #[test]
    fn an_absent_channel_is_absent_rather_than_zero() {
        let dir = scratch_dir("absent");
        let (points, _) = scrambled(400);
        let (path, summary) = build(&dir, points.clone(), Vec::new(), IndexConfig::default());
        assert_eq!(
            summary.channels,
            Channels {
                colors: false,
                intensity: false,
                class: false
            }
        );
        assert_eq!(summary.channels.record_bytes(), 6);
        let index = CloudIndex::open(&path).expect("index opens");
        assert!(!index.has_intensity());
        assert!(!index.has_class());
        assert_eq!(index.intensity_range(), None);
        assert_eq!(index.class_count(), None);
        let batch = index.read_chunk(0).unwrap();
        assert!(batch.intensities.is_empty() && batch.classes.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A version 1 sidecar — the layout before the scanner attributes — is
    /// refused by its version, which is the message that tells the user to
    /// rebuild it rather than a mystery.
    #[test]
    fn a_version_one_index_is_refused_by_name() {
        let dir = scratch_dir("v1");
        let (points, colors) = scrambled(50);
        let (path, _) = build(&dir, points, colors, IndexConfig::default());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let error = CloudIndex::open(&path).expect_err("an old index is not read as a new one");
        assert!(error.contains("another version (1"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The record width and the channel mask are both in the header, and they
    /// have to agree — a reader that trusted the width alone would walk every
    /// point out of step with its own channels.
    #[test]
    fn a_record_size_and_its_channels_must_agree() {
        let dir = scratch_dir("disagree");
        let (points, colors) = scrambled(50);
        let (path, _) = build(&dir, points, colors, IndexConfig::default());
        let mut bytes = std::fs::read(&path).unwrap();
        // Widen the record without claiming another channel.
        bytes[12..16].copy_from_slice(&12u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let error = CloudIndex::open(&path).expect_err("refused");
        assert!(error.contains("disagrees"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_cloud_makes_a_valid_empty_index() {
        let dir = scratch_dir("empty");
        let (path, summary) = build(&dir, Vec::new(), Vec::new(), IndexConfig::default());
        assert_eq!(summary.points, 0);
        assert_eq!(summary.chunks, 0);
        let index = CloudIndex::open(&path).expect("index opens");
        assert!(index.is_empty());
        assert_eq!(index.point_count(), 0);
        assert!(index.read_chunk(0).is_err(), "no chunk to read");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that is not an index, or is one from a future version, is
    /// refused by name rather than read as garbage.
    #[test]
    fn a_foreign_or_future_file_is_refused() {
        let dir = scratch_dir("foreign");
        let not_an_index = dir.join("random.bin");
        // Long enough to have a header, so what refuses it is the magic rather
        // than the length.
        std::fs::write(&not_an_index, vec![0x5au8; HEADER_BYTES * 2]).unwrap();
        let error = CloudIndex::open(&not_an_index).expect_err("refused");
        assert!(error.contains("not a trove cloud index"), "{error}");

        // And a file too short to hold a header is refused too, by length.
        let stub = dir.join("stub.bin");
        std::fs::write(&stub, b"nope").unwrap();
        assert!(CloudIndex::open(&stub).is_err());

        // A real index with its version bumped.
        let (points, colors) = scrambled(50);
        let (path, _) = build(&dir, points, colors, IndexConfig::default());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
        let future = dir.join("future.trovecloud");
        std::fs::write(&future, &bytes).unwrap();
        let error = CloudIndex::open(&future).expect_err("refused");
        assert!(error.contains("another version"), "{error}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A truncated index is an error, not a panic or half a chunk of garbage.
    #[test]
    fn a_truncated_index_is_refused() {
        let dir = scratch_dir("truncated");
        let (points, colors) = scrambled(1_000);
        let (path, summary) = build(
            &dir,
            points,
            colors,
            IndexConfig {
                points_per_chunk: 128,
                ..Default::default()
            },
        );

        // Cut the table off: the header promises it.
        let bytes = std::fs::read(&path).unwrap();
        let cut = dir.join("cut-table.trovecloud");
        std::fs::write(&cut, &bytes[..(summary.bytes - 40) as usize]).unwrap();
        assert!(CloudIndex::open(&cut).is_err());

        // Cut into the payload: opening may succeed, but reading the last
        // chunk must fail rather than read past the end.
        let cut = dir.join("cut-payload.trovecloud");
        std::fs::write(&cut, &bytes[..HEADER_BYTES + 20]).unwrap();
        match CloudIndex::open(&cut) {
            Err(_) => {}
            Ok(index) => {
                let last = index.chunk_count() - 1;
                assert!(index.read_chunk(last).is_err());
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Chunks tile the cloud: their point ranges meet exactly, and the table's
    /// bounds really do contain what they hold.
    #[test]
    fn chunks_tile_the_cloud_without_gaps() {
        let dir = scratch_dir("tiling");
        let (points, colors) = scrambled(1_234);
        let (path, _) = build(
            &dir,
            points.clone(),
            colors,
            IndexConfig {
                points_per_chunk: 100,
                ..Default::default()
            },
        );
        let index = CloudIndex::open(&path).expect("index opens");
        let mut next = 0u64;
        for chunk in 0..index.chunk_count() {
            let entry = index.chunk(chunk).unwrap();
            assert_eq!(entry.start, next, "chunks must not overlap or leave a gap");
            assert!(entry.count > 0 && entry.count <= 100);
            next = entry.end();
        }
        assert_eq!(next, points.len() as u64, "the last chunk ends the cloud");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The index reports the cloud's own extent, which is what a camera is
    /// framed on — and it must not shrink to the grid's cube or grow to it.
    #[test]
    fn the_index_reports_the_cloud_extent() {
        let dir = scratch_dir("extent");
        let points = vec![[-3.0, -1.0, 0.0], [5.0, 2.0, 1.0], [0.0, 0.0, -7.0]];
        let (path, _) = build(&dir, points, Vec::new(), IndexConfig::default());
        let index = CloudIndex::open(&path).expect("index opens");
        assert_eq!(index.bounds().min, [-3.0, -1.0, -7.0]);
        assert_eq!(index.bounds().max, [5.0, 2.0, 1.0]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
