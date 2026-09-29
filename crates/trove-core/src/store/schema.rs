//! The library schema: one script, one version, one door.
//!
//! `PRAGMA user_version` records the shape a library on disk has. This build
//! *creates* the whole shape from nothing and *refuses* anything else — an
//! existing library has to be at [`SCHEMA_VERSION`] exactly, or it does not
//! open. The refusal names both versions and costs nothing: it happens before
//! a single statement runs, so a library this build cannot read is left
//! exactly as it was found.
//!
//! That is deliberate, and it is the second time this file has landed here.
//! The first migration chain was twelve steps of history that only ever
//! served this project's own development (an abandoned FTS5 table, columns
//! altered in and dropped again) and was deleted before 0.5. A short upgrade
//! list then existed for two steps — the AI embedding table and the `sha256`
//! → `content_hash` rename — and went the same way once the rename had run on
//! every library worth carrying forward. A chain is a thing to own, test and
//! keep correct forever; a version gate is four lines. When the next shape
//! change arrives and there *are* libraries worth walking forward, a list can
//! come back — with the rule that a step must be applicable from a shape that
//! matches the version on record.
//!
//! That is where this file stands now: [`UPGRADES`] holds nine steps, because
//! every one of them landed while the version before it was already in the
//! field — v14 → v15 for the `ai_analysis` cache, v15 → v16 for a container's
//! appearance, v16 → v17 for the 3D viewport's look, v17 → v18 for the ordered
//! live-listing indexes, v18 → v19 for image sequences, v19 → v20 for the
//! indexed source path, v20 → v21 for the task journal, v21 → v22 to take the
//! `ai_analysis` cache back out again, and v22 → v23 to put the star rating's
//! domain in the database. Everything not on the list is still refused by name.
//!
//! v23 is also the first step this schema has run that *moves data toward a
//! narrower rule* rather than only adding to the shape: `0` was a legal rating
//! until the type made it unnameable, so the step folds stored zeros onto NULL
//! in the same transaction that installs the guards (which do not fire for a
//! NULL, so the fold itself passes them). It is still not a rescue for a library
//! that refuses to open — nothing here repairs a row the build cannot read.
//!
//! The last step is the first removal this schema has made, and it is worth
//! reading as a fact rather than a tidy-up: `ai_analysis` was created, indexed
//! and given its own migration step, and **no code ever inserted into it or
//! selected from it**. What an analysis run records — and what its undo reads
//! back — is a marker inside `assets.extra`. So the table was schema weight
//! plus a step to keep correct forever, for a row nobody wrote. The v14 → v15
//! step still creates it, because a step must describe the shape it actually
//! produced; a library walking the whole chain therefore gains the table at v15
//! and loses it at v22, which is what a chain is for.

/// The schema this build creates, and the only shape it opens. A library at
/// any other version is refused by name rather than guessed at.
///
/// The number continued from 12 rather than restarting: every library in
/// existence was written by a build whose chain ended there, and that shape
/// is the pre-`asset_embeddings` subset of the one below — which is the only
/// sense in which a version number means anything.
pub const SCHEMA_VERSION: i64 = 24;

/// One upgrade step: the DDL that takes a library from `from` to `to`, and the
/// data that DDL cannot move.
///
/// A step applies to a shape that matches `from` exactly — the version on
/// record is the whole guard, there is no fingerprint of the shape itself —
/// and re-running it must be harmless, because a crash between the DDL and the
/// version bump would otherwise leave the library unopenable. For additive DDL
/// `IF NOT EXISTS` says that; for a step that *changes* a table's shape the
/// same claim is made by [`Store::apply_upgrade`](super::Store::apply_upgrade)
/// tolerating the two ways an `ALTER` reports "already done", and by the data
/// step writing only what has not been written yet.
pub struct Upgrade {
    pub from: i64,
    pub to: i64,
    pub sql: &'static str,
    /// Runs after `sql`, in the same transaction, and may itself change the shape
    /// (see [`migrate_appearance`]).
    pub data: Option<fn(&rusqlite::Connection) -> crate::error::Result<()>>,
}

/// The upgrade list: each step may be applied to a library at its `from`
/// version to reach its `to`. A library at any other version is still refused
/// by name.
pub const UPGRADES: &[Upgrade] = &[
    Upgrade {
        from: 14,
        to: 15,
        sql: UPGRADE_14_TO_15,
        data: None,
    },
    Upgrade {
        from: 15,
        to: 16,
        sql: UPGRADE_15_TO_16,
        data: Some(migrate_appearance),
    },
    Upgrade {
        from: 16,
        to: 17,
        sql: UPGRADE_16_TO_17,
        data: None,
    },
    Upgrade {
        from: 17,
        to: 18,
        sql: UPGRADE_17_TO_18,
        data: Some(analyze_statistics),
    },
    Upgrade {
        from: 18,
        to: 19,
        sql: UPGRADE_18_TO_19,
        data: None,
    },
    Upgrade {
        from: 19,
        to: 20,
        sql: UPGRADE_19_TO_20,
        data: Some(analyze_statistics),
    },
    Upgrade {
        from: 20,
        to: 21,
        sql: UPGRADE_20_TO_21,
        data: None,
    },
    Upgrade {
        from: 21,
        to: 22,
        sql: UPGRADE_21_TO_22,
        data: None,
    },
    Upgrade {
        from: 22,
        to: 23,
        sql: UPGRADE_22_TO_23,
        data: Some(fold_zero_ratings),
    },
    Upgrade {
        from: 23,
        to: 24,
        sql: UPGRADE_23_TO_24,
        data: None,
    },
];

/// v23 → v24: give the undo history a table of its own.
///
/// Until this step the history was two `Vec`s inside `Library`: every mutation a
/// user had made was undoable until they closed the app, and the cap that made
/// that bounded was the memory it lived in. The step is pure DDL and the new
/// table is empty afterwards — nothing done before this build can be taken back,
/// because nothing done before it recorded what to take back.
const UPGRADE_23_TO_24: &str = r#"
    CREATE TABLE IF NOT EXISTS undo_log (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        action     TEXT NOT NULL,
        target     TEXT,
        count      INTEGER NOT NULL DEFAULT 1,
        op         TEXT NOT NULL,
        created_at TEXT NOT NULL,
        undone_at  TEXT
    );

    -- The two reads this table exists for: the newest row with `undone_at` NULL
    -- (what Ctrl+Z takes next) and the newest one with it set (what
    -- Ctrl+Shift+Z puts back). Both are this index, prefix-matched, newest-last.
    CREATE INDEX IF NOT EXISTS undo_log_side ON undo_log(undone_at, seq);
"#;

/// v22 → v23: put the star rating's domain in the database.
///
/// A rating is one of five stars or it is absent, and until this step only Rust
/// said so -- in one function, [`crate::model::AssetPatch::validate`], which every
/// other writer (import, AI analysis, the CLI, a hand-edited file) could and did
/// route around. The column accepted any integer.
///
/// Two guards rather than a column `CHECK`, and the reason is what a `CHECK` would
/// cost: SQLite cannot add one to an existing column, so asking for it means
/// recreating `assets` -- its twenty-one indexes, and the three outbox triggers
/// that fire on it -- inside a migration whose whole job is to not touch the rows.
/// A trigger is additive DDL that says the same thing and can be re-run.
///
/// `ABORT` rather than `ROLLBACK` because the statement is the unit the caller
/// expects to fail: a batch metadata write that hits one out-of-domain value
/// loses that statement, not the transaction the caller had already opened.
const UPGRADE_22_TO_23: &str = r#"
    CREATE TRIGGER IF NOT EXISTS assets_rating_insert_guard
    BEFORE INSERT ON assets
    WHEN NEW.rating IS NOT NULL AND (NEW.rating < 1 OR NEW.rating > 5)
    BEGIN
        SELECT RAISE(ABORT, 'rating must be 1..=5, or NULL for unrated');
    END;

    CREATE TRIGGER IF NOT EXISTS assets_rating_update_guard
    BEFORE UPDATE OF rating ON assets
    WHEN NEW.rating IS NOT NULL AND (NEW.rating < 1 OR NEW.rating > 5)
    BEGIN
        SELECT RAISE(ABORT, 'rating must be 1..=5, or NULL for unrated');
    END;
"#;

/// Fold the ratings a library already holds onto the domain the guards accept.
///
/// `0` was reachable: the bound this build checked was `0..=5`, and a star row
/// cannot draw it -- so a stored zero is "not rated" with a number attached, and
/// that is exactly the state [`crate::model::Rating`] refuses to hold. It becomes
/// NULL, which is what an unrated asset already is. Values above five are left
/// where they are rather than clamped: nobody chose them, and silently rewriting a
/// judgement the user may have made in another tool is worse than reading it as
/// unrated at the row boundary (see `store::assets`'s rating read).
///
/// Safe to land twice -- after the first pass no row matches, and the guards the
/// same step installed do not fire for a NULL.
fn fold_zero_ratings(conn: &rusqlite::Connection) -> crate::error::Result<()> {
    conn.execute("UPDATE assets SET rating = NULL WHERE rating = 0", [])?;
    Ok(())
}

/// v21 → v22: drop the `ai_analysis` cache table and its index.
///
/// Both statements are `IF EXISTS` because the shape a library arrives at is not
/// guaranteed to have them: a v14 library creates the table at v15, while one
/// made fresh by *this* build never has it — the create-from-nothing script
/// stopped writing it. Re-running the step is harmless for the same reason.
const UPGRADE_21_TO_22: &str = r#"
    DROP INDEX IF EXISTS idx_ai_analysis_model;
    DROP TABLE IF EXISTS ai_analysis;
"#;

/// v20 → v21: the task journal.
///
/// Persists task metadata so the UI can surface interrupted work after a
/// restart and track retry history. The table is append-only for status
/// updates (upsert on task_id), so a crash mid-write leaves the previous
/// state intact.
const UPGRADE_20_TO_21: &str = r#"
    CREATE TABLE IF NOT EXISTS task_journal (
        task_id      TEXT PRIMARY KEY,
        kind         TEXT NOT NULL,
        label        TEXT NOT NULL,
        status       TEXT NOT NULL,
        done         INTEGER NOT NULL DEFAULT 0,
        total        INTEGER NOT NULL DEFAULT 0,
        summary      TEXT,
        error        TEXT,
        retry_count  INTEGER NOT NULL DEFAULT 0,
        max_retries  INTEGER NOT NULL DEFAULT 0,
        started_at   TEXT NOT NULL,
        finished_at  TEXT
    );
"#;

/// v19 → v20: an index the folder queries can actually use.
///
/// `source_path` is a *generated* column over the JSON key of the same name,
/// not a second copy of it: it is computed on read, so it cannot drift from
/// `extra`, it takes no space in the row, and no writer has to know it exists —
/// `assets::COLS` and the insert list are unchanged. What it buys is
/// indexability. The folder filter and the folders panel both asked
/// `json_extract(extra, '$.source_path')`, which no index can serve, so both
/// scanned every live row: on a 100 000-asset library the folder filter cost
/// 142 ms and the panel's list of folders 194 ms. Under this index the filter
/// plans as a range `SEARCH` and costs 0.014 ms.
///
/// `ANALYZE` runs as the data step for the reason [`UPGRADE_17_TO_18`] gives:
/// an index the planner has no statistics for is an index that sits unused.
const UPGRADE_19_TO_20: &str = r#"
    ALTER TABLE assets ADD COLUMN source_path TEXT
        GENERATED ALWAYS AS (json_extract(extra, '$.source_path')) VIRTUAL;
    CREATE INDEX IF NOT EXISTS idx_assets_source_path
        ON assets(source_path COLLATE NOCASE) WHERE trashed_at IS NULL;
"#;

/// v18 → v19: image sequences, as a group over the frames that already exist.
///
/// A sequence is *not* an asset. The frames stay ordinary rows — linked, hashed,
/// searchable, individually restorable — and these two tables only say which of
/// them form a run and in what order. That shape is what makes the feature
/// reversible: dissolving a sequence deletes rows here and changes nothing about
/// the files.
///
/// A side table rather than columns on `assets` for the reason `model_looks`
/// gives: a column would have to be threaded through the row struct, the insert
/// list, the positional reader and every `Asset` literal in the app, for
/// something only the listing's "hide the non-representative frames" predicate
/// joins on.
const UPGRADE_18_TO_19: &str = r#"
    CREATE TABLE IF NOT EXISTS asset_sequences (
        id               TEXT PRIMARY KEY,
        -- The frame the grid shows, which is `position` 0 below. Kept here as
        -- well because every listing needs it and one row read should be enough.
        primary_asset_id TEXT NOT NULL UNIQUE REFERENCES assets(id) ON DELETE CASCADE,
        fps              REAL NOT NULL DEFAULT 30 CHECK (fps >= 1 AND fps <= 240),
        created_at       TEXT NOT NULL,
        updated_at       TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS asset_sequence_frames (
        sequence_id  TEXT NOT NULL REFERENCES asset_sequences(id) ON DELETE CASCADE,
        -- UNIQUE, so a frame belongs to at most one run: a file cannot be a
        -- frame of two sequences, and a listing that hides it would otherwise
        -- have to choose which one it belongs to.
        asset_id     TEXT NOT NULL UNIQUE REFERENCES assets(id) ON DELETE CASCADE,
        -- Display order within the run. 0 is the visible card, anything above
        -- is a hidden member, which is what the listing filter keys on.
        -- (No semicolons anywhere in this DDL's comments: `apply_upgrade`
        -- splits the step on them.)
        position     INTEGER NOT NULL CHECK (position >= 0),
        -- The number written in the file name, which may start anywhere
        -- (`shot0001.exr` → 1) and so is not the same thing as `position`.
        frame_number INTEGER NOT NULL CHECK (frame_number >= 0),
        PRIMARY KEY (sequence_id, position)
    );
"#;

/// v17 → v18: the "filter + sort" partial indexes, and the statistics that
/// make the planner use them.
///
/// Both halves are load-bearing, which is what the measurements say (100k
/// assets, a 200-row page at offset 40k, the live browse):
///
/// | | no statistics | statistics, v17 shape | statistics + these indexes |
/// |---|---|---|---|
/// | newest | 188 ms | 57 ms | **1.9 ms** |
/// | name | 93 ms | 92 ms | **0.36 ms** |
/// | size | 32 ms | 17 ms | **0.40 ms** |
/// | rating | 44 ms | 87 ms | **0.48 ms** |
/// | type=newest | 34 ms | 18 ms | **0.39 ms** |
///
/// Without the indexes the sort is a temp B-tree over the whole live set, per
/// page. Without the statistics the indexes are *there and unused*: `ANALYZE`
/// is what tells SQLite that `idx_assets_trashed` matches every row in the
/// library, so the ordered scan wins on price instead of losing to a guess.
/// (`PRAGMA optimize` is not enough — it samples, and a sample calls
/// `trashed_at` selective at 2001 rows per value when the column is NULL in all
/// 100000 of them. Same wrong plan, 189 ms.)
///
/// Partial (`WHERE trashed_at IS NULL`) because every listing that sorts is a
/// live listing, and the trash is small enough to sort. Each carries `id`
/// second: a listing orders `…, id ASC` so two pages of one browse cannot
/// disagree about which of two same-stamped assets comes first, and an index
/// that cannot supply that tiebreaker buys a temp B-tree anyway.
///
/// Five, not seven: these are the sorts the sort menu and `trove --sort` can
/// ask for. `updated_at`, `duration_ms` and the dominant colour are in the
/// `AssetSort` enum but reachable from neither interface, so an index for them
/// would be 5–7 MB per 100k assets of pure write amplification.
const UPGRADE_17_TO_18: &str = r#"
    CREATE INDEX IF NOT EXISTS idx_assets_live_created
        ON assets(created_at DESC, id ASC) WHERE trashed_at IS NULL;
    CREATE INDEX IF NOT EXISTS idx_assets_live_name
        ON assets(file_name COLLATE NOCASE DESC, id ASC) WHERE trashed_at IS NULL;
    CREATE INDEX IF NOT EXISTS idx_assets_live_size
        ON assets(size_bytes DESC, id ASC) WHERE trashed_at IS NULL;
    CREATE INDEX IF NOT EXISTS idx_assets_live_rating
        ON assets(rating DESC, id ASC) WHERE trashed_at IS NULL;
    CREATE INDEX IF NOT EXISTS idx_assets_live_kind_created
        ON assets(kind, created_at DESC, id ASC) WHERE trashed_at IS NULL;
"#;

/// Gather planner statistics for the whole library.
///
/// Runs as an upgrade step and from [`Store::ensure_statistics`](super::Store::ensure_statistics);
/// re-running is harmless, which is the rule every step obeys. 136 ms at 100k
/// assets — once, on a background open, in exchange for the difference between
/// a 188 ms page and a 2 ms one.
pub fn analyze_statistics(conn: &rusqlite::Connection) -> crate::error::Result<()> {
    conn.execute_batch("ANALYZE;")?;
    Ok(())
}

/// v16 → v17: the 3D viewport's look, per asset.
///
/// A side table rather than a column on `assets`, the way `view_history` is: a
/// column there would have to be threaded through the row struct, the insert
/// list, the positional reader and every `Asset` literal in the app, for a
/// preference that only the viewport reads. `NULL`-by-absence says "the
/// default look" without a sentinel.
const UPGRADE_16_TO_17: &str = r#"
    CREATE TABLE IF NOT EXISTS model_looks (
        asset_id TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
        look     TEXT NOT NULL
    );
"#;

/// v15 → v16: a container's own glyph and accent.
///
/// One nullable JSON column per container table — the shape the smart
/// collection's rule tree already uses, and `NULL` for the folders that ask for
/// nothing. The v15 accent column is dropped once [`migrate_appearance`] has
/// folded it in, so no second source of the same fact is left on disk.
const UPGRADE_15_TO_16: &str = r#"
    ALTER TABLE collections ADD COLUMN appearance TEXT;
    ALTER TABLE smart_collections ADD COLUMN appearance TEXT;
"#;

/// Fold the free-form accent a v15 smart collection could pick onto the named
/// palette, nearest first, then retire the column that held it.
///
/// The translation is a judgment rather than a move, which is why it is here
/// and not in SQL: a stored hex was chosen against whichever theme was open
/// when it was chosen, and the palette entry it maps to is the closest thing
/// this build can say about it. One click in the picker puts the user's
/// intention right; refusing to open the library over it would not.
///
/// Both halves are safe to land twice: only rows with a colour and no
/// appearance are folded, and the column is dropped only while it is there.
fn migrate_appearance(conn: &rusqlite::Connection) -> crate::error::Result<()> {
    use crate::model::{Accent, Appearance};

    if !column_exists(conn, "smart_collections", "color")? {
        return Ok(());
    }
    let doomed: Vec<(String, Appearance)> = {
        let mut stmt =
            conn.prepare("SELECT id, color FROM smart_collections WHERE color IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let hex: String = row.get(1)?;
            Ok((id, hex))
        })?;
        let mut out = Vec::new();
        for row in rows.flatten() {
            if let Some(accent) = Accent::from_hex(&row.1) {
                let appearance = Appearance {
                    glyph: None,
                    accent: Some(accent),
                };
                out.push((row.0, appearance));
            }
        }
        out
    };
    for (id, appearance) in doomed {
        conn.execute(
            "UPDATE smart_collections SET appearance = ?1 WHERE id = ?2 AND appearance IS NULL",
            rusqlite::params![appearance.to_storage(), id],
        )?;
    }
    conn.execute("ALTER TABLE smart_collections DROP COLUMN color", [])?;
    Ok(())
}

/// Whether a table has a column, which is how a step asks whether it has
/// already been applied without guessing at an error message.
pub(super) fn column_exists(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> crate::error::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .flatten()
        .collect::<Vec<String>>();
    Ok(names.iter().any(|name| name == column))
}

/// v14 → v15: the AI analysis cache.
///
/// Purely additive — no existing table is touched — so a v14 library that
/// already carries the table (a hand-edited file, a build that ran ahead of
/// the version bump) migrates cleanly too.
const UPGRADE_14_TO_15: &str = r#"
    CREATE TABLE IF NOT EXISTS ai_analysis (
        asset_id      TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        model_version TEXT NOT NULL,
        result_json   TEXT NOT NULL,
        analysed_at   TEXT NOT NULL,
        PRIMARY KEY (asset_id, model_version)
    );

    CREATE INDEX IF NOT EXISTS idx_ai_analysis_model ON ai_analysis(model_version);
"#;

/// Create the current shape from nothing.
///
/// The table comments are the parts of the old migration comments that still
/// describe the schema rather than the history of getting here.
pub const SCHEMA: &str = r#"
    CREATE TABLE assets (
        id             TEXT PRIMARY KEY,
        -- `linked` for the import mode users reach (the file stays where it
        -- is); `stored` for copies Trove makes for itself, which is the media
        -- package extraction and the re-encoded output of an in-place edit.
        origin         TEXT NOT NULL DEFAULT 'stored'
                       CHECK (origin IN ('stored', 'linked')),
        -- Library-relative blob path. NULL for linked assets, whose original
        -- path rides in `extra` (see the note on that column).
        rel_path       TEXT,
        file_name      TEXT NOT NULL,
        ext            TEXT NOT NULL DEFAULT '',
        mime           TEXT NOT NULL DEFAULT '',
        size_bytes     INTEGER NOT NULL DEFAULT 0,
        content_hash         TEXT,
        kind           TEXT NOT NULL DEFAULT '',
        width          INTEGER,
        height         INTEGER,
        duration_ms    INTEGER,
        captured_at    TEXT,
        title          TEXT,
        description    TEXT,
        -- One of five stars, or NULL for "not rated". A pair of guards keeps
        -- anything else out of this column -- see UPGRADE_22_TO_23.
        rating         INTEGER,
        is_favorite    INTEGER NOT NULL DEFAULT 0,
        source_url     TEXT,
        -- Mined facts as JSON: EXIF, palette, signature, and `source_path` for
        -- linked assets. The folder queries read that one key through the
        -- generated `source_path` column below rather than through
        -- `json_extract`, which no index can serve.
        extra          TEXT NOT NULL DEFAULT '{}',
        created_at     TEXT NOT NULL,
        updated_at     TEXT NOT NULL,
        trashed_at     TEXT,
        -- Workflow state (`model::UsageStatus`) and the tri-state commercial
        -- licence flag (NULL = not verified).
        usage_status   TEXT NOT NULL DEFAULT 'unused',
        commercial_use INTEGER,
        -- Where a linked file lives on disk, lifted out of `extra` so the
        -- folder filter has something to index. VIRTUAL: computed on read, so
        -- it can never disagree with the JSON it comes from and it takes no
        -- space in the row — only [`idx_assets_source_path`] costs anything.
        -- Last in the table because an upgraded library gets it there.
        source_path    TEXT GENERATED ALWAYS AS (json_extract(extra, '$.source_path')) VIRTUAL
    );

    CREATE INDEX idx_assets_trashed  ON assets(trashed_at);
    CREATE INDEX idx_assets_ext      ON assets(ext);
    CREATE INDEX idx_assets_kind     ON assets(kind);
    CREATE INDEX idx_assets_content_hash   ON assets(content_hash);
    CREATE INDEX idx_assets_created  ON assets(created_at);
    CREATE INDEX idx_assets_rating   ON assets(rating);
    CREATE INDEX idx_assets_favorite ON assets(is_favorite);
    CREATE INDEX idx_assets_size     ON assets(size_bytes);

    -- The ordered live listings, and the reason a page of a big library costs
    -- 2 ms rather than 188. Same five shapes [`UPGRADE_17_TO_18`] creates; see
    -- that comment for why each carries `id` second and `trashed_at` as a
    -- partial-index predicate.
    CREATE INDEX idx_assets_live_created ON assets(created_at DESC, id ASC)
        WHERE trashed_at IS NULL;
    CREATE INDEX idx_assets_live_name ON assets(file_name COLLATE NOCASE DESC, id ASC)
        WHERE trashed_at IS NULL;
    CREATE INDEX idx_assets_live_size ON assets(size_bytes DESC, id ASC)
        WHERE trashed_at IS NULL;
    CREATE INDEX idx_assets_live_rating ON assets(rating DESC, id ASC)
        WHERE trashed_at IS NULL;
    CREATE INDEX idx_assets_live_kind_created ON assets(kind, created_at DESC, id ASC)
        WHERE trashed_at IS NULL;

    -- The folder filter and the folders panel, which both ask about
    -- `source_path`. `COLLATE NOCASE` is load-bearing rather than cosmetic:
    -- SQLite turns `LIKE 'prefix%'` into a range scan only when the index
    -- collation matches the comparison's case sensitivity, and `LIKE` is
    -- case-insensitive unless `PRAGMA case_sensitive_like` is switched on. On a
    -- 100 000-asset library the folder filter plans as a `SEARCH` over this
    -- index and costs 0.014 ms; the same generated column under a BINARY index
    -- plans as a full `SCAN` and costs 39 ms. It also loosens nothing — the
    -- `json_extract` this replaced was already a case-insensitive `LIKE`.
    CREATE INDEX idx_assets_source_path ON assets(source_path COLLATE NOCASE)
        WHERE trashed_at IS NULL;

    -- The rating's domain, in the database rather than only in Rust: see
    -- UPGRADE_22_TO_23 for why this is a pair of guards and not a column CHECK.
    CREATE TRIGGER assets_rating_insert_guard
    BEFORE INSERT ON assets
    WHEN NEW.rating IS NOT NULL AND (NEW.rating < 1 OR NEW.rating > 5)
    BEGIN
        SELECT RAISE(ABORT, 'rating must be 1..=5, or NULL for unrated');
    END;

    CREATE TRIGGER assets_rating_update_guard
    BEFORE UPDATE OF rating ON assets
    WHEN NEW.rating IS NOT NULL AND (NEW.rating < 1 OR NEW.rating > 5)
    BEGIN
        SELECT RAISE(ABORT, 'rating must be 1..=5, or NULL for unrated');
    END;

    CREATE TABLE collections (
        id         TEXT PRIMARY KEY,
        parent_id  TEXT REFERENCES collections(id) ON DELETE CASCADE,
        name       TEXT NOT NULL,
        -- This folder's own glyph and accent as JSON, or NULL for the default
        -- folder look (see `crate::model::Appearance`).
        appearance TEXT,
        position   INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );

    CREATE INDEX idx_collections_parent ON collections(parent_id);

    -- Many-to-many: one asset may appear in several collections.
    CREATE TABLE asset_collection (
        asset_id      TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        collection_id TEXT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
        position      INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (asset_id, collection_id)
    );

    CREATE INDEX idx_asset_collection_col ON asset_collection(collection_id);

    -- Hierarchical tags. Deleting a parent promotes its children
    -- (ON DELETE SET NULL) so a subtree is never lost by one click.
    CREATE TABLE tags (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL COLLATE NOCASE UNIQUE,
        color      TEXT,
        created_at TEXT NOT NULL,
        parent_id  TEXT REFERENCES tags(id) ON DELETE SET NULL
    );

    CREATE TABLE asset_tag (
        asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        tag_id   TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
        PRIMARY KEY (asset_id, tag_id)
    );

    CREATE INDEX idx_asset_tag_tag ON asset_tag(tag_id);

    -- `parent_id` may reference either `collections.id` or another
    -- `smart_collections.id`, which a single SQL FK cannot express — existence
    -- and acyclicity are enforced by the store layer instead, and deleting a
    -- parent of either kind cascades to the smart subtree in code
    -- (`smart_collections::delete*`).
    CREATE TABLE smart_collections (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        query      TEXT NOT NULL,
        -- Same column and same type as `collections.appearance`: the folder
        -- tree draws the two alike. It replaces v15's free-form `color`, which
        -- could only ever be right in the theme it was picked in.
        appearance TEXT,
        position   INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        parent_id  TEXT
    );

    CREATE INDEX idx_smart_collections_parent ON smart_collections(parent_id);

    -- Recently-viewed history. One row per asset with the last time it was
    -- selected; `record` upserts so re-viewing bumps `viewed_at`, a cap prunes
    -- the table, and rows vanish with their asset.
    CREATE TABLE view_history (
        asset_id  TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
        viewed_at TEXT NOT NULL
    );

    CREATE INDEX idx_view_history_viewed ON view_history(viewed_at);

    -- The 3D viewport's look, one row per asset the user has tuned: which field
    -- is painted, along which axis, with which colour scale. A *reference* to a
    -- scale by id, so editing or deleting that scale reaches every model that
    -- names it. See [`UPGRADE_16_TO_17`].
    CREATE TABLE IF NOT EXISTS model_looks (
        asset_id TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
        look     TEXT NOT NULL
    );

    -- Image sequences: a group over frames that are ordinary assets, and the
    -- order within it. See [`UPGRADE_18_TO_19`] for why this is a side table and
    -- why dissolving a sequence never touches a file.
    CREATE TABLE IF NOT EXISTS asset_sequences (
        id               TEXT PRIMARY KEY,
        primary_asset_id TEXT NOT NULL UNIQUE REFERENCES assets(id) ON DELETE CASCADE,
        fps              REAL NOT NULL DEFAULT 30 CHECK (fps >= 1 AND fps <= 240),
        created_at       TEXT NOT NULL,
        updated_at       TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS asset_sequence_frames (
        sequence_id  TEXT NOT NULL REFERENCES asset_sequences(id) ON DELETE CASCADE,
        asset_id     TEXT NOT NULL UNIQUE REFERENCES assets(id) ON DELETE CASCADE,
        position     INTEGER NOT NULL CHECK (position >= 0),
        frame_number INTEGER NOT NULL CHECK (frame_number >= 0),
        PRIMARY KEY (sequence_id, position)
    );

    -- The Tantivy outbox: filled by the triggers below on every asset and tag
    -- write, so no mutation site has to remember the index, and the drain
    -- re-derives everything from the rows, so a lost index self-repairs.
    --
    -- No UNIQUE constraint here: SQLite trigger/FK-cascade interactions can
    -- surface spurious UNIQUE errors against it, and the drain is idempotent,
    -- so duplicate pending rows are harmless.
    CREATE TABLE search_queue (
        asset_id TEXT NOT NULL,
        deleted  INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX idx_search_queue_asset ON search_queue(asset_id);

    -- OR REPLACE inside a trigger inherits the outer statement's conflict
    -- resolution (a DELETE carries ABORT), which aborts on a pending row — so
    -- these use OR IGNORE plus an explicit flag update instead.
    CREATE TRIGGER search_queue_assets_insert AFTER INSERT ON assets BEGIN
        INSERT OR IGNORE INTO search_queue(asset_id, deleted) VALUES (new.id, 0);
    END;
    CREATE TRIGGER search_queue_assets_update AFTER UPDATE ON assets BEGIN
        INSERT OR IGNORE INTO search_queue(asset_id, deleted) VALUES (new.id, 0);
        UPDATE search_queue SET deleted = 0 WHERE asset_id = new.id;
    END;
    CREATE TRIGGER search_queue_assets_delete AFTER DELETE ON assets BEGIN
        INSERT OR IGNORE INTO search_queue(asset_id, deleted) VALUES (old.id, 1);
        UPDATE search_queue SET deleted = 1 WHERE asset_id = old.id;
    END;
    CREATE TRIGGER search_queue_asset_tag_insert AFTER INSERT ON asset_tag BEGIN
        INSERT OR IGNORE INTO search_queue(asset_id, deleted) VALUES (new.asset_id, 0);
    END;
    CREATE TRIGGER search_queue_asset_tag_delete AFTER DELETE ON asset_tag BEGIN
        INSERT OR IGNORE INTO search_queue(asset_id, deleted) VALUES (old.asset_id, 0);
    END;
    CREATE TRIGGER search_queue_tags_update AFTER UPDATE ON tags BEGIN
        INSERT OR IGNORE INTO search_queue(asset_id, deleted)
        SELECT asset_id, 0 FROM asset_tag WHERE tag_id = new.id;
    END;

    -- AI embeddings, one unit-normalized vector per (asset, model, space).
    -- `space` separates the text view of an asset from its image view — a
    -- CLIP-style model can legitimately hold both, in the same vector space,
    -- while a plain text model only ever fills 'text'. Vectors are stored
    -- little-endian f32, L2-normalized on write, so similarity search is a
    -- plain dot product over the BLOBs. `dim` guards against a model-config
    -- change quietly mixing vector shapes under one model name; `source_hash`
    -- is the fingerprint of the exact input text (or file) that produced the
    -- vector, so a backfill can skip rows whose inputs have not changed.
    -- Deleting an asset deletes its embeddings with it (CASCADE).
    CREATE TABLE asset_embeddings (
        asset_id    TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        -- Model identity as the provider names it, e.g.
        -- 'text-embedding-3-small'. Everything about comparability hangs off
        -- this string: a query vector is only scored against rows of the
        -- same model and space.
        model       TEXT NOT NULL,
        space       TEXT NOT NULL CHECK (space IN ('text', 'image')),
        dim         INTEGER NOT NULL,
        source_hash TEXT NOT NULL DEFAULT '',
        vector      BLOB NOT NULL,
        updated_at  TEXT NOT NULL,
        PRIMARY KEY (asset_id, model, space)
    );

    CREATE INDEX idx_asset_embeddings_model ON asset_embeddings(model, space);

    -- No `ai_analysis` table. It existed from v15 to v21 and held nothing: no
    -- code ever wrote or read it, because an analysis run records its marker in
    -- `assets.extra` and its undo reads that back. See [`UPGRADE_21_TO_22`].

    -- The task journal: persists task metadata across restarts so the UI can
    -- surface interrupted work and track retry history. See [`UPGRADE_20_TO_21`].
    CREATE TABLE IF NOT EXISTS task_journal (
        task_id      TEXT PRIMARY KEY,
        kind         TEXT NOT NULL,
        label        TEXT NOT NULL,
        status       TEXT NOT NULL,
        done         INTEGER NOT NULL DEFAULT 0,
        total        INTEGER NOT NULL DEFAULT 0,
        summary      TEXT,
        error        TEXT,
        retry_count  INTEGER NOT NULL DEFAULT 0,
        max_retries  INTEGER NOT NULL DEFAULT 0,
        started_at   TEXT NOT NULL,
        finished_at  TEXT
    );
    -- The undo history: one row per invertible metadata mutation, so the history
    -- survives restarting. `undone_at` is the undo/redo boundary — NULL means the
    -- mutation is in effect, and the newest such row is the next thing to take
    -- back. `op` holds `history::undo::Op` as JSON, which is what puts that
    -- enum's field names on disk. See [`UPGRADE_23_TO_24`].
    CREATE TABLE IF NOT EXISTS undo_log (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        action     TEXT NOT NULL,
        target     TEXT,
        count      INTEGER NOT NULL DEFAULT 1,
        op         TEXT NOT NULL,
        created_at TEXT NOT NULL,
        undone_at  TEXT
    );
    CREATE INDEX IF NOT EXISTS undo_log_side ON undo_log(undone_at, seq);
"#;
