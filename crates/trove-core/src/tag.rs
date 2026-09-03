//! 标签与标签分组。

use serde::{Deserialize, Serialize};

use crate::id::{TagGroupId, TagId};

/// 标签分组（如「项目」「风格」）。每个库内置一个「默认分组」。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagGroup {
    pub id: TagGroupId,
    pub name: String,
    pub color: Option<String>,
}

impl TagGroup {
    pub fn new(name: String) -> Self {
        Self {
            id: TagGroupId::new(),
            name,
            color: None,
        }
    }
}

/// 标签。`group_id` 必填——所有标签都属于某个分组，保证 `UNIQUE(group_id, name)` 约束正确。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: TagId,
    pub group_id: TagGroupId,
    pub name: String,
    pub color: Option<String>,
}

impl Tag {
    pub fn new(group_id: TagGroupId, name: String) -> Self {
        Self {
            id: TagId::new(),
            group_id,
            name,
            color: None,
        }
    }
}
