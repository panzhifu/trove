//! Media import: content-addressed blob storage, type probing, thumbnails and
//! the file import pipeline.

pub mod anim;
pub mod audio_prep;
pub mod blob;
pub mod chunked;
pub mod color;
pub mod color_profile;
pub mod convert;
pub mod edit;
pub mod export;
pub mod font_language;
pub mod formats;
pub mod gpu;
pub mod hash;
pub mod hash_cache;
pub mod hdr;
pub mod height_color;
pub mod import;
pub mod index;
pub mod metadata;
pub mod pipeline;
pub mod precheck;
pub mod probe;
pub mod proc;
pub mod render3d;
pub mod search;
pub mod sequence;
pub mod spectrum;
pub mod subtitles;
pub mod text;
pub mod thumb;
pub mod video;
pub mod waveform;

/// The midpoint of the paper every generated card is baked on — the model
/// renderer's own background ramp, [`render3d::BG_TOP`] → [`render3d::BG_BOTTOM`]
/// — as `0xRRGGBB` for the UI surfaces that hold one: a baked thumbnail cannot
/// follow the theme, so the frame it sits in takes this instead of a theme
/// surface reading as a grey band around the art.
///
/// A card that fills its frame never shows it. What remains is the fallback —
/// an asset whose card has not been baked, which shows a kind icon on this
/// colour — and the single colour a letterboxed card is judged against, which
/// is why it is the ramp's midpoint rather than either end.
pub const CARD_PAPER_RGB: u32 = 0xEA_EC_EF;

/// The ink a generated card draws with: the same dark grey for a waveform bar,
/// a specimen glyph and a text card's lines, so the cards differ only in what
/// they show, not in what they are drawn with. `0xRRGGBB` for the UI surfaces
/// that draw one of these cards live (the font specimen in the grid).
pub const CARD_INK_RGB: u32 = 0x20_21_24;

/// The ink's muted companion on the same paper — the grey a live waveform strip
/// draws with, and what a card's own label uses. Fixed rather than themed for
/// the same reason as [`CARD_INK_RGB`]: it sits on paper that does not follow
/// the theme.
pub const CARD_MUTED_RGB: u32 = 0x8C_8C_96;

/// [`CARD_INK_RGB`] as RGB bytes, for the rasterizers that blend a card's ink.
pub const CARD_INK: [u8; 3] = [
    (CARD_INK_RGB >> 16) as u8,
    (CARD_INK_RGB >> 8) as u8,
    CARD_INK_RGB as u8,
];

/// [`CARD_MUTED_RGB`] as RGB bytes, for the same reason.
pub const CARD_MUTED: [u8; 3] = [
    (CARD_MUTED_RGB >> 16) as u8,
    (CARD_MUTED_RGB >> 8) as u8,
    CARD_MUTED_RGB as u8,
];

pub use formats::streaming_point_cloud::StreamingPointCloud;
