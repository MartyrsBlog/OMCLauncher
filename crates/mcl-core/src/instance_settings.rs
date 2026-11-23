//! 每实例游戏设置（内存 / 分辨率 / JVM 参数 / Java 路径）。
//!
//! 独立存于 `<实例目录>/settings.json`：不塞进全局 `instances.json`，
//! 复制/移动实例目录时设置自然跟随，也避免旧版本文件迁移。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CoreResult;
use crate::settings::InstanceDefaults;

pub const DEFAULT_MEMORY_MB: u32 = 4096;
pub const DEFAULT_WIDTH: u32 = 854;
pub const DEFAULT_HEIGHT: u32 = 480;
const MIN_MEMORY_MB: u32 = 512;
const MAX_MEMORY_MB: u32 = 32_768;
const MIN_SIDE: u32 = 320;
const MAX_SIDE: u32 = 7_680;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceSettings {
    pub memory_mb: u32,
    pub width: u32,
    pub height: u32,
    pub fullscreen: bool,
    /// 追加 JVM 参数（空格分隔），如 `-XX:+UseG1GC`
    pub jvm_args: String,
    /// 追加游戏参数
    pub game_args: String,
    /// 指定 Java 可执行文件；空 = 自动选择
    pub java_path: String,
    /// 内置图标标识（`IconKind::id`）；空 = 跟随加载器
    pub icon: String,
}

impl Default for InstanceSettings {
    fn default() -> Self {
        Self {
            memory_mb: DEFAULT_MEMORY_MB,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fullscreen: false,
            jvm_args: String::new(),
            game_args: String::new(),
            java_path: String::new(),
            icon: String::new(),
        }
    }
}

impl InstanceSettings {
    pub fn path(instance_dir: &Path) -> std::path::PathBuf {
        instance_dir.join("settings.json")
    }

    /// 读取并收敛到合法区间；缺失/损坏 → 默认值（不阻塞启动）
    pub fn load(instance_dir: &Path) -> Self {
        let mut settings: Self = std::fs::read(Self::path(instance_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        settings.normalize();
        settings
    }

    /// 读取实例设置；**没有落盘时使用全局默认值**（全局默认 + 实例覆盖）。
    ///
    /// 只判断文件是否存在，不判断内容是否损坏——损坏文件回退内置默认，
    /// 避免用户的全局默认悄悄覆盖一个“读到一半”的配置。
    pub fn load_with_defaults(instance_dir: &Path, defaults: &InstanceDefaults) -> Self {
        if Self::path(instance_dir).is_file() {
            Self::load(instance_dir)
        } else {
            Self::from_defaults(defaults)
        }
    }

    pub fn from_defaults(defaults: &InstanceDefaults) -> Self {
        let mut settings = Self {
            memory_mb: defaults.memory_mb,
            width: defaults.width,
            height: defaults.height,
            fullscreen: defaults.fullscreen,
            jvm_args: defaults.jvm_args.clone(),
            game_args: defaults.game_args.clone(),
            java_path: String::new(),
            icon: String::new(),
        };
        settings.normalize();
        settings
    }

    /// 原子写入（临时文件 + 改名）
    pub fn save(&self, instance_dir: &Path) -> CoreResult<()> {
        let mut normalized = self.clone();
        normalized.normalize();
        let path = Self::path(instance_dir);
        let bytes = serde_json::to_vec_pretty(&normalized)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn normalize(&mut self) {
        self.memory_mb = self.memory_mb.clamp(MIN_MEMORY_MB, MAX_MEMORY_MB);
        self.width = self.width.clamp(MIN_SIDE, MAX_SIDE);
        self.height = self.height.clamp(MIN_SIDE, MAX_SIDE);
        self.jvm_args = self.jvm_args.trim().to_string();
        self.game_args = self.game_args.trim().to_string();
        self.java_path = self.java_path.trim().to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mcl-instsettings-{tag}-{stamp}"));
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    #[test]
    fn defaults_are_sane() {
        let settings = InstanceSettings::default();
        assert_eq!(settings.memory_mb, 4096);
        assert_eq!((settings.width, settings.height), (854, 480));
        assert!(!settings.fullscreen);
    }

    #[test]
    fn roundtrip_persists() {
        let dir = temp_dir("roundtrip");
        let settings = InstanceSettings {
            memory_mb: 8192,
            width: 1920,
            height: 1080,
            fullscreen: true,
            jvm_args: "-XX:+UseG1GC".into(),
            game_args: "--demo".into(),
            java_path: "/usr/bin/java".into(),
            icon: "chest".into(),
        };
        settings.save(&dir).expect("保存");
        assert_eq!(InstanceSettings::load(&dir), settings);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let dir = temp_dir("clamp");
        std::fs::write(
            InstanceSettings::path(&dir),
            br#"{"memory_mb": 1, "width": 99999, "height": 1}"#,
        )
        .expect("写文件");

        let settings = InstanceSettings::load(&dir);
        assert_eq!(settings.memory_mb, 512);
        assert_eq!(settings.width, 7680);
        assert_eq!(settings.height, 320);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(InstanceSettings::load(&dir), InstanceSettings::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn global_defaults_apply_until_instance_writes_its_own() {
        let dir = temp_dir("defaults");
        let defaults = InstanceDefaults {
            memory_mb: 8192,
            width: 1280,
            height: 720,
            fullscreen: true,
            jvm_args: "-XX:+UseG1GC".into(),
            game_args: "--demo".into(),
        };

        let effective = InstanceSettings::load_with_defaults(&dir, &defaults);
        assert_eq!(effective.memory_mb, 8192);
        assert_eq!((effective.width, effective.height), (1280, 720));
        assert!(effective.fullscreen);

        // 实例自己的设置优先于全局默认
        InstanceSettings {
            memory_mb: 2048,
            ..effective
        }
        .save(&dir)
        .expect("保存");
        assert_eq!(
            InstanceSettings::load_with_defaults(&dir, &defaults).memory_mb,
            2048
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
