//! Migration from other asset managers: scan an Eagle `.library` folder or a
//! Billfish library into a plan, then apply the plan to a store. Files stay
//! where they are — the apply phase imports through the regular link path —
//! and tags, ratings, notes, source URLs and folder structure travel with
//! them.
//!
//! Both formats are read the way Serpent reads them (`reference/Serpent`),
//! the only documented parser for Billfish's database; Eagle's layout is the
//! app's own on-disk format. Nothing here writes to the source library — a
//! scan opens even the database read-only.
//!
//! The two sources map their folder trees differently, and the difference is
//! load-bearing: **Eagle's folders are virtual categories** (a file belongs to
//! several without moving), so they become Trove's nested collections or the
//! structure is lost. **Billfish's folders are real directories** the files
//! actually live in, so the folders panel rebuilds that tree for free from
//! the recorded source paths and creating collections would only duplicate
//! it — Billfish items therefore carry no folder memberships.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::OpenFlags;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::{AssetId, AssetPatch, NewCollection, Rating, TagId};
use crate::store::{Store, assets, collections, tags};

/// Which asset manager a scanned library belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Eagle,
    Billfish,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::Eagle => "eagle",
            SourceKind::Billfish => "billfish",
        }
    }
}

/// Serpent's cap on Eagle annotations; a longer note is truncated on a char
/// boundary rather than rejected — the text is the user's.
pub const MAX_NOTE_BYTES: usize = 10_000;

/// One asset a scan found worth migrating.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanItem {
    /// The file's real path on disk — what the link import will record.
    pub source_path: PathBuf,
    pub file_name: String,
    pub size: u64,
    pub tags: Vec<String>,
    /// Eagle folder ids this asset belongs to, resolved through the plan's
    /// [`MigrationPlan::folders`]. Empty for Billfish (its folders are real
    /// directories — see the module comment).
    pub folder_ids: Vec<String>,
    /// 1..=5, already clamped; 0 or absent reads as `None`.
    pub rating: Option<u8>,
    pub note: Option<String>,
    pub url: Option<String>,
}

/// A file the scan saw but will not migrate, and why. `skipped` is advice,
/// not an error — the rest of the library still migrates.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

/// The result of a scan: everything `apply` needs, and nothing it doesn't.
/// A plan is plain data — serializable for a `--dry-run`, cheap to hold.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationPlan {
    pub kind: SourceKind,
    /// The library root the scan matched on.
    pub root: PathBuf,
    pub items: Vec<PlanItem>,
    /// Eagle only: folder id → names from the root down. Billfish plans are
    /// empty here by design.
    pub folders: BTreeMap<String, Vec<String>>,
    pub skipped: Vec<Skipped>,
}

impl MigrationPlan {
    /// Every distinct tag name, sorted — the set `apply` will ensure.
    pub fn tag_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .items
            .iter()
            .flat_map(|item| item.tags.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The files `apply` will hand to the importer, plan order.
    pub fn file_paths(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .map(|item| item.source_path.clone())
            .collect()
    }
}

/// Recognize a library folder. Neither check opens the database — Billfish's
/// database file merely has to exist; Eagle's `metadata.json` has to parse as
/// an object carrying the `folders` key, which is what Serpent treats as the
/// library marker too.
pub fn detect(path: &Path) -> Option<SourceKind> {
    if !path.is_dir() {
        return None;
    }
    if is_eagle_root(path) {
        return Some(SourceKind::Eagle);
    }
    if path.join(".bf").join("billfish.db").is_file() {
        return Some(SourceKind::Billfish);
    }
    None
}

fn is_eagle_root(path: &Path) -> bool {
    if !path.join("images").is_dir() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path.join("metadata.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .is_some_and(|value| value.get("folders").is_some_and(|f| f.is_array()))
}

/// Scan a library into a plan. Pure read: the Eagle scan touches only
/// `metadata.json` files and `std::fs::metadata`, the Billfish scan opens its
/// database with SQLite's read-only flag.
pub fn scan(path: &Path) -> Result<MigrationPlan> {
    match detect(path) {
        Some(SourceKind::Eagle) => scan_eagle(path),
        Some(SourceKind::Billfish) => scan_billfish(path),
        None => Err(Error::Validation(format!(
            "not a recognizable Eagle or Billfish library: {}",
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Eagle
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EagleFolder {
    id: String,
    name: String,
    #[serde(default)]
    children: Vec<EagleFolder>,
}

/// Flatten Eagle's folder tree into id → name chain from the root down.
fn flatten_eagle_folders(
    nodes: &[EagleFolder],
    prefix: Vec<String>,
    chains: &mut BTreeMap<String, Vec<String>>,
) {
    for node in nodes {
        let mut chain = prefix.clone();
        chain.push(node.name.clone());
        flatten_eagle_folders(&node.children, chain.clone(), chains);
        chains.insert(node.id.clone(), chain);
    }
}

fn scan_eagle(root: &Path) -> Result<MigrationPlan> {
    let text = std::fs::read_to_string(root.join("metadata.json"))?;
    let header: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| Error::Validation(format!("Eagle metadata.json: {error}")))?;

    let mut folders = BTreeMap::new();
    if let Some(tree) = header.get("folders")
        && let Ok(nodes) = serde_json::from_value::<Vec<EagleFolder>>(tree.clone())
    {
        flatten_eagle_folders(&nodes, Vec::new(), &mut folders);
    }

    let images = root.join("images");
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let entries = std::fs::read_dir(&images).map_err(|error| {
        Error::Io(std::io::Error::other(format!(
            "reading {}: {error}",
            images.display()
        )))
    })?;
    for entry in entries.flatten() {
        let info = entry.path();
        if !info.is_dir() {
            continue;
        }
        let meta_path = info.join("metadata.json");
        let Ok(text) = std::fs::read_to_string(&meta_path) else {
            skipped.push(Skipped {
                path: info.clone(),
                reason: "metadata.json 不可读".into(),
            });
            continue;
        };
        let meta: serde_json::Value = match serde_json::from_str(&text) {
            Ok(meta) => meta,
            Err(error) => {
                skipped.push(Skipped {
                    path: info.clone(),
                    reason: format!("metadata.json: {error}"),
                });
                continue;
            }
        };
        if meta
            .get("isDeleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            skipped.push(Skipped {
                path: info.clone(),
                reason: "在源库的回收站里".into(),
            });
            continue;
        }

        let name = str_field(&meta, "name").unwrap_or_default();
        let ext = str_field(&meta, "ext").unwrap_or_default();
        let item_id = str_field(&meta, "id")
            .unwrap_or_else(|| file_name(&info).trim_end_matches(".info").to_string());

        // The file: Eagle keeps it inside the `.info` folder next to the
        // metadata. `<name>.<ext>` first, then the largest remaining file —
        // the same heuristic Serpent uses, minus the thumbnails and the
        // metadata backups.
        let mut source_path = if ext.is_empty() {
            None
        } else {
            let candidate = info.join(format!("{name}.{ext}"));
            candidate.is_file().then_some(candidate)
        };
        if source_path.is_none() {
            source_path = largest_payload_file(&info);
        }
        let Some(source_path) = source_path else {
            skipped.push(Skipped {
                path: info.clone(),
                reason: "没有找到文件本体（可能只有书签或备注）".into(),
            });
            continue;
        };
        let file_name = match Path::new(&name).file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => item_id.clone(),
        };

        let size = meta
            .get("size")
            .and_then(|v| v.as_u64())
            .or_else(|| std::fs::metadata(&source_path).ok().map(|m| m.len()))
            .unwrap_or(0);

        let tags: Vec<String> = string_list(&meta, "tags");
        let folder_ids: Vec<String> = meta
            .get("folders")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|id| folders.contains_key(*id))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let rating = meta
            .get("star")
            .and_then(|v| v.as_u64())
            .map(|star| star.min(5) as u8)
            .filter(|star| *star >= 1);
        let note = str_field(&meta, "annotation").map(|note| truncate_utf8(&note, MAX_NOTE_BYTES));
        let url = str_field(&meta, "url").filter(|url| is_web_url(url));

        items.push(PlanItem {
            source_path,
            file_name,
            size,
            tags,
            folder_ids,
            rating,
            note,
            url,
        });
    }

    Ok(MigrationPlan {
        kind: SourceKind::Eagle,
        root: root.to_path_buf(),
        items,
        folders,
        skipped,
    })
}

/// The largest file in an Eagle `.info` folder that is not the metadata and
/// not a thumbnail — the file the record describes, when the name/ext pair
/// does not resolve.
fn largest_payload_file(info: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(info).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == "metadata.json" || name.to_lowercase().contains("thumbnail") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let size = metadata.len();
        if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
            best = Some((size, path));
        }
    }
    best.map(|(_, path)| path)
}

// ---------------------------------------------------------------------------
// Billfish
// ---------------------------------------------------------------------------

/// A Billfish library: the `.bf/billfish.db` SQLite database next to the
/// files. 3.x keeps normalized tables; 2.x keeps a single loose table, which
/// is read the forgiving way Serpent reads it — find the path-ish column,
/// then best-effort the rest.
fn scan_billfish(root: &Path) -> Result<MigrationPlan> {
    let db = root.join(".bf").join("billfish.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| Error::Db(format!("opening {}: {error}", db.display())))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(30));

    let tables = table_names(&conn)?;
    if tables.iter().any(|t| t == "bf_file") {
        scan_billfish_v3(root, &conn)
    } else {
        scan_billfish_v2(root, &conn, &tables)
    }
}

fn table_names(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .map_err(db_error)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?
        .flatten()
        .collect();
    Ok(names)
}

/// Billfish 3.x: `bf_folder` is the directory tree the files sit in (pid 0 is
/// the root), `bf_file` records `name` + `pid` but no path — the relative
/// path is the folder chain plus the name. `bf_material_userdata` carries the
/// user's own metadata one-to-one per file.
fn scan_billfish_v3(root: &Path, conn: &rusqlite::Connection) -> Result<MigrationPlan> {
    // Folder id → (parent id, name). pid 0 means the root.
    let mut folders: HashMap<i64, (i64, String)> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, pid, name FROM bf_folder")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(db_error)?;
        for row in rows.flatten() {
            folders.insert(row.0, (row.1, row.2));
        }
    }

    // Recycle-bin folders (and everything under them) stay behind.
    let recycled: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM bf_folder WHERE COALESCE(is_recycle, 0) = 1")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(db_error)?;
        rows.flatten().collect()
    };
    let in_recycled = |mut pid: i64| -> bool {
        while pid != 0 {
            if recycled.contains(&pid) {
                return true;
            }
            match folders.get(&pid) {
                Some(&(parent, _)) => pid = parent,
                None => return false,
            }
        }
        false
    };

    // Tag joins: file id → tag names. 2.x names the table `bf_tag`, 3.x
    // `bf_tag_v2` — the join references whichever exists.
    let tag_table = if table_names(conn)?.iter().any(|t| t == "bf_tag_v2") {
        "bf_tag_v2"
    } else {
        "bf_tag"
    };
    let mut file_tags: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let sql = format!(
            "SELECT j.file_id, t.name FROM bf_tag_join_file j JOIN {tag_table} t ON t.id = j.tag_id"
        );
        let mut stmt = conn.prepare(&sql).map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?;
        for row in rows.flatten() {
            file_tags.entry(row.0).or_default().push(row.1);
        }
    }

    let mut items = Vec::new();
    let mut skipped = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT f.id, f.name, f.pid, COALESCE(f.file_size, 0), \
                 u.note, u.origin, u.score \
                 FROM bf_file f LEFT JOIN bf_material_userdata u ON u.file_id = f.id \
                 WHERE COALESCE(f.is_hide, 0) = 0",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })
            .map_err(db_error)?;

        for row in rows.flatten() {
            let (id, name, pid, size, note, origin, score) = row;
            let Some(relative) = folder_chain_path(&folders, pid, &name) else {
                skipped.push(Skipped {
                    path: PathBuf::from(&name),
                    reason: "文件夹链断了，无法重建相对路径".into(),
                });
                continue;
            };
            if in_recycled(pid) {
                skipped.push(Skipped {
                    path: relative,
                    reason: "在源库的回收站里".into(),
                });
                continue;
            }
            let source_path = root.join(&relative);
            if !source_path.is_file() {
                skipped.push(Skipped {
                    path: relative.clone(),
                    reason: "文件不在磁盘上".into(),
                });
                continue;
            }
            let rating = score
                .and_then(|score| u8::try_from(score).ok())
                .filter(|s| (1..=5).contains(s));
            items.push(PlanItem {
                source_path,
                file_name: name,
                size: size.max(0) as u64,
                tags: file_tags.get(&id).cloned().unwrap_or_default(),
                folder_ids: Vec::new(),
                rating,
                note: note.filter(|n| !n.trim().is_empty()),
                url: origin.filter(|url| is_web_url(url)),
            });
        }
    }

    Ok(MigrationPlan {
        kind: SourceKind::Billfish,
        root: root.to_path_buf(),
        items,
        folders: BTreeMap::new(),
        skipped,
    })
}

/// Walk `bf_folder`'s parent chain up to the root and join the names into the
/// file's path relative to the library root.
fn folder_chain_path(
    folders: &HashMap<i64, (i64, String)>,
    mut pid: i64,
    file_name: &str,
) -> Option<PathBuf> {
    let mut parts = Vec::new();
    while pid != 0 {
        let (parent, name) = folders.get(&pid)?;
        parts.push(name.clone());
        pid = *parent;
    }
    parts.reverse();
    let mut relative = PathBuf::new();
    for part in parts {
        relative.push(part);
    }
    relative.push(file_name);
    Some(relative)
}

/// Billfish 2.x: no documented schema. Serpent's fallback scans every table
/// for a path-ish column and picks the metadata columns by candidate name —
/// the same leniency here, reading only what is recognizable.
fn scan_billfish_v2(
    root: &Path,
    conn: &rusqlite::Connection,
    tables: &[String],
) -> Result<MigrationPlan> {
    const PATH_COLUMNS: [&str; 5] = ["path", "filepath", "filename", "relativepath", "assetpath"];

    let mut items = Vec::new();
    let mut skipped = Vec::new();
    'tables: for table in tables {
        if table.starts_with("sqlite_") {
            continue;
        }
        let Ok(columns) = column_names(conn, table) else {
            continue;
        };
        let Some(path_column) = columns
            .iter()
            .find(|c| PATH_COLUMNS.contains(&c.to_lowercase().as_str()))
        else {
            continue;
        };
        let pick = |candidates: &[&str]| -> Option<String> {
            columns
                .iter()
                .find(|c| candidates.contains(&c.to_lowercase().as_str()))
                .cloned()
        };
        let note_column = pick(&["description", "remark", "note", "memo", "comment", "desc"]);
        let url_column = pick(&["source_url", "origin", "url", "website"]);
        let tags_column = pick(&["tags", "tag", "labels", "keywords"]);
        let rating_column = pick(&["rating", "score", "star", "stars"]);

        let sql = format!(
            "SELECT \"{path_column}\"{note}{url}{tags}{rating} FROM \"{table}\"",
            path_column = path_column,
            note = note_column
                .as_ref()
                .map_or(String::new(), |c| format!(", \"{c}\"")),
            url = url_column
                .as_ref()
                .map_or(String::new(), |c| format!(", \"{c}\"")),
            tags = tags_column
                .as_ref()
                .map_or(String::new(), |c| format!(", \"{c}\"")),
            rating = rating_column
                .as_ref()
                .map_or(String::new(), |c| format!(", \"{c}\"")),
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => continue 'tables,
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1).ok().flatten(),
                row.get::<_, Option<String>>(2).ok().flatten(),
                row.get::<_, Option<String>>(3).ok().flatten(),
                row.get::<_, Option<i64>>(4).ok().flatten(),
            ))
        });
        let Ok(rows) = rows else {
            continue;
        };
        for row in rows.flatten() {
            let (path, note, url, tags_raw, rating) = row;
            let source_path = resolve_source(root, &path);
            let Some(source_path) = source_path else {
                skipped.push(Skipped {
                    path: PathBuf::from(&path),
                    reason: "文件不在磁盘上".into(),
                });
                continue;
            };
            items.push(PlanItem {
                file_name: file_name(&source_path),
                source_path,
                size: 0,
                tags: parse_loose_tags(tags_raw.as_deref()),
                folder_ids: Vec::new(),
                rating: rating
                    .and_then(|r| u8::try_from(r).ok())
                    .filter(|r| (1..=5).contains(r)),
                note: note.filter(|n| !n.trim().is_empty()),
                url: url.filter(|u| is_web_url(u)),
            });
        }
        // One path-bearing table is the asset table; the rest are children.
        if !items.is_empty() {
            break;
        }
    }

    Ok(MigrationPlan {
        kind: SourceKind::Billfish,
        root: root.to_path_buf(),
        items,
        folders: BTreeMap::new(),
        skipped,
    })
}

/// A recorded path is used as-is when absolute, else relative to the library
/// root — and only when the file is actually there.
fn resolve_source(root: &Path, recorded: &str) -> Option<PathBuf> {
    let path = Path::new(recorded);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    candidate.is_file().then_some(candidate)
}

/// Loose tag lists: a JSON array, or one of the separators Chinese Windows
/// software actually writes — `,` `，` `;` `；` and newlines.
fn parse_loose_tags(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<String>>(trimmed).unwrap_or_default();
    }
    let mut names: Vec<String> = trimmed
        .split([',', '，', ';', '；', '\n', '\r'])
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn column_names(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(db_error)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(db_error)?
        .flatten()
        .collect();
    Ok(names)
}

fn db_error(error: rusqlite::Error) -> Error {
    Error::Db(error.to_string())
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// What the metadata-preparation pass built. `apply` returns it inside the
/// report; the job path holds onto it between its import and backfill halves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreparedMetadata {
    /// Tag name → tag id, every name the plan mentions.
    pub tag_ids: HashMap<String, uuid::Uuid>,
    /// Eagle folder id → collection id.
    pub collection_ids: BTreeMap<String, uuid::Uuid>,
    pub collections_created: u64,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct MigrationApplyReport {
    pub imported: u64,
    pub reused: u64,
    pub skipped: u64,
    pub tagged: u64,
    pub collections_created: u64,
    /// Records that got at least one piece of metadata.
    pub backfilled: u64,
    /// Imported records whose plan item could not be found (a source path
    /// that stopped matching) — nothing was guessed for them.
    pub backfill_failed: u64,
}

/// Create the collections an Eagle plan's folder tree needs and ensure every
/// tag. Idempotent: an existing collection (same parent, same name
/// case-insensitively) or tag is reused, so running a migration twice creates
/// nothing a second time.
pub fn prepare_metadata(
    plan: &MigrationPlan,
    conn: &rusqlite::Connection,
) -> Result<PreparedMetadata> {
    let mut prepared = PreparedMetadata::default();

    for name in plan.tag_names() {
        let tag = tags::ensure_named(conn, &name)?;
        prepared.tag_ids.insert(name, tag.id);
    }

    // Parents before leaves: sort the chains by length, then walk each one,
    // finding-or-creating each level under the previous.
    let all = plan.folders.clone();
    let mut ordered: Vec<(String, Vec<String>)> = all.into_iter().collect();
    ordered.sort_by_key(|(_, chain)| chain.len());
    // (parent id, lowercase name) → collection id.
    let mut cache: HashMap<(Option<uuid::Uuid>, String), uuid::Uuid> = HashMap::new();
    let existing = collections::list(conn)?;
    for (folder_id, chain) in ordered {
        let mut parent: Option<uuid::Uuid> = None;
        for name in &chain {
            let key = (parent, name.to_lowercase());
            if let Some(id) = cache.get(&key) {
                parent = Some(*id);
                continue;
            }
            let found = existing
                .iter()
                .find(|c| c.parent_id == parent && c.name.to_lowercase() == key.1)
                .map(|c| c.id);
            let id = match found {
                Some(id) => id,
                None => {
                    let created = collections::create(
                        conn,
                        &NewCollection {
                            parent_id: parent,
                            name: name.clone(),
                            position: 0,
                        },
                    )?;
                    prepared.collections_created += 1;
                    created.id
                }
            };
            cache.insert(key, id);
            parent = Some(id);
        }
        prepared
            .collection_ids
            .insert(folder_id, parent.expect("a chain is never empty"));
    }

    Ok(prepared)
}

/// One backfill decision per imported asset: which plan item it came from
/// (`None` = the source path stopped matching — leave the record alone).
pub type MatchedItems = Vec<Option<usize>>;

/// Write the plan's metadata onto the imported records. `matched` parallels
/// `asset_ids`; the same content imported twice (a re-run) gets the same
/// values written again, which is what makes a migration re-runnable.
pub fn backfill_items(
    conn: &rusqlite::Connection,
    plan: &MigrationPlan,
    prepared: &PreparedMetadata,
    asset_ids: &[uuid::Uuid],
    matched: &MatchedItems,
) -> Result<(u64, u64, u64)> {
    let mut tagged = 0u64;
    let mut backfilled = 0u64;
    let mut failed = 0u64;

    for (asset_id, plan_index) in asset_ids.iter().zip(matched) {
        let Some(index) = plan_index else {
            failed += 1;
            continue;
        };
        let item = &plan.items[*index];
        let mut touched = false;

        let patch = AssetPatch {
            title: None,
            description: item.note.clone().map(Some),
            kind: None,
            rating: item.rating.and_then(Rating::new).map(Some),
            is_favorite: None,
            source_url: item.url.clone().map(Some),
            usage_status: None,
            commercial_use: None,
            facts: None,
        };
        let patch_writes =
            patch.description.is_some() || patch.rating.is_some() || patch.source_url.is_some();
        if patch_writes {
            assets::update(conn, *asset_id, &patch)?;
            touched = true;
        }

        let mut has_tags = false;
        for name in &item.tags {
            let Some(tag_id) = prepared.tag_ids.get(name) else {
                continue;
            };
            tags::add_to_asset(conn, AssetId(*asset_id), TagId(*tag_id))?;
            has_tags = true;
        }
        if has_tags {
            tagged += 1;
            touched = true;
        }

        for folder_id in &item.folder_ids {
            if let Some(collection_id) = prepared.collection_ids.get(folder_id) {
                collections::add_asset(
                    conn,
                    crate::model::CollectionId(*collection_id),
                    AssetId(*asset_id),
                )?;
                touched = true;
            }
        }

        if touched {
            backfilled += 1;
        }
    }
    Ok((tagged, backfilled, failed))
}

/// The one-call form: prepare, import through the regular link path, then
/// backfill by reading each imported record's source path back. The job path
/// (`tasks::migration`) runs the same three phases across an import that
/// reports progress and honors cancellation; this form is for tests, the CLI
/// without a job, and small libraries.
pub fn apply(
    plan: &MigrationPlan,
    store: &Store,
    data_root: impl AsRef<Path>,
    cache_root: impl AsRef<Path>,
) -> Result<MigrationApplyReport> {
    let data_root = data_root.as_ref().to_path_buf();
    let cache_root = cache_root.as_ref().to_path_buf();
    // Prepare and backfill each run inside one transaction: a 10k-item
    // migration is tens of thousands of small writes, and committing those
    // one statement at a time is most of the metadata pass on a large plan.
    let prepared = store.transaction(|tx| prepare_metadata(plan, tx))?;

    let report = crate::media::import::import_files(
        store,
        &data_root,
        &cache_root,
        &plan.file_paths(),
        crate::media::import::ImportStorage::Link,
        None,
    )?;

    // Which plan item did each imported record come from? The importer writes
    // the source path into `facts`, so the record answers for itself — this
    // survives the content-hash reuse path (a record the library already had).
    let index: HashMap<&Path, usize> = plan
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.source_path.as_path(), i))
        .collect();
    let mut matched = MatchedItems::with_capacity(report.imported.len());
    let mut asset_ids = Vec::with_capacity(report.imported.len());
    for item in &report.imported {
        // The record answers for itself: the importer wrote the source path
        // into `facts`, which is exactly the plan item's `source_path`.
        matched.push(
            store_asset_source(store, item.asset_id)
                .as_deref()
                .and_then(|path| index.get(Path::new(path)).copied()),
        );
        asset_ids.push(item.asset_id);
    }
    let (tagged, backfilled, failed) =
        store.transaction(|tx| backfill_items(tx, plan, &prepared, &asset_ids, &matched))?;

    Ok(MigrationApplyReport {
        imported: report.imported.iter().filter(|i| !i.reused).count() as u64,
        reused: report.imported.iter().filter(|i| i.reused).count() as u64,
        skipped: report.skipped.len() as u64,
        tagged,
        collections_created: prepared.collections_created,
        backfilled,
        backfill_failed: failed,
    })
}

/// The recorded source path of an imported record (its `facts.source_path`).
fn store_asset_source(store: &Store, asset_id: uuid::Uuid) -> Option<String> {
    let asset = assets::get(store.conn(), asset_id).ok()??;
    asset.facts.source_path
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn str_field(meta: &serde_json::Value, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_list(meta: &serde_json::Value, key: &str) -> Vec<String> {
    meta.get(key)
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Truncate to a byte budget without splitting a character.
fn truncate_utf8(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The fresh/held split for a plan preview, answered over the target
/// library's database file (opened read-only, on whatever thread the caller
/// likes). `(fresh, held)`: fresh = files the loose dedup key does not know,
/// held = files that would fold into their records at the commit.
pub fn preview_split(db_path: &Path, plan: &MigrationPlan) -> (usize, usize) {
    let paths = plan.file_paths();
    let fresh = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map(|conn| assets::unimported_paths(&conn, &paths).len())
    .unwrap_or_else(|_| paths.len());
    let held = paths.len().saturating_sub(fresh);
    (fresh, held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::model::{AssetKind, AssetLocation, AssetQuery};

    /// One 1×1 PNG, the smallest file the importer will probe.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// A scratch root, torn down on drop. Nothing here touches the machine's
    /// real configuration.
    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "trove-migrate-{tag}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn library_dir(&self, slug: &str) -> PathBuf {
            self.root.join("data/libraries").join(slug)
        }

        fn cache_dir(&self, slug: &str) -> PathBuf {
            self.root.join("cache/libraries").join(slug)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// An Eagle library: one folder tree (`旅行 → 2024`), a fully-described
    /// image inside it, a minimal text file outside any folder, a deleted
    /// entry and an entry whose file is gone.
    fn eagle_fixture(root: &Path) {
        let images = root.join("images");
        std::fs::create_dir_all(images.join("A1.info")).unwrap();
        std::fs::create_dir_all(images.join("A2.info")).unwrap();
        std::fs::create_dir_all(images.join("A3.info")).unwrap();
        std::fs::create_dir_all(images.join("A4.info")).unwrap();
        std::fs::write(
            root.join("metadata.json"),
            r#"{"folders":[{"id":"f1","name":"旅行","children":[{"id":"f2","name":"2024"}]}]}"#,
        )
        .unwrap();
        std::fs::write(images.join("A1.info").join("a.png"), PNG_1X1).unwrap();
        std::fs::write(
            images.join("A1.info").join("metadata.json"),
            r#"{"id":"A1","name":"a","ext":"png","size":80,"tags":["风景","日落"],
                "folders":["f2"],"star":4,"annotation":"一张好图","url":"https://example.com/post"}"#,
        )
        .unwrap();
        std::fs::write(images.join("A2.info").join("b.txt"), b"plain").unwrap();
        std::fs::write(
            images.join("A2.info").join("metadata.json"),
            r#"{"id":"A2","name":"b","ext":"txt","tags":[" 风景 "]}"#,
        )
        .unwrap();
        std::fs::write(
            images.join("A3.info").join("metadata.json"),
            r#"{"id":"A3","name":"c","ext":"png","isDeleted":true}"#,
        )
        .unwrap();
        std::fs::write(
            images.join("A4.info").join("metadata.json"),
            r#"{"id":"A4","name":"d","ext":"png"}"#,
        )
        .unwrap();
    }

    #[test]
    fn an_eagle_library_scans_into_a_complete_plan() {
        let sandbox = Sandbox::new("eagle-scan");
        let lib_dir = sandbox.root.join("Photos.library");
        eagle_fixture(&lib_dir);

        assert_eq!(detect(&lib_dir), Some(SourceKind::Eagle));
        let plan = scan(&lib_dir).unwrap();
        assert_eq!(plan.kind, SourceKind::Eagle);
        assert_eq!(plan.items.len(), 2, "deleted and file-less entries skip");
        assert_eq!(plan.skipped.len(), 2);

        let a1 = plan.items.iter().find(|i| i.file_name == "a").unwrap();
        assert_eq!(a1.tags, vec!["风景", "日落"]);
        assert_eq!(a1.rating, Some(4));
        assert_eq!(a1.note.as_deref(), Some("一张好图"));
        assert_eq!(a1.url.as_deref(), Some("https://example.com/post"));
        assert_eq!(a1.folder_ids, vec!["f2"]);
        assert_eq!(
            plan.folders.get("f2").unwrap(),
            &vec!["旅行".to_string(), "2024".to_string()]
        );
        assert!(plan.tag_names().contains(&"日落".to_string()));

        // The file the heuristic picked is the one the metadata names.
        assert!(a1.source_path.ends_with("a.png"));
    }

    #[test]
    fn an_eagle_migration_carries_the_metadata_across() {
        let sandbox = Sandbox::new("eagle-apply");
        let lib_dir = sandbox.root.join("Photos.library");
        eagle_fixture(&lib_dir);
        let plan = scan(&lib_dir).unwrap();

        let library = Library::open(
            sandbox.library_dir("imported"),
            sandbox.cache_dir("imported"),
        )
        .unwrap();
        let report = apply(
            &plan,
            library.store(),
            sandbox.library_dir("imported"),
            sandbox.cache_dir("imported"),
        )
        .unwrap();
        assert_eq!(report.imported, 2);
        assert_eq!(report.reused, 0);
        assert_eq!(report.collections_created, 2, "旅行 and 2024");
        assert_eq!(report.backfilled, 2, "both items carry something");
        assert_eq!(report.tagged, 2);

        // The records: linked to the files where they are, with the plan's
        // metadata written on.
        let conn = library.store().conn();
        let page = assets::query(conn, &AssetQuery::live()).unwrap();
        assert_eq!(page.items.len(), 2);
        let image = page
            .items
            .iter()
            .find(|a| a.kind == AssetKind::Image)
            .unwrap();
        assert_eq!(image.rating.map(|r| r.get()), Some(4));
        assert_eq!(image.description.as_deref(), Some("一张好图"));
        assert_eq!(
            image.source_url.as_deref(),
            Some("https://example.com/post")
        );
        assert!(matches!(image.location(), AssetLocation::Linked { .. }));
        let text = page
            .items
            .iter()
            .find(|a| a.kind == AssetKind::Document)
            .unwrap();
        assert_eq!(text.rating, None, "the minimal item carries no rating");

        // Tags and the collection tree.
        let tagged = library.tags_for_asset(image.id).unwrap();
        let mut names: Vec<String> = tagged.into_iter().map(|t| t.name).collect();
        names.sort();
        assert_eq!(names, vec!["日落", "风景"]);
        let collections = collections::list(conn).unwrap();
        assert_eq!(collections.len(), 2);
        let trip = collections.iter().find(|c| c.name == "旅行").unwrap();
        let year = collections.iter().find(|c| c.name == "2024").unwrap();
        assert_eq!(year.parent_id, Some(trip.id), "the folder tree nests");
        let member = library.collections_for_asset(image.id).unwrap();
        assert_eq!(member.len(), 1, "the image sits in 旅行/2024");
        assert_eq!(member[0].name, "2024");
    }

    #[test]
    fn an_eagle_migration_re_runs_without_duplicating_anything() {
        let sandbox = Sandbox::new("eagle-rerun");
        let lib_dir = sandbox.root.join("Photos.library");
        eagle_fixture(&lib_dir);
        let plan = scan(&lib_dir).unwrap();
        let data = sandbox.library_dir("imported");
        let cache = sandbox.cache_dir("imported");
        let library = Library::open(&data, &cache).unwrap();

        let first = apply(&plan, library.store(), &data, &cache).unwrap();
        assert_eq!(first.imported, 2);
        let second = apply(&plan, library.store(), &data, &cache).unwrap();
        assert_eq!(second.reused, 2, "content dedup folds the second run");
        assert_eq!(second.imported, 0);
        assert_eq!(second.collections_created, 0, "collections are reused");
        assert_eq!(second.tagged, 2, "tags re-assert, not duplicate");
        let conn = library.store().conn();
        let page = assets::query(conn, &AssetQuery::live()).unwrap();
        assert_eq!(page.total, 2, "no record doubled");
        let collections = collections::list(conn).unwrap();
        assert_eq!(collections.len(), 2, "no collection doubled");
    }

    /// A Billfish 3.x library: real directories, a `.bf/billfish.db` beside
    /// them, one hidden file the scan must leave alone.
    fn billfish_fixture(root: &Path) {
        let dir = root.join("素材").join("2024");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("photo.png"), PNG_1X1).unwrap();
        std::fs::write(root.join("素材").join("hidden.txt"), b"hidden").unwrap();

        let bf = root.join(".bf");
        std::fs::create_dir_all(&bf).unwrap();
        let conn = rusqlite::Connection::open(bf.join("billfish.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE bf_folder (id INTEGER PRIMARY KEY, pid INTEGER, name TEXT, is_recycle INTEGER);
             CREATE TABLE bf_file (id INTEGER PRIMARY KEY, name TEXT, pid INTEGER, is_hide INTEGER, file_size INTEGER);
             CREATE TABLE bf_material_userdata (file_id INTEGER, note TEXT, origin TEXT, score INTEGER);
             CREATE TABLE bf_tag_v2 (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE bf_tag_join_file (file_id INTEGER, tag_id INTEGER);
             INSERT INTO bf_folder VALUES (1, 0, '素材', 0), (2, 1, '2024', 0), (3, 0, '回收站', 1);
             INSERT INTO bf_file VALUES (10, 'photo.png', 2, 0, 80), (11, 'hidden.txt', 1, 1, 6), (12, 'gone.png', 3, 0, 80);
             INSERT INTO bf_material_userdata VALUES (10, '备注一条', 'https://example.com/b', 5);
             INSERT INTO bf_tag_v2 VALUES (7, '旅行');
             INSERT INTO bf_tag_join_file VALUES (10, 7);",
        )
        .unwrap();
    }

    #[test]
    fn a_billfish_library_scans_rebuilds_paths_and_skips_the_bin() {
        let sandbox = Sandbox::new("billfish-scan");
        let lib_dir = sandbox.root.join("Fish Library");
        billfish_fixture(&lib_dir);

        assert_eq!(detect(&lib_dir), Some(SourceKind::Billfish));
        let plan = scan(&lib_dir).unwrap();
        assert_eq!(plan.kind, SourceKind::Billfish);
        assert!(
            plan.folders.is_empty(),
            "billfish folders are real directories"
        );
        assert_eq!(plan.items.len(), 1, "hidden and recycle-bin files stay");
        let item = &plan.items[0];
        assert_eq!(item.tags, vec!["旅行"]);
        assert_eq!(item.rating, Some(5));
        assert_eq!(item.note.as_deref(), Some("备注一条"));
        assert_eq!(item.url.as_deref(), Some("https://example.com/b"));
        assert!(
            plan.skipped.iter().any(|s| s.reason == "在源库的回收站里"),
            "the recycle-bin file is named, not silently dropped"
        );
        // The hidden file is filtered at the SQL level (the user hid it in
        // Billfish), so the only skip named is the recycle-bin one.
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.items.iter().all(|i| !i.file_name.contains("hidden")));
        assert!(
            item.source_path.ends_with("2024/photo.png"),
            "the folder chain rebuilds the path"
        );
    }

    #[test]
    fn a_billfish_migration_links_the_files_and_writes_the_userdata() {
        let sandbox = Sandbox::new("billfish-apply");
        let lib_dir = sandbox.root.join("Fish Library");
        billfish_fixture(&lib_dir);
        let plan = scan(&lib_dir).unwrap();

        let data = sandbox.library_dir("imported");
        let library = Library::open(&data, sandbox.cache_dir("imported")).unwrap();
        let report = apply(&plan, library.store(), &data, sandbox.cache_dir("imported")).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(
            report.collections_created, 0,
            "billfish folders stay directories"
        );

        let conn = library.store().conn();
        let page = assets::query(conn, &AssetQuery::live()).unwrap();
        assert_eq!(page.items.len(), 1);
        let asset = &page.items[0];
        assert_eq!(asset.rating.map(|r| r.get()), Some(5));
        assert_eq!(asset.description.as_deref(), Some("备注一条"));
        assert_eq!(asset.source_url.as_deref(), Some("https://example.com/b"));
        let AssetLocation::Linked { source_path } = asset.location() else {
            panic!("billfish files stay where they are: {:?}", asset.location());
        };
        assert!(source_path.ends_with("photo.png"));
        let names: Vec<String> = library
            .tags_for_asset(asset.id)
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["旅行"]);
    }

    /// Billfish 2.x: one loose table, tags as a separator string, no
    /// documented schema — the scan reads what it can recognize.
    #[test]
    fn a_billfish_2x_library_reads_through_the_lenient_path() {
        let sandbox = Sandbox::new("billfish-2x");
        let lib_dir = sandbox.root.join("OldFish");
        std::fs::create_dir_all(lib_dir.join(".bf")).unwrap();
        std::fs::write(lib_dir.join("old.png"), PNG_1X1).unwrap();
        let conn = rusqlite::Connection::open(lib_dir.join(".bf").join("billfish.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE assets (path TEXT, description TEXT, rating INTEGER, source_url TEXT, tags TEXT);
             INSERT INTO assets VALUES ('old.png', '旧库备注', 3, 'https://example.com/old', '[\"标签A\",\"标签B\"]');",
        )
        .unwrap();

        let plan = scan(&lib_dir).unwrap();
        assert_eq!(plan.items.len(), 1);
        let item = &plan.items[0];
        assert_eq!(item.tags, vec!["标签A", "标签B"]);
        assert_eq!(item.rating, Some(3));
        assert_eq!(item.note.as_deref(), Some("旧库备注"));
        assert_eq!(item.url.as_deref(), Some("https://example.com/old"));
    }

    #[test]
    fn detect_refuses_what_is_not_a_library() {
        let sandbox = Sandbox::new("detect");
        let plain = sandbox.root.join("plain");
        std::fs::create_dir_all(plain.join("images")).unwrap();
        std::fs::write(plain.join("images").join("x.png"), PNG_1X1).unwrap();
        assert_eq!(detect(&plain), None);
        // metadata.json without the folders key is not Eagle's marker.
        std::fs::write(plain.join("metadata.json"), r#"{"hello":1}"#).unwrap();
        assert_eq!(detect(&plain), None);
        assert!(scan(&plain).is_err());
    }

    #[test]
    fn trashed_billfish_folders_are_left_behind() {
        let sandbox = Sandbox::new("billfish-bin");
        let lib_dir = sandbox.root.join("Fish Library");
        billfish_fixture(&lib_dir);
        // The recycle-bin file (id 12, under 回收站) has no file on disk; give
        // it one and watch the scan still refuse it.
        let bin = lib_dir.join("回收站");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("gone.png"), PNG_1X1).unwrap();
        let plan = scan(&lib_dir).unwrap();
        assert!(plan.items.iter().all(|i| !i.source_path.starts_with(&bin)));
    }

    #[test]
    fn trashed_assets_in_the_target_library_stay_reachable_to_the_plan() {
        // A trashed asset in the target library still answers the dedup
        // question through its content, and a migration writes nothing into
        // the trash. Round-trip through a real library to pin it.
        let sandbox = Sandbox::new("trash-target");
        let lib_dir = sandbox.root.join("Photos.library");
        eagle_fixture(&lib_dir);
        let plan = scan(&lib_dir).unwrap();
        let data = sandbox.library_dir("imported");
        let library = Library::open(&data, sandbox.cache_dir("imported")).unwrap();
        let first = apply(&plan, library.store(), &data, sandbox.cache_dir("imported")).unwrap();
        assert_eq!(first.imported, 2);

        // Trash one, migrate again: the content dedup still folds it (the
        // record exists), the library does not gain a second copy.
        let conn = library.store().conn();
        let page = assets::query(conn, &AssetQuery::live()).unwrap();
        let image = page
            .items
            .iter()
            .find(|a| a.kind == AssetKind::Image)
            .unwrap();
        library.trash_assets(&[image.id]).unwrap();
        let second = apply(&plan, library.store(), &data, sandbox.cache_dir("imported")).unwrap();
        // The trashed record's content is no longer live, so its file imports
        // fresh — the trash keeps its member, the library gains a live twin.
        assert_eq!(second.reused, 1, "the live text asset folds");
        assert_eq!(second.imported, 1, "the trashed image re-imports");
        let trashed = assets::query(conn, &AssetQuery::trashed()).unwrap();
        assert_eq!(
            trashed.total, 1,
            "the migration did not resurrect the trash"
        );
    }
}
