//! CJK language resolution for font assets.
//!
//! The decision chain is a port of Serpent's `font-metadata.ts` — the OS/2
//! code-page bits, the name-table LCID vote, the family-name keywords — kept
//! here as pure functions over plain data so tests need no font file. The
//! rule at its core survives the port unchanged: **coverage never decides a
//! language**. Kana are part of the shared CJK glyph set (194 of Serpent's
//! 1171 sampled system fonts contain kana and are Chinese or Korean), so
//! cmap coverage only ever picks between languages that are already
//! declared; it is never a vote.
//!
//! Order of evidence when a file declares more than one CJK language:
//! name-table locales, then family-name keywords, then the code-page
//! priority Blender's font preview uses (Korean and Traditional unambigu-
//! ously, Japanese and Simplified only against each other). Still nothing?
//! `None` — a neutral answer beats a wrong one.

/// The CJK languages an OpenType file can declare, in the ASCII token
/// spelling (`ja`, `ko`, `zh-Hans`, `zh-Hant`) that ends up in the facts
/// JSON and the search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CjkLanguage {
    Korean,
    TraditionalChinese,
    Japanese,
    SimplifiedChinese,
}

impl CjkLanguage {
    pub fn token(self) -> &'static str {
        match self {
            Self::Korean => "ko",
            Self::TraditionalChinese => "zh-Hant",
            Self::Japanese => "ja",
            Self::SimplifiedChinese => "zh-Hans",
        }
    }

    const ALL: [Self; 4] = [
        Self::Korean,
        Self::TraditionalChinese,
        Self::Japanese,
        Self::SimplifiedChinese,
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|l| *l == self)
            .expect("closed set")
    }
}

/// The one language a font file claims, as far as the file itself can be
/// believed. [`Language::Latin`] is a conclusion (no CJK declared);
/// `None` is the honest "the file declares two things and nothing breaks
/// the tie".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Latin,
    Cjk(CjkLanguage),
}

impl Language {
    pub fn token(self) -> &'static str {
        match self {
            Self::Latin => "latin",
            Self::Cjk(CjkLanguage::Korean) => "ko",
            Self::Cjk(CjkLanguage::TraditionalChinese) => "zh-Hant",
            Self::Cjk(CjkLanguage::Japanese) => "ja",
            Self::Cjk(CjkLanguage::SimplifiedChinese) => "zh-Hans",
        }
    }
}

/// What the file declares, as plain data. Assembled from the face by
/// [`super::metadata`]; consumed here and unit-tested without any font.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    /// `ulCodePageRange1/2` from the OS/2 table — `None` when the table is
    /// v0, which predates the fields and therefore declares nothing.
    pub code_page_range: Option<(u32, u32)>,
    /// Windows LCIDs from the name table records.
    pub name_locales: Vec<u16>,
    /// Family-ish names (family, typographic family, full name).
    pub family_names: Vec<String>,
    /// Name-table strings recorded under a non-English locale: kana or
    /// hangul there is strong evidence about which CJK language this is.
    pub localized_names: Vec<String>,
    /// cmap coverage. Feeds the Latin fallback and nothing else.
    pub covers_latin: bool,
}

/// The decision outcome: everything declared, plus the one language we are
/// willing to name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolution {
    pub declared: Vec<CjkLanguage>,
    pub resolved: Option<Language>,
}

/// Win32 code page bitfields, OpenType spec §OS/2: 17 JIS / 18 GB2312 /
/// 19 Wansung / 20 Big5 / 21 Johab. The declaration order matches the
/// code-page priority below: Korean first, then Traditional.
fn declared_cjk(range: Option<(u32, u32)>) -> Vec<CjkLanguage> {
    let Some((bits1, bits2)) = range else {
        return Vec::new();
    };
    let bit = |n: u32| {
        if n < 32 {
            bits1 >> n & 1 == 1
        } else {
            bits2 >> (n - 32) & 1 == 1
        }
    };
    let mut declared = Vec::new();
    if bit(19) || bit(21) {
        declared.push(CjkLanguage::Korean);
    }
    if bit(20) {
        declared.push(CjkLanguage::TraditionalChinese);
    }
    if bit(17) {
        declared.push(CjkLanguage::Japanese);
    }
    if bit(18) {
        declared.push(CjkLanguage::SimplifiedChinese);
    }
    declared
}

/// The LCIDs that identify which locale a font was localized *for*. Only
/// CJK locales are interesting here; anything else contributes no vote.
fn lcid_language(lcid: u16) -> Option<CjkLanguage> {
    match lcid {
        0x0411 => Some(CjkLanguage::Japanese),
        0x0804 | 0x1004 => Some(CjkLanguage::SimplifiedChinese),
        0x0404 | 0x0c04 | 0x1404 => Some(CjkLanguage::TraditionalChinese),
        0x0412 => Some(CjkLanguage::Korean),
        _ => None,
    }
}

/// Family/typographic-name keywords. Deliberately narrow: they only break a
/// tie between languages the file declares at once (Noto Sans JP and Source
/// Han Sans CN both set the JIS *and* GB2312 bits), and a false positive
/// would put the wrong language on the card. The one lossy clause against
/// Serpent is `gothic.*jp`, which needs a regex engine to express and buys
/// little beyond the literal `jp` token it also matches.
const LANG_KOREAN: &[&str] = &[
    "korean", "kr", "malgun", "batang", "gulim", "dotum", "nanum", "gungsuh", "돋움", "바탕",
    "굴림", "궁서", "맑은",
];
const LANG_JAPANESE: &[&str] = &[
    "japanese",
    "jp",
    "jpn",
    "meiryo",
    "hiragino",
    "mincho",
    "ゴシック",
    "明朝",
    "丸ゴ",
    "メイリオ",
    "ヒラギノ",
    "游ゴ",
    "角ゴ",
];
const LANG_TRADITIONAL: &[&str] = &[
    "traditional",
    "tc",
    "tw",
    "hk",
    "jhenghei",
    "mingliu",
    "正黑",
    "細明",
    "新細明",
    "標楷",
    "儷宋",
    "繁體",
    "繁体",
];
const LANG_SIMPLIFIED: &[&str] = &[
    "simplified",
    "sc",
    "cn",
    "gb",
    "yahei",
    "simsun",
    "dengxian",
    "source han sans cn",
    "思源黑体",
    "思源宋体",
    "雅黑",
    "宋体",
    "黑体",
    "楷体",
    "仿宋",
    "简体",
    "简",
];

/// `(^|[^a-z])token([^a-z]|$)` without a regex: the token must not sit
/// inside a longer lowercase run, so "jp" matches "Noto Sans JP" and
/// "jpfont" is not poisoned by "monoJP", but "kanji" is left alone.
fn has_token(lower: &str, token: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut start = 0;
    while let Some(pos) = lower[start..].find(token) {
        let i = start + pos;
        let end = i + token.len();
        let bounded = (i == 0 || !bytes[i - 1].is_ascii_lowercase())
            && (end == bytes.len() || !bytes[end].is_ascii_lowercase());
        if bounded {
            return true;
        }
        // `token` is ASCII, so `i` sits on a char boundary and `i + 1` does too.
        start = i + 1;
    }
    false
}

fn has_kana(text: &str) -> bool {
    text.chars().any(|c| ('\u{3040}'..='\u{30ff}').contains(&c))
}

fn has_hangul(text: &str) -> bool {
    text.chars().any(|c| {
        ('\u{1100}'..='\u{11ff}').contains(&c)
            || ('\u{3130}'..='\u{318f}').contains(&c)
            || ('\u{ac00}'..='\u{d7af}').contains(&c)
    })
}

/// First past the post with an explicit tie rule: a tie is no winner.
#[derive(Default)]
struct Votes([u32; 4]);

impl Votes {
    fn vote(&mut self, lang: CjkLanguage) {
        self.0[lang.index()] += 1;
    }

    fn winner(&self) -> Option<CjkLanguage> {
        let (best_i, &best_count) = self.0.iter().enumerate().max_by_key(|(_, count)| **count)?;
        if best_count == 0 {
            return None;
        }
        if self.0.iter().filter(|count| **count == best_count).count() > 1 {
            return None;
        }
        Some(CjkLanguage::ALL[best_i])
    }
}

fn family_name_winner(evidence: &Evidence, declared: &[CjkLanguage]) -> Option<CjkLanguage> {
    let mut votes = Votes::default();
    let family = evidence.family_names.join(" ").to_lowercase();
    for (lang, keywords) in [
        (CjkLanguage::Korean, LANG_KOREAN),
        (CjkLanguage::Japanese, LANG_JAPANESE),
        (CjkLanguage::TraditionalChinese, LANG_TRADITIONAL),
        (CjkLanguage::SimplifiedChinese, LANG_SIMPLIFIED),
    ] {
        if keywords.iter().any(|kw| has_token(&family, kw)) {
            votes.vote(lang);
        }
    }
    // Kana or hangul inside a localized name is a very strong signal — a
    // Chinese font's Chinese names contain neither.
    let localized = evidence.localized_names.join(" ");
    if has_kana(&localized) {
        votes.vote(CjkLanguage::Japanese);
    }
    if has_hangul(&localized) {
        votes.vote(CjkLanguage::Korean);
    }
    let winner = votes.winner()?;
    // A keyword hit counts even when the code-page bits did not declare the
    // language (that is the OS/2 v0 path); when they did declare something
    // else entirely, the file wins and we stay silent.
    if declared.is_empty() || declared.contains(&winner) {
        Some(winner)
    } else {
        None
    }
}

fn locale_winner(evidence: &Evidence, declared: &[CjkLanguage]) -> Option<CjkLanguage> {
    let mut votes = Votes::default();
    for locale in &evidence.name_locales {
        if let Some(lang) = lcid_language(*locale)
            && declared.contains(&lang)
        {
            votes.vote(lang);
        }
    }
    votes.winner()
}

pub fn resolve(evidence: &Evidence) -> Resolution {
    let declared = declared_cjk(evidence.code_page_range);
    let resolved = match &declared[..] {
        // OS/2 v0 carries no code-page fields: a keyword hit still counts
        // (the keyword vote accepts undeclared languages on purpose), and
        // beyond that only Latin is safe to name.
        _ if evidence.code_page_range.is_none() => family_name_winner(evidence, &declared)
            .map(Language::Cjk)
            .or_else(|| evidence.covers_latin.then_some(Language::Latin)),
        [] => evidence.covers_latin.then_some(Language::Latin),
        [one] => Some(Language::Cjk(*one)),
        many => locale_winner(evidence, many)
            .or_else(|| family_name_winner(evidence, many))
            .or_else(|| {
                // The code-page priority: Korean and Traditional win
                // outright; Japanese and Simplified only when the other is
                // absent. `declared_cjk` built `many` in this priority
                // order already, so first match is the priority order.
                many.iter().copied().find(|lang| match lang {
                    CjkLanguage::Korean | CjkLanguage::TraditionalChinese => true,
                    CjkLanguage::Japanese => !many.contains(&CjkLanguage::SimplifiedChinese),
                    CjkLanguage::SimplifiedChinese => !many.contains(&CjkLanguage::Japanese),
                })
            })
            .map(Language::Cjk),
    };
    Resolution { declared, resolved }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(code_page: Option<(u32, u32)>) -> Evidence {
        Evidence {
            code_page_range: code_page,
            ..Default::default()
        }
    }

    /// Bit 17 is JIS, 18 GB2312, 19 Wansung, 20 Big5, 21 Johab — all five
    /// live in `ulCodePageRange1`; the pair exists because the bitfield as
    /// a whole is 64 bits wide.
    #[test]
    fn code_page_bits_declare_in_priority_order() {
        let range = (1 << 17, 0);
        assert_eq!(declared_cjk(Some(range)), vec![CjkLanguage::Japanese]);
        let range = (1 << 18, 0);
        assert_eq!(
            declared_cjk(Some(range)),
            vec![CjkLanguage::SimplifiedChinese]
        );
        // Johab (21) and Wansung (19) are one Korean.
        assert_eq!(
            declared_cjk(Some((1 << 21, 0))),
            declared_cjk(Some((1 << 19, 0)))
        );
        // Priority order when several bits are set at once.
        let all = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20);
        assert_eq!(
            declared_cjk(Some((all, 0))),
            vec![
                CjkLanguage::Korean,
                CjkLanguage::TraditionalChinese,
                CjkLanguage::Japanese,
                CjkLanguage::SimplifiedChinese,
            ]
        );
    }

    #[test]
    fn a_single_declaration_is_taken_at_face_value() {
        let mut e = evidence(Some((1 << 17, 0)));
        e.covers_latin = true;
        assert_eq!(
            resolve(&e).resolved,
            Some(Language::Cjk(CjkLanguage::Japanese))
        );
    }

    #[test]
    fn no_declaration_resolves_to_latin_when_covered() {
        let mut e = evidence(Some((0, 0)));
        e.covers_latin = true;
        assert_eq!(resolve(&e).resolved, Some(Language::Latin));
        e.covers_latin = false;
        // A font with no declaration and no Latin coverage stays unnamed.
        assert_eq!(resolve(&e).resolved, None);
    }

    /// Source Han Sans CN declares JIS *and* GB2312; the name locale is
    /// what says Simplified.
    #[test]
    fn name_locales_break_a_multi_declaration_tie() {
        let mut e = evidence(Some(((1 << 17) | (1 << 18), 0)));
        e.name_locales = vec![0x0804];
        assert_eq!(
            resolve(&e).resolved,
            Some(Language::Cjk(CjkLanguage::SimplifiedChinese))
        );
        e.name_locales = vec![0x0411];
        assert_eq!(
            resolve(&e).resolved,
            Some(Language::Cjk(CjkLanguage::Japanese))
        );
        // A locale that voted against nothing declared contributes no vote,
        // and the priority rule then decides: both declared → falls through
        // to "Japanese requires Simplified absent" → neither priority applies
        // → unresolved. That is the honest answer for a tie with no signal.
        e.name_locales = vec![0x0409];
        assert_eq!(resolve(&e).resolved, None);
    }

    #[test]
    fn family_keywords_break_a_tie_the_locale_vote_cannot() {
        let mut e = evidence(Some(((1 << 17) | (1 << 18), 0)));
        e.family_names = vec!["Noto Sans JP".into()];
        assert_eq!(
            resolve(&e).resolved,
            Some(Language::Cjk(CjkLanguage::Japanese))
        );
        e.family_names = vec!["Source Han Sans CN".into()];
        assert_eq!(
            resolve(&e).resolved,
            Some(Language::Cjk(CjkLanguage::SimplifiedChinese))
        );
    }

    #[test]
    fn a_keyword_hit_names_an_undeclared_language_on_os2_v0() {
        let mut e = evidence(None);
        e.family_names = vec!["Malgun Gothic".into()];
        assert_eq!(
            resolve(&e).resolved,
            Some(Language::Cjk(CjkLanguage::Korean))
        );
    }

    /// Token boundaries: "jp" hits "Noto Sans JP" but not "kanji"; "kr"
    /// hits "Malgun KR" but not "kraft".
    #[test]
    fn keyword_tokens_respect_word_boundaries() {
        assert!(has_token("noto sans jp", "jp"));
        assert!(!has_token("kanji", "jp"));
        assert!(has_token("malgun kr", "kr"));
        assert!(!has_token("kraft", "kr"));
        // CJK keywords need no boundaries of their own: 游 inside 游ゴシック
        // is part of the same literal.
        assert!(has_token("游ゴシック", "游ゴ"));
    }

    #[test]
    fn tied_votes_have_no_winner() {
        let mut votes = Votes::default();
        votes.vote(CjkLanguage::Japanese);
        votes.vote(CjkLanguage::Korean);
        assert_eq!(votes.winner(), None);
        votes.vote(CjkLanguage::Japanese);
        assert_eq!(votes.winner(), Some(CjkLanguage::Japanese));
    }
}
