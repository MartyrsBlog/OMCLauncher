//! 启动器设置
//!
//! 持久化用户偏好，当前含**下载并发数**（设置页可调）。与账号存储同款策略：
//! JSON 落盘、先写临时文件再原子改名；读取失败或文件损坏时回退默认值，
//! 绝不因设置问题阻塞启动。
//!
//! 后续设置项（全局限速 / 代理 / 主题 / 减少动效）在同一文件按字段扩展，
//! 旧版本文件缺字段时依赖 `#[serde(default)]` 平滑升级。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CoreResult;

/// 下载并发数默认值
///
/// 16→27.6；32→29.3/41.3；64→41.8/62.5/77.3；128→43.3/45.5；256→37.3/63.6。
/// 波动大（延迟主导），配对比较下 64 为吞吐拐点（≥128 无增益，仅增加客户端
/// 资源与公益镜像负载）。
pub const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 64;
/// 并发数下限
pub const MIN_DOWNLOAD_CONCURRENCY: usize = 1;
pub const MAX_DOWNLOAD_CONCURRENCY: usize = 256;

/// 新建实例的默认游戏设置（实例未落 `settings.json` 时使用；已落盘的实例以自身为准）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceDefaults {
    pub memory_mb: u32,
    pub width: u32,
    pub height: u32,
    pub fullscreen: bool,
    pub jvm_args: String,
    pub game_args: String,
}

impl Default for InstanceDefaults {
    fn default() -> Self {
        Self {
            memory_mb: 4096,
            width: 854,
            height: 480,
            fullscreen: false,
            jvm_args: String::new(),
            game_args: String::new(),
        }
    }
}

impl InstanceDefaults {
    /// 内存收敛到 1024~32768 MB（与实例设置页的步进器一致）
    pub fn set_memory_mb(&mut self, value: u32) -> u32 {
        self.memory_mb = value.clamp(1024, 32768);
        self.memory_mb
    }
}

/// 用户可调设置的持久化快照
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherSettings {
    /// 下载并发数（1~256，默认 64）
    pub download_concurrency: usize,
    /// 网络代理 URL（空 = 直连）；支持 http/https/socks5，例如 `http://127.0.0.1:7890`
    pub proxy_url: String,
    /// 新建实例的默认游戏设置（对应 HMCL 的全局默认 + 实例覆盖模型）
    pub instance_defaults: InstanceDefaults,
    /// 下载源偏好：`auto`（官方优先）/ `official` / `mirror`（镜像优先）
    pub download_source: String,
    /// 当前选中的实例（首页 hero 与下载中心的目标实例）
    pub selected_instance: String,
    /// 游戏目录列表（实例根目录，绝对路径；去重保序，至少一项）。
    /// 多目录参考 HMCL `GameDirectoryManager`：左列按目录过滤，新建实例落入默认目录。
    pub game_directories: Vec<String>,
    /// 默认游戏目录（ ∈ `game_directories`；空 = 归一化时回落首项）
    pub default_game_directory: String,
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            download_concurrency: DEFAULT_DOWNLOAD_CONCURRENCY,
            proxy_url: String::new(),
            instance_defaults: InstanceDefaults::default(),
            download_source: "auto".to_string(),
            selected_instance: String::new(),
            game_directories: Vec::new(),
            default_game_directory: String::new(),
        }
    }
}

impl LauncherSettings {
    /// 默认存放位置：`<数据目录>/settings.json`
    pub fn default_path() -> PathBuf {
        crate::platform::paths::data_dir().join("settings.json")
    }

    /// 读取并规范化（缺失/损坏 → 默认值；越界值收敛到合法区间）
    pub fn load_from(path: impl AsRef<Path>) -> Self {
        let mut settings: Self = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        settings.download_concurrency = clamp_download_concurrency(settings.download_concurrency);
        settings.proxy_url = settings.proxy_url.trim().to_string();
        settings.normalize_download_source();
        settings
            .instance_defaults
            .set_memory_mb(settings.instance_defaults.memory_mb);
        settings.normalize_game_directories();
        settings
    }

    /// 归一化游戏目录：去空白项、去重（保序）；列表为空时回落主实例根；
    /// 默认目录必须在列表内，否则回落首项。
    pub fn normalize_game_directories(&mut self) {
        let primary = crate::platform::paths::instances_dir()
            .to_string_lossy()
            .to_string();
        let mut list: Vec<String> = Vec::new();
        for dir in &self.game_directories {
            let dir = dir.trim();
            if dir.is_empty() || list.iter().any(|existing| existing == dir) {
                continue;
            }
            list.push(dir.to_string());
        }
        if list.is_empty() {
            list.push(primary.clone());
        }
        let default = self.default_game_directory.trim().to_string();
        self.default_game_directory = if default.is_empty() || !list.contains(&default) {
            list[0].clone()
        } else {
            default
        };
        self.game_directories = list;
    }

    /// 新建实例落入的根目录（= 默认游戏目录）
    pub fn default_instances_root(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.default_game_directory)
    }

    pub fn load() -> Self {
        Self::load_from(Self::default_path())
    }

    /// 是否镜像优先（镜像源排在官方源之前）
    pub fn prefer_mirror(&self) -> bool {
        self.download_source == "mirror"
    }

    /// 规范化下载源取值（未知值回落 `auto`）
    pub fn normalize_download_source(&mut self) {
        self.download_source = match self.download_source.as_str() {
            "official" | "mirror" => self.download_source.clone(),
            _ => "auto".to_string(),
        };
    }

    /// 设置并发数（自动收敛到合法区间），返回收敛后的值
    pub fn set_download_concurrency(&mut self, value: usize) -> usize {
        self.download_concurrency = clamp_download_concurrency(value);
        self.download_concurrency
    }

    /// 构造代理（空/非法 URL 返回 `None`，调用方按直连处理）。
    pub fn proxy(&self) -> Option<reqwest::Proxy> {
        let url = self.proxy_url.trim();
        if url.is_empty() {
            return None;
        }
        reqwest::Proxy::all(url).ok()
    }

    /// 原子写入（临时文件 + 改名，防写入中断损坏文件）
    pub fn save_to(&self, path: impl AsRef<Path>) -> CoreResult<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn save(&self) -> CoreResult<()> {
        self.save_to(Self::default_path())
    }
}

/// 收敛并发数到 [`MIN_DOWNLOAD_CONCURRENCY`]..=[`MAX_DOWNLOAD_CONCURRENCY`]
pub fn clamp_download_concurrency(value: usize) -> usize {
    value.clamp(MIN_DOWNLOAD_CONCURRENCY, MAX_DOWNLOAD_CONCURRENCY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mcl-settings-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.json")
    }

    #[test]
    fn default_concurrency_is_64() {
        assert_eq!(LauncherSettings::default().download_concurrency, 64);
    }

    #[test]
    fn roundtrip_persists_across_load() {
        let path = temp_path("roundtrip");
        let mut settings = LauncherSettings::default();
        settings.set_download_concurrency(96);
        settings.save_to(&path).expect("保存");

        let loaded = LauncherSettings::load_from(&path);
        assert_eq!(loaded.download_concurrency, 96);
    }

    #[test]
    fn missing_or_corrupt_file_falls_back_to_default() {
        let missing = temp_path("missing");
        assert_eq!(
            LauncherSettings::load_from(&missing).download_concurrency,
            DEFAULT_DOWNLOAD_CONCURRENCY
        );

        let corrupt = temp_path("corrupt");
        std::fs::create_dir_all(corrupt.parent().expect("父目录")).expect("建目录");
        std::fs::write(&corrupt, b"{ not json").expect("写坏文件");
        assert_eq!(
            LauncherSettings::load_from(&corrupt).download_concurrency,
            DEFAULT_DOWNLOAD_CONCURRENCY
        );
    }

    #[test]
    fn out_of_range_values_are_clamped_on_set_and_load() {
        let mut settings = LauncherSettings::default();
        assert_eq!(
            settings.set_download_concurrency(0),
            MIN_DOWNLOAD_CONCURRENCY
        );
        assert_eq!(
            settings.set_download_concurrency(9999),
            MAX_DOWNLOAD_CONCURRENCY
        );

        // 文件中的越界值在读取时同样收敛（含旧版本/手改文件的防御）
        let path = temp_path("clamp-load");
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(&path, br#"{ "download_concurrency": 100000 }"#).expect("写文件");
        assert_eq!(
            LauncherSettings::load_from(&path).download_concurrency,
            MAX_DOWNLOAD_CONCURRENCY
        );
    }

    #[test]
    fn proxy_roundtrip_and_validation() {
        let path = temp_path("proxy");
        let mut settings = LauncherSettings::default();
        assert!(settings.proxy().is_none(), "空代理应视为直连");

        settings.proxy_url = "  http://127.0.0.1:7890  ".to_string();
        settings.save_to(&path).expect("保存");
        let loaded = LauncherSettings::load_from(&path);
        assert_eq!(loaded.proxy_url, "http://127.0.0.1:7890");
        assert!(loaded.proxy().is_some(), "合法 URL 应能构造代理");

        settings.proxy_url = "not a url".to_string();
        assert!(settings.proxy().is_none(), "非法 URL 应回退直连");
    }

    #[test]
    fn game_directories_default_to_primary_root() {
        let mut settings = LauncherSettings::load_from(std::path::Path::new("/nonexistent"));
        settings.normalize_game_directories();
        let primary = crate::platform::paths::instances_dir()
            .to_string_lossy()
            .to_string();
        assert_eq!(settings.game_directories, vec![primary.clone()]);
        assert_eq!(settings.default_game_directory, primary);
        assert_eq!(
            settings.default_instances_root(),
            std::path::PathBuf::from(&primary)
        );
    }

    #[test]
    fn game_directories_normalize_trims_dedupes_and_fixes_default() {
        let primary = crate::platform::paths::instances_dir()
            .to_string_lossy()
            .to_string();
        let mut settings = LauncherSettings {
            game_directories: vec![
                "  ".to_string(),
                format!("{primary} "),
                "/tmp/mc-a".to_string(),
                "/tmp/mc-a".to_string(),
                "/tmp/mc-b".to_string(),
            ],
            default_game_directory: "/tmp/missing".to_string(),
            ..LauncherSettings::default()
        };
        settings.normalize_game_directories();
        assert_eq!(
            settings.game_directories,
            vec![
                primary.clone(),
                "/tmp/mc-a".to_string(),
                "/tmp/mc-b".to_string()
            ]
        );
        assert_eq!(
            settings.default_game_directory, primary,
            "默认不在列表内 → 回落首项"
        );
    }

    #[test]
    fn download_source_defaults_and_normalizes() {
        let mut settings = LauncherSettings::default();
        assert_eq!(settings.download_source, "auto");
        assert!(!settings.prefer_mirror());

        settings.download_source = "mirror".into();
        assert!(settings.prefer_mirror());

        // 未知取值回落 auto
        settings.download_source = "fastest".into();
        settings.normalize_download_source();
        assert_eq!(settings.download_source, "auto");
        assert!(!settings.prefer_mirror());
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // 字段扩展前的旧文件（未来新增字段）不应导致解析失败
        let path = temp_path("forward-compat");
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(
            &path,
            br#"{ "download_concurrency": 32, "future_option": true }"#,
        )
        .expect("写文件");
        assert_eq!(LauncherSettings::load_from(&path).download_concurrency, 32);
    }
}
