//! 实例管理
//!
//! 生命周期：创建 / 克隆 / 快照 / 导入导出 / 删除。
//! **共享目录引用计数**：删除实例仅清理独占文件，
//! 共享的 `assets`/`libraries` 待引用归零后由 [`InstanceStore::gc_shared`] 回收。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::models::{Instance, LoaderSpec};

/// 实例存储抽象：生产实现落 SQLite + 文件系统，测试用内存实现
pub trait InstanceStore: Send + Sync {
    fn list(&self) -> Vec<Instance>;
    fn get(&self, id: &str) -> Option<Instance>;
    fn create(&mut self, instance: Instance) -> CoreResult<()>;
    fn remove(&mut self, id: &str) -> CoreResult<()>;
    /// 修改实例显示名（去首尾空白，非空且 ≤40 字符；不改变目录与 ID）
    fn rename(&mut self, id: &str, name: &str) -> CoreResult<()>;
    /// 切换实例加载器（安装/更换组件后更新 `version_id` 映射；不改目录与名称）。
    fn set_loader(&mut self, id: &str, loader: LoaderSpec) -> CoreResult<()>;
    /// 一致性快照：调用方在多次查询间持有时，不会看到中途写入的半成品。
    fn snapshot(&self) -> InstanceSnapshot {
        InstanceSnapshot::new(self.list())
    }
}

/// 不可变实例索引快照（借用 HMCL `GameRepositorySnapshot` 的思路）。
#[derive(Debug, Clone)]
pub struct InstanceSnapshot(std::sync::Arc<[Instance]>);

impl InstanceSnapshot {
    pub fn new(instances: Vec<Instance>) -> Self {
        Self(std::sync::Arc::from(instances))
    }

    pub fn instances(&self) -> &[Instance] {
        &self.0
    }

    pub fn get(&self, id: &str) -> Option<&Instance> {
        self.0.iter().find(|item| item.id == id)
    }

    pub fn known_ids(&self) -> Vec<String> {
        self.0.iter().map(|item| item.id.clone()).collect()
    }
}

/// 内存实现（M0 骨架期使用；M1 替换为 SQLite 实现）
#[derive(Default)]
pub struct MemoryInstanceStore {
    instances: Vec<Instance>,
}

impl MemoryInstanceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InstanceStore for MemoryInstanceStore {
    fn list(&self) -> Vec<Instance> {
        self.instances.clone()
    }

    fn get(&self, id: &str) -> Option<Instance> {
        self.instances.iter().find(|i| i.id == id).cloned()
    }

    fn create(&mut self, instance: Instance) -> CoreResult<()> {
        if self.instances.iter().any(|i| i.id == instance.id) {
            return Err(crate::error::CoreError::Unsupported(format!(
                "实例 ID 已存在：{}",
                instance.id
            )));
        }
        self.instances.push(instance);
        Ok(())
    }

    fn remove(&mut self, id: &str) -> CoreResult<()> {
        self.instances.retain(|i| i.id != id);
        Ok(())
    }

    fn rename(&mut self, id: &str, name: &str) -> CoreResult<()> {
        let name = validate_display_name(name)?;
        let instance = self
            .instances
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| crate::error::CoreError::Unsupported(format!("实例不存在：{id}")))?;
        instance.name = name;
        Ok(())
    }

    fn set_loader(&mut self, id: &str, loader: LoaderSpec) -> CoreResult<()> {
        let instance = self
            .instances
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| crate::error::CoreError::Unsupported(format!("实例不存在：{id}")))?;
        instance.set_loader(loader);
        Ok(())
    }
}

/// 实例文件存储格式（`instances.json`）
#[derive(Debug, Default, Serialize, Deserialize)]
struct InstanceFile {
    #[serde(default)]
    instances: Vec<Instance>,
}

/// 基于 JSON 文件的实例存储（M1 生产实现）
///
/// 落盘于 `<数据目录>/instances.json`，策略与账号/设置一致：先写临时文件再原子改名；
/// 文件缺失或损坏时回退空列表（实例丢失可经重新安装恢复，不阻塞启动）。
/// 每个实例的游戏目录为 `<数据目录>/instances/<id>`。
pub struct FileInstanceStore {
    path: PathBuf,
    data: InstanceFile,
}

impl FileInstanceStore {
    /// 默认存放位置：`<数据目录>/instances.json`
    pub fn default_path() -> PathBuf {
        crate::platform::paths::data_dir().join("instances.json")
    }

    pub fn open(path: impl Into<PathBuf>) -> CoreResult<Self> {
        let path = path.into();
        let data = if path.is_file() {
            serde_json::from_slice(&std::fs::read(&path)?).unwrap_or_default()
        } else {
            InstanceFile::default()
        };
        Ok(Self { path, data })
    }

    pub fn open_default() -> CoreResult<Self> {
        Self::open(Self::default_path())
    }

    fn persist(&self) -> CoreResult<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.data)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 生成唯一实例 ID（`<loader>-<mc_version>`，冲突时追加 `-2`、`-3`…）
    fn unique_id(&self, loader: &LoaderSpec, mc_version: &str) -> String {
        let base = format!("{}-{}", loader_slug(loader), mc_version);
        if !self.data.instances.iter().any(|item| item.id == base) {
            return base;
        }
        let mut suffix = 2;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.data.instances.iter().any(|item| item.id == candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// 创建实例元数据（游戏目录 = `<数据目录>/instances/<id>`；目录本体由安装流程创建）
    pub fn create_for(
        &mut self,
        mc_version: &str,
        loader: LoaderSpec,
        display_name: Option<String>,
    ) -> CoreResult<Instance> {
        self.create_for_in(
            &crate::platform::paths::instances_dir(),
            mc_version,
            loader,
            display_name,
        )
    }

    /// 在指定游戏目录根下创建实例元数据（多目录：由设置的默认目录决定 `root`）
    pub fn create_for_in(
        &mut self,
        root: &Path,
        mc_version: &str,
        loader: LoaderSpec,
        display_name: Option<String>,
    ) -> CoreResult<Instance> {
        let id = self.unique_id(&loader, mc_version);
        let instance = Instance {
            name: display_name.unwrap_or_else(|| format!("{mc_version} {}", loader.label())),
            dir: root.join(&id),
            id,
            mc_version: mc_version.to_string(),
            patches: if matches!(loader, LoaderSpec::Vanilla) {
                Vec::new()
            } else {
                vec![loader]
            },
            last_played: None,
            // 新实例默认共享 assets/libraries
            shared: true,
        };
        InstanceStore::create(self, instance.clone())?;
        Ok(instance)
    }

    /// 记录最近游玩时间（启动成功后调用；失败不影响启动结果）
    pub fn touch_last_played(&mut self, id: &str, timestamp: i64) -> CoreResult<()> {
        if let Some(instance) = self.data.instances.iter_mut().find(|item| item.id == id) {
            instance.last_played = Some(timestamp);
            self.persist()?;
        }
        Ok(())
    }

    /// 旧版单目录迁移（一次性）：扫描 `<legacy_root>/versions/<v>/<v>.json`，
    /// 为每个已安装版本建立指向旧目录的实例，保住已有安装。
    /// 返回新建实例数量；重复调用幂等（已迁移的跳过）。
    pub fn migrate_legacy(&mut self, legacy_root: &Path) -> CoreResult<usize> {
        let versions_dir = legacy_root.join("versions");
        let mut version_names: Vec<String> = std::fs::read_dir(&versions_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        version_names.sort();

        let mut created = 0;
        for version in version_names {
            // 仅迁移"确有版本 JSON"的目录（半途安装的目录不视为可用版本）
            if !versions_dir
                .join(&version)
                .join(format!("{version}.json"))
                .is_file()
            {
                continue;
            }
            if self
                .data
                .instances
                .iter()
                .any(|item| item.dir == legacy_root && item.mc_version == version)
            {
                continue;
            }

            let id = self.unique_id(&LoaderSpec::Vanilla, &version);
            self.data.instances.push(Instance {
                id,
                name: format!("{version}（原有目录）"),
                mc_version: version,
                patches: Vec::new(),
                dir: legacy_root.to_path_buf(),
                last_played: None,
                // 旧目录自带 assets/libraries，保持隔离（不迁移文件，零风险）
                shared: false,
            });
            created += 1;
        }

        if created > 0 {
            self.persist()?;
        }
        Ok(created)
    }
}

impl InstanceStore for FileInstanceStore {
    fn list(&self) -> Vec<Instance> {
        self.data.instances.clone()
    }

    fn get(&self, id: &str) -> Option<Instance> {
        self.data
            .instances
            .iter()
            .find(|item| item.id == id)
            .cloned()
    }

    fn create(&mut self, instance: Instance) -> CoreResult<()> {
        if self
            .data
            .instances
            .iter()
            .any(|item| item.id == instance.id)
        {
            return Err(CoreError::Unsupported(format!(
                "实例 ID 已存在：{}",
                instance.id
            )));
        }
        self.data.instances.push(instance);
        self.persist()
    }

    /// 移除实例记录（**不删除游戏文件**——文件删除是显式的高危操作，M2 由用户二次确认后执行）
    fn remove(&mut self, id: &str) -> CoreResult<()> {
        let before = self.data.instances.len();
        self.data.instances.retain(|item| item.id != id);
        if self.data.instances.len() != before {
            self.persist()?;
        }
        Ok(())
    }

    fn rename(&mut self, id: &str, name: &str) -> CoreResult<()> {
        let name = validate_display_name(name)?;
        let instance = self
            .data
            .instances
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| CoreError::Unsupported(format!("实例不存在：{id}")))?;
        instance.name = name;
        self.persist()
    }

    fn set_loader(&mut self, id: &str, loader: LoaderSpec) -> CoreResult<()> {
        let instance = self
            .data
            .instances
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| CoreError::Unsupported(format!("实例不存在：{id}")))?;
        instance.set_loader(loader);
        self.persist()
    }
}

/// 显示名校验：去首尾空白、非空、≤40 字符（按 Unicode 字符计数）。
fn validate_display_name(name: &str) -> CoreResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Unsupported("实例名不能为空".to_string()));
    }
    if trimmed.chars().count() > 40 {
        return Err(CoreError::Unsupported(
            "实例名过长（最多 40 个字符）".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

/// 从版本 JSON 的 `id` / `inheritsFrom` 推断加载器与 MC 版本。
///
/// 借用 HMCL `GameComponentAnalyzer` 的思路：优先看组件的命名结构，
/// 而不是只认某一种字符串前缀；后续接 Forge/NeoForge 安装时无需改调用方。
pub fn detect_loader(version_id: &str, inherits_from: Option<&str>) -> (LoaderSpec, String) {
    for (prefix, make) in [
        (
            "fabric-loader-",
            LoaderSpec::Fabric as fn(String) -> LoaderSpec,
        ),
        ("quilt-loader-", LoaderSpec::Quilt),
    ] {
        if let Some(rest) = version_id.strip_prefix(prefix) {
            if let Some(mc) = inherits_from {
                let suffix = format!("-{mc}");
                let loader_version = rest.strip_suffix(&suffix).unwrap_or(rest);
                return (make(loader_version.to_string()), mc.to_string());
            }
        }
    }

    if let Some(rest) = version_id.strip_prefix("neoforge-") {
        // neoforge 的版本号不带 MC 版本，靠 inheritsFrom
        return (
            LoaderSpec::NeoForge(rest.to_string()),
            inherits_from.unwrap_or(rest).to_string(),
        );
    }

    if let Some(rest) = version_id.strip_prefix("forge-") {
        // forge-<mc>-<loader>
        if let Some((mc, loader)) = rest.split_once('-') {
            return (
                LoaderSpec::Forge(loader.to_string()),
                inherits_from.unwrap_or(mc).to_string(),
            );
        }
    }

    (
        LoaderSpec::Vanilla,
        inherits_from.unwrap_or(version_id).to_string(),
    )
}

/// 实例 ID 中的加载器片段（目录名保持 ASCII，显示名可用中文）
fn loader_slug(loader: &LoaderSpec) -> &'static str {
    match loader {
        LoaderSpec::Vanilla => "vanilla",
        LoaderSpec::Fabric(_) => "fabric",
        LoaderSpec::Quilt(_) => "quilt",
        LoaderSpec::Forge(_) => "forge",
        LoaderSpec::NeoForge(_) => "neoforge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::LoaderSpec;
    use std::path::PathBuf;

    fn sample(id: &str) -> Instance {
        Instance {
            id: id.to_string(),
            name: format!("实例 {id}"),
            mc_version: "1.20.1".into(),
            patches: vec![LoaderSpec::Fabric("0.15.11".into())],
            dir: PathBuf::from(format!("/tmp/{id}")),
            last_played: None,
            shared: false,
        }
    }

    #[test]
    fn old_instance_json_without_shared_field_loads_as_isolated() {
        // 升级前的 instances.json（无 shared 字段）必须解析为 false，
        // 行为与旧版本一致（自带 assets/libraries），零迁移风险
        let json = r#"{
            "id": "vanilla-1.20.1",
            "name": "1.20.1（原有目录）",
            "mc_version": "1.20.1",
            "loader": { "type": "vanilla" },
            "dir": "/tmp/legacy",
            "last_played": null
        }"#;
        let instance: Instance = serde_json::from_str(json).expect("反序列化");
        assert!(!instance.shared);
    }

    #[test]
    fn create_list_remove_roundtrip() {
        let mut store = MemoryInstanceStore::new();
        store.create(sample("a")).expect("创建成功");
        store.create(sample("b")).expect("创建成功");

        assert_eq!(store.list().len(), 2);
        assert_eq!(store.get("a").expect("应存在").mc_version, "1.20.1");

        store.remove("a").expect("删除成功");
        assert_eq!(store.list().len(), 1);
        assert!(store.get("a").is_none());
    }

    #[test]
    fn rename_trims_and_rejects_invalid() {
        let mut store = MemoryInstanceStore::new();
        store.create(sample("a")).expect("创建成功");

        store.rename("a", "  生存服  ").expect("重命名成功");
        assert_eq!(store.get("a").unwrap().name, "生存服");

        assert!(store.rename("a", "   ").is_err(), "空名应被拒绝");
        assert!(store.rename("a", &"长".repeat(41)).is_err(), "超长应被拒绝");
        assert!(store.rename("missing", "x").is_err(), "不存在的实例应报错");
    }

    #[test]
    fn set_loader_keeps_instance_version_dir() {
        let mut store = MemoryInstanceStore::new();
        store.create(sample("a")).expect("创建成功");
        // 目录按实例固定：切换加载器不换目录
        assert_eq!(store.get("a").unwrap().version_id(), "a");
        assert_eq!(
            store.get("a").unwrap().loader(),
            &LoaderSpec::Fabric("0.15.11".to_string())
        );

        store
            .set_loader("a", LoaderSpec::Forge("47.2.0".to_string()))
            .expect("切换加载器");
        let instance = store.get("a").expect("实例存在");
        assert_eq!(instance.name, "实例 a");
        assert_eq!(instance.version_id(), "a");
        assert_eq!(instance.loader(), &LoaderSpec::Forge("47.2.0".into()));
        // 补丁层仍是单层（叠加能力预留）
        assert_eq!(instance.patches.len(), 1);
        assert_eq!(instance.patch_signature(), "Forge 47.2.0");
    }

    #[test]
    fn file_store_rename_persists() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mcl-rename-{stamp}"));
        let path = dir.join("instances.json");

        let mut store = FileInstanceStore::open(&path).expect("打开存储");
        store.create(sample("a")).expect("创建成功");
        store.rename("a", "新名字").expect("重命名成功");
        drop(store);

        let reloaded = FileInstanceStore::open(&path).expect("重新打开");
        assert_eq!(reloaded.get("a").unwrap().name, "新名字");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_loaders_from_version_ids() {
        assert!(matches!(
            detect_loader("26.3", None),
            (LoaderSpec::Vanilla, mc) if mc == "26.3"
        ));
        assert!(matches!(
            detect_loader("fabric-loader-0.16.9-1.20.1", Some("1.20.1")),
            (LoaderSpec::Fabric(v), mc) if v == "0.16.9" && mc == "1.20.1"
        ));
        assert!(matches!(
            detect_loader("quilt-loader-0.24.0-1.20.1", Some("1.20.1")),
            (LoaderSpec::Quilt(v), mc) if v == "0.24.0" && mc == "1.20.1"
        ));
        assert!(matches!(
            detect_loader("neoforge-21.1.80", Some("1.21.1")),
            (LoaderSpec::NeoForge(v), mc) if v == "21.1.80" && mc == "1.21.1"
        ));
        assert!(matches!(
            detect_loader("forge-1.20.1-47.2.0", None),
            (LoaderSpec::Forge(v), mc) if v == "47.2.0" && mc == "1.20.1"
        ));
    }

    #[test]
    fn snapshot_is_stable_across_writes() {
        let mut store = MemoryInstanceStore::new();
        store.create(sample("a")).expect("创建成功");
        let snapshot = store.snapshot();

        store.create(sample("b")).expect("创建成功");
        assert_eq!(snapshot.instances().len(), 1, "快照不应看到后续写入");
        assert!(snapshot.get("b").is_none());
        assert_eq!(store.snapshot().known_ids(), vec!["a", "b"]);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let mut store = MemoryInstanceStore::new();
        store.create(sample("dup")).expect("首次创建成功");
        assert!(store.create(sample("dup")).is_err(), "重复 ID 应被拒绝");
    }

    #[test]
    fn file_store_roundtrip_unique_ids_and_last_played() {
        let dir = std::env::temp_dir().join("mcl-instance-store");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("instances.json");

        let first = {
            let mut store = FileInstanceStore::open(&path).expect("打开");
            let instance = store
                .create_for("1.20.1", LoaderSpec::Vanilla, None)
                .expect("创建");
            assert_eq!(instance.id, "vanilla-1.20.1");
            assert_eq!(instance.name, "1.20.1 原版");
            assert!(instance.shared, "新实例默认共享 assets/libraries");
            assert!(instance
                .dir
                .starts_with(crate::platform::paths::instances_dir()));

            // 同版本同加载器再建 → 追加后缀，不覆盖
            let second = store
                .create_for("1.20.1", LoaderSpec::Vanilla, None)
                .expect("创建");
            assert_eq!(second.id, "vanilla-1.20.1-2");
            instance
        };

        // 重新打开：数据持久化、最近游玩可记录
        let mut reopened = FileInstanceStore::open(&path).expect("重开");
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(reopened.get(&first.id).expect("存在").mc_version, "1.20.1");
        reopened
            .touch_last_played(&first.id, 123_456)
            .expect("记录");

        let again = FileInstanceStore::open(&path).expect("三开");
        assert_eq!(
            again.get(&first.id).expect("存在").last_played,
            Some(123_456)
        );

        // 移除记录不影响其余实例
        let mut removed = FileInstanceStore::open(&path).expect("四开");
        removed.remove(&first.id).expect("移除");
        assert_eq!(removed.list().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_is_idempotent_and_skips_partial_dirs() {
        let dir = std::env::temp_dir().join("mcl-instance-migrate");
        let _ = std::fs::remove_dir_all(&dir);

        // 伪造旧目录：两个已装版本 + 一个半途目录（无版本 JSON，应跳过）
        for version in ["1.19.2", "1.20.1"] {
            let version_dir = dir.join("minecraft").join("versions").join(version);
            std::fs::create_dir_all(&version_dir).expect("建目录");
            std::fs::write(version_dir.join(format!("{version}.json")), b"{}").expect("写 JSON");
        }
        std::fs::create_dir_all(dir.join("minecraft").join("versions").join("半途"))
            .expect("建目录");

        let legacy_root = dir.join("minecraft");
        let store_path = dir.join("instances.json");

        let mut store = FileInstanceStore::open(&store_path).expect("打开");
        assert_eq!(store.migrate_legacy(&legacy_root).expect("迁移"), 2);
        assert_eq!(store.migrate_legacy(&legacy_root).expect("重复调用幂等"), 0);

        let instances = store.list();
        assert!(instances.iter().all(|item| item.dir == legacy_root));
        assert!(
            instances.iter().any(|item| item.id == "vanilla-1.20.1"),
            "应登记 1.20.1：{instances:?}"
        );
        assert!(
            instances.iter().all(|item| item.mc_version != "半途"),
            "半途目录不应登记"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
