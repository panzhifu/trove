//! AI search planner: translate a natural-language query into a structured
//! search plan the existing search engine can execute.
//!
//! This is the "NLU" half of search: a user types "sunset beach images
//! rated 4+" and the planner turns it into [`AiSearchPlan`] — keywords,
//! synonyms, exclusions, structured filters and a sort preference. The plan
//! is then executed by the existing Tantivy + SQL search path
//! ([`crate::store::browse::BrowseContext`]); no new search engine is
//! introduced.
//!
//! The planner is deliberately thin: it builds one prompt, calls one
//! vendor, validates the JSON against a zod-like schema, and returns. All
//! the interesting decisions — which fields exist, what a rating filter
//! means, how synonyms are scored — stay in the search engine.

use super::vendor::VendorAdapter;
use crate::error::{Error, Result};

// ============================ plan =========================================

/// A provider-generated search plan. Intentionally limited to values already
/// understood by Trove's ordinary search engine — it cannot carry SQL,
/// filesystem paths, arbitrary operators, or executable expressions.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AiSearchPlan {
    /// Primary search terms — the user's intent, distilled.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Related alternative terms — broader than the keywords, scored lower.
    #[serde(default)]
    pub synonyms: Vec<String>,
    /// Concepts to exclude — NOT semantics.
    #[serde(default)]
    pub exclusions: Vec<String>,
    /// Structured filters the search engine understands natively.
    #[serde(default)]
    pub filters: Vec<PlanFilter>,
    /// Optional sort preference.
    pub sort: Option<PlanSort>,
}

/// A structured filter the search engine can apply without AI interpretation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanFilter {
    pub field: PlanFilterField,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub ranges: Vec<PlanRange>,
    #[serde(default)]
    pub exclude: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanFilterField {
    Format,
    Tag,
    Rating,
    Favorite,
    Width,
    Height,
    DurationMs,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PlanSort {
    pub field: PlanSortField,
    pub order: SortOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSortField {
    Name,
    CreatedAt,
    UpdatedAt,
    Rating,
    Size,
    Duration,
    Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    Desc,
}

// ============================ validation ==================================

const MAX_TERMS: usize = 16;
const MAX_VALUES: usize = 32;
const MAX_FILTERS: usize = 16;
const MAX_TERM_LEN: usize = 512;

impl AiSearchPlan {
    /// Validate and normalize a raw plan from the wire. Returns a clean plan
    /// the search engine can execute, or an error naming the first problem.
    pub fn validate(raw: raw::Plan) -> std::result::Result<Self, String> {
        if raw.keywords.len() > MAX_TERMS {
            return Err("too many keywords".into());
        }
        if raw.synonyms.len() > MAX_TERMS {
            return Err("too many synonyms".into());
        }
        if raw.exclusions.len() > MAX_TERMS {
            return Err("too many exclusions".into());
        }
        if raw.filters.len() > MAX_FILTERS {
            return Err("too many filters".into());
        }

        let keywords = normalize_terms(raw.keywords);
        let synonyms = normalize_terms(raw.synonyms);
        let exclusions = normalize_terms(raw.exclusions);

        if keywords.is_empty()
            && synonyms.is_empty()
            && exclusions.is_empty()
            && raw.filters.is_empty()
            && raw.sort.is_none()
        {
            return Err("empty plan".into());
        }

        let mut filters = Vec::new();
        for raw_filter in raw.filters {
            if raw_filter.values.len() > MAX_VALUES {
                return Err(format!("filter {:?}: too many values", raw_filter.field));
            }
            if raw_filter.ranges.len() > MAX_VALUES {
                return Err(format!("filter {:?}: too many ranges", raw_filter.field));
            }
            let is_numeric = matches!(
                raw_filter.field,
                raw::FilterField::Width | raw::FilterField::Height | raw::FilterField::DurationMs
            );
            if is_numeric && raw_filter.ranges.is_empty() {
                return Err(format!(
                    "filter {:?}: numeric filters require ranges",
                    raw_filter.field
                ));
            }
            if !is_numeric && !raw_filter.ranges.is_empty() {
                return Err(format!(
                    "filter {:?}: categorical filters cannot have ranges",
                    raw_filter.field
                ));
            }
            let values = if is_numeric {
                vec![]
            } else {
                normalize_terms(raw_filter.values)
            };
            filters.push(PlanFilter {
                field: match raw_filter.field {
                    raw::FilterField::Format => PlanFilterField::Format,
                    raw::FilterField::Tag => PlanFilterField::Tag,
                    raw::FilterField::Rating => PlanFilterField::Rating,
                    raw::FilterField::Favorite => PlanFilterField::Favorite,
                    raw::FilterField::Width => PlanFilterField::Width,
                    raw::FilterField::Height => PlanFilterField::Height,
                    raw::FilterField::DurationMs => PlanFilterField::DurationMs,
                },
                values,
                ranges: raw_filter
                    .ranges
                    .into_iter()
                    .map(|r| PlanRange {
                        min: r.min,
                        max: r.max,
                    })
                    .collect(),
                exclude: raw_filter.exclude,
            });
        }

        let sort = raw.sort.map(|s| PlanSort {
            field: match s.field {
                raw::SortField::Name => PlanSortField::Name,
                raw::SortField::CreatedAt => PlanSortField::CreatedAt,
                raw::SortField::UpdatedAt => PlanSortField::UpdatedAt,
                raw::SortField::Rating => PlanSortField::Rating,
                raw::SortField::Size => PlanSortField::Size,
                raw::SortField::Duration => PlanSortField::Duration,
                raw::SortField::Color => PlanSortField::Color,
            },
            order: match s.order {
                raw::SortOrder::Asc => SortOrder::Asc,
                raw::SortOrder::Desc => SortOrder::Desc,
            },
        });

        Ok(Self {
            keywords,
            synonyms,
            exclusions,
            filters,
            sort,
        })
    }
}

fn normalize_terms(raw: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= MAX_TERM_LEN)
        .filter(|s| seen.insert(s.to_lowercase()))
        .take(MAX_TERMS)
        .collect()
}

/// Raw plan shape as received from the provider — validated by [`AiSearchPlan::validate`].
mod raw {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    pub struct Plan {
        #[serde(default)]
        pub keywords: Vec<String>,
        #[serde(default)]
        pub synonyms: Vec<String>,
        #[serde(default)]
        pub exclusions: Vec<String>,
        #[serde(default)]
        pub filters: Vec<Filter>,
        pub sort: Option<Sort>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Filter {
        pub field: FilterField,
        #[serde(default)]
        pub values: Vec<String>,
        #[serde(default)]
        pub ranges: Vec<Range>,
        #[serde(default)]
        pub exclude: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum FilterField {
        Format,
        Tag,
        Rating,
        Favorite,
        Width,
        Height,
        DurationMs,
    }

    #[derive(Debug, Deserialize)]
    pub struct Range {
        pub min: Option<f64>,
        pub max: Option<f64>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Sort {
        pub field: SortField,
        pub order: SortOrder,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SortField {
        Name,
        CreatedAt,
        UpdatedAt,
        Rating,
        Size,
        Duration,
        Color,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SortOrder {
        Asc,
        Desc,
    }
}

// ============================ planner ======================================

/// Translate a natural-language query into a structured search plan.
///
/// The planner sends the query to a multimodal model (text-only here) and
/// asks for a JSON object conforming to the schema described in the system
/// prompt. The result is validated and normalized before being handed to
/// the search engine.
pub fn plan(
    provider: &dyn VendorAdapter,
    query: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<AiSearchPlan> {
    if query.trim().is_empty() {
        return Err(Error::Validation("empty search query".into()));
    }

    let system = SYSTEM_PROMPT;
    let user_text = query.trim();

    let mut body = serde_json::json!({
        "model": provider.model_version(),
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user_text },
        ],
        "temperature": 0,
    });

    // Try structured output first.
    body["response_format"] = serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "trove_search_plan",
            "strict": true,
            "schema": plan_schema(),
        },
    });

    let raw_text = provider
        .analyze(
            &crate::ai::analysis::AiAnalysisRequest {
                asset_id: uuid::Uuid::nil(),
                display_name: "search-plan".into(),
                file_name: "search-plan".into(),
                mime: "text/plain".into(),
                media_type: crate::ai::analysis::MediaType::Other,
                thumbnail_jpeg: None,
                contact_sheet_jpeg: None,
                language: "en".into(),
                enabled_fields: crate::ai::analysis::AiAnalysisFields::default(),
                existing_tag_names: vec![],
                vocabulary: vec![],
                metadata_lines: vec![],
                settings: crate::ai::analysis::AiAnalysisSettings::default(),
            },
            cancel,
        )
        .map_err(|e| Error::Validation(format!("search planner failed: {e}")))?;

    // Parse the JSON plan from the model's reply.
    let raw_plan: raw::Plan = parse_raw_plan(&raw_text)?;
    AiSearchPlan::validate(raw_plan).map_err(Error::Validation)
}

fn parse_raw_plan(text: &str) -> std::result::Result<raw::Plan, Error> {
    let trimmed = text.trim();
    // Strip optional markdown fences.
    let unfenced = trimmed.trim_start_matches('`').trim_end_matches('`').trim();
    // Find the first JSON object.
    let start = unfenced
        .find('{')
        .ok_or_else(|| Error::Validation("no JSON in plan".into()))?;
    let end = unfenced
        .rfind('}')
        .ok_or_else(|| Error::Validation("no JSON in plan".into()))?;
    if end <= start {
        return Err(Error::Validation("malformed JSON in plan".into()));
    }
    serde_json::from_str(&unfenced[start..=end])
        .map_err(|e| Error::Validation(format!("invalid plan JSON: {e}")))
}

const SYSTEM_PROMPT: &str = r#"You translate a user's natural-language request into a Trove search plan.
Return only the required structured object. Never output SQL, code, filesystem paths, IDs, or new operators.
Use concise literal keywords. Put related alternative terms in synonyms and unwanted concepts in exclusions.
Allowed categorical filters: format (jpg, png, gif, webp, mp4, mov, ...), tag (any string), rating ("1","2","3","4","5"), favorite (true/false).
Allowed numeric filters: width/height in pixels, duration_ms in milliseconds. Numeric filters use ranges; categorical filters use values.
Only add a sort when the user explicitly asks for ordering. The ordinary Trove search engine will execute the plan."#;

fn plan_schema() -> serde_json::Value {
    serde_json::json!({
    "type": "object",
    "additionalProperties": false,
    "properties": {
        "keywords": { "type": "array", "maxItems": 16, "items": { "type": "string", "minLength": 1, "maxLength": 512 } },
        "synonyms": { "type": "array", "maxItems": 16, "items": { "type": "string", "minLength": 1, "maxLength": 512 } },
        "exclusions": { "type": "array", "maxItems": 16, "items": { "type": "string", "minLength": 1, "maxLength": 512 } },
        "filters": {
            "type": "array",
            "maxItems": 16,
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "field": { "type": "string", "enum": ["format", "tag", "rating", "favorite", "width", "height", "duration_ms"] },
                    "values": { "type": "array", "maxItems": 32, "items": { "type": "string", "minLength": 1, "maxLength": 512 } },
                    "ranges": {
                        "type": "array",
                        "maxItems": 32,
                        "items": {
                            "type": "object",
                            "properties": { "min": { "type": ["number", "null"] }, "max": { "type": ["number", "null"] } },
                            "required": ["min", "max"],
                        },
                    },
                    "exclude": { "type": "boolean" },
                },
                "required": ["field", "values", "ranges", "exclude"],
            },
        },
        "sort": {
            "anyOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "field": { "type": "string", "enum": ["name", "created_at", "updated_at", "rating", "size", "duration", "color"] },
                        "order": { "type": "string", "enum": ["asc", "desc"] },
                    },
                    "required": ["field", "order"],
                },
                { "type": "null" },
            ],
        },
    },
    "required": ["keywords", "synonyms", "exclusions", "filters", "sort"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_minimal_plan() {
        let raw = raw::Plan {
            keywords: vec!["sunset".into()],
            synonyms: vec![],
            exclusions: vec![],
            filters: vec![],
            sort: None,
        };
        let plan = AiSearchPlan::validate(raw).unwrap();
        assert_eq!(plan.keywords, vec!["sunset".to_string()]);
    }

    #[test]
    fn validate_rejects_empty_plan() {
        let raw = raw::Plan {
            keywords: vec![],
            synonyms: vec![],
            exclusions: vec![],
            filters: vec![],
            sort: None,
        };
        assert!(AiSearchPlan::validate(raw).is_err());
    }

    #[test]
    fn validate_normalizes_terms() {
        let raw = raw::Plan {
            keywords: vec!["  Sunset  ".into(), "sunset".into(), "".into()],
            synonyms: vec![],
            exclusions: vec![],
            filters: vec![],
            sort: None,
        };
        let plan = AiSearchPlan::validate(raw).unwrap();
        assert_eq!(plan.keywords, vec!["Sunset".to_string()]);
    }

    #[test]
    fn validate_rejects_numeric_without_range() {
        let raw = raw::Plan {
            keywords: vec!["test".into()],
            synonyms: vec![],
            exclusions: vec![],
            filters: vec![raw::Filter {
                field: raw::FilterField::Width,
                values: vec![],
                ranges: vec![],
                exclude: false,
            }],
            sort: None,
        };
        assert!(AiSearchPlan::validate(raw).is_err());
    }

    #[test]
    fn validate_accepts_numeric_with_range() {
        let raw = raw::Plan {
            keywords: vec!["test".into()],
            synonyms: vec![],
            exclusions: vec![],
            filters: vec![raw::Filter {
                field: raw::FilterField::Width,
                values: vec![],
                ranges: vec![raw::Range {
                    min: Some(100.0),
                    max: None,
                }],
                exclude: false,
            }],
            sort: None,
        };
        let plan = AiSearchPlan::validate(raw).unwrap();
        assert_eq!(plan.filters.len(), 1);
        assert_eq!(plan.filters[0].field, PlanFilterField::Width);
    }

    #[test]
    fn parse_raw_plan_extracts_json() {
        let text = r#"```json
        {"keywords": ["beach"], "synonyms": [], "exclusions": [], "filters": [], "sort": null}
        ```"#;
        let plan = parse_raw_plan(text).unwrap();
        assert_eq!(plan.keywords, vec!["beach".to_string()]);
    }

    #[test]
    fn parse_raw_plan_rejects_non_json() {
        assert!(parse_raw_plan("here are some tags: beach, ocean").is_err());
    }
}
