//! Incremental PLY point-cloud file loader with octree LOD.
//!
//! Opens a PLY file, parses the header with the same code the whole-file
//! loader uses ([`crate::media::formats::ply::parse_ply_header`]), then yields
//! chunks of points via [`StreamChunk`]. Each chunk can be fed into an octree
//! incrementally.
//!
//! Property handling is deliberately shared with `ply`: the columns are
//! resolved by name and the byte offsets and declared types come from the
//! header, so a record with extra fields (`nx ny nz`, an intensity, a scalar
//! grid) or `uchar` colours is read correctly instead of being assumed to be
//! `x y z [r g b]` at fixed offsets. A file whose vertex element cannot be
//! strided — a list property, or the vertex element behind another element —
//! is refused, and the caller falls back to the whole-file loader.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::media::formats::ply::{self, PlyEndian, PlyType};
use crate::media::formats::types::Bounds;

/// A PLY header larger than this is not a PLY header; refusing keeps a
/// mislabelled file from being read to the end one line at a time.
const MAX_HEADER_BYTES: usize = 1 << 20;

/// A bounded sample of a file's points, with their colours when it has any.
///
/// This is what a thumbnail of a file too large to parse is drawn from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SampledPoints {
    pub positions: Vec<[f32; 3]>,
    /// Parallel to `positions`; empty when the file carries no colours.
    pub colors: Vec<[f32; 3]>,
}

/// One chunk of loaded points, ready to be inserted into the octree.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Points in this chunk (model space).
    pub positions: Vec<[f32; 3]>,
    /// Optional per-point colors (RGB 0..1), parallel to positions.
    pub colors: Vec<[f32; 3]>,
    /// Whether this is the final chunk.
    pub is_last: bool,
}

/// Progress of a streaming load operation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadProgress {
    /// Bytes read so far.
    pub bytes_read: u64,
    /// Total file size in bytes.
    pub total_bytes: u64,
    /// Points loaded so far.
    pub points_loaded: usize,
    /// Whether loading is complete.
    pub complete: bool,
}

impl LoadProgress {
    /// Fraction of file loaded [0, 1].
    pub fn ratio(&self) -> f32 {
        if self.total_bytes == 0 {
            return 1.0;
        }
        (self.bytes_read as f32 / self.total_bytes as f32).clamp(0.0, 1.0)
    }
}

/// Incremental PLY point-cloud loader.
pub struct PointStreamer {
    reader: BufReader<File>,
    format: PlyFormat,
    vertex_count: usize,
    vertices_read: usize,
    /// Where the interesting values sit inside one vertex record.
    channels: VertexChannels,
    /// Byte width of one binary vertex record; unused for ASCII.
    stride: usize,
    /// Whether the header declares a `face` element. The streamer reads a
    /// surface's vertices as happily as a cloud's — it is the caller that
    /// decides whether a surface belongs here.
    declares_faces: bool,
    /// Bytes the header occupied, so progress can be measured from the body.
    body_start: u64,
    /// Bytes of ASCII body consumed so far.
    ascii_bytes: u64,
    /// Bounds of the points this stream has produced so far.
    bounds: Bounds,
    /// Reusable binary record buffer, so a chunk does not allocate per row.
    row: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlyFormat {
    Ascii,
    BinaryLittleEndian,
    BinaryBigEndian,
}

/// The vertex element's layout: which property holds which channel, and —
/// for binary bodies — where each property starts and what it is declared as.
struct VertexChannels {
    /// Property index of `x`, `y`, `z`.
    position: [usize; 3],
    /// Property indices of `red`, `green`, `blue` (or `diffuse_*`).
    colors: Option<[usize; 3]>,
    /// Byte offset of every property inside a fixed-size binary record.
    offsets: Vec<usize>,
    /// Declared type of every property, for colour normalisation.
    types: Vec<PlyType>,
}

impl VertexChannels {
    /// The position of `index` in a binary record, with its type.
    fn binary(&self, index: usize, order: PlyEndian, row: &[u8]) -> f64 {
        ply::scalar_row(row, self.offsets[index], self.types[index], order)
    }

    /// One colour channel of a binary record, normalised to 0..=1.
    fn binary_color(&self, index: usize, order: PlyEndian, row: &[u8]) -> f32 {
        ply::color_row(row, self.offsets[index], self.types[index], order)
    }
}

impl PointStreamer {
    /// Open a PLY file and parse its header.
    ///
    /// Fails for a file this reader cannot walk — one with a `face` element,
    /// a vertex element that is not first, or a vertex record that is not a
    /// fixed size — so the caller can use the whole-file loader instead.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<(Self, usize), String> {
        let file = File::open(path.as_ref()).map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(file);

        let (header, body_start) = read_header(&mut reader)?;
        let (endian, elements) = ply::parse_ply_header(&header)?;

        // Only a face element that declares records makes this a surface. A
        // `face 0` element is a point cloud an exporter tagged with an empty
        // face list, and it belongs on the streaming path rather than being
        // bounced to the mesh fallback.
        let declares_faces = elements
            .iter()
            .any(|element| element.name == "face" && element.count > 0);
        let vertex = elements
            .iter()
            .find(|element| element.name == "vertex")
            .ok_or("the PLY has no vertex element")?;
        let columns =
            ply::vertex_columns(&vertex.properties).ok_or("the PLY vertex element has no x/y/z")?;

        let format = match endian {
            None => PlyFormat::Ascii,
            Some(PlyEndian::Little) => PlyFormat::BinaryLittleEndian,
            Some(PlyEndian::Big) => PlyFormat::BinaryBigEndian,
        };
        // The binary reader strides records straight after the header, so
        // anything declared before `vertex` would be read as its points.
        if format != PlyFormat::Ascii
            && elements.first().map(|element| element.name.as_str()) != Some("vertex")
        {
            return Err("the PLY vertex element is not the first element".into());
        }
        let stride = ply::fixed_row_stride(&vertex.properties)
            .ok_or("the PLY vertex has a list property")?;
        let offsets = ply::property_offsets(&vertex.properties);
        let types: Vec<PlyType> = vertex
            .properties
            .iter()
            .map(|property| property.ty)
            .collect();

        Ok((
            Self {
                reader,
                format,
                vertex_count: vertex.count,
                vertices_read: 0,
                channels: VertexChannels {
                    position: [columns.x, columns.y, columns.z],
                    colors: columns.colors,
                    offsets,
                    types,
                },
                stride,
                declares_faces,
                body_start,
                ascii_bytes: 0,
                bounds: Bounds::empty(),
                row: vec![0u8; stride],
            },
            vertex.count,
        ))
    }

    /// Whether the file declares faces, i.e. is a surface rather than a cloud.
    pub fn declares_faces(&self) -> bool {
        self.declares_faces
    }

    /// Walk `samples` records spread evenly over the body, handing each one's
    /// position (and colour, when the file has any) to `visit`.
    ///
    /// Records are read one seek at a time, which is the price of an even
    /// spread: runs of consecutive records would be cheaper on a spinning disk
    /// but would sample the cloud in clumps — an extent estimate from clumps
    /// is too small, and a thumbnail drawn from them is missing whole regions
    /// of the model. Even a few thousand seeks are nothing next to the pass
    /// over the file they replace.
    ///
    /// `None` for a file whose records cannot be seeked to (ASCII PLY) or that
    /// holds no vertices.
    fn walk_samples(
        &mut self,
        samples: usize,
        mut visit: impl FnMut([f32; 3], Option<[f32; 3]>),
    ) -> Option<()> {
        let order = match self.format {
            PlyFormat::BinaryLittleEndian => PlyEndian::Little,
            PlyFormat::BinaryBigEndian => PlyEndian::Big,
            // Variable-length records: no way to seek to the n-th one.
            PlyFormat::Ascii => return None,
        };
        let count = self.vertex_count as u64;
        if count == 0 || samples == 0 || self.stride == 0 {
            return None;
        }
        // Leave the reader where it was: this is a side look, not a read.
        let here = self.reader.stream_position().ok()?;
        let stride = (count / samples as u64).max(1);
        // The walk plus the last record: the walk stops one stride short of
        // the end, and a file whose extremes sit there would be missed.
        let last = count - 1;
        let mut index = 0u64;
        let mut sampled_last = false;
        loop {
            let at = self.body_start + index * self.stride as u64;
            if self.reader.seek(SeekFrom::Start(at)).is_err()
                || self.reader.read_exact(&mut self.row).is_err()
            {
                break;
            }
            let position = self
                .channels
                .position
                .map(|column| self.channels.binary(column, order, &self.row) as f32);
            let color = self.channels.colors.map(|[r, g, b]| {
                [r, g, b].map(|column| self.channels.binary_color(column, order, &self.row))
            });
            visit(position, color);
            sampled_last |= index == last;
            if index + stride >= count {
                break;
            }
            index += stride;
        }
        if !sampled_last {
            let at = self.body_start + last * self.stride as u64;
            if self.reader.seek(SeekFrom::Start(at)).is_ok()
                && self.reader.read_exact(&mut self.row).is_ok()
            {
                let position = self
                    .channels
                    .position
                    .map(|column| self.channels.binary(column, order, &self.row) as f32);
                let color = self.channels.colors.map(|[r, g, b]| {
                    [r, g, b].map(|column| self.channels.binary_color(column, order, &self.row))
                });
                visit(position, color);
            }
        }
        let _ = self.reader.seek(SeekFrom::Start(here));
        Some(())
    }

    /// The whole file's extent, measured from a sample of its records.
    ///
    /// The viewport frames the camera on this, so it needs it *before* the
    /// cloud is loaded: the renderable mesh is rebuilt from a camera-dependent
    /// subset of the points every frame, so its own bounding box moves when
    /// the user turns the camera — framing on that rescales the model as it is
    /// rotated, and again every time another chunk arrives.
    ///
    /// The result approximates the cloud's extent rather than bounding it
    /// exactly — a lone outlier between two samples is not seen. That is the
    /// right trade for framing (the caller grows the frame if the loaded data
    /// turns out to reach further), and it is what makes the estimate cost a
    /// few thousand reads instead of a pass over twenty gigabytes.
    pub fn sample_bounds(&mut self, samples: usize) -> Option<Bounds> {
        let mut bounds = Bounds::empty();
        self.walk_samples(samples, |position, _| bounds.extend(position))?;
        (!bounds.is_empty()).then_some(bounds)
    }

    /// A bounded sample of the file's positions, spread evenly across it.
    ///
    /// This is what a thumbnail of a huge file is drawn from: a twenty-
    /// gigabyte scan has to be summarised from a few thousand reads, not from
    /// a pass over the whole file. Colours come along when the file has them,
    /// so a scan's card is not grey.
    pub fn sample_points(&mut self, samples: usize) -> Option<SampledPoints> {
        let has_colors = self.channels.colors.is_some();
        let mut positions = Vec::with_capacity(samples);
        let mut colors = if has_colors {
            Vec::with_capacity(samples)
        } else {
            Vec::new()
        };
        self.walk_samples(samples, |position, color| {
            positions.push(position);
            if let Some(color) = color {
                colors.push(color);
            }
        })?;
        (!positions.is_empty()).then_some(SampledPoints { positions, colors })
    }

    /// Total number of vertices in the file.
    pub fn vertex_count(&self) -> usize {
        self.vertex_count
    }

    /// Total bytes of the file, for progress reporting.
    pub fn total_bytes(&self) -> u64 {
        self.reader
            .get_ref()
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Number of vertices read so far.
    pub fn vertices_read(&self) -> usize {
        self.vertices_read
    }

    /// Whether the file declares printable per-point colours.
    pub fn has_colors(&self) -> bool {
        self.channels.colors.is_some()
    }

    /// Read the next chunk of at most `max_points` points.
    pub fn next_chunk(&mut self, max_points: usize) -> Result<Option<StreamChunk>, String> {
        if self.vertices_read >= self.vertex_count || max_points == 0 {
            return Ok(None);
        }

        let to_read = (self.vertex_count - self.vertices_read).min(max_points);
        let has_colors = self.has_colors();
        let mut positions = Vec::with_capacity(to_read);
        let mut colors = if has_colors {
            Vec::with_capacity(to_read)
        } else {
            Vec::new()
        };

        match self.format {
            PlyFormat::Ascii => {
                for _ in 0..to_read {
                    let Some(line) = next_record_line(&mut self.reader)? else {
                        return Err(format!(
                            "the PLY body ended after {} of {} vertices",
                            self.vertices_read, self.vertex_count
                        ));
                    };
                    self.ascii_bytes += line.len() as u64 + 1;
                    let scalars = tokenise(&line);
                    let position = self
                        .channels
                        .position
                        .map(|index| scalars.get(index).copied().unwrap_or(0.0) as f32);
                    positions.push(position);
                    if let Some([r, g, b]) = self.channels.colors {
                        colors.push([r, g, b].map(|index| {
                            let value = scalars.get(index).copied().unwrap_or(0.0);
                            ply::unit_colour(value, self.channels.types[index])
                        }));
                    }
                    self.vertices_read += 1;
                }
            }
            PlyFormat::BinaryLittleEndian => {
                self.read_binary(PlyEndian::Little, to_read, &mut positions, &mut colors)?;
            }
            PlyFormat::BinaryBigEndian => {
                self.read_binary(PlyEndian::Big, to_read, &mut positions, &mut colors)?;
            }
        }

        for position in &positions {
            self.bounds.extend(*position);
        }
        let is_last = self.vertices_read >= self.vertex_count;
        Ok(Some(StreamChunk {
            positions,
            colors,
            is_last,
        }))
    }

    fn read_binary(
        &mut self,
        order: PlyEndian,
        count: usize,
        positions: &mut Vec<[f32; 3]>,
        colors: &mut Vec<[f32; 3]>,
    ) -> Result<(), String> {
        for _ in 0..count {
            self.reader
                .read_exact(&mut self.row)
                .map_err(|e| format!("the PLY body ends early: {e}"))?;

            let position = self
                .channels
                .position
                .map(|index| self.channels.binary(index, order, &self.row) as f32);
            positions.push(position);
            if let Some([r, g, b]) = self.channels.colors {
                colors.push(
                    [r, g, b].map(|index| self.channels.binary_color(index, order, &self.row)),
                );
            }
            self.vertices_read += 1;
        }
        Ok(())
    }

    /// Bounds of the points this stream has produced so far.
    pub fn loaded_bounds(&self) -> Bounds {
        self.bounds
    }

    /// Current load progress. The byte count is derived from the records
    /// actually consumed — a stride for binary, the line lengths for ASCII —
    /// so it needs no seek and is right for both.
    pub fn progress(&self) -> LoadProgress {
        let body = match self.format {
            PlyFormat::Ascii => self.ascii_bytes,
            _ => (self.vertices_read as u64).saturating_mul(self.stride as u64),
        };
        LoadProgress {
            bytes_read: (self.body_start + body).min(self.total_bytes()),
            total_bytes: self.total_bytes(),
            points_loaded: self.vertices_read,
            complete: self.vertices_read >= self.vertex_count,
        }
    }
}

/// Read the header lines up to and including `end_header`, reporting how many
/// bytes they took so progress can be measured from the start of the body.
fn read_header(reader: &mut BufReader<File>) -> Result<(String, u64), String> {
    let mut header = String::new();
    let mut consumed = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the PLY header ends before end_header".into());
        }
        consumed += read as u64;
        header.push_str(&String::from_utf8_lossy(&line));
        if header.len() > MAX_HEADER_BYTES {
            return Err("the PLY header is implausibly large".into());
        }
        if String::from_utf8_lossy(&line)
            .trim()
            .eq_ignore_ascii_case("end_header")
        {
            return Ok((header, consumed));
        }
    }
}

/// The next non-blank body line, without its terminator.
fn next_record_line(reader: &mut BufReader<File>) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok(None);
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        if !text.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(Some(text.to_vec()));
        }
    }
}

/// Every number on one ASCII record, in property order.
fn tokenise(line: &[u8]) -> Vec<f64> {
    let mut values = Vec::new();
    let mut tokens = ply::Tokens::new(line);
    while let Some(token) = tokens.next_token() {
        match ply::token_f64(token) {
            Some(value) => values.push(value),
            // A non-numeric token cannot sit in the middle of a record; the
            // columns after it are missing rather than shifted.
            None => break,
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a binary little-endian PLY with `uchar` colours and two extra
    /// properties before the colours, the layout real scans use.
    fn write_binary_scan(path: &Path, points: &[[f32; 3]], colors: &[[u8; 3]]) {
        let header = format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex {}\n\
             property float x\nproperty float y\nproperty float z\n\
             property float nx\nproperty float ny\nproperty float nz\n\
             property uchar red\nproperty uchar green\nproperty uchar blue\n\
             property uchar alpha\nend_header\n",
            points.len()
        );
        let mut file = Vec::new();
        file.extend_from_slice(header.as_bytes());
        for (point, color) in points.iter().zip(colors) {
            for value in point {
                file.extend_from_slice(&value.to_le_bytes());
            }
            for value in [0.0f32, 0.0, 1.0] {
                file.extend_from_slice(&value.to_le_bytes());
            }
            file.extend_from_slice(&[color[0], color[1], color[2], 255]);
        }
        std::fs::write(path, &file).unwrap();
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("trove-stream-{}-{name}", std::process::id()))
    }

    /// The heart of it: a record with extra properties and `uchar` colours
    /// must still be strided correctly. Reading colours as `f32` — the old
    /// assumption — desynchronises the stream and every point after the
    /// first comes out as garbage.
    #[test]
    fn binary_records_with_extra_properties_and_uchar_colors_stay_aligned() {
        let path = temp("aligned.ply");
        let points = [[0.0, 1.0, 2.0], [3.0, 4.0, 5.0], [6.0, 7.0, 8.0]];
        let colors = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]];
        write_binary_scan(&path, &points, &colors);

        let (mut streamer, count) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(count, 3);
        let chunk = streamer.next_chunk(8).unwrap().expect("a chunk");
        assert_eq!(chunk.positions, points);
        // `uchar` colours are 0..=255 in the file and 0..=1 in the mesh.
        assert_eq!(
            chunk.colors,
            vec![[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
        assert!(chunk.is_last);
    }

    /// A partially-consumed file resumes where it stopped.
    #[test]
    fn chunks_are_incremental() {
        let path = temp("chunks.ply");
        let points: Vec<[f32; 3]> = (0..5).map(|i| [i as f32, 0.0, 0.0]).collect();
        let colors = vec![[255u8, 0, 0]; 5];
        write_binary_scan(&path, &points, &colors);

        let (mut streamer, count) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(count, 5);
        let first = streamer.next_chunk(2).unwrap().expect("first");
        assert_eq!(first.positions, points[0..2].to_vec());
        assert!(!first.is_last);
        assert_eq!(streamer.vertices_read(), 2);
        let rest = streamer.next_chunk(8).unwrap().expect("rest");
        assert_eq!(rest.positions, points[2..5].to_vec());
        assert!(rest.is_last);
        assert!(streamer.next_chunk(8).unwrap().is_none());
    }

    /// ASCII colours declared `uchar` are 0..=255 and have to be scaled; the
    /// old reader passed them through raw, so a red scan rendered white.
    #[test]
    fn ascii_colors_are_normalised_by_their_declared_type() {
        let path = temp("ascii.ply");
        let body = "ply\nformat ascii 1.0\nelement vertex 2\n\
                    property float x\nproperty float y\nproperty float z\n\
                    property uchar red\nproperty uchar green\nproperty uchar blue\n\
                    end_header\n1 2 3 255 0 0\n4 5 6 0 128 255\n";
        std::fs::write(&path, body).unwrap();

        let (mut streamer, count) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(count, 2);
        let chunk = streamer.next_chunk(8).unwrap().expect("a chunk");
        assert_eq!(chunk.positions, vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        let colors = chunk.colors;
        assert_eq!(colors[0], [1.0, 0.0, 0.0]);
        assert!((colors[1][1] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(colors[1][2], 1.0);
    }

    /// The extent of the whole file has to come from the file, not from the
    /// points that happen to have been read: framing on those rescales the
    /// view as the cloud streams in.
    #[test]
    fn sampling_the_file_gives_the_whole_extent() {
        let path = temp("extent.ply");
        // Two far-apart clusters, with the spread-out points last so a
        // "first chunk" shortcut would get it wrong.
        let mut points: Vec<[f32; 3]> = (0..600).map(|i| [i as f32 * 0.01, 0.0, 0.0]).collect();
        points.push([-500.0, -400.0, -300.0]);
        points.push([500.0, 400.0, 300.0]);
        write_binary_scan(&path, &points, &vec![[0u8, 0, 0]; points.len()]);

        let (mut streamer, count) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(count, points.len());

        // Enough samples to land on every record: the extent is exact.
        let extent = streamer
            .sample_bounds(points.len())
            .expect("binary PLY can be sampled");
        assert_eq!(extent.min, [-500.0, -400.0, -300.0]);
        assert_eq!(extent.max, [500.0, 400.0, 300.0]);

        // A coarse sample is an approximation: it stays *inside* the real
        // extent — the caller grows the frame as the outlier chunks arrive,
        // never the other way round — and always includes the last record, so
        // a file whose extremes sit at its end is not missed wholesale.
        let coarse = streamer.sample_bounds(16).expect("sampled");
        for axis in 0..3 {
            assert!(coarse.min[axis] >= extent.min[axis], "{coarse:?}");
            assert!(coarse.max[axis] <= extent.max[axis], "{coarse:?}");
        }
        assert_eq!(coarse.max, [500.0, 400.0, 300.0], "the last record");

        // And sampling does not eat the body: the first chunk still reads the
        // first records.
        let chunk = streamer.next_chunk(3).unwrap().expect("a chunk");
        assert_eq!(chunk.positions, points[0..3].to_vec());
    }

    /// ASCII PLY records are not fixed-size, so there is nothing to seek to.
    #[test]
    fn sampling_an_ascii_file_is_declined() {
        let path = temp("extent-ascii.ply");
        let body = "ply\nformat ascii 1.0\nelement vertex 2\n\
                    property float x\nproperty float y\nproperty float z\n\
                    end_header\n0 0 0\n1 1 1\n";
        std::fs::write(&path, body).unwrap();
        let (mut streamer, _) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert!(streamer.sample_bounds(16).is_none());
    }

    /// A file with faces is still readable — the thumbnail path samples one —
    /// but it says so, so the viewer can hand it to the mesh loader.
    #[test]
    fn a_declared_face_element_is_reported() {
        let path = temp("faces.ply");
        let body = "ply\nformat ascii 1.0\nelement vertex 3\n\
                    property float x\nproperty float y\nproperty float z\n\
                    element face 1\nproperty list uchar int vertex_indices\n\
                    end_header\n0 0 0\n1 0 0\n0 1 0\n3 0 1 2\n";
        std::fs::write(&path, body).unwrap();
        let (streamer, count) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(count, 3);
        assert!(streamer.declares_faces());
    }

    /// `element face 0` declares no surface: the file is a point cloud an
    /// exporter tagged with an empty face element, and it must take the
    /// streaming path instead of being bounced to the mesh fallback.
    #[test]
    fn an_empty_face_element_is_not_a_surface() {
        let path = temp("facezero.ply");
        let body = "ply\nformat ascii 1.0\nelement vertex 3\n\
                    property float x\nproperty float y\nproperty float z\n\
                    element face 0\nproperty list uchar int vertex_indices\n\
                    end_header\n0 0 0\n1 0 0\n0 1 0\n";
        std::fs::write(&path, body).unwrap();
        let (streamer, count) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        assert_eq!(count, 3);
        assert!(!streamer.declares_faces());
    }

    /// Sampling a cloud is how a thumbnail of a twenty-gigabyte scan is drawn:
    /// a bounded number of points, spread over the file, colours included.
    #[test]
    fn sampling_points_returns_an_even_bounded_subset() {
        let path = temp("points.ply");
        let points: Vec<[f32; 3]> = (0..5_000).map(|i| [i as f32, 0.0, 0.0]).collect();
        let colors: Vec<[u8; 3]> = (0..5_000).map(|i| [(i % 251) as u8, 7, 9]).collect();
        write_binary_scan(&path, &points, &colors);

        let (mut streamer, _) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        let sampled = streamer.sample_points(200).expect("sampled");
        let (sampled_positions, sampled_colors) = (sampled.positions, sampled.colors);

        // One point per sample, plus the last record when the stride does not
        // land on it.
        assert!(
            (200..=201).contains(&sampled_positions.len()),
            "{}",
            sampled_positions.len()
        );
        // Spread across the file rather than clustered at the front.
        let last = sampled_positions.last().unwrap()[0];
        assert_eq!(last, 4_999.0, "the walk must reach the end of the file");
        // Evenly spread rather than clustered: consecutive samples are a
        // stride apart, so no region of the file is skipped for long.
        let biggest_gap = sampled_positions
            .windows(2)
            .map(|pair| pair[1][0] - pair[0][0])
            .fold(0.0f32, f32::max);
        assert!(biggest_gap <= (5_000.0 / 200.0) + 1.0, "gap {biggest_gap}");
        assert_eq!(sampled_colors.len(), sampled_positions.len());
        assert_eq!(sampled_colors[0], [0.0, 7.0 / 255.0, 9.0 / 255.0]);
        // The body is where it was: sampling is a side look.
        let chunk = streamer.next_chunk(1).unwrap().expect("a chunk");
        assert_eq!(chunk.positions, vec![[0.0, 0.0, 0.0]]);
    }

    /// Colour columns are resolved by name, so a file that orders its
    /// properties unusually still reads correctly.
    #[test]
    fn columns_are_resolved_by_name_not_position() {
        let path = temp("order.ply");
        let body = "ply\nformat ascii 1.0\nelement vertex 1\n\
                    property uchar blue\nproperty float z\nproperty uchar red\n\
                    property float x\nproperty uchar green\nproperty float y\n\
                    end_header\n64 3.0 255 1.0 128 2.0\n";
        std::fs::write(&path, body).unwrap();

        let (mut streamer, _) = PointStreamer::open(&path).expect("opens");
        std::fs::remove_file(&path).ok();
        let chunk = streamer.next_chunk(1).unwrap().expect("a chunk");
        assert_eq!(chunk.positions, vec![[1.0, 2.0, 3.0]]);
        let color = chunk.colors[0];
        assert_eq!(color[0], 1.0);
        assert!((color[1] - 128.0 / 255.0).abs() < 1e-6);
        assert!((color[2] - 64.0 / 255.0).abs() < 1e-6);
    }

    /// Progress is measured from the reader's position, which is past the
    /// header and the records that have actually been read.
    #[test]
    fn progress_tracks_the_reader_position() {
        let path = temp("progress.ply");
        let points: Vec<[f32; 3]> = (0..4).map(|i| [i as f32, 0.0, 0.0]).collect();
        write_binary_scan(&path, &points, &[[0u8, 0, 0]; 4]);

        let (mut streamer, _) = PointStreamer::open(&path).expect("opens");
        let total = streamer.total_bytes();
        std::fs::remove_file(&path).ok();
        let before = streamer.progress();
        assert!(before.bytes_read > 0 && before.bytes_read < total);
        assert!(!before.complete);
        streamer.next_chunk(4).unwrap().expect("a chunk");
        let after = streamer.progress();
        assert!(after.bytes_read > before.bytes_read);
        assert_eq!(after.bytes_read, total);
        assert!(after.complete);
        assert_eq!(after.ratio(), 1.0);
    }
}
