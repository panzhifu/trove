//! Watched-folder scanning: find files that appeared under the configured
//! watch roots so they can be imported automatically.
//!
//! The scanner is a plain filesystem walk driven by a periodic timer in the
//! app layer — no inotify/kqueue dependency. `new_files` is pure so it can be
//! unit-tested; the caller owns the "already seen" set (in-memory: a restart
//! re-baselines instead of replaying the whole folder into the library).

// Wired into the app loop by the folder-watching feature; scan helpers are
// exercised by unit tests in the meantime.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// How deep below a watch root the walk descends. Watch roots are user
/// folders; going deeper than this is more likely to crawl something huge
/// than to find real assets.
const MAX_DEPTH: usize = 6;

/// Files that appeared under `roots` but are not in `seen`. On first contact
/// with a root the caller should seed `seen` with [`all_files`] instead, so
/// attaching a watch does not retro-import the whole folder.
pub fn new_files(roots: &[PathBuf], seen: &HashSet<PathBuf>) -> Vec<PathBuf> {
    all_files(roots)
        .into_iter()
        .filter(|p| !seen.contains(p))
        .collect()
}

/// Every regular file below `roots`, skipping hidden entries (dot files and
/// dot directories) at any depth.
pub fn all_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        walk(root, 0, &mut out);
    }
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk(&path, depth + 1, out),
            Ok(ft) if ft.is_file() => out.push(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_files_skips_hidden_and_respects_depth() {
        let root = std::env::temp_dir().join(format!("trove-watcher-test-{}", std::process::id()));
        let sub = root.join("nested");
        let deep = sub.join("a").join("b").join("c").join("d").join("e").join("f");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("top.png"), b"x").unwrap();
        std::fs::write(root.join(".hidden.png"), b"x").unwrap();
        std::fs::write(sub.join("inner.jpg"), b"x").unwrap();
        std::fs::write(deep.join("too-deep.png"), b"x").unwrap();

        let files = all_files(&[root.clone()]);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"top.png".to_string()));
        assert!(names.contains(&"inner.jpg".to_string()));
        assert!(!names.iter().any(|n| n.starts_with('.')));
        assert!(!names.contains(&"too-deep.png".to_string()));

        // New files are the delta against the seen set.
        let mut seen: HashSet<PathBuf> = files.iter().cloned().collect();
        let fresh = root.join("later.gif");
        std::fs::write(&fresh, b"x").unwrap();
        let delta = new_files(&[root.clone()], &seen);
        assert_eq!(delta, vec![fresh.clone()]);
        seen.insert(fresh);
        assert!(new_files(&[root.clone()], &seen).is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
