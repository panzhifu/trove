//! The app asset source: gpui-kit's default component icon bundle, plus
//! the few extra Lucide icons the video player needs (volume, shrink)
//! that the default bundle's subset does not carry.

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, Result, SharedString};

// Volume-1/2/X and Shrink are not in gpui-kit's `default-icons.txt`, so
// they are missing from the component `IconName` and its bundle; this
// embeds exactly those four (Lucide, ISC license) and nothing else.
icon_assets!(
    pub(crate) VideoIcons,
    [Volume1, Volume2, VolumeX, Shrink]
);

/// Chain trove's extra icons in front of gpui-kit's default bundle: kit
/// icons keep rendering everywhere, the four video icons resolve from
/// here.
pub(crate) struct TroveAssets;

impl AssetSource for TroveAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(data) = VideoIcons.load(path)? {
            return Ok(Some(data));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names = VideoIcons.list(path)?;
        names.extend(Assets.list(path)?);
        Ok(names)
    }
}
