//! 扫描实例根目录，收养未登记但结构完整的实例（手动复制 / 恢复备份后使用）。
//!
//! 判定条件：`<dir>/versions/<x>/<x>.json` 可解析出 `id`；
//! 加载器从 `id` / `inheritsFrom` 推断（当前支持原版与 Fabric）。

use std::path::Path;

use mcl_core::models::Instance;

/// 返回 `root` 下尚未在 `known_ids` 中的实例（按目录名排序，结果稳定）。
pub fn scan_orphans(root: &Path, known_ids: &[String]) -> Vec<Instance> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    let shared = root
        .parent()
        .map(|data| data.join("shared/assets").is_dir())
        .unwrap_or(false);

    dirs.into_iter()
        .filter_map(|dir| {
            let id = dir.file_name()?.to_str()?.to_string();
            if known_ids.iter().any(|known| known == &id) {
                return None;
            }
            instance_from_dir(id, &dir, shared)
        })
        .collect()
}

/// 扫描多个游戏目录根（多目录支持）：合并各根的孤儿实例，按 id 去重。
pub fn scan_all_roots(roots: &[std::path::PathBuf], known_ids: &[String]) -> Vec<Instance> {
    let mut found: Vec<Instance> = Vec::new();
    for root in roots {
        for instance in scan_orphans(root, known_ids) {
            if found.iter().any(|existing| existing.id == instance.id) {
                continue;
            }
            found.push(instance);
        }
    }
    found
}

/// 递归统计目录占用（字节）。目录不存在时返回 0；只读元数据，不跟随符号链接。
pub fn dir_size(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total += dir_size(&entry.path());
        } else if file_type.is_file() {
            total += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        }
    }
    total
}

fn instance_from_dir(id: String, dir: &Path, shared: bool) -> Option<Instance> {
    let versions = dir.join("versions");
    let mut entries: Vec<_> = std::fs::read_dir(versions)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();

    for version_dir in entries {
        let Some(json_path) = first_json(&version_dir) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&json_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(version_id) = json.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        let inherits = json.get("inheritsFrom").and_then(|value| value.as_str());
        let (loader, mc_version) = mcl_core::instance::detect_loader(version_id, inherits);

        return Some(Instance {
            name: id.clone(),
            id,
            mc_version,
            patches: if matches!(loader, mcl_core::models::LoaderSpec::Vanilla) {
                Vec::new()
            } else {
                vec![loader]
            },
            dir: dir.to_path_buf(),
            last_played: None,
            shared,
        });
    }
    None
}

fn first_json(dir: &Path) -> Option<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files.into_iter().next()
}

/// 从版本 JSON 的 `id` / `inheritsFrom` 推断加载器与 MC 版本。
///
/// 实现在 `mcl_core::instance::detect_loader`（与安装/启动共用同一套识别规则）。
#[cfg(test)]
mod tests {
    use super::*;
    use mcl_core::models::LoaderSpec;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mcl-scan-{tag}-{stamp}"));
        std::fs::create_dir_all(&root).expect("创建临时目录");
        root
    }

    fn write_version(root: &Path, dir: &str, version_id: &str, inherits: Option<&str>) {
        let version_dir = root.join(dir).join("versions").join(version_id);
        std::fs::create_dir_all(&version_dir).expect("创建版本目录");
        let mut json = serde_json::json!({ "id": version_id });
        if let Some(mc) = inherits {
            json["inheritsFrom"] = serde_json::Value::String(mc.to_string());
        }
        std::fs::write(
            version_dir.join(format!("{version_id}.json")),
            serde_json::to_vec(&json).unwrap(),
        )
        .expect("写版本 JSON");
    }

    #[test]
    fn scans_vanilla_and_fabric_instances() {
        let root = temp_root("mixed");
        write_version(&root, "vanilla-26.3", "26.3", None);
        write_version(
            &root,
            "fabric-26.3",
            "fabric-loader-0.16.9-26.3",
            Some("26.3"),
        );

        let found = scan_orphans(&root, &[]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|item| item.id == "vanilla-26.3"
            && item.mc_version == "26.3"
            && matches!(item.loader(), LoaderSpec::Vanilla)));
        assert!(found.iter().any(|item| item.id == "fabric-26.3"
            && item.mc_version == "26.3"
            && matches!(item.loader(), LoaderSpec::Fabric(v) if v == "0.16.9")));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn skips_known_and_incomplete_dirs() {
        let root = temp_root("skip");
        write_version(&root, "vanilla-26.3", "26.3", None);
        std::fs::create_dir_all(root.join("empty-dir")).unwrap();

        let found = scan_orphans(&root, &["vanilla-26.3".to_string()]);
        assert!(found.is_empty(), "已登记或无版本 JSON 的目录应跳过");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let root = std::env::temp_dir().join(format!("mcl-dir-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).expect("mkdir");
        std::fs::write(root.join("a.bin"), vec![0u8; 100]).expect("write a");
        std::fs::write(root.join("nested/b.bin"), vec![0u8; 23]).expect("write b");

        assert_eq!(dir_size(&root), 123);
        assert_eq!(dir_size(&root.join("missing")), 0);

        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
