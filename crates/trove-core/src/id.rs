//! 类型安全的新类型 ID 与内容哈希。
//!
//! 身份与内容分离是资产仓库的根基（见 `docs/asset-repository.md` §3）：
//! - `AssetId` / `FolderId` / `TagId` … 是**身份**（UUID），重命名、挪动、打标签都不变；
//! - [`Sha256`] 是**内容哈希**，随文件内容变化，用于去重与「原件被修改」检测。

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256 as Sha256Hasher};
use uuid::Uuid;

use crate::error::CoreError;

/// 生成一个包装 [`Uuid`] 的新类型 ID：类型安全，防止把素材 ID 当文件夹 ID 用。
macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            /// 生成一个随机 v4 UUID 作为新 ID。
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// 由已有 UUID 构造。
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }
    };
}

define_id!(
    /// 素材 ID。
    AssetId
);
define_id!(
    /// 文件夹 ID。
    FolderId
);
define_id!(
    /// 库 ID。
    LibraryId
);
define_id!(
    /// 标签分组 ID。
    TagGroupId
);
define_id!(
    /// 标签 ID。
    TagId
);
define_id!(
    /// 智能文件夹 ID（v2 预留）。
    SmartFolderId
);

/// SHA-256 内容哈希（32 字节）。
///
/// 序列化为 64 位小写十六进制字符串。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    /// 对字节计算 SHA-256。
    pub fn hash(bytes: &[u8]) -> Self {
        Self(Sha256Hasher::digest(bytes).into())
    }

    /// 由 32 字节原始值构造。
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// 由 64 位十六进制字符串解析。
    pub fn from_hex(s: &str) -> Result<Self, CoreError> {
        if s.len() != 64 {
            return Err(CoreError::InvalidSha256(format!(
                "expected 64 hex chars, got {}",
                s.len()
            )));
        }
        let bytes = s.as_bytes();
        let mut out = [0u8; 32];
        for (i, chunk) in bytes.chunks(2).enumerate() {
            let hi = hex_val(chunk[0]).ok_or_else(|| CoreError::InvalidSha256(s.to_string()))?;
            let lo = hex_val(chunk[1]).ok_or_else(|| CoreError::InvalidSha256(s.to_string()))?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 64 位小写十六进制字符串。
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    /// 磁盘分片目录名：十六进制前两位（如 `ab`），用于 `blobs/ab/…`。
    pub fn shard(&self) -> String {
        self.to_hex()[..2].to_string()
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl std::fmt::Display for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sha256({})", self.to_hex())
    }
}

impl Serialize for Sha256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Sha256::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_roundtrip() {
        let h = Sha256::hash(b"trove");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Sha256::from_hex(&hex).unwrap(), h);
        assert_eq!(h.shard(), hex[..2]);
    }

    #[test]
    fn id_types_are_distinct() {
        // 编译期保证：AssetId 与 FolderId 不能互相赋值。
        let a = AssetId::new();
        let f = FolderId::new();
        assert_ne!(a.to_string(), f.to_string());
    }
}
