//! 游戏目录布局
//!
//! 实例根目录即 `.minecraft` 等价物：
//!
//! ```text
//! <root>/
//! ├── versions/<id>/<id>.json      合并后的版本 JSON
//! ├── versions/<id>/<id>.jar       客户端 jar
//! ├── versions/<id>/natives/       解压后的本地库（每版本独立，支持多实例并行）
//! ├── libraries/                   类库
//! ├── assets/indexes/              资源索引
//! └── assets/objects/             资源对象（内容寻址，天然跨版本复用）
//! ```

use std::path::{Path, PathBuf};

use crate::models::Instance;

#[derive(Debug, Clone)]
pub struct GameLayout {
    root: PathBuf,
    /// 共享存储根：`assets` 与 `libraries` 跨实例复用。
    /// `None` 表示完全隔离（历史实例保持自带目录）。
    shared: Option<PathBuf>,
}

impl GameLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            shared: None,
        }
    }

    /// 共享模式：实例私有内容（versions/mods/saves/…）在 `root`，
    /// 资源与类库在 `shared_root`（`<数据目录>/shared/`）
    pub fn with_shared(root: impl Into<PathBuf>, shared_root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            shared: Some(shared_root.into()),
        }
    }

    /// 按实例元数据选择布局（`Instance::shared` 决定是否用共享目录）
    pub fn for_instance(instance: &Instance) -> Self {
        if instance.shared {
            Self::with_shared(&instance.dir, crate::platform::paths::shared_dir())
        } else {
            Self::new(&instance.dir)
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 共享内容的**写入/读取根**：共享模式下为共享目录，隔离模式下等同实例根。
    /// `libraries/` 与 `assets/` 前缀的文件都以此目录为基准。
    pub fn shared_data_root(&self) -> &Path {
        self.shared.as_deref().unwrap_or(&self.root)
    }

    pub fn version_dir(&self, id: &str) -> PathBuf {
        self.root.join("versions").join(id)
    }

    pub fn version_jar(&self, id: &str) -> PathBuf {
        self.version_dir(id).join(format!("{id}.jar"))
    }

    pub fn version_json(&self, id: &str) -> PathBuf {
        self.version_dir(id).join(format!("{id}.json"))
    }

    /// natives 目录（每版本独立，避免多实例并行时互相覆盖）
    pub fn natives_dir(&self, id: &str) -> PathBuf {
        self.version_dir(id).join("natives")
    }

    pub fn libraries_dir(&self) -> PathBuf {
        match &self.shared {
            Some(shared) => shared.join("libraries"),
            None => self.root.join("libraries"),
        }
    }

    pub fn assets_dir(&self) -> PathBuf {
        match &self.shared {
            Some(shared) => shared.join("assets"),
            None => self.root.join("assets"),
        }
    }

    pub fn asset_index(&self, id: &str) -> PathBuf {
        self.assets_dir().join("indexes").join(format!("{id}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths_are_consistent() {
        let layout = GameLayout::new("/instances/fabric-1.20.1");
        assert_eq!(
            layout.version_jar("fabric-loader-0.15.11-1.20.1"),
            PathBuf::from("/instances/fabric-1.20.1/versions/fabric-loader-0.15.11-1.20.1/fabric-loader-0.15.11-1.20.1.jar")
        );
        assert_eq!(
            layout.natives_dir("1.20.1"),
            PathBuf::from("/instances/fabric-1.20.1/versions/1.20.1/natives")
        );
        assert_eq!(
            layout.asset_index("5"),
            PathBuf::from("/instances/fabric-1.20.1/assets/indexes/5.json")
        );
        assert!(layout.libraries_dir().starts_with(layout.root()));
    }

    #[test]
    fn shared_layout_splits_private_and_shared_content() {
        let layout = GameLayout::with_shared("/instances/a", "/data/shared");

        // 实例私有：版本 JSON / natives / 日志等
        assert_eq!(
            layout.version_dir("1.20.1"),
            PathBuf::from("/instances/a/versions/1.20.1")
        );
        assert!(layout.natives_dir("1.20.1").starts_with("/instances/a"));

        // 共享：类库与资源
        assert_eq!(
            layout.libraries_dir(),
            PathBuf::from("/data/shared/libraries")
        );
        assert_eq!(layout.assets_dir(), PathBuf::from("/data/shared/assets"));
        assert_eq!(
            layout.asset_index("5"),
            PathBuf::from("/data/shared/assets/indexes/5.json")
        );
    }

    #[test]
    fn for_instance_respects_shared_flag() {
        let dir = std::env::temp_dir().join("mcl-layout-instance");
        let mut instance = crate::models::Instance {
            id: "a".into(),
            name: "A".into(),
            mc_version: "1.20.1".into(),
            patches: Vec::new(),
            dir: dir.clone(),
            last_played: None,
            shared: true,
        };

        let shared_layout = GameLayout::for_instance(&instance);
        assert!(shared_layout
            .libraries_dir()
            .starts_with(crate::platform::paths::shared_dir()));

        instance.shared = false;
        let isolated = GameLayout::for_instance(&instance);
        assert_eq!(isolated.libraries_dir(), dir.join("libraries"));
    }
}
