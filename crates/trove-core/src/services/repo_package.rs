//! The repository package: one library — its records, its own media, and
//! copies of the files it merely links — in a single `.trove` file.
//!
//! A library's records are only half its data. Most assets are *linked*: the
//! files stay wherever the user keeps them and the database stores paths, so
//! copying `data/libraries/<slug>/` to another machine leaves every one of
//! those assets pointing at nothing. A package closes that gap by carrying
//! the linked files too, each under the id of the record it belongs to:
//!
//! ```text
//! <library-name>-<stamp>.trove
//! ├── manifest.json                     what this package is, which library
//! ├── library.db                        consistent VACUUM snapshot
//! ├── library.json                      per-library preferences
//! ├── media/…                           content-addressed blobs
//! └── linked/<asset-id>/<file-name>     copies of the linked files
//! ```
//!
//! Installing unpacks the tree into a fresh library directory and then
//! reconnects the linked records: where the original file still exists at its
//! recorded path the link is left alone, and where it does not the packed
//! copy is staged into `media/` — content addressing verifies the bytes on
//! the way in — and the record becomes stored. Every derived structure the
//! library keeps elsewhere (thumbnails, the search indexes) is rebuilt when
//! the imported library next opens.
//!
//! The writer is the `zip` crate (deflate, streaming, zip64 — a library with
//! its linked files crosses both the 4 GiB and the 65 535-entry marks long
//! before a package should refuse). A package that cannot be finished is
//! removed rather than left half written where the user chose to put it.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use zip::write::SimpleFileOptions;

use crate::error::{Error, Result};
use crate::model::{Asset, AssetLocation, AssetQuery};
use crate::store;

/// Manifest format tag, bumped when the layout changes.
const FORMAT: &str = "trove-library";
/// Manifest format version.
const VERSION: u32 = 1;

/// The manifest at the top of every package: what it is, which library it
/// holds, and what the writer found. Read back by [`read_manifest`] — the
/// desktop app needs the library name to register the import under before
/// any unpacking happens.
#[derive(Debug, Clone, Deserialize)]
pub struct PackageManifest {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub application: String,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub exported_at: String,
    pub library: PackageLibrary,
    #[serde(default)]
    pub counts: PackageCounts,
}

/// The library a package was taken from. The `slug` is informational — an
/// import always allocates a fresh slug — but the name is what the new
/// library is registered under when the caller has nothing better.
#[derive(Debug, Clone, Deserialize)]
pub struct PackageLibrary {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
}

/// What the writer packed, repeated for the reader's summary.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PackageCounts {
    #[serde(default)]
    pub assets: u64,
    #[serde(default)]
    pub linked_packed: u64,
    #[serde(default)]
    pub linked_missing: u64,
}

impl PackageManifest {
    fn validate(&self) -> Result<()> {
        if self.format != FORMAT {
            return Err(Error::Validation(format!(
                "not a Trove repository package (format {:?}, expected {FORMAT:?})",
                self.format
            )));
        }
        if self.version > VERSION {
            return Err(Error::Validation(format!(
                "repository package version {} is newer than this build understands ({VERSION})",
                self.version
            )));
        }
        Ok(())
    }
}

/// Read and validate the manifest of a `.trove` package without unpacking it.
///
/// Every way this can refuse — not a zip, no manifest, a foreign format tag,
/// a version from the future — reads as the same verdict to the caller: the
/// file is not a repository package this build can install.
pub fn read_manifest(archive: &Path) -> Result<PackageManifest> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| not_a_package())?;
    let mut entry = zip
        .by_name("manifest.json")
        .map_err(|_| not_a_package())?;
    let manifest: PackageManifest = serde_json::from_reader(&mut entry)
        .map_err(|error| Error::Validation(format!("{}: {error}", not_a_package())))?;
    manifest.validate()?;
    Ok(manifest)
}

fn not_a_package() -> Error {
    Error::Validation("not a Trove repository package (manifest.json missing or unreadable)".into())
}

/// Outcome of [`export_library_package`].
#[derive(Debug, Clone, PartialEq)]
pub struct RepoExportReport {
    /// The package that was written.
    pub path: PathBuf,
    /// ZIP entries written, manifest included.
    pub files: u64,
    /// Sum of the entries' *uncompressed* sizes.
    pub bytes: u64,
    /// Linked files packed under `linked/`.
    pub linked_packed: u64,
    /// Linked records whose source file was not on disk when the package was
    /// taken — recorded in the manifest, not packed.
    pub linked_missing: u64,
}

/// Suggested file name for a new package: `<library-name>-<stamp>.trove`.
///
/// The name is flattened to a file-system-friendly form; a library whose name
/// flattens to nothing (say, one of punctuation) becomes `library`.
pub fn package_file_name(name: &str) -> String {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let base: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "library" } else { base };
    format!("{base}-{stamp}.trove")
}

/// Write the repository package for the library living at `library_dir` to
/// `dest` (a `.trove` path).
///
/// The function works on the directory, never on an open [`Library`] — which
/// is not `Send` — so it runs on a background thread while the desktop app
/// holds the library open: the database ships as a `VACUUM INTO` snapshot,
/// consistent under concurrent writes. A package that cannot be finished is
/// removed rather than left half written where the user chose to put it.
pub fn export_library_package(
    library_dir: &Path,
    library_name: &str,
    dest: &Path,
) -> Result<RepoExportReport> {
    match build_package(library_dir, library_name, dest) {
        Ok(report) => Ok(report),
        Err(error) => {
            let _ = std::fs::remove_file(dest);
            Err(error)
        }
    }
}

fn build_package(
    library_dir: &Path,
    library_name: &str,
    dest: &Path,
) -> Result<RepoExportReport> {
    let snapshot = vacuum_snapshot(&library_dir.join("library.db"))?;
    let result = write_package(library_dir, library_name, dest, &snapshot);
    let _ = std::fs::remove_file(&snapshot);
    result
}

fn write_package(
    library_dir: &Path,
    library_name: &str,
    dest: &Path,
    snapshot: &Path,
) -> Result<RepoExportReport> {
    // The snapshot is a private copy: read the asset rows from it, so the
    // package and its manifest describe exactly one instant of the library.
    let conn = rusqlite::Connection::open(snapshot)
        .map_err(|e| Error::Db(format!("opening the snapshot: {e}")))?;
    let assets = enumerate_assets(&conn)?;
    let slug = library_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("library")
        .to_string();

    let file = std::fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut files = 0u64;
    let mut bytes = 0u64;

    // The library's own media, then the linked files under the id of the
    // record each one belongs to — a source that has vanished since the
    // import is counted, not packed, so the manifest tells the reader what
    // the package cannot restore.
    let (n, b) = add_tree(&mut zip, "media", &library_dir.join("media"))?;
    files += n;
    bytes += b;
    let mut linked_packed = 0u64;
    let mut linked_missing = 0u64;
    for asset in &assets {
        let AssetLocation::Linked { source_path } = asset.location() else {
            continue;
        };
        let source = PathBuf::from(&source_path);
        if !source.is_file() {
            linked_missing += 1;
            continue;
        }
        let name = std::path::Path::new(&asset.file_name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("asset");
        files += 1;
        bytes += start_and_copy(
            &mut zip,
            &format!("linked/{}/{}", asset.id, name),
            &source,
            &options,
        )?;
        linked_packed += 1;
    }

    // The manifest last: it carries the counts above, and no reader minds —
    // `read_manifest` looks the entry up by name, not by position.
    let manifest = serde_json::json!({
        "format": FORMAT,
        "version": VERSION,
        "application": "trove",
        "app_version": env!("CARGO_PKG_VERSION"),
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "library": { "slug": slug, "name": library_name },
        "counts": {
            "assets": assets.len() as u64,
            "linked_packed": linked_packed,
            "linked_missing": linked_missing,
        },
    });
    let text = serde_json::to_string_pretty(&manifest)?;
    zip.start_file("manifest.json", options)
        .map_err(zip_error)?;
    std::io::Write::write_all(&mut zip, text.as_bytes())?;
    files += 1;
    bytes += text.len() as u64;

    // The database ships as the snapshot taken at the top of this function.
    files += 1;
    bytes += start_and_copy(&mut zip, "library.db", snapshot, &options)?;

    // Preferences when the library has any; a fresh library may not.
    let prefs = library_dir.join("library.json");
    if prefs.is_file() {
        files += 1;
        bytes += start_and_copy(&mut zip, "library.json", &prefs, &options)?;
    }

    zip.finish().map_err(zip_error)?;
    Ok(RepoExportReport {
        path: dest.to_path_buf(),
        files,
        bytes,
        linked_packed,
        linked_missing,
    })
}

/// Every asset record in the snapshot — live and trashed both: the database
/// is restored whole, so the package has to carry what the trash refers to.
fn enumerate_assets(conn: &rusqlite::Connection) -> Result<Vec<Asset>> {
    let mut assets = Vec::new();
    for pool in [AssetQuery::live(), AssetQuery::trashed()] {
        assets.extend(store::assets::query(conn, &pool)?.items);
    }
    Ok(assets)
}

/// Outcome of [`install_library_package`].
#[derive(Debug, Clone, PartialEq)]
pub struct RepoImportReport {
    /// The library name the package records.
    pub library_name: String,
    /// Asset records the package's manifest counted.
    pub assets_total: u64,
    /// Linked records whose file arrived from the package and now lives in
    /// the library's own store.
    pub materialized: u64,
    /// Linked records whose original file still exists where it was — left
    /// linked, nothing copied.
    pub kept_linked: u64,
    /// Linked records with neither an original on disk nor a copy in the
    /// package. The records are intact; the app shows them as it shows any
    /// file that went missing, and `Library::relink_asset` reconnects one
    /// when its file turns up.
    pub missing: u64,
}

/// Unpack a `.trove` package into `dest_dir` — a fresh library directory a
/// caller obtained from the registry — and reconnect its linked records.
///
/// The database, preferences and media tree are laid out exactly as the
/// library expects them; the caller then opens the library as usual and the
/// derived structures rebuild themselves. Unknown entry prefixes are ignored,
/// so a package written by a newer version still installs what this one
/// understands.
pub fn install_library_package(archive: &Path, dest_dir: &Path) -> Result<RepoImportReport> {
    let manifest = read_manifest(archive)?;
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(zip_error)?;

    // Pass one: everything but the linked copies — the tree a library
    // directory is made of. The manifest and the linked/ tree are left for
    // the passes that know what to do with them.
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(zip_error)?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        if !unpack_entry(&mut entry, &rel, dest_dir)? {
            continue;
        }
    }

    // Pass two: reconcile the linked records with what the package carries.
    // The database is on disk now, so every record can say what became of
    // its file — including the ones the package has no copy for, which the
    // `linked/` tree alone would never mention.
    let conn = rusqlite::Connection::open(dest_dir.join("library.db"))
        .map_err(|e| Error::Db(format!("opening the restored database: {e}")))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
    let mut report = RepoImportReport {
        library_name: manifest.library.name.clone(),
        assets_total: manifest.counts.assets,
        materialized: 0,
        kept_linked: 0,
        missing: 0,
    };

    // The copies the package carries, by the id of the record each is for.
    let mut packed: std::collections::HashMap<uuid::Uuid, String> =
        std::collections::HashMap::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(zip_error)?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        if let Some((id, name)) = linked_entry(&rel) {
            packed.insert(id, name);
        }
    }

    for asset in enumerate_assets(&conn)? {
        let AssetLocation::Linked { source_path } = asset.location() else {
            continue;
        };
        if Path::new(&source_path).is_file() {
            // The original is where it was: the link stays, nothing is
            // copied.
            report.kept_linked += 1;
            continue;
        }
        let Some(name) = packed.get(&asset.id) else {
            report.missing += 1;
            continue;
        };
        let mut entry = zip
            .by_name(&format!("linked/{}/{}", asset.id, name))
            .map_err(zip_error)?;
        if materialize(&conn, &asset, &mut entry, dest_dir)? {
            report.materialized += 1;
        } else {
            report.missing += 1;
        }
    }
    Ok(report)
}

/// Stage one packed copy into the library's own store and turn its record
/// stored. Content addressing is the integrity check: the bytes are hashed
/// on the way in, and a copy that does not hash to the recorded value is not
/// the file it claims to be — the record stays linked (to a path that may
/// come back), the copy is dropped, and the import carries on rather than
/// letting one drifted file fail the whole package. Returns whether the
/// record was converted.
fn materialize(
    conn: &rusqlite::Connection,
    asset: &Asset,
    entry: &mut zip::read::ZipFile<'_>,
    dest_dir: &Path,
) -> Result<bool> {
    let recorded = asset.content_hash.as_deref().unwrap_or_default();
    // Extract next to where the blob will live, so staging renames within
    // one filesystem instead of copying across devices.
    let staging = dest_dir
        .join("media")
        .join(format!(".pkg-{}", uuid::Uuid::new_v4()));
    if let Some(parent) = staging.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let converted = (|| -> Result<bool> {
        let mut out = std::fs::File::create(&staging)?;
        std::io::copy(entry, &mut out)?;
        drop(out);
        let staged = crate::media::blob::stage(&staging, dest_dir, &asset.ext)?;
        if !staged.content_hash.eq_ignore_ascii_case(recorded) {
            tracing::warn!(
                id = %asset.id,
                name = %asset.file_name,
                recorded = %recorded,
                found = %staged.content_hash,
                "repository package: a linked copy does not match its record; \
                 the record stays linked"
            );
            // The copy is not the record's content — drop it, unless the
            // name already belonged to another asset's blob.
            if !staged.existed {
                let _ = std::fs::remove_file(dest_dir.join(&staged.rel_path));
            }
            return Ok(false);
        }
        store::assets::set_stored_blob(conn, asset.id, &staged.rel_path)?;
        Ok(true)
    })();
    let _ = std::fs::remove_file(&staging);
    converted
}

/// `linked/<asset-id>/<file-name>` → `(id, file-name)`, or `None` for any
/// other entry. The id is the record the writer packed the file for; a name
/// that does not parse is a package written by something else, and the entry
/// is skipped.
fn linked_entry(rel: &Path) -> Option<(uuid::Uuid, String)> {
    let mut components = rel.components();
    if components.next()?.as_os_str() != "linked" {
        return None;
    }
    let id = uuid::Uuid::parse_str(components.next()?.as_os_str().to_str()?).ok()?;
    let name = components.next()?.as_os_str().to_str()?.to_string();
    if components.next().is_some() {
        return None;
    }
    Some((id, name))
}

/// Pass-one unpack: `library.db`, `library.json` and the `media/` tree land
/// in the library directory, everything else waits. Returns whether the
/// entry was written.
fn unpack_entry(
    entry: &mut zip::read::ZipFile<'_>,
    rel: &Path,
    dest_dir: &Path,
) -> Result<bool> {
    let mut components = rel.components();
    let top = components.next().expect("enclosed_name is non-empty");
    let keep = match top.as_os_str().to_str() {
        Some("library.db") | Some("library.json") => rel.components().count() == 1,
        Some("media") => true,
        _ => false,
    };
    if !keep {
        return Ok(false);
    }
    let dest = dest_dir.join(rel);
    if entry.is_dir() {
        std::fs::create_dir_all(&dest)?;
        return Ok(true);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(&dest)?;
    std::io::copy(entry, &mut out)?;
    Ok(true)
}

fn start_and_copy(
    zip: &mut zip::ZipWriter<std::fs::File>,
    name: &str,
    src: &Path,
    options: &SimpleFileOptions,
) -> Result<u64> {
    zip.start_file(name, *options).map_err(zip_error)?;
    Ok(std::io::copy(&mut std::fs::File::open(src)?, zip)?)
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error::Io(io::Error::other(error))
}

/// Add every file under `root` (when it exists) under the entry prefix
/// `prefix/`, returning the `(files, bytes)` it contributed. Directories the
/// package has no use for are skipped: the per-library `backups/` snapshots
/// (a package is not the place for a backup of a backup) and SQLite's
/// sidecar files, which belong to a live database and are folded into its
/// snapshot instead.
fn add_tree(
    zip: &mut zip::ZipWriter<std::fs::File>,
    prefix: &str,
    root: &Path,
) -> Result<(u64, u64)> {
    if !root.is_dir() {
        return Ok((0, 0));
    }
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut files = 0u64;
    let mut bytes = 0u64;
    'outer: for path in walk(root)? {
        let rel = path
            .strip_prefix(root)
            .expect("walk returns paths under its root");
        for part in rel.components() {
            if part.as_os_str() == std::ffi::OsStr::new("backups") {
                continue 'outer;
            }
        }
        let name = match path.file_name().and_then(|f| f.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if name.ends_with(".db-wal") || name.ends_with(".db-shm") {
            continue;
        }
        // The one special case: a live `library.db` is snapshotted, not read
        // mid-write. Everything else streams in as-is.
        if name == "library.db" {
            let snapshot = vacuum_snapshot(&path)?;
            let out = start_and_copy(zip, &format!("{prefix}/library.db"), &snapshot, &options);
            let _ = std::fs::remove_file(&snapshot);
            files += 1;
            bytes += out?;
            continue;
        }
        let rel = rel.to_string_lossy().replace('\\', "/");
        files += 1;
        bytes += start_and_copy(zip, &format!("{prefix}/{rel}"), &path, &options)?;
    }
    Ok((files, bytes))
}

/// Every file under `root`, depth-first, root itself excluded.
fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in read_dirs(&dir)? {
            if entry.is_dir() {
                stack.push(entry);
            } else {
                files.push(entry);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn read_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    // A missing root is the normal state of a fresh library (`media/` is
    // created on demand), not an error; anything else — permissions
    // included — is.
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries.flatten().map(|e| e.path()).collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// A consistent copy of `library.db` via SQLite's `VACUUM INTO` — the same
/// mechanism the per-library backups use, safe while other connections are
/// writing. The snapshot lands in the temp directory and is the caller's to
/// delete.
fn vacuum_snapshot(db: &Path) -> Result<PathBuf> {
    let snapshot = std::env::temp_dir().join(format!(
        "trove-repo-package-{}-{}.db",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let conn =
        rusqlite::Connection::open(db).map_err(|e| Error::Db(format!("{}: {e}", db.display())))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
    conn.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])
        .map_err(|e| Error::Db(format!("snapshotting {}: {e}", db.display())))?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch pair of roots, torn down on drop. Tests open real libraries
    /// in here through [`Library::open`] — nothing touches the machine's real
    /// configuration, and no process-wide state is mutated.
    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "trove-repo-package-{tag}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn library_dir(&self, slug: &str) -> PathBuf {
            self.root.join("data/libraries").join(slug)
        }

        fn cache_dir(&self, slug: &str) -> PathBuf {
            self.root.join("cache/libraries").join(slug)
        }

        /// Where the seeded sources live — the "user's files", outside every
        /// library. A test simulates a machine without them by deleting this.
        fn sources_dir(&self) -> PathBuf {
            self.root.join("sources")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// One 1×1 PNG, the smallest file the importer will probe.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
        0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
        0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
        0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// Two plain text files with distinct bytes — distinct, because two
    /// imports of one content collapse into one record.
    const LINKED_BYTES: &[u8] = b"linked-file-one";
    const GONE_BYTES: &[u8] = b"linked-file-two";

    /// Seed a library with one stored asset (a copy in `media/`), one linked
    /// asset whose source exists, one linked asset whose source is gone, and
    /// one trashed stored asset. The sources live in the sandbox's `sources/`
    /// directory — outside the library, where a user's linked files are.
    /// Returns the ids of the three records.
    fn seed_library(
        sandbox: &Sandbox,
        slug: &str,
    ) -> (uuid::Uuid, uuid::Uuid, uuid::Uuid) {
        let root = sandbox.library_dir(slug);
        let sources = sandbox.sources_dir();
        std::fs::create_dir_all(&sources).unwrap();
        let stored_src = sources.join("stored.png");
        std::fs::write(&stored_src, PNG_1X1).unwrap();
        let linked_src = sources.join("linked.txt");
        std::fs::write(&linked_src, LINKED_BYTES).unwrap();
        let gone_src = sources.join("gone.txt");
        std::fs::write(&gone_src, GONE_BYTES).unwrap();

        // Preferences on disk, so the package has one to carry.
        std::fs::write(root.join("library.json"), "{}").unwrap();
        let lib = crate::library::Library::open(&root, &sandbox.cache_dir(slug)).unwrap();
        let stored = lib
            .import_into_store(std::slice::from_ref(&stored_src), None)
            .unwrap();
        let linked = lib.link_files(&[linked_src.clone()], None).unwrap();
        let gone = lib.link_files(&[gone_src.clone()], None).unwrap();
        std::fs::remove_file(&gone_src).unwrap();

        let stored_id = stored.imported[0].asset_id;
        let linked_id = linked.imported[0].asset_id;
        let gone_id = gone.imported[0].asset_id;
        assert_ne!(linked_id, gone_id, "distinct content, distinct records");
        let trashed = lib.trash_assets(&[stored_id]).unwrap();
        assert_eq!(trashed, 1);
        (stored_id, linked_id, gone_id)
    }

    fn entry_names(archive: &mut zip::ZipArchive<std::fs::File>) -> Vec<String> {
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    fn read_entry(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Vec<u8> {
        let mut entry = archive.by_name(name).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn package_holds_records_media_and_linked_copies() {
        let sandbox = Sandbox::new("export");
        let dir = sandbox.library_dir("work");
        std::fs::create_dir_all(&dir).unwrap();
        seed_library(&sandbox, "work");

        let dest = sandbox.root.join("work.trove");
        let report =
            export_library_package(&dir, "Work", &dest).unwrap();
        assert_eq!(report.path, dest);
        assert_eq!(report.linked_packed, 1, "one source present, one gone");
        assert_eq!(report.linked_missing, 1);

        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names = entry_names(&mut archive);
        for expected in ["manifest.json", "library.db", "library.json"] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}: {names:?}");
        }
        assert!(
            names.iter().any(|n| n.starts_with("media/") && n.ends_with(".png")),
            "the stored blob rides along: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("backups/")),
            "snapshot-of-snapshot must not ride along: {names:?}"
        );

        // The manifest names the library and the counts it was taken with.
        let manifest: PackageManifest = serde_json::from_slice(&read_entry(
            &mut archive,
            "manifest.json",
        ))
        .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.library.name, "Work");
        assert_eq!(manifest.library.slug, "work");
        assert_eq!(manifest.counts.assets, 3, "live and trashed both");
        assert_eq!(manifest.counts.linked_packed, 1);
        assert_eq!(manifest.counts.linked_missing, 1);

        // The database ships as a readable snapshot.
        let db = sandbox.root.join("snapshot-check.db");
        std::fs::write(&db, read_entry(&mut archive, "library.db")).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
        std::fs::remove_file(&db).ok();

        // The one linked file whose source existed is in the package, under
        // the id of its record, with its original bytes.
        let linked_name = names
            .iter()
            .find(|n| n.starts_with("linked/") && n.ends_with(".txt"))
            .expect("the linked copy is in the package");
        assert_eq!(read_entry(&mut archive, linked_name), LINKED_BYTES);
    }

    /// A failure leaves no partial package behind where the user chose to put
    /// it. The failure is a corrupt database — the realistic way an export
    /// dies mid-write.
    #[test]
    fn a_failed_export_is_removed_not_left_half_written() {
        let sandbox = Sandbox::new("fail");
        let dir = sandbox.library_dir("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("library.db"), b"this is not a database").unwrap();

        let dest = sandbox.root.join("broken.trove");
        assert!(export_library_package(&dir, "Broken", &dest).is_err());
        assert!(!dest.exists(), "no half-written package may survive");
    }

    /// The round trip: export, then take the source files away — a package
    /// carried to another machine, or a library whose originals were since
    /// deleted — then install. The linked asset comes back stored, with the
    /// exact original bytes; the one whose source was already gone at export
    /// has no copy to become, and stays a linked record pointing nowhere;
    /// the trashed asset is still in the trash.
    #[test]
    fn an_install_restores_the_library_and_reconnects_links() {
        let sandbox = Sandbox::new("roundtrip");
        let dir = sandbox.library_dir("work");
        std::fs::create_dir_all(&dir).unwrap();
        let (stored_id, linked_id, gone_id) = seed_library(&sandbox, "work");

        let dest = sandbox.root.join("work.trove");
        export_library_package(&dir, "Work", &dest).unwrap();
        // The new machine has none of the source files.
        std::fs::remove_dir_all(sandbox.sources_dir()).unwrap();

        let installed = sandbox.library_dir("imported");
        std::fs::create_dir_all(&installed).unwrap();
        let report = install_library_package(&dest, &installed).unwrap();
        assert_eq!(report.library_name, "Work");
        assert_eq!(report.assets_total, 3);
        assert_eq!(report.kept_linked, 0, "no source survived the move");
        assert_eq!(
            report.materialized, 1,
            "the source packed before it vanished returns stored"
        );
        assert_eq!(report.missing, 1, "the source that was already gone");

        let lib = crate::library::Library::open(&installed, &sandbox.cache_dir("imported"))
            .unwrap();
        let stored = lib.asset(stored_id).unwrap().unwrap();
        assert!(
            matches!(stored.location(), AssetLocation::Stored { .. }),
            "the trashed asset keeps its blob: {:?}",
            stored.location()
        );
        assert!(stored.placement().is_trashed(), "the trash travels");

        let linked = lib.asset(linked_id).unwrap().unwrap();
        let AssetLocation::Stored { rel_path } = linked.location() else {
            panic!("the packed source must come back stored: {:?}", linked.location());
        };
        assert_eq!(std::fs::read(installed.join(&rel_path)).unwrap(), LINKED_BYTES);

        let gone = lib.asset(gone_id).unwrap().unwrap();
        assert!(
            matches!(gone.location(), AssetLocation::Linked { .. }),
            "with no copy to become, the record stays linked: {:?}",
            gone.location()
        );
    }

    /// Install where the source files still exist at their recorded paths —
    /// the same machine, or a mount that moved with the library: the links
    /// stay links and nothing is copied for them.
    #[test]
    fn an_install_beside_the_surviving_sources_keeps_the_links() {
        let sandbox = Sandbox::new("kept-links");
        let dir = sandbox.library_dir("work");
        std::fs::create_dir_all(&dir).unwrap();
        let (_stored_id, linked_id, _gone_id) = seed_library(&sandbox, "work");

        let dest = sandbox.root.join("work.trove");
        export_library_package(&dir, "Work", &dest).unwrap();

        let installed = sandbox.library_dir("imported");
        std::fs::create_dir_all(&installed).unwrap();
        let report = install_library_package(&dest, &installed).unwrap();
        assert_eq!(report.kept_linked, 1, "the surviving source stays linked");
        assert_eq!(report.materialized, 0);
        assert_eq!(report.missing, 1, "the source that was gone before export");

        let lib = crate::library::Library::open(&installed, &sandbox.cache_dir("imported"))
            .unwrap();
        let linked = lib.asset(linked_id).unwrap().unwrap();
        let AssetLocation::Linked { source_path } = linked.location() else {
            panic!("the surviving source must stay linked: {:?}", linked.location());
        };
        assert_eq!(std::fs::read(&source_path).unwrap(), LINKED_BYTES);
    }

    /// Not a package: the install refuses before touching the destination,
    /// and a manifest claiming a newer version is turned away too.
    #[test]
    fn an_install_rejects_what_it_does_not_understand() {
        let sandbox = Sandbox::new("reject");
        let installed = sandbox.library_dir("imported");
        std::fs::create_dir_all(&installed).unwrap();

        let bogus = sandbox.root.join("bogus.trove");
        std::fs::write(&bogus, b"this is not a zip").unwrap();
        assert!(read_manifest(&bogus).is_err());
        assert!(install_library_package(&bogus, &installed).is_err());

        // A manifest whose version is ahead of this build: readable, refused.
        let future = sandbox.root.join("future.trove");
        {
            let file = std::fs::File::create(&future).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file(
                "manifest.json",
                SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            std::io::Write::write_all(
                &mut zip,
                br#"{"format":"trove-library","version":99,"library":{"name":"X"}}"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        assert!(read_manifest(&future).is_err());
        assert!(install_library_package(&future, &installed).is_err());
    }
}
