//! Trove 核心数据模型与业务逻辑。
//!
//! 本 crate 是资产仓库的心脏，遵循两条铁律：
//! 1. 不依赖 GPUI —— 产出的都是普通数据，可 headless 测试；
//! 2. 不依赖插件 runtime —— 插件系统通过注入 [`trove_plugin_api::FormatPlugin`] 来扩展。
//!
//! 归一化图像类型（[`DecodedImage`] / [`ImageMeta`] / [`Rgba`]）定义在叶子 crate
//! `trove-plugin-api`，core 仅复用，不重新定义。详细设计见 `docs/asset-repository.md`。

mod asset;
mod blob;
mod error;
mod folder;
mod id;
mod library;
pub mod store;
mod smart_folder;
mod tag;
mod thumbnail;

pub use asset::{Asset, Rating};
pub use blob::Blob;
pub use error::CoreError;
pub use folder::Folder;
pub use id::{AssetId, FolderId, LibraryId, Sha256, SmartFolderId, TagGroupId, TagId};
pub use library::{Library, CURRENT_SCHEMA_VERSION};
pub use smart_folder::SmartFolder;
pub use tag::{Tag, TagGroup};
pub use thumbnail::ThumbnailSize;

// 归一化图像类型来自 plugin-api（插件边界的共享契约），core 直接复用。
pub use trove_plugin_api::{DecodedImage, ImageMeta, Rgba};
