//! 维护操作
//!
//! 共享目录（`<数据目录>/shared/{libraries,assets}`）跨实例复用，但**没有引用计数**：
//! 「移除实例」只移除登记，共享文件不会回收，与需求文档「引用归零后回收」存在差距。
//! 本模块用**可达性分析**补上这一步：扫描所有启用共享的实例，从它们已落地的版本
//! JSON 推导出真正被引用的类库与资源对象，其余文件即无主文件（即便误删，也能由
//! 安装/修复流程重新下载，风险可控）。
//!
//! 防误删策略：
//! - **没有已登记实例时拒绝清理**（避免 instances.json 丢失时清空整个缓存）；
//! - 只统计 `shared == true` 的实例（隔离实例的文件在自身目录，不占共享区）；
//! - 引用到的资源索引读不出来时，**保留全部资源对象**（保守策略，宁可留不可误删）；
//! - `.part` / `.partN`（未完成的下载）一律保留，不影响续传。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{CoreError, CoreResult};
use crate::models::Instance;

/// 一次回收计划（先规划后执行：UI 可先展示"将清理什么"，用户确认后再执行）
#[derive(Debug, Default, Clone)]
pub struct GcPlan {
    /// 共享目录根（执行时用于清理空目录）
    pub root: PathBuf,
    /// 计划删除的文件（绝对路径）
    pub candidates: Vec<PathBuf>,
    /// 计划释放的字节数
    pub total_bytes: u64,
    /// 读不到的资源索引（对应资源对象整体保留）
    pub unknown_indexes: Vec<String>,
}

impl GcPlan {
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub fn total_mb(&self) -> f64 {
        self.total_bytes as f64 / 1_048_576.0
    }
}

/// 执行结果
#[derive(Debug, Default, Clone)]
pub struct GcReport {
    pub removed: usize,
    pub removed_bytes: u64,
    pub failures: Vec<(String, String)>,
}

impl GcReport {
    pub fn removed_mb(&self) -> f64 {
        self.removed_bytes as f64 / 1_048_576.0
    }
}

/// 规划共享目录回收：`shared_root` 下未被任何共享实例引用的文件列为候选
pub fn plan_shared_gc(shared_root: &Path, instances: &[Instance]) -> CoreResult<GcPlan> {
    if instances.is_empty() {
        return Err(CoreError::Unsupported(
            "没有已登记的实例：为避免误删共享缓存，已跳过清理".into(),
        ));
    }

    // ① 引用集合（相对 shared_root，`/` 分隔）
    let mut referenced: HashSet<String> = HashSet::new();
    let mut unknown_indexes: Vec<String> = Vec::new();
    for instance in instances.iter().filter(|instance| instance.shared) {
        collect_instance_refs(instance, shared_root, &mut referenced, &mut unknown_indexes);
    }

    // ② 遍历共享目录：引用之外的文件即候选
    let mut plan = GcPlan {
        root: shared_root.to_path_buf(),
        unknown_indexes,
        ..GcPlan::default()
    };
    let keep_asset_objects = plan.unknown_indexes.is_empty();
    if shared_root.is_dir() {
        walk(
            shared_root,
            shared_root,
            &referenced,
            keep_asset_objects,
            &mut plan,
        );
    }
    plan.candidates.sort();
    Ok(plan)
}

/// 执行清理计划（删除候选文件，并顺带清掉因此变空的目录）
pub fn apply_shared_gc(plan: &GcPlan) -> GcReport {
    let mut report = GcReport::default();
    for path in &plan.candidates {
        let size = std::fs::metadata(path)
            .map(|meta| meta.len())
            .unwrap_or_default();
        match std::fs::remove_file(path) {
            Ok(()) => {
                report.removed += 1;
                report.removed_bytes += size;
            }
            Err(error) => report
                .failures
                .push((path.display().to_string(), error.to_string())),
        }
    }
    if !plan.root.as_os_str().is_empty() {
        remove_empty_dirs(&plan.root);
    }
    report
}

/// 递归清理空目录（自底向上；根目录保留）
fn remove_empty_dirs(root: &Path) {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut empty = true;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path) {
                    let _ = std::fs::remove_dir(&path);
                } else {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        empty
    }
    if root.is_dir() {
        walk(root);
    }
}

/// 单实例引用提取：遍历其 `versions/<id>/<id>.json`（合并后的清单，加载器实例同样如此）
fn collect_instance_refs(
    instance: &Instance,
    shared_root: &Path,
    referenced: &mut HashSet<String>,
    unknown_indexes: &mut Vec<String>,
) {
    let versions_dir = instance.dir.join("versions");
    let Ok(entries) = std::fs::read_dir(&versions_dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let json_path = dir.join(format!("{name}.json"));
        let Ok(bytes) = std::fs::read(&json_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };

        collect_library_refs(&value, referenced);
        if let Some(index_id) = value.pointer("/assetIndex/id").and_then(Value::as_str) {
            collect_asset_refs(shared_root, index_id, referenced, unknown_indexes);
        }
    }
}

/// 类库引用：优先 `downloads.artifact.path`（安装计划补全过），回退 Maven 坐标推导
fn collect_library_refs(value: &Value, referenced: &mut HashSet<String>) {
    let Some(libraries) = value.get("libraries").and_then(Value::as_array) else {
        return;
    };
    for library in libraries {
        if let Some(path) = library
            .pointer("/downloads/artifact/path")
            .and_then(Value::as_str)
        {
            referenced.insert(format!("libraries/{path}"));
            continue;
        }
        if let Some(name) = library.get("name").and_then(Value::as_str) {
            if let Some(path) = crate::loader::fabric::maven_relative_path(name) {
                referenced.insert(format!("libraries/{path}"));
            }
        }
    }
}

/// 资源对象引用：资源索引（`assets/indexes/<id>.json`）→ 内容寻址对象路径。
/// 索引缺失或损坏时记入 `unknown_indexes`，由调用方整体保守保留。
fn collect_asset_refs(
    shared_root: &Path,
    index_id: &str,
    referenced: &mut HashSet<String>,
    unknown_indexes: &mut Vec<String>,
) {
    referenced.insert(format!("assets/indexes/{index_id}.json"));

    let index_path = shared_root
        .join("assets")
        .join("indexes")
        .join(format!("{index_id}.json"));
    let parsed = std::fs::read(&index_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());

    let Some(value) = parsed else {
        if !unknown_indexes.iter().any(|known| known == index_id) {
            unknown_indexes.push(index_id.to_string());
        }
        return;
    };
    let Some(objects) = value.get("objects").and_then(Value::as_object) else {
        return;
    };
    for object in objects.values() {
        if let Some(hash) = object.get("hash").and_then(Value::as_str) {
            if hash.len() >= 2 {
                referenced.insert(format!("assets/objects/{}/{}", &hash[..2], hash));
            }
        }
    }
}

/// 递归遍历共享目录，收集候选文件
fn walk(
    root: &Path,
    dir: &Path,
    referenced: &HashSet<String>,
    keep_asset_objects: bool,
    plan: &mut GcPlan,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, referenced, keep_asset_objects, plan);
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");

        // 未完成的下载：保留（续传依赖）
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if is_partial(file_name) {
            continue;
        }
        // 索引未知时保留全部资源对象（保守）
        if !keep_asset_objects && relative.starts_with("assets/objects/") {
            continue;
        }
        // `.sha1` 侧车与主文件同生共死
        let reference_key = relative.strip_suffix(".sha1").unwrap_or(&relative);
        if referenced.contains(reference_key) {
            continue;
        }

        plan.total_bytes += entry.metadata().map(|meta| meta.len()).unwrap_or_default();
        plan.candidates.push(path);
    }
}

/// 未完成的下载文件（`xxx.part` / `xxx.partN`）
fn is_partial(file_name: &str) -> bool {
    match file_name.rsplit_once(".part") {
        Some((_, suffix)) => suffix.is_empty() || suffix.chars().all(|ch| ch.is_ascii_digit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mcl-gc-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    fn shared_instance(dir: &Path) -> Instance {
        Instance {
            id: "test".into(),
            name: "Test".into(),
            mc_version: "1.20.1".into(),
            patches: Vec::new(),
            dir: dir.to_path_buf(),
            last_played: None,
            shared: true,
        }
    }

    #[test]
    fn collects_stale_files_and_keeps_referenced_ones() {
        let root = temp_dir("plan");
        let shared = root.join("shared");
        let instance_dir = root.join("instance");

        // 共享区：1 个被引用类库 + 1 个无主类库；1 个被引用资源对象 + 1 个无主对象
        std::fs::create_dir_all(shared.join("libraries/a/b")).expect("建目录");
        std::fs::write(shared.join("libraries/a/b/keep.jar"), b"keep").expect("写");
        std::fs::write(shared.join("libraries/stale.jar"), b"stale").expect("写");
        std::fs::create_dir_all(shared.join("assets/objects/ab")).expect("建目录");
        std::fs::write(shared.join("assets/objects/ab/keephash"), b"keep").expect("写");
        std::fs::create_dir_all(shared.join("assets/objects/cd")).expect("建目录");
        std::fs::write(shared.join("assets/objects/cd/stalehash"), b"stale").expect("写");
        std::fs::write(shared.join("libraries/inflight.jar.part"), b"partial").expect("写");

        // 资源索引：引用 ab/keephash
        std::fs::create_dir_all(shared.join("assets/indexes")).expect("建目录");
        std::fs::write(
            shared.join("assets/indexes/5.json"),
            br#"{ "objects": { "minecraft/x": { "hash": "abcdef0123456789", "size": 1 } } }"#,
        )
        .expect("写索引");
        // 上面 hash 的前两位是 "ab"，但完整对象名需与 hash 一致：改用真实文件名保持自洽
        std::fs::rename(
            shared.join("assets/objects/ab/keephash"),
            shared.join("assets/objects/ab/abcdef0123456789"),
        )
        .expect("改名");

        // 实例版本 JSON：引用 libraries/a/b/keep.jar 与 assetIndex 5
        let version_dir = instance_dir.join("versions/1.20.1");
        std::fs::create_dir_all(&version_dir).expect("建目录");
        std::fs::write(
            version_dir.join("1.20.1.json"),
            br#"{
                "libraries": [
                    { "name": "a:b:1", "downloads": { "artifact": { "path": "a/b/keep.jar" } } }
                ],
                "assetIndex": { "id": "5" }
            }"#,
        )
        .expect("写 JSON");

        let instances = vec![shared_instance(&instance_dir)];
        let plan = plan_shared_gc(&shared, &instances).expect("规划");
        assert!(plan.unknown_indexes.is_empty(), "索引可读，不应有未知索引");

        let names: Vec<String> = plan
            .candidates
            .iter()
            .map(|path| {
                path.strip_prefix(&shared)
                    .expect("在共享目录内")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            names,
            vec!["assets/objects/cd/stalehash", "libraries/stale.jar"],
            "只应清理无主文件"
        );

        let report = apply_shared_gc(&plan);
        assert_eq!(report.removed, 2);
        assert!(report.failures.is_empty());
        assert!(shared.join("libraries/a/b/keep.jar").is_file());
        assert!(!shared.join("libraries/stale.jar").exists());
        assert!(
            shared.join("libraries/inflight.jar.part").is_file(),
            "续传文件（.part）必须保留"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_when_no_instances_registered() {
        let root = temp_dir("empty");
        let error = plan_shared_gc(&root.join("shared"), &[]).expect_err("应拒绝");
        assert!(error.to_string().contains("没有已登记的实例"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_asset_objects_when_index_unreadable() {
        let root = temp_dir("unknown-index");
        let shared = root.join("shared");
        let instance_dir = root.join("instance");

        std::fs::create_dir_all(shared.join("assets/objects/ff")).expect("建目录");
        std::fs::write(shared.join("assets/objects/ff/orphan"), b"x").expect("写");

        let version_dir = instance_dir.join("versions/1.20.1");
        std::fs::create_dir_all(&version_dir).expect("建目录");
        std::fs::write(
            version_dir.join("1.20.1.json"),
            br#"{ "assetIndex": { "id": "5" } }"#,
        )
        .expect("写 JSON");

        let plan = plan_shared_gc(&shared, &[shared_instance(&instance_dir)]).expect("规划");
        assert_eq!(plan.unknown_indexes, vec!["5".to_string()]);
        assert!(plan.is_empty(), "索引缺失时应保留全部资源对象");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_shared_instances_do_not_keep_shared_files() {
        let root = temp_dir("non-shared");
        let shared = root.join("shared");
        let instance_dir = root.join("instance");

        std::fs::create_dir_all(shared.join("libraries")).expect("建目录");
        std::fs::write(shared.join("libraries/orphan.jar"), b"x").expect("写");

        let version_dir = instance_dir.join("versions/1.20.1");
        std::fs::create_dir_all(&version_dir).expect("建目录");
        std::fs::write(
            version_dir.join("1.20.1.json"),
            br#"{ "libraries": [ { "name": "a:b:1", "downloads": { "artifact": { "path": "a/b/orphan.jar" } } } ] }"#,
        )
        .expect("写 JSON");

        let mut instance = shared_instance(&instance_dir);
        instance.shared = false;
        let plan = plan_shared_gc(&shared, &[instance]).expect("规划");
        assert_eq!(
            plan.candidates.len(),
            1,
            "隔离实例不占用共享区，其引用不保留共享文件"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
