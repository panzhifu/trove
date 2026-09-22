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
// Pencil: the preview toolbar's pixel-edit buttons. Trash and Eraser:
// the title bar's empty-the-trash and clear-the-history actions. Sparkles:
// the AI tagging action, where `Bot` (the settings page) would read as
// "chat" rather than "do something to these assets". None of them are in
// `default-icons.txt`, so without listing them here they render blank.
// The appearance catalogue: every glyph a folder can be given, plus the four
// the collection tree falls back to. They are listed here because the default
// bundle carries only a tenth of the catalog — an icon outside it loads as empty
// and paints invisible, which is the failure `every_appearance_icon_is_bundled`
// keeps from coming back.
icon_assets!(
    pub(crate) ExtraIcons,
    [
        // The video player, the preview toolbar and the title bar.
        Volume1,
        Volume2,
        VolumeX,
        Shrink,
        RotateCcw,
        FlipHorizontal2,
        FlipVertical2,
        Pencil,
        Trash,
        Eraser,
        Sparkles,
        // What a folder may be given.
        Archive,
        Award,
        Bookmark,
        Box,
        Briefcase,
        Brush,
        Cake,
        Camera,
        Car,
        Circle,
        Clock,
        Cloud,
        Diamond,
        FileArchive,
        FileBox,
        Film,
        Flame,
        Flower,
        FolderHeart,
        FolderKey,
        FolderLock,
        Folders,
        GalleryThumbnails,
        Gift,
        House,
        Image,
        Images,
        Layers,
        LayoutGrid,
        Leaf,
        Library,
        MapPin,
        Mic,
        Mountain,
        Music,
        Package,
        Plane,
        Shapes,
        Ship,
        ShoppingBag,
        Tag,
        Tags,
        TreePine,
        Type,
        Video,
        X,
        Zap,
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
            "icons/trash.svg",
            "icons/eraser.svg",
            "icons/sparkles.svg",
        ] {
            assert!(
                ExtraIcons.load(path).unwrap().is_some(),
                "{path} is not embedded; add it to ExtraIcons"
            );
        }
    }

    /// The folder-look catalogue is generated from a list of ~60 names, so it is
    /// checked against the bundle rather than by eye: an icon the picker offers
    /// but the asset source does not carry paints a blank cell, and the user
    /// sees a picker full of nothing.
    #[test]
    fn every_appearance_icon_is_bundled() {
        for name in crate::panels::appearance::catalog_icons() {
            let path = name.path();
            assert!(
                TroveAssets.load(&path).unwrap().is_some(),
                "{path} is offered by the appearance catalogue but not embedded; \
                 add it to `ExtraIcons`"
            );
        }
    }
}
