//! 文件夹：组织素材的树形结构（邻接表）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::FolderId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    /// 父文件夹；`None` 表示根文件夹。
    pub parent_id: Option<FolderId>,
    /// 同级排序（越小越靠前）。
    pub sort_order: i64,
    /// 显示颜色（如 `#ff0000`）。
    pub color: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Folder {
    pub fn new(name: String, parent_id: Option<FolderId>) -> Self {
        let now = Utc::now();
        Self {
            id: FolderId::new(),
            name,
            parent_id,
            sort_order: 0,
            color: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}
