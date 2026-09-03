//! 内容寻址的物理文件（blob）。
//!
//! blob（内容）与 asset（元数据）分表，支持去重：同一张图导入两次，
//! 物理文件只存一份，两条 asset 行共享同一个 blob。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::id::Sha256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub sha256: Sha256,
    /// 扩展名（不含点），如 `jpg`。
    pub ext: String,
    /// 相对库根的路径：`blobs/{前两位十六进制}/{sha256}.{ext}`。
    pub rel_path: PathBuf,
    pub size_bytes: u64,
    /// 引用计数：引用此 blob 的素材数量；降到 0 可回收。
    pub ref_count: u32,
}

impl Blob {
    /// 由内容哈希 + 扩展名 + 大小构造；`rel_path` 按磁盘约定推导。
    ///
    /// 新建 blob 时至少被一个 asset 引用，故 `ref_count` 初始为 1。
    pub fn new(sha256: Sha256, ext: impl Into<String>, size_bytes: u64) -> Self {
        let ext = ext.into();
        let rel_path = Self::rel_path(&sha256, &ext);
        Self {
            sha256,
            ext,
            rel_path,
            size_bytes,
            ref_count: 1,
        }
    }

    /// blob 的磁盘路径约定：`blobs/{前两位十六进制}/{sha256}.{ext}`。
    pub fn rel_path(sha256: &Sha256, ext: &str) -> PathBuf {
        PathBuf::from("blobs")
            .join(sha256.shard())
            .join(format!("{}.{}", sha256.to_hex(), ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_uses_shard_and_hex() {
        let b = Blob::new(Sha256::hash(b"trove"), "png", 42);
        let hex = b.sha256.to_hex();
        assert_eq!(
            b.rel_path,
            PathBuf::from("blobs").join(&hex[..2]).join(format!("{}.png", hex))
        );
        assert_eq!(b.ref_count, 1);
    }
}
