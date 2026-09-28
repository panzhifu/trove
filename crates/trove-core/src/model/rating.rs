//! The star rating: one number the user picked out of five.
//!
//! The rating is not a measurement. Nothing in a file says how good it is; the
//! value exists only because somebody dragged a pointer over five glyphs and
//! stopped on one of them. That is why its domain is `1..=5` and why "no rating"
//! is a separate fact rather than the number zero: an unrated asset was never
//! judged, and `0` would be a judgement a star row cannot even draw.
//!
//! Two halves enforce that, and they are deliberately not the same strength:
//!
//! - **In Rust**, [`Rating`] cannot hold anything else, so the value a writer
//!   produces is always showable. The old spelling was `Option<u8>` with a
//!   bounds check living inside [`crate::model::AssetPatch::validate`] — a rule
//!   one function remembered and every other constructor, importer and AI
//!   analysis job was free to forget.
//! - **In SQLite**, a guard trigger aborts a write outside that domain
//!   (`store::schema`'s v22→v23 step). It is the only half that reaches a
//!   database this build did not write, and it is a trigger rather than a column
//!   `CHECK` because adding a `CHECK` to an existing column means recreating the
//!   table — and with it the twenty-one indexes and the three outbox triggers
//!   hanging off `assets`.
//!
//! The read side meets rows that predate both, or were edited by hand. A value
//! outside the domain is degraded to "unrated" there rather than becoming an
//! error: the row is still a file the user has, and one stray number is not a
//! reason to lose it from a listing. See
//! [`crate::store::assets`]’s rating read.

use serde::{Deserialize, Serialize};

/// The highest rating a row can carry — five stars, and no more.
pub const MAX_RATING: u8 = 5;

/// The lowest: one star. Below this the user did not rate the asset at all,
/// which is [`None`] in every type that holds a rating.
pub const MIN_RATING: u8 = 1;

/// A star rating the user actually gave, `1..=MAX_RATING`.
///
/// Constructed through [`Rating::new`] (which answers `None` for anything a
/// star row cannot draw) or [`Rating::clamp`] (which is for values arriving
/// from outside this build — a model's answer, an XMP packet — where "roughly
/// this good" is the honest reading). Compares and serializes as the number it
/// wraps: the export format writes a bare integer, and `rating:4` in a search
/// box still parses against the same column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rating(u8);

impl Rating {
    /// The rating `value`, if a star row can show it. `None` for `0` (which is
    /// not a rating, it is the absence of one) and for anything above
    /// [`MAX_RATING`].
    pub fn new(value: u8) -> Option<Self> {
        if (MIN_RATING..=MAX_RATING).contains(&value) {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The nearest rating to `value`, so an outside value becomes a judgement
    /// this build can show instead of a rejected row. `0` has no answer here —
    /// it is not a low rating — so callers that mean "unrated" say so with
    /// [`None`] on the field that holds this one.
    pub fn clamp(value: u8) -> Self {
        Self(value.clamp(MIN_RATING, MAX_RATING))
    }

    /// The number a star row draws, and the number the column holds.
    pub fn get(self) -> u8 {
        self.0
    }

    /// `self` at least as good as `other` — the comparison a "show me 4 stars
    /// and up" filter makes, named so the reading sites say what they mean.
    pub fn at_least(self, other: Self) -> bool {
        self.0 >= other.0
    }

    /// The five ratings a picker offers, lowest first.
    pub fn all() -> [Self; MAX_RATING as usize] {
        [Self(1), Self(2), Self(3), Self(4), Self(5)]
    }
}

impl std::fmt::Display for Rating {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Rating> for u8 {
    fn from(rating: Rating) -> u8 {
        rating.0
    }
}

impl TryFrom<u8> for Rating {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Rating::new(value)
            .ok_or_else(|| format!("rating must be {MIN_RATING}..={MAX_RATING}, got {value}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_RATING, MIN_RATING, Rating};

    #[test]
    fn only_a_value_a_star_row_can_draw_is_a_rating() {
        assert!(Rating::new(0).is_none(), "zero is the absence of a rating");
        assert!(Rating::new(MIN_RATING).is_some());
        assert!(Rating::new(MAX_RATING).is_some());
        assert!(
            Rating::new(MAX_RATING + 1).is_none(),
            "six stars is not a rating"
        );
        assert_eq!(Rating::new(3).unwrap().get(), 3);
    }

    #[test]
    fn it_orders_and_round_trips_as_the_number_it_wraps() {
        let two = Rating::new(2).unwrap();
        let four = Rating::new(4).unwrap();
        assert!(four.at_least(two) && !two.at_least(four));
        assert_eq!(two.cmp(&four), std::cmp::Ordering::Less);

        // The export writes a bare integer, and `rating:` in a search box reads
        // one back: the type must not add a wrapper object to either.
        assert_eq!(serde_json::to_string(&four).unwrap(), "4");
        assert_eq!(
            serde_json::from_str::<Rating>("3").unwrap(),
            Rating::new(3).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&Some(four)).unwrap(),
            "4",
            "an unrated/rated pair still serializes as null / number"
        );
        assert_eq!(
            serde_json::to_string(&Option::<Rating>::None).unwrap(),
            "null"
        );
    }

    #[test]
    fn values_from_outside_the_build_become_the_nearest_rating() {
        assert_eq!(Rating::clamp(0), Rating::new(MIN_RATING).unwrap());
        assert_eq!(Rating::clamp(99), Rating::new(MAX_RATING).unwrap());
        assert_eq!(Rating::clamp(3), Rating::new(3).unwrap());
        assert_eq!(Rating::all().len(), MAX_RATING as usize);
        assert_eq!(
            Rating::all().last().copied().unwrap(),
            Rating::new(MAX_RATING).unwrap()
        );
    }
}
