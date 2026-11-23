use std::path::PathBuf;

use mcl_core::instance::{FileInstanceStore, InstanceStore};
use mcl_core::models::Instance;

/// 按 ID 读取实例（不存在/存储不可用时返回 `None`）。
pub fn instance_by_id(id: &str) -> Option<Instance> {
    FileInstanceStore::open_default()
        .ok()
        .and_then(|store| store.get(id))
}

/// 把 UI 传来的模组文件名解析为实例 `mods/` 目录内的真实路径。
///
/// 只接受纯文件名（拒绝 `/`、`\`、`..`），并同时尝试启用/禁用两种磁盘形态。
/// 只依赖 `instance_id`，不接触任何 UI 状态。
pub fn mod_file_path(instance_id: &str, file_name: &str) -> Option<PathBuf> {
    if file_name.is_empty() || file_name.contains(['/', '\\']) || file_name.contains("..") {
        return None;
    }
    let instance = instance_by_id(instance_id)?;
    let mods_dir = instance.dir.join("mods");

    let direct = mods_dir.join(file_name);
    if direct.is_file() {
        return Some(direct);
    }
    let disabled = mods_dir.join(format!("{file_name}.disabled"));
    disabled.is_file().then_some(disabled)
}

/// 首次运行迁移：把旧版单目录（`<数据目录>/minecraft`）中已安装的版本登记为实例，
/// 保住升级前已下载的游戏（仅当 `instances.json` 尚不存在时执行一次）。
///
/// 失败只写 stderr，不阻塞启动。
pub fn migrate_legacy_instances() {
    if FileInstanceStore::default_path().exists() {
        return;
    }
    let legacy_root = mcl_core::platform::paths::data_dir().join("minecraft");
    if !legacy_root.join("versions").is_dir() {
        return;
    }
    match FileInstanceStore::open_default() {
        Ok(mut store) => match store.migrate_legacy(&legacy_root) {
            Ok(count) if count > 0 => {
                eprintln!("[migrate] 已把旧目录中 {count} 个版本登记为实例");
            }
            Ok(_) => {}
            Err(error) => eprintln!("[migrate] 实例迁移失败：{error}"),
        },
        Err(error) => eprintln!("[migrate] 实例存储打开失败：{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_file_path_rejects_unsafe_names() {
        // 这些名字在触碰实例存储之前就应被拒绝，因此不需要真实实例
        for name in ["", "../evil.jar", "a/b.jar", "a\\b.jar", ".."] {
            assert!(
                mod_file_path("any-instance", name).is_none(),
                "应拒绝非法文件名：{name}"
            );
        }
    }
}
