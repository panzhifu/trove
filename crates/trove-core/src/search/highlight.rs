//! Where a search box's own words land in a string, so the interface can mark
//! them.
//!
//! The index answers *which* assets matched and never says which characters to
//! paint — and the answer is not in the index anyway: a term that got in
//! through a 2-gram, a pinyin syllable or a fuzzy edit has no single span in the
//! stored text. So this asks the narrower question that can be answered from
//! the row alone: where does the word the user typed occur in it, case
//! insensitively. A document found by `mao` matching 猫 is a real hit and stays
//! unmarked; a marked range is always one the user can read back.

use std::ops::Range;

use super::expression::{Expression, Target};

/// The words of a parsed query, kept with the surface each was qualified
/// against.
///
/// Negated atoms are left out: `-cat` excludes, and marking the word that got
/// an asset *dropped* would read as "this one matched here".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lexicon {
    entries: Vec<(Target, String)>,
}

impl Lexicon {
    pub fn from_expression(expr: &Expression) -> Self {
        Self {
            entries: expr
                .groups
                .iter()
                .flat_map(|group| group.atoms.iter())
                .filter(|atom| !atom.negate)
                .map(|atom| (atom.target, atom.text.clone()))
                .collect(),
        }
    }

    /// The words of whatever is in the box. Parsing is the cheap leg: the same
    /// grammar the ranking runs on is what marks the hits, so the two cannot
    /// disagree about what was asked for.
    pub fn from_query(query: &str) -> Self {
        Self::from_expression(&super::expression::parse(query).into_expression())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Byte ranges of `text` holding one of the words a query may match in any
    /// of `targets`, merged so that overlapping hits mark one span.
    ///
    /// The caller names the surfaces it is rendering: a grid row shows the
    /// title, so `desc:胶片` and `tag:胶片` must not light up in it.
    pub fn ranges_in(&self, targets: &[Target], text: &str) -> Vec<Range<usize>> {
        if self.entries.is_empty() || text.is_empty() {
            return Vec::new();
        }
        // One lower-cased copy serves every term, and its offsets are the ones
        // returned. Case-folding changes byte length for a handful of
        // characters (`İ` folds to two), and then an offset would name the
        // wrong bytes — mark nothing rather than mark the wrong thing.
        let lower = text.to_lowercase();
        if lower.len() != text.len() {
            return Vec::new();
        }
        let mut hits: Vec<Range<usize>> = Vec::new();
        for (target, term) in &self.entries {
            if !targets.contains(target) {
                continue;
            }
            hits.extend(occurrences(&lower, term));
        }
        merge(hits)
    }
}

/// Every non-overlapping occurrence of `needle` in `hay`, as byte ranges.
fn occurrences(hay: &str, needle: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    // `base` only ever advances by whole matches, so it stays on a character
    // boundary and the slice below cannot panic.
    let mut base = 0;
    while let Some(offset) = hay[base..].find(needle) {
        let start = base + offset;
        base = start + needle.len();
        out.push(start..base);
    }
    out
}

/// Sort and coalesce touching or nested ranges into the spans to mark.
fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::expression::Target;

    /// The surfaces a listing row shows: one string, which is the title when
    /// the asset has one and the file name when it does not.
    const ROW: &[Target] = &[Target::All, Target::Name, Target::Title];

    fn marks(query: &str, text: &str) -> Vec<Range<usize>> {
        Lexicon::from_query(query).ranges_in(ROW, text)
    }

    #[test]
    fn a_word_is_marked_every_time_it_occurs() {
        assert_eq!(marks("sunset", "sunset at sunset.png"), vec![0..6, 10..16]);
    }

    #[test]
    fn matching_ignores_case_but_the_ranges_are_the_originals() {
        assert_eq!(marks("SUNSET", "Sunset reel.MP4"), vec![0..6]);
    }

    #[test]
    fn a_cjk_word_is_marked_where_it_sits() {
        // Byte offsets, not character indices: 猫 is three bytes in.
        assert_eq!(marks("猫", "三只猫和一只猫"), vec![6..9, 18..21]);
    }

    #[test]
    fn a_pinyin_hit_stays_unmarked() {
        // The ranking finds 猫 through `mao`; this module does not claim to
        // know that it did, so it marks nothing rather than an unrelated span.
        assert!(marks("mao", "猫.png").is_empty());
    }

    #[test]
    fn a_negated_word_marks_nothing() {
        // `sunset` still marks its own bytes; the word that got an asset
        // *dropped* must not light up as if it were why this one is here.
        assert_eq!(marks("sunset -beach", "sunset beach.png"), vec![0..6]);
    }

    #[test]
    fn a_qualified_word_marks_only_its_own_surface() {
        let lexicon = Lexicon::from_query("desc:胶片");
        assert!(lexicon.ranges_in(ROW, "胶片.png").is_empty());
        assert_eq!(
            lexicon.ranges_in(&[Target::Description], "三段胶片"),
            vec![6..12]
        );
    }

    #[test]
    fn overlapping_words_mark_one_span() {
        // `cat` covers 0..3 and `catalog` 0..7; two nested ranges would ask the
        // renderer to style the same bytes twice.
        assert_eq!(marks("cat catalog", "catalogue"), vec![0..7]);
    }

    #[test]
    fn a_box_of_only_filters_marks_nothing() {
        assert!(marks("ext:png rating:3", "shot.png").is_empty());
    }

    #[test]
    fn a_phrase_marks_the_whole_phrase() {
        assert_eq!(marks("\"sunset shot\"", "a sunset shot.png"), vec![2..13]);
    }
}
