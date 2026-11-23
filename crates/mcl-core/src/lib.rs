//! # mcl-core —— OMCLauncher 业务核心库
//!
//!
//! 设计约束：
//! - **零 UI 依赖**：不引用任何 Slint 类型，可被 GUI 与未来 CLI 复用
//! - **trait 边界**：`AuthProvider` / `DownloadSource` / `ModLoaderInstaller` / `PlatformApi`
//!   均以 trait 暴露，支持测试替身与运行期多实现
//! - **服务间通信**：领域事件经 [`task::EventBus`] 广播
//!
//! 当前状态：M0 骨架——领域类型、trait 边界、可测纯函数（版本 JSON 合并 / Range 切分 /
//! classpath 拼装 / 路径解析）已就绪；网络与安装流程在 M0 PoC 阶段填充。

pub mod account;
pub mod auth;
pub mod crash;
pub mod credential;
pub mod download;
pub mod error;
pub mod game;
pub mod http;
pub mod instance;
pub mod instance_settings;
pub mod java;
pub mod launch;
pub mod loader;
pub mod maintenance;
pub mod models;
pub mod pack;
pub mod platform;
pub mod resource;
pub mod settings;
pub mod task;
pub mod util;
pub mod version;

pub use error::{CoreError, CoreResult};
