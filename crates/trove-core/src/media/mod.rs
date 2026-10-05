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

/// The paper every generated card is baked on — waveform, text/subtitle,
/// font specimen — as `0xRRGGBB` for the UI surfaces that show one: a baked
/// thumbnail cannot follow the theme, so the frame it sits in matches this
/// instead of a theme surface reading as a grey band around the art.
///
/// The value is the midpoint of the background the 3D model card is rendered
/// on (`render3d::BG_TOP` → `render3d::BG_BOTTOM`), so a waveform or a
/// subtitle card reads as the same surface as a model card rather than a warm
/// paper sitting next to a cool grey.
pub const CARD_PAPER_RGB: u32 = 0xEA_EC_EF;

/// The same paper as RGB bytes, for the rasterizers that fill a bitmap.
pub const CARD_PAPER: [u8; 3] = [
    (CARD_PAPER_RGB >> 16) as u8,
    (CARD_PAPER_RGB >> 8) as u8,
    CARD_PAPER_RGB as u8,
];

pub use formats::streaming_point_cloud::StreamingPointCloud;
