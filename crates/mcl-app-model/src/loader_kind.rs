//! 加载器种类：UI 传字符串 id，核心用 [`LoaderSpec`] 表达具体版本。
//!
//! 集中一处做映射，避免 `"fabric"` / `"neoforge"` 这类字面量散落在 UI 与任务层。

use mcl_core::models::LoaderSpec;

/// 可安装的加载器种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl LoaderKind {
    /// 全部加载器（与安装向导格子顺序一致）
    pub const ALL: [LoaderKind; 4] = [
        LoaderKind::Fabric,
        LoaderKind::Quilt,
        LoaderKind::Forge,
        LoaderKind::NeoForge,
    ];

    /// UI / 任务里使用的稳定标识
    pub fn id(self) -> &'static str {
        match self {
            LoaderKind::Fabric => "fabric",
            LoaderKind::Quilt => "quilt",
            LoaderKind::Forge => "forge",
            LoaderKind::NeoForge => "neoforge",
        }
    }

    /// 展示名
    pub fn label(self) -> &'static str {
        match self {
            LoaderKind::Fabric => "Fabric",
            LoaderKind::Quilt => "Quilt",
            LoaderKind::Forge => "Forge",
            LoaderKind::NeoForge => "NeoForge",
        }
    }

    /// 解析 UI 标识（未知返回 `None`）
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    /// 具体版本的加载器规格
    pub fn spec(self, version: &str) -> LoaderSpec {
        match self {
            LoaderKind::Fabric => LoaderSpec::Fabric(version.to_string()),
            LoaderKind::Quilt => LoaderSpec::Quilt(version.to_string()),
            LoaderKind::Forge => LoaderSpec::Forge(version.to_string()),
            LoaderKind::NeoForge => LoaderSpec::NeoForge(version.to_string()),
        }
    }

    /// 是否走「官方 installer」流程（Forge / NeoForge）
    pub fn uses_installer(self) -> bool {
        matches!(self, LoaderKind::Forge | LoaderKind::NeoForge)
    }

    pub(crate) fn flavor(self) -> Option<mcl_core::loader::forge::Flavor> {
        match self {
            LoaderKind::Forge => Some(mcl_core::loader::forge::Flavor::Forge),
            LoaderKind::NeoForge => Some(mcl_core::loader::forge::Flavor::NeoForge),
            _ => None,
        }
    }

    pub(crate) fn meta(self) -> Option<mcl_core::version::service::MetaLoader> {
        match self {
            LoaderKind::Fabric => Some(mcl_core::version::service::MetaLoader::Fabric),
            LoaderKind::Quilt => Some(mcl_core::version::service::MetaLoader::Quilt),
            _ => None,
        }
    }
}

/// 加载器版本行（UI 直接展示）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderVersion {
    pub version: String,
    /// 角标：最新 / 稳定 / 测试（无角标为空）
    pub tag: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for kind in LoaderKind::ALL {
            assert_eq!(LoaderKind::from_id(kind.id()), Some(kind));
        }
        assert_eq!(LoaderKind::from_id("vanilla"), None);
    }

    #[test]
    fn spec_matches_kind() {
        assert_eq!(
            LoaderKind::Quilt.spec("0.26.0"),
            LoaderSpec::Quilt("0.26.0".into())
        );
        assert!(LoaderKind::Forge.uses_installer());
        assert!(!LoaderKind::Fabric.uses_installer());
        assert!(LoaderKind::NeoForge.flavor().is_some());
        assert!(LoaderKind::Fabric.meta().is_some());
    }
}
