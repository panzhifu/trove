//! Library statistics: the counts the settings dashboard shows. Aggregated
//! in one pass over cheap `COUNT` queries.

use super::rows;
use crate::error::Result;
use crate::model::AssetKind;

/// Snapshot of library sizes for the settings UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryStats {
    /// Live (not trashed) assets.
    pub live: u64,
    /// Assets in the trash.
    pub trashed: u64,
    /// Live assets per kind, canonical kind order, only non-zero entries.
    pub by_kind: Vec<(AssetKind, u64)>,
    /// Sum of live asset file sizes.
    pub total_bytes: u64,
    pub tags: u64,
    pub collections: u64,
    pub smart_collections: u64,
}

/// Gather the stats snapshot.
pub fn library_stats(conn: &rusqlite::Connection) -> Result<LibraryStats> {
    let mut stats = LibraryStats::default();

    let mut counts: [u64; 8] = [0; 8];
    {
        let mut stmt = conn.prepare(
            "SELECT kind, COUNT(*) FROM assets \
             WHERE trashed_at IS NULL GROUP BY kind",
        )?;
        let mut it = stmt.query([])?;
        while let Some(row) = it.next()? {
            let kind_raw: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            counts[kind_index(&parse_kind(&kind_raw))] += count.max(0) as u64;
        }
    }

    stats.by_kind = counts
        .into_iter()
        .zip([
            AssetKind::Image,
            AssetKind::Video,
            AssetKind::Audio,
            AssetKind::Document,
            AssetKind::Archive,
            AssetKind::Font,
            AssetKind::Model,
            AssetKind::Other,
        ])
        .filter(|(count, _)| *count > 0)
        .map(|(count, kind)| (kind, count))
        .collect();

    stats.total_bytes = rows::query_count(
        conn,
        "SELECT COALESCE(SUM(size_bytes), 0) FROM assets WHERE trashed_at IS NULL",
        vec![],
    )? as u64;

    stats.live = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE trashed_at IS NULL",
        vec![],
    )? as u64;
    stats.trashed = rows::query_count(
        conn,
        "SELECT COUNT(*) FROM assets WHERE trashed_at IS NOT NULL",
        vec![],
    )? as u64;
    stats.tags = rows::query_count(conn, "SELECT COUNT(*) FROM tags", vec![])? as u64;
    stats.collections = rows::query_count(conn, "SELECT COUNT(*) FROM collections", vec![])? as u64;
    stats.smart_collections =
        rows::query_count(conn, "SELECT COUNT(*) FROM smart_collections", vec![])? as u64;

    Ok(stats)
}

fn parse_kind(s: &str) -> AssetKind {
    match s {
        "image" => AssetKind::Image,
        "video" => AssetKind::Video,
        "audio" => AssetKind::Audio,
        "document" => AssetKind::Document,
        "archive" => AssetKind::Archive,
        "font" => AssetKind::Font,
        "model" => AssetKind::Model,
        _ => AssetKind::Other,
    }
}

/// Canonical kind order used for the `by_kind` vector.
fn kind_index(kind: &AssetKind) -> usize {
    match kind {
        AssetKind::Image => 0,
        AssetKind::Video => 1,
        AssetKind::Audio => 2,
        AssetKind::Document => 3,
        AssetKind::Archive => 4,
        AssetKind::Font => 5,
        AssetKind::Model => 6,
        AssetKind::Other => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_asset;
    use crate::store::Store;
    use uuid::Uuid;

    #[test]
    fn stats_aggregate_kinds_and_counts() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        for (name, kind) in [
            ("a.png", AssetKind::Image),
            ("b.png", AssetKind::Image),
            ("c.mp3", AssetKind::Audio),
        ] {
            let mut asset = test_asset(name, kind, Uuid::new_v4());
            asset.size_bytes = 100;
            crate::store::assets::insert(conn, &asset).unwrap();
        }
        // One trashed asset must not count as live.
        let mut gone = test_asset("gone.png", AssetKind::Image, Uuid::new_v4());
        gone.size_bytes = 500;
        crate::store::assets::insert(conn, &gone).unwrap();
        crate::store::assets::set_trashed(conn, gone.id, true).unwrap();

        let stats = library_stats(conn).unwrap();
        assert_eq!(stats.live, 3);
        assert_eq!(stats.trashed, 1);
        assert_eq!(
            stats.by_kind,
            vec![(AssetKind::Image, 2), (AssetKind::Audio, 1)]
        );
        assert_eq!(stats.total_bytes, 300);
        assert_eq!(stats.tags, 0);
        assert_eq!(stats.collections, 0);
    }
}
