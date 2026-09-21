//! The composable import pipeline: one file flows through an ordered list of
//! [`Stage`]s, each declaring what it needs and what it produces.
//!
//! ## Why this exists
//!
//! Staging used to be one function that did six things in a fixed order, and
//! every consumer of the image it decoded did its own decode: the thumbnail
//! writer decoded the original, the palette went looking for the written
//! thumbnail, the visual signature opened that thumbnail again. Three decodes
//! of the same picture per file — and on a 6000x4000 JPEG that is the
//! dominant cost of an import.
//!
//! Here a stage declares what it reads ([`Stage::needs`], hard; [`Stage::uses`],
//! optional) and what it leaves behind ([`Stage::produces`]). The builder sorts
//! the graph, inserts a default producer for any needed slot nobody fills, and
//! the artifacts a stage produces land in [`StageIo`] where every later stage
//! can pick them up. [`Decoded`] is the artifact that matters today: the one
//! decode, read by the thumbnail writer, the palette miner and the visual
//! signature alike.
//!
//! ## Adding a consumer
//!
//! A new consumer of image pixels is one struct: declare `uses(&[Need::Decode])`
//! (or `needs` it, to have the decode inserted for you if a caller left it out),
//! read `io.artifacts.get::<Decoded>()`, and put it in the pipeline list. No
//! existing stage changes. A stage whose input is not the decode — video
//! posters, for instance — declares nothing and reads `io`'s plain fields.
//!
//! ## What a stage may fail on
//!
//! `Err` skips the *file* (the job reports it like any other failure), so only
//! genuine per-file problems return `Err`: an unreadable source to hash, a
//! failed blob copy. Everything derived — decode, thumbnail, metadata, palette,
//! signature — is best-effort and stays `Ok`: a file that cannot be decoded
//! still becomes an asset with a name, a size and a hash.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::error::{Error, Result};
use crate::model::AssetKind;

use super::import::ImportStorage;
use super::{blob, color, hash, hash_cache, metadata, probe, search, thumb, video};

/// What running a stage costs, so a scheduler can tell a metadata read from an
/// ffmpeg spawn. Recorded on every stage; the import pool reads it (a `Proc`
/// stage such as video poster extraction must not be allowed to fan out one
/// subprocess per worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    /// Pure computation on what is already in memory.
    Cheap,
    /// File reads/writes.
    Io,
    /// Pixel work (decode, resize, hashing an image).
    Cpu,
    /// A subprocess (ffmpeg / ffprobe / heif-dec).
    Proc,
}

/// A slot a stage can need or produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Need {
    /// The content hash (and, for a copying import, the blob) exists — see
    /// [`HashStage`] for the three ways it can be answered.
    Hash,
    /// Kind, mime and container facts are known.
    Probe,
    /// The image has been decoded once — see [`Decoded`].
    Decode,
    /// A thumbnail exists in the cache root.
    Thumb,
}

impl Need {
    /// Every slot, in dependency order (each one only needs slots before it).
    pub const ALL: [Need; 4] = [Need::Hash, Need::Probe, Need::Decode, Need::Thumb];

    /// Stable name for logs and tests.
    pub fn name(self) -> &'static str {
        match self {
            Need::Hash => "hash",
            Need::Probe => "probe",
            Need::Decode => "decode",
            Need::Thumb => "thumb",
        }
    }
}

/// A pipeline could not be assembled from the stages it was handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// Two stages claimed the same slot; the later reader would be reading
    /// whichever of them ran last.
    DuplicateProducer(Need),
    /// A stage needs a slot that has no default producer to insert.
    NoProducer(Need),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateProducer(need) => {
                write!(f, "two stages produce the same slot: {}", need.name())
            }
            Self::NoProducer(need) => write!(f, "no producer for slot: {}", need.name()),
        }
    }
}

impl std::error::Error for PipelineError {}

/// One step of the pipeline.
pub trait Stage: Send + Sync {
    /// Stable name (logs, tests, progress labels).
    fn name(&self) -> &'static str;

    /// Slots that must have a producer in the pipeline. A missing one is filled
    /// with the default stage for that slot; ordering is derived from it.
    fn needs(&self) -> &'static [Need] {
        &[]
    }

    /// Slots this stage reads *when an earlier producer left them behind*.
    /// Optional: without the producer the stage still runs, just less cheaply.
    /// Like [`Stage::needs`], this pulls the stage after its producer.
    fn uses(&self) -> &'static [Need] {
        &[]
    }

    /// Slots this stage fills for later stages.
    fn produces(&self) -> &'static [Need] {
        &[]
    }

    /// What this stage costs, for the scheduler.
    fn cost(&self) -> Cost {
        Cost::Cheap
    }

    fn run(&self, io: &mut StageIo) -> Result<()>;
}

/// Values a stage leaves behind for later stages, keyed by type.
///
/// A stage that wants something richer than the plain fields on [`StageIo`]
/// puts it here and a later stage picks it up by type — that is what makes one
/// decode shareable without giving every stage a field to pass around.
#[derive(Default)]
pub struct Artifacts {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Artifacts {
    pub fn put<T: Any + Send + Sync>(&mut self, value: T) {
        self.map.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>())?.downcast_ref::<T>()
    }

    pub fn has<T: Any + Send + Sync>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }
}

/// The one decode of an image, shared by every consumer that needs pixels.
///
/// The full-size pixels are *not* kept: the decode stage downscales and drops
/// them, so what stays alive through the rest of the file is a thumbnail-sized
/// buffer (a 6000x4000 source leaves ~1 MB here, not ~100 MB).
pub struct Decoded {
    /// Downscaled to [`thumb::THUMB_MAX`], original colour type — the
    /// thumbnail JPEG is written from this.
    pub small: image::DynamicImage,
    /// The same pixels as RGB — the palette and the visual signature read this.
    pub rgb: image::RgbImage,
    /// Intrinsic size of the source, when this decode knows it. `None` when the
    /// decode came from the thumbnail cache instead of the original (see
    /// [`DecodeStage`]), in which case the probe's header reading stands.
    pub dims: Option<(u32, u32)>,
}

/// The state of one file as it flows through the pipeline.
///
/// Inputs are set by the caller ([`StageIo::new`]); everything else is filled
/// in by stages. A stage must not assume a field is set unless a slot it
/// declared covers it.
pub struct StageIo {
    // -- inputs ---------------------------------------------------------------
    pub src: PathBuf,
    pub data_root: PathBuf,
    pub cache_root: PathBuf,
    pub storage: ImportStorage,

    // -- filled by stages -----------------------------------------------------
    pub file_name: String,
    pub ext: String,
    pub kind: AssetKind,
    pub mime: String,
    /// Hex BLAKE3 of the content. Also the thumbnail cache key, so a stage
    /// may read it only once [`Need::Hash`] has run.
    pub content_hash: String,
    pub size: u64,
    /// Library-relative blob path; empty for a linked import.
    pub rel_path: String,
    /// Linked import: nothing was copied, the record points at `src`.
    pub linked: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    /// Absolute thumbnail path, when one was written or already cached.
    pub thumb: Option<PathBuf>,
    pub mined: metadata::MinedMetadata,

    pub artifacts: Artifacts,
}

impl StageIo {
    /// Start one file. Only the file name and extension are derived here — both
    /// are pure path work, and the hash stage needs the extension to name a
    /// blob.
    pub fn new(
        src: &Path,
        data_root: &Path,
        cache_root: &Path,
        storage: ImportStorage,
    ) -> Result<Self> {
        Ok(Self {
            file_name: file_name_of(src)?,
            ext: probe::normalize_ext(
                &src.extension()
                    .map(|e| e.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            src: src.to_path_buf(),
            data_root: data_root.to_path_buf(),
            cache_root: cache_root.to_path_buf(),
            storage,
            kind: AssetKind::Other,
            mime: String::new(),
            content_hash: String::new(),
            size: 0,
            rel_path: String::new(),
            linked: !storage.copies(),
            width: None,
            height: None,
            duration_ms: None,
            thumb: None,
            mined: metadata::MinedMetadata::default(),
            artifacts: Artifacts::default(),
        })
    }

    /// The file the media stages read: the original for a linked import, the
    /// staged blob for a copying one. Only valid once `Need::Hash` ran.
    pub fn blob_path(&self) -> PathBuf {
        if self.linked {
            self.src.clone()
        } else {
            self.data_root.join(&self.rel_path)
        }
    }
}

/// An ordered list of stages, assembled once from what they declare.
pub struct Pipeline {
    stages: Vec<Arc<dyn Stage>>,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("stages", &self.stage_names())
            .finish()
    }
}

impl Pipeline {
    /// Assemble `stages`.
    ///
    /// Every slot a stage reads must have a producer: a missing one gets its
    /// default stage inserted ahead of its first reader, which is what lets a
    /// caller list only the stages it cares about. Reads declared through
    /// [`Stage::uses`] are filled the same way but are not required — if
    /// nothing can produce one, the stage degrades instead of failing the
    /// build. A slot a hard `needs` cannot be filled from, and a slot produced
    /// twice, are both errors.
    pub fn build(mut stages: Vec<Arc<dyn Stage>>) -> std::result::Result<Self, PipelineError> {
        // Insert one producer per pass and re-read the list: an inserted
        // producer has needs of its own (the decode stage needs the probe), so
        // the pass has to run again. `Need::ALL` is in dependency order and the
        // chain is at most that deep, so this converges well inside the bound.
        for _ in 0..=Need::ALL.len() + 1 {
            let have = produced(&stages);
            let mut pending: Vec<(Need, bool)> = Vec::new();
            for stage in &stages {
                for need in stage.needs() {
                    if !have.contains(need) {
                        pending.push((*need, true));
                    }
                }
                for need in stage.uses() {
                    if !have.contains(need) {
                        pending.push((*need, false));
                    }
                }
            }
            if pending.is_empty() {
                break;
            }
            let Some(producer) = pending.iter().find_map(|(need, _)| default_producer(*need))
            else {
                // Nothing left to insert. A soft read that nobody can fill is
                // fine; a hard one is a broken pipeline.
                return match pending
                    .iter()
                    .find(|(_, hard)| *hard)
                    .map(|(need, _)| *need)
                {
                    Some(need) => Err(PipelineError::NoProducer(need)),
                    None => Ok(Self { stages }),
                };
            };
            let at = insert_at(&stages, &producer);
            stages.insert(at, producer);
        }

        let mut seen: Vec<Need> = Vec::new();
        for stage in &stages {
            for need in stage.produces() {
                if seen.contains(need) {
                    return Err(PipelineError::DuplicateProducer(*need));
                }
                seen.push(*need);
            }
        }
        Ok(Self { stages })
    }

    /// Run every stage over one file. The first `Err` stops this file; the job
    /// reports it as a skip and moves on.
    pub fn run(&self, io: &mut StageIo) -> Result<()> {
        for stage in &self.stages {
            stage.run(io)?;
        }
        Ok(())
    }

    /// Stage names in execution order (logs, benchmarks, tests).
    pub fn stage_names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Stage names and costs in execution order (the scheduler's view).
    pub fn costs(&self) -> Vec<(&'static str, Cost)> {
        self.stages.iter().map(|s| (s.name(), s.cost())).collect()
    }
}

/// Every slot the given stages produce.
fn produced(stages: &[Arc<dyn Stage>]) -> Vec<Need> {
    stages.iter().flat_map(|s| s.produces().to_vec()).collect()
}

/// Where an inserted producer goes: ahead of the first stage that needs or uses
/// its slot, but after whatever produces the slots *it* needs.
fn insert_at(stages: &[Arc<dyn Stage>], producer: &Arc<dyn Stage>) -> usize {
    let consumers = |s: &Arc<dyn Stage>| {
        let mut reads = s.needs().to_vec();
        reads.extend_from_slice(s.uses());
        reads
    };
    let Some(first) = stages.iter().position(|s| {
        let slot = producer.produces().first().copied();
        slot.is_some_and(|slot| consumers(s).contains(&slot))
    }) else {
        return stages.len();
    };
    let mut at = first;
    while at > 0
        && producer
            .needs()
            .iter()
            .any(|n| stages[at - 1].produces().contains(n))
    {
        at -= 1;
    }
    at
}

/// The default stage that fills `need`.
fn default_producer(need: Need) -> Option<Arc<dyn Stage>> {
    match need {
        Need::Hash => Some(Arc::new(HashStage)),
        Need::Probe => Some(Arc::new(ProbeStage)),
        Need::Decode => Some(Arc::new(DecodeStage)),
        Need::Thumb => Some(Arc::new(ThumbStage)),
    }
}

/// The pipeline every import runs, built once.
///
/// The built-in six run first; stages contributed by registered plugins
/// ([`crate::plugins`]) are appended after them — so a plugin that enriches
/// what the miner found reads finished metadata — and the enabled set is
/// snapshotted from the configuration at this first build. A plugin whose
/// stages the pipeline rejects (a duplicated slot, say) is logged and left
/// out rather than taking the built-in pipeline down with it.
pub fn default_pipeline() -> &'static Pipeline {
    static PIPELINE: OnceLock<Pipeline> = OnceLock::new();
    PIPELINE.get_or_init(|| {
        let builtin = || -> Vec<Arc<dyn Stage>> {
            vec![
                Arc::new(HashStage),
                Arc::new(ProbeStage),
                Arc::new(DecodeStage),
                Arc::new(ThumbStage),
                Arc::new(MineStage),
                Arc::new(VisualSigStage),
            ]
        };
        let mut stages = builtin();
        stages.extend(crate::plugins::pipeline_stages(
            &crate::config::AppConfig::load().disabled_plugins,
        ));
        match Pipeline::build(stages) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                tracing::error!(
                    %error,
                    "plugin stages rejected by the pipeline; the built-in stages run alone"
                );
                Pipeline::build(builtin()).expect("the built-in pipeline is well-formed")
            }
        }
    })
}

// ---------------------------------------------------------------------------
// The stages
// ---------------------------------------------------------------------------

/// The content hash (and, for a copying import, the blob). Reads the source
/// once — or not at all, when something cheaper already knows the answer.
///
/// Three tiers, cheapest first, and each one is skipped only when the one
/// above it can *prove* what the content is:
///
/// 1. **The hash cache remembers this file** — `(path, size, mtime)` matched
///    an entry, so its digest is in hand without a single read. A copying
///    import still copies the bytes, with [`blob::stage_with`] told the hash
///    so it does not hash them again.
/// 2. **The cheap sample matches a fingerprint that was hashed before** — one
///    lookup against three blocks read out of the middle of the file, which
///    is how a re-import of a duplicate under another name (or a second copy
///    inside the same batch) is answered without reading the whole thing.
///    See [`hash::SAMPLE_MIN_BYTES`] for why small sources go straight to
///    tier 3 instead: sampling them would cost what reading them costs.
/// 3. **A full read** — BLAKE3, across cores for anything large
///    ([`hash::PARALLEL_MIN_BYTES`]).
///
/// What tiers 1 and 2 have in common is that they answer with a hash some
/// *earlier* read established; neither invents one. A tier that cannot answer
/// falls through rather than guessing, which is what keeps a cold or corrupt
/// cache a performance problem instead of a correctness one.
pub struct HashStage;

impl Stage for HashStage {
    fn name(&self) -> &'static str {
        "hash"
    }

    fn produces(&self) -> &'static [Need] {
        &[Need::Hash]
    }

    fn cost(&self) -> Cost {
        Cost::Io
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        let copies = io.storage.copies();
        let stamp = hash_cache::stamp(&io.src);

        // Tier 1: this exact file, already read once.
        if let Some((size, mtime)) = stamp
            && let Some(hash) = hash_cache::lookup(&io.cache_root, &io.src, size, mtime)
        {
            return self.use_known(io, copies, hash, size);
        }

        // Tier 2, for sources where a sample is worth taking. The sampler
        // answers `None` for anything small enough that the sample would cost
        // what the read costs; those fall straight through to tier 3.
        let sample = hash::fingerprint(&io.src)
            .map_err(|error| Error::Validation(format!("hash {}: {error}", io.src.display())))?;
        if let Some(sample) = &sample
            && let Some(hash) =
                hash_cache::hash_for_fingerprint(&io.cache_root, sample.size, &sample.digest)
        {
            self.remember(io, &hash, sample.size, stamp, Some(&sample.digest));
            return self.use_known(io, copies, hash, sample.size);
        }

        // Tier 3: the real read. A copying import stages in the same pass, so
        // the bytes are neither read nor moved twice.
        let (content_hash, size) = if copies {
            let staged = blob::stage(&io.src, &io.data_root, &io.ext)?;
            io.rel_path = staged.rel_path;
            (staged.content_hash, staged.size)
        } else {
            let (hash, size) = blob::hash_file(&io.src)?;
            io.rel_path = String::new();
            (hash, size)
        };
        self.remember(
            io,
            &content_hash,
            size,
            stamp,
            sample.as_ref().map(|sample| sample.digest.as_str()),
        );
        io.content_hash = content_hash;
        io.size = size;
        io.linked = !copies;
        Ok(())
    }
}

impl HashStage {
    /// Publish a hash that was already known. A copying import still has to
    /// move the bytes — it just does not have to hash them again to name
    /// where they go.
    fn use_known(&self, io: &mut StageIo, copies: bool, hash: String, size: u64) -> Result<()> {
        if copies {
            let staged = blob::stage_with(&io.src, &io.data_root, &io.ext, Some(&hash))?;
            io.rel_path = staged.rel_path;
        } else {
            io.rel_path = String::new();
        }
        io.content_hash = hash;
        io.size = size;
        io.linked = !copies;
        Ok(())
    }

    /// Remember what this file hashed to, so the next encounter is a `stat`.
    /// `fingerprint` is `None` when the sample is not worth indexing — the
    /// file was small enough that the sample *was* the digest, in which case
    /// the index entry would only duplicate the path entry.
    fn remember(
        &self,
        io: &StageIo,
        hash: &str,
        size: u64,
        stamp: Option<(u64, i64)>,
        fingerprint: Option<&str>,
    ) {
        if let Some((_, mtime)) = stamp {
            hash_cache::record(
                &io.cache_root,
                &io.src,
                size,
                mtime,
                hash,
                fingerprint.unwrap_or_default(),
            );
        }
    }
}

/// Extension to kind / mime, plus the size and duration a container can tell us
/// without decoding pixels.
pub struct ProbeStage;

impl Stage for ProbeStage {
    fn name(&self) -> &'static str {
        "probe"
    }

    fn needs(&self) -> &'static [Need] {
        &[Need::Hash]
    }

    fn produces(&self) -> &'static [Need] {
        &[Need::Probe]
    }

    fn cost(&self) -> Cost {
        Cost::Cheap
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        let p = probe::probe(&io.ext);
        io.kind = p.kind;
        io.mime = p.mime;
        match p.kind {
            // Dimensions come from the header, not from a decode, so a folder
            // can be probed without reading any pixels. The decode stage
            // overwrites them with the real thing when it can. Formats whose
            // "header" read is secretly a full decode are skipped here — the
            // decode stage is about to read exactly those pixels anyway.
            AssetKind::Image if !probe::dimensions_need_full_decode(&io.ext) => {
                if let Some(d) = probe::image_dimensions(&io.blob_path()) {
                    io.width = Some(d.width);
                    io.height = Some(d.height);
                }
            }
            // MP4-family containers carry track dimensions + duration in the
            // moov box: a pure-Rust read, no subprocess. Every other container
            // (mkv, webm, avi, flv, mpeg-ts) needs `ffprobe`, which is why that
            // path takes a process slot.
            AssetKind::Video => {
                if let Some(f) = probe::video_facts(&io.blob_path()) {
                    io.width = Some(f.width);
                    io.height = Some(f.height);
                    io.duration_ms = f.duration_ms;
                } else if let Some(f) = video::probe(&io.blob_path()) {
                    io.width = Some(f.width);
                    io.height = Some(f.height);
                    io.duration_ms = (f.duration_ms > 0).then_some(f.duration_ms);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// The one decode. Images only; everything else is a no-op here.
///
/// Which file it decodes depends on the cache: a fresh import decodes the
/// original and downscales it, while a *re-import* of content that already has
/// a thumbnail decodes that thumbnail instead — the palette and the signature
/// only ever read thumbnail-sized pixels, so paying for a 6000x4000 decode
/// again would buy nothing.
pub struct DecodeStage;

impl Stage for DecodeStage {
    fn name(&self) -> &'static str {
        "decode"
    }

    fn needs(&self) -> &'static [Need] {
        &[Need::Probe]
    }

    fn produces(&self) -> &'static [Need] {
        &[Need::Decode]
    }

    fn cost(&self) -> Cost {
        Cost::Cpu
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        if io.kind != AssetKind::Image {
            return Ok(());
        }
        let blob = io.blob_path();
        let cached = thumb::cached(&io.cache_root, &io.content_hash);
        let (pixels, from_cache) = match cached {
            Some(thumb_path) => match thumb::decode_image(&thumb_path) {
                Some(small) => (small, true),
                // A corrupt cache entry is not fatal: fall through to the
                // original, which the thumbnail stage will rewrite.
                None => match thumb::decode_image(&blob) {
                    Some(full) => (full, false),
                    None => return Ok(()),
                },
            },
            None => match thumb::decode_image(&blob) {
                Some(full) => (full, false),
                None => return Ok(()),
            },
        };

        // Dimensions before the downscale: the small buffer's size is the
        // thumbnail's, not the asset's.
        let full_dims = (pixels.width(), pixels.height());
        let small = if from_cache {
            pixels
        } else {
            thumb::downscale(&pixels)
        };
        let rgb = small.to_rgb8();
        let dims = if from_cache {
            None
        } else {
            Some(match io.ext.as_str() {
                // An SVG renders into a box capped at THUMB_MAX, so its pixels
                // are not its intrinsic size; the document header is.
                "svg" => probe::image_dimensions(&blob)
                    .map(|d| (d.width, d.height))
                    .unwrap_or(full_dims),
                _ => full_dims,
            })
        };
        if let Some((w, h)) = dims {
            io.width = Some(w);
            io.height = Some(h);
        }

        io.artifacts.put(Decoded { small, rgb, dims });
        Ok(())
    }
}

/// The thumbnail cache entry. Uses the shared decode when there is one, and
/// the per-kind writer (ffmpeg poster, font card, model card) otherwise.
pub struct ThumbStage;

impl Stage for ThumbStage {
    fn name(&self) -> &'static str {
        "thumb"
    }

    fn needs(&self) -> &'static [Need] {
        &[Need::Probe]
    }

    fn uses(&self) -> &'static [Need] {
        &[Need::Decode]
    }

    fn produces(&self) -> &'static [Need] {
        &[Need::Thumb]
    }

    fn cost(&self) -> Cost {
        Cost::Io
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        let out = thumb::abs_path(&io.cache_root, &io.content_hash);
        if out.is_file() {
            io.thumb = Some(out);
            return Ok(());
        }
        io.thumb = match io.artifacts.get::<Decoded>() {
            Some(decoded) => thumb::write_downscaled(&decoded.small, &out),
            None => thumb::ensure(&io.cache_root, &io.content_hash, io.kind, &io.blob_path()),
        };
        Ok(())
    }
}

/// Mined metadata: EXIF from the file, palette from the shared decode.
pub struct MineStage;

impl Stage for MineStage {
    fn name(&self) -> &'static str {
        "mine"
    }

    fn uses(&self) -> &'static [Need] {
        &[Need::Decode]
    }

    fn cost(&self) -> Cost {
        Cost::Io
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        let blob = io.blob_path();
        io.mined = match io.artifacts.get::<Decoded>() {
            Some(decoded) => {
                metadata::mine_from_palette(&blob, io.kind, color::dominant_from_rgb(&decoded.rgb))
            }
            // No decode (a video, a font, an undecodable image): the palette
            // comes off the thumbnail the previous stage just wrote, or off the
            // file itself when there is no thumbnail either.
            None => {
                let color_source = io.thumb.clone().unwrap_or_else(|| blob.clone());
                metadata::mine(&blob, io.kind, &color_source)
            }
        };
        if io.mined.duration_ms.is_none() {
            io.mined.duration_ms = io.duration_ms;
        }
        Ok(())
    }
}

/// pHash + colour histogram, from the shared decode.
pub struct VisualSigStage;

impl Stage for VisualSigStage {
    fn name(&self) -> &'static str {
        "visual-sig"
    }

    fn needs(&self) -> &'static [Need] {
        &[Need::Decode]
    }

    fn cost(&self) -> Cost {
        Cost::Cpu
    }

    fn run(&self, io: &mut StageIo) -> Result<()> {
        if io.kind != AssetKind::Image {
            return Ok(());
        }
        let Some(decoded) = io.artifacts.get::<Decoded>() else {
            return Ok(());
        };
        let sig = search::VisualSignature::from_rgb(&decoded.rgb);
        if sig.phash != search::PHash(0) {
            sig.apply_to_facts(&mut io.mined.facts);
        }
        Ok(())
    }
}

/// A source file's name, truncated to the model's limit.
fn file_name_of(src: &Path) -> Result<String> {
    let name = src
        .file_name()
        .ok_or_else(|| Error::Validation("path has no file name".into()))?
        .to_string_lossy()
        .to_string();
    if name.trim().is_empty() {
        return Err(Error::Validation("path has no file name".into()));
    }
    let mut name = name;
    name.truncate(crate::model::MAX_NAME_LEN);
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stage that reads the decode without producing anything.
    struct ReadsDecode;

    impl Stage for ReadsDecode {
        fn name(&self) -> &'static str {
            "reads-decode"
        }
        fn uses(&self) -> &'static [Need] {
            &[Need::Decode]
        }
        fn run(&self, io: &mut StageIo) -> Result<()> {
            assert!(io.artifacts.has::<Decoded>(), "decode must run first");
            Ok(())
        }
    }

    /// A stage that produces a slot someone else already produces.
    struct DuplicateThumb;

    impl Stage for DuplicateThumb {
        fn name(&self) -> &'static str {
            "duplicate-thumb"
        }
        fn produces(&self) -> &'static [Need] {
            &[Need::Thumb]
        }
        fn run(&self, _io: &mut StageIo) -> Result<()> {
            Ok(())
        }
    }

    /// Fill the slots the decode reads, without touching a file — so a test can
    /// aim the decode stage at whatever it likes.
    struct StubHash;

    impl Stage for StubHash {
        fn name(&self) -> &'static str {
            "stub-hash"
        }
        fn produces(&self) -> &'static [Need] {
            &[Need::Hash]
        }
        fn run(&self, _io: &mut StageIo) -> Result<()> {
            Ok(())
        }
    }

    struct StubProbe;

    impl Stage for StubProbe {
        fn name(&self) -> &'static str {
            "stub-probe"
        }
        fn needs(&self) -> &'static [Need] {
            &[Need::Hash]
        }
        fn produces(&self) -> &'static [Need] {
            &[Need::Probe]
        }
        fn run(&self, _io: &mut StageIo) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn missing_producers_are_inserted_ahead_of_their_readers() {
        // A caller that lists only the stages it cares about still gets a
        // working pipeline: the slots it reads are filled for it.
        let pipeline = Pipeline::build(vec![Arc::new(ReadsDecode), Arc::new(MineStage)]).unwrap();
        assert_eq!(
            pipeline.stage_names(),
            vec!["hash", "probe", "decode", "reads-decode", "mine"]
        );
    }

    #[test]
    fn two_producers_of_one_slot_are_rejected() {
        let err =
            Pipeline::build(vec![Arc::new(ThumbStage), Arc::new(DuplicateThumb)]).unwrap_err();
        assert_eq!(err, PipelineError::DuplicateProducer(Need::Thumb));
    }

    #[test]
    fn the_default_pipeline_is_ordered_by_dependencies() {
        assert_eq!(
            default_pipeline().stage_names(),
            vec!["hash", "probe", "decode", "thumb", "mine", "visual-sig"]
        );
    }

    #[test]
    fn a_non_image_skips_the_pixel_stages() {
        let root = std::env::temp_dir().join(format!("trove-pipe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("notes.txt");
        std::fs::write(&src, b"not an image").unwrap();

        let mut io = StageIo::new(&src, &root, &root.join("cache"), ImportStorage::Link).unwrap();
        default_pipeline().run(&mut io).unwrap();
        assert_eq!(io.kind, AssetKind::Document);
        assert!(!io.artifacts.has::<Decoded>());
        assert!(io.thumb.is_none());
        assert!(!io.content_hash.is_empty(), "the hash still ran");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Tier 1, and the whole reason the hash cache exists: a file the pipeline
    /// has already read is answered from its `stat`.
    ///
    /// Proving it needs the file to still exist (nothing else could produce a
    /// hash for it) while its *content* is no longer what was hashed — so the
    /// test rewrites the bytes and puts the modification time back, which is
    /// exactly the assumption the cache is built on, applied deliberately.
    #[test]
    fn a_second_run_answers_from_the_hash_cache_without_reading_the_file() {
        let root = std::env::temp_dir().join(format!("trove-pipe-{}", uuid::Uuid::new_v4()));
        let cache = root.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("pic.bin");
        std::fs::write(&src, b"first content").unwrap();
        let mtime = std::fs::metadata(&src).unwrap().modified().unwrap();

        let mut first = StageIo::new(&src, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut first).unwrap();
        assert_eq!(
            first.content_hash,
            crate::media::hash::hash_bytes(b"first content")
        );

        // Same length, different bytes, same (restored) modification time.
        std::fs::write(&src, b"other content").unwrap();
        let handle = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();

        let mut again = StageIo::new(&src, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut again).unwrap();
        assert_eq!(
            again.content_hash, first.content_hash,
            "the remembered hash answered"
        );
        assert_ne!(
            again.content_hash,
            crate::media::hash::hash_bytes(b"other content"),
            "the file was not read again"
        );

        // And a file that really changed (mtime moved) is read again.
        std::fs::write(&src, b"third content").unwrap();
        let mut third = StageIo::new(&src, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut third).unwrap();
        assert_eq!(
            third.content_hash,
            crate::media::hash::hash_bytes(b"third content")
        );

        crate::media::hash_cache::clear(&cache);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Tier 2: a file too large to sample in full is hashed once, and its
    /// sample then answers for the same content under another name.
    ///
    /// The test aims at the boundary on purpose — the duplicate differs in a
    /// region the sample does not cover — because that is the exact trade
    /// [`super::hash::fingerprint`] documents: the sample decides, and a full
    /// read is what would decide otherwise. Asserting the *documented*
    /// behaviour (rather than the true hash) is the point: if someone later
    /// "fixes" the cheap tier into a full read, this test says so.
    #[test]
    fn a_large_sources_sample_answers_for_a_duplicate_under_another_name() {
        use crate::media::hash;

        let root = std::env::temp_dir().join(format!("trove-pipe-{}", uuid::Uuid::new_v4()));
        let cache = root.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let len = 8 << 20;
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

        let original = root.join("original.bin");
        std::fs::write(&original, &payload).unwrap();
        let mut first = StageIo::new(&original, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut first).unwrap();
        let hash_a = first.content_hash.clone();
        assert_eq!(hash_a, hash::hash_bytes(&payload), "a full read, once");

        // The pipeline recorded this file's sample, which is what the next
        // tier reads.
        let sample = hash::fingerprint(&original)
            .unwrap()
            .expect("8 MiB is past the sampling threshold");
        assert_eq!(
            crate::media::hash_cache::hash_for_fingerprint(&cache, sample.size, &sample.digest)
                .as_deref(),
            Some(hash_a.as_str())
        );

        // A same-length copy with one byte changed *outside* the sampled
        // head/middle/tail: never seen by path, so only the sample can answer.
        let mut edited = payload.clone();
        edited[100 * 1024] ^= 0xFF;
        let duplicate = root.join("duplicate.bin");
        std::fs::write(&duplicate, &edited).unwrap();
        let mut second = StageIo::new(&duplicate, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut second).unwrap();
        assert_eq!(
            second.content_hash, hash_a,
            "the sample matched, so the remembered hash answered"
        );
        assert_ne!(
            second.content_hash,
            hash::hash_bytes(&edited),
            "a full read would have found the edited byte"
        );

        crate::media::hash_cache::clear(&cache);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A copying import that is answered from the cache still moves the
    /// bytes: the known hash names the destination, it does not replace the
    /// copy.
    #[test]
    fn a_remembered_hash_still_moves_the_bytes_of_a_copying_import() {
        let root = std::env::temp_dir().join(format!("trove-pipe-{}", uuid::Uuid::new_v4()));
        let cache = root.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("pic.png");
        std::fs::copy(gradient_png(&root), &src).unwrap();

        let mut first = StageIo::new(&src, &root, &cache, ImportStorage::Copy).unwrap();
        default_pipeline().run(&mut first).unwrap();
        assert!(!first.rel_path.is_empty(), "the blob was placed");
        let blob = root.join(&first.rel_path);
        assert!(blob.is_file());

        // Second time around the hash comes from the cache; the blob is still
        // placed, at the same path, with the same content.
        let mut again = StageIo::new(&src, &root, &cache, ImportStorage::Copy).unwrap();
        default_pipeline().run(&mut again).unwrap();
        assert_eq!(again.rel_path, first.rel_path);
        assert_eq!(again.content_hash, first.content_hash);
        assert_eq!(std::fs::read(&blob).unwrap(), std::fs::read(&src).unwrap());

        crate::media::hash_cache::clear(&cache);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A gradient PNG: wide enough for the 9x8 difference hash to be non-zero
    /// (a solid image hashes to 0, which the signature stage treats as "no
    /// signature").
    fn gradient_png(dir: &Path) -> PathBuf {
        let mut img = image::RgbImage::new(32, 32);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            let v = (x * 8) as u8;
            *px = image::Rgb([v, v / 2, 255 - v]);
        }
        let path = dir.join("gradient.png");
        img.save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        path
    }

    #[test]
    fn one_decode_feeds_the_thumbnail_the_palette_and_the_signature() {
        let root = std::env::temp_dir().join(format!("trove-pipe-{}", uuid::Uuid::new_v4()));
        let cache = root.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let src = gradient_png(&root);

        let mut io = StageIo::new(&src, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut io).unwrap();

        // The decode is on the state for later stages to read, not thrown away
        // and re-created from the written thumbnail.
        assert!(io.artifacts.has::<Decoded>());
        assert_eq!(io.width, Some(32), "dimensions come from the decode");
        assert_eq!(io.height, Some(32));

        let thumb = io.thumb.clone().expect("thumbnail written");
        assert!(thumb.is_file());
        assert_eq!(thumb, thumb::abs_path(&cache, &io.content_hash));

        // Palette and signature both landed, read from that one decode.
        let visual = &io.mined.facts.visual;
        assert!(visual.dominant_color.is_some(), "palette mined");
        assert!(
            visual
                .dominant_colors
                .as_ref()
                .is_some_and(|c| !c.is_empty())
        );
        assert!(visual.visual_phash.is_some(), "pHash stored");
        assert!(visual.visual_color_hist.is_some(), "histogram stored");

        std::fs::remove_dir_all(&root).ok();
    }

    /// A re-import must not decode a 6000x4000 original just to read a palette:
    /// with the thumbnail already cached, the decode stage takes the cheap
    /// route. Deleting the original afterwards proves which file it read —
    /// everything else on the state is stubbed out so only the decode is real.
    #[test]
    fn a_re_import_decodes_the_cached_thumbnail_not_the_original() {
        let root = std::env::temp_dir().join(format!("trove-pipe-{}", uuid::Uuid::new_v4()));
        let cache = root.join("cache");
        std::fs::create_dir_all(&root).unwrap();
        let src = gradient_png(&root);

        let mut first = StageIo::new(&src, &root, &cache, ImportStorage::Link).unwrap();
        default_pipeline().run(&mut first).unwrap();
        let decoded = first.artifacts.get::<Decoded>().expect("fresh decode");
        assert_eq!(decoded.dims, Some((32, 32)));
        std::fs::remove_file(&src).unwrap();

        let pipeline = Pipeline::build(vec![
            Arc::new(StubHash),
            Arc::new(StubProbe),
            Arc::new(DecodeStage),
        ])
        .unwrap();
        assert_eq!(
            pipeline.stage_names(),
            vec!["stub-hash", "stub-probe", "decode"]
        );

        // Same content hash (the cache key), but only the thumbnail exists.
        let mut again = StageIo::new(&src, &root, &cache, ImportStorage::Link).unwrap();
        again.content_hash = first.content_hash.clone();
        again.kind = AssetKind::Image;
        pipeline.run(&mut again).unwrap();

        let decoded = again
            .artifacts
            .get::<Decoded>()
            .expect("decoded from cache");
        assert!(
            decoded.dims.is_none(),
            "a cached decode does not claim the original's dimensions"
        );
        assert!(decoded.rgb.width() <= thumb::THUMB_MAX);

        std::fs::remove_dir_all(&root).ok();
    }
}
