//! Image sequences: telling a run of frames from a pile of similarly named files.
//!
//! A render drops `shot0001.exr … shot0150.exr` and a folder of screenshots
//! drops `shot1.png`, `shot2.png`, `shot9.png` — and only the first of those is
//! an animation. The rules below are the ones that keep the two apart, and they
//! are deliberately strict in the places where being clever would be wrong:
//!
//! * **Three frames minimum.** Two numbered files are a pair, not a run.
//! * **No gaps.** `0001, 0002, 0004` is two runs of two and one, and neither is
//!   long enough to be worth a card. A missing frame is a render that failed,
//!   and grouping a broken run hides the fact.
//! * **Padding is part of the identity.** `001..010` and `1..10` are different
//!   conventions and never merge, while an unpadded tail (`01..09` then
//!   `10..12`) does join the padded run it belongs to — because that is how a
//!   shell loop writes a sequence when someone forgets to pad the tens.
//! * **Same directory, same prefix, same extension.** A cross-directory merge
//!   would turn two shots into one long clip.
//!
//! Nothing here touches the database or the files' contents: dimension agreement
//! is checked where the rows are, by the caller that has them.

use std::path::{Path, PathBuf};

use crate::model::AssetKind;

/// The fewest frames that make a run. Two files named 1 and 2 are a pair.
pub const MIN_FRAMES: usize = 3;

/// How a frame's number is written in its own name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NumberStyle {
    /// `shot0001` — digits at the end of the stem.
    Trailing,
    /// `shot (1)` — the copy-the-filename convention, digits in parentheses.
    Parens,
}

/// One frame of a detected run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub path: PathBuf,
    /// The number written in the name, which may start anywhere (`0001` → 1).
    pub number: u64,
}

/// A detected run, in ascending frame order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    /// The shared stem, with the number and the extension taken off.
    pub prefix: String,
    /// Lower-case extension, no dot.
    pub extension: String,
    pub style: NumberStyle,
    pub frames: Vec<Frame>,
}

impl Sequence {
    /// How the run reads in a card or a dialog: `shot0001~0150`, the way a
    /// compositor writes it, in the width the file names themselves use.
    pub fn display_name(&self) -> String {
        let (Some(first), Some(last)) = (self.frames.first(), self.frames.last()) else {
            return self.prefix.clone();
        };
        match self.style {
            NumberStyle::Trailing => format!(
                "{}{:0w$}~{:0w$}",
                self.prefix,
                first.number,
                last.number,
                w = written_width(&first.path)
            ),
            NumberStyle::Parens => format!("{}({})~({})", self.prefix, first.number, last.number),
        }
    }
}

/// How many digits the number in this file's name was written with — `0001` is
/// four, even though its value is one digit wide.
fn written_width(path: &Path) -> usize {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| {
            stem.chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .count()
        })
        .unwrap_or(0)
}

/// The number a frame name ends with, for ordering a run the user picked by
/// hand. `None` when the name carries no trailing number at all.
pub fn frame_number(path: &Path) -> Option<u64> {
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    parse_number(stem).map(|(_, number, _, _)| number)
}

/// What one file name contributes to a grouping decision.
struct Parsed {
    number: u64,
    /// Digits as written, for the padding partition.
    digits: usize,
    /// Whether the written number starts with a zero, i.e. was padded on
    /// purpose rather than just happening to be short.
    padded: bool,
    path: PathBuf,
}

/// Group `paths` into runs of numbered image frames.
///
/// Takes paths, not assets, because the two callers have different things
/// available: the import walk has a sorted list of files and no rows yet, and
/// the "create a sequence from this selection" gesture has rows. Both can hand
/// over paths.
pub fn detect<I, P>(paths: I) -> Vec<Sequence>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    // (directory, prefix, extension, style) → frames. Case-insensitive on the
    // three text parts, because a run that changes case mid-way is the same run
    // and a file system may not care.
    let mut groups: std::collections::BTreeMap<(String, String, String, NumberStyle), Vec<Parsed>> =
        std::collections::BTreeMap::new();

    for path in paths {
        let path = path.as_ref();
        let Some(extension) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        else {
            continue;
        };
        // Only what this app classifies as a picture. Reusing the classifier is
        // the point: a list here would drift from the one that decides whether
        // the frame has a thumbnail at all.
        if crate::media::probe::probe(&extension).kind != AssetKind::Image {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((prefix, number, digits, style)) = parse_number(stem) else {
            continue;
        };
        let directory = path
            .parent()
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let parsed = Parsed {
            number,
            digits,
            padded: digits > 1 && stem[stem.len() - digits..].starts_with('0'),
            path: path.to_path_buf(),
        };
        groups
            .entry((directory, prefix.to_lowercase(), extension, style))
            .or_default()
            .push(parsed);
    }

    let mut runs = Vec::new();
    for ((_, prefix, extension, style), frames) in groups {
        for run in partition(frames) {
            if run.len() < MIN_FRAMES {
                continue;
            }
            let frames = run
                .into_iter()
                .map(|parsed| Frame {
                    path: parsed.path,
                    number: parsed.number,
                })
                .collect::<Vec<_>>();
            runs.push(Sequence {
                // Cloned, not moved: one prefix group can yield several runs
                // once a gap splits it.
                prefix: prefix.clone(),
                extension: extension.clone(),
                style,
                frames,
            });
        }
    }
    runs.sort_by(|a, b| {
        a.frames
            .first()
            .map(|f| f.path.as_path())
            .cmp(&b.frames.first().map(|f| f.path.as_path()))
    });
    runs
}

/// Split one prefix group into runs, honouring the padding partitions and the
/// zero-gap rule.
fn partition(mut frames: Vec<Parsed>) -> Vec<Vec<Parsed>> {
    // Which widths were written on purpose. An unpadded frame joins a padded
    // partition only when its own digit count is exactly that width.
    let widths: Vec<usize> = {
        let mut seen: Vec<usize> = frames
            .iter()
            .filter(|f| f.padded)
            .map(|f| f.digits)
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    };
    let mut buckets: std::collections::BTreeMap<usize, Vec<Parsed>> =
        std::collections::BTreeMap::new();
    for frame in frames.drain(..) {
        let key = if frame.padded || widths.contains(&frame.digits) {
            frame.digits
        } else {
            0
        };
        buckets.entry(key).or_default().push(frame);
    }

    let mut runs = Vec::new();
    for (_, mut bucket) in buckets {
        bucket.sort_by_key(|frame| frame.number);
        let mut current: Vec<Parsed> = Vec::new();
        for frame in bucket {
            // Zero tolerance: a jump closes the run and starts a new one.
            if let Some(last) = current.last()
                && frame.number != last.number + 1
            {
                runs.push(std::mem::take(&mut current));
            }
            current.push(frame);
        }
        if !current.is_empty() {
            runs.push(current);
        }
    }
    runs
}

/// The number at the end of a stem, as `(stem without it, value, digits written,
/// style)`.
///
/// Parentheses are tried first: `shot (1)` ends in a digit either way, and the
/// trailing-number rule would read it as a frame of a run called `shot (`.
fn parse_number(stem: &str) -> Option<(String, u64, usize, NumberStyle)> {
    let chars: Vec<char> = stem.chars().collect();
    let take = |digits: &[char]| -> Option<(u64, usize)> {
        let text: String = digits.iter().collect();
        let value = text.parse::<u64>().ok()?;
        Some((value, text.len()))
    };

    if let Some(open) = chars.iter().rposition(|c| *c == '(')
        && chars.last() == Some(&')')
    {
        let digits = &chars[open + 1..chars.len() - 1];
        if !digits.is_empty()
            && digits.iter().all(|c| c.is_ascii_digit())
            && let Some((value, len)) = take(digits)
        {
            let prefix: String = chars[..open].iter().collect();
            return Some((prefix, value, len, NumberStyle::Parens));
        }
    }

    let end = chars.len();
    let start = chars
        .iter()
        .rposition(|c| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    if start == end {
        return None;
    }
    let digits = &chars[start..end];
    let (value, len) = take(digits)?;
    let prefix: String = chars[..start].iter().collect();
    // A run needs something to be a run *of*: `0001.png` alone is a file with a
    // name, not frame one of an empty prefix.
    if prefix.is_empty() {
        return None;
    }
    Some((prefix, value, len, NumberStyle::Trailing))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape every test wants: frame names under one directory.
    fn frames(names: impl IntoIterator<Item = String>) -> Vec<PathBuf> {
        names
            .into_iter()
            .map(|name| PathBuf::from("/renders").join(name))
            .collect()
    }

    fn range(from: u64, to: u64, pattern: &str) -> Vec<String> {
        (from..=to)
            .map(|i| pattern.replace('#', &i.to_string()))
            .collect()
    }

    fn numbers(sequence: &Sequence) -> Vec<u64> {
        sequence.frames.iter().map(|f| f.number).collect()
    }

    #[test]
    fn a_padded_render_is_one_run() {
        let names: Vec<String> = (1..=10).map(|i| format!("shot{i:04}.exr")).collect();
        let runs = detect(frames(names));
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(numbers(&runs[0]), (1..=10).collect::<Vec<_>>());
        assert_eq!(runs[0].extension, "exr");
        assert_eq!(runs[0].prefix, "shot");
        assert_eq!(runs[0].style, NumberStyle::Trailing);
    }

    /// The shell-loop case: the first nine frames were padded to two digits and
    /// the tens could not be, so the run changes written width mid-way and is
    /// still one run.
    #[test]
    fn an_unpadded_tail_joins_the_padded_run() {
        let names: Vec<String> = (1..=12).map(|i| format!("f{i:0>2}.png")).collect();
        let runs = detect(frames(names));
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(numbers(&runs[0]), (1..=12).collect::<Vec<_>>());
    }

    /// The two conventions never merge: `001..010` is a padded partition of width
    /// 3, and `1..10` has no padded bucket to join.
    #[test]
    fn padded_and_unpadded_are_different_runs() {
        let mut names: Vec<String> = (1..=5).map(|i| format!("a{i:03}.png")).collect();
        names.extend((1..=5).map(|i| format!("a{i}.png")));
        let runs = detect(frames(names));
        assert_eq!(runs.len(), 2, "{runs:?}");
        for run in &runs {
            assert_eq!(numbers(run), vec![1, 2, 3, 4, 5]);
        }
        assert_ne!(runs[0].frames[0].path, runs[1].frames[0].path);
    }

    /// Zero tolerance. A dropped frame is a render that failed, and a run that
    /// spans it would play as if nothing happened.
    #[test]
    fn a_gap_ends_the_run() {
        let names = ["b0001.png", "b0002.png", "b0004.png", "b0005.png"];
        let runs = detect(names.iter().map(Path::new));
        assert!(
            runs.is_empty(),
            "two runs of two are neither long enough: {runs:?}"
        );
    }

    #[test]
    fn two_files_are_not_a_sequence() {
        assert!(detect(frames(range(1, 2, "c###.png"))).is_empty());
    }

    #[test]
    fn different_directories_never_merge() {
        assert!(
            detect(["/a/d0001.png", "/b/d0001.png"]).is_empty(),
            "one frame per directory is not a run"
        );
    }

    #[test]
    fn parens_style_is_its_own_convention() {
        let names: Vec<String> = (1..=4).map(|i| format!("take ({i}).jpg")).collect();
        let runs = detect(frames(names));
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].style, NumberStyle::Parens);
        assert_eq!(runs[0].prefix, "take ");
        assert_eq!(numbers(&runs[0]), vec![1, 2, 3, 4]);
    }

    /// A numbered `.txt` is not a frame: the extension gate is the same
    /// classifier that decides whether the file has a thumbnail, so the two
    /// cannot disagree about what a sequence is made of.
    #[test]
    fn non_images_are_not_frames() {
        let names: Vec<String> = (1..=6).map(|i| format!("notes{i}.txt")).collect();
        assert!(detect(frames(names)).is_empty());
    }

    /// A bare number is a name, not a frame of something.
    #[test]
    fn a_stem_that_is_only_a_number_is_not_a_frame() {
        assert!(detect(frames(range(1, 6, "#.png"))).is_empty());
    }

    /// Mixed extensions are two runs: a `.png` pass and its `.jpg` proxy do not
    /// interleave into one animation.
    #[test]
    fn extensions_separate_runs() {
        let mut names: Vec<String> = (1..=4).map(|i| format!("m{i}.png")).collect();
        names.extend((1..=4).map(|i| format!("m{i}.jpg")));
        let runs = detect(frames(names));
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].extension, "jpg");
        assert_eq!(runs[1].extension, "png");
    }

    #[test]
    fn the_display_name_keeps_the_written_width() {
        let names: Vec<String> = (1..=150).map(|i| format!("shot{i:04}.exr")).collect();
        let runs = detect(frames(names));
        assert_eq!(runs[0].display_name(), "shot0001~0150");
    }
}
