//! 跨模块共享的领域类型

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 账号类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// 微软正版账号（MSA → XBL → XSTS → Minecraft 认证链）
    Microsoft,
    /// 离线模式
    Offline,
    /// authlib-injector 外置登录
    External,
}

impl AccountKind {
    /// UI 展示用标签
    pub fn label(self) -> &'static str {
        match self {
            AccountKind::Microsoft => "微软",
            AccountKind::Offline => "离线",
            AccountKind::External => "外置",
        }
    }
}

/// 账号信息；令牌不在本结构内，由 keyring 单独管理
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub uuid: String,
    pub kind: AccountKind,
    /// 令牌过期时间（Unix 秒）；离线账号为 `None`
    pub expires_at: Option<i64>,
}

/// 判定令牌是否需要刷新：提前 5 分钟刷新窗口
pub fn refresh_needed(expires_at: Option<i64>, now: i64) -> bool {
    const REFRESH_WINDOW_SECS: i64 = 300;
    match expires_at {
        Some(exp) => exp - now <= REFRESH_WINDOW_SECS,
        None => false,
    }
}

/// 模组加载器规格
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "version", rename_all = "snake_case")]
pub enum LoaderSpec {
    Vanilla,
    Fabric(String),
    Quilt(String),
    Forge(String),
    NeoForge(String),
}

impl LoaderSpec {
    /// 展示标签，如 `Fabric 0.15.11`
    pub fn label(&self) -> String {
        match self {
            LoaderSpec::Vanilla => "原版".to_string(),
            LoaderSpec::Fabric(v) => format!("Fabric {v}"),
            LoaderSpec::Quilt(v) => format!("Quilt {v}"),
            LoaderSpec::Forge(v) => format!("Forge {v}"),
            LoaderSpec::NeoForge(v) => format!("NeoForge {v}"),
        }
    }

    /// 不含版本号的加载器名（如 `Fabric`）。
    ///
    /// 局部左列只有 200px，宽度放不下 `Fabric 0.15.11`；实例头在那里用
    /// `1.20.1 · Fabric` 这种短摘要，完整标签仍走 [`Self::label`]。
    pub fn name(&self) -> &'static str {
        match self {
            LoaderSpec::Vanilla => "原版",
            LoaderSpec::Fabric(_) => "Fabric",
            LoaderSpec::Quilt(_) => "Quilt",
            LoaderSpec::Forge(_) => "Forge",
            LoaderSpec::NeoForge(_) => "NeoForge",
        }
    }

    /// 是否需要在版本 JSON 之上注入加载器配置文件（原版为否）
    pub fn needs_installer(&self) -> bool {
        !matches!(self, LoaderSpec::Vanilla)
    }
}

/// 实例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub mc_version: String,
    /// 叠加层（借鉴 HMCL `GameInstancePatch`）：按顺序合并到原版之上。
    ///
    /// 现在只会有 0~1 个加载器层；结构上支持叠加（如 Forge + OptiFine），
    /// 切换加载器 = 替换第 0 层，而不是换实例目录。
    #[serde(default)]
    pub patches: Vec<LoaderSpec>,
    pub dir: PathBuf,
    /// 最后游玩时间（Unix 秒）
    pub last_played: Option<i64>,
    /// 是否使用共享 `assets`/`libraries`。
    ///
    /// - 新建实例为 `true`：资源与类库落 `<数据目录>/shared/`，多实例互不重复下载；
    /// - 旧版本文件缺失该字段 → `false`（历史实例保持自带目录，零迁移风险）。
    #[serde(default)]
    pub shared: bool,
}

impl Instance {
    /// 实例在游戏目录中的版本标识（版本 JSON / natives 所在目录名）。
    ///
    /// **按实例固定**（= `id`）：切换加载器时复用同一个 `versions/<id>/`，
    /// 不再产生 `versions/fabric-loader-…` / `versions/1.20.1-forge-…` 两套目录。
    /// 原版客户端 jar 始终落在 `versions/<mc_version>/`，各层共用。
    pub fn version_id(&self) -> String {
        self.id.clone()
    }

    /// 主加载器（第 0 层；无补丁 = 原版）。
    ///
    /// 只关心「装了什么加载器」的调用方用它；需要叠加语义时读 `patches`。
    pub fn loader(&self) -> &LoaderSpec {
        self.patches.first().unwrap_or(&LoaderSpec::Vanilla)
    }

    /// 替换加载器层（保留后续层，为叠加预留）
    pub fn set_loader(&mut self, spec: LoaderSpec) {
        if self.patches.is_empty() {
            self.patches.push(spec);
        } else {
            self.patches[0] = spec;
        }
    }

    /// 合并签名：加载器层变了就必须重装（缓存失效判据）
    pub fn patch_signature(&self) -> String {
        self.patches
            .iter()
            .map(|patch| patch.label())
            .collect::<Vec<_>>()
            .join("+")
    }
}

/// 官方版本类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionKind {
    Release,
    Snapshot,
    OldBeta,
    OldAlpha,
}

impl VersionKind {
    /// UI 标签
    pub fn label(self) -> &'static str {
        match self {
            VersionKind::Release => "release",
            VersionKind::Snapshot => "snapshot",
            VersionKind::OldBeta => "old_beta",
            VersionKind::OldAlpha => "old_alpha",
        }
    }
}

/// 版本清单条目（version_manifest_v2.json）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionSummary {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: VersionKind,
    #[serde(rename = "releaseTime")]
    pub release_time: String,
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
}

/// 下载任务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Running,
    Paused,
    Done,
    Failed,
}

/// 需下载的单个文件（下载引擎最小调度单元）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub url: String,
    /// 官方清单提供的 SHA-1
    pub sha1: Option<String>,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_window_boundaries() {
        let now = 1_000_000;
        // 无过期时间（离线账号）永不刷新
        assert!(!refresh_needed(None, now));
        // 已过期
        assert!(refresh_needed(Some(now - 10), now));
        // 剩余 4 分钟（小于阈值）→ 需要刷新
        assert!(refresh_needed(Some(now + 240), now));
        // 剩余 10 分钟 → 不需要
        assert!(!refresh_needed(Some(now + 600), now));
    }

    #[test]
    fn loader_label_and_installer_flag() {
        assert_eq!(LoaderSpec::Vanilla.label(), "原版");
        assert!(!LoaderSpec::Vanilla.needs_installer());
        assert_eq!(
            LoaderSpec::Fabric("0.15.11".into()).label(),
            "Fabric 0.15.11"
        );
        assert!(LoaderSpec::NeoForge("21.1.0".into()).needs_installer());
    }

    #[test]
    fn loader_name_has_no_version_suffix() {
        assert_eq!(LoaderSpec::Vanilla.name(), "原版");
        assert_eq!(LoaderSpec::Fabric("0.15.11".into()).name(), "Fabric");
        assert_eq!(LoaderSpec::Quilt("0.24.0".into()).name(), "Quilt");
        assert_eq!(LoaderSpec::Forge("47.2.0".into()).name(), "Forge");
        assert_eq!(LoaderSpec::NeoForge("21.1.0".into()).name(), "NeoForge");
    }

    #[test]
    fn version_kind_labels_match_ui_doc() {
        assert_eq!(VersionKind::Release.label(), "release");
        assert_eq!(VersionKind::OldAlpha.label(), "old_alpha");
    }
}
