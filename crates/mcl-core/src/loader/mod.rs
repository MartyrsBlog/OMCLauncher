//! 模组加载器安装
//!
//! 每个加载器一个子模块，产出**与原版合并后的完整版本 JSON**，随后复用通用安装
//! 流水线（[`crate::version::service::VersionService::install_merged`]）。
//! 后续新增加载器（Quilt/Forge/NeoForge）只需实现同样的三个步骤：
//! 版本枚举 → 档案 JSON 获取 → 合并/补全后交给流水线。

pub mod fabric;
pub mod forge;
pub mod quilt;
