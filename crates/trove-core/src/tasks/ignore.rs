//! Git-style ignore files: what a scanned folder asks to be left out.
//!
//! A watch root — or a folder dropped onto the window — is somebody's
//! directory, and the way a directory says "not this" about its own contents is
//! the file Git already reads. So that is the file this scan reads: `.gitignore`
//! in the folder itself or in any folder above it *inside the scan*,
//! `.git/info/exclude` for the private rules a repository never commits, and
//! `.ignore` for what should be left out here and now (the ripgrep convention,
//! and the one that has the final say for a folder).
//!
//! The precedence is Git's, and honoring it is the whole point of borrowing the
//! format. These are the rules `git check-ignore` applies — measured, not
//! recalled:
//!
//! - rules are relative to the file that declares them, so a `*.log` in `sub/`
//!   binds `sub/` and everything below it, and nothing beside it;
//! - within one folder, `.git/info/exclude` is read before `.gitignore` (a
//!   committed rule overrules a private one) and `.ignore` last of all;
//! - the *deepest* file that speaks for a path decides it, so `sub/.gitignore`
//!   can overrule the root's; within one file the last matching line wins, so
//!   `!keep.log` written after `*.log` re-includes it;
//! - a folder that is left out takes its subtree with it: `build/` followed by
//!   `!build/keep.png` still leaves `keep.png` out, because no rule below a
//!   pruned directory is ever consulted.
//!
//! That last rule is why [`own_rules`] is gathered per directory rather than
//! once per scan, and why the two callers reach the same decision by different
//! routes. A scan that walks ([`crate::tasks::import::all_files`]) never enters
//! what it leaves out, so it carries the rules of the folders it came through
//! and asks the deepest first ([`is_left_out`]): one pass over a short list per
//! entry, and it learns what a folder declares from the listing it has already
//! read. The event path holds a bare path with no walk behind it ([`Ignores`]),
//! so it replays the descent from the watch root and caches what it reads; the
//! cache is dropped on the settings cadence, so an ignore file the user edits
//! takes effect without a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// A folder's own ignore files, in the order Git reads them — so the last one
/// that matches a path has the final say for that folder.
const OWN_IGNORE_FILES: [&str; 2] = [".gitignore", ".ignore"];

/// Where a Git repository keeps the excludes it does not commit. Not a name a
/// listing shows: a repository announces itself by holding `.git`.
const GIT_EXCLUDE: &str = ".git/info/exclude";

/// The rules a scan carries: the ignore files of the folders it has walked
/// through, outermost first, so the one declared by the folder it is standing in
/// comes last.
pub type Rules = Vec<Arc<Gitignore>>;

/// The rules `dir` declares about its own contents, or `None` when it declares
/// none. Everything is matched relative to `dir`, which is what keeps a rule in
/// a subdirectory inside that subdirectory.
///
/// `listed` is `dir`'s directory listing, for a caller that has already read it
/// — a full scan always has, and then an absent ignore file costs a string
/// comparison where probing costs an `open()`. Without a listing the names are
/// tried on disk.
///
/// A line that does not parse is dropped and the rest of the file still
/// applies; an unreadable file declares nothing. Neither is reported: a broken
/// line is Git's business to complain about, and a folder the user asked to
/// leave out needs no receipt.
pub fn own_rules(dir: &Path, listed: Option<&[&str]>) -> Option<Arc<Gitignore>> {
    let mut builder = GitignoreBuilder::new(dir);
    for path in declared(dir, listed) {
        builder.add(path);
    }
    match builder.build() {
        Ok(rules) if !rules.is_empty() => Some(Arc::new(rules)),
        _ => None,
    }
}

/// The ignore files `dir` holds, in read order.
fn declared(dir: &Path, listed: Option<&[&str]>) -> Vec<PathBuf> {
    let present = |name: &str| listed.is_none_or(|names| names.contains(&name));
    let mut paths = Vec::new();
    if present(".git") {
        let exclude = dir.join(GIT_EXCLUDE);
        if exclude.is_file() {
            paths.push(exclude);
        }
    }
    for name in OWN_IGNORE_FILES {
        if present(name) {
            paths.push(dir.join(name));
        }
    }
    paths
}

/// Whether the rules a scan carried into a folder leave `path` out: the watch
/// root's own file, then every folder between it and `path`, the one holding
/// `path` last.
/// The deepest file that speaks wins, and a re-include ends the search rather
/// than letting a shallower file overrule it.
///
/// Pruning is the caller's job, and a scan gets it for free: a folder left out
/// is never entered, so nothing below it reaches this function.
pub fn is_left_out(rules: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
    for rule in rules.iter().rev() {
        match rule.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// The same decision for a caller holding only a path: the descent is replayed
/// from the watch root, and the reads that cost anything are cached.
#[derive(Default)]
pub struct Ignores {
    /// Directory -> what it declares about its own contents (`None`: nothing).
    own: HashMap<PathBuf, Option<Arc<Gitignore>>>,
}

impl Ignores {
    /// Whether an ignore file between `root` and `path` leaves `path` out. A
    /// path outside `root` is not `root`'s to decide.
    ///
    /// The walk is top-down because Git's is: every directory on the way is
    /// asked in turn and the first one left out ends the question, since a rule
    /// below a pruned directory is never read. Within one directory its own
    /// file speaks last, so it overrules the ones above it.
    pub fn is_left_out(&mut self, root: &Path, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(root) else {
            return true;
        };
        // Every step from the root down to `path`, so each can be asked as what
        // it is: a directory walked through, or the leaf the caller named.
        let mut steps: Vec<PathBuf> = Vec::with_capacity(rel.components().count());
        let mut step = root.to_path_buf();
        for component in rel.components() {
            step = step.join(component);
            steps.push(step.clone());
        }
        for (depth, node) in steps.iter().enumerate() {
            let node_is_dir = depth + 1 < steps.len() || is_dir;
            // The directories whose rules reach `node`: every step above it and
            // the root itself, deepest first.
            let above = steps[..depth]
                .iter()
                .rev()
                .map(PathBuf::as_path)
                .chain([root]);
            for dir in above {
                let Some(rules) = self.own(dir) else {
                    continue;
                };
                match rules.matched(node, node_is_dir) {
                    Match::Ignore(_) => return true,
                    // Settled: this step is re-included, so nothing shallower
                    // gets a vote on it. Ask about what it contains instead.
                    Match::Whitelist(_) => break,
                    Match::None => {}
                }
            }
        }
        false
    }

    /// What `dir` declares, read from disk the first time it is asked.
    fn own(&mut self, dir: &Path) -> Option<&Arc<Gitignore>> {
        let entry = self
            .own
            .entry(dir.to_path_buf())
            .or_insert_with(|| own_rules(dir, None));
        entry.as_ref()
    }

    /// Forget what was read, so an edited ignore file is read again.
    pub fn clear(&mut self) {
        self.own.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "trove-ignore-{name}-{}-{}",
            std::process::id(),
            crate::model::new_id().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, name: &str, text: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The rules a scan carries into a folder: the root's first, the folder it
    /// has reached last.
    fn chain(root: &Path, walked: &[&Path]) -> Vec<Arc<Gitignore>> {
        std::iter::once(root)
            .chain(walked.iter().copied())
            .filter_map(|dir| own_rules(dir, None))
            .collect()
    }

    /// A rule declared in a folder binds that folder's tree — nothing beside it.
    #[test]
    fn a_rule_is_relative_to_the_folder_that_declared_it() {
        let root = temp_dir("scoped");
        let sub = root.join("sub");
        let other = root.join("other");
        write(&sub, ".gitignore", "*.log\n");

        let in_sub = chain(&root, &[&sub]);
        assert!(is_left_out(&in_sub, &sub.join("a.log"), false));
        assert!(
            is_left_out(&in_sub, &sub.join("deep/er/a.log"), false),
            "a rule stopped reaching down"
        );
        // A scan in the sibling carries no rules of `sub`'s: it never walked
        // through it.
        assert!(chain(&root, &[&other]).is_empty());
        assert!(!is_left_out(
            &chain(&root, &[&other]),
            &other.join("a.log"),
            false
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The closer file overrules the farther one, in both directions.
    #[test]
    fn the_deepest_file_that_speaks_for_a_path_decides_it() {
        let root = temp_dir("precedence");
        write(&root, ".gitignore", "*.log\n");
        let sub = root.join("sub");
        write(&sub, ".gitignore", "!keep.log\n");
        let in_sub = chain(&root, &[&sub]);

        assert!(
            !is_left_out(&in_sub, &sub.join("keep.log"), false),
            "sub/.gitignore could not re-include what the root left out"
        );
        assert!(is_left_out(&in_sub, &sub.join("drop.log"), false));
        // … and only inside `sub`: the root's own file still stands there.
        assert!(is_left_out(
            &chain(&root, &[]),
            &root.join("keep.log"),
            false
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Within one file the last match wins, so a re-include has to be written
    /// after the rule it undoes.
    #[test]
    fn the_last_matching_line_in_a_file_wins() {
        let root = temp_dir("order");
        write(&root, ".gitignore", "!keep.log\n*.log\n");
        assert!(
            is_left_out(&chain(&root, &[]), &root.join("keep.log"), false),
            "an earlier re-include overruled a later rule"
        );
        write(&root, ".gitignore", "*.log\n!keep.log\n");
        let rules = chain(&root, &[]);
        assert!(!is_left_out(&rules, &root.join("keep.log"), false));
        assert!(is_left_out(&rules, &root.join("drop.log"), false));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_trailing_slash_leaves_out_directories_only() {
        let root = temp_dir("only-dir");
        write(&root, ".gitignore", "build/\n");
        std::fs::create_dir_all(root.join("build")).unwrap();
        let rules = chain(&root, &[]);
        assert!(is_left_out(&rules, &root.join("build"), true));
        assert!(
            !is_left_out(&rules, &root.join("build"), false),
            "a directory rule matched a file of the same name"
        );
        assert!(!is_left_out(&rules, &root.join("build.png"), false));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_leading_slash_anchors_a_rule_to_its_own_folder() {
        let root = temp_dir("anchored");
        write(&root, ".gitignore", "/tmp\n");
        std::fs::create_dir_all(root.join("a")).unwrap();
        let rules = chain(&root, &[]);
        assert!(is_left_out(&rules, &root.join("tmp"), true));
        assert!(!is_left_out(&rules, &root.join("a/tmp"), true));

        std::fs::remove_dir_all(&root).ok();
    }

    /// All three files are read, and the order they are read in is the order of
    /// precedence Git uses within one folder.
    #[test]
    fn exclude_then_gitignore_then_ignore() {
        let root = temp_dir("exclude");
        write(&root.join(".git"), "info/exclude", "secret.png\nprivate\n");
        write(&root, ".gitignore", "!private\npublic\n");

        let rules = chain(&root, &[]);
        assert!(
            is_left_out(&rules, &root.join("secret.png"), false),
            ".git/info/exclude was not read"
        );
        assert!(
            !is_left_out(&rules, &root.join("private"), false),
            "the committed file could not overrule the private excludes"
        );
        assert!(is_left_out(&rules, &root.join("public"), false));

        // `.ignore` is read last, so it overrules both.
        write(&root, ".ignore", "!public\nkeep.jpg\n");
        let rules = chain(&root, &[]);
        assert!(!is_left_out(&rules, &root.join("public"), false));
        assert!(is_left_out(&rules, &root.join("keep.jpg"), false));

        std::fs::remove_dir_all(&root).ok();
    }

    /// A name the scan already has in hand needs no probe to rule out: the
    /// listing said so.
    #[test]
    fn a_listing_short_circuits_the_probe() {
        let root = temp_dir("listed");
        write(&root, ".gitignore", "*.log\n");
        assert!(own_rules(&root, Some(&["readme.txt"][..])).is_none());
        assert!(own_rules(&root, Some(&[".gitignore"][..])).is_some());
        // No listing to go by: the file is found the slow way.
        assert!(own_rules(&root, None).is_some());
        assert!(own_rules(&root.join("nothing-here"), None).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    /// The event path has one path at a time and no walk to lean on, so it
    /// replays the descent — which is what carries both halves of Git's rule:
    /// an ignored folder leaves out its whole subtree, and nothing below it can
    /// be re-included.
    #[test]
    fn an_ignored_directory_takes_its_contents_with_it() {
        let root = temp_dir("pruned");
        write(
            &root,
            ".gitignore",
            "node_modules/\n*.log\n!node_modules/keep.js\n",
        );
        write(&root.join("pkg/node_modules/left"), "a.js", "x");
        let deep = root.join("pkg/node_modules/left/a.js");
        write(&root.join("pkg"), "old.log", "x");
        write(&root.join("pkg"), "keep.png", "x");

        let mut ignores = Ignores::default();
        assert!(
            ignores.is_left_out(&root, &deep, false),
            "a file under an ignored directory was kept"
        );
        // A file rule reaches down as far as the folder that declared it.
        assert!(ignores.is_left_out(&root, &root.join("pkg/old.log"), false));
        assert!(!ignores.is_left_out(&root, &root.join("pkg/keep.png"), false));
        // The re-include below `node_modules/` never gets read, the way
        // `git check-ignore` treats it.
        assert!(ignores.is_left_out(&root, &root.join("node_modules/keep.js"), false));
        // Not this root's to decide.
        assert!(ignores.is_left_out(&root, &PathBuf::from("/somewhere/else"), false));

        std::fs::remove_dir_all(&root).ok();
    }

    /// A folder's rules are read once per cycle, not once per candidate: the
    /// kernel reports a copied folder file by file.
    #[test]
    fn reads_are_cached_until_cleared() {
        let root = temp_dir("cached");
        write(&root, ".gitignore", "*.log\n");
        let mut ignores = Ignores::default();
        assert!(ignores.is_left_out(&root, &root.join("a.log"), false));
        assert_eq!(ignores.own.len(), 1);
        assert!(ignores.is_left_out(&root, &root.join("b.log"), false));
        assert_eq!(ignores.own.len(), 1, "a second candidate read it again");

        // An edit is invisible until the cache is dropped …
        write(&root, ".gitignore", "");
        assert!(ignores.is_left_out(&root, &root.join("a.log"), false));
        // … and the watch loop drops it on the settings cadence.
        ignores.clear();
        assert!(!ignores.is_left_out(&root, &root.join("a.log"), false));

        std::fs::remove_dir_all(&root).ok();
    }
}
