//! Trove 插件契约：WIT 接口定义 + 宿主侧 trait + 归一化类型。
//!
//! 本 crate 是插件边界上的**单一事实源**：
//! - `wit/` 下的 WIT 是 ABI 契约（插件侧由 `cargo-component` 从它生成绑定）；
//! - [`FormatPlugin`] / [`ActionPlugin`] 是宿主侧 trait（静态分发，见 `docs/plugin-system.md`）；
//! - [`image`] 里的归一化类型是 trait 返回值，也是 WIT record 的 Rust 镜像。
//!
//! **依赖铁律**：本 crate 不依赖 core / host / gpui / wasmtime，保持叶子地位，
//! 这样插件无论编译进主程序还是编译为 `.wasm`，都只看得到这一层。

mod action;
mod format;
mod image;

pub use action::{ActionPlugin, ActionResult};
pub use format::FormatPlugin;
pub use image::{DecodedImage, ImageMeta, Rgba};
