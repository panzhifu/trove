//! How much disk Trove is using, broken down the way the settings page shows
//! it: the library's own data, the parts that can be deleted and rebuilt, the
//! logs, and the files waiting to be imported.
//!
//! Walking these trees is the only way to get a real number — the database
//! knows how big the *assets* are, but nothing tracks the bytes the
//! application itself has written. The walk is pure IO over directories that
//! are small (thousands of files at worst), but it is still IO, so callers
//! should run it off the UI thread.

use std::path::Path;

use crate::paths;

/// Bytes and file count under one directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirUsage {
    pub bytes: u64,
    pub files: u64,
}

impl DirUsage {
    /// Total of two measurements, for rolling a tree up into its parts.
    fn plus(self, other: Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            files: self.files + other.files,
        }
    }

    /// Nothing has been written here yet.
    pub fn is_empty(&self) -> bool {
        self.files == 0
    }
}

/// The whole picture, one field per line the settings page draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageReport {
    /// Settings, themes and history — `~/.config/trove`.
    pub config: DirUsage,
    /// The library database itself (`library.db`).
    pub library_db: DirUsage,
    /// Database snapshots under `<library>/backups`.
    pub backups: DirUsage,
    /// Blobs under `<library>/media`. Only ever non-empty for assets that
    /// were stored before linking became the only import mode.
    pub media: DirUsage,
    /// Thumbnails under the cache root — deletable, rebuildable.
    pub thumbs: DirUsage,
    /// The full-text index under the cache root — deletable, rebuildable.
    pub search_index: DirUsage,
    /// Log files under the state root.
    pub logs: DirUsage,
    /// Screenshots and collected files waiting to be imported, or already
    /// imported and linked from here.
    pub incoming: DirUsage,
    /// Everything above, added up.
    pub total: DirUsage,
}

impl StorageReport {
    /// What the library takes up on disk, database and blobs together. The
    /// backups are counted separately: they are a safety net, not the library.
    pub fn library(&self) -> DirUsage {
        self.library_db.plus(self.media)
    }

    /// What can be deleted and regenerated: the cache root's two tenants.
    pub fn cache(&self) -> DirUsage {
        self.thumbs.plus(self.search_index)
    }
}

/// Measure every directory the application owns, plus the open library's
/// parts.
///
/// Takes the library's two roots rather than the [`Library`] itself so the
/// walk can run on a background thread — a `Library` owns a database
/// connection and is not `Send`.
pub fn report(library_data_root: &Path, library_cache_root: &Path) -> StorageReport {
    let config = dir_usage(&paths::config_dir());
    let library_db = dir_usage(&library_data_root.join("library.db"));
    let backups = dir_usage(&library_data_root.join("backups"));
    let media = dir_usage(&library_data_root.join("media"));
    let thumbs = dir_usage(&library_cache_root.join("thumbs"));
    let search_index = dir_usage(&library_cache_root.join("search_index"));
    let logs = dir_usage(&paths::logs_dir());
    let incoming = dir_usage(&paths::incoming_dir());

    let total = config
        .plus(library_db)
        .plus(backups)
        .plus(media)
        .plus(thumbs)
        .plus(search_index)
        .plus(logs)
        .plus(incoming);

    StorageReport {
        config,
        library_db,
        backups,
        media,
        thumbs,
        search_index,
        logs,
        incoming,
        total,
    }
}

/// Bytes and file count under `path`. A missing directory is zero, not an
/// error: every one of these trees is created on demand.
///
/// Symlinks are counted as their own (tiny) entries rather than followed —
/// following them could walk into an unrelated tree, and nothing here writes
/// symlinks.
pub fn dir_usage(path: &Path) -> DirUsage {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return DirUsage::default();
    };
    if !meta.is_dir() {
        return DirUsage {
            bytes: meta.len(),
            files: 1,
        };
    }

    let mut usage = DirUsage::default();
    let Ok(entries) = std::fs::read_dir(path) else {
        return usage;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            usage = usage.plus(dir_usage(&child));
        } else {
            usage.bytes += meta.len();
            usage.files += 1;
        }
    }
    usage
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trove-usage-{name}-{}",
            crate::model::new_id().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_tree_measures_zero() {
        let usage = dir_usage(&std::env::temp_dir().join("trove-usage-does-not-exist"));
        assert_eq!(usage, DirUsage::default());
        assert!(usage.is_empty());
    }

    #[test]
    fn a_tree_sums_its_files_at_every_depth() {
        let root = temp_dir("tree");
        std::fs::write(root.join("top.bin"), vec![0u8; 10]).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/mid.bin"), vec![0u8; 20]).unwrap();
        std::fs::write(root.join("a/b/deep.bin"), vec![0u8; 30]).unwrap();

        let usage = dir_usage(&root);
        assert_eq!(usage.files, 3, "directories are not files");
        assert_eq!(usage.bytes, 60);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A file measured directly is one file, which is how the database and
    /// the log file are counted.
    #[test]
    fn a_single_file_measures_itself() {
        let root = temp_dir("file");
        let file = root.join("library.db");
        std::fs::write(&file, vec![0u8; 42]).unwrap();

        let usage = dir_usage(&file);
        assert_eq!(usage.files, 1);
        assert_eq!(usage.bytes, 42);

        std::fs::remove_dir_all(&root).ok();
    }

    /// The parts add up to the whole, and the cache is exactly its two
    /// tenants — the number the settings page offers to free.
    #[test]
    fn the_parts_add_up_to_the_total() {
        let report = StorageReport {
            config: DirUsage { bytes: 1, files: 1 },
            library_db: DirUsage { bytes: 2, files: 1 },
            backups: DirUsage { bytes: 4, files: 2 },
            media: DirUsage { bytes: 8, files: 3 },
            thumbs: DirUsage {
                bytes: 16,
                files: 4,
            },
            search_index: DirUsage {
                bytes: 32,
                files: 5,
            },
            logs: DirUsage {
                bytes: 64,
                files: 6,
            },
            incoming: DirUsage {
                bytes: 128,
                files: 7,
            },
            total: DirUsage {
                bytes: 255,
                files: 29,
            },
        };
        assert_eq!(
            report.library(),
            DirUsage {
                bytes: 10,
                files: 4
            }
        );
        assert_eq!(
            report.cache(),
            DirUsage {
                bytes: 48,
                files: 9
            }
        );
        assert_eq!(report.total.bytes, 255);
    }
}
