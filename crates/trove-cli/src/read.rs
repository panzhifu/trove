//! The commands that only look: whatever an agent needs to understand a
//! library before touching anything.
//!
//! Every one of these works while the desktop app holds the library open.

use serde_json::{Map, Value, json};

use trove_core::model::{Asset, AssetKind, AssetQuery, Page};
use trove_core::{paths, search};

use crate::cli::{Cli, FilterArgs, GetArgs, ListArgs, SearchArgs};
use crate::ctx::{
    CliError, Env, Rendered, asset_detail, asset_summary, parse_asset_ids, resolve_collection,
    resolve_tags,
};

// ---------------------------------------------------------------------------
// libraries / paths — configuration only, no database
// ---------------------------------------------------------------------------

/// Every library the config knows about, and whether its database is there.
pub fn libraries() -> Result<Rendered, CliError> {
    let config = trove_core::config::AppConfig::load();
    let active = config.active_slug();

    let items: Vec<Value> = config
        .libraries
        .iter()
        .map(|entry| {
            let data_root = entry.dir();
            let database = data_root.join("library.db");
            let meta = std::fs::metadata(&database).ok();
            json!({
                "slug": entry.slug,
                "name": entry.name,
                "active": entry.slug == active,
                "data_dir": data_root.display().to_string(),
                "cache_dir": entry.cache_dir().display().to_string(),
                "database": database.display().to_string(),
                "database_exists": meta.is_some(),
                "database_bytes": meta.map(|meta| meta.len()),
            })
        })
        .collect();

    let human = if items.is_empty() {
        "no libraries configured — start the Trove desktop app once to create one".to_string()
    } else {
        let rows: Vec<Vec<String>> = config
            .libraries
            .iter()
            .map(|entry| {
                vec![
                    entry.slug.clone(),
                    entry.name.clone(),
                    if entry.slug == active { "yes" } else { "" }.to_string(),
                    entry.dir().display().to_string(),
                ]
            })
            .collect();
        table(&["SLUG", "NAME", "ACTIVE", "DATA DIR"], &rows)
    };

    let result = json!({
        "libraries": items,
        "active_library": active,
        "config_file": paths::config_file().display().to_string(),
    });
    Ok(Rendered::new(result, human))
}

/// Where this library and its derivatives live.
pub fn paths(args: &Cli) -> Result<Rendered, CliError> {
    let config = trove_core::config::AppConfig::load();
    let entry = crate::ctx::resolve_entry(&config, args.library.as_deref())?;
    let data_root = entry.dir();
    let cache_root = entry.cache_dir();

    let result = json!({
        "library": {
            "slug": entry.slug,
            "name": entry.name,
            "data_dir": data_root.display().to_string(),
            "cache_dir": cache_root.display().to_string(),
            "database": data_root.join("library.db").display().to_string(),
            "media": data_root.join("media").display().to_string(),
            "backups": data_root.join("backups").display().to_string(),
            "metadata_json": data_root.join("library.json").display().to_string(),
            "search_index": cache_root.join("search_index").display().to_string(),
            "thumbnails": cache_root.join("thumbs").display().to_string(),
            // The other two derived tenants, so a script asking "what is using
            // my cache" gets an answer that names everything in it.
            "waveforms": cache_root.join("waveforms").display().to_string(),
            "contact_sheet": cache_root.join("contact-sheet").display().to_string(),
        },
        "data_dir": paths::data_dir().display().to_string(),
        "cache_dir": paths::cache_dir().display().to_string(),
        "config_file": paths::config_file().display().to_string(),
        "history_file": paths::history_file().display().to_string(),
        "themes_dir": paths::themes_dir().display().to_string(),
        "inbox": paths::incoming_dir().display().to_string(),
        "logs_dir": paths::logs_dir().display().to_string(),
    });

    let human = format!(
        "library   {}\ndata      {}\ncache     {}\nconfig    {}\ninbox     {}\nlogs      {}",
        data_root.display(),
        paths::data_dir().display(),
        paths::cache_dir().display(),
        paths::config_file().display(),
        paths::incoming_dir().display(),
        paths::logs_dir().display(),
    );
    Ok(Rendered::new(result, human))
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

/// Counts, sizes and index state — the first thing to ask about a library.
pub fn info(env: &Env) -> Result<Rendered, CliError> {
    let stats = env.library.stats()?;
    let conn = env.library.store().conn();

    let mut by_kind = Map::new();
    for (kind, count) in &stats.by_kind {
        by_kind.insert(kind_name(*kind), json!(count));
    }

    let documents = env.library.text_index().num_docs();
    let pending = search::pending_count(conn)?;

    let result = json!({
        "library": {
            "slug": env.slug,
            "name": env.name,
            "data_dir": env.data_root.display().to_string(),
            "cache_dir": env.cache_root.display().to_string(),
            "database": env.data_root.join("library.db").display().to_string(),
        },
        "schema_version": trove_core::store::schema::SCHEMA_VERSION,
        "assets": {
            "live": stats.live,
            "trashed": stats.trashed,
            "total_bytes": stats.total_bytes,
            "by_kind": by_kind,
        },
        "tags": stats.tags,
        "collections": stats.collections,
        "smart_collections": stats.smart_collections,
        "index": {
            "documents": documents,
            "live_assets": stats.live,
            // Rows the outbox still owes the index. Non-zero while the app
            // holds the writer is normal, not a fault.
            "pending": pending,
            "writable": env.index_writable,
            "behind": documents < stats.live || pending > 0,
        },
    });

    let mut kinds: Vec<String> = stats
        .by_kind
        .iter()
        .map(|(kind, count)| format!("{} {count}", kind_name(*kind)))
        .collect();
    kinds.sort();
    let human = format!(
        "library    {} ({})\nassets     {} live, {} trashed, {} total\nkinds      {}\ntags       {}\ncollections {} (+{} smart)\nindex      {} docs, {} pending, {}\nschema     v{}",
        env.name,
        env.slug,
        stats.live,
        stats.trashed,
        human_bytes(stats.total_bytes),
        if kinds.is_empty() {
            "none".to_string()
        } else {
            kinds.join(", ")
        },
        stats.tags,
        stats.collections,
        stats.smart_collections,
        documents,
        pending,
        if env.index_writable {
            "writable"
        } else {
            "read-only"
        },
        trove_core::store::schema::SCHEMA_VERSION,
    );
    Ok(Rendered::new(result, human))
}

// ---------------------------------------------------------------------------
// list / search
// ---------------------------------------------------------------------------

/// The general listing.
pub fn list(env: &Env, args: &ListArgs) -> Result<Rendered, CliError> {
    let query = build_query(env, &args.filter)?;
    let page = trove_core::store::assets::query(env.library.store().conn(), &query)?;
    Ok(page_rendered(env, page))
}

/// Full-text search, relevance-ordered.
pub fn search(
    env: &Env,
    args: &SearchArgs,
    style: &crate::ctx::Style,
) -> Result<Rendered, CliError> {
    let text = args.query.join(" ");
    let query = build_query(env, &args.filter)?;
    if !env.index_writable {
        // The index on disk was last written by the app, so a search sees the
        // library as of that moment. Worth saying, since the assets are not.
        style.note("the desktop app holds the index, so this search uses it as of that last write");
    }
    let page = env.library.search_assets(&text, &query)?;
    Ok(page_rendered(env, page))
}

fn build_query(env: &Env, filter: &FilterArgs) -> Result<AssetQuery, CliError> {
    if let Some(rating) = filter.min_rating
        && !(1..=5).contains(&rating)
    {
        return Err(CliError::usage("--min-rating takes a value from 1 to 5"));
    }
    if filter.limit == 0 {
        return Err(CliError::usage("--limit must be at least 1"));
    }
    if filter.limit > trove_core::store::assets::MAX_PAGE {
        // Said here rather than by the store so it reports as a bad ask about
        // the flag, and so `--limit` never quietly fetches less than it names.
        return Err(CliError::usage(format!(
            "--limit takes at most {}",
            trove_core::store::assets::MAX_PAGE
        )));
    }

    let mut query = AssetQuery {
        kind: filter.kind.map(Into::into),
        is_favorite: filter.favorite.then_some(true),
        min_rating: filter.min_rating,
        ext: filter
            .ext
            .as_ref()
            .map(|ext| ext.trim().trim_start_matches('.').to_lowercase()),
        orientation: filter.orientation.map(Into::into),
        aspect: filter.aspect.map(Into::into),
        resolution: filter.resolution.map(Into::into),
        source_path_prefix: filter.folder.clone(),
        usage_status: filter.usage.map(Into::into),
        is_trashed: filter.trashed,
        sort: filter.sort.into(),
        sort_desc: !filter.asc,
        limit: Some(filter.limit),
        offset: filter.offset,
        ..AssetQuery::default()
    };

    if !filter.tag.is_empty() {
        query.tag_ids = resolve_tags(env, &filter.tag)?;
    }
    if let Some(raw) = &filter.collection {
        query.collection_id = Some(resolve_collection(env, raw)?.id);
    }
    Ok(query)
}

fn page_rendered(env: &Env, page: Page<Asset>) -> Rendered {
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|asset| asset_summary(env, asset))
        .collect();
    let count = items.len();

    let mut object = Map::new();
    object.insert("total".into(), json!(page.total));
    // `total` is a floor when the search ran out of candidates: the library
    // holds at least this many matches, possibly more that were never seen.
    // Omitting this would let `trove search` print a count it does not know.
    object.insert("truncated".into(), json!(page.truncated));
    object.insert("returned".into(), json!(count));
    object.insert("assets".into(), Value::Array(items));
    let result = Value::Object(object);

    let rows: Vec<Vec<String>> = page
        .items
        .iter()
        .map(|asset| {
            vec![
                dimensions(asset),
                human_bytes(asset.size_bytes),
                asset.file_name.clone(),
                asset.id.to_string(),
            ]
        })
        .collect();
    let mut human = table(&["SIZE", "BYTES", "FILE", "ID"], &rows);
    if count == 0 {
        human = "no matching assets".to_string();
    }
    Rendered::new(result, human)
}

// ---------------------------------------------------------------------------
// get
// ---------------------------------------------------------------------------

/// The full record of one or more assets.
pub fn get(env: &Env, args: &GetArgs) -> Result<Rendered, CliError> {
    let ids = parse_asset_ids(&args.ids)?;
    let conn = env.library.store().conn();

    let mut items = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match trove_core::store::assets::get(conn, id)? {
            Some(asset) => items.push(asset_detail(env, &asset)?),
            None => missing.push(id.to_string()),
        }
    }

    // Nothing found at all is a failure, not an empty result: an id that does
    // not resolve is almost always a typo or the wrong library.
    if items.is_empty() {
        return Err(CliError::runtime(format!(
            "no such asset: {}",
            missing.join(", ")
        )));
    }

    let human = items
        .iter()
        .map(|item| {
            let text = |key: &str| match item.get(key) {
                Some(Value::String(value)) => value.clone(),
                Some(Value::Null) | None => "-".to_string(),
                Some(other) => other.to_string(),
            };
            let list = |key: &str| {
                let joined = item
                    .get(key)
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                empty_as_dash(joined)
            };
            let size = human_bytes(item.get("size_bytes").and_then(Value::as_u64).unwrap_or(0));
            let dimensions = format!(
                "{}x{}",
                item.get("width").and_then(Value::as_u64).unwrap_or(0),
                item.get("height").and_then(Value::as_u64).unwrap_or(0)
            );
            let source = item
                .get("source_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            let mut out = text("file_name");
            for (label, value) in [
                ("id", text("id")),
                ("kind", text("kind")),
                ("size", size),
                ("dimensions", dimensions),
                ("title", text("title")),
                ("description", text("description")),
                ("rating", text("rating")),
                ("favourite", text("is_favorite")),
                ("usage", text("usage_status")),
                ("tags", list("tags")),
                ("collections", list("collections")),
                ("source", empty_as_dash(source)),
                ("file", text("file_path")),
            ] {
                out.push_str(&format!("\n  {label:<12} {value}"));
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut object = Map::new();
    object.insert("assets".into(), Value::Array(items));
    if !missing.is_empty() {
        object.insert("missing".into(), json!(missing));
    }
    Ok(Rendered::new(Value::Object(object), human))
}

// ---------------------------------------------------------------------------
// tags / collections
// ---------------------------------------------------------------------------

/// Tags with the size of the subtree each one covers.
pub fn tags(env: &Env) -> Result<Rendered, CliError> {
    let conn = env.library.store().conn();
    let all = trove_core::store::tags::list(conn)?;
    let counts = trove_core::store::tags::counts_by_tag(conn)?;

    let items: Vec<Value> = all
        .iter()
        .map(|tag| {
            json!({
                "id": tag.id,
                "name": tag.name,
                "color": tag.color,
                "parent_id": tag.parent_id,
                // Includes the subtree, matching what filtering by this tag
                // would return — see `ctx::resolve_tags`.
                "assets": counts.get(&tag.id).copied().unwrap_or(0),
                "created_at": tag.created_at,
            })
        })
        .collect();

    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|tag| {
            vec![
                counts.get(&tag.id).copied().unwrap_or(0).to_string(),
                tag.name.clone(),
                tag.id.to_string(),
            ]
        })
        .collect();
    let human = if rows.is_empty() {
        "no tags yet".to_string()
    } else {
        table(&["ASSETS", "TAG", "ID"], &rows)
    };
    Ok(Rendered::new(json!({ "tags": items }), human))
}

/// A container's own look, or `null` for the folder tree's default. Said as
/// `kind` + `value` because the two glyph kinds mean different things to a
/// script: an emoji is a literal, an icon is a name in the app's catalogue.
fn appearance_json(appearance: &trove_core::model::Appearance) -> Value {
    use trove_core::model::Glyph;
    if appearance.is_plain() {
        return Value::Null;
    }
    json!({
        "glyph": appearance.glyph.as_ref().map(|glyph| match glyph {
            Glyph::Emoji(text) => json!({ "kind": "emoji", "value": text }),
            Glyph::Icon(name) => json!({ "kind": "icon", "value": name }),
        }),
        "accent": appearance.accent.map(|accent| accent.as_str()),
    })
}

/// The collection tree, plus smart collections.
pub fn collections(env: &Env) -> Result<Rendered, CliError> {
    let conn = env.library.store().conn();
    let all = trove_core::store::collections::list(conn)?;
    let smart = trove_core::store::smart_collections::list(conn)?;

    let mut items = Vec::new();
    for collection in &all {
        items.push(json!({
            "id": collection.id,
            "name": collection.name,
            "parent_id": collection.parent_id,
            "position": collection.position,
            "assets": trove_core::store::collections::count_assets(conn, collection.id)?,
            "kind": "collection",
            "appearance": appearance_json(&collection.appearance),
        }));
    }
    let smart_items: Vec<Value> = smart
        .iter()
        .map(|collection| {
            json!({
                "id": collection.id,
                "name": collection.name,
                "parent_id": collection.parent_id,
                "kind": "smart",
                "appearance": appearance_json(&collection.appearance),
            })
        })
        .collect();

    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|collection| {
            vec![
                trove_core::store::collections::count_assets(conn, collection.id)
                    .unwrap_or(0)
                    .to_string(),
                collection.name.clone(),
                collection.id.to_string(),
            ]
        })
        .collect();
    let human = if rows.is_empty() && smart_items.is_empty() {
        "no collections yet".to_string()
    } else {
        table(&["ASSETS", "COLLECTION", "ID"], &rows)
    };

    Ok(Rendered::new(
        json!({ "collections": items, "smart_collections": smart_items }),
        human,
    ))
}

// ---------------------------------------------------------------------------
// duplicates / folders
// ---------------------------------------------------------------------------

/// Near-duplicate images, largest cluster first.
pub fn duplicates(env: &Env) -> Result<Rendered, CliError> {
    let groups = env.library.find_duplicates()?;

    let mut reclaimable: u64 = 0;
    let mut items = Vec::new();
    for group in &groups {
        // Keeping one member means every other member's bytes are reclaimable.
        let size = group
            .assets
            .first()
            .map(|asset| asset.size_bytes)
            .unwrap_or(0);
        reclaimable += size.saturating_mul(group.assets.len().saturating_sub(1) as u64);
        items.push(json!({
            "count": group.assets.len(),
            "size_bytes": size,
            "assets": group
                .assets
                .iter()
                .map(|asset| asset_summary(env, asset))
                .collect::<Vec<_>>(),
        }));
    }

    let rows: Vec<Vec<String>> = groups
        .iter()
        .flat_map(|group| {
            group.assets.iter().enumerate().map(|(index, asset)| {
                vec![
                    if index == 0 {
                        group.assets.len().to_string()
                    } else {
                        String::new()
                    },
                    asset.file_name.clone(),
                    human_bytes(asset.size_bytes),
                    asset.id.to_string(),
                ]
            })
        })
        .collect();
    let human = if rows.is_empty() {
        "no near-duplicates found".to_string()
    } else {
        table(&["GROUP", "FILE", "BYTES", "ID"], &rows)
    };

    Ok(Rendered::new(
        json!({
            "groups": items,
            "group_count": groups.len(),
            "reclaimable_bytes": reclaimable,
        }),
        human,
    ))
}

/// The folders this library's files came from.
pub fn folders(env: &Env) -> Result<Rendered, CliError> {
    let folders = trove_core::store::assets::source_folders(env.library.store().conn())?;
    let items: Vec<Value> = folders
        .iter()
        .map(|(path, count)| json!({ "path": path, "assets": count }))
        .collect();
    let rows: Vec<Vec<String>> = folders
        .iter()
        .map(|(path, count)| vec![count.to_string(), path.clone()])
        .collect();
    let human = if rows.is_empty() {
        "no source folders recorded".to_string()
    } else {
        table(&["ASSETS", "FOLDER"], &rows)
    };
    Ok(Rendered::new(json!({ "folders": items }), human))
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Check the things that quietly break a library.
pub fn doctor(env: &Env) -> Result<Rendered, CliError> {
    let conn = env.library.store().conn();
    let stats = env.library.stats()?;
    let documents = env.library.text_index().num_docs();
    let pending = search::pending_count(conn)?;
    let inbox = trove_core::services::collect::inbox_items().len();

    let mut checks: Vec<Value> = Vec::new();
    let mut push = |name: &str, ok: bool, detail: String| {
        checks.push(json!({ "check": name, "ok": ok, "detail": detail }));
    };

    push(
        "database",
        env.data_root.join("library.db").is_file(),
        env.data_root.join("library.db").display().to_string(),
    );
    push(
        "schema",
        true,
        format!("v{}", trove_core::store::schema::SCHEMA_VERSION),
    );
    push(
        "index",
        documents >= stats.live && pending == 0,
        format!(
            "{documents} documents for {} live assets, {pending} rows queued",
            stats.live
        ),
    );
    push(
        "index-writable",
        env.index_writable,
        if env.index_writable {
            "this process owns the writer".into()
        } else {
            "the desktop app holds the writer; queries use the index as it last left it".into()
        },
    );

    let ffmpeg = trove_core::media::video::ffmpeg_available();
    push(
        "ffmpeg",
        ffmpeg,
        if ffmpeg {
            "video frame extraction available".into()
        } else {
            "not on PATH: video thumbnails and non-MP4 probing degrade".into()
        },
    );
    let ffprobe = trove_core::media::video::ffprobe_available();
    push(
        "ffprobe",
        ffprobe,
        if ffprobe {
            "container probing available".into()
        } else {
            "not on PATH: non-MP4 videos import without duration or dimensions".into()
        },
    );

    let embedding = trove_core::config::AppConfig::load().ai_embedding;
    match embedding {
        Some(config) => push(
            "embeddings",
            true,
            format!(
                "{} at {} (not exercised by doctor)",
                config.model, config.base_url
            ),
        ),
        None => push(
            "embeddings",
            true,
            "no embedding endpoint configured; search is text-only".into(),
        ),
    }

    push(
        "inbox",
        true,
        format!(
            "{inbox} file(s) waiting under {}",
            paths::incoming_dir().display()
        ),
    );

    let failures = checks
        .iter()
        .filter(|check| check.get("ok") == Some(&Value::Bool(false)))
        .count();

    let human = checks
        .iter()
        .map(|check| {
            format!(
                "{} {:<16} {}",
                if check.get("ok") == Some(&Value::Bool(false)) {
                    "!"
                } else {
                    "."
                },
                check.get("check").and_then(Value::as_str).unwrap_or(""),
                check.get("detail").and_then(Value::as_str).unwrap_or(""),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Rendered::new(
        json!({ "checks": checks, "failures": failures, "ok": failures == 0 }),
        human,
    ))
}

// ---------------------------------------------------------------------------
// index status
// ---------------------------------------------------------------------------

/// Where the search index stands relative to the database.
pub fn index_status(env: &Env) -> Result<Rendered, CliError> {
    let conn = env.library.store().conn();
    let stats = env.library.stats()?;
    let documents = env.library.text_index().num_docs();
    let pending = search::pending_count(conn)?;
    let index_dir = env.cache_root.join("search_index");

    let result = json!({
        "documents": documents,
        "live_assets": stats.live,
        "pending": pending,
        "writable": env.index_writable,
        "index_dir": index_dir.display().to_string(),
        "behind": documents < stats.live || pending > 0,
    });
    let human = format!(
        "{} documents for {} live assets; {} queued; {}\n{}",
        documents,
        stats.live,
        pending,
        if env.index_writable {
            "this process owns the writer"
        } else {
            "read-only (the desktop app owns the writer)"
        },
        index_dir.display(),
    );
    Ok(Rendered::new(result, human))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The model's spelling of an asset kind (`image`, `model`, …).
fn kind_name(kind: AssetKind) -> String {
    match serde_json::to_value(kind) {
        Ok(Value::String(name)) => name,
        _ => format!("{kind:?}").to_lowercase(),
    }
}

fn dimensions(asset: &Asset) -> String {
    match (asset.width, asset.height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => match asset.duration_ms {
            Some(ms) => format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60),
            None => "-".to_string(),
        },
    }
}

fn empty_as_dash(value: String) -> String {
    if value.trim().is_empty() {
        "-".into()
    } else {
        value
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A plain text table. Column widths come from the widest cell; nothing
/// wraps, because a wrapped table is harder to read than a long line.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|header| cell_width(header)).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell_width(cell));
            }
        }
    }

    let mut out = String::new();
    let header: Vec<String> = headers.iter().map(|header| header.to_string()).collect();
    push_row(&mut out, &header, &widths);
    let rule: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    push_row(&mut out, &rule, &widths);
    for row in rows {
        push_row(&mut out, row, &widths);
    }
    out.trim_end().to_string()
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        let pad = widths
            .get(index)
            .copied()
            .unwrap_or(0)
            .saturating_sub(cell_width(cell));
        if index + 1 < cells.len() {
            line.push_str(&" ".repeat(pad));
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

/// Display width in terminal cells: CJK and emoji take two columns, which is
/// the difference between an aligned table and a ragged one.
fn cell_width(text: &str) -> usize {
    text.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD)
}
