//! Schema versioning and DDL.
//!
//! The library tracks its schema with `PRAGMA user_version`. A migration is a
//! full `execute_batch` script applied atomically by the caller. Never edit a
//! released migration; append a new one.

/// Current schema version, bumped whenever a migration is appended.
pub const SCHEMA_VERSION: i64 = 7;

/// One migration per version index: `MIGRATIONS[0]` upgrades 0 -> 1, and so on.
pub const MIGRATIONS: &[&str] = &[
    // v1: initial asset library.
    r#"
    CREATE TABLE assets (
        id          TEXT PRIMARY KEY,
        origin      TEXT NOT NULL DEFAULT 'stored'
                    CHECK (origin IN ('stored', 'linked')),
        rel_path    TEXT,
        file_name   TEXT NOT NULL,
        ext         TEXT NOT NULL DEFAULT '',
        mime        TEXT NOT NULL DEFAULT '',
        size_bytes  INTEGER NOT NULL DEFAULT 0,
        sha256      TEXT,
        kind        TEXT NOT NULL DEFAULT '',
        width       INTEGER,
        height      INTEGER,
        duration_ms INTEGER,
        captured_at TEXT,
        title       TEXT,
        description TEXT,
        rating      INTEGER,
        is_favorite INTEGER NOT NULL DEFAULT 0,
        source_url  TEXT,
        extra       TEXT NOT NULL DEFAULT '{}',
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        trashed_at  TEXT
    );

    CREATE INDEX idx_assets_trashed ON assets(trashed_at);
    CREATE INDEX idx_assets_ext      ON assets(ext);
    CREATE INDEX idx_assets_kind     ON assets(kind);
    CREATE INDEX idx_assets_sha256   ON assets(sha256);

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

    CREATE TABLE tags (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL COLLATE NOCASE UNIQUE,
        color      TEXT,
        created_at TEXT NOT NULL
    );

    CREATE TABLE asset_tag (
        asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
        tag_id   TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
        PRIMARY KEY (asset_id, tag_id)
    );

    CREATE INDEX idx_asset_tag_tag ON asset_tag(tag_id);
    "#,
    // v2: full-text search index + smart (saved-search) collections.
    // The FTS table mirrors live+trashed assets; search filters live rows at
    // query time so trash/restore never touch the index.
    r#"
    CREATE VIRTUAL TABLE asset_fts USING fts5(
        asset_id UNINDEXED,
        file_name, title, description,
        tokenize = 'unicode61'
    );

    CREATE TABLE smart_collections (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        query      TEXT NOT NULL,
        color      TEXT,
        position   INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
    // v3: tag names join the full-text index. FTS5 tables cannot be altered,
    // so the index is dropped and recreated; the Library backfill (which runs
    // after migrations) rebuilds it from the live asset rows.
    r#"
    DROP TABLE IF EXISTS asset_fts;

    CREATE VIRTUAL TABLE asset_fts USING fts5(
        asset_id UNINDEXED,
        file_name, title, description, tags,
        tokenize = 'unicode61'
    );
    "#,
    // v4: CLIP embedding column for semantic search. BLOB stores the
    // 512/768-dim float32 vector (L2-normalized). NULL when not yet computed.
    // LEGACY: CLIP was removed; visual search now uses pHash + colour
    // signatures in `extra`. The column stays (SQLite cannot drop columns
    // on old versions) but nothing reads or writes it.
    r#"
    ALTER TABLE assets ADD COLUMN embedding BLOB;
    "#,
    // v5: per-asset color label (Lightroom/Bridge-style flag). Stores one of
    // `model::COLOR_LABELS` ("red" … "purple"); NULL = unlabeled.
    r#"
    ALTER TABLE assets ADD COLUMN color_label TEXT;
    "#,
    // v6: hierarchical tags. Deleting a parent promotes its children
    // (ON DELETE SET NULL) so a subtree is never lost by one click.
    r#"
    ALTER TABLE tags ADD COLUMN parent_id TEXT REFERENCES tags(id) ON DELETE SET NULL;
    "#,
    // v7: recently-viewed history. One row per asset with the last time it
    // was selected; `record` upserts so re-viewing bumps `viewed_at`, and a
    // cap prunes the table to the most recent entries. Rows vanish with
    // their asset (ON DELETE CASCADE).
    r#"
    CREATE TABLE view_history (
        asset_id  TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
        viewed_at TEXT NOT NULL
    );

    CREATE INDEX idx_view_history_viewed ON view_history(viewed_at);
    "#,
];
