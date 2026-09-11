//! Opening a library file in an external application.
//!
//! A stored asset lives at a content-addressed path inside the library, and
//! that path *is* its SHA-256: dedup hands the same blob to every record
//! that shares the content, and the integrity job re-hashes it. Letting an
//! editor write to it would therefore corrupt the library, so external
//! editing goes through a working copy under `<config>/edit/<asset>/` which
//! can be imported back as a new asset.
//!
//! Linked assets are the user's own files, kept where they are — those open
//! in place, no copy involved.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::library::LibraryController;
use trove_core::config::AppConfig;
use trove_core::model::{Asset, Origin};
use trove_core::services::open_with;
use trove_core::store::assets;

/// Files above this size are not re-hashed to detect edits: the check runs
/// while the context menu opens, and a hash of a large video would stall
/// the frame. A size mismatch is still caught for free.
const MAX_HASH_BYTES: u64 = 32 * 1024 * 1024;

/// The file an external application is asked to open.
#[derive(Debug, Clone)]
pub struct OpenTarget {
    /// Where the library keeps the asset (the blob, or the linked file).
    pub source: PathBuf,
    /// The path handed over: `source` itself, or the working copy.
    pub path: PathBuf,
    /// True when `path` is a working copy that must exist before launching.
    pub working_copy: bool,
}

impl OpenTarget {
    /// Display name for notifications.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// Resolve the file to open for `id`.
pub fn target(controller: &LibraryController, id: Uuid) -> Option<OpenTarget> {
    let asset = assets::get(controller.library.store().conn(), id)
        .ok()
        .flatten()?;
    target_for_asset(controller, &asset)
}

/// Resolve the file to open for an already-loaded asset.
pub fn target_for_asset(controller: &LibraryController, asset: &Asset) -> Option<OpenTarget> {
    let source = match asset.origin {
        Origin::Linked => PathBuf::from(asset.extra.get("source_path")?.as_str()?),
        Origin::Stored => controller.library.root().join(asset.rel_path.as_ref()?),
    };
    if !source.is_file() {
        return None;
    }
    if asset.origin == Origin::Linked {
        return Some(OpenTarget {
            path: source.clone(),
            source,
            working_copy: false,
        });
    }
    Some(OpenTarget {
        source,
        path: copy_path_for(asset.id, &asset.file_name, &asset.ext)?,
        working_copy: true,
    })
}

/// Where the working copy for `id` lives: `<config>/edit/<id>/<name>.<ext>`,
/// keeping the original name so editors title their window sensibly.
pub fn copy_path_for(id: Uuid, file_name: &str, ext: &str) -> Option<PathBuf> {
    let dir = AppConfig::config_dir()?.join("edit").join(id.to_string());
    // `file_name` comes from the database, but treat it as untrusted: a
    // path separator in it would escape the working directory.
    let name = Path::new(file_name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = if name.is_empty() {
        if ext.is_empty() {
            id.to_string()
        } else {
            format!("{id}.{ext}")
        }
    } else {
        name
    };
    Some(dir.join(name))
}

/// Make sure the target exists: a working copy is created on first use.
pub fn publish(target: &OpenTarget) -> std::io::Result<()> {
    if !target.working_copy || target.path.exists() {
        return Ok(());
    }
    if let Some(parent) = target.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&target.source, &target.path).map(|_| ())
}

/// Prepare the target and launch it. `app` of `None` means the desktop
/// default handler.
pub fn launch(target: &OpenTarget, app: Option<&open_with::Application>) -> std::io::Result<()> {
    publish(target)?;
    match app {
        Some(app) => open_with::launch(app, &target.path),
        None => open_with::launch_default(&target.path),
    }
}

/// The working copy of `id` when it exists and no longer matches what the
/// library holds — i.e. the user edited it. `None` for linked assets (their
/// original is edited in place) and for untouched copies.
pub fn edited_copy(controller: &LibraryController, id: Uuid) -> Option<PathBuf> {
    let asset = assets::get(controller.library.store().conn(), id)
        .ok()
        .flatten()?;
    if asset.origin != Origin::Stored {
        return None;
    }
    let copy = copy_path_for(id, &asset.file_name, &asset.ext)?;
    let size = std::fs::metadata(&copy).ok()?.len();
    if size != asset.size_bytes {
        return Some(copy);
    }
    if size > MAX_HASH_BYTES {
        return None;
    }
    let (sha, _) = trove_core::media::blob::hash_file(&copy).ok()?;
    (Some(sha.as_str()) != asset.sha256.as_deref()).then_some(copy)
}

#[cfg(test)]
mod tests {
    use super::{OpenTarget, copy_path_for, publish};
    use uuid::Uuid;

    /// The copy is made once: relaunching the editor after an edit must not
    /// restore the library's bytes over the user's work.
    #[test]
    fn publishing_creates_the_copy_once_and_never_overwrites_an_edit() {
        let dir = std::env::temp_dir().join(format!("trove-open-with-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let source = dir.join("blob.png");
        std::fs::write(&source, b"original bytes").expect("source");
        let target = OpenTarget {
            source,
            // A sub-directory that does not exist yet: `publish` creates it.
            path: dir.join("edit").join("holiday.png"),
            working_copy: true,
        };

        publish(&target).expect("first publish");
        assert_eq!(
            std::fs::read(&target.path).expect("copy"),
            b"original bytes"
        );

        std::fs::write(&target.path, b"edited bytes").expect("edit");
        publish(&target).expect("second publish");
        assert_eq!(std::fs::read(&target.path).expect("copy"), b"edited bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_working_copy_lives_under_edit_and_keeps_the_original_name() {
        let id = Uuid::nil();
        let path = copy_path_for(id, "holiday.png", "png").expect("path");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "holiday.png");
        // `<config>/edit/<asset id>/holiday.png`
        let tail: Vec<String> = path
            .iter()
            .rev()
            .take(3)
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            tail,
            vec![
                "holiday.png".to_string(),
                id.to_string(),
                "edit".to_string()
            ]
        );
    }

    /// A separator in the stored name must not escape the working directory.
    #[test]
    fn a_path_separator_in_the_name_cannot_escape_the_copy_directory() {
        let id = Uuid::nil();
        let path = copy_path_for(id, "../../escape.png", "png").expect("path");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "escape.png");
    }

    #[test]
    fn a_nameless_asset_falls_back_to_its_id_and_extension() {
        let id = Uuid::nil();
        let path = copy_path_for(id, "", "tiff").expect("path");
        assert!(path.ends_with(format!("{id}.tiff")));
    }
}
