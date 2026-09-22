//! AI analysis protocol: the shared types and policies that tie the vendor
//! adapters to the rest of Trove.
//!
//! This is the "structured output" half of Trove's AI: a multimodal model
//! (vision + text) is handed an asset's thumbnail and metadata, and returns a
//! description, tags, and an optional rating. It is deliberately separate
//! from the text-only [`crate::ai::EmbeddingProvider`] used for vector search
//! — an embedding turns text into a vector; this answers a question about the
//! asset, which is what the automatic describer/tagger/rater asks.
//!
//! Everything here is synchronous by design: providers run on background task
//! threads ([`crate::tasks`]), which are plain `std::thread`s.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::model::{Asset, AssetKind, MAX_NAME_LEN};

/// Bumped whenever the prompt or the parsing changes in a way that makes
/// earlier results stale. It is part of the fingerprint stored beside every
/// analysed asset, so rewording the prompt makes the next run re-analyse the
/// library instead of skipping it as already done.
pub const PROMPT_VERSION: u32 = 1;

/// How many existing tags are quoted to the model. A library with thousands
/// of tags would otherwise spend its whole prompt on the vocabulary; the
/// most-used ones are the ones worth reusing anyway.
const VOCABULARY_CAP: usize = 400;

/// Ceiling on a tag's word count. This is what keeps the prose fallback
/// honest: when a model answers with a sentence, it must not be stored as a
/// tag.
const MAX_TAG_WORDS: usize = 3;

// ============================ output =======================================

/// What one settled analysis run produced for one asset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiAnalysisResult {
    /// Natural-language description of the asset content.
    #[serde(default)]
    pub description: Option<String>,
    /// Relevant keyword tags describing the asset.
    pub tags: Vec<String>,
    /// Aesthetic score from 1 to 5, when the model was asked for one.
    #[serde(default)]
    pub rating: Option<u8>,
    /// The vendor model version that produced this result. Stored beside
    /// every asset it tagged, so a later run can tell whose work it is
    /// looking at.
    #[serde(default)]
    pub model_version: String,
}

/// Fields the user may ask the model to produce. A model is always free to
/// produce fewer, but never more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AiAnalysisFields {
    pub description: bool,
    pub tags: bool,
    pub rating: bool,
}

impl Default for AiAnalysisFields {
    fn default() -> Self {
        Self {
            description: true,
            tags: true,
            rating: false,
        }
    }
}

impl AiAnalysisFields {
    pub fn is_empty(self) -> bool {
        !self.description && !self.tags && !self.rating
    }
}

// ============================ request ======================================

/// Everything a vendor adapter needs to analyse one asset.
#[derive(Debug, Clone)]
pub struct AiAnalysisRequest {
    /// Asset id the result will be written back to.
    pub asset_id: Uuid,
    /// UI display name (basename of library-relative path).
    pub display_name: String,
    /// Original file name.
    pub file_name: String,
    /// MIME type of the asset.
    pub mime: String,
    /// Visual presentation kind — drives the system prompt explanation.
    pub media_type: MediaType,
    /// Asset thumbnail as JPEG bytes. `None` is a text-only request.
    pub thumbnail_jpeg: Option<Vec<u8>>,
    /// For videos: a contact-sheet (key-frame collage) as JPEG bytes.
    pub contact_sheet_jpeg: Option<Vec<u8>>,
    /// The prompt language line (may list multiple).
    pub language: String,
    /// Which fields the model should produce.
    pub enabled_fields: AiAnalysisFields,
    /// Extra metadata lines the caller mined from the asset — camera, GPS,
    /// capture date, dominant colour, font family, … — shown to the model
    /// verbatim. Built by [`asset_metadata_lines`].
    pub metadata_lines: Vec<String>,
    /// Tags already on the asset — shown to the model so it does not repeat
    /// them.
    pub existing_tag_names: Vec<String>,
    /// The library's tag vocabulary, most used first. The model is asked to
    /// prefer these spellings over inventing near-synonyms, and the new-word
    /// budget in [`AiAnalysisSettings::max_new_tags`] is enforced against
    /// them in [`post_process`].
    pub vocabulary: Vec<String>,
    /// Analysis policy settings.
    pub settings: AiAnalysisSettings,
}

/// Visual presentation kind — drives the system prompt explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaType {
    Image,
    Video,
    Model3D,
    Other,
}

/// User-tunable policy knobs that the prompt cannot enforce. The prompt is
/// advisory: a model that answers in prose, wraps its JSON in a fence, or
/// invents a 40-word tag must not be able to pollute the tag tree, so every
/// field goes through [`post_process`] before it reaches the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiAnalysisSettings {
    /// Ceiling on tags per asset.
    pub max_tags: usize,
    /// How many tags per asset may be invented beyond the library
    /// vocabulary. Zero means "reuse only", the safest setting for a library
    /// whose tag tree is already deliberate.
    pub max_new_tags: u32,
    /// Ceiling on one tag's character length.
    pub max_tag_chars: usize,
    /// Ceiling on a description's character count (CJK).
    pub max_description_chars_zh: usize,
    /// Ceiling on a description's word count (English).
    pub max_description_words_en: usize,
    /// When true, the model may not invent tags — only ones already on the
    /// asset or in the library vocabulary are allowed.
    pub force_existing_tags: bool,
}

impl Default for AiAnalysisSettings {
    fn default() -> Self {
        Self {
            max_tags: 12,
            max_new_tags: 3,
            max_tag_chars: 40,
            max_description_chars_zh: 200,
            max_description_words_en: 60,
            force_existing_tags: false,
        }
    }
}

// ============================ prompt builders =============================

/// The standing instruction: role, rules, output shape, language.
///
/// Built once per run — it is the same for every asset — and written in
/// English because that is the language instruction-following is tuned in;
/// only the *output* is asked for in the library's language.
pub fn system_prompt(request: &AiAnalysisRequest) -> String {
    let settings = &request.settings;
    let fields = request.enabled_fields;
    let language = request.language.as_str();
    let mut prompt = String::from(
        "You are the analysis assistant of a personal media library. You are given one \
         asset at a time and answer with a structured description of it.\n\nRules:\n",
    );
    prompt.push_str("- Describe what is there, not what might be there.\n");
    prompt.push_str("- Reply with a JSON object and nothing else: no prose, no code fence.\n");

    if fields.description {
        prompt.push_str("- `description`: a concise natural-language description, or null.\n");
    }
    if fields.tags {
        prompt.push_str(
            "- `tags`: an array of keyword tags, one concept per tag, at most three words each.\n",
        );
        prompt.push_str(&format!("- At most {} tags.\n", settings.max_tags));

        let vocabulary: Vec<&str> = request
            .vocabulary
            .iter()
            .map(String::as_str)
            .take(VOCABULARY_CAP)
            .collect();
        if vocabulary.is_empty() {
            prompt.push_str("- The library has no tags yet: every tag will be a new one.\n");
        } else {
            prompt.push_str(
                "\nPrefer a tag from the vocabulary below. Do not invent a near-synonym of one \
                 that is already there, and copy a vocabulary tag's spelling exactly when you \
                 use it.\n",
            );
            prompt.push_str(&format!(
                "- At most {} tag(s) may be new, i.e. absent from the vocabulary. The rest must come from it.\n",
                settings.max_new_tags
            ));
            prompt.push_str(&format!(
                "\nVocabulary ({} tags, most used first):\n{}\n",
                vocabulary.len(),
                vocabulary.join(", ")
            ));
        }
    }
    if fields.rating {
        prompt.push_str(
            "- `rating`: an integer aesthetic score from 1 (lowest) to 5 (highest), or null.\n",
        );
    }

    if !fields.description && !fields.tags && !fields.rating {
        prompt.push_str("- Return an empty object when there is nothing to describe.\n");
    }

    prompt.push_str(&format!(
        "\nWrite every text value in {}.",
        language_name(language)
    ));
    prompt
}

/// The per-asset text handed to the model: the facts the library knows.
pub fn user_text_lines(req: &AiAnalysisRequest) -> Vec<String> {
    let mut lines = vec![
        "Asset metadata:".to_string(),
        format!("- Name: {}", req.display_name),
        format!("Filename: {}", req.file_name),
    ];
    lines.extend(req.metadata_lines.iter().cloned());

    if let Some(_sheet) = &req.contact_sheet_jpeg {
        if req.thumbnail_jpeg.is_some() {
            lines.push(
                "The first image is the asset and the second is a contact sheet of key frames; \
                 every frame carries its timestamp (HH:MM:SS.mmm) at the bottom right."
                    .to_string(),
            );
        } else {
            lines.push(
                "The supplied image is a contact sheet of key video frames; every frame carries \
                 its timestamp (HH:MM:SS.mmm) at the bottom right."
                    .to_string(),
            );
        }
    }

    if !req.existing_tag_names.is_empty() {
        lines.push(format!(
            "Already tagged: {}",
            req.existing_tag_names.join(", ")
        ));
    }
    lines
}

/// The unguessable facts the library mined from an asset — everything a
/// picture cannot show and a bad file name leaves out — as prompt lines.
pub fn asset_metadata_lines(asset: &Asset) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
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
    lines
}

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

// ============================ post-processing =============================

/// The boundary the tag tree is protected by. Everything a model said goes
/// through this before it reaches the store — a model that emits a 40-word
/// "tag", invents duplicates, or returns a description that runs to 4000
/// characters cannot pollute the asset record.
pub fn post_process(
    mut result: AiAnalysisResult,
    existing_tag_names: &[String],
    vocabulary: &[String],
    settings: &AiAnalysisSettings,
    language: &str,
) -> AiAnalysisResult {
    // Every spelling the library already knows — the asset's own tags and the
    // wider vocabulary — so a model that answers "Cat" reuses a stored "cat"
    // instead of creating a second tag.
    let known: std::collections::HashMap<String, String> = existing_tag_names
        .iter()
        .chain(vocabulary.iter())
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(|n| (n.to_lowercase(), n.to_string()))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut tags: Vec<String> = Vec::new();
    let mut invented: u32 = 0;
    for raw in result.tags {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > settings.max_tag_chars {
            continue;
        }
        if trimmed.split_whitespace().count() > MAX_TAG_WORDS {
            continue;
        }
        if trimmed.len() > MAX_NAME_LEN {
            continue;
        }
        let key = trimmed.to_lowercase();
        if !seen.insert(key.clone()) {
            continue;
        }
        let canonical = known.get(&key);
        if canonical.is_none() {
            // A word the library does not have: allowed only within the
            // budget, and never when the caller asked for reuse only.
            if settings.force_existing_tags || invented >= settings.max_new_tags {
                continue;
            }
            invented += 1;
        }
        tags.push(cloned_or_owned(canonical, trimmed));
        if tags.len() >= settings.max_tags {
            break;
        }
    }
    result.tags = tags;

    // Description: truncate on the right axis for the language.
    if let Some(desc) = result.description.take() {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            let language = language.to_lowercase();
            let truncated = if language.starts_with("zh") || language.starts_with("ja") {
                trimmed
                    .chars()
                    .take(settings.max_description_chars_zh)
                    .collect::<String>()
            } else {
                trimmed
                    .split_whitespace()
                    .take(settings.max_description_words_en)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            if !truncated.is_empty() {
                result.description = Some(truncated);
            }
        }
    }

    // Rating: clamp to 1..=5, reject 0.
    result.rating = result.rating.filter(|&r| (1..=5).contains(&r));

    result
}

fn cloned_or_owned(opt: Option<&String>, fallback: &str) -> String {
    match opt {
        Some(s) => s.clone(),
        None => fallback.to_string(),
    }
}

/// Parse a free-text model reply into an [`AiAnalysisResult`]. Tolerates
/// markdown fences and prose before/after the JSON object — every
/// mainstream model is tried before falling back to text.
pub fn parse_model_reply(text: &str, model_version: &str) -> Result<AiAnalysisResult> {
    let trimmed = text.trim();
    let unfenced = trimmed
        .trim_start_matches(|c: char| c == '`' || c.is_whitespace())
        .trim_end_matches(|c: char| c == '`' || c.is_whitespace());

    let value: serde_json::Value = serde_json::from_str(unfenced).or_else(|_| {
        // Try to find a JSON object inside the text.
        let start = unfenced
            .find('{')
            .ok_or_else(|| Error::Validation("model reply did not contain a JSON object".into()))?;
        let end = unfenced
            .rfind('}')
            .ok_or_else(|| Error::Validation("model reply did not contain a JSON object".into()))?;
        if end <= start {
            return Err(Error::Validation(
                "model reply did not contain a JSON object".into(),
            ));
        }
        serde_json::from_str(&unfenced[start..=end])
            .map_err(|e| Error::Validation(format!("model reply contained invalid JSON: {e}")))
    })?;

    let obj = value
        .as_object()
        .ok_or_else(|| Error::Validation("model reply was not a JSON object".into()))?;

    let description = obj.get("description").and_then(|v| match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        other => Some(other.to_string()),
    });

    let tags = obj
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let rating = obj.get("rating").and_then(|v| match v {
        serde_json::Value::Null => None,
        serde_json::Value::Number(n) => n.as_u64().map(|u| u as u8),
        serde_json::Value::String(s) => s.trim().parse::<u8>().ok(),
        _ => None,
    });

    Ok(AiAnalysisResult {
        description,
        tags,
        rating,
        model_version: model_version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request carrying everything the prompt builders read.
    fn request(
        enabled_fields: AiAnalysisFields,
        vocabulary: Vec<String>,
        existing: Vec<String>,
    ) -> AiAnalysisRequest {
        AiAnalysisRequest {
            asset_id: Uuid::new_v4(),
            display_name: "sunset.png".into(),
            file_name: "sunset.png".into(),
            mime: "image/png".into(),
            media_type: MediaType::Image,
            thumbnail_jpeg: None,
            contact_sheet_jpeg: None,
            language: "en".into(),
            enabled_fields,
            metadata_lines: Vec::new(),
            existing_tag_names: existing,
            vocabulary,
            settings: AiAnalysisSettings::default(),
        }
    }

    #[test]
    fn system_prompt_mentions_all_enabled_fields() {
        let prompt = system_prompt(&request(
            AiAnalysisFields {
                description: true,
                tags: true,
                rating: true,
            },
            Vec::new(),
            Vec::new(),
        ));
        assert!(prompt.contains("description"));
        assert!(prompt.contains("tags"));
        assert!(prompt.contains("rating"));
        assert!(prompt.contains("English"));
    }

    #[test]
    fn system_prompt_quotes_the_vocabulary_and_the_new_word_budget() {
        let prompt = system_prompt(&request(
            AiAnalysisFields::default(),
            vec!["beach".into(), "sunset".into()],
            Vec::new(),
        ));
        assert!(prompt.contains("beach, sunset"), "{prompt}");
        assert!(prompt.contains("At most 3 tag(s) may be new"));
    }

    #[test]
    fn user_text_includes_existing_tags() {
        let mut req = request(
            AiAnalysisFields::default(),
            Vec::new(),
            vec!["beach".into(), "trip".into()],
        );
        req.language = "zh-CN".into();
        let lines = user_text_lines(&req);
        let joined = lines.join("\n");
        assert!(joined.contains("Already tagged: beach, trip"));
    }

    #[test]
    fn post_process_caps_tags_and_reuses_vocabulary() {
        let settings = AiAnalysisSettings {
            max_tags: 2,
            max_new_tags: 3,
            max_tag_chars: 40,
            max_description_chars_zh: 200,
            max_description_words_en: 60,
            force_existing_tags: true,
        };
        let result = AiAnalysisResult {
            description: Some("A nice beach".into()),
            tags: vec!["Beach".into(), "ocean".into(), "sunset".into()],
            rating: Some(4),
            model_version: "test-model".into(),
        };
        let processed = post_process(result, &["beach".into()], &[], &settings, "en");
        assert_eq!(processed.tags, vec!["beach".to_string()]);
        assert!(processed.description.is_some());
        assert_eq!(processed.rating, Some(4));
    }

    #[test]
    fn post_process_enforces_the_new_word_budget() {
        // `max_new_tags: 1` admits the first unknown word and drops the rest,
        // while a vocabulary word is always allowed through.
        let settings = AiAnalysisSettings {
            max_new_tags: 1,
            ..AiAnalysisSettings::default()
        };
        let result = AiAnalysisResult {
            description: None,
            tags: vec!["ocean".into(), "sunset".into(), "beach".into()],
            rating: None,
            model_version: "test".into(),
        };
        let processed = post_process(result, &["beach".into()], &[], &settings, "en");
        assert_eq!(
            processed.tags,
            vec!["ocean".to_string(), "beach".to_string()]
        );
    }

    #[test]
    fn post_process_rejects_zero_rating_and_caps_five() {
        let settings = AiAnalysisSettings::default();
        let result = AiAnalysisResult {
            description: None,
            tags: vec![],
            rating: Some(0),
            model_version: "test".into(),
        };
        let processed = post_process(result, &[], &[], &settings, "en");
        assert_eq!(processed.rating, None);

        let result = AiAnalysisResult {
            description: None,
            tags: vec![],
            rating: Some(7),
            model_version: "test".into(),
        };
        let processed = post_process(result, &[], &[], &settings, "en");
        assert_eq!(processed.rating, None);
    }

    #[test]
    fn parse_model_reply_reads_clean_json() {
        let reply = r#"{"description": "a beach", "tags": ["ocean", "sand"], "rating": 4}"#;
        let result = parse_model_reply(reply, "test-model").unwrap();
        assert_eq!(result.description.as_deref(), Some("a beach"));
        assert_eq!(result.tags, vec!["ocean", "sand"]);
        assert_eq!(result.rating, Some(4));
    }

    #[test]
    fn parse_model_reply_sees_through_fences_and_prose() {
        let reply = r#"```json
        {"description": "a beach", "tags": ["ocean"]}
        ```
        Here are your tags."#;
        let result = parse_model_reply(reply, "test-model").unwrap();
        assert_eq!(result.description.as_deref(), Some("a beach"));
        assert_eq!(result.tags, vec!["ocean"]);
    }

    #[test]
    fn parse_model_reply_handles_nulls_and_missing_fields() {
        let reply = r#"{"description": null, "tags": []}"#;
        let result = parse_model_reply(reply, "test-model").unwrap();
        assert!(result.description.is_none());
        assert!(result.tags.is_empty());
        assert!(result.rating.is_none());
    }

    #[test]
    fn parse_model_reply_replies_without_json() {
        let reply = "Here are some tags for you: beach, ocean";
        assert!(parse_model_reply(reply, "test-model").is_err());
    }

    #[test]
    fn language_name_maps_locales() {
        assert_eq!(language_name("zh-CN"), "Simplified Chinese");
        assert_eq!(language_name("ja"), "Japanese");
        assert_eq!(language_name("en-US"), "English");
    }
}
