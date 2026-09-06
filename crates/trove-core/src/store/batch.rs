//! Batch asset mutations. Each operation updates many assets in a single
//! statement (atomic, no application-level loop over individual rows), or is
//! otherwise idempotent. Empty id lists are a no-op returning `0`.
//!
//! The `Library` facade wraps these for the UI; panels should never loop over
//! ids themselves.

use chrono::Utc;
use libsql::Connection;
use uuid::Uuid;

use super::rows;
use crate::error::Result;

/// Trash or restore many assets in one statement.
pub fn set_trashed_many(conn: &Connection, ids: &[Uuid], trashed: bool) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut sql =
        String::from("UPDATE assets SET trashed_at = ?1, updated_at = ?2 WHERE id IN (");
    let mut args = vec![
        if trashed {
            rows::ts(Utc::now()).into()
        } else {
            libsql::Value::Null
        },
        rows::ts(Utc::now()).into(),
    ];

    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        // Placeholder numbers continue after the two leading args.
        sql.push_str(&format!("?{}", i + 3));
        args.push(rows::uuid(*id).into());
    }
    sql.push(')');
    rows::execute(conn, &sql, args)
}

/// Favorite / unfavorite many assets in one statement.
pub fn set_favorite_many(conn: &Connection, ids: &[Uuid], favorite: bool) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut sql =
        String::from("UPDATE assets SET is_favorite = ?1, updated_at = ?2 WHERE id IN (");
    let mut args = vec![
        libsql::Value::Integer(favorite as i64),
        rows::ts(Utc::now()).into(),
    ];

    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("?{}", i + 3));
        args.push(rows::uuid(*id).into());
    }
    sql.push(')');
    rows::execute(conn, &sql, args)
}

/// Attach many assets to a collection. Idempotent (`INSERT OR IGNORE`); returns
/// the number of ids processed.
pub fn add_to_collection_many(
    conn: &Connection,
    collection_id: Uuid,
    ids: &[Uuid],
) -> Result<u64> {
    for id in ids {
        super::collections::add_asset(conn, collection_id, *id)?;
    }
    Ok(ids.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Asset, AssetKind, Origin, now};
    use crate::store::Store;

    fn sample_asset(store: &Store, name: &str, kind: AssetKind) -> Uuid {
        let id = Uuid::new_v4();
        let asset = Asset {
            id,
            origin: Origin::Stored,
            rel_path: Some(format!("media/{}/{}", &id.to_string()[..2], name)),
            file_name: name.to_string(),
            ext: "png".into(),
            mime: "image/png".into(),
            size_bytes: 128,
            sha256: Some("a".repeat(64)),
            kind,
            width: Some(1),
            height: Some(1),
            duration_ms: None,
            captured_at: None,
            title: None,
            description: None,
            rating: None,
            is_favorite: false,
            source_url: None,
            extra: Default::default(),
            created_at: now(),
            updated_at: now(),
            trashed_at: None,
        };
        crate::store::assets::insert(store.conn(), &asset).unwrap();
        id
    }

    #[test]
    fn batch_trash_restore_and_favorite() {
        let store = Store::in_memory().unwrap();
        let a = sample_asset(&store, "a.png", AssetKind::Image);
        let b = sample_asset(&store, "b.png", AssetKind::Image);

        assert_eq!(set_trashed_many(store.conn(), &[], true).unwrap(), 0);
        assert_eq!(set_trashed_many(store.conn(), &[a, b], true).unwrap(), 2);
        let (_, trashed) = crate::store::assets::query(
            store.conn(),
            &crate::model::AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trashed.len(), 2);

        // Restore only one.
        assert_eq!(set_trashed_many(store.conn(), &[a], false).unwrap(), 1);
        let (_, live) = crate::store::assets::query(
            store.conn(),
            &crate::model::AssetQuery::default(),
        )
        .unwrap();
        assert_eq!(live.len(), 1);

        // Bring `b` back so both are live, then favorite both in one statement.
        assert_eq!(set_trashed_many(store.conn(), &[b], false).unwrap(), 1);
        assert_eq!(set_favorite_many(store.conn(), &[a, b], true).unwrap(), 2);
        let (_, all) = crate::store::assets::query(
            store.conn(),
            &crate::model::AssetQuery {
                is_favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn batch_add_to_collection_is_idempotent() {
        let store = Store::in_memory().unwrap();
        let a = sample_asset(&store, "a.png", AssetKind::Image);
        let c = crate::store::collections::create(
            store.conn(),
            &crate::model::NewCollection {
                parent_id: None,
                name: "album".into(),
                position: 0,
            },
        )
        .unwrap();

        assert_eq!(add_to_collection_many(store.conn(), c.id, &[a]).unwrap(), 1);
        assert_eq!(add_to_collection_many(store.conn(), c.id, &[a, a]).unwrap(), 2);
        assert_eq!(crate::store::collections::count_assets(store.conn(), c.id).unwrap(), 1);
    }
}