//! Content hashing: BLAKE3, with the three properties an importer wants.
//!
//! Every content hash in the pipeline is BLAKE3 (`blake3::Hasher`, 32-byte
//! digest, 64 hex characters). Two things made the switch worth its
//! migration:
//!
//! 1. **Speed.** The hash is on the critical path of every import: with
//!    [`ImportStorage::Link`] the read-for-hashing is the *whole* I/O an
//!    import does.
//! 2. **It is a tree, so it parallelises.** [`Hasher::update_rayon`] splits
//!    the input into subtrees and hashes them on the rayon pool the caller is
//!    running on. That is what [`hash_file`] uses for large sources: one file,
//!    many cores, with the read still sequential — see
//!    [`PARALLEL_MIN_BYTES`].
//!
//! [`ImportStorage::Link`]: crate::media::import::ImportStorage::Link
//! [`Hasher::update_rayon`]: blake3::Hasher::update_rayon
//!
//! ## Entry points, one digest
//!
//! | function | reads | cost | used by |
//! |---|---|---|---|
//! | [`hash_bytes`] | nothing | O(len), one core | in-memory payloads (edit output) |
//! | [`fingerprint`] | 3 blocks, ~1/20 of the file | O(1) in the file's size | the hash stage's cheap tier |
//! | [`hash_file`] | every byte | O(len), all cores above the threshold | the hash stage's full read |
//! | [`hash_file_cached`] | every byte, once ever | as above, then a `stat` | callers outside the pipeline (relink) |
//!
//! [`fingerprint`] covers the file's exact length plus its head, middle and
//! tail — enough to tell two files apart in practice, and the reason a
//! re-import of a 4 GB video can be recognised without reading 4 GB. What it
//! is *not* is a hash of the whole content: two files that differ only in an
//! unsampled region share one. Every caller of it treats that as a
//! pre-filter, never as the recorded hash.
//!
//! It is also only offered for sources of [`SAMPLE_MIN_BYTES`] and up. Below
//! that the sample would cost as much as the read it is trying to avoid (and
//! for a really small file it *is* the read), so the honest answer is `None`
//! and the caller reads the file — which is what a caller would do anyway.
//!
//! "All cores above the threshold" is not a figure of speech about the
//! machine: [`Hasher::update_rayon`] fans out through the rayon pool the
//! *caller* is running on, so a wide staging pool hashing a batch of
//! photographs and one staging worker hashing a 2 GB video both stay inside
//! the pool they were given.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Hex characters in a content hash. 64 rather than 32: the blob layout, the
/// thumbnail file names and the storage column all read a hash as opaquely
/// 64-hex, and a shorter digest would mean revisiting every one of them for
/// nothing a longer digest does not already give.
pub const HEX_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Buffer sizing
// ---------------------------------------------------------------------------

/// Smallest read buffer. Below this the syscall count stops mattering and the
/// allocation does not: a 1x1 PNG is read in one call either way.
pub const BUFFER_MIN: usize = 64 * 1024;

/// Largest read buffer on the single-threaded path. Four staging threads at
/// this width is 16 MiB of scratch, which is still nothing next to the
/// decodes they are otherwise holding.
pub const BUFFER_MAX: usize = 4 * 1024 * 1024;

/// Smallest buffer handed to the parallel path. `blake3` documents
/// `update_rayon` as *slower* than a plain update below ~128 KiB (x86_64);
/// 1 MiB is comfortably past that on every architecture this runs on.
pub const PARALLEL_BUFFER_MIN: usize = 1024 * 1024;

/// Largest buffer handed to the parallel path.
pub const PARALLEL_BUFFER_MAX: usize = 16 * 1024 * 1024;

/// From what source size hashing a file in parallel pays.
///
/// 4 MiB is the point where one core's BLAKE3 (~2-3 GB/s) spends more than a
/// millisecond on a file, and where the read has already stopped dominating
/// the hash. Below it the join overhead is measurable and the win is not;
/// above it a single large file uses cores that a batch of photographs would
/// otherwise have left idle.
pub const PARALLEL_MIN_BYTES: u64 = 4 * 1024 * 1024;

/// The read buffer for `len` bytes on the single-threaded path.
///
/// Adaptive rather than fixed: the buffers this replaces were 256 KiB for
/// every file, which is four times what the read of a 40 KiB thumbnail needs
/// and sixteen times less than a 2 GB video would like — and the import pool
/// runs up to twelve of these at once, so the fixed one was both allocating
/// too much on the small end and copying too often on the large end.
///
/// The rule is "a sixteenth of the file, rounded to a power of two, clamped
/// to [`BUFFER_MIN`]..=[`BUFFER_MAX`]": a 1 MiB file reads in 64 KiB bites, a
/// 256 MiB file in 4 MiB ones. The power-of-two part is not cosmetic —
/// `blake3` computes a subtree only for whole power-of-two blocks, so a
/// buffer that is not one splits into a serial tail.
pub fn buffer_size(len: u64) -> usize {
    let ideal = (len / 16).clamp(BUFFER_MIN as u64, BUFFER_MAX as u64) as usize;
    round_down_pow2(ideal.clamp(BUFFER_MIN, BUFFER_MAX))
}

/// The read buffer for `len` bytes on the parallel path, and the granularity
/// the parallel path chunks at. Same rule as [`buffer_size`] on the wider
/// clamp, for the same reason: each buffer is handed to one `update_rayon`.
pub fn parallel_buffer_size(len: u64) -> usize {
    let ideal = (len / 16).clamp(PARALLEL_BUFFER_MIN as u64, PARALLEL_BUFFER_MAX as u64) as usize;
    round_down_pow2(ideal.clamp(PARALLEL_BUFFER_MIN, PARALLEL_BUFFER_MAX))
}

/// Whether a source of `len` bytes is hashed across cores.
pub fn parallelizable(len: u64) -> bool {
    len >= PARALLEL_MIN_BYTES
}

/// The largest power of two at or below `n` (so the value stays inside the
/// clamp it was handed).
fn round_down_pow2(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    1usize << (usize::BITS - 1 - n.leading_zeros())
}

// ---------------------------------------------------------------------------
// The digest
// ---------------------------------------------------------------------------

/// Hex of a digest. Lowercase, no separator, `HEX_LEN` characters for a
/// BLAKE3 digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("write to string cannot fail");
    }
    s
}

/// BLAKE3 of an in-memory payload. No file is touched; used where the bytes
/// are already in hand (the re-encoded output of an in-place edit).
pub fn hash_bytes(bytes: &[u8]) -> String {
    hex(blake3::hash(bytes).as_bytes())
}

/// Streaming BLAKE3 of a file, plus its length: the digest and the size come
/// from one read, so no caller has to stat the file separately and race a
/// writer to learn how big it was.
///
/// Files below [`PARALLEL_MIN_BYTES`] are hashed on the calling thread;
/// larger ones hand each buffer to the rayon pool the caller runs on, which
/// for the import pipeline is the staging pool itself (so a wide batch of
/// photographs and one huge video do not both try to use every core).
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    Ok((hash_reader(file, len)?, len))
}

/// [`hash_file`], answered from the hash cache when the file has not changed
/// since it was last read — the same shortcut the import pipeline takes, for
/// callers outside it (relinking a moved file, most of all: the file was
/// read once at import, and relinking should not be a second read).
///
/// Always reads when the cache cannot answer, and records what it read.
pub fn hash_file_cached(cache_root: &Path, path: &Path) -> std::io::Result<(String, u64)> {
    use super::hash_cache;
    let stamp = hash_cache::stamp(path);
    if let Some((size, mtime)) = stamp
        && let Some(hash) = hash_cache::lookup(cache_root, path, size, mtime)
    {
        return Ok((hash, size));
    }
    let (hash, size) = hash_file(path)?;
    if let Some((_, mtime)) = stamp {
        // No sample entry: this caller is not the pipeline, and the cache
        // exists here to answer "what did this file hash to", not to seed the
        // duplicate-detection index.
        hash_cache::record(cache_root, path, size, mtime, &hash, "");
    }
    Ok((hash, size))
}

/// Streaming BLAKE3 of an open reader of known length. Split out so the copy
/// path ([`copy_and_hash`]) and the plain hash share one loop.
pub fn hash_reader(mut reader: File, len: u64) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    if parallelizable(len) {
        let mut buffer = vec![0u8; parallel_buffer_size(len)];
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update_rayon(&buffer[..n]);
        }
    } else {
        let mut buffer = vec![0u8; buffer_size(len)];
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
    }
    Ok(hex(hasher.finalize().as_bytes()))
}

/// Copy `src` to `dst`, hashing while copying so the source is read exactly
/// once. Returns the digest and the byte count.
///
/// The write stays sequential (one writer, one stream) while the *hash* of
/// each buffer may fan out — for a large source that is the difference
/// between a copy that costs a read plus a hash and a copy that costs a read.
pub fn copy_and_hash(src: &Path, dst: &Path) -> std::io::Result<(String, u64)> {
    let input = File::open(src)?;
    let len = input.metadata()?.len();
    copy_stream(input, len, dst)
}

/// Copy a file whose content hash is already known: same loop, without the
/// hash. Used when the dedup pre-check or the hash cache told us what the
/// bytes are — the bytes still have to be *moved*, but they do not have to be
/// hashed a second time to learn what we already know.
pub fn copy(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let input = File::open(src)?;
    let len = input.metadata()?.len();
    copy_buffer(input, len, dst)?;
    Ok(len)
}

fn copy_stream(mut input: File, len: u64, dst: &Path) -> std::io::Result<(String, u64)> {
    let mut output = File::create(dst)?;
    let mut hasher = blake3::Hasher::new();
    let parallel = parallelizable(len);
    let mut buffer = vec![
        0u8;
        if parallel {
            parallel_buffer_size(len)
        } else {
            buffer_size(len)
        }
    ];
    let mut copied: u64 = 0;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        if parallel {
            hasher.update_rayon(&buffer[..n]);
        } else {
            hasher.update(&buffer[..n]);
        }
        copied += n as u64;
    }
    output.flush()?;
    drop(output);
    Ok((hex(hasher.finalize().as_bytes()), copied))
}

/// The copy loop without a hasher, for [`copy`].
fn copy_buffer(mut input: File, len: u64, dst: &Path) -> std::io::Result<u64> {
    let mut output = File::create(dst)?;
    let mut buffer = vec![
        0u8;
        if parallelizable(len) {
            parallel_buffer_size(len)
        } else {
            buffer_size(len)
        }
    ];
    let mut copied: u64 = 0;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        copied += n as u64;
    }
    output.flush()?;
    Ok(copied)
}

// ---------------------------------------------------------------------------
// The cheap pre-check
// ---------------------------------------------------------------------------

/// Smallest sampled block, and the size of the sample for a file of about
/// 1 MiB: 3 × 16 KiB = 48 KiB.
pub const SAMPLE_BLOCK_MIN: usize = 16 * 1024;

/// Largest sampled block: 3 × 64 KiB = 192 KiB for anything from 4 MiB up.
pub const SAMPLE_BLOCK_MAX: usize = 64 * 1024;

/// Below this a sample is not offered at all — see [`fingerprint`].
///
/// The sample costs three blocks out of about a twentieth of the file, so
/// taking one from a small source is close to reading it outright: for a
/// 40 KiB thumbnail it *is* reading it, only with a detour. A MiB is where
/// three 16 KiB blocks are worth roughly a twentieth of the source, which is
/// the most this check is ever allowed to cost.
pub const SAMPLE_MIN_BYTES: u64 = 1024 * 1024;

/// The size of one sampled block for a file of `len` bytes.
///
/// Proportional (a sixty-fourth of the file) rather than fixed, so the cost of
/// the check is bounded as a *fraction* of the source instead of growing with
/// it in absolute terms on the small end: three blocks are always about a
/// twentieth of the file, 48 KiB for a 1 MiB source and 192 KiB for a 4 MiB
/// one and up.
///
/// The power-of-two rounding is for the same reason the read buffers use it:
/// `blake3` only computes a subtree per whole power-of-two block, so a ragged
/// size leaves a serial tail.
pub fn sample_block(len: u64) -> usize {
    let ideal = (len / 64).clamp(SAMPLE_BLOCK_MIN as u64, SAMPLE_BLOCK_MAX as u64) as usize;
    round_down_pow2(ideal.clamp(SAMPLE_BLOCK_MIN, SAMPLE_BLOCK_MAX))
}

/// A cheap stand-in for a file's content hash.
///
/// See [`fingerprint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The BLAKE3 digest of the sampled blocks and the file's length.
    pub digest: String,
    /// The file's length, read from the same handle.
    pub size: u64,
}

/// A cheap stand-in for a file's content hash: the exact byte length plus the
/// head, middle and tail blocks, hashed with BLAKE3 — or `None` when the file
/// is below [`SAMPLE_MIN_BYTES`] and a sample would cost about what reading
/// the file costs.
///
/// Cost is O(1) in the file's size and about a twentieth of it in bytes:
/// three reads and a seek, milliseconds for a 10 GB video. That is the whole
/// point — it is what the hash stage looks at *before* deciding that a source
/// deserves having every byte of it read, and a hit means the read does not
/// happen.
pub fn fingerprint(path: &Path) -> std::io::Result<Option<Sample>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < SAMPLE_MIN_BYTES {
        // Not worth sampling: below this the sample is a large fraction of
        // the file, and a caller that wants the content hash of a file this
        // small should simply hash it — which costs one sequential read.
        return Ok(None);
    }

    let block_size = sample_block(len);
    let mut hasher = blake3::Hasher::new();
    // The length goes in first so two files that happen to share a head can
    // never share a fingerprint because their middles were sampled at the
    // same offsets.
    hasher.update(&len.to_le_bytes());
    let mut block = vec![0u8; block_size];
    for offset in sample_offsets(len, block_size) {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut block)?;
        hasher.update(&block);
    }
    Ok(Some(Sample {
        digest: hex(hasher.finalize().as_bytes()),
        size: len,
    }))
}

/// Where the sample reads: head, middle and tail, none of them overlapping.
///
/// `len >= SAMPLE_MIN_BYTES` and `block <= SAMPLE_BLOCK_MAX <
/// SAMPLE_MIN_BYTES / 3`, so the three blocks always fit without touching.
fn sample_offsets(len: u64, block: usize) -> Vec<u64> {
    let block = block as u64;
    let mut offsets = vec![0, len / 2 - block / 2, len - block];
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-hash-{name}-{}-{}",
            std::process::id(),
            crate::model::new_id().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The digest is the documented shape: lowercase hex, 64 characters —
    /// which is what keeps the blob layout and the storage column unchanged.
    #[test]
    fn a_digest_is_64_lowercase_hex_characters() {
        let digest = hash_bytes(b"trove");
        assert_eq!(digest.len(), HEX_LEN);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, digest.to_lowercase());
        // A known vector, so a swap of algorithm cannot pass silently.
        assert_eq!(
            hex(blake3::hash(b"").as_bytes()),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn the_same_bytes_always_hash_the_same_way() {
        assert_eq!(hash_bytes(b"same"), hash_bytes(b"same"));
        assert_ne!(hash_bytes(b"same"), hash_bytes(b"other"));
    }

    /// Adaptive buffers: the clamps hold, the value is a power of two, and it
    /// grows with the source instead of being one fixed 256 KiB for all.
    #[test]
    fn buffer_sizes_are_adaptive_powers_of_two_within_the_clamps() {
        for len in [
            0u64,
            1,
            4096,
            40 * 1024,
            256 * 1024,
            1 << 20,
            1 << 24,
            1 << 34,
        ] {
            let buffer = buffer_size(len);
            assert!(
                (BUFFER_MIN..=BUFFER_MAX).contains(&buffer),
                "buffer {buffer} out of range for {len}"
            );
            assert!(buffer.is_power_of_two(), "{buffer} is not a power of two");
        }
        // Monotone: a bigger file never gets a smaller buffer.
        assert!(buffer_size(64 * 1024) <= buffer_size(1 << 20));
        assert!(buffer_size(1 << 20) <= buffer_size(1 << 30));
        // The point of the change: a thumbnail-sized file does not allocate
        // the photograph-sized buffer.
        assert_eq!(buffer_size(4096), BUFFER_MIN);
        assert_eq!(buffer_size(1 << 34), BUFFER_MAX);
    }

    #[test]
    fn only_large_sources_hash_in_parallel() {
        assert!(!parallelizable(0));
        assert!(!parallelizable(PARALLEL_MIN_BYTES - 1));
        assert!(parallelizable(PARALLEL_MIN_BYTES));
        let buffer = parallel_buffer_size(PARALLEL_MIN_BYTES);
        assert!((PARALLEL_BUFFER_MIN..=PARALLEL_BUFFER_MAX).contains(&buffer));
        assert!(buffer.is_power_of_two());
    }

    /// The parallel path and the serial one must produce the same digest: the
    /// threshold is a performance knob, not a semantic one.
    #[test]
    fn the_parallel_path_agrees_with_the_serial_one() {
        let dir = temp_dir("parallel");
        // Just over the threshold, and big enough that the parallel path
        // takes more than one buffer.
        let big = dir.join("big.bin");
        let payload: Vec<u8> = (0..(PARALLEL_MIN_BYTES as usize + 4096))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&big, &payload).unwrap();

        let (parallel, size) = hash_file(&big).unwrap();
        assert_eq!(size, payload.len() as u64);
        assert_eq!(parallel, hash_bytes(&payload));

        // A small file goes down the serial path and still agrees.
        let small = dir.join("small.bin");
        std::fs::write(&small, b"trove").unwrap();
        assert_eq!(hash_file(&small).unwrap().0, hash_bytes(b"trove"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Copying hashes what it wrote, and writes exactly what it read.
    #[test]
    fn copy_and_hash_moves_the_bytes_and_hashes_them() {
        let dir = temp_dir("copy");
        let src = dir.join("src.bin");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 97) as u8).collect();
        std::fs::write(&src, &payload).unwrap();

        let dst = dir.join("dst.bin");
        let (digest, size) = copy_and_hash(&src, &dst).unwrap();
        assert_eq!(size, payload.len() as u64);
        assert_eq!(digest, hash_bytes(&payload));
        assert_eq!(std::fs::read(&dst).unwrap(), payload);

        // The no-hash variant writes the same bytes.
        let plain = dir.join("plain.bin");
        assert_eq!(copy(&src, &plain).unwrap(), payload.len() as u64);
        assert_eq!(std::fs::read(&plain).unwrap(), payload);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Below the sampling threshold there is no sample to give: the caller is
    /// told so and reads the file, which costs about the same.
    #[test]
    fn a_small_file_offers_no_sample() {
        let dir = temp_dir("small-fp");
        let file = dir.join("small.bin");
        std::fs::write(&file, b"a small payload").unwrap();
        assert_eq!(fingerprint(&file).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The boundary, one byte apart: at the threshold there is a sample, one
    /// byte below it there is not. A caller that needs a content hash cannot
    /// mistake the two.
    #[test]
    fn the_sampling_threshold_is_where_it_says_it_is() {
        let dir = temp_dir("boundary");
        for (name, len) in [
            ("under", SAMPLE_MIN_BYTES as usize - 1),
            ("at", SAMPLE_MIN_BYTES as usize),
        ] {
            let file = dir.join(format!("{name}.bin"));
            std::fs::write(&file, vec![7u8; len]).unwrap();
            let sample = fingerprint(&file).unwrap();
            assert_eq!(sample.is_some(), len as u64 >= SAMPLE_MIN_BYTES, "{name}");
            if let Some(sample) = sample {
                assert_eq!(sample.size, len as u64);
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The blocks scale with the source, so the check stays a bounded fraction
    /// of it instead of growing on the small end or shrinking to nothing on
    /// the large one.
    #[test]
    fn the_sampled_blocks_scale_with_the_source() {
        assert_eq!(sample_block(SAMPLE_MIN_BYTES), SAMPLE_BLOCK_MIN);
        assert_eq!(sample_block(SAMPLE_BLOCK_MAX as u64 * 64), SAMPLE_BLOCK_MAX);
        assert_eq!(sample_block(u64::MAX / 2), SAMPLE_BLOCK_MAX);
        for len in [SAMPLE_MIN_BYTES, 4 << 20, 1 << 30, u64::MAX / 4] {
            let block = sample_block(len);
            assert!(
                (SAMPLE_BLOCK_MIN..=SAMPLE_BLOCK_MAX).contains(&block),
                "{block} out of range for {len}"
            );
            assert!(block.is_power_of_two(), "{block} is not a power of two");
            // The whole sample is a few percent of the source, never more.
            assert!(3 * block as u64 <= len / 4, "sample too big for {len}");
        }
        assert!(sample_block(SAMPLE_MIN_BYTES) < sample_block(1 << 30));
    }

    /// A large file's fingerprint is content-sensitive: same bytes at the same
    /// length always agree, a changed head, middle or tail does not.
    #[test]
    fn a_large_files_fingerprint_reads_a_sample_and_still_tells_content_apart() {
        let dir = temp_dir("large-fp");
        let file = dir.join("large.bin");
        let len = 8 << 20;
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &payload).unwrap();

        let sample = fingerprint(&file).unwrap().expect("large enough to sample");
        assert_eq!(sample.size, len as u64);
        assert_ne!(sample.digest, hash_bytes(&payload));
        // Stable across calls.
        assert_eq!(fingerprint(&file).unwrap().unwrap().digest, sample.digest);

        for site in ["head", "middle", "tail"] {
            let mut edited = payload.clone();
            let at = match site {
                "head" => 16,
                "middle" => len / 2,
                _ => len - 16,
            };
            edited[at] ^= 0xFF;
            let other = dir.join(format!("{site}.bin"));
            std::fs::write(&other, &edited).unwrap();
            assert_ne!(
                fingerprint(&other).unwrap().unwrap().digest,
                sample.digest,
                "a change in the {site} must show"
            );
        }

        // Same content in a different file → the same fingerprint (this is
        // what the dedup cache keys on).
        let twin = dir.join("twin.bin");
        std::fs::write(&twin, &payload).unwrap();
        assert_eq!(fingerprint(&twin).unwrap().unwrap().digest, sample.digest);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sample_offsets_stay_inside_the_file() {
        for len in [SAMPLE_MIN_BYTES, 5 << 20, 1 << 33] {
            let block = sample_block(len) as u64;
            let offsets = sample_offsets(len, sample_block(len));
            assert_eq!(offsets.len(), 3, "{len}");
            assert_eq!(offsets[0], 0);
            assert!(offsets.iter().all(|o| o + block <= len), "{len}");
            // No overlap between consecutive blocks.
            for pair in offsets.windows(2) {
                assert!(pair[0] + block <= pair[1], "{len}");
            }
        }
    }

    /// A missing file is an error rather than an empty-content hash, so a
    /// racing delete can never be imported as a file of zero bytes.
    #[test]
    fn hashing_a_missing_file_fails() {
        let dir = temp_dir("missing");
        let gone = dir.join("gone.bin");
        assert!(hash_file(&gone).is_err());
        assert!(fingerprint(&gone).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
