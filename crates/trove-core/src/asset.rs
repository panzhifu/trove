//! 素材：用户视角的一条资产（元数据），指向一个 blob。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::id::{AssetId, FolderId, Sha256};

/// 素材评分，0..=5（0 表示未评分）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Rating(u8);

impl Rating {
    pub const MIN: u8 = 0;
    pub const MAX: u8 = 5;
    pub const UNRATED: Rating = Rating(0);

    /// 校验并构造；越界返回 [`CoreError::InvalidRating`]。
    pub fn new(value: u8) -> Result<Self, CoreError> {
        if value > Self::MAX {
            return Err(CoreError::InvalidRating(value));
        }
        Ok(Self(value))
    }

    pub fn get(&self) -> u8 {
        self.0
    }

    pub fn is_rated(&self) -> bool {
        self.0 > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// 身份（UUID），稳定不变。
    pub id: AssetId,
    /// 内容哈希，指向 [`crate::Blob`]。
    pub sha256: Sha256,
    /// 显示名称（不含扩展名）。
    pub name: String,
    /// 扩展名（不含点），如 `png`。
    pub ext: String,
    /// 所在文件夹；`None` 表示未分类。
    pub folder_id: Option<FolderId>,
    pub size_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub mime: Option<String>,
    pub rating: Rating,
    /// 注释 / 备注（自由文本）。
    pub annotation: String,
    /// 来源 URL（如从网页拖入）。
    pub source_url: Option<String>,
    /// 是否在回收站（软删除）。
    pub is_trashed: bool,
    pub imported_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

impl Asset {
    /// 带扩展名的完整文件名，如 `design.png`。
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.name, self.ext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_bounds() {
        assert_eq!(Rating::new(0).unwrap().get(), 0);
        assert_eq!(Rating::new(5).unwrap().get(), 5);
        assert!(Rating::new(6).is_err());
        assert!(!Rating::UNRATED.is_rated());
    }
}
