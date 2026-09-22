//! Turning an asset into a prompt, and a model's reply into tags.
//!
//! Two halves that have to agree: what the model is told about the asset, and
//! what is accepted back from it. The interesting decisions are all in the
//! second half — a model that answers in prose, wraps its JSON in a code
//! fence, numbers its list or invents a 40-word "tag" must not be able to
//! pollute a tag tree, so everything it says goes through [`normalize_tag`]
//! before it reaches the store.

use crate::model::{Asset, AssetKind, MAX_NAME_LEN};

/// Bumped whenever the prompt or the parsing changes in a way that makes
/// earlier results stale. It is part of the fingerprint stored beside every
/// tagged asset, so rewording the prompt makes the next run re-tag the
/// library instead of skipping it as already done.
pub const PROMPT_VERSION: u32 = 1;

/// Ceiling on tags per asset. The prompt asks for fewer; this is the line
/// where a chatty model stops being followed.
const MAX_TAGS_PER_ASSET: usize = 12;

/// Ceiling on one tag's length. Well under [`MAX_NAME_LEN`] on purpose: a
/// "tag" that long is a sentence the model failed to turn into tags.
const MAX_TAG_CHARS: usize = 40;

/// Ceiling on one tag's word count, matching what the prompt asks for. This
/// is what keeps the prose fallback honest: when a model answers with a
/// paragraph, splitting it on commas produces clauses, and a clause is not a
/// tag. (CJK text has no spaces, so a Chinese tag is one "word" — the limit
/// only bites in the scripts that need it.)
const MAX_TAG_WORDS: usize = 3;

/// How many existing tags are quoted to the model. A library with thousands
/// of tags would otherwise spend its whole prompt on the vocabulary; the
/// most-used ones are the ones worth reusing anyway.
const VOCABULARY_CAP: usize = 400;

/// What the model is told about the library and the job.
pub struct PromptOptions<'a> {
    /// Tags already in the library, most used first — the vocabulary the
    /// model is asked to reuse.
    pub vocabulary: &'a [String],
    /// How many tags it may invent beyond that vocabulary.
    pub max_new_tags: u32,
    /// Language the tags are written in (`zh-CN`, `en`, …).
    pub language: &'a str,
}

/// A description of the asset for the model: everything the library knows
/// that a picture cannot show, and everything a bad file name leaves out.
pub fn asset_digest(asset: &Asset, existing_tags: &[String]) -> String {
    let mut lines = vec![format!("file name: {}", asset.file_name)];
    push_line(&mut lines, "title", asset.title.as_deref());
    push_line(&mut lines, "description", asset.description.as_deref());
    lines.push(format!("kind: {}", kind_name(asset.kind)));

    if let (Some(width), Some(height)) = (asset.width, asset.height) {
        let shape = if width > height {
            "landscape"
        } else if height > width {
            "portrait"
        } else {
            "square"
        };
        lines.push(format!("dimensions: {width}x{height} ({shape})"));
    }
    if let Some(ms) = asset.duration_ms {
        lines.push(format!("duration: {:.1}s", ms as f64 / 1000.0));
    }
    if let Some(captured) = asset.captured_at {
        lines.push(format!("captured: {}", captured.format("%Y-%m-%d")));
    }

    // Camera facts. The body and the lens say more about a photo than its
    // file name ever will, and a GPS fix is a location tag nobody has to
    // guess.
    let photo = &asset.facts.photo;
    let camera: Vec<String> = [photo.make.as_deref(), photo.model.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    if !camera.is_empty() {
        lines.push(format!("camera: {}", camera.join(" ")));
    }
    let exposure: Vec<String> = [
        photo.focal_length_mm.as_deref().map(|f| format!("{f}mm")),
        photo.aperture_f.as_deref().map(str::to_string),
        photo.exposure_time.as_deref().map(str::to_string),
        photo.iso.map(|iso| format!("ISO {iso}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !exposure.is_empty() {
        lines.push(format!("exposure: {}", exposure.join(", ")));
    }
    if let (Some(lat), Some(lng)) = (photo.gps_lat, photo.gps_lng) {
        lines.push(format!("location: {lat:.4}, {lng:.4}"));
    }

    if let Some(colour) = asset.facts.visual.dominant_color.as_deref() {
        lines.push(format!("dominant colour: {colour}"));
    }
    push_line(&mut lines, "artist", asset.facts.media.artist.as_deref());
    push_line(&mut lines, "album", asset.facts.media.album.as_deref());
    push_line(
        &mut lines,
        "font family",
        asset.facts.font.family.as_deref(),
    );
    push_line(&mut lines, "font style", asset.facts.font.style.as_deref());

    if !existing_tags.is_empty() {
        lines.push(format!("already tagged: {}", existing_tags.join(", ")));
    }
    lines.join("\n")
}

/// The standing instruction: role, rules, vocabulary, language.
///
/// Built once per run — it is the same for every asset — and written in
/// English because that is the language instruction-following is tuned in;
/// only the *tags* are asked for in the library's language.
pub fn system_prompt(options: &PromptOptions<'_>) -> String {
    let mut prompt = String::from(
        "You are the tagging assistant of a personal media library. You are given \
         one asset at a time and answer with tags describing it.\n\nRules:\n",
    );
    prompt.push_str(
        "- Reply with a JSON array of strings and nothing else: no prose, no code fence.\n",
    );
    prompt.push_str("- One concept per tag, at most three words, no punctuation at either end.\n");
    prompt.push_str(&format!(
        "- At most {MAX_TAGS_PER_ASSET} tags. An empty array is a valid answer when the asset is unremarkable.\n",
    ));
    prompt.push_str("- Never repeat a tag, and never emit an empty one.\n");
    prompt.push_str("- Describe what is there, not what might be there.\n");

    let vocabulary: Vec<&str> = options
        .vocabulary
        .iter()
        .map(String::as_str)
        .take(VOCABULARY_CAP)
        .collect();
    if vocabulary.is_empty() {
        prompt.push_str("\nThe library has no tags yet: every tag will be a new one.\n");
    } else {
        prompt.push_str(
            "\nPrefer a tag from the vocabulary below. Do not invent a near-synonym of one \
             that is already there, and copy a vocabulary tag's spelling exactly when you \
             use it.\n",
        );
        prompt.push_str(&format!(
            "- At most {} tag(s) may be new, i.e. absent from the vocabulary. The rest must come from it.\n",
            options.max_new_tags
        ));
        prompt.push_str(&format!(
            "\nVocabulary ({} tags, most used first):\n{}\n",
            vocabulary.len(),
            vocabulary.join(", ")
        ));
    }

    prompt.push_str(&format!(
        "\nWrite every tag in {}.",
        language_name(options.language)
    ));
    prompt
}

/// The per-asset line that says whether an image came with the request.
///
/// Deliberately not in [`system_prompt`]: whether a thumbnail exists is a
/// property of the individual asset — a video without a decoded frame, a
/// format with no preview — and a system message claiming "the image is
/// attached" would be wrong for exactly those.
pub fn image_note(attached: bool) -> &'static str {
    if attached {
        "(The image is attached: tag what you can see in it.)"
    } else {
        "(No image is attached; work from the facts above.)"
    }
}

/// Parse whatever the model replied into clean tags: JSON first, and a
/// forgiving line/comma split when it answered in prose anyway.
pub fn parse_tags(reply: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for candidate in candidates(reply) {
        let Some(tag) = normalize_tag(&candidate) else {
            continue;
        };
        // Case-insensitive because that is how the tag store resolves names:
        // emitting both "Cat" and "cat" would create one tag, not two.
        if tags.iter().any(|kept| kept.eq_ignore_ascii_case(&tag)) {
            continue;
        }
        tags.push(tag);
        if tags.len() >= MAX_TAGS_PER_ASSET {
            break;
        }
    }
    tags
}

fn candidates(reply: &str) -> Vec<String> {
    if let Some(values) = json_array(reply) {
        return values;
    }
    // Not JSON, or JSON we could not read: treat separators as separators and
    // let `normalize_tag` throw away whatever is left of the prose.
    reply
        .lines()
        .flat_map(|line| line.split([',', ';', '、']))
        .map(str::to_string)
        .collect()
}

/// The first JSON array in the reply, if it parses as an array of strings.
fn json_array(reply: &str) -> Option<Vec<String>> {
    let start = reply.find('[')?;
    let end = reply.rfind(']')?;
    if end <= start {
        return None;
    }
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&reply[start..=end]).ok()?;
    Some(
        parsed
            .into_iter()
            .filter_map(|value| match value {
                serde_json::Value::String(text) => Some(text),
                // A model that emitted `[{"tag": "cat"}]` is close enough to
                // be worth reading rather than discarding.
                serde_json::Value::Object(mut object) => object
                    .remove("tag")
                    .or_else(|| object.remove("name"))
                    .and_then(|value| value.as_str().map(str::to_string)),
                _ => None,
            })
            .collect(),
    )
}

/// Clean one candidate tag, or reject it.
///
/// This is the boundary the tag tree is protected by. List markers, quotes,
/// brackets, fences and trailing punctuation come off; anything still too
/// long, too empty, too wordy — or not a tag at all — is dropped.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let trimmed = strip_list_marker(raw.trim());
    let trimmed = trimmed.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\''
                    | '`'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '('
                    | ')'
                    | '*'
                    | '#'
                    | ','
                    | ';'
                    | ':'
                    | '。'
                    | '，'
                    | '、'
                    | '\\'
                    | '|'
                    | '…'
            )
    });
    // Checked before the whitespace collapse, which would otherwise turn a
    // newline into an innocent-looking space.
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let length = collapsed.chars().count();
    if length > MAX_TAG_CHARS || length > MAX_NAME_LEN {
        return None;
    }
    if collapsed.split_whitespace().count() > MAX_TAG_WORDS {
        return None;
    }
    Some(collapsed)
}

/// Strip one leading list marker: `1. `, `2) `, `- `, `* `, `• `.
///
/// Digits followed by anything other than a separator are left alone, so a
/// tag like `24mm` survives — which is the whole reason this is not a regex
/// over "leading non-letters".
fn strip_list_marker(text: &str) -> &str {
    let bytes = text.as_bytes();
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > 0 {
        let after = bytes.get(digits).copied();
        match after {
            Some(b'.') | Some(b')') | Some(b':') => return text[digits + 1..].trim_start(),
            Some(b' ') => return text[digits..].trim_start(),
            // The digits belong to the tag (`24mm`, `4000x3000`).
            _ => return text,
        }
    }
    if matches!(bytes.first(), Some(b'-') | Some(b'*') | Some(b'+')) {
        // Only when it reads as a bullet: the marker stands alone or is
        // followed by a space. `-cat` is a tag, not a list item.
        let rest = &text[1..];
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return rest.trim_start();
        }
        return text;
    }
    text.strip_prefix('•')
        .or_else(|| text.strip_prefix('·'))
        .map(str::trim_start)
        .unwrap_or(text)
}

/// Add `label: value` when the value is present and not blank.
fn push_line(lines: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        lines.push(format!("{label}: {value}"));
    }
}

fn kind_name(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Image => "image",
        AssetKind::Video => "video",
        AssetKind::Audio => "audio",
        AssetKind::Document => "document",
        AssetKind::Archive => "archive",
        AssetKind::Font => "font",
        AssetKind::Model => "3D model",
        AssetKind::Other => "file",
    }
}

/// The language name to ask for, from a locale tag. Falls back to English,
/// which is also what an unset preference means.
fn language_name(locale: &str) -> &'static str {
    match locale.split(['-', '_']).next().unwrap_or_default() {
        "zh" => "Simplified Chinese",
        "ja" => "Japanese",
        "ko" => "Korean",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "pt" => "Portuguese",
        "ru" => "Russian",
        _ => "English",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AssetFacts;

    fn asset(file_name: &str) -> Asset {
        let mut asset = crate::model::test_asset(file_name, AssetKind::Image, uuid::Uuid::nil());
        asset.width = Some(4000);
        asset.height = Some(3000);
        asset.captured_at = Some(chrono::DateTime::from_timestamp(1_770_000_000, 0).unwrap());
        asset
    }

    #[test]
    fn a_digest_carries_the_facts_the_file_name_does_not() {
        let facts = AssetFacts {
            photo: crate::model::PhotoFacts {
                make: Some("Canon".into()),
                model: Some("EOS R6".into()),
                focal_length_mm: Some("35".into()),
                aperture_f: Some("f/1.8".into()),
                iso: Some(400),
                ..Default::default()
            },
            ..AssetFacts::default()
        };

        let mut asset = asset("IMG_4821.jpg");
        asset.facts = facts;
        let digest = asset_digest(&asset, &["旅行".to_string()]);

        assert!(digest.contains("IMG_4821.jpg"));
        assert!(digest.contains("dimensions: 4000x3000 (landscape)"));
        assert!(digest.contains("camera: Canon EOS R6"));
        assert!(digest.contains("35mm"), "{digest}");
        assert!(digest.contains("f/1.8"));
        assert!(digest.contains("ISO 400"));
        assert!(digest.contains("already tagged: 旅行"));
    }

    #[test]
    fn a_digest_of_a_bare_asset_still_says_what_it_is() {
        let digest = asset_digest(&asset("notes.txt"), &[]);
        assert!(digest.contains("file name: notes.txt"));
        assert!(!digest.contains("camera:"), "no invented facts: {digest}");
        assert!(!digest.contains("location:"));
    }

    #[test]
    fn an_empty_vocabulary_asks_for_new_tags_instead_of_reuse() {
        let prompt = system_prompt(&PromptOptions {
            vocabulary: &[],
            max_new_tags: 3,
            language: "zh-CN",
        });
        assert!(prompt.contains("no tags yet"));
        assert!(!prompt.contains("Prefer a tag from the vocabulary"));
        assert!(prompt.contains("Simplified Chinese"));
    }

    #[test]
    fn a_vocabulary_is_quoted_with_its_new_tag_budget() {
        let vocabulary = vec!["旅行".to_string(), "风景".to_string()];
        let prompt = system_prompt(&PromptOptions {
            vocabulary: &vocabulary,
            max_new_tags: 1,
            language: "en",
        });
        assert!(prompt.contains("旅行, 风景"));
        assert!(prompt.contains("At most 1 tag(s) may be new"));
        assert!(prompt.contains("English"));
    }

    #[test]
    fn the_image_note_follows_the_asset_not_the_prompt() {
        assert!(image_note(true).contains("image is attached"));
        assert!(image_note(false).contains("No image"));
    }

    #[test]
    fn parse_reads_a_json_array() {
        assert_eq!(
            parse_tags(r#"["cat", "tabby"]"#),
            vec!["cat".to_string(), "tabby".to_string()]
        );
    }

    #[test]
    fn parse_sees_through_the_packaging_models_add() {
        // A code fence around the array.
        assert_eq!(
            parse_tags("```json\n[\"cat\"]\n```"),
            vec!["cat".to_string()]
        );
        // Prose in front of it.
        assert_eq!(
            parse_tags("Here are the tags:\n[\"cat\", \"pet\"]\nHope that helps!"),
            vec!["cat".to_string(), "pet".to_string()]
        );
        // Objects instead of strings.
        assert_eq!(
            parse_tags(r#"[{"tag": "cat"}, {"name": "pet"}]"#),
            vec!["cat".to_string(), "pet".to_string()]
        );
    }

    #[test]
    fn parse_falls_back_to_plain_separators() {
        assert_eq!(
            parse_tags("cat, tabby, pet"),
            vec!["cat".to_string(), "tabby".to_string(), "pet".to_string()]
        );
        assert_eq!(
            parse_tags("- cat\n- tabby\n"),
            vec!["cat".to_string(), "tabby".to_string()]
        );
        assert_eq!(
            parse_tags("1. cat\n2. tabby"),
            vec!["cat".to_string(), "tabby".to_string()]
        );
    }

    #[test]
    fn parse_drops_what_is_not_a_tag() {
        assert!(parse_tags("").is_empty());
        assert!(parse_tags("```").is_empty());
        assert!(parse_tags("[]").is_empty());
        // A model that answered with a paragraph instead of tags.
        let paragraph = "I would describe this image as a photograph of a cat \
                         sitting on a wooden table in a sunlit room, taken with a \
                         wide angle lens and a shallow depth of field.";
        assert!(
            parse_tags(paragraph).is_empty(),
            "{}",
            parse_tags(paragraph).join("|")
        );
    }

    #[test]
    fn parse_deduplicates_case_insensitively_and_caps_the_count() {
        assert_eq!(
            parse_tags(r#"["Cat", "cat", "CAT"]"#),
            vec!["Cat".to_string()],
            "the store resolves names case-insensitively, so these are one tag"
        );
        let many: Vec<String> = (0..40).map(|i| format!("\"t{i}\"")).collect();
        let reply = format!("[{}]", many.join(", "));
        assert_eq!(parse_tags(&reply).len(), MAX_TAGS_PER_ASSET);
    }

    #[test]
    fn normalize_strips_the_packaging_and_keeps_the_words() {
        assert_eq!(normalize_tag("  \"cat\"  ").as_deref(), Some("cat"));
        assert_eq!(normalize_tag("1. cat").as_deref(), Some("cat"));
        assert_eq!(normalize_tag("- *cat*").as_deref(), Some("cat"));
        assert_eq!(normalize_tag("cat,").as_deref(), Some("cat"));
        assert_eq!(normalize_tag("  猫 咪  ").as_deref(), Some("猫 咪"));
        assert_eq!(
            normalize_tag("urban  landscape").as_deref(),
            Some("urban landscape")
        );
    }

    #[test]
    fn normalize_rejects_what_a_tag_cannot_be() {
        assert_eq!(normalize_tag(""), None);
        assert_eq!(normalize_tag("   "), None);
        assert_eq!(normalize_tag("\"\""), None);
        assert_eq!(normalize_tag(&"x".repeat(MAX_TAG_CHARS + 1)), None);
        assert_eq!(
            normalize_tag("two\nlines"),
            None,
            "a control character got in"
        );
    }
}
