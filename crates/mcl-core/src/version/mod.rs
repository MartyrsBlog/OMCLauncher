//! 版本与实例安装
//!
//! 模块职责：
//! - 远端版本清单拉取与本地缓存
//! - 版本 JSON 继承链合并（[`manifest::merge_inherited`]，风险 T3）
//! - 安装编排：文件清单生成 → 交给 `DownloadEngine`

pub mod manifest;
pub mod service;
