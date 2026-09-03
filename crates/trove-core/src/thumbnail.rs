//! 缩略图档位。缩略图是缓存，永远可重算。

/// 缩略图档位：缩略图是缓存，永远可重算。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThumbnailSize {
    /// 网格用。
    Small,
    /// 检查面板用。
    Large,
}

impl ThumbnailSize {
    /// 缩略图边长（像素）。
    pub fn px(self) -> u32 {
        match self {
            ThumbnailSize::Small => 256,
            ThumbnailSize::Large => 512,
        }
    }

    pub const ALL: [ThumbnailSize; 2] = [ThumbnailSize::Small, ThumbnailSize::Large];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_sizes() {
        assert_eq!(ThumbnailSize::Small.px(), 256);
        assert_eq!(ThumbnailSize::Large.px(), 512);
    }
}
