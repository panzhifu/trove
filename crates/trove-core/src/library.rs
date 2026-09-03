//! 库清单：对应磁盘上的 `library.json`。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::LibraryId;

/// 库 schema 当前版本；启动时据此做迁移。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 一个自包含的资产库（托管库，见 `docs/asset-repository.md` §2）。
///
/// 一个库 = 磁盘上一个文件夹，包含 `library.json`、`index.db`、`blobs/`、`thumbs/`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub id: LibraryId,
    /// 库名（显示用）。
    pub name: String,
    /// schema 版本，用于启动迁移。
    pub schema_version: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Library {
    pub fn new(name: String) -> Self {
        let now = Utc::now();
        Self {
            id: LibraryId::new(),
            name,
            schema_version: CURRENT_SCHEMA_VERSION,
            created_at: now,
            updated_at: now,
        }
    }
}
