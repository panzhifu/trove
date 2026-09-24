//! Image sequences: the group rows, and the one clause that hides their members.
//!
//! A sequence is a *group over assets that already exist*. Nothing here copies,
//! moves or rewrites a file: the frames stay ordinary rows (linked, hashed,
//! searchable, individually restorable) and these two tables only record which of
//! them form a run, in what order, and at what frame rate. That is what makes the
//! feature reversible — dissolving a sequence deletes rows and changes nothing
//! else — and it is why the listing filter is the load-bearing part of the whole
//! design (see [`HIDDEN_FRAMES`]).

use rusqlite::Connection;
use uuid::Uuid;

use super::rows;
use crate::error::{Error, Result};
use crate::media::sequence;

/// The clause that keeps a sequence's hidden frames out of a listing.
///
/// `position > 0` is the whole rule: the first frame is the card, the rest are
/// members of it. It is a `NOT EXISTS` rather than a join because every listing
/// already has its own shape (browse, search-rank, recent, smart) and this has to
/// drop into all of them without changing what they select.
///
/// Applied on the live branch only. A trashed frame must list and restore on its
/// own, and `empty_trash` enumerates through the same builder — filtering there
/// would strand a hundred and forty-nine members' files on disk forever.
pub const HIDDEN_FRAMES: &str = "NOT EXISTS (SELECT 1 FROM asset_sequence_frames f \
                                 WHERE f.asset_id = assets.id AND f.position > 0)";

/// The same rule as [`HIDDEN_FRAMES`], written against an arbitrary id column.
///
/// The counting surfaces that read the junction tables directly — a tag's asset
/// count, a collection's, a folder rollup, the library totals — cannot reuse the
/// listing's clause verbatim, and a number that counts a hidden frame is a number
/// that disagrees with the one card the grid shows for it.
pub fn hidden_beside(id_column: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM asset_sequence_frames f \
         WHERE f.asset_id = {id_column} AND f.position > 0)"
    )
}

/// What a frame belongs to, as the surfaces that show it need it.
#[derive(Debug, Clone, PartialEq)]
pub struct Membership {
    pub sequence_id: Uuid,
    /// Display order within the run: `0` is the card the grid shows.
    pub position: i64,
    /// Frames in the run, in display order.
    pub frames: Vec<Uuid>,
    fps: f64,
}

impl Membership {
    /// Frames per second, for the player and the card cycle.
    pub fn fps(&self) -> f64 {
        self.fps
    }
}

/// Group `ids` into one sequence and return its id.
///
/// The rules are the ones a user would expect from a folder of renders, and each
/// refusal names the thing that went wrong rather than reporting a count:
/// at least three frames, all of them live, none already a member of another
/// run, all from one directory, and — when the frames carry dimensions at all —
/// the same dimensions. A run whose frames differ in size is a mistake or two
/// shots, and animating it would show the mistake as a flicker.
pub fn create(conn: &Connection, ids: &[Uuid], fps: f64) -> Result<Uuid> {
    if ids.len() < sequence::MIN_FRAMES {
        return Err(Error::Validation(format!(
            "a sequence needs at least {} frames",
            sequence::MIN_FRAMES
        )));
    }
    if !(1.0..=240.0).contains(&fps) {
        return Err(Error::Validation(
            "frame rate must be between 1 and 240".into(),
        ));
    }

    let mut rows_out: Vec<(Uuid, String, Option<u32>, Option<u32>)> = Vec::new();
    for &id in ids {
        let found: Option<(String, Option<u32>, Option<u32>)> = rows::query_one(
            conn,
            "SELECT COALESCE(json_extract(extra, '$.source_path'), ''), width, height
             FROM assets WHERE id = ?1 AND trashed_at IS NULL",
            vec![rows::uuid(id).into()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<u32>>(1)?,
                    row.get::<_, Option<u32>>(2)?,
                ))
            },
        )?;
        let (path, width, height) = found.ok_or_else(|| {
            Error::Validation("every frame of a sequence has to be a live asset".into())
        })?;
        rows_out.push((id, path, width, height));
    }

    // One directory. A run that spans folders is two shots, and merging them
    // would turn a library into one long clip.
    let directory = |path: &str| {
        std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let first_directory = directory(&rows_out[0].1);
    if rows_out
        .iter()
        .any(|(_, path, _, _)| directory(path) != first_directory)
    {
        return Err(Error::Validation(
            "a sequence's frames all have to come from the same folder".into(),
        ));
    }

    // Dimensions, when they are known at all: an asset imported before the
    // probe ran has none, and that is not a reason to refuse the run.
    let sizes: Vec<(u32, u32)> = rows_out
        .iter()
        .filter_map(|(_, _, width, height)| match (width, height) {
            (Some(width), Some(height)) => Some((*width, *height)),
            _ => None,
        })
        .collect();
    if sizes.len() > 1 && sizes.iter().any(|size| *size != sizes[0]) {
        return Err(Error::Validation(
            "the frames are not all the same size".into(),
        ));
    }

    // Already a member of something? The frame column is UNIQUE, so the answer
    // is always at most one — and the message says which run it is in.
    let taken: Option<Uuid> = rows::query_one(
        conn,
        &format!(
            "SELECT f.sequence_id FROM asset_sequence_frames f
             WHERE f.asset_id IN ({}) LIMIT 1",
            (0..ids.len())
                .map(|ix| format!("?{}", ix + 1))
                .collect::<Vec<_>>()
                .join(",")
        ),
        ids.iter().map(|id| rows::uuid(*id).into()).collect(),
        |row| rows::parse_uuid(&row.get::<_, String>(0)?),
    )?;
    if let Some(existing) = taken {
        return Err(Error::Validation(format!(
            "one of those frames already belongs to sequence {existing}"
        )));
    }

    // Display order: the number the file name carries, which is what a render
    // means by "frame 12". Names without a usable number fall back to their own
    // order, so a hand-picked set never loses to a parsing rule.
    let key = |path: &str| -> (String, u64, String) {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let stem = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let number = sequence::frame_number(std::path::Path::new(path)).unwrap_or(u64::MAX);
        (
            stem.trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string(),
            number,
            name,
        )
    };
    rows_out.sort_by_key(|entry| key(&entry.1));

    let id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO asset_sequences (id, primary_asset_id, fps, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
        rusqlite::params![rows::uuid(id), rows::uuid(rows_out[0].0), fps, now],
    )?;
    for (position, (frame, path, _, _)) in rows_out.iter().enumerate() {
        // The number as written in the name, which may start anywhere; a name
        // without one keeps its display position instead.
        let number = sequence::frame_number(std::path::Path::new(path)).unwrap_or(position as u64);
        conn.execute(
            "INSERT INTO asset_sequence_frames (sequence_id, asset_id, position, frame_number)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![rows::uuid(id), rows::uuid(*frame), position as i64, number],
        )?;
    }
    Ok(id)
}

/// Delete the group rows for these sequences. The frames themselves are left
/// exactly as they were — that is the point of the shape.
pub fn dissolve(conn: &Connection, ids: &[Uuid]) -> Result<usize> {
    let mut removed = 0usize;
    for id in ids {
        conn.execute(
            "DELETE FROM asset_sequences WHERE id = ?1",
            [rows::uuid(*id)],
        )?;
        removed += 1;
    }
    Ok(removed)
}

/// Change a run's frame rate. The table's `CHECK` is the range guard; this says
/// so in words the UI can show.
pub fn set_fps(conn: &Connection, id: Uuid, fps: f64) -> Result<()> {
    if !(1.0..=240.0).contains(&fps) {
        return Err(Error::Validation(
            "frame rate must be between 1 and 240".into(),
        ));
    }
    let changed = conn.execute(
        "UPDATE asset_sequences SET fps = ?2, updated_at = ?3 WHERE id = ?1",
        rusqlite::params![rows::uuid(id), fps, chrono::Utc::now().to_rfc3339()],
    )?;
    if changed == 0 {
        return Err(Error::Validation("no such sequence".into()));
    }
    Ok(())
}

/// The run this asset is a frame of, if it is one.
pub fn membership(conn: &Connection, asset_id: Uuid) -> Result<Option<Membership>> {
    let found: Option<(String, i64)> = rows::query_one(
        conn,
        "SELECT f.sequence_id, f.position FROM asset_sequence_frames f WHERE f.asset_id = ?1",
        vec![rows::uuid(asset_id).into()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let Some((raw, position)) = found else {
        return Ok(None);
    };
    let sequence_id = rows::parse_uuid(&raw)?;
    // One query for both halves of the answer: the run's frame order and its
    // rate, which sits on the group row and so repeats per frame.
    let mut statement = conn.prepare(
        "SELECT f.asset_id, s.fps FROM asset_sequence_frames f
         JOIN asset_sequences s ON s.id = f.sequence_id
         WHERE f.sequence_id = ?1 ORDER BY f.position",
    )?;
    let collected: Vec<(String, f64)> = statement
        .query_map(rusqlite::params![rows::uuid(sequence_id)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .filter_map(|row| row.ok())
        .collect();
    drop(statement);
    let fps = collected.first().map(|entry| entry.1).unwrap_or(30.0);
    let frames = collected
        .iter()
        .filter_map(|(text, _)| Uuid::parse_str(text).ok())
        .collect();
    Ok(Some(Membership {
        sequence_id,
        position,
        frames,
        fps,
    }))
}

/// Every asset that is a member of a sequence, with its own id. Used by the
/// import path to skip work for frames the grid will never show on its own.
pub fn hidden_members(conn: &Connection) -> Result<Vec<Uuid>> {
    let mut statement = conn.prepare(&format!(
        "SELECT f.asset_id FROM asset_sequence_frames f WHERE f.position > 0 AND {HIDDEN_FRAMES}"
    ))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        if let Ok(uuid) = Uuid::parse_str(&row?) {
            out.push(uuid);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Asset, AssetKind};
    use crate::store::{Store, assets};
    use std::path::Path;

    fn store(label: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("trove-seq-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join("library.db")).expect("a store")
    }

    /// A frame as the library would hold it: linked, with the source path the
    /// same-directory rule reads.
    fn frame(dir: &Path, name: &str, id: Uuid) -> Asset {
        let path = dir.join(name);
        std::fs::write(&path, b"frame").unwrap();
        let mut asset = crate::model::test_asset(name, AssetKind::Image, id);
        asset.facts.source_path = Some(path.to_string_lossy().to_string());
        asset.width = Some(64);
        asset.height = Some(64);
        asset
    }

    fn insert(conn: &Connection, asset: &Asset) {
        assets::insert(conn, asset).unwrap();
    }

    #[test]
    fn a_run_of_frames_becomes_one_sequence() {
        let store = store("create");
        let conn = store.conn();
        let dir = std::env::temp_dir().join(format!("trove-seq-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let ids: Vec<Uuid> = (1..=6).map(|_| Uuid::new_v4()).collect();
        for (id, name) in ids.iter().zip((1..=6).map(|i| format!("shot{i:04}.exr"))) {
            insert(conn, &frame(&dir, &name, *id));
        }
        // Inserted out of order on purpose: the run is ordered by the number in
        // the name, not by whatever order the rows arrived in.
        let mut picked = ids.clone();
        picked.reverse();
        let sequence_id = create(conn, &picked, 24.0).expect("a sequence");

        let first = membership(conn, ids[0]).unwrap().expect("membership");
        assert_eq!(first.sequence_id, sequence_id);
        assert_eq!(first.position, 0, "shot0001 is the card");
        assert_eq!(first.frames, ids, "and the run is in frame order");
        assert_eq!(first.fps(), 24.0);

        let last = membership(conn, ids[5]).unwrap().expect("membership");
        assert_eq!(last.position, 5, "the tail is a hidden member");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of the listing filter: the card is one row, the members
    /// are not, and the total says the same.
    #[test]
    fn hidden_members_leave_the_listing_and_the_count() {
        let store = store("hide");
        let conn = store.conn();
        let dir = std::env::temp_dir().join(format!("trove-seq-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let ids: Vec<Uuid> = (1..=6).map(|_| Uuid::new_v4()).collect();
        for (id, name) in ids.iter().zip((1..=6).map(|i| format!("f{i:03}.png"))) {
            insert(conn, &frame(&dir, &name, *id));
        }
        let query = crate::model::AssetQuery::default();
        assert_eq!(assets::count(conn, &query).unwrap(), 6, "before");
        create(conn, &ids, 30.0).unwrap();

        let listed = assets::query(conn, &query).unwrap();
        assert_eq!(listed.total, 1, "one card for the run");
        assert_eq!(listed.items[0].id, ids[0]);

        // The surfaces that do not go through the listing's builder have to
        // reach the same number: a sidebar total that counts the hidden frames
        // disagrees with the cards under it, and that is the bug the clause
        // exists to avoid rather than one it fixes.
        let stats = crate::store::stats::library_stats(conn).unwrap();
        assert_eq!(stats.live, 1, "the library total is the card count");
        assert_eq!(
            stats
                .by_kind
                .iter()
                .find(|(kind, _)| *kind == AssetKind::Image)
                .map(|(_, count)| *count),
            Some(1),
            "and so is the per-kind rollup"
        );

        // Dissolving puts every frame back on its own, files untouched.
        dissolve(conn, &[ids[0]]).ok();
        let sequence_id = {
            let m = membership(conn, ids[1]).unwrap();
            m.map(|m| m.sequence_id)
        };
        if let Some(sequence_id) = sequence_id {
            dissolve(conn, &[sequence_id]).unwrap();
        }
        assert_eq!(assets::count(conn, &query).unwrap(), 6, "after dissolving");
        assert!(
            std::fs::read_dir(&dir).unwrap().count() >= 6,
            "dissolving never touches a file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_trashed_member_is_still_listed_and_purgeable() {
        let store = store("trash");
        let conn = store.conn();
        let dir = std::env::temp_dir().join(format!("trove-seq-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ids: Vec<Uuid> = (1..=4).map(|_| Uuid::new_v4()).collect();
        for (id, name) in ids.iter().zip((1..=4).map(|i| format!("t{i}.png"))) {
            insert(conn, &frame(&dir, &name, *id));
        }
        create(conn, &ids, 30.0).unwrap();
        assets::set_trashed(conn, ids[3], true).unwrap();

        let trashed = assets::query(
            conn,
            &crate::model::AssetQuery {
                is_trashed: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            trashed.items.iter().any(|asset| asset.id == ids[3]),
            "a trashed frame has to be restorable on its own"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each refusal names the thing that went wrong, because the gesture that
    /// hits them is a user picking files by hand.
    #[test]
    fn the_refusals_are_specific() {
        let store = store("refuse");
        let conn = store.conn();
        let dir = std::env::temp_dir().join(format!("trove-seq-src-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let two = [Uuid::new_v4(), Uuid::new_v4()];
        assert!(create(conn, &two, 30.0).is_err(), "two is a pair");

        let ids: Vec<Uuid> = (1..=3).map(|_| Uuid::new_v4()).collect();
        for (id, name) in ids.iter().zip(["a1.png", "a2.png", "a3.png"]) {
            insert(conn, &frame(&dir, name, *id));
        }
        let sequence_id = create(conn, &ids, 30.0).unwrap();
        assert!(
            create(conn, &ids, 30.0).is_err(),
            "a frame cannot be in two runs"
        );

        // Same run, different folder: refused.
        let other = std::env::temp_dir().join(format!("trove-seq-other-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&other).unwrap();
        let mut mixed = ids.clone();
        mixed.push(Uuid::new_v4());
        insert(conn, &frame(&other, "a4.png", *mixed.last().unwrap()));
        assert!(
            create(conn, &mixed, 30.0).is_err(),
            "a run does not span folders"
        );

        // Different sizes: refused, and the message says why.
        let wide = Uuid::new_v4();
        let mut asset = frame(&dir, "b1.png", wide);
        asset.width = Some(128);
        insert(conn, &asset);
        let b = [wide, ids[0], ids[1]];
        let error = create(conn, &b, 30.0).expect_err("a refusal");
        assert!(
            error.to_string().contains("size"),
            "the refusal names the problem: {error}"
        );

        // Frame rate is bounded by the table's own CHECK.
        assert!(set_fps(conn, sequence_id, 0.).is_err());
        assert!(set_fps(conn, sequence_id, 60.0).is_ok());
        assert_eq!(membership(conn, ids[0]).unwrap().unwrap().fps(), 60.0);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }
}
