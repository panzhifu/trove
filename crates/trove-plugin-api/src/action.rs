//! 动作插件：宿主侧 trait（静态分发）。

use serde::{Deserialize, Serialize};

/// 动作执行结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub success: bool,
    pub message: String,
    /// 输出字节（压缩/转换后的文件）；失败或无输出时为 `None`。
    pub output: Option<Vec<u8>>,
}

impl ActionResult {
    pub fn ok(output: Vec<u8>) -> Self {
        Self {
            success: true,
            message: String::new(),
            output: Some(output),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            output: None,
        }
    }
}

/// 动作插件：对字节做变换（压缩、转格式、缩放、导出）。
pub trait ActionPlugin: Send + Sync {
    /// 执行动作。`params` 为 JSON 字符串（v1 简化；WIT 动态参数较繁琐）。
    fn run(&self, bytes: &[u8], params: &str) -> Result<ActionResult, String>;
}
