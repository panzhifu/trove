//! The app asset source: gpui-kit's default component icon bundle, plus the
//! extra Lucide icons the app needs that the default bundle's subset does
//! not carry.
//!
//! The `assets::IconName` catalog has a variant for every bundled SVG, but
//! the default `Assets` source embeds only the list in gpui-kit's
//! `default-icons.txt` — a variant outside that list loads as empty and
//! paints an *invisible* icon. Anything the UI needs beyond the subset is
//! named in `ExtraIcons` below.

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, Result, SharedString};

// Volume-1/2/X and Shrink: the video player. RotateCcw and the two flips and
// Pencil: the preview toolbar's pixel-edit buttons. Axis3d and LocateFixed:
// the model viewport's scene-axes and trihedron switches. Trash and Eraser:
// the title bar's empty-the-trash and clear-the-history actions. None of them
// are in `default-icons.txt`, so without listing them here they render blank.
icon_assets!(
    pub(crate) ExtraIcons,
    [
        Volume1,
        Volume2,
        VolumeX,
        Shrink,
        RotateCcw,
        FlipHorizontal2,
        FlipVertical2,
        Pencil,
        Axis3d,
        LocateFixed,
        Trash,
        Eraser,
    ]
);

/// Chain trove's extra icons in front of gpui-kit's default bundle: kit
/// icons keep rendering everywhere, the extras resolve from here.
pub(crate) struct TroveAssets;

impl AssetSource for TroveAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(data) = ExtraIcons.load(path)? {
            return Ok(Some(data));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names = ExtraIcons.list(path)?;
        names.extend(Assets.list(path)?);
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon the UI names beyond gpui-kit's default bundle must actually
    /// resolve, or its button paints invisible — the failure that hid the
    /// preview toolbar's edit buttons once already.
    #[test]
    fn every_extra_icon_is_bundled() {
        for path in [
            "icons/volume-1.svg",
            "icons/volume-2.svg",
            "icons/volume-x.svg",
            "icons/shrink.svg",
            "icons/rotate-ccw.svg",
            "icons/flip-horizontal-2.svg",
            "icons/flip-vertical-2.svg",
            "icons/pencil.svg",
            "icons/axis-3d.svg",
            "icons/locate-fixed.svg",
            "icons/trash.svg",
            "icons/eraser.svg",
        ] {
            assert!(
                ExtraIcons.load(path).unwrap().is_some(),
                "{path} is not embedded; add it to ExtraIcons"
            );
        }
    }
}
