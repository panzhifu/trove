//! Background removal as a job: run the saliency graph over a set of images
//! and write each transparent PNG where the import will pick it up.
//!
//! The graph loads once per run and is reused for every asset — parsing its
//! 168 MB of protobuf costs more than one inference, so a batch of fifty pays
//! for the read once. Output goes to the caller's directory (the incoming
//! drop, in the app) rather than into the library directly: the cutout is a
//! derived file the user has not seen yet, and the import that follows is what
//! decides whether it stays.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use uuid::Uuid;

use super::JobContext;
use crate::error::{Error, Result};
use crate::media::matting::SaliencyModel;
use crate::media::thumb::blob_path;
use crate::model::{Asset, AssetKind, AssetQuery, AssetSort, TrashPool};
use crate::store::{Store, assets};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Page size when walking the library; the store caps a query at 1000.
const PAGE: u32 = 1_000;

/// Which images to cut out, and where the results go.
#[derive(Clone)]
pub struct CutoutOptions {
    pub db_path: PathBuf,
    pub data_root: PathBuf,
    /// Where the transparent PNGs land.
    pub out_dir: PathBuf,
    /// The U²-Net checkpoint, already resolved by the model directory scan.
    pub model_path: PathBuf,
    /// The assets to process. Empty = every live image.
    pub only: Vec<Uuid>,
}

/// What a run did.
#[derive(Debug, Default, Clone)]
pub struct CutoutOutcome {
    /// Assets whose cutout was written.
    pub cut: u64,
    /// The files behind [`Self::cut`], in the order they landed — the caller
    /// imports exactly these, so a count alone would not do.
    pub written: Vec<PathBuf>,
    /// Assets with no readable file, an undecodable image, or a failed run.
    pub failed: u64,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// Cut every candidate out, one inference each.
pub fn run(options: &CutoutOptions, ctx: &JobContext) -> crate::Result<CutoutOutcome> {
    let started = Instant::now();
    // Open through the store once so pending migrations apply, then take a
    // connection of our own — the same arrangement every job uses.
    Store::open(&options.db_path)?;
    let conn = Connection::open(&options.db_path)
        .map_err(|e| Error::Db(format!("open library database: {e}")))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| Error::Db(format!("set busy timeout: {e}")))?;

    let model = SaliencyModel::load(&options.model_path)?;
    tracing::info!(
        path = %options.model_path.display(),
        repaired = model.repaired_resizes(),
        "matting: graph loaded"
    );

    let mut outcome = CutoutOutcome::default();
    let candidates = match candidates(&conn, options) {
        Ok(assets) => assets,
        Err(error) => {
            outcome.error = Some(format!("list assets: {error}"));
            return Ok(outcome);
        }
    };

    let total = candidates.len() as u64;
    ctx.set_total(total);
    let mut done: u64 = 0;
    if let Err(error) = std::fs::create_dir_all(&options.out_dir) {
        outcome.error = Some(format!("create {}: {error}", options.out_dir.display()));
        return Ok(outcome);
    }

    for asset in candidates {
        ctx.park_if_paused();
        if ctx.cancelled() {
            outcome.cancelled = true;
            break;
        }
        ctx.set_summary(format!("{}/{}", done + 1, total));
        match cut_one(options, &model, &asset) {
            Ok(path) => {
                outcome.cut += 1;
                outcome.written.push(path);
            }
            Err(error) => {
                // One unreadable file is that asset's problem; the run keeps
                // going and the count says how many did not make it.
                tracing::warn!(asset = %asset.id, %error, "matting: cutout failed");
                outcome.failed += 1;
            }
        }
        done += 1;
        ctx.progress(done, total);
    }

    tracing::info!(
        cut = outcome.cut,
        failed = outcome.failed,
        seconds = started.elapsed().as_secs_f64(),
        "matting: run finished"
    );
    Ok(outcome)
}

/// One asset: read its file, cut it out, write the PNG.
fn cut_one(options: &CutoutOptions, model: &SaliencyModel, asset: &Asset) -> Result<PathBuf> {
    let source = blob_path(&options.data_root, asset)
        .ok_or_else(|| Error::Validation("asset has no file to read".into()))?;
    let dest = cutout_path(&options.out_dir, &source);
    model.write_cutout(&source, &dest)?;
    Ok(dest)
}

/// Where a cutout of `source` goes: in `out_dir`, named after the source,
/// never on top of a file that already exists — two cutouts of the same photo
/// are two real results.
pub fn cutout_path(out_dir: &Path, source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty());
    let name = |taken: u32| match &stem {
        Some(stem) if taken == 0 => format!("{stem} cutout.png"),
        Some(stem) => format!("{stem} cutout ({taken}).png"),
        None if taken == 0 => "cutout.png".into(),
        None => format!("cutout ({taken}).png"),
    };
    let mut dest = out_dir.join(name(0));
    // Numbering starts at 2: the plain name is number one, and that is the
    // convention every other self-produced file in the app follows.
    let mut taken = 2;
    while dest.is_file() {
        dest = out_dir.join(name(taken));
        taken += 1;
    }
    dest
}

/// The images this run should process.
fn candidates(conn: &Connection, options: &CutoutOptions) -> crate::Result<Vec<Asset>> {
    let is_image = |asset: &Asset| asset.kind == AssetKind::Image;
    if !options.only.is_empty() {
        let mut assets = assets::by_ids(conn, &options.only)?;
        assets.retain(is_image);
        return Ok(assets);
    }

    let mut out: Vec<Asset> = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = assets::query(
            conn,
            &AssetQuery {
                limit: Some(PAGE),
                offset,
                sort: AssetSort::CreatedAt,
                sort_desc: true,
                pool: TrashPool::Live,
                ..AssetQuery::live()
            },
        )?;
        let short = page.items.len() < PAGE as usize;
        out.extend(page.items.into_iter().filter(is_image));
        if short {
            break;
        }
        offset += PAGE as u64;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The naming rule the batch depends on: a cutout never overwrites, and
    /// the second one of the same source is the one that says so.
    #[test]
    fn a_cutout_never_lands_on_top_of_another() {
        let dir = std::env::temp_dir().join(format!("trove-cutout-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = Path::new("/nowhere/photo.jpg");

        let first = cutout_path(&dir, source);
        assert_eq!(first, dir.join("photo cutout.png"));
        std::fs::write(&first, b"x").unwrap();

        assert_eq!(
            cutout_path(&dir, source),
            dir.join("photo cutout (2).png"),
            "the taken name has to be stepped past"
        );
        std::fs::write(dir.join("photo cutout (2).png"), b"x").unwrap();
        assert_eq!(cutout_path(&dir, source), dir.join("photo cutout (3).png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path with no file name to build on still gets a destination — the
    /// fallback has to be a name the import will accept, not an empty one.
    #[test]
    fn a_source_without_a_stem_still_gets_a_name() {
        assert_eq!(
            cutout_path(Path::new("/tmp"), Path::new("/")),
            Path::new("/tmp/cutout.png")
        );
    }
}
