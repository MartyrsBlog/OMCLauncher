//! 统一错误类型

use thiserror::Error;

/// 核心库错误。UI 层只做展示，不做分类判断——分类与诊断建议由
/// [`crate::launch`] 的崩溃分析器承担。
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("网络请求失败：{0}")]
    Network(String),

    #[error("文件校验失败（SHA-1 不匹配）：{path}")]
    HashMismatch { path: String },

    #[error("版本清单解析失败：{0}")]
    Manifest(String),

    #[error("账号认证失败：{0}")]
    Auth(String),

    #[error("Java 运行时不可用：{0}")]
    JavaUnavailable(String),

    #[error("不支持的操作：{0}")]
    Unsupported(String),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 解析失败：{0}")]
    Json(#[from] serde_json::Error),
}

pub type CoreResult<T> = Result<T, CoreError>;
