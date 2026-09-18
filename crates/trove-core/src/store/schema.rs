//! The library schema: one script, one version.
//!
//! `PRAGMA user_version` records the shape a library on disk has. This build
//! *creates* the schema and *refuses* anything else; it does not upgrade
//! libraries written by an earlier build.
//!
//! The migration chain that used to live here was twelve steps of history that
//! only ever served this project's own development: three of them created and
//! dropped an FTS5 table that text search abandoned, two altered in columns for
//! features that no longer exist, and one added a column nothing has read since
//! the workflow state replaced it. Before 0.5 there is nobody to upgrade, so
//! the chain is gone and [`SCHEMA`] is the whole shape. A library from a build
//! before the collapse gets an error naming both versions instead of a
//! half-upgraded database.

/// The schema this build creates. An existing library has to already be at
/// this version to open.
///
/// The number stays at 12 rather than restarting: every library in existence
/// was written by a build whose chain ended here, and the shape below is that
/// same shape (the two columns dropped from it were read by nothing, and SQLite
/// never looks at a column nobody selects). A fresh file and an existing one
/// are therefore the same schema as far as the code is concerned — which is the
/// only sense in which a version number means anything.
pub const SCHEMA_VERSION: i64 = 12;

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
        sha256         TEXT,
        kind           TEXT NOT NULL DEFAULT '',
        width          INTEGER,
        height         INTEGER,
        duration_ms    INTEGER,
        captured_at    TEXT,
        title          TEXT,
        description    TEXT,
        rating         INTEGER,
        is_favorite    INTEGER NOT NULL DEFAULT 0,
        source_url     TEXT,
        -- Mined facts as JSON: EXIF, palette, signature, and `source_path` for
        -- linked assets — which is why folder grouping is a
        -- `json_extract(assets.extra, '$.source_path')` query.
        extra          TEXT NOT NULL DEFAULT '{}',
        created_at     TEXT NOT NULL,
        updated_at     TEXT NOT NULL,
        trashed_at     TEXT,
        -- Workflow state (`model::UsageStatus`) and the tri-state commercial
        -- licence flag (NULL = not verified).
        usage_status   TEXT NOT NULL DEFAULT 'unused',
        commercial_use INTEGER
    );

    CREATE INDEX idx_assets_trashed  ON assets(trashed_at);
    CREATE INDEX idx_assets_ext      ON assets(ext);
    CREATE INDEX idx_assets_kind     ON assets(kind);
    CREATE INDEX idx_assets_sha256   ON assets(sha256);
    CREATE INDEX idx_assets_created  ON assets(created_at);
    CREATE INDEX idx_assets_rating   ON assets(rating);
    CREATE INDEX idx_assets_favorite ON assets(is_favorite);
    CREATE INDEX idx_assets_size     ON assets(size_bytes);

    CREATE TABLE collections (
        id         TEXT PRIMARY KEY,
        parent_id  TEXT REFERENCES collections(id) ON DELETE CASCADE,
        name       TEXT NOT NULL,
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
        color      TEXT,
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
"#;
