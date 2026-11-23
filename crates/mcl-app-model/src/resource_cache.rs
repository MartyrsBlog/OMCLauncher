//! 资源版本列表的磁盘缓存（最小方案）。
//!
//! 只缓存「某个项目在给定 MC 版本 / 加载器下的版本列表」，TTL 24 小时；
//! 不做 manifest、不做预取 —— 条目少时扫目录的成本可以忽略。
//! 启动时清掉 7 天前的文件，超过 256 MB 按 mtime 删到 90%。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::Serialize;
use sha1::{Digest, Sha1};

/// 缓存有效期：版本列表以天为单位变化，24 小时足够新鲜。
pub const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 起点上限：元数据都很小，256 MB 约等于上万条，短期不会到。
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// 无论大小，超过 7 天的条目一律清掉。
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 缓存目录：`<数据目录>/cache/resource`
pub fn cache_dir() -> PathBuf {
    mcl_core::platform::paths::data_dir()
        .join("cache")
        .join("resource")
}

fn path_for(key: &str) -> PathBuf {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    cache_dir().join(format!("{:x}.json", hasher.finalize()))
}

/// 读缓存。超过 TTL 视为未命中（不返回过期数据，避免 UI 显示旧版本）。
pub fn load<T: DeserializeOwned>(key: &str) -> Option<T> {
    let path = path_for(key);
    let age = fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > TTL {
        return None;
    }
    serde_json::from_slice(&fs::read(&path).ok()?).ok()
}

/// 写缓存。失败静默（缓存不能影响主流程）。
pub fn store<T: Serialize>(key: &str, value: &T) {
    let dir = cache_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(value) {
        let _ = fs::write(path_for(key), bytes);
    }
}

/// 启动清理：先删过期，再按 mtime 删到上限的 90%。
pub fn prune_cache() -> u64 {
    prune_dir(&cache_dir(), MAX_BYTES, MAX_AGE)
}

fn prune_dir(dir: &Path, limit: u64, max_age: Duration) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut removed = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(now);
        if now.duration_since(mtime).unwrap_or_default() > max_age {
            if fs::remove_file(entry.path()).is_ok() {
                removed += meta.len();
            }
            continue;
        }
        files.push((mtime, meta.len(), entry.path()));
    }

    let total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    if total <= limit {
        return removed;
    }
    files.sort_by_key(|(mtime, _, _)| *mtime);
    let target = limit / 10 * 9;
    let mut remaining = total;
    for (_, size, path) in files {
        if remaining <= target {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            remaining = remaining.saturating_sub(size);
            removed += size;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_and_distinct() {
        assert_eq!(path_for("versions:a"), path_for("versions:a"));
        assert_ne!(path_for("versions:a"), path_for("versions:b"));
        assert!(path_for("versions:a").starts_with(cache_dir()));
    }

    #[test]
    fn prune_removes_expired_and_oversized() {
        let dir = std::env::temp_dir().join(format!("mcl-cache-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.json"), vec![0u8; 100]).unwrap();
        fs::write(dir.join("b.json"), vec![0u8; 100]).unwrap();
        // 上限设成 100：总量 200，应删到 90 以下（删一个）
        let removed = prune_dir(&dir, 100, Duration::from_secs(3600));
        assert!(removed >= 100);
        let left: u64 = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum();
        assert!(left <= 90);
        let _ = fs::remove_dir_all(&dir);
    }
}
