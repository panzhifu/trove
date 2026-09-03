//! core 的统一错误类型。

/// 资产仓库核心错误。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid sha256: {0}")]
    InvalidSha256(String),

    #[error("invalid rating: {0} (must be 0..=5)")]
    InvalidRating(u8),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("database error: {0}")]
    Db(#[from] libsql::Error),

    #[error("uuid parse error: {0}")]
    Uuid(#[from] uuid::Error),
}
