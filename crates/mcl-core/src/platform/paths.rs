//! 跨平台路径解析
//!
//! 解析逻辑抽为**纯函数** [`resolve_data_dir`]，三平台分支均可单元测试，
//! 无需依赖运行平台。

use std::path::PathBuf;

/// 应用数据目录名
pub const APP_DIR_NAME: &str = "OMCLauncher";

/// 纯函数：按平台规则计算数据目录
///
/// - Windows：`%APPDATA%\OMCLauncher`
/// - macOS：`~/Library/Application Support/OMCLauncher`
/// - Linux/其他：`$XDG_DATA_HOME/mclauncher`，缺省 `~/.local/share/mclauncher`
pub fn resolve_data_dir(
    os: &str,
    home: &str,
    appdata: Option<&str>,
    xdg_data_home: Option<&str>,
) -> PathBuf {
    match os {
        "windows" => PathBuf::from(appdata.unwrap_or(home)).join(APP_DIR_NAME),
        "macos" => PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join(APP_DIR_NAME),
        _ => {
            let base = match xdg_data_home {
                Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
                _ => PathBuf::from(home).join(".local").join("share"),
            };
            base.join("mclauncher")
        }
    }
}

/// 便携模式标记文件（与可执行文件同目录时启用）
pub const PORTABLE_MARKER: &str = "mclauncher.portable";
/// 便携模式的数据目录名（可执行文件同级）
pub const PORTABLE_DIR_NAME: &str = "mclauncher-data";
/// 系统安装前缀：这些位置的可执行文件一律用 XDG，不写同级目录
const SYSTEM_PREFIXES: &[&str] = &[
    "/usr/bin",
    "/usr/local/bin",
    "/usr/sbin",
    "/bin",
    "/sbin",
    "/opt",
    "/snap",
    "/var/lib/flatpak",
];

/// 该目录是否为系统安装位置（deb/rpm/flatpak 等）
pub fn is_system_prefix(dir: &std::path::Path) -> bool {
    let text = dir.to_string_lossy();
    SYSTEM_PREFIXES
        .iter()
        .any(|prefix| text == *prefix || text.starts_with(&format!("{prefix}/")))
}

/// 便携判定（纯逻辑，便于测试）
///
/// 只认**显式意图**：标记文件或 `MCL_PORTABLE=1`。
/// 不做「目录可写就当便携」的猜测——否则从旧版本升级时数据目录会悄悄搬家，
/// 用户看到的是「实例全没了」。
pub fn portable_data_dir(exe_dir: &std::path::Path, explicit: bool) -> Option<PathBuf> {
    if !explicit {
        return None;
    }
    Some(exe_dir.join(PORTABLE_DIR_NAME))
}

/// 便携数据目录（不可用时返回 `None`）
fn resolve_portable(exe_dir: &std::path::Path) -> Option<PathBuf> {
    let marker = exe_dir.join(PORTABLE_MARKER).is_file();
    let env_flag = std::env::var("MCL_PORTABLE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    portable_data_dir(exe_dir, marker || env_flag)
}

/// 当前平台的数据目录
///
/// 优先级：`MCL_DATA_DIR` 环境变量 → 便携目录（标记文件 / 非系统前缀且可写）→ 平台默认。
pub fn data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("MCL_DATA_DIR") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if let Some(dir) = resolve_portable(parent) {
                return dir;
            }
        }
    }

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();

    resolve_data_dir(
        std::env::consts::OS,
        &home,
        std::env::var("APPDATA").ok().as_deref(),
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
    )
}

/// 实例根目录
pub fn instances_dir() -> PathBuf {
    data_dir().join("instances")
}

/// 共享资源目录（跨实例复用，引用计数管理见 `crate::instance`）
pub fn shared_dir() -> PathBuf {
    data_dir().join("shared")
}

/// 自动下载的 Java 运行时目录
pub fn java_dir() -> PathBuf {
    data_dir().join("java")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_requires_explicit_intent() {
        let exe = std::path::Path::new("/home/steve/mclauncher");
        // 显式要求 → 便携
        assert_eq!(
            portable_data_dir(exe, true),
            Some(exe.join(PORTABLE_DIR_NAME))
        );
        // 未显式要求 → 一律回落（不做「可写即便携」的猜测）
        assert_eq!(portable_data_dir(exe, false), None);
        // 系统前缀判定
        assert!(is_system_prefix(std::path::Path::new("/usr/bin")));
        assert!(is_system_prefix(std::path::Path::new("/opt/mclauncher")));
        assert!(!is_system_prefix(std::path::Path::new("/home/steve/bin")));
    }

    #[test]
    fn windows_uses_appdata() {
        assert_eq!(
            resolve_data_dir(
                "windows",
                "C:\\Users\\steve",
                Some("C:\\Users\\steve\\AppData\\Roaming"),
                None
            ),
            PathBuf::from("C:\\Users\\steve\\AppData\\Roaming").join("OMCLauncher")
        );
        // 缺 APPDATA 时回退 HOME
        assert_eq!(
            resolve_data_dir("windows", "C:\\Users\\steve", None, None),
            PathBuf::from("C:\\Users\\steve").join("OMCLauncher")
        );
    }

    #[test]
    fn macos_uses_application_support() {
        assert_eq!(
            resolve_data_dir("macos", "/Users/steve", None, None),
            PathBuf::from("/Users/steve/Library/Application Support/OMCLauncher")
        );
    }

    #[test]
    fn linux_prefers_xdg_then_falls_back() {
        assert_eq!(
            resolve_data_dir("linux", "/home/steve", None, Some("/home/steve/.data")),
            PathBuf::from("/home/steve/.data/mclauncher")
        );
        assert_eq!(
            resolve_data_dir("linux", "/home/steve", None, None),
            PathBuf::from("/home/steve/.local/share/mclauncher")
        );
        // 空 XDG 值视为未设置
        assert_eq!(
            resolve_data_dir("linux", "/home/steve", None, Some("")),
            PathBuf::from("/home/steve/.local/share/mclauncher")
        );
    }

    #[test]
    fn derived_dirs_are_under_data_dir() {
        let data = data_dir();
        assert!(instances_dir().starts_with(&data));
        assert!(shared_dir().starts_with(&data));
    }
}
