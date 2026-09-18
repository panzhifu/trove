//! The full-backup archive: every piece of Trove's own data in one ZIP.
//!
//! A backup here means *everything needed to reconstruct a machine's Trove*:
//! the software configuration (`config.json` — the library registry, every
//! preference, keybindings, plugin settings) plus every library's data
//! (database, `library.json`, the `media/` blob store) and the `incoming/`
//! files libraries link. Only the cache root — thumbnails, the full-text
//! index — is left out, because it is derived data the app rebuilds on its
//! own.
//!
//! The ZIP's entry layout mirrors the platform roots one level down, so a
//! restore is "unzip `config/` over the config dir and `data/` over the data
//! dir":
//!
//! ```text
//! trove-backup-<stamp>.zip
//! ├── manifest.json                     what this archive is, which libraries
//! ├── config/config.json                preferences + library registry
//! ├── config/history.json               recent colours, recent libraries
//! ├── config/themes/…                   user themes
//! ├── data/incoming/…                   screenshots and extension uploads
//! └── data/libraries/<slug>/
//!     ├── library.db                    consistent VACUUM snapshot
//!     ├── library.json                  per-library preferences
//!     └── media/…                       content-addressed blobs
//! ```
//!
//! The writer is the `zip` crate (deflate, streaming, zip64 — a media store
//! crosses both the 4 GiB and the 65 535-entry marks long before a backup
//! should refuse). An archive that cannot be finished is removed rather than
//! left half written where the user chose to put it.

use std::io;
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;

use crate::config::AppConfig;
use crate::error::{Error, Result};
use crate::paths;

/// Manifest format tag, bumped when the layout changes.
const FORMAT: &str = "trove-backup";
/// Manifest format version.
const VERSION: u32 = 1;

/// Outcome of [`create_full_backup`].
#[derive(Debug, Clone, PartialEq)]
pub struct FullBackupReport {
    /// The archive that was written.
    pub path: PathBuf,
    /// Files (ZIP entries) written, manifest included.
    pub files: u64,
    /// Sum of the entries' *uncompressed* sizes.
    pub bytes: u64,
}

/// Suggested file name for a new archive: `trove-backup-<stamp>.zip`.
pub fn backup_file_name() -> String {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    format!("trove-backup-{stamp}.zip")
}

/// Write the full-backup archive to `dest` (a `.zip` path) from the live
/// config and data roots.
pub fn create_full_backup(dest: &Path) -> Result<FullBackupReport> {
    write_full_backup(dest, &paths::config_dir(), &paths::data_dir())
}

/// The archive writer proper, parameterized over the two roots so tests can
/// point it at a sandbox instead of the machine's real configuration. An
/// archive that cannot be finished is removed rather than left half written
/// where the user chose to put it.
fn write_full_backup(
    dest: &Path,
    config_dir: &Path,
    data_dir: &Path,
) -> Result<FullBackupReport> {
    match build_archive(dest, config_dir, data_dir) {
        Ok(report) => Ok(report),
        Err(error) => {
            let _ = std::fs::remove_file(dest);
            Err(error)
        }
    }
}

fn build_archive(dest: &Path, config_dir: &Path, data_dir: &Path) -> Result<FullBackupReport> {
    let file = std::fs::File::create(dest)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut files = 0u64;
    let mut bytes = 0u64;

    let add = |zip: &mut zip::ZipWriter<std::fs::File>,
                   name: String,
                   src: &Path|
     -> Result<u64> {
        zip.start_file(name.as_str(), options)
            .map_err(zip_error)?;
        let size = std::io::copy(&mut std::fs::File::open(src)?, zip)?;
        Ok(size)
    };

    // The manifest first: what this archive is and which libraries it holds.
    let manifest = write_manifest(dest, config_dir, data_dir)?;
    match add(&mut zip, "manifest.json".to_string(), &manifest) {
        Ok(size) => {
            files += 1;
            bytes += size;
        }
        Err(error) => {
            let _ = std::fs::remove_file(&manifest);
            let _ = zip.finish();
            return Err(error);
        }
    }
    let _ = std::fs::remove_file(&manifest);

    // Software data: config files and user themes. `config.json` carries the
    // library registry — without it the `data/libraries/` entries would have
    // no one pointing at them.
    for file in ["config.json", "history.json"] {
        let path = config_dir.join(file);
        if path.is_file() {
            files += 1;
            bytes += add(&mut zip, format!("config/{file}"), &path)?;
        }
    }
    let (n, b) = add_tree(&mut zip, "config/themes", &config_dir.join("themes"))?;
    files += n;
    bytes += b;

    // Library data: every library directory, snapshots' snapshots (backups/)
    // excepted. The databases go in as fresh VACUUM snapshots, so an archive
    // taken while a session is running is still consistent.
    let libraries = data_dir.join("libraries");
    for entry in read_dirs(&libraries)? {
        if !entry.is_dir() {
            continue;
        }
        let slug = file_name(&entry);
        let (n, b) = add_tree(&mut zip, &format!("data/libraries/{slug}"), &entry)?;
        files += n;
        bytes += b;
    }

    // Files Trove produced itself and then linked into a library; losing them
    // would break exactly those assets.
    let (n, b) = add_tree(&mut zip, "data/incoming", &data_dir.join("incoming"))?;
    files += n;
    bytes += b;

    zip.finish().map_err(zip_error)?;
    Ok(FullBackupReport {
        path: dest.to_path_buf(),
        files,
        bytes,
    })
}

/// The manifest: what the archive is, which application wrote it, and the
/// library registry as it stood — the human-readable index of what `data/`
/// holds. The registry is read from `config_dir` (not the live config paths)
/// so the manifest always describes exactly the archive it travels in.
/// Written to a sibling temp file so it streams in like every other entry,
/// and the caller deletes it once it is in.
fn write_manifest(dest: &Path, config_dir: &Path, data_dir: &Path) -> Result<PathBuf> {
    let config: Option<AppConfig> = std::fs::read_to_string(config_dir.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let manifest = serde_json::json!({
        "format": FORMAT,
        "version": VERSION,
        "application": "trove",
        "app_version": env!("CARGO_PKG_VERSION"),
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "libraries": config
            .as_ref()
            .map(|c| c.libraries.clone())
            .unwrap_or_default(),
        "active_library": config
            .as_ref()
            .map(|c| c.active_slug())
            .unwrap_or_else(|| paths::DEFAULT_LIBRARY_SLUG.to_string()),
        // A restore unzips `config/` over the platform config dir and `data/`
        // over the data dir; the cache root is deliberately absent.
        "layout": {
            "config/": config_dir,
            "data/": data_dir,
        },
    });
    let text = serde_json::to_string_pretty(&manifest)?;
    let temp = dest.with_extension(format!("manifest-{}.tmp", std::process::id()));
    std::fs::write(&temp, text)?;
    Ok(temp)
}

/// Add every file under `root` (when it exists) under the entry prefix
/// `prefix/`, returning the `(files, bytes)` it contributed. Directories a
/// backup regenerates or has no use for are skipped: the per-library
/// `backups/` snapshots (a backup of a backup) and SQLite's sidecar files,
/// which belong to a live database and are folded into its snapshot instead.
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

fn start_and_copy(
    zip: &mut zip::ZipWriter<std::fs::File>,
    name: &str,
    src: &Path,
    options: &SimpleFileOptions,
) -> Result<u64> {
    zip.start_file(name, *options)
        .map_err(zip_error)?;
    Ok(std::io::copy(&mut std::fs::File::open(src)?, zip)?)
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error::Io(io::Error::other(error))
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
    // A missing root is the normal state of a fresh install (`libraries/`,
    // `incoming/`), not an error; anything else — permissions included — is.
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(entries.flatten().map(|e| e.path()).collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A consistent copy of `library.db` via SQLite's `VACUUM INTO` — the same
/// mechanism the per-library backups use, safe while other connections are
/// writing. The snapshot lands in the temp directory and is the caller's to
/// delete.
fn vacuum_snapshot(db: &Path) -> Result<PathBuf> {
    let snapshot = std::env::temp_dir().join(format!(
        "trove-backup-{}-{}.db",
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

    /// A scratch pair of roots, torn down on drop. Tests hand these straight
    /// to [`write_full_backup`] — nothing here touches the machine's real
    /// configuration, and no process-wide state is mutated (parallel tests
    /// would race on it).
    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "trove-archive-{tag}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(root.join("config/themes")).unwrap();
            std::fs::create_dir_all(root.join("data")).unwrap();
            Self { root }
        }

        fn config_dir(&self) -> PathBuf {
            self.root.join("config")
        }

        fn data_dir(&self) -> PathBuf {
            self.root.join("data")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Write a small but real library database at `dir/library.db`.
    fn seed_library(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("library.db")).unwrap();
        conn.execute_batch("CREATE TABLE demo (x TEXT); INSERT INTO demo VALUES ('v1');")
            .unwrap();
        std::fs::write(dir.join("library.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("media/ab")).unwrap();
        std::fs::write(dir.join("media/ab/blob1"), b"blob-one-bytes").unwrap();
        // A snapshot directory and WAL sidecars must be skipped, not copied.
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        std::fs::write(dir.join("backups/library-1.db"), b"old-snapshot").unwrap();
        std::fs::write(dir.join("library.db-wal"), b"live-wal").unwrap();
    }

    fn entry_names(archive: &mut zip::ZipArchive<std::fs::File>) -> Vec<String> {
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[test]
    fn archive_holds_config_libraries_and_incoming() {
        let sandbox = Sandbox::new("full");
        let config = sandbox.config_dir();
        std::fs::write(
            config.join("config.json"),
            r#"{ "libraries": [{ "slug": "work", "name": "Work" }] }"#,
        )
        .unwrap();
        std::fs::write(config.join("themes/dark.json"), "{}").unwrap();
        seed_library(&sandbox.data_dir().join("libraries/work"));
        std::fs::create_dir_all(sandbox.data_dir().join("incoming")).unwrap();
        std::fs::write(sandbox.data_dir().join("incoming/shot.png"), b"png").unwrap();

        let dest = sandbox.root.join("trove-backup.zip");
        let report = write_full_backup(&dest, &config, &sandbox.data_dir()).unwrap();

        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names = entry_names(&mut archive);
        for expected in [
            "manifest.json",
            "config/config.json",
            "config/themes/dark.json",
            "data/libraries/work/library.json",
            "data/libraries/work/media/ab/blob1",
            "data/incoming/shot.png",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected}: {names:?}"
            );
        }

        // The database ships as a snapshot: no WAL sidecar beside it, and its
        // content is the seeded one, readable from the extracted entry.
        assert!(
            !names.iter().any(|n| n.ends_with("-wal") || n.ends_with("-shm")),
            "no live-database sidecars in the archive: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("backups/")),
            "snapshot-of-snapshot must not ride along: {names:?}"
        );
        let mut db_entry = archive
            .by_name("data/libraries/work/library.db")
            .unwrap();
        let snapshot = sandbox.root.join("extracted.db");
        std::io::copy(&mut db_entry, &mut std::fs::File::create(&snapshot).unwrap()).unwrap();
        drop(db_entry);
        let conn = rusqlite::Connection::open(&snapshot).unwrap();
        let value: String = conn
            .query_row("SELECT x FROM demo", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "v1");

        // The manifest names the registry the archive was taken from.
        let mut manifest = archive.by_name("manifest.json").unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut manifest, &mut text).unwrap();
        assert!(text.contains(r#""slug": "work""#), "{text}");
        assert!(text.contains(FORMAT), "{text}");

        assert_eq!(report.path, dest);
        assert!(report.files >= 6, "{report:?}");
        assert!(report.bytes > 0, "{report:?}");
        std::fs::remove_file(&snapshot).ok();
    }

    /// A failure leaves no partial archive behind where the user chose to put
    /// the backup. The failure is a corrupt database — the realistic way a
    /// backup dies mid-write.
    #[test]
    fn a_failed_archive_is_removed_not_left_half_written() {
        let sandbox = Sandbox::new("fail");
        seed_library(&sandbox.data_dir().join("libraries/work"));
        // Not a SQLite file: `VACUUM INTO` refuses it once the archive build
        // reaches this library.
        let broken = sandbox.data_dir().join("libraries/broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("library.db"), b"this is not a database").unwrap();

        let dest = sandbox.root.join("trove-backup.zip");
        assert!(write_full_backup(&dest, &sandbox.config_dir(), &sandbox.data_dir()).is_err());
        assert!(!dest.exists(), "no half-written archive may survive");
    }

    /// No libraries and no config at all: the archive still builds, holding
    /// just the manifest — a valid, if boring, backup.
    #[test]
    fn an_empty_install_yields_a_manifest_only_archive() {
        let sandbox = Sandbox::new("empty");
        let dest = sandbox.root.join("trove-backup.zip");
        let report = write_full_backup(&dest, &sandbox.config_dir(), &sandbox.data_dir()).unwrap();
        assert_eq!(report.files, 1, "{report:?}");
        let file = std::fs::File::open(&dest).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(entry_names(&mut archive), vec!["manifest.json"]);
    }
}
