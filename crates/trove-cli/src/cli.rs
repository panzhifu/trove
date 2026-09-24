//! The command surface — every flag the binary accepts, and nothing else.
//!
//! Kept apart from the implementations for one reason: `--help` is the
//! contract. The desktop app can be learned by clicking around; a CLI that
//! scripts and agents drive has to state its whole surface up front, so the
//! doc comments here are written for that reader (one line of purpose, then
//! the caveats that actually change a decision).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use trove_core::model::{
    AspectPreset, AssetKind, AssetSort, Orientation, ResolutionBand, UsageStatus,
};

/// Query and edit a Trove library from the command line.
///
/// Every command prints one JSON document to stdout and exits 0. Errors go to
/// stderr as `{"error":{"kind":…,"message":…}}` with a non-zero exit status:
/// 2 for a usage mistake, 3 when the library cannot be opened (absent, an
/// unusable schema version, or — for a command that needs to write — already
/// held open by the desktop app), and 1 for everything else.
///
/// The library operated on is the one the desktop app has active; `--library`
/// picks another by slug, and `trove libraries` lists them. Queries work
/// while the app is running: the library is then opened read-only, so a
/// search answers from the index as the app last left it.
#[derive(Debug, Parser)]
#[command(
    name = "trove",
    version,
    about = "Query and edit a Trove library from the command line",
    propagate_version = true,
    disable_help_subcommand = true,
    max_term_width = 100
)]
pub struct Cli {
    /// Library slug to work on (default: the active library).
    #[arg(long, short = 'l', global = true, value_name = "SLUG")]
    pub library: Option<String>,

    /// Print tables instead of JSON.
    #[arg(long, global = true, conflicts_with = "compact")]
    pub human: bool,

    /// Print JSON on a single line.
    #[arg(long, global = true)]
    pub compact: bool,

    /// Keep stderr quiet: errors only, no notes.
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the libraries configured on this machine.
    ///
    /// Reads the configuration only — no database is opened, so this still
    /// works when a library is missing or refuses to open.
    Libraries,

    /// Show what a library holds: counts, sizes, and index state.
    Info,

    /// Print the directories this library uses.
    ///
    /// Reads the configuration only, like `libraries`.
    Paths,

    /// List assets — the general "what is in here" question.
    List(ListArgs),

    /// Search names, titles, descriptions and tags, best match first.
    ///
    /// CJK-aware (word + n-gram) and pinyin-aware, so `mao` finds 猫. Every
    /// `list` filter still applies, but results are ordered by relevance and
    /// `--sort` is ignored.
    Search(SearchArgs),

    /// Print the full record of one or more assets.
    ///
    /// Each record carries `file_path`: the absolute path of the file on
    /// disk, ready to be handed to another program.
    Get(GetArgs),

    /// List tags with the number of assets each one holds.
    Tags,

    /// List collections, shallowest first, plus smart collections.
    Collections,

    /// Group live images that look alike.
    ///
    /// Grouping uses the visual signatures computed after import; assets
    /// imported before that feature existed carry none and are not grouped.
    Duplicates,

    /// List the folders this library's files were imported from.
    Folders,

    /// Check that the library and the helpers it depends on are usable.
    Doctor,

    /// Import files or directories.
    Import(ImportArgs),

    /// Analyse assets with a multimodal model.
    ///
    /// Each asset's thumbnail and metadata are handed to the configured
    /// vendor (OpenAI / Anthropic / Gemini / DashScope); the description,
    /// tags and rating it returns are written back. Tags already in the
    /// library are offered to the model to reuse, and the few it invents are
    /// filed under a parent tag so the whole run can be reviewed — or thrown
    /// away with `--undo`. Assets a previous run already described are
    /// skipped, so running it twice costs nothing the second time.
    Analyze(AnalyzeArgs),

    /// Edit an asset's metadata.
    Set(SetArgs),

    /// Add or remove tags on assets.
    Tag(TagArgs),

    /// Move assets to the trash. Reversible, and deletes no files.
    Trash(Ids),

    /// Restore assets from the trash.
    Restore(Ids),

    /// Permanently delete assets, including files the library owns.
    ///
    /// Irreversible. Files the library merely links are left alone unless
    /// they live in Trove's own inbox. Refuses to run without `--yes`.
    Purge(PurgeArgs),

    /// Create, rename and delete collections, and move assets in and out.
    #[command(subcommand)]
    Collection(CollectionCommand),

    /// Inspect or rebuild the full-text search index.
    #[command(subcommand)]
    Index(IndexCommand),
}

// ---------------------------------------------------------------------------
// Shared argument shapes
// ---------------------------------------------------------------------------

/// The filters `list` and `search` share.
#[derive(Debug, Args)]
pub struct FilterArgs {
    /// Only assets of this kind.
    #[arg(long, value_enum, value_name = "KIND")]
    pub kind: Option<KindArg>,

    /// Only assets carrying all of these tags (name or UUID, repeatable).
    #[arg(long, value_name = "TAG")]
    pub tag: Vec<String>,

    /// Only assets in this collection (name or UUID).
    #[arg(long, value_name = "COLLECTION")]
    pub collection: Option<String>,

    /// Only favourites.
    #[arg(long)]
    pub favorite: bool,

    /// Only assets rated at least this high.
    #[arg(long, value_name = "1-5")]
    pub min_rating: Option<u8>,

    /// Only this file extension, without the dot (`png`, `mp4`).
    #[arg(long, value_name = "EXT")]
    pub ext: Option<String>,

    /// Only images with this orientation.
    #[arg(long, value_enum, value_name = "ORIENTATION")]
    pub orientation: Option<OrientationArg>,

    /// Only assets whose dimensions match this media preset.
    #[arg(long, value_enum, value_name = "PRESET")]
    pub aspect: Option<AspectArg>,

    /// Only assets whose longer edge falls in this resolution band.
    #[arg(long, value_enum, value_name = "BAND")]
    pub resolution: Option<ResolutionArg>,

    /// Only assets whose recorded source path starts with this prefix.
    #[arg(long, value_name = "PREFIX")]
    pub folder: Option<String>,

    /// Only assets in this usage state.
    #[arg(long, value_enum, value_name = "STATE")]
    pub usage: Option<UsageArg>,

    /// Look in the trash instead of the live library.
    #[arg(long)]
    pub trashed: bool,

    /// Sort key. Ignored by `search`, which is always relevance-ordered.
    #[arg(long, value_enum, default_value = "created")]
    pub sort: SortArg,

    /// Sort ascending instead of descending.
    #[arg(long)]
    pub asc: bool,

    /// Maximum number of records to return (the store refuses a window wider
    /// than 20000).
    #[arg(long, short = 'n', default_value_t = 50, value_name = "N")]
    pub limit: u32,

    /// Records to skip. Page a large result with `--offset`.
    #[arg(long, default_value_t = 0, value_name = "N")]
    pub offset: u64,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub filter: FilterArgs,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Words to look for; separate arguments are joined, and they are AND-ed.
    #[arg(required = true, value_name = "WORDS")]
    pub query: Vec<String>,

    #[command(flatten)]
    pub filter: FilterArgs,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// Asset UUIDs, exactly as `list` or `search` printed them.
    #[arg(required = true, value_name = "UUID")]
    pub ids: Vec<String>,
}

/// A bare list of asset UUIDs.
#[derive(Debug, Args)]
pub struct Ids {
    #[arg(required = true, value_name = "UUID")]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {
    #[arg(required = true, value_name = "UUID")]
    pub ids: Vec<String>,

    /// Confirm the permanent deletion. Without it nothing is deleted.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Files or directories to import.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Copy the files into the library instead of linking to them.
    ///
    /// The default is a link: the file stays where it is and the record
    /// points at it. Copy makes the library own its own copy, which is what
    /// a source that is about to disappear needs (a camera card, a download
    /// directory, a temporary export).
    #[arg(long)]
    pub copy: bool,

    /// Put the imported assets in this collection (name or UUID).
    #[arg(long, value_name = "COLLECTION")]
    pub into: Option<String>,

    /// Report what would be imported without importing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// What `analyze` runs with. Everything is optional: what is left out comes
/// from the library's stored analysis configuration, which is also where the
/// vendor, the endpoint and the model live.
#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Tag only these assets. Omit for every live asset.
    #[arg(long, value_name = "UUID")]
    pub ids: Vec<String>,

    /// Stop after this many assets.
    #[arg(long, short = 'n', value_name = "N")]
    pub limit: Option<u64>,

    /// Report what would be tagged and stop. No request is sent, nothing is
    /// written — not even an empty tag.
    #[arg(long)]
    pub dry_run: bool,

    /// Re-analyse assets a previous run already described, instead of
    /// skipping them.
    #[arg(long)]
    pub force: bool,

    /// Send text only, even when the endpoint would take an image. Worth
    /// setting for a text-only model: without it the first request discovers
    /// that by being refused.
    #[arg(long, conflicts_with = "with_images")]
    pub no_images: bool,

    /// Send the asset's thumbnail as well, overriding a stored
    /// `send_images: false`.
    #[arg(long, conflicts_with = "no_images")]
    pub with_images: bool,

    /// How many tags per asset the model may invent. `0` restricts it to the
    /// library's existing vocabulary.
    #[arg(long, value_name = "N")]
    pub max_new_tags: Option<u32>,

    /// Ask for a description too (overrides the stored `fields`).
    #[arg(long, conflicts_with = "no_description")]
    pub description: bool,

    /// Do not ask for a description.
    #[arg(long)]
    pub no_description: bool,

    /// Ask for an aesthetic rating too.
    #[arg(long, conflicts_with = "no_rating")]
    pub rating: bool,

    /// Do not ask for a rating.
    #[arg(long)]
    pub no_rating: bool,

    /// Parent tag for the invented ones; pass an empty string to file them at
    /// the root.
    #[arg(long, value_name = "TAG")]
    pub parent_tag: Option<String>,

    /// Language to write tags in (`zh-CN`, `en`, …); defaults to the
    /// library's interface language.
    #[arg(long, value_name = "LOCALE")]
    pub language: Option<String>,

    /// Requests in flight. Four by default; one for a rate-limited endpoint.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Detach everything previous runs added, and forget they ran.
    ///
    /// Needs no endpoint: taking tags back asks no model anything. Tags left
    /// without assets are reported, never deleted. Descriptions and ratings
    /// are left in place.
    #[arg(long, conflicts_with_all = ["dry_run", "force", "no_images", "with_images", "max_new_tags", "parent_tag", "limit", "description", "no_description", "rating", "no_rating"])]
    pub undo: bool,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    #[arg(required = true, value_name = "UUID")]
    pub ids: Vec<String>,

    /// Set the title.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,
    /// Clear the title.
    #[arg(long, conflicts_with = "title")]
    pub clear_title: bool,

    /// Set the description.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,
    /// Clear the description.
    #[arg(long, conflicts_with = "description")]
    pub clear_description: bool,

    /// Set the star rating.
    #[arg(long, value_name = "1-5")]
    pub rating: Option<u8>,
    /// Clear the star rating.
    #[arg(long, conflicts_with = "rating")]
    pub clear_rating: bool,

    /// Mark as a favourite.
    #[arg(long, conflicts_with = "no_favorite")]
    pub favorite: bool,
    /// Clear the favourite mark.
    #[arg(long)]
    pub no_favorite: bool,

    /// Set the source URL.
    #[arg(long, value_name = "URL")]
    pub source_url: Option<String>,
    /// Clear the source URL.
    #[arg(long, conflicts_with = "source_url")]
    pub clear_source_url: bool,

    /// Set the usage state.
    #[arg(long, value_enum, value_name = "STATE")]
    pub usage: Option<UsageArg>,
}

#[derive(Debug, Args)]
pub struct TagArgs {
    #[arg(required = true, value_name = "UUID")]
    pub ids: Vec<String>,

    /// Tags to add; missing ones are created under the library root.
    #[arg(long, value_name = "TAG")]
    pub add: Vec<String>,

    /// Tags to remove. Unknown names are ignored, not created.
    #[arg(long, value_name = "TAG")]
    pub remove: Vec<String>,

    /// Replace the whole tag set with exactly these tags.
    #[arg(long, value_name = "TAG", conflicts_with_all = ["add", "remove"])]
    pub set: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum CollectionCommand {
    /// List collections. Same output as `trove collections`.
    List,

    /// Create a collection.
    Create {
        /// Name of the new collection.
        name: String,
        /// Parent collection (name or UUID); omit for a top-level one.
        #[arg(long, value_name = "COLLECTION")]
        parent: Option<String>,
    },

    /// Rename a collection.
    Rename {
        /// Collection to rename (name or UUID).
        collection: String,
        /// New name.
        name: String,
    },

    /// Delete a collection. The assets stay in the library.
    Rm {
        /// Collection to delete (name or UUID).
        collection: String,
    },

    /// Add assets to a collection.
    Add {
        /// Collection (name or UUID).
        collection: String,
        #[arg(required = true, value_name = "UUID")]
        assets: Vec<String>,
    },

    /// Remove assets from a collection.
    Remove {
        /// Collection (name or UUID).
        collection: String,
        #[arg(required = true, value_name = "UUID")]
        assets: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    /// Show indexed document count, pending outbox rows, and whether this
    /// process owns the writer.
    Status,

    /// Rebuild the index from the database.
    ///
    /// Needs the writer lock, so the desktop app has to be closed first. The
    /// index is a pure derivative of the database: this loses nothing but
    /// time.
    Rebuild,
}

// ---------------------------------------------------------------------------
// Value enums — the CLI's own spelling of the model's closed sets, so the
// flags stay stable even if the model renames a variant.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Font,
    Model,
    Other,
}

impl From<KindArg> for AssetKind {
    fn from(value: KindArg) -> Self {
        match value {
            KindArg::Image => AssetKind::Image,
            KindArg::Video => AssetKind::Video,
            KindArg::Audio => AssetKind::Audio,
            KindArg::Document => AssetKind::Document,
            KindArg::Archive => AssetKind::Archive,
            KindArg::Font => AssetKind::Font,
            KindArg::Model => AssetKind::Model,
            KindArg::Other => AssetKind::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OrientationArg {
    Landscape,
    Portrait,
    Square,
}

impl From<OrientationArg> for Orientation {
    fn from(value: OrientationArg) -> Self {
        match value {
            OrientationArg::Landscape => Orientation::Landscape,
            OrientationArg::Portrait => Orientation::Portrait,
            OrientationArg::Square => Orientation::Square,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AspectArg {
    /// WeChat Official Account cover canvas (2.35:1).
    WechatCover,
    /// Wide video cover (16:9).
    VideoWide,
    /// Vertical short-video canvas (9:16).
    VideoVertical,
    /// Classic photo landscape (4:3).
    PhotoLandscape,
    /// Classic photo portrait (3:4).
    PhotoPortrait,
    /// Square canvas (1:1).
    Square,
}

impl From<AspectArg> for AspectPreset {
    fn from(value: AspectArg) -> Self {
        match value {
            AspectArg::WechatCover => AspectPreset::WechatCover,
            AspectArg::VideoWide => AspectPreset::VideoWide,
            AspectArg::VideoVertical => AspectPreset::VideoVertical,
            AspectArg::PhotoLandscape => AspectPreset::PhotoLandscape,
            AspectArg::PhotoPortrait => AspectPreset::PhotoPortrait,
            AspectArg::Square => AspectPreset::Square,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ResolutionArg {
    /// Longer edge under 2240 px.
    #[value(name = "1k")]
    OneK,
    /// 2240–3199 px.
    #[value(name = "2k")]
    TwoK,
    /// 3200 px and up.
    #[value(name = "4k")]
    FourK,
}

impl From<ResolutionArg> for ResolutionBand {
    fn from(value: ResolutionArg) -> Self {
        match value {
            ResolutionArg::OneK => ResolutionBand::OneK,
            ResolutionArg::TwoK => ResolutionBand::TwoK,
            ResolutionArg::FourK => ResolutionBand::FourK,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UsageArg {
    Unused,
    Used,
}

impl From<UsageArg> for UsageStatus {
    fn from(value: UsageArg) -> Self {
        match value {
            UsageArg::Unused => UsageStatus::Unused,
            UsageArg::Used => UsageStatus::Used,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortArg {
    /// Import time. The default, newest first.
    Created,
    /// File name, case-insensitive.
    Name,
    /// File size in bytes.
    Size,
    /// Star rating.
    Rating,
}

impl From<SortArg> for AssetSort {
    fn from(value: SortArg) -> Self {
        match value {
            SortArg::Created => AssetSort::CreatedAt,
            SortArg::Name => AssetSort::Name,
            SortArg::Size => AssetSort::SizeBytes,
            SortArg::Rating => AssetSort::Rating,
        }
    }
}
