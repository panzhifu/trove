//! Process plumbing shared by every command: the output contract, the
//! exit-code mapping, and opening the library.
//!
//! The output contract is the point of this whole binary, so it is stated
//! once here: **stdout carries exactly one JSON document** — the command's
//! result — and **stderr carries diagnostics**, notes normally and a single
//! `{"error":…}` document when the command fails. A caller can therefore
//! parse stdout unconditionally and only read the exit status to decide
//! whether the parse is worth attempting.

use std::io::IsTerminal;
use std::path::PathBuf;

use serde_json::{Value, json};
use trove_core::config::{AppConfig, LibraryEntry};
use trove_core::library::Library;
use trove_core::model::{Asset, Collection, Tag};
use uuid::Uuid;

use crate::cli::Cli;

/// A usage mistake. Matches clap's own code for the same reason.
const EXIT_USAGE: i32 = 2;
/// The library is not usable: absent, an unreadable schema version, or held
/// open by a process that a writing command needs to displace. Kept distinct
/// because it is the one failure a caller can act on.
const EXIT_LIBRARY: i32 = 3;
const EXIT_FAILURE: i32 = 1;

#[derive(Debug)]
pub struct CliError {
    kind: &'static str,
    message: String,
    code: i32,
}

impl CliError {
    fn new(kind: &'static str, code: i32, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("usage", EXIT_USAGE, message)
    }

    pub fn library(message: impl Into<String>) -> Self {
        Self::new("library", EXIT_LIBRARY, message)
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self::new("runtime", EXIT_FAILURE, message)
    }

    pub fn code(&self) -> i32 {
        self.code
    }

    /// Print the failure. A person watching a terminal gets a sentence; a
    /// program reading a pipe gets the JSON form.
    pub fn report(&self) {
        if std::io::stderr().is_terminal() {
            eprintln!("trove: {}: {}", self.kind, self.message);
        } else {
            eprintln!(
                "{}",
                json!({ "error": { "kind": self.kind, "message": self.message } })
            );
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl From<trove_core::Error> for CliError {
    fn from(error: trove_core::Error) -> Self {
        Self::runtime(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::runtime(format!("cannot encode the result: {error}"))
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// A command's result: the JSON document that goes to stdout, and the table
/// `--human` swaps in. Both are built together so the two views can never
/// disagree about what happened.
pub struct Rendered {
    pub json: Value,
    pub human: String,
}

impl Rendered {
    pub fn new(json: Value, human: impl Into<String>) -> Self {
        Self {
            json,
            human: human.into(),
        }
    }
}

/// How to render results, fixed once at startup.
pub struct Style {
    human: bool,
    compact: bool,
    quiet: bool,
}

impl Style {
    pub fn new(args: &Cli) -> Self {
        Self {
            human: args.human,
            compact: args.compact,
            quiet: args.quiet,
        }
    }

    pub fn emit(&self, rendered: &Rendered) -> Result<(), CliError> {
        let text = if self.human {
            rendered.human.clone()
        } else if self.compact {
            serde_json::to_string(&rendered.json)?
        } else {
            serde_json::to_string_pretty(&rendered.json)?
        };
        println!("{text}");
        Ok(())
    }

    /// A note for whoever is watching. Never part of the JSON contract, so it
    /// goes to stderr and `--quiet` silences it.
    pub fn note(&self, message: &str) {
        if !self.quiet {
            eprintln!("trove: {message}");
        }
    }

    /// A line about work in progress. Only when stderr is a terminal: a
    /// script does not want it, and interleaving it with the error document a
    /// caller may be parsing would be worse than useless.
    pub fn progress(&self, message: &str) {
        if !self.quiet && std::io::stderr().is_terminal() {
            eprintln!("trove: {message}");
        }
    }
}

// ---------------------------------------------------------------------------
// The open library
// ---------------------------------------------------------------------------

/// An open library plus the facts about it that outlive a single command.
pub struct Env {
    pub library: Library,
    pub slug: String,
    pub name: String,
    pub data_root: PathBuf,
    pub cache_root: PathBuf,
    /// Whether this process owns the search index writer. `false` means the
    /// desktop app has the library open and index writes are unavailable.
    pub index_writable: bool,
}

impl Env {
    /// Open the library named on the command line (or the active one).
    ///
    /// Always the read-only form: a query must work while the app holds the
    /// library open, and a command that needs to write reports
    /// [`Env::index_writable`] as `false` rather than refusing to start.
    pub fn open(args: &Cli) -> Result<Self, CliError> {
        let config = AppConfig::load();
        let entry = resolve_entry(&config, args.library.as_deref())?;
        let data_root = entry.dir();
        let cache_root = entry.cache_dir();
        let library = Library::open_read_only(&data_root, &cache_root).map_err(|error| {
            CliError::library(format!("cannot open library '{}': {error}", entry.slug))
        })?;
        let index_writable = library.text_index().is_writable();
        Ok(Self {
            library,
            slug: entry.slug,
            name: entry.name,
            data_root,
            cache_root,
            index_writable,
        })
    }
}

pub(crate) fn resolve_entry(
    config: &AppConfig,
    slug: Option<&str>,
) -> Result<LibraryEntry, CliError> {
    if let Some(slug) = slug {
        return config
            .libraries
            .iter()
            .find(|entry| entry.slug == slug)
            .cloned()
            .ok_or_else(|| {
                CliError::usage(format!(
                    "no library with slug '{slug}'; `trove libraries` lists them"
                ))
            });
    }
    if config.libraries.is_empty() {
        return Err(CliError::library(
            "no library is configured yet; start the Trove desktop app once to create one",
        ));
    }
    Ok(config.active_entry())
}

// ---------------------------------------------------------------------------
// Resolving names to ids
// ---------------------------------------------------------------------------

/// Parse one asset id, with the message a typo deserves.
pub fn parse_asset_id(raw: &str) -> Result<Uuid, CliError> {
    Uuid::parse_str(raw.trim()).map_err(|_| {
        CliError::usage(format!(
            "'{raw}' is not an asset id; ids are the UUIDs `list` and `search` print"
        ))
    })
}

pub fn parse_asset_ids(raw: &[String]) -> Result<Vec<Uuid>, CliError> {
    raw.iter().map(|raw| parse_asset_id(raw)).collect()
}

/// Resolve `--tag NAME|UUID` to tag ids, expanding a parent into its subtree.
///
/// The expansion is the app's own rule (filtering by a parent includes what
/// is filed under it), and it is the reason this cannot be a plain name
/// lookup: the caller gets the same set of assets the UI would show.
pub fn resolve_tags(env: &Env, names: &[String]) -> Result<Vec<Uuid>, CliError> {
    let conn = env.library.store().conn();
    let all = trove_core::store::tags::list(conn)?;
    let mut ids = Vec::new();
    for raw in names {
        let tag = find_tag(&all, raw).ok_or_else(|| {
            CliError::usage(format!("no tag named '{raw}'; `trove tags` lists them"))
        })?;
        ids.extend(trove_core::store::tags::subtree_ids(conn, tag.id)?);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn find_tag<'a>(all: &'a [Tag], raw: &str) -> Option<&'a Tag> {
    let raw = raw.trim();
    if let Ok(id) = Uuid::parse_str(raw) {
        return all.iter().find(|tag| tag.id == id);
    }
    all.iter().find(|tag| tag.name.eq_ignore_ascii_case(raw))
}

/// Resolve a collection by name or UUID.
pub fn resolve_collection(env: &Env, raw: &str) -> Result<Collection, CliError> {
    let all = trove_core::store::collections::list(env.library.store().conn())?;
    let raw = raw.trim();
    if let Ok(id) = Uuid::parse_str(raw)
        && let Some(found) = all.iter().find(|collection| collection.id == id)
    {
        return Ok(found.clone());
    }
    all.iter()
        .find(|collection| collection.name.eq_ignore_ascii_case(raw))
        .cloned()
        .ok_or_else(|| {
            CliError::usage(format!(
                "no collection named '{raw}'; `trove collections` lists them"
            ))
        })
}

// ---------------------------------------------------------------------------
// Asset views
// ---------------------------------------------------------------------------

/// The compact view `list` and `search` return per asset.
///
/// Deliberately not the whole record: a listing of 50 assets should stay
/// readable, and the fields here are the ones that decide what to do next.
/// `file_path` is included because it is the single most useful derived
/// value — the absolute path of the file, ready for another program.
pub fn asset_summary(env: &Env, asset: &Asset) -> Value {
    json!({
        "id": asset.id,
        "file_name": asset.file_name,
        "file_path": env
            .library
            .asset_file(asset.id)
            .map(|path| path.display().to_string()),
        "kind": asset.kind,
        "ext": asset.ext,
        "mime": asset.mime,
        "size_bytes": asset.size_bytes,
        "width": asset.width,
        "height": asset.height,
        "duration_ms": asset.duration_ms,
        "title": asset.title,
        "rating": asset.rating,
        "is_favorite": asset.is_favorite,
        "usage_status": asset.usage_status,
        "trashed": asset.trashed_at.is_some(),
        "origin": asset.origin,
        "created_at": asset.created_at,
    })
}

/// The full record `get` returns: the stored row, plus the associations that
/// save a follow-up query (tags and collections by name) and the file path.
pub fn asset_detail(env: &Env, asset: &Asset) -> Result<Value, CliError> {
    let conn = env.library.store().conn();
    let tags: Vec<String> = trove_core::store::tags::for_asset(conn, asset.id)?
        .into_iter()
        .map(|tag| tag.name)
        .collect();
    let collections: Vec<String> = trove_core::store::collections::for_asset(conn, asset.id)?
        .into_iter()
        .map(|collection| collection.name)
        .collect();

    let mut value = serde_json::to_value(asset)?;
    let object = value
        .as_object_mut()
        .expect("an asset always serializes to a JSON object");
    object.insert(
        "file_path".into(),
        match env.library.asset_file(asset.id) {
            Some(path) => json!(path.display().to_string()),
            None => Value::Null,
        },
    );
    object.insert("tags".into(), json!(tags));
    object.insert("collections".into(), json!(collections));
    Ok(value)
}
