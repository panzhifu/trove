//! Schema versioning and DDL.
//!
//! The library tracks its schema with `PRAGMA user_version`. A migration is a
//! full `execute_batch` script applied atomically by the caller. Never edit a
//! released migration; append a new one.

/// Current schema version, bumped whenever a migration is appended.
pub const SCHEMA_VERSION: i64 = 12;

/// One migration per version index: `MIGRATIONS[0]` upgrades 0 -> 1, and so on.
///
/// # The `asset_fts` removal (a documented exception to "never edit a released
/// migration")
///
/// This array used to build a SQLite FTS5 table `asset_fts` in v2, recreate it
/// in v3 and v11, and drop it for good in v12 when text search moved to the
/// Tantivy index (`crate::search`). The three `CREATE VIRTUAL TABLE` statements
/// are now gone: they were dead weight — nothing outside this file has
/// referenced the table since v12 landed, and each create was already followed
/// by a `DROP` in a later migration.
///
/// Removing them keeps the *end state identical* from every starting point: a
/// fresh library never creates the table, one at v1 skips the create and still
/// walks through the drops, and one at v7+ never re-runs these migrations at
/// all. The `DROP TABLE IF EXISTS` lines stay on purpose — libraries on disk
/// today were written by the released (v7) build and genuinely contain the
/// table, so the drops are the only thing that still has work to do.
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
    // v2: smart (saved-search) collections. Historically this migration also
    // created the FTS5 table `asset_fts`; that DDL is gone (see the note above
    // `MIGRATIONS`) because text search now runs on the Tantivy index.
    r#"
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
    // v3: historic FTS5 tag-column migration. Its `CREATE VIRTUAL TABLE` is
    // gone (see the note above `MIGRATIONS`); the drop stays so libraries
    // written by pre-v12 builds shed the table on upgrade.
    r#"
    DROP TABLE IF EXISTS asset_fts;
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
    // v8: hierarchical smart collections. `parent_id` may reference either
    // `collections.id` or another `smart_collections.id`, which a single SQL
    // FK cannot express — existence and acyclicity are enforced by the store
    // layer instead. Deleting a parent (of either kind) cascades to the smart
    // subtree, also handled in code (`smart_collections::delete*`).
    r#"
    ALTER TABLE smart_collections ADD COLUMN parent_id TEXT;

    CREATE INDEX idx_smart_collections_parent ON smart_collections(parent_id);
    "#,
    // v9: usage status (workflow state, `model::UsageStatus`) and the
    // commercial-use tri-state license flag (`NULL` = not verified) replace
    // the v5 color label, which stays as a legacy column that nothing reads
    // or writes anymore.
    r#"
    ALTER TABLE assets ADD COLUMN usage_status TEXT NOT NULL DEFAULT 'unused';

    ALTER TABLE assets ADD COLUMN commercial_use INTEGER;
    "#,
    // v10: sort/filter indexes. Every view orders by created_at, and the
    // toolbar filters and smart rules hit rating, is_favorite and
    // size_bytes; the earlier set only covered trashed_at/ext/kind/sha256.
    r#"
    CREATE INDEX idx_assets_created  ON assets(created_at);
    CREATE INDEX idx_assets_rating   ON assets(rating);
    CREATE INDEX idx_assets_favorite ON assets(is_favorite);
    CREATE INDEX idx_assets_size     ON assets(size_bytes);
    "#,
    // v11: historic trigram-tokenizer migration for the FTS5 index. The
    // `CREATE VIRTUAL TABLE` is gone (see the note above `MIGRATIONS`); the
    // drop stays as cleanup for libraries that still carry the table.
    r#"
    DROP TABLE IF EXISTS asset_fts;
    "#,
    // v12: text search moves out of SQLite into the Tantivy index under
    // `<root>/search_index`, so the FTS5 table is dropped here for the last
    // time. Synchronization goes through a queue table filled by triggers on
    // every asset / tag write (the outbox pattern): no mutation site has to
    // remember the index, and the drain re-derives everything from the rows,
    // so a lost index self-repairs.
    r#"
    DROP TABLE IF EXISTS asset_fts;

    -- No UNIQUE constraint here: SQLite trigger/FK-cascade interactions
    -- can surface spurious UNIQUE errors against it, and the drain is
    -- idempotent, so duplicate pending rows are harmless.
    CREATE TABLE search_queue (
        asset_id TEXT NOT NULL,
        deleted  INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_search_queue_asset ON search_queue(asset_id);

    -- OR REPLACE inside a trigger inherits the outer statement's conflict
    -- resolution (a DELETE carries ABORT), which aborts on a pending row —
    -- so these use OR IGNORE plus an explicit flag update instead.
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
    "#,
];
