//! Query construction: every way a user's text becomes a Tantivy query —
//! plain terms, field-qualified ones, grams for substrings, pinyin, phrases
//! and AI search plans.

use tantivy::Term;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};

use super::expression;
use super::index::TextIndex;
use super::tokenizer::jieba;

impl TextIndex {
    /// The 2/3-gram alternative for a term: an OR over the gram fields for
    /// short terms, an AND of per-gram ORs for longer ones (a necessary,
    /// well-ranked approximation of the substring). Returned as ONE query
    /// so it competes as a single `should` alternative next to the word /
    /// fuzzy / pinyin paths instead of constraining them.
    ///
    /// The grams keep spaces (`NgramTokenizer` runs over characters), which is
    /// what makes a quoted phrase a real substring match rather than a mere
    /// AND of its words.
    fn gram_query(&self, term: &str, target: expression::Target) -> Box<dyn Query> {
        let tris = self.f.tris_for(target);
        let n = term.chars().count();
        let gram_shoulds = |gram: &str| {
            tris.iter()
                .map(|f| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(*f, gram),
                            IndexRecordOption::WithFreqs,
                        )) as Box<dyn Query>,
                    )
                })
                .collect::<Vec<_>>()
        };
        if n == 2 || n == 3 {
            Box::new(BooleanQuery::new(gram_shoulds(term)))
        } else {
            let chars: Vec<char> = term.chars().collect();
            let mut must: Vec<(Occur, Box<dyn Query>)> = chars
                .windows(3)
                .map(|g| {
                    let gram: String = g.iter().collect();
                    (
                        Occur::Must,
                        Box::new(BooleanQuery::new(gram_shoulds(&gram))) as Box<dyn Query>,
                    )
                })
                .collect();
            if must.is_empty() {
                must.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.f.abbr, "\u{0}no-match"),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                ));
            }
            Box::new(BooleanQuery::new(must))
        }
    }

    /// One user term → its ranked alternatives, across every surface.
    fn term_query(&self, term: &str) -> Box<dyn Query> {
        self.term_query_on(term, expression::Target::All)
    }

    /// One user term, restricted to `target`'s surfaces.
    ///
    /// A qualified term keeps the word, fuzzy, prefix and gram paths. Pinyin
    /// and abbreviation matching is now field-scoped: `tag:mao` matches pinyin
    /// from tags only, not from the file name or other surfaces.
    fn term_query_on(&self, term: &str, target: expression::Target) -> Box<dyn Query> {
        let lower = term.to_lowercase();
        let n = term.chars().count();
        let words = self.f.words_for(target);
        let global = target == expression::Target::All;

        let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let push_exact = |shoulds: &mut Vec<(Occur, Box<dyn Query>)>, f: Field, boost: f32| {
            shoulds.push((
                Occur::Should,
                Box::new(tantivy::query::BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(f, &lower),
                        IndexRecordOption::WithFreqs,
                    )),
                    boost,
                )) as Box<dyn Query>,
            ));
        };

        if term.chars().all(|c| c.is_ascii_alphanumeric()) {
            for f in &words {
                push_exact(&mut shoulds, *f, 3.0);
            }
            // Typo tolerance: short stems only get one edit to stay precise.
            if n >= 4 {
                let distance = if n >= 8 { 2 } else { 1 };
                for f in &words {
                    shoulds.push((
                        Occur::Should,
                        Box::new(tantivy::query::BoostQuery::new(
                            Box::new(FuzzyTermQuery::new(
                                Term::from_field_text(*f, &lower),
                                distance,
                                true,
                            )),
                            1.0,
                        )) as Box<dyn Query>,
                    ));
                }
            }
            // Word-start prefix ("sun" → "sunset") and pinyin lookups.
            if n >= 2 {
                for f in &words {
                    shoulds.push((
                        Occur::Should,
                        Box::new(FuzzyTermQuery::new_prefix(
                            Term::from_field_text(*f, &lower),
                            0,
                            false,
                        )) as Box<dyn Query>,
                    ));
                }
            }
            if global {
                // Unqualified: search the global pinyin/abbr fields (all surfaces)
                shoulds.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new_prefix(
                        Term::from_field_text(self.f.pinyin, &lower),
                        0,
                        false,
                    )) as Box<dyn Query>,
                ));
                shoulds.push((
                    Occur::Should,
                    Box::new(FuzzyTermQuery::new_prefix(
                        Term::from_field_text(self.f.abbr, &lower),
                        0,
                        false,
                    )) as Box<dyn Query>,
                ));
            } else if n >= 2 {
                // Field-qualified: search per-surface pinyin if available
                let surface_pinyin = self.f.pinyin_for(target);
                if let Some(py_field) = surface_pinyin {
                    shoulds.push((
                        Occur::Should,
                        Box::new(FuzzyTermQuery::new_prefix(
                            Term::from_field_text(py_field, &lower),
                            0,
                            false,
                        )) as Box<dyn Query>,
                    ));
                }
            }
            // Grams keep infix substrings findable (`ower` → `flower`).
            if n >= 2 {
                shoulds.push((Occur::Should, self.gram_query(&lower, target)));
            }
        } else {
            // CJK / mixed: whole-term word matches plus 2–3 gram
            // substrings; longer terms AND their 3-grams.
            for f in &words {
                push_exact(&mut shoulds, *f, 3.0);
            }
            shoulds.push((Occur::Should, self.gram_query(&lower, target)));
        }
        Box::new(BooleanQuery::new(shoulds))
    }

    /// All terms ANDed; an empty query matches nothing via a sentinel term.
    pub(super) fn build_query(&self, text: &str) -> Box<dyn Query> {
        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        let mut must: Vec<(Occur, Box<dyn Query>)> = cleaned
            .split_whitespace()
            .map(|term| (Occur::Must, self.term_query(term)))
            .collect();
        if must.is_empty() {
            must.push((Occur::Must, self.no_match()));
        }
        Box::new(BooleanQuery::new(must))
    }

    /// Rank the documents an [`expression::Expression`] describes.
    ///
    /// Groups OR with each other; inside a group every positive atom is a
    /// `must` and every `-` atom a `must_not`.
    ///
    /// A group holding only negations matches **nothing**. It could instead be
    /// read as "everything but", and that reading is what a search engine with
    /// a query language would do — but it would also mean a mistyped leading
    /// dash widens a query into the whole library. Refusing to widen is the
    /// contract the box has always had (`store::tests::
    /// search_syntax_characters_stay_literal`), so exclusion needs something
    /// positive to exclude from.
    ///
    /// One group of positive atoms builds the same object
    /// [`build_query`](Self::build_query) does, which is what keeps an
    /// unremarkable query behaving exactly as it always did.
    pub(super) fn build_expression_query(&self, expr: &expression::Expression) -> Box<dyn Query> {
        let groups = expr
            .groups
            .iter()
            .map(|group| self.build_group_query(group))
            .collect::<Vec<_>>();
        match groups.len() {
            0 => self.no_match(),
            // One disjunct needs no OR around it: the group query *is* the
            // query, which is what makes a plain query byte-identical to the
            // path it replaced.
            1 => groups.into_iter().next().expect("one group"),
            _ => Box::new(BooleanQuery::new(
                groups
                    .into_iter()
                    .map(|group| (Occur::Should, group))
                    .collect(),
            )),
        }
    }

    fn build_group_query(&self, group: &expression::Group) -> Box<dyn Query> {
        if group.atoms.iter().all(|atom| atom.negate) {
            return self.no_match();
        }
        let clauses = group
            .atoms
            .iter()
            .map(|atom| {
                let occur = if atom.negate {
                    Occur::MustNot
                } else {
                    Occur::Must
                };
                // Quoted phrases use positional matching; unquoted terms use
                // the regular ranked-alternatives path.
                let query = if atom.quoted && atom.text.contains(' ') {
                    self.phrase_query_on(&atom.text, atom.target)
                } else {
                    self.term_query_on(&atom.text, atom.target)
                };
                (occur, query)
            })
            .collect();
        Box::new(BooleanQuery::new(clauses))
    }

    /// Build a positional phrase query for a quoted span. The phrase is
    /// tokenized with jieba, and the resulting terms must appear in sequence
    /// in the indexed text. Falls back to n-gram matching if the phrase
    /// tokenizes to a single term (no positional information to enforce).
    fn phrase_query_on(&self, phrase: &str, target: expression::Target) -> Box<dyn Query> {
        let lower = phrase.to_lowercase();
        let words = self.f.words_for(target);

        // Tokenize the phrase with jieba to get the sequence of terms
        let jieba = jieba();
        let tokens: Vec<String> = jieba
            .cut(&lower, true)
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .collect();

        // If the phrase tokenizes to a single term, there's no positional
        // constraint to enforce — fall back to the regular term query.
        if tokens.len() <= 1 {
            return self.term_query_on(&lower, target);
        }

        // Build a PhraseQuery for each surface field, OR them together.
        // PhraseQuery requires terms to appear in sequence with correct positions.
        let mut phrase_shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for field in &words {
            let terms: Vec<Term> = tokens
                .iter()
                .map(|token| Term::from_field_text(*field, token))
                .collect();
            phrase_shoulds.push((Occur::Should, Box::new(PhraseQuery::new(terms))));
        }

        // Also add n-gram matching as a fallback for partial matches
        phrase_shoulds.push((Occur::Should, self.gram_query(&lower, target)));

        Box::new(BooleanQuery::new(phrase_shoulds))
    }

    /// A term that can never have been indexed, standing in for "no results".
    fn no_match(&self) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.f.abbr, "\u{0}no-match"),
            IndexRecordOption::Basic,
        ))
    }

    /// Build a Tantivy query from an AI search plan. Keywords are ANDed
    /// (Must), synonyms are ORed as optional boosts (Should), and
    /// exclusions are negated (MustNot).
    pub(super) fn build_plan_query(
        &self,
        plan: &crate::ai::search_planner::AiSearchPlan,
    ) -> Box<dyn Query> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // Keywords: all must match (AND).
        for keyword in &plan.keywords {
            clauses.push((Occur::Must, self.term_query(keyword)));
        }

        // Synonyms: optional boost (OR) — only meaningful when at least one
        // synonym matches.
        for synonym in &plan.synonyms {
            clauses.push((Occur::Should, self.term_query(synonym)));
        }

        // A plan can arrive with nothing positive in it: the planner rejects a
        // plan only when keywords, synonyms, exclusions, filters *and* sort are
        // all empty, so one exclusion and no keywords is a valid plan.
        //
        // Measured on tantivy 0.26, a `BooleanQuery` whose only clauses are
        // `MustNot` answers **nothing** — not "every document minus the
        // matches", which is what the Lucene-flavoured reading of that shape
        // predicts. So this guard is not patching a live wrong answer; it is
        // refusing to depend on that fact, and it makes this path and
        // `build_group_query` reach the same verdict for the same shape by the
        // same words rather than by two engines' semantics.
        if clauses.is_empty() {
            return self.no_match();
        }

        // Exclusions: must NOT match.
        for exclusion in &plan.exclusions {
            clauses.push((Occur::MustNot, self.term_query(exclusion)));
        }

        Box::new(BooleanQuery::new(clauses))
    }
}
