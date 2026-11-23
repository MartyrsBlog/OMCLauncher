//! 平台抽象层

pub mod browser;
pub mod clipboard;
pub mod paths;

use std::path::PathBuf;

/// 平台能力抽象。Windows / macOS / Linux 各有一个实现，
/// 运行期通过依赖注入提供给服务层。
pub trait PlatformApi: Send + Sync {
    /// 应用数据目录
    fn data_dir(&self) -> PathBuf;

    /// 探测本机可用的 Java 运行时
    ///
    /// 策略链：
    /// - Windows：注册表 + `JAVA_HOME` + PATH + 常见安装路径
    /// - macOS：`/usr/libexec/java_home -V` + `JAVA_HOME` + PATH
    /// - Linux：`update-alternatives` + `JAVA_HOME` + PATH
    fn find_javas(&self) -> Vec<PathBuf>;

    /// 系统通知
    fn notify(&self, title: &str, body: &str);
}

/// 本机实现（M0 骨架：路径已可用，Java 探测与通知在 M1 补全）
pub struct NativePlatform;

impl PlatformApi for NativePlatform {
    fn data_dir(&self) -> PathBuf {
        paths::data_dir()
    }

    fn find_javas(&self) -> Vec<PathBuf> {
        // TODO(M1): 按上述策略链实现探测；当前返回空表示「未发现 Java」，
        // 上层据此触发 的自动下载流程。
        Vec::new()
    }

    fn notify(&self, title: &str, body: &str) {
        // TODO(M1): 经 notify-rust 接入
        tracing::info!(title, body, "系统通知（M0 仅记录日志）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_platform_returns_current_data_dir() {
        let platform = NativePlatform;
        assert_eq!(platform.data_dir(), paths::data_dir());
    }

    #[test]
    fn javas_probe_is_empty_in_skeleton() {
        let platform = NativePlatform;
        assert!(platform.find_javas().is_empty());
    }
}
