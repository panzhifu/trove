//! The 3D viewport's look, per asset.
//!
//! One row per model whose colours the user has tuned in the viewport's panel.
//! Like [`super::view_history`] this is a side table rather than a column on
//! `assets`: a row exists only once the default has been left behind, so a model
//! nobody tuned is not a special case anywhere in the reading code — and a
//! column there would have to be threaded through the row struct, the insert
//! list, the positional reader and every `Asset` literal in the app.
//!
//! The stored text *names* a colour scale rather than copying it
//! ([`StoredLook`]), which is what lets one edit in the scale editor change
//! every model that uses that scale, and what makes a deleted scale fall back to
//! the built-in default instead of leaving a model painted from a stale copy.

use rusqlite::Connection;
use uuid::Uuid;

use super::rows;
use crate::error::Result;
use crate::media::height_color::StoredLook;

/// The look this asset was last left in, or `None` while it still uses the
/// default.
///
/// A value this build cannot read reads as the default rather than failing the
/// listing: a model's colours are not worth refusing to open a library over, and
/// the same happens at draw time to a scale that has been deleted.
pub fn get(conn: &Connection, asset_id: Uuid) -> Result<Option<StoredLook>> {
    let stored: Option<Option<String>> = rows::query_one(
        conn,
        "SELECT look FROM model_looks WHERE asset_id = ?1",
        vec![rows::uuid(asset_id).into()],
        |row| Ok(row.get::<_, Option<String>>(0)?),
    )?;
    let look = stored
        .flatten()
        .map(|text| StoredLook::from_storage(Some(&text)));
    // The default is stored as no row, so a row that has somehow come to hold it
    // reads the same way rather than as a look of its own.
    Ok(look.filter(|look| look != &StoredLook::default()))
}

/// Keep `look` as this asset's own, or drop the row once it is back to the
/// default. Writing the default and writing nothing are the same fact.
pub fn set(conn: &Connection, asset_id: Uuid, look: &StoredLook) -> Result<()> {
    match look.to_storage() {
        Some(text) => {
            rows::execute(
                conn,
                "INSERT INTO model_looks (asset_id, look) VALUES (?1, ?2) \
                 ON CONFLICT(asset_id) DO UPDATE SET look = excluded.look",
                vec![rows::uuid(asset_id).into(), text.into()],
            )?;
        }
        None => {
            rows::execute(
                conn,
                "DELETE FROM model_looks WHERE asset_id = ?1",
                vec![rows::uuid(asset_id).into()],
            )?;
        }
    }
    Ok(())
}
