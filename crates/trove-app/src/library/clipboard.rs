//! Copying an asset's pixels onto the system clipboard.
//!
//! On X11 and Wayland a copy is *owned by the copying process*: nothing is
//! transferred until another application asks for the data. Dropping the
//! `arboard` handle right after `set_image` would therefore lose the clipboard
//! contents before anyone could paste them, so one handle is kept alive for
//! the whole session (see [`CLIPBOARD`]).

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use image::GenericImageView as _;

/// The session-wide clipboard handle. See the module docs for why it cannot
/// just be created and dropped inside [`copy_image`].
static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();

/// Longest edge handed to the clipboard. Beyond this the transfer gets slow
/// and some receivers choke on the pixel count.
const MAX_EDGE: u32 = 4096;

/// Why a copy did not happen.
#[derive(Debug)]
pub enum CopyImageError {
    /// The file could not be decoded (unsupported or corrupt).
    Undecodable,
    /// Decoded fine, but the system clipboard refused the data.
    Clipboard(String),
}

impl std::fmt::Display for CopyImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undecodable => f.write_str("the file could not be decoded as an image"),
            Self::Clipboard(e) => write!(f, "clipboard unavailable: {e}"),
        }
    }
}

/// Decode `path` and place it on the system clipboard as an image.
///
/// Returns the size actually copied, which is smaller than the source when
/// the image exceeds [`MAX_EDGE`].
pub fn copy_image(path: &Path) -> Result<(u32, u32), CopyImageError> {
    let decoded =
        trove_core::media::thumb::decode_image(path).ok_or(CopyImageError::Undecodable)?;
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err(CopyImageError::Undecodable);
    }
    // Cap the payload: pasting a 100 MP photo is slow and often pointless.
    let scale = (MAX_EDGE as f32 / width.max(height) as f32).min(1.0);
    let decoded = if scale < 1.0 {
        decoded.thumbnail(
            (width as f32 * scale).max(1.0) as u32,
            (height as f32 * scale).max(1.0) as u32,
        )
    } else {
        decoded
    };

    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    let data = arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: Cow::Owned(rgba.into_raw()),
    };

    let slot = CLIPBOARD.get_or_init(|| Mutex::new(None));
    let mut guard = slot
        .lock()
        .map_err(|_| CopyImageError::Clipboard("clipboard lock poisoned".to_string()))?;
    let clipboard = match guard.as_mut() {
        Some(clipboard) => clipboard,
        None => guard.insert(
            arboard::Clipboard::new().map_err(|e| CopyImageError::Clipboard(e.to_string()))?,
        ),
    };
    clipboard
        .set_image(data)
        .map_err(|e| CopyImageError::Clipboard(e.to_string()))?;
    Ok((width, height))
}
