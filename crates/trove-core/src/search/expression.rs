//! The search box's expression grammar: a small parser that turns what the
//! user typed into a structured query the index and SQL layers can both act
//! on.
//!
//! # Grammar
//!
//! ```text
//! expr    := group ( '|' group )*         // '|' is OR, the loosest binding
//! group   := atom+                        // space is AND
//! atom    := '-'? ( field ':' value | term )
//! value   := WORD | '"' PHRASE '"'
//! term    := WORD | '"' PHRASE '"'
//! ```
//!
//! So `猫 狗 | 鸟` is `(猫 AND 狗) OR 鸟`, and `-猫 狗` is 狗 without 猫. A
//! quoted span is one atom even when it holds spaces or a `|`, so `"a | b"`
//! searches for that literal text rather than OR-ing anything.
//!
//! # What a bare term means
//!
//! Unchanged from before this module existed: every whitespace-separated term
//! is looked up across all four indexed surfaces with the word / fuzzy /
//! substring / pinyin alternatives competing by score (see
//! [`super::TextIndex`]). A plain query therefore has to produce the same
//! query object the old splitter did; [`Expression::is_plain`] says when that
//! is the case, and `trove-app`'s browse tests pin the equivalence.
//!
//! # Field qualifiers
//!
//! A text qualifier narrows *where* the term matches rather than moving the
//! condition into SQL, because the value is free text and the interesting
//! behaviour (substrings, typo tolerance) lives in the index:
//!
//! | qualifier | aliases | searched surface |
//! |---|---|---|
//! | `name:` | `filename:`, `file:` | the file name |
//! | `title:` | | the asset title |
//! | `desc:` | `description:` | the description |
//! | `tag:` | `tags:` | the tag names |
//!
//! Pinyin and abbreviation are indexed over all four surfaces concatenated
//! (see `TextIndex::index_asset`), so a *qualified* term deliberately loses
//! them: matching `tag:mao` against a pinyin that came from the file name
//! would be a wrong answer with no visible cause.
//!
//! The remaining qualifiers are structured and go to SQL, since they compare
//! against columns rather than text:
//!
//! | qualifier | aliases | meaning |
//! |---|---|---|
//! | `ext:` | `format:` | extension, case-insensitive; repeats OR |
//! | `kind:` | `type:` | asset kind |
//! | `path:` | `folder:` | source-path prefix |
//! | `rating:` | `stars:` | at least this many stars |
//! | `fav:` | `favorite:` | `yes` / `no` |
//!
//! `-ext:png` and friends negate. An unqualified `foo:bar` whose field is
//! unknown to us stays one search term, so a file genuinely named `arc:1` is
//! still findable.

use std::fmt;

use crate::model::{AssetKind, QueryCondition};

/// Where a term is allowed to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Any indexed surface, plus the pinyin and abbreviation fields.
    All,
    Name,
    Title,
    Description,
    Tags,
}

/// One search term: a span of text that must (or must not) match one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atom {
    pub target: Target,
    /// Leading `-`: this atom excludes rather than requires.
    pub negate: bool,
    /// The text, lower-cased. A quoted phrase keeps its inner spaces, so the
    /// index sees one term rather than several.
    pub text: String,
    /// Came from a `"..."` span.
    pub quoted: bool,
}

impl Atom {
    fn is_positive(&self) -> bool {
        !self.negate
    }
}

/// A conjunctive group: every non-negated atom must match and none of the
/// negated ones may. Groups are OR-ed with each other.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Group {
    pub atoms: Vec<Atom>,
}

impl Group {
    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }
}

// A structured condition on database columns is not defined here: it lives
// with the rest of the query vocabulary in [`crate::model::QueryCondition`],
// because the listing paths apply it, not this module.

/// A parsed search box.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Expression {
    /// OR-ed groups. Empty means the box held no text term, which is not the
    /// same as an unfiltered browse — callers enter the search path only when
    /// this is non-empty or [`Self::filters`] is.
    pub groups: Vec<Group>,
    pub filters: Vec<QueryCondition>,
}

/// What the parser could not make sense of. Carried alongside the result:
/// half a query the user can see beats an error dialog, so the caller searches
/// with what parsed and reports the rest.
///
/// The payload is the offending *span*, never prose: the interface speaks nine
/// languages, so the sentence is the caller's locale key and this is the data
/// that fills it in. [`Display`] stays English, for logs and the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    /// The field name, or the fragment itself, depending on the kind.
    pub span: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxErrorKind {
    /// A `"` with no closing partner; the remainder was read as one phrase.
    UnclosedQuote,
    /// `field:` with nothing after it.
    EmptyValue,
    /// `kind:` / `rating:` / `fav:` with an unparseable value; atom dropped.
    BadValue,
    /// A `:` with no field name in front of it.
    MissingField,
}

impl SyntaxError {
    fn new(kind: SyntaxErrorKind, span: impl Into<String>) -> Self {
        Self {
            kind,
            span: span.into(),
        }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            SyntaxErrorKind::UnclosedQuote => write!(f, "unclosed quote in `{}`", self.span),
            SyntaxErrorKind::EmptyValue => write!(f, "`{}:` needs a value", self.span),
            SyntaxErrorKind::BadValue => {
                write!(f, "`{}` is not a value that field takes", self.span)
            }
            SyntaxErrorKind::MissingField => {
                write!(f, "`{}` has no field name before the colon", self.span)
            }
        }
    }
}

/// A parsed box, plus whatever did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub expression: Expression,
    pub errors: Vec<SyntaxError>,
}

impl Parsed {
    /// The expression alone, for callers that would rather search with what
    /// they got than surface a half-message.
    pub fn into_expression(self) -> Expression {
        self.expression
    }
}

impl Expression {
    /// True when the box is a plain whitespace-separated run of positive,
    /// unqualified terms — the shape that predates this grammar. Callers use
    /// it to keep the old code path in charge where the two are equivalent by
    /// construction, so a behaviour change can only come from syntax the user
    /// actually typed.
    pub fn is_plain(&self) -> bool {
        self.filters.is_empty()
            && self.groups.len() == 1
            && self.groups.first().is_some_and(|g| {
                !g.atoms.is_empty()
                    && g.atoms
                        .iter()
                        .all(|a| a.is_positive() && a.target == Target::All)
            })
    }

    /// No text terms and no filters: nothing to search for.
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(Group::is_empty) && self.filters.is_empty()
    }

    /// The box re-rendered as it would read unquoted, used by the CLI and the
    /// popover to echo what was understood.
    pub fn terms(&self) -> Vec<&Atom> {
        self.groups.iter().flat_map(|g| g.atoms.iter()).collect()
    }

    /// The plain text a vector embedding should be computed for.
    ///
    /// The semantic leg scores *one* vector against the query, so it can only
    /// carry an unqualified query: anything with a qualifier, an exclusion or a
    /// disjunction has no single text to embed, and returning `None` makes the
    /// caller drop the vector leg rather than let it rank against a
    /// paraphrase the user did not type.
    pub fn embeddable_text(&self) -> Option<String> {
        self.is_plain().then(|| {
            self.groups
                .first()
                .map(|g| {
                    g.atoms
                        .iter()
                        .map(|a| a.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
        })
    }
}

/// Alias table for the text qualifiers. Keys are matched lower-cased.
const FIELD_TARGETS: &[(&str, Target)] = &[
    ("name", Target::Name),
    ("filename", Target::Name),
    ("file", Target::Name),
    ("title", Target::Title),
    ("desc", Target::Description),
    ("description", Target::Description),
    ("tag", Target::Tags),
    ("tags", Target::Tags),
];

fn field_target(field: &str) -> Option<Target> {
    FIELD_TARGETS
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, target)| *target)
}

/// Fields that have a structured meaning, so an empty value after the colon is
/// the user's mistake rather than a term named `field:`.
const STRUCTURAL_FIELDS: &[&str] = &[
    "ext", "format", "kind", "type", "path", "folder", "rating", "stars", "fav", "favorite",
];

fn is_structural(field: &str) -> bool {
    STRUCTURAL_FIELDS.contains(&field)
}

/// Parse the search box. Never fails: an unparseable fragment is reported in
/// [`Parsed::errors`] and skipped.
pub fn parse(input: &str) -> Parsed {
    Parser {
        // Control characters would otherwise reach Tantivy as text; the path
        // this replaces stripped them in `build_query`, so this one does too.
        chars: input.chars().filter(|c| !c.is_control()).collect(),
        at: 0,
        errors: Vec::new(),
    }
    .run()
}

/// An atom is either a text term or a structured condition.
enum AtomOr {
    Term(Atom),
    Filter(QueryCondition),
}

/// One scanned token.
struct Token {
    text: String,
    /// The token *began* with a quote, so a colon inside it is content and not
    /// a field separator: `"a:b"` is one phrase, `tag:"a b"` is a qualifier.
    quoted: bool,
    /// Part of the token came from inside quotes, so its value is a phrase even
    /// though the token itself opened bare.
    value_quoted: bool,
}

struct Parser {
    chars: Vec<char>,
    at: usize,
    errors: Vec<SyntaxError>,
}

impl Parser {
    fn run(mut self) -> Parsed {
        let mut groups: Vec<Group> = Vec::new();
        let mut filters: Vec<QueryCondition> = Vec::new();
        let mut group = Group::default();

        loop {
            self.skip_spaces();
            if self.at_end() {
                break;
            }
            if self.peek() == '|' {
                self.bump();
                if !group.is_empty() {
                    groups.push(std::mem::take(&mut group));
                } else {
                    group = Group::default();
                }
                continue;
            }
            let before = self.at;
            match self.atom() {
                Ok(Some(AtomOr::Term(atom))) => group.atoms.push(atom),
                Ok(Some(AtomOr::Filter(filter))) => filters.push(filter),
                Ok(None) => {}
                Err(error) => self.errors.push(error),
            }
            // An atom that consumed nothing would spin forever; skip a
            // character so the loop always makes progress.
            if self.at == before {
                self.bump();
            }
        }

        if !group.is_empty() {
            groups.push(group);
        }

        Parsed {
            expression: Expression {
                groups,
                filters: QueryCondition::fold(filters),
            },
            errors: self.errors,
        }
    }

    /// One atom, or `None` when only a separator was left.
    fn atom(&mut self) -> Result<Option<AtomOr>, SyntaxError> {
        let Some(token) = self.token()? else {
            return Ok(None);
        };

        let negate = token.text.starts_with('-') && token.text.len() > 1;
        let body = if negate {
            &token.text[1..]
        } else {
            &token.text
        };
        let quoted = token.value_quoted;

        // `field:` is only a qualifier when the token did not open quoted: a
        // phrase like `"a:b"` is content, and so is a file named `arc:1`.
        if !token.quoted
            && let Some((field, inline)) = body.split_once(':')
        {
            return self.qualified(field, negate, inline, quoted);
        }

        let text = clean(body);
        if text.is_empty() {
            return Ok(None);
        }
        Ok(Some(AtomOr::Term(Atom {
            target: Target::All,
            negate,
            text,
            quoted,
        })))
    }

    /// Turn `field` + value into a term or a structured filter. `inline` is
    /// whatever followed the colon on the same token; empty means the value was
    /// a token of its own and still has to be read.
    fn qualified(
        &mut self,
        field: &str,
        negate: bool,
        inline: &str,
        quoted: bool,
    ) -> Result<Option<AtomOr>, SyntaxError> {
        let field = field.to_lowercase();

        if field.is_empty() {
            return Err(SyntaxError::new(
                SyntaxErrorKind::MissingField,
                format!(":{inline}"),
            ));
        }

        let (raw, quoted) = if !inline.is_empty() {
            (inline.to_string(), quoted)
        } else {
            match self.token()? {
                Some(next) => (next.text, next.value_quoted),
                None => {
                    return if field_target(&field).is_some() || is_structural(&field) {
                        Err(SyntaxError::new(SyntaxErrorKind::EmptyValue, field))
                    } else {
                        // Not a qualifier we know: the token was a term.
                        Ok(self.term(&format!("{field}:"), negate, quoted))
                    };
                }
            }
        };

        if let Some(target) = field_target(&field) {
            let text = clean(&raw);
            if text.is_empty() {
                return Err(SyntaxError::new(SyntaxErrorKind::EmptyValue, field));
            }
            return Ok(Some(AtomOr::Term(Atom {
                target,
                negate,
                text,
                quoted,
            })));
        }

        let filter = match field.as_str() {
            "ext" | "format" => {
                let value = clean(&raw).to_lowercase();
                if value.is_empty() {
                    return Err(SyntaxError::new(SyntaxErrorKind::EmptyValue, field));
                }
                QueryCondition::Ext {
                    values: vec![value],
                    negate,
                }
            }
            "kind" | "type" => match parse_kind(&raw) {
                Some(kind) => QueryCondition::Kind {
                    kinds: vec![kind],
                    negate,
                },
                None => {
                    return Err(SyntaxError::new(
                        SyntaxErrorKind::BadValue,
                        format!("{field}:{raw}"),
                    ));
                }
            },
            "path" | "folder" => {
                let prefix = raw.trim().to_string();
                if prefix.is_empty() {
                    return Err(SyntaxError::new(SyntaxErrorKind::EmptyValue, field));
                }
                QueryCondition::Path { prefix, negate }
            }
            "rating" | "stars" => match parse_rating(&raw) {
                Some(rating) => QueryCondition::MinRating(rating),
                None => {
                    return Err(SyntaxError::new(
                        SyntaxErrorKind::BadValue,
                        format!("{field}:{raw}"),
                    ));
                }
            },
            "fav" | "favorite" => match parse_bool(&raw) {
                // `-fav:no` reads as "must not be a favorite".
                Some(value) => QueryCondition::Favorite(negate != value),
                None => {
                    return Err(SyntaxError::new(
                        SyntaxErrorKind::BadValue,
                        format!("{field}:{raw}"),
                    ));
                }
            },
            // An unknown `foo:bar`: keep the user's intent by searching for the
            // whole token rather than dropping it.
            _ => {
                return Ok(self.term(&format!("{field}:{raw}"), negate, quoted));
            }
        };
        Ok(Some(AtomOr::Filter(filter)))
    }

    fn term(&self, text: &str, negate: bool, quoted: bool) -> Option<AtomOr> {
        let text = clean(text);
        if text.is_empty() {
            return None;
        }
        Some(AtomOr::Term(Atom {
            target: Target::All,
            negate,
            text,
            quoted,
        }))
    }

    /// The next token: a bare word, or a balanced `"..."` span. A quote that
    /// opens mid-token (`tag:"a b"`) contributes its contents to the same
    /// token, so a quoted value after a colon needs no special case here.
    fn token(&mut self) -> Result<Option<Token>, SyntaxError> {
        self.skip_spaces();
        if self.at_end() {
            return Ok(None);
        }
        if self.peek() == '"' {
            self.bump();
            return Ok(Some(Token {
                text: self.read_to('"')?,
                quoted: true,
                value_quoted: true,
            }));
        }

        let mut text = String::new();
        let mut value_quoted = false;
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() || c == '|' {
                break;
            }
            if c == '"' {
                self.bump();
                text.push_str(&self.read_to('"')?);
                value_quoted = true;
                break;
            }
            text.push(c);
            self.bump();
        }
        Ok(Some(Token {
            text,
            quoted: false,
            value_quoted,
        }))
    }

    /// Read up to and past `close`, complaining once if the input ends first.
    fn read_to(&mut self, close: char) -> Result<String, SyntaxError> {
        let mut out = String::new();
        loop {
            match self.peek_char() {
                None => {
                    self.errors.push(SyntaxError::new(
                        SyntaxErrorKind::UnclosedQuote,
                        out.clone(),
                    ));
                    return Ok(out);
                }
                Some(c) if c == close => {
                    self.bump();
                    return Ok(out);
                }
                Some(c) => {
                    self.bump();
                    out.push(c);
                }
            }
        }
    }

    // ---- cursor ----------------------------------------------------------

    fn peek(&self) -> char {
        self.chars[self.at]
    }

    fn peek_char(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn bump(&mut self) {
        self.at += 1;
    }

    fn at_end(&self) -> bool {
        self.at >= self.chars.len()
    }

    fn skip_spaces(&mut self) {
        while self.peek_char().is_some_and(|c| c.is_whitespace()) {
            self.bump();
        }
    }
}

/// Lower-case and collapse the whitespace a quoted span can carry, so the
/// index sees the shape it was written with: `"a   b"` and `"a b"` are the
/// same query, and `str::to_lowercase` is what handles the Unicode corners a
/// per-character `to_lowercase` would get wrong.
fn clean(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn parse_kind(text: &str) -> Option<AssetKind> {
    match text.trim().to_lowercase().as_str() {
        "image" | "img" | "图片" => Some(AssetKind::Image),
        "video" | "视频" => Some(AssetKind::Video),
        "audio" | "sound" | "音频" => Some(AssetKind::Audio),
        "font" | "字体" => Some(AssetKind::Font),
        "model" | "3d" => Some(AssetKind::Model),
        "document" | "doc" | "文档" => Some(AssetKind::Document),
        "archive" | "压缩包" => Some(AssetKind::Archive),
        "other" | "其他" => Some(AssetKind::Other),
        _ => None,
    }
}

fn parse_rating(text: &str) -> Option<u8> {
    let digits: String = text.trim().chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u8>().ok().filter(|r| (1..=5).contains(r))
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_lowercase().as_str() {
        "yes" | "true" | "1" | "是" => Some(true),
        "no" | "false" | "0" | "否" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The atoms of a parsed box, flattened across groups, as `(text, target)`.
    fn atoms(input: &str) -> Vec<(String, Target)> {
        parse(input)
            .into_expression()
            .terms()
            .iter()
            .map(|a| (a.text.clone(), a.target))
            .collect()
    }

    /// The texts of one group's atoms.
    fn group_texts(expr: &Expression, group: usize) -> Vec<String> {
        expr.groups[group]
            .atoms
            .iter()
            .map(|a| a.text.clone())
            .collect()
    }

    #[test]
    fn a_bare_query_is_one_group_of_plain_terms() {
        let parsed = parse("sunset beach");
        assert!(parsed.errors.is_empty());
        let expr = parsed.into_expression();
        assert!(expr.is_plain(), "must keep the pre-expression path");
        assert_eq!(expr.groups.len(), 1);
        assert_eq!(group_texts(&expr, 0), vec!["sunset", "beach"]);
    }

    #[test]
    fn a_query_embeds_for_the_semantic_leg_only_while_it_is_plain() {
        assert_eq!(
            parse("猫").into_expression().embeddable_text().as_deref(),
            Some("猫")
        );
        assert_eq!(
            parse("猫 花园")
                .into_expression()
                .embeddable_text()
                .as_deref(),
            Some("猫 花园")
        );
        // Anything the vector leg cannot paraphrase as one sentence is left to
        // the text ranking.
        assert_eq!(parse("猫 | 狗").into_expression().embeddable_text(), None);
        assert_eq!(parse("-猫").into_expression().embeddable_text(), None);
        assert_eq!(parse("tag:猫").into_expression().embeddable_text(), None);
    }

    #[test]
    fn control_characters_are_dropped_before_splitting_as_they_were() {
        // The path this replaces filtered control characters out and *then*
        // split on whitespace, so a tab disappears instead of separating —
        // `a\tb` is one term. Inherited on purpose: matching the old query is
        // what keeps a saved search from changing meaning.
        assert_eq!(atoms("a\u{0}b\tc"), vec![("abc".to_string(), Target::All)]);
        // A real space still separates.
        assert_eq!(atoms("a\u{0}b c").len(), 2);
    }

    #[test]
    fn space_ands_and_bar_ors_with_space_binding_tighter() {
        let expr = parse("猫 狗 | 鸟").into_expression();
        assert!(!expr.is_plain());
        assert_eq!(expr.groups.len(), 2);
        assert_eq!(group_texts(&expr, 0), vec!["猫", "狗"]);
        assert_eq!(group_texts(&expr, 1), vec!["鸟"]);
    }

    #[test]
    fn leading_dash_excludes_within_its_own_group() {
        let expr = parse("-猫 狗").into_expression();
        assert_eq!(expr.groups.len(), 1);
        let atoms = &expr.groups[0].atoms;
        assert!(atoms[0].negate);
        assert_eq!(atoms[0].text, "猫");
        assert!(!atoms[1].negate);
        // An exclusion is not a plain query: it changes which documents the
        // engine may return, not just their order.
        assert!(!expr.is_plain());
        assert_eq!(expr.embeddable_text(), None);
    }

    #[test]
    fn a_quote_keeps_spaces_and_a_bar_literal() {
        let expr = parse("\"a | b\" c").into_expression();
        assert_eq!(expr.groups.len(), 1, "the bar inside quotes is content");
        let atoms = &expr.groups[0].atoms;
        assert_eq!(atoms[0].text, "a | b");
        assert!(atoms[0].quoted);
        assert_eq!(atoms[1].text, "c");
    }

    #[test]
    fn an_unclosed_quote_is_reported_and_the_remainder_reads_as_a_phrase() {
        let parsed = parse("\"猫 花园");
        assert_eq!(
            parsed.errors,
            vec![SyntaxError::new(
                SyntaxErrorKind::UnclosedQuote,
                "猫 花园".to_string()
            )]
        );
        assert_eq!(parsed.expression.groups[0].atoms[0].text, "猫 花园");
    }

    #[test]
    fn text_qualifiers_pick_one_indexed_surface() {
        assert_eq!(atoms("tag:猫"), vec![("猫".to_string(), Target::Tags)]);
        assert_eq!(atoms("name:sun"), vec![("sun".to_string(), Target::Name)]);
        assert_eq!(
            atoms("desc:海边"),
            vec![("海边".to_string(), Target::Description)]
        );
        // Aliases resolve to the same surface.
        for (input, want) in [
            ("filename:x", Target::Name),
            ("file:x", Target::Name),
            ("title:x", Target::Title),
            ("description:x", Target::Description),
            ("tags:x", Target::Tags),
        ] {
            assert_eq!(atoms(input), vec![("x".to_string(), want)], "{input}");
        }
        // Case of the field name is not meaningful.
        assert_eq!(atoms("TAG:猫"), atoms("tag:猫"));
        // A space after the colon is allowed.
        assert_eq!(atoms("tag: 猫"), atoms("tag:猫"));
    }

    #[test]
    fn a_qualified_atom_can_be_negated_and_quoted() {
        let expr = parse("-tag:\"夏日 海边\"").into_expression();
        let atom = &expr.groups[0].atoms[0];
        assert!(atom.negate);
        assert_eq!(atom.text, "夏日 海边");
        assert!(atom.quoted);
        assert_eq!(atom.target, Target::Tags);
    }

    #[test]
    fn an_unknown_colon_token_stays_one_search_term() {
        // A file genuinely named `arc:1` must remain findable.
        assert_eq!(atoms("arc:1"), vec![("arc:1".to_string(), Target::All)]);
        // So must a quoted span that happens to contain a colon.
        assert_eq!(atoms("\"a:b\""), vec![("a:b".to_string(), Target::All)]);
    }

    #[test]
    fn structural_qualifiers_become_filters() {
        let expr = parse("ext:png").into_expression();
        assert!(expr.groups.is_empty(), "a filter alone carries no term");
        assert_eq!(
            expr.filters,
            vec![QueryCondition::Ext {
                values: vec!["png".into()],
                negate: false
            }]
        );

        assert_eq!(
            parse("format:JPG").into_expression().filters,
            vec![QueryCondition::Ext {
                values: vec!["jpg".into()],
                negate: false
            }]
        );
        assert_eq!(
            parse("kind:image").into_expression().filters,
            vec![QueryCondition::Kind {
                kinds: vec![AssetKind::Image],
                negate: false
            }]
        );
        assert_eq!(
            parse("type:视频").into_expression().filters,
            vec![QueryCondition::Kind {
                kinds: vec![AssetKind::Video],
                negate: false
            }]
        );
        assert_eq!(
            parse("rating:4").into_expression().filters,
            vec![QueryCondition::MinRating(4)]
        );
        assert_eq!(
            parse("fav:yes").into_expression().filters,
            vec![QueryCondition::Favorite(true)]
        );
        // `fav:no` already means "not a favorite"; `-fav:no` negates that
        // assertion and asks for favourites.
        assert_eq!(
            parse("fav:no").into_expression().filters,
            vec![QueryCondition::Favorite(false)]
        );
        assert_eq!(
            parse("-fav:no").into_expression().filters,
            vec![QueryCondition::Favorite(true)]
        );
        assert_eq!(
            parse("-fav:yes").into_expression().filters,
            vec![QueryCondition::Favorite(false)]
        );
        assert_eq!(
            parse("path:/data/img").into_expression().filters,
            vec![QueryCondition::Path {
                prefix: "/data/img".into(),
                negate: false
            }]
        );
    }

    #[test]
    fn repeated_ext_and_kind_or_within_one_condition() {
        // Merging matters: `ext:png ext:jpg` is one OR-set, not two filters
        // that would AND into nothing.
        let expr = parse("ext:png ext:jpg").into_expression();
        assert_eq!(
            expr.filters,
            vec![QueryCondition::Ext {
                values: vec!["png".into(), "jpg".into()],
                negate: false
            }]
        );
        let expr = parse("kind:image kind:video").into_expression();
        assert_eq!(
            expr.filters,
            vec![QueryCondition::Kind {
                kinds: vec![AssetKind::Image, AssetKind::Video],
                negate: false
            }]
        );
    }

    #[test]
    fn a_negated_ext_stays_its_own_set_rather_than_joining_the_positive_one() {
        let expr = parse("ext:png -ext:jpg").into_expression();
        assert_eq!(
            expr.filters,
            vec![
                QueryCondition::Ext {
                    values: vec!["png".into()],
                    negate: false
                },
                QueryCondition::Ext {
                    values: vec!["jpg".into()],
                    negate: true
                },
            ]
        );
    }

    #[test]
    fn terms_and_filters_coexist() {
        let expr = parse("猫 ext:png rating:3").into_expression();
        assert_eq!(expr.groups[0].atoms[0].text, "猫");
        assert_eq!(expr.filters.len(), 2);
        assert!(
            !expr.is_plain(),
            "filters narrow the row set, not the ranking"
        );
    }

    #[test]
    fn a_bare_field_with_no_value_is_an_error_not_a_silent_drop() {
        let parsed = parse("tag:");
        assert_eq!(
            parsed.errors,
            vec![SyntaxError::new(
                SyntaxErrorKind::EmptyValue,
                "tag".to_string()
            )]
        );
        assert!(parsed.expression.is_empty());
    }

    #[test]
    fn an_unparseable_structured_value_is_reported() {
        let parsed = parse("kind:banana rating:9 fav:maybe");
        assert_eq!(parsed.errors.len(), 3, "{:?}", parsed.errors);
        assert!(parsed.expression.is_empty());
    }

    #[test]
    fn stray_bars_and_dashes_do_not_spin_or_produce_empty_groups() {
        // An empty disjunct would match everything and swallow the query.
        for input in ["|", "||", "猫 |", "| 猫", "猫 || 狗", "-", "  ", ""] {
            let expr = parse(input).into_expression();
            for group in &expr.groups {
                assert!(!group.is_empty(), "{input:?} left an empty group");
            }
        }
        assert_eq!(parse("猫 |").into_expression().groups.len(), 1);
        assert_eq!(parse("猫 || 狗").into_expression().groups.len(), 2);
    }

    #[test]
    fn an_empty_box_is_empty_and_not_a_search() {
        let expr = parse("   ").into_expression();
        assert!(expr.is_empty());
        assert!(!expr.is_plain());
    }
}
