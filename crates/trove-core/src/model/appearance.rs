//! How a container — a collection or a smart collection, both of which the
//! folder tree draws — asks to be looked at: a glyph of its own and an accent
//! colour.
//!
//! The two container types share this one type because the tree draws them the
//! same way and a user expects the same affordance on either. It is stored as
//! JSON in one nullable column beside each row, which is how the smart
//! collection's rule tree is already stored: a container that customises
//! nothing holds `NULL` rather than an empty object, so the common case costs
//! nothing and a clear is a write of `NULL`, not a delete.
//!
//! The accent is a *name*, never a colour value, and that is the load-bearing
//! decision: the same hex that reads as a warm accent on a light background is
//! muddy on a dark one, so a stored hex would have to be right in both themes
//! at the moment it was picked. Storing the name lets each theme decide, which
//! is also what makes a user's custom theme able to restate the palette.
//! Likewise the icon is a *name* from the application's catalogue, so a value
//! this build cannot resolve is a drawing detail, not a broken row.

use serde::{Deserialize, Serialize};

/// A container's own glyph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Glyph {
    /// An Emoji, kept as the literal the user picked (possibly a joined
    /// sequence). The text engine paints it in the emoji font's own colours —
    /// an accent cannot tint it, which is why [`crate::model::Appearance`]
    /// stays meaningful on its own.
    Emoji(String),
    /// A vector icon, named in kebab case. Painted in the accent colour.
    Icon(String),
}

/// The named accents a container may take. Eight, so the picker is a row of
/// swatches rather than a colour wheel, and so the set reads as a palette
/// rather than as an arbitrary hex the user happened to stop on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    Red,
    Orange,
    Yellow,
    Green,
    Cyan,
    Blue,
    Purple,
    Pink,
}

/// Every accent, in picker order.
const ACCENTS: [Accent; 8] = [
    Accent::Red,
    Accent::Orange,
    Accent::Yellow,
    Accent::Green,
    Accent::Cyan,
    Accent::Blue,
    Accent::Purple,
    Accent::Pink,
];

impl Accent {
    /// The accents, in the order a picker should show them.
    pub const ALL: [Accent; 8] = ACCENTS;

    /// The stored name. Lowercase and short, because it lands in a JSON column
    /// a user may read with their own eyes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Cyan => "cyan",
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Pink => "pink",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        ACCENTS.iter().copied().find(|a| a.as_str() == name)
    }

    /// The colour this name stands for, as a starting point: what a theme that
    /// says nothing about the palette draws, and what a legacy free-form hex is
    /// matched against. The application's own per-theme values are allowed to
    /// differ from it — the name is the contract, this is the reference.
    pub fn reference(self) -> &'static str {
        match self {
            Self::Red => "#ef4444",
            Self::Orange => "#f97316",
            Self::Yellow => "#eab308",
            Self::Green => "#22c55e",
            Self::Cyan => "#06b6d4",
            Self::Blue => "#3b82f6",
            Self::Purple => "#a855f7",
            Self::Pink => "#ec4899",
        }
    }

    /// The accent nearest a colour, which is how a stored free-form hex (what
    /// smart collections used to keep) is walked onto the palette. Squared RGB
    /// distance: crude by design, since it only has to pick one of eight and
    /// the user is one click away from correcting it.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let (r, g, b) = rgb(hex)?;
        ACCENTS.iter().copied().min_by_key(|accent| {
            let (ar, ag, ab) = rgb(accent.reference()).unwrap_or((0, 0, 0));
            let d = |x: u32, y: u32| x.abs_diff(y) * x.abs_diff(y);
            d(r, ar) + d(g, ag) + d(b, ab)
        })
    }
}

/// What a container asks for: a glyph, an accent, both, or neither.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appearance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glyph: Option<Glyph>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<Accent>,
}

impl Appearance {
    /// Nothing customised, which is what every container starts as and what
    /// "back to the default folder look" sets.
    pub fn is_plain(&self) -> bool {
        self.glyph.is_none() && self.accent.is_none()
    }

    /// The stored text, or `None` for [`Self::is_plain`] so a cleared row goes
    /// back to `NULL` rather than keeping an empty object around.
    pub fn to_storage(&self) -> Option<String> {
        if self.is_plain() {
            return None;
        }
        serde_json::to_string(self).ok()
    }

    /// Read a stored value. An unreadable one is *dropped*, not failed: a
    /// container's look is the least important thing in its row, and refusing
    /// to list a library over a bad glyph would be a bad trade. The same
    /// happens to a name this build cannot resolve, at draw time.
    pub fn from_storage(text: Option<&str>) -> Self {
        text.and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or_default()
    }

    /// Keep the parts worth storing: a glyph that is empty, oversized, or
    /// carrying path separators is dropped, so what lands in the library is
    /// what the picker offered. Returns `None` when nothing survives, which
    /// lets a caller store the plain `NULL` rather than an object of holes.
    pub fn sanitized(mut self) -> Option<Self> {
        self.glyph = self.glyph.filter(|glyph| match glyph {
            Glyph::Emoji(text) => is_emoji(text),
            Glyph::Icon(name) => is_icon_name(name),
        });
        (!self.is_plain()).then_some(self)
    }
}

/// An Emoji: a glyph from a non-ASCII script (which an Emoji always is —
/// ASCII stays out so the field cannot be used to stash a path), at most a
/// few joined code points, and no control characters.
fn is_emoji(text: &str) -> bool {
    !text.is_empty()
        && text.chars().next().is_some_and(|c| !c.is_ascii())
        && text.chars().count() <= 8
        && !text
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}

/// An icon name: the kebab-case shape this project's catalogue uses, and
/// nothing else. Length-capped so a hand-edited library cannot store a blob.
fn is_icon_name(name: &str) -> bool {
    let valid = !name.is_empty()
        && name.len() <= 40
        && !name.contains("--")
        && !name.starts_with('-')
        && !name.ends_with('-');
    valid
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The channels of a `#rrggbb` / `#rgb` colour, leading `#` optional.
fn rgb(hex: &str) -> Option<(u32, u32, u32)> {
    let digits = hex.trim().trim_start_matches('#');
    let expanded: String;
    let hex = match digits.len() {
        6 => digits,
        3 => {
            expanded = digits.chars().flat_map(|c| [c, c]).collect();
            &expanded
        }
        _ => return None,
    };
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u32::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok();
    Some((byte(0)?, byte(1)?, byte(2)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_appearance_is_stored_as_nothing() {
        assert_eq!(Appearance::default().to_storage(), None);
        assert!(Appearance::default().is_plain());
        let one = Appearance {
            glyph: None,
            accent: Some(Accent::Cyan),
        };
        assert!(one.to_storage().is_some());
        assert_eq!(Appearance::from_storage(one.to_storage().as_deref()), one);
        // Clearing is a write of nothing, not a write of an empty object.
        assert_eq!(Appearance::default().sanitized(), None);
    }

    /// A library that predates a name this build knows has to keep opening.
    #[test]
    fn an_unreadable_stored_value_degrades_to_the_default() {
        assert_eq!(
            Appearance::from_storage(Some("not json")),
            Appearance::default()
        );
        assert_eq!(Appearance::from_storage(None), Appearance::default());
        // A known shape with an unknown accent name is dropped by serde.
        assert_eq!(
            Appearance::from_storage(Some(r#"{"accent":"chartreuse"}"#)),
            Appearance::default()
        );
    }

    #[test]
    fn only_the_shapes_the_picker_offers_are_kept() {
        let emoji = Appearance {
            glyph: Some(Glyph::Emoji("📷".into())),
            accent: None,
        };
        assert!(emoji.clone().sanitized().is_some());
        let too_long = "x".repeat(9);
        for rejected in ["", "camera", "a\u{0}b", "path/like", &too_long] {
            let probe = Appearance {
                glyph: Some(Glyph::Emoji(rejected.to_string())),
                accent: None,
            };
            assert_eq!(probe.sanitized(), None, "kept {rejected:?}");
        }
        let too_long = "c".repeat(41);
        for rejected in [
            "", "Camera", "cam--era", "-cam", "cam-", "cam er", &too_long,
        ] {
            let probe = Appearance {
                glyph: Some(Glyph::Icon(rejected.to_string())),
                accent: None,
            };
            assert_eq!(probe.sanitized(), None, "kept icon {rejected:?}");
        }
        assert!(
            Appearance {
                glyph: Some(Glyph::Icon("image-plus".into())),
                accent: Some(Accent::Purple),
            }
            .sanitized()
            .is_some()
        );
    }

    /// The stored spelling is the contract with what is already on disk.
    #[test]
    fn stored_names_are_lowercase_and_round_trip() {
        for accent in Accent::ALL {
            assert_eq!(Accent::parse(accent.as_str()), Some(accent));
            assert_eq!(
                Appearance {
                    glyph: None,
                    accent: Some(accent),
                }
                .to_storage()
                .unwrap(),
                format!(r#"{{"accent":"{}"}}"#, accent.as_str())
            );
        }
        assert_eq!(Accent::parse("Red"), None);
        assert_eq!(Accent::parse(""), None);
    }

    /// Every legacy hex lands on the accent a user would have picked: the
    /// reference colours map to themselves, and the neighbours go with them.
    #[test]
    fn a_legacy_hex_maps_to_the_nearest_accent() {
        for accent in Accent::ALL {
            assert_eq!(
                Accent::from_hex(accent.reference()),
                Some(accent),
                "{} did not map to itself",
                accent.as_str()
            );
        }
        assert_eq!(Accent::from_hex("#ff0000"), Some(Accent::Red));
        assert_eq!(Accent::from_hex("#0000ff"), Some(Accent::Blue));
        assert_eq!(Accent::from_hex("green"), None);
        assert_eq!(Accent::from_hex("#12345"), None);
    }
}
