//! 插件边界上的归一化图像类型。
//!
//! 这些类型同时是 WIT（`wit/format.wit`）里 record 的 Rust 镜像，也是宿主侧
//! [`crate::FormatPlugin`] trait 的返回值。插件（WASM）只做「字节 → RGBA / 元数据」。

use serde::{Deserialize, Serialize};

/// 解码后的 RGBA8 图像。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedImage {
    /// RGBA8 像素，长度应为 `width * height * 4`。
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl DecodedImage {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            data,
            width,
            height,
        }
    }

    /// 校验 `data` 长度是否与尺寸一致。
    pub fn validate(&self) -> Result<(), String> {
        if self.expected_len() == Some(self.data.len()) {
            Ok(())
        } else {
            Err(format!(
                "decoded image buffer has {} bytes, expected {} for {}x{}",
                self.data.len(),
                self.expected_len().unwrap_or(0),
                self.width,
                self.height,
            ))
        }
    }

    /// 期望的字节数 = `width * height * 4`（防溢出）。
    pub fn expected_len(&self) -> Option<usize> {
        let px = (self.width as usize).checked_mul(self.height as usize)?;
        px.checked_mul(4)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// 图像元数据（建索引用，不参与渲染）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub color_space: Option<String>,
    /// 预留：主色（v2 颜色筛选）。
    pub dominant_colors: Vec<Rgba>,
}

/// RGBA 颜色。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_image_validate() {
        let ok = DecodedImage::new(1, 1, vec![0; 4]);
        assert!(ok.validate().is_ok());

        let bad = DecodedImage::new(1, 1, vec![0; 3]);
        assert!(bad.validate().is_err());
    }
}
