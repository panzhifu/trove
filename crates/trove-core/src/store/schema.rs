//! libSQL schema 定义与迁移。
//!
//! 迁移策略：`meta` 表记录 `schema_version`，打开库时按 [`MIGRATIONS`] 逐级应用。
//! 时间戳一律存 Unix 毫秒（INTEGER），ID 与哈希存 TEXT。

/// 当前 schema 版本（等于 [`MIGRATIONS`] 的长度）。
pub const SCHEMA_VERSION: i64 = 1;

/// 迁移列表：`MIGRATIONS[i]` 把库从版本 `i` 升到 `i + 1`。
pub const MIGRATIONS: &[&str] = &[MIGRATION_1];

/// v1：初始 schema。
///
/// 库的 id/name 存于 `library.json` 清单（见 [`crate::Library`]），不进表。
const MIGRATION_1: &str = r#"
CREATE TABLE blobs (
    sha256     TEXT PRIMARY KEY,
    ext        TEXT NOT NULL,
    rel_path   TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    ref_count  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE folders (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    parent_id  TEXT REFERENCES folders(id),
    sort_order INTEGER NOT NULL DEFAULT 0,
    color      TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE assets (
    id          TEXT PRIMARY KEY,
    sha256      TEXT NOT NULL REFERENCES blobs(sha256),
    name        TEXT NOT NULL,
    ext         TEXT NOT NULL,
    folder_id   TEXT REFERENCES folders(id),
    size_bytes  INTEGER NOT NULL,
    width       INTEGER,
    height      INTEGER,
    mime        TEXT,
    rating      INTEGER NOT NULL DEFAULT 0,
    annotation  TEXT NOT NULL DEFAULT '',
    source_url  TEXT,
    is_trashed  INTEGER NOT NULL DEFAULT 0,
    imported_at INTEGER NOT NULL,
    modified_at INTEGER NOT NULL
);

CREATE TABLE tag_groups (
    id    TEXT PRIMARY KEY,
    name  TEXT NOT NULL,
    color TEXT
);

CREATE TABLE tags (
    id       TEXT PRIMARY KEY,
    group_id TEXT NOT NULL REFERENCES tag_groups(id),
    name     TEXT NOT NULL,
    color    TEXT,
    UNIQUE(group_id, name)
);

CREATE TABLE asset_tags (
    asset_id TEXT NOT NULL REFERENCES assets(id),
    tag_id   TEXT NOT NULL REFERENCES tags(id),
    PRIMARY KEY (asset_id, tag_id)
);

CREATE TABLE smart_folders (
    id    TEXT PRIMARY KEY,
    name  TEXT NOT NULL,
    query TEXT NOT NULL
);

CREATE INDEX idx_assets_sha256   ON assets(sha256);
CREATE INDEX idx_assets_folder   ON assets(folder_id);
CREATE INDEX idx_assets_ext      ON assets(ext);
CREATE INDEX idx_assets_rating   ON assets(rating);
CREATE INDEX idx_assets_imported ON assets(imported_at);
CREATE INDEX idx_assets_modified ON assets(modified_at);
CREATE INDEX idx_folders_parent  ON folders(parent_id);
CREATE INDEX idx_tags_group      ON tags(group_id);
CREATE INDEX idx_asset_tags_tag  ON asset_tags(tag_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_matches_migration_count() {
        // 迁移是逐级应用的：MIGRATIONS[i] 把版本 i 升到 i+1，
        // 因此最终版本必须等于迁移条数，否则迁移链断裂。
        assert_eq!(SCHEMA_VERSION, MIGRATIONS.len() as i64);
    }
}
