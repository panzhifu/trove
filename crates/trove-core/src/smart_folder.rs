//! 智能文件夹：保存的搜索条件（v2 预留，数据结构先行）。

use serde::{Deserialize, Serialize};

use crate::id::SmartFolderId;

/// 保存的搜索条件，`query` 为结构化 JSON（条件树）。
///
/// 注意：`serde_json::Value` 不实现 `Eq`（含浮点），故只派生 `PartialEq`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmartFolder {
    pub id: SmartFolderId,
    pub name: String,
    /// 搜索条件（JSON），如 `{"and": [{"field": "rating", "op": ">=", "value": 4}]}`。
    pub query: serde_json::Value,
}

impl SmartFolder {
    pub fn new(name: String, query: serde_json::Value) -> Self {
        Self {
            id: SmartFolderId::new(),
            name,
            query,
        }
    }
}
