//! 格式插件：宿主侧 trait（静态分发）。

use crate::image::{DecodedImage, ImageMeta};

/// 格式插件：识别并解码一种文件格式。
///
/// 宿主侧实现（`trove-plugin-host` 的 `WasmFormatPlugin`）把调用委托给对应的
/// `.wasm` 组件；本 trait 方法被单态化，全程零 `dyn`（见 `docs/plugin-system.md` §1）。
///
/// 要求 `Send + Sync`：插件实例以 `Arc` 共享，在后台执行器上解码。
/// WASM 实现需把 wasmtime 的 `Store` 包进 `Mutex` 以达成 `Sync`。
pub trait FormatPlugin: Send + Sync {
    /// 支持的文件扩展名（不含点，小写），如 `["jxl", "avif"]`。
    fn extensions(&self) -> &[String];

    /// 解码为 RGBA8。
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, String>;

    /// 提取元数据（宽高、色彩空间、主色）。
    fn extract_meta(&self, bytes: &[u8]) -> Result<ImageMeta, String>;
}
