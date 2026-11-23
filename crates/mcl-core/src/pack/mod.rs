//! 整合包导入：Modrinth `.mrpack` 解析与安装 + 内置精选清单
//!
//! 安装流水线（`install_mrpack`，CLI 与 GUI 共用）：
//! 1. 解析 `.mrpack`（清单 + 加载器版本）；
//! 2. 落实例并安装加载器；
//! 3. 下载清单内文件（`mods/`、`resourcepacks/` 等，SHA-1 校验、命中跳过）；
//! 4. 解压 `overrides/` 与 `client-overrides/` 到实例根。
//!
//! 取消/续传语义沿用下载引擎：暂停后再次执行同一条目即续传（哈希跳过 + `.part`）。
//!
//! ⚠️ 加载器范围：当前仅支持 **Fabric** 整合包；Quilt/Forge/NeoForge 会返回可读错误
//!（需求文档 M2 计划覆盖其余加载器）。CurseForge 清单（`manifest.json`）需要
//! CurseForge API Key 才能解析文件下载地址，暂不内置。

pub mod curated;
pub mod mrpack;

use std::path::{Path, PathBuf};

use crate::download::DownloadEngine;
use crate::error::{CoreError, CoreResult};
use crate::game::GameLayout;
use crate::instance::{FileInstanceStore, InstanceStore};
use crate::models::{FileEntry, Instance, LoaderSpec};
use crate::version::service::{InstallOptions, VersionService};

pub use curated::{curated_pack, CuratedPack, CURATED_PACKS};
pub use mrpack::{extract_overrides, parse_index, read_mrpack, ModpackIndex, PackFile};

/// 整合包安装结果（加载器 + 清单文件 + overrides 的合并报告）
#[derive(Debug, Clone)]
pub struct PackInstallReport {
    pub instance: Instance,
    /// 实际使用的加载器版本
    pub loader: String,
    /// 加载器流水线派发的文件数
    pub loader_files: usize,
    /// 清单文件派发数
    pub files: usize,
    /// 清单文件命中跳过数
    pub skipped: usize,
    /// 清单文件实际下载字节
    pub bytes: u64,
    /// 解压的 overrides 文件数
    pub overrides: usize,
    pub failures: Vec<(String, String)>,
    /// 清单层面的问题条目（如缺下载地址）
    pub issues: Vec<(String, String)>,
    pub cancelled: bool,
}

impl PackInstallReport {
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty() && !self.cancelled
    }

    /// 需要重新下载的清单文件数
    pub fn redownloaded(&self) -> usize {
        self.files.saturating_sub(self.skipped)
    }
}

/// 确保精选整合包的 `.mrpack` 已在本地缓存（SHA-1 命中则跳过下载），返回文件路径
pub async fn ensure_curated_mrpack(
    engine: &DownloadEngine,
    pack: &CuratedPack,
    cache_dir: &Path,
) -> CoreResult<PathBuf> {
    std::fs::create_dir_all(cache_dir)?;
    let dest = cache_dir.join(format!("{}.mrpack", pack.id));
    let entry = FileEntry {
        path: format!("{}.mrpack", pack.id),
        url: pack.url.to_string(),
        sha1: Some(pack.sha1.to_string()),
        size: pack.size,
    };
    engine.fetch(&entry, &dest).await?;
    Ok(dest)
}

/// 安装 `.mrpack` 到新实例（或复用同名未装成的实例）
pub async fn install_mrpack(
    version: &VersionService,
    store: &mut FileInstanceStore,
    engine: &DownloadEngine,
    mrpack_path: &Path,
    options: &InstallOptions,
    task_id: u64,
) -> CoreResult<PackInstallReport> {
    let index = read_mrpack(mrpack_path)?;
    let loader_version = match &index.loader {
        LoaderSpec::Fabric(loader) => loader.clone(),
        other => {
            return Err(CoreError::Unsupported(format!(
                "该整合包需要 {}：当前版本仅支持 Fabric 整合包（其余加载器见 M2 计划）",
                other.label()
            )));
        }
    };

    // 复用「同名且未装成」的实例（失败重试/暂停续传不在列表里堆叠空实例）
    let reusable = store.list().into_iter().find(|instance| {
        instance.name == index.name
            && instance.mc_version == index.mc_version
            && instance.loader() == &index.loader
            && !GameLayout::for_instance(instance)
                .version_json(&instance.version_id())
                .is_file()
    });
    let instance = match reusable {
        Some(instance) => instance,
        None => store.create_for_in(
            &crate::settings::LauncherSettings::load().default_instances_root(),
            &index.mc_version,
            index.loader.clone(),
            Some(index.name.clone()),
        )?,
    };
    let layout = GameLayout::for_instance(&instance);

    // ① 加载器（原版 + Fabric）
    let (_, loader_report) = version
        .install_fabric(
            &index.mc_version,
            Some(&loader_version),
            Some(&instance.version_id()),
            &layout,
            engine,
            options,
            task_id,
        )
        .await?;

    let mut report = PackInstallReport {
        instance: instance.clone(),
        loader: loader_version,
        loader_files: loader_report.files,
        files: 0,
        skipped: 0,
        bytes: 0,
        overrides: 0,
        failures: loader_report.failures.clone(),
        issues: index.issues.clone(),
        cancelled: loader_report.cancelled,
    };
    if !loader_report.is_complete() {
        return Ok(report);
    }

    // ② 清单内文件（相对实例根：mods/、resourcepacks/ 等）
    let stats = engine
        .fetch_many_cancellable(
            index.file_entries(),
            layout.root(),
            task_id,
            options.cancel.clone(),
        )
        .await;
    report.files = stats.files;
    report.skipped = stats.skipped;
    report.bytes = stats.bytes;
    report.cancelled = stats.cancelled;
    report.failures.extend(stats.failures.clone());
    if !stats.failures.is_empty() || stats.cancelled {
        return Ok(report);
    }

    // ③ overrides / client-overrides
    report.overrides = extract_overrides(mrpack_path, layout.root())?;
    Ok(report)
}

/// 便捷入口：精选包 → 缓存（`<数据目录>/meta/packs/`）→ 安装（CLI / GUI 均走此路径）
pub async fn install_curated_pack(
    version: &VersionService,
    store: &mut FileInstanceStore,
    engine: &DownloadEngine,
    pack: &CuratedPack,
    options: &InstallOptions,
    task_id: u64,
) -> CoreResult<PackInstallReport> {
    let cache_dir = crate::platform::paths::data_dir()
        .join("meta")
        .join("packs");
    let mrpack = ensure_curated_mrpack(engine, pack, &cache_dir).await?;
    install_mrpack(version, store, engine, &mrpack, options, task_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_install_entry_points_are_consistent() {
        // 精选清单里的条目都应能被 install_curated_pack 解析（ID 可查、URL 可校验）
        for pack in CURATED_PACKS {
            let found = curated_pack(pack.id).expect("ID 应可查");
            assert_eq!(found.sha1, pack.sha1);
            assert!(pack.size > 0, "体积应已知（展示与预分配用）：{}", pack.id);
        }
    }
}
