//! 应用服务层：安装 / 启动 / 修复 / 整合包编排。
//!
//! 这些流程原先住在 UI 壳里，但它们与界面无关——只依赖 `mcl-core` 与任务注册表。
//! 下沉到这里之后，任何 UI 壳（Slint / 未来的 GPUI / CLI）都能复用同一套编排。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};
use tokio::sync::broadcast;

use mcl_core::account::AccountStore;
use mcl_core::auth::microsoft::MicrosoftAuth;
use mcl_core::credential::{self, TokenKind};
use mcl_core::download::DownloadEngine;
use mcl_core::game::GameLayout;
use mcl_core::http::HttpClient;
use mcl_core::instance::{FileInstanceStore, InstanceStore};
use mcl_core::instance_settings::InstanceSettings;
use mcl_core::java;
use mcl_core::launch::{self, LaunchOptions};
use mcl_core::models::{refresh_needed, Account, AccountKind, LoaderSpec, TaskState};
use mcl_core::settings::LauncherSettings;
use mcl_core::task::{EventBus, TaskEvent};
use mcl_core::version::service::{InstallOptions, InstallReport, VersionService};
use mcl_core::CoreResult;

use crate::loader_kind::{LoaderKind, LoaderVersion};
use crate::{
    now_unix, pack_report_to_outcome, report_to_outcome, IconImage, InstallOutcome, TaskKind,
    TaskRegistry,
};

/// 供 tokio 任务使用的服务集合（`Arc` 共享，全部 `Send + Sync`）
/// 游戏目录列表（路径 + 实例数 + 是否默认目录）
pub fn game_directories() -> Vec<GameDirectoryInfo> {
    let settings = LauncherSettings::load();
    let instances = FileInstanceStore::open_default()
        .map(|store| store.list())
        .unwrap_or_default();
    settings
        .game_directories
        .iter()
        .map(|path| GameDirectoryInfo {
            label: std::path::Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone()),
            path: path.clone(),
            // 按前缀归属：实例目录 = 根目录/<实例名>
            instance_count: instances
                .iter()
                .filter(|instance| instance.dir.starts_with(path.as_str()))
                .count(),
            is_default: *path == settings.default_game_directory,
        })
        .collect()
}

/// 添加游戏目录（绝对路径、非重复；目录不存在则创建）
pub fn add_game_directory(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("目录路径不能为空".into());
    }
    if !std::path::Path::new(trimmed).is_absolute() {
        return Err("请填写绝对路径（例如 ~/games/minecraft 的展开形式）".into());
    }
    let mut settings = LauncherSettings::load();
    settings.normalize_game_directories();
    if settings
        .game_directories
        .iter()
        .any(|existing| existing == trimmed)
    {
        return Err("该目录已在列表中".into());
    }
    std::fs::create_dir_all(trimmed).map_err(|error| format!("目录创建失败：{error}"))?;
    settings.game_directories.push(trimmed.to_string());
    settings.normalize_game_directories();
    settings.save().map_err(|error| error.to_string())?;
    Ok(())
}

/// 移除游戏目录（至少保留一个；目录内还有实例时拒绝）
pub fn remove_game_directory(path: &str) -> Result<(), String> {
    let mut settings = LauncherSettings::load();
    settings.normalize_game_directories();
    if settings.game_directories.len() <= 1 {
        return Err("至少保留一个游戏目录".into());
    }
    if let Ok(store) = FileInstanceStore::open_default() {
        let inside = store
            .list()
            .iter()
            .filter(|instance| instance.dir.starts_with(path))
            .count();
        if inside > 0 {
            return Err(format!("该目录还有 {inside} 个实例，请先删除或移走它们"));
        }
    }
    if !settings
        .game_directories
        .iter()
        .any(|existing| existing == path)
    {
        return Err("目录不在列表中".into());
    }
    settings
        .game_directories
        .retain(|existing| existing != path);
    settings.normalize_game_directories();
    settings.save().map_err(|error| error.to_string())?;
    Ok(())
}

/// 设为默认游戏目录（新建实例的落点）
pub fn set_default_game_directory(path: &str) -> Result<(), String> {
    let mut settings = LauncherSettings::load();
    settings.normalize_game_directories();
    if !settings
        .game_directories
        .iter()
        .any(|existing| existing == path)
    {
        return Err("目录不在列表中".into());
    }
    settings.default_game_directory = path.trim().to_string();
    settings.normalize_game_directories();
    settings.save().map_err(|error| error.to_string())?;
    Ok(())
}

/// 游戏目录条目（实例库左列与目录管理页共用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameDirectoryInfo {
    /// 展示名（路径最后一段）
    pub label: String,
    /// 绝对路径
    pub path: String,
    /// 该目录下的实例数
    pub instance_count: usize,
    /// 是否默认目录（新建实例落点）
    pub is_default: bool,
}

pub struct Services {
    http: Arc<HttpClient>,
    pub version: Arc<VersionService>,
    pub bus: EventBus,
    pub tasks: TaskRegistry,
    /// 游戏异常退出通知（UI 订阅后展示横幅/诊断）
    crash_tx: broadcast::Sender<CrashNotice>,
    /// Fabric 官方支持的游戏版本（懒加载；`None` = 还没取到，不代表不支持）
    fabric_support: Mutex<Option<Arc<HashSet<String>>>>,
    /// 加载器版本清单缓存：`(kind_id, mc_version)` → 行。
    /// 安装页打开时预取一次，既用于「该加载器是否支持此版本」的判断，也让点选后立刻出列表。
    loader_cache: Mutex<HashMap<(String, String), Vec<LoaderVersion>>>,
}

/// 新建实例的规格：安装设置页给什么就传什么，`None` 表示沿用默认。
struct NewInstanceSpec<'a> {
    version: &'a str,
    /// 实例显示名（`None` = 「版本 + 加载器」默认名）
    name: Option<&'a str>,
    /// 内置图标 id（`None` / 空 = 跟随加载器）
    icon: Option<&'a str>,
    /// 父目录（`None` = 设置里的默认实例根）
    root: Option<&'a Path>,
    /// `Some((kind, loader_version))` 时同时安装加载器
    loader: Option<(&'a str, &'a str)>,
}

impl Services {
    pub fn new() -> CoreResult<Self> {
        let http = Arc::new(HttpClient::with_default_sources()?);
        let data_dir = mcl_core::platform::paths::data_dir();
        let version = Arc::new(VersionService::new(http.clone(), data_dir.join("meta")));
        let (crash_tx, _) = broadcast::channel(8);

        Ok(Self {
            http,
            version,
            bus: EventBus::default(),
            tasks: TaskRegistry::default(),
            crash_tx,
            fabric_support: Mutex::new(None),
            loader_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 订阅游戏异常退出事件（退出码非 0 或进程被终止）
    pub fn subscribe_crashes(&self) -> broadcast::Receiver<CrashNotice> {
        self.crash_tx.subscribe()
    }

    /// 预取 Fabric 支持的游戏版本清单（启动时调用；失败静默，后续按需重试）
    pub async fn prefetch_fabric_support(&self) {
        let _ = self.ensure_fabric_support().await;
    }

    /// 同步读取缓存结果：`None` = 清单尚未取到，不能据此判断“不支持”
    pub fn fabric_support_cached(&self, mc_version: &str) -> Option<bool> {
        self.fabric_support
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|set| set.contains(mc_version)))
    }

    /// 该版本是否有 Fabric 支持（必要时拉取清单）；清单拉取失败返回 `None`
    pub async fn fabric_supported(&self, mc_version: &str) -> Option<bool> {
        self.ensure_fabric_support()
            .await
            .map(|set| set.contains(mc_version))
    }

    async fn ensure_fabric_support(&self) -> Option<Arc<HashSet<String>>> {
        if let Some(set) = self
            .fabric_support
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            return Some(set);
        }

        let versions = mcl_core::loader::fabric::game_versions(&self.http)
            .await
            .ok()?;
        let set = Arc::new(versions.into_iter().collect::<HashSet<_>>());
        if let Ok(mut guard) = self.fabric_support.lock() {
            *guard = Some(set.clone());
        }
        Some(set)
    }

    /// 指定 MC 版本可用的加载器版本（新→旧，带角标）
    ///
    /// Fabric 先查官方支持清单：不支持的版本直接给出明确原因，避免把
    /// Fabric Meta 的 HTTP 400 当成“网络连不上”。
    /// 带缓存的加载器版本清单：安装页预取与点选共用，避免重复请求。
    pub async fn loader_versions_cached(
        &self,
        kind: LoaderKind,
        mc_version: &str,
    ) -> Result<Vec<LoaderVersion>, String> {
        let key = (kind.id().to_string(), mc_version.to_string());
        if let Some(rows) = self
            .loader_cache
            .lock()
            .ok()
            .and_then(|guard| guard.get(&key).cloned())
        {
            return Ok(rows);
        }
        let rows = self.loader_versions(kind, mc_version).await?;
        if let Ok(mut guard) = self.loader_cache.lock() {
            guard.insert(key, rows.clone());
        }
        Ok(rows)
    }

    pub async fn loader_versions(
        &self,
        kind: LoaderKind,
        mc_version: &str,
    ) -> Result<Vec<LoaderVersion>, String> {
        if kind == LoaderKind::Fabric && self.fabric_supported(mc_version).await == Some(false) {
            return Err(format!(
                "Fabric 官方未提供 {mc_version} 的支持，该版本只能安装原版"
            ));
        }
        match kind {
            LoaderKind::Fabric => {
                let rows = mcl_core::loader::fabric::loader_versions(&self.http, mc_version)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(rows
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| LoaderVersion {
                        version: item.version,
                        tag: if index == 0 {
                            "最新".to_string()
                        } else if item.stable {
                            "稳定".to_string()
                        } else {
                            "测试".to_string()
                        },
                    })
                    .collect())
            }
            LoaderKind::Quilt => {
                let rows = mcl_core::loader::quilt::loader_versions(&self.http, mc_version)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(rows
                    .into_iter()
                    .enumerate()
                    .map(|(index, version)| LoaderVersion {
                        version,
                        tag: if index == 0 {
                            "最新".into()
                        } else {
                            String::new()
                        },
                    })
                    .collect())
            }
            LoaderKind::Forge | LoaderKind::NeoForge => {
                let flavor = kind.flavor().expect("flavor");
                let rows = mcl_core::loader::forge::loader_versions(&self.http, flavor, mc_version)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(rows
                    .into_iter()
                    .enumerate()
                    .map(|(index, version)| LoaderVersion {
                        version,
                        tag: if index == 0 {
                            "最新".into()
                        } else {
                            String::new()
                        },
                    })
                    .collect())
            }
        }
    }

    /// 执行（或续传/重试）一个任务：按类型分派到对应流程
    pub async fn run_install(
        &self,
        kind: &TaskKind,
        task_id: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<InstallOutcome, String> {
        let options = InstallOptions {
            cancel: Some(cancel.clone()),
            ..Default::default()
        };
        match kind {
            TaskKind::InstallVersion {
                version,
                name,
                icon,
                root,
                loader,
            } => {
                self.install_version_task(
                    NewInstanceSpec {
                        version,
                        name: name.as_deref(),
                        icon: icon.as_deref(),
                        root: root.as_deref(),
                        loader: loader
                            .as_ref()
                            .map(|(kind, ver)| (kind.as_str(), ver.as_str())),
                    },
                    task_id,
                    &options,
                )
                .await
            }
            TaskKind::InstallLoader {
                instance_id,
                kind,
                loader_version,
                ..
            } => {
                self.install_loader_task(instance_id, kind, loader_version, task_id, &options)
                    .await
            }
            TaskKind::Launch { instance_id, .. } => {
                self.launch_task(instance_id, task_id, cancel).await
            }
            TaskKind::Repair { instance_id, .. } => {
                self.repair_task(instance_id, task_id, &options).await
            }
            TaskKind::InstallPack(pack_id) => {
                self.install_pack_task(pack_id, task_id, &options).await
            }
            TaskKind::ImportPack(path) => self.import_pack_task(path, task_id, &options).await,
            TaskKind::InstallJava { major } => {
                self.install_java_task(*major, task_id, &options).await
            }
        }
    }

    /// 下载并安装 Java 运行时（首次启动的「立即下载」）。
    ///
    /// 复用 `java::ensure_java`：已有满足版本的运行时就跳过下载。
    /// 返回的 `label` 为空 —— 这一步不创建实例，只补运行环境。
    async fn install_java_task(
        &self,
        major: u32,
        task_id: u64,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, String> {
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);
        let runtime = mcl_core::java::ensure_java(
            &self.http,
            &engine,
            major,
            task_id,
            options.cancel.clone(),
        )
        .await
        .map_err(|error| error.to_string())?;

        Ok(InstallOutcome::Done(match runtime {
            Some(runtime) => format!("Java {} 已就绪 · {}", runtime.major, runtime.path.display()),
            None => format!("Java {major} 已就绪"),
        }))
    }

    /// 安装内置精选整合包：清单下载 → 加载器 → 模组/资源 → overrides
    async fn install_pack_task(
        &self,
        pack_id: &str,
        task_id: u64,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, String> {
        let pack = mcl_core::pack::curated_pack(pack_id)
            .ok_or_else(|| format!("未知的精选整合包：{pack_id}"))?;
        let mut store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);

        let report = mcl_core::pack::install_curated_pack(
            &self.version,
            &mut store,
            &engine,
            pack,
            options,
            task_id,
        )
        .await
        .map_err(|error| error.to_string())?;

        Ok(pack_report_to_outcome(&report))
    }

    /// 导入本地 `.mrpack`
    async fn import_pack_task(
        &self,
        path: &str,
        task_id: u64,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, String> {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Ok(InstallOutcome::Failed(format!(
                "整合包文件不存在：{}",
                path.display()
            )));
        }

        let mut store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);

        let report = mcl_core::pack::install_mrpack(
            &self.version,
            &mut store,
            &engine,
            &path,
            options,
            task_id,
        )
        .await
        .map_err(|error| error.to_string())?;

        Ok(pack_report_to_outcome(&report))
    }

    /// 安装原版实例
    /// 新建实例并安装（原版，或原版 + 加载器）。
    ///
    /// 实例名 / 图标 / 父目录由安装设置页给定；`None` 时沿用默认（版本+加载器名、跟随加载器图标、默认实例根）。
    async fn install_version_task(
        &self,
        spec: NewInstanceSpec<'_>,
        task_id: u64,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, String> {
        let NewInstanceSpec {
            version,
            name,
            icon,
            root,
            loader,
        } = spec;
        let root = root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| LauncherSettings::load().default_instances_root());

        // 加载器：`None` = 只装原版
        let resolved = match loader {
            Some((kind_id, requested)) => {
                let kind = LoaderKind::from_id(kind_id)
                    .ok_or_else(|| format!("未知的加载器类型：{kind_id}"))?;
                let version = self
                    .resolve_loader_version(kind, version, requested)
                    .await?;
                Some((kind, version))
            }
            None => None,
        };
        let spec = match &resolved {
            Some((kind, loader_version)) => kind.spec(loader_version),
            None => LoaderSpec::Vanilla,
        };

        let mut store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;

        // 复用"同版本 + 同加载器已登记但未装成"的实例（失败重试/暂停续传不在列表里堆叠空实例）；
        // 安装完成标志：版本 JSON 落地（安装流水线的最后一步，见 install_merged）
        let reusable = store.list().into_iter().find(|instance| {
            instance.mc_version == version
                && instance.loader() == &spec
                && !GameLayout::new(&instance.dir)
                    .version_json(&instance.version_id())
                    .is_file()
        });

        let instance = match reusable {
            Some(instance) => instance,
            None => store
                .create_for_in(
                    &root,
                    version,
                    spec,
                    name.map(std::string::ToString::to_string),
                )
                .map_err(|error| error.to_string())?,
        };

        // 图标：`file:<路径>` 是安装页选的自定义图片（复制成 icon.png），否则是内置图标 id
        if let Some(icon) = icon.filter(|value| !value.is_empty()) {
            if let Some(path) = icon.strip_prefix("file:") {
                crate::set_instance_icon(&instance.dir, Path::new(path))?;
            } else {
                let mut settings =
                    mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
                settings.icon = icon.to_string();
                settings
                    .save(&instance.dir)
                    .map_err(|error| error.to_string())?;
            }
        }

        // 共享模式：assets/libraries 落全局共享目录
        let layout = GameLayout::for_instance(&instance);
        // 并发数每次任务读取一次设置（设置页改动无需重启，下个任务即生效）
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);

        let (label, report) = match resolved {
            Some((kind, loader_version)) => {
                let (label, report) = self
                    .install_into_layout(
                        kind,
                        version,
                        &loader_version,
                        &instance.version_id(),
                        &layout,
                        &engine,
                        options,
                        task_id,
                    )
                    .await?;
                (Some(label), report)
            }
            None => (
                None,
                self.version
                    .install(version, &layout, &engine, options, task_id)
                    .await
                    .map_err(|error| error.to_string())?,
            ),
        };

        Ok(report_to_outcome(&report, &instance.name, label.as_deref()))
    }

    /// 修复实例：以实例已落地的合并 JSON 为基准全量校验，缺什么补什么
    ///
    /// 与「安装」共用流水线（命中跳过按 SHA-1 判定），因此修复只下载差异文件；
    /// 不发清单请求——本地 JSON 即标准答案，清单源不可达时同样可用。
    async fn repair_task(
        &self,
        instance_id: &str,
        task_id: u64,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, String> {
        let store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;

        let layout = GameLayout::for_instance(&instance);
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);

        // 客户端 jar 归属：原版 = 版本号自身；加载器实例 = 原版版本号（复用 jar，不复制副本）
        let report = self
            .version
            .repair(
                &instance.version_id(),
                &instance.mc_version,
                &layout,
                &engine,
                options,
                task_id,
            )
            .await
            .map_err(|error| error.to_string())?;

        if report.cancelled {
            return Ok(InstallOutcome::Paused);
        }
        if !report.failures.is_empty() {
            return Ok(InstallOutcome::Failed(format!(
                "「{}」修复未完成：{} 个文件失败",
                instance.name,
                report.failures.len()
            )));
        }
        let re_downloaded = report.files.saturating_sub(report.skipped);
        Ok(InstallOutcome::Done(format!(
            "「{}」修复完成：校验 {} 个文件，重下 {} 个（{:.1} MB）",
            instance.name,
            report.files,
            re_downloaded,
            report.bytes as f64 / 1_048_576.0
        )))
    }

    /// 给已有实例安装/更换加载器（组件安装，不创建新实例）
    async fn install_loader_task(
        &self,
        instance_id: &str,
        kind: &str,
        loader_version: &str,
        task_id: u64,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, String> {
        let kind = LoaderKind::from_id(kind).ok_or_else(|| format!("未知的加载器类型：{kind}"))?;

        let mut store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;
        let mc_version = instance.mc_version.clone();

        let loader_version = self
            .resolve_loader_version(kind, &mc_version, loader_version)
            .await?;

        // 与创建实例时相同的安装流水线；只是布局指向已有实例目录
        let layout = GameLayout::for_instance(&instance);
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);

        let (label, report) = self
            .install_into_layout(
                kind,
                &mc_version,
                &loader_version,
                &instance.version_id(),
                &layout,
                &engine,
                options,
                task_id,
            )
            .await?;

        // 只有安装成功后才切换实例的 version_id，失败时保持原状
        store
            .set_loader(instance_id, kind.spec(&loader_version))
            .map_err(|error| error.to_string())?;

        Ok(report_to_outcome(&report, &instance.name, Some(&label)))
    }

    /// 按加载器种类分派到对应流水线，返回（展示用加载器版本，安装报告）
    #[allow(clippy::too_many_arguments)] // 与 core 的安装入口保持一致
    async fn install_into_layout(
        &self,
        kind: LoaderKind,
        mc_version: &str,
        loader_version: &str,
        version_id: &str,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
    ) -> Result<(String, InstallReport), String> {
        if let Some(meta) = kind.meta() {
            let (loader, report) = self
                .version
                .install_meta(
                    meta,
                    mc_version,
                    Some(loader_version),
                    Some(version_id),
                    layout,
                    engine,
                    options,
                    task_id,
                )
                .await
                .map_err(|error| error.to_string())?;
            return Ok((loader, report));
        }

        let flavor = kind.flavor().expect("installer 加载器必须有 flavor");
        let java = self.installer_java(mc_version, task_id).await?;
        let (_, report) = self
            .version
            .install_forge(
                flavor,
                mc_version,
                loader_version,
                Some(version_id),
                layout,
                engine,
                options,
                task_id,
                &java.path,
                Some((&self.bus, task_id)),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok((loader_version.to_string(), report))
    }

    /// 空版本 = 取最新；非空原样返回
    async fn resolve_loader_version(
        &self,
        kind: LoaderKind,
        mc_version: &str,
        requested: &str,
    ) -> Result<String, String> {
        if !requested.trim().is_empty() {
            return Ok(requested.trim().to_string());
        }
        self.loader_versions(kind, mc_version)
            .await?
            .into_iter()
            .next()
            .map(|row| row.version)
            .ok_or_else(|| format!("{} 没有可用于 {mc_version} 的版本", kind.label()))
    }

    /// 官方 installer 需要 Java：优先复用已有运行时，缺失时自动下载（与启动一致）
    async fn installer_java(
        &self,
        mc_version: &str,
        task_id: u64,
    ) -> Result<java::JavaRuntime, String> {
        let manifest = self
            .version
            .manifest()
            .await
            .map_err(|error| error.to_string())?;
        let merged = self
            .version
            .resolve(mc_version, &manifest)
            .await
            .map_err(|error| error.to_string())?;
        let required = merged
            .pointer("/javaVersion/majorVersion")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(8) as u32;

        if let Ok(runtime) = java::resolve_java(required) {
            return Ok(runtime);
        }
        self.bus.publish(TaskEvent::StateChanged {
            id: task_id,
            state: TaskState::Running,
            message: format!("未找到 Java {required}+，正在自动下载运行时…"),
        });
        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);
        match java::ensure_java(&self.http, &engine, required, task_id, None)
            .await
            .map_err(|error| error.to_string())?
        {
            Some(runtime) => Ok(runtime),
            None => Err("Java 运行时下载未完成，加载器安装已取消".to_string()),
        }
    }

    /// 已装资源索引：`<实例>/.mcl/installed.json` 里的 project_id 列表
    pub fn resource_installed(&self, instance_id: &str) -> Vec<String> {
        let Some(instance) = FileInstanceStore::open_default()
            .ok()
            .and_then(|store| store.get(instance_id))
        else {
            return Vec::new();
        };
        let path = instance.dir.join(".mcl").join("installed.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Vec::new();
        };
        value
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// 记录一条已装资源（project_id → { file, version }）
    fn record_installed(
        instance_dir: &std::path::Path,
        project_id: &str,
        file: &str,
        version_id: &str,
        name: &str,
        dir: &std::path::Path,
    ) {
        let index_dir = instance_dir.join(".mcl");
        let _ = std::fs::create_dir_all(&index_dir);
        let path = index_dir.join("installed.json");
        let mut map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| {
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok()
            })
            .unwrap_or_default();
        map.insert(
            project_id.to_string(),
            serde_json::json!({
                "file": file,
                "version": version_id,
                "name": name,
                // 安装目录（只有实例内的安装才会写这条记录）
                "dir": dir.display().to_string(),
            }),
        );
        if let Ok(text) = serde_json::to_string_pretty(&map) {
            let _ = std::fs::write(path, text);
        }
    }

    /// 检查已装资源的更新：返回需要升级的项目（project_id、最新版本标签）
    pub async fn resource_updates(
        &self,
        instance_id: &str,
    ) -> Result<Vec<(String, String, String)>, String> {
        let store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;
        let index_path = instance.dir.join(".mcl").join("installed.json");
        let Ok(text) = std::fs::read_to_string(&index_path) else {
            return Ok(Vec::new());
        };
        let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
        else {
            return Ok(Vec::new());
        };
        let loader = match instance.loader() {
            LoaderSpec::Fabric(_) => Some("fabric".to_string()),
            LoaderSpec::Quilt(_) => Some("quilt".to_string()),
            LoaderSpec::Forge(_) => Some("forge".to_string()),
            LoaderSpec::NeoForge(_) => Some("neoforge".to_string()),
            LoaderSpec::Vanilla => None,
        };

        let mut updates: Vec<(String, String, String)> = Vec::new();
        for (project_id, value) in map {
            // 新格式是数组（同一项目多个目录），旧格式是单个对象
            let entries: Vec<&serde_json::Value> = match &value {
                serde_json::Value::Array(rows) => rows.iter().collect(),
                other => vec![other],
            };
            let installed = entries
                .iter()
                .find_map(|entry| entry.get("version").and_then(|version| version.as_str()))
                .unwrap_or_default()
                .to_string();
            let name = entries
                .iter()
                .find_map(|entry| entry.get("name").and_then(|name| name.as_str()))
                .unwrap_or(&project_id)
                .to_string();
            let versions = mcl_core::resource::versions(
                &self.http,
                &project_id,
                Some(&instance.mc_version),
                loader.as_deref(),
            )
            .await
            .unwrap_or_default();
            let Some(latest) = versions.first() else {
                continue;
            };
            // 旧索引没有版本号 → 只报告可更新，不判断新旧
            if installed.is_empty() || latest.id != installed {
                updates.push((project_id, name, latest.version_number.clone()));
            }
        }
        Ok(updates)
    }

    /// 项目详情（简介、关注数、分类等）
    pub async fn resource_detail(
        &self,
        source: mcl_core::resource::ResourceSource,
        project_id: &str,
    ) -> Result<mcl_core::resource::ResourceDetail, String> {
        mcl_core::resource::project_from(&self.http, source, project_id)
            .await
            .map_err(|error| error.to_string())
    }

    /// 某项目的可用版本（按实例的 MC 版本过滤，加载器可选）
    pub async fn resource_versions(
        &self,
        source: mcl_core::resource::ResourceSource,
        project_id: &str,
        mc_version: &str,
        loader: Option<&str>,
    ) -> Result<Vec<mcl_core::resource::ResourceVersion>, String> {
        // 同一项目 + 同一筛选条件只取一次；命中磁盘缓存直接返回（TTL 24h）
        let key = format!(
            "versions:{}:{}:{}:{}",
            source.id(),
            project_id,
            mc_version.trim(),
            loader.unwrap_or("")
        );
        if let Some(rows) = crate::resource_cache::load(&key) {
            return Ok(rows);
        }
        let rows = mcl_core::resource::versions_from(
            &self.http,
            source,
            project_id,
            (!mc_version.trim().is_empty()).then_some(mc_version.trim()),
            loader,
        )
        .await
        .map_err(|error| error.to_string())?;
        crate::resource_cache::store(&key, &rows);
        Ok(rows)
    }

    /// 安装指定版本（详情页里选某一版）
    #[allow(clippy::too_many_arguments)]
    pub async fn resource_install_version(
        &self,
        source: mcl_core::resource::ResourceSource,
        kind: &str,
        project_id: &str,
        project_name: &str,
        version_id: &str,
        instance_id: &str,
        target_dir: Option<&std::path::Path>,
    ) -> Result<String, String> {
        use mcl_core::resource::ResourceKind;

        let category = match kind {
            "resourcepack" => ResourceKind::ResourcePack,
            "shader" => ResourceKind::Shader,
            "modpack" => ResourceKind::Modpack,
            _ => ResourceKind::Mod,
        };
        let store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;

        let versions =
            mcl_core::resource::versions_from(&self.http, source, project_id, None, None)
                .await
                .map_err(|error| error.to_string())?;
        let version = versions
            .iter()
            .find(|item| item.id == version_id)
            .ok_or_else(|| "该版本已不可用".to_string())?;
        let file = version
            .files
            .first()
            .ok_or_else(|| "该版本没有可下载文件".to_string())?;

        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);
        let default_dir = instance.dir.join(category.target_dir().unwrap_or_default());
        // 只有装进实例的默认资源目录才算「已安装」。装到别处只是保存一份文件：
        // 模组加载器只读 mods/（资源包/光影同理），不会加载实例之外的内容。
        let installs_into_instance = target_dir.map(|dir| dir == default_dir).unwrap_or(true);
        let path = match target_dir {
            Some(dir) => mcl_core::resource::install_file_to(&engine, dir, file).await,
            None => mcl_core::resource::install_file(&engine, category, &instance.dir, file).await,
        }
        .map_err(|error| error.to_string())?;
        if installs_into_instance {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                Self::record_installed(
                    &instance.dir,
                    project_id,
                    name,
                    version_id,
                    project_name,
                    &default_dir,
                );
            }
        }
        Ok(path.display().to_string())
    }

    /// 资源站检索：`kind` = mod / resourcepack / shader / modpack。
    /// 返回条目与站点总数（页码分页用）；`env` 只对 Modrinth 生效。
    #[allow(clippy::too_many_arguments)]
    pub async fn resource_search_page(
        &self,
        source: mcl_core::resource::ResourceSource,
        kind: &str,
        text: &str,
        offset: usize,
        sort: &str,
        mc_version: &str,
        loader: &str,
        env: &str,
    ) -> Result<mcl_core::resource::SearchPage, String> {
        use mcl_core::resource::{ResourceKind, ResourceQuery};

        let category = match kind {
            "resourcepack" => ResourceKind::ResourcePack,
            "shader" => ResourceKind::Shader,
            "modpack" => ResourceKind::Modpack,
            _ => ResourceKind::Mod,
        };
        mcl_core::resource::search_from_page(
            &self.http,
            source,
            category,
            &ResourceQuery {
                text: text.to_string(),
                offset,
                sort: sort.to_string(),
                mc_version: (!mc_version.trim().is_empty()).then(|| mc_version.trim().to_string()),
                loader: (!loader.trim().is_empty()).then(|| loader.trim().to_string()),
                env: (!env.trim().is_empty()).then(|| env.trim().to_string()),
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// 拉取资源封面（Modrinth CDN 图标）：命中本地缓存则不联网。
    /// 失败返回 `None`，UI 回落到类目语义图标。
    pub async fn resource_icon(&self, url: &str) -> Option<IconImage> {
        crate::resource_icon::fetch(&self.http, url).await
    }

    /// 简介配图：同一套缓存，按 1024 解码（详情页正文用）
    pub async fn resource_image(&self, url: &str) -> Option<IconImage> {
        crate::resource_icon::fetch_sized(&self.http, url, 1024).await
    }

    /// 账号头像：按 UUID 拉取皮肤并渲染成 64×64 头部（离线/失败返回 `None`）。
    pub async fn account_avatar(&self, uuid: &str) -> Option<IconImage> {
        crate::avatar::fetch(&self.http, uuid).await
    }

    /// 把资源装进实例：按实例的 MC 版本与加载器取最新匹配版本
    pub async fn resource_install(
        &self,
        source: mcl_core::resource::ResourceSource,
        kind: &str,
        project_id: &str,
        project_name: &str,
        instance_id: &str,
        target_dir: Option<&std::path::Path>,
    ) -> Result<String, String> {
        use mcl_core::resource::ResourceKind;

        let category = match kind {
            "resourcepack" => ResourceKind::ResourcePack,
            "shader" => ResourceKind::Shader,
            "modpack" => ResourceKind::Modpack,
            _ => ResourceKind::Mod,
        };

        // 整合包：下载 .mrpack 后交给既有导入流程（它会创建新实例）
        if category == ResourceKind::Modpack {
            let versions =
                mcl_core::resource::versions_from(&self.http, source, project_id, None, None)
                    .await
                    .map_err(|error| error.to_string())?;
            let file = versions
                .first()
                .and_then(|version| version.files.first())
                .ok_or_else(|| "该整合包没有可下载文件".to_string())?;
            let dir = mcl_core::platform::paths::data_dir()
                .join("cache")
                .join("packs");
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let concurrency = LauncherSettings::load().download_concurrency;
            let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);
            let dest = dir.join(
                mcl_core::resource::sanitize_filename(&file.filename).map_err(|e| e.to_string())?,
            );
            engine
                .fetch(&file.entry(), &dest)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(dest.display().to_string());
        }

        let store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;

        let loader = match instance.loader() {
            LoaderSpec::Fabric(_) => Some("fabric".to_string()),
            LoaderSpec::Quilt(_) => Some("quilt".to_string()),
            LoaderSpec::Forge(_) => Some("forge".to_string()),
            LoaderSpec::NeoForge(_) => Some("neoforge".to_string()),
            LoaderSpec::Vanilla => None,
        };

        let versions = mcl_core::resource::versions_from(
            &self.http,
            source,
            project_id,
            Some(&instance.mc_version),
            loader.as_deref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let version = versions
            .first()
            .ok_or_else(|| format!("没有匹配 {} 的版本", instance.mc_version))?;
        let file = version
            .files
            .first()
            .ok_or_else(|| "该版本没有可下载文件".to_string())?;

        let concurrency = LauncherSettings::load().download_concurrency;
        let engine = DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);
        let default_dir = instance.dir.join(category.target_dir().unwrap_or_default());
        let installs_into_instance = target_dir.map(|dir| dir == default_dir).unwrap_or(true);
        let path = match target_dir {
            Some(dir) => mcl_core::resource::install_file_to(&engine, dir, file).await,
            None => mcl_core::resource::install_file(&engine, category, &instance.dir, file).await,
        }
        .map_err(|error| error.to_string())?;
        if installs_into_instance {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                Self::record_installed(
                    &instance.dir,
                    project_id,
                    name,
                    "",
                    project_name,
                    &default_dir,
                );
            }
        }
        Ok(path.display().to_string())
    }

    /// 取当前账号与其令牌：离线账号用占位符，正版账号从系统钥匙串读取。
    /// 令牌临近过期时先用 refresh token 静默续期。
    async fn current_account(&self) -> Result<(Account, String), String> {
        let store = AccountStore::open_default().map_err(|error| error.to_string())?;
        let account = store.current().cloned().ok_or(
            "尚未添加账号。M1.5 将提供账号管理界面；当前可先用命令行：\
             mcl-cli account add-offline <用户名> 或 mcl-cli login",
        )?;

        if account.kind == AccountKind::Microsoft && refresh_needed(account.expires_at, now_unix())
        {
            return self.refresh_account(&account).await;
        }

        let token = match account.kind {
            AccountKind::Offline => "0".to_string(),
            _ => credential::load_token(&account.id, TokenKind::MinecraftAccess)
                .map_err(|error| error.to_string())?
                .ok_or("该账号令牌不在系统钥匙串中，请重新登录")?,
        };

        Ok((account, token))
    }

    /// 用 refresh token 重走认证链，成功后回写新令牌与过期时间。
    async fn refresh_account(&self, account: &Account) -> Result<(Account, String), String> {
        let client_id = MicrosoftAuth::resolve_client_id()
            .ok_or("登录已过期，且构建未注入 client_id，请重新登录")?;
        let refresh_token = credential::load_token(&account.id, TokenKind::MsaRefresh)
            .map_err(|error| error.to_string())?
            .ok_or("登录已过期，请重新登录")?;

        let auth = MicrosoftAuth::new(client_id).map_err(|error| error.to_string())?;
        let session = auth
            .refresh_session(&refresh_token)
            .await
            .map_err(|error| format!("自动续期失败，请重新登录：{error}"))?;

        let _ = credential::save_token(
            &account.id,
            TokenKind::MinecraftAccess,
            &session.access_token,
        );
        if let Some(new_refresh) = &session.refresh_token {
            let _ = credential::save_token(&account.id, TokenKind::MsaRefresh, new_refresh);
        }

        let mut updated = session.account.clone();
        updated.expires_at = Some(session.expires_at);
        if let Ok(mut store) = AccountStore::open_default() {
            let _ = store.upsert(updated.clone());
        }
        Ok((updated, session.access_token))
    }

    /// 启动指定实例内的游戏
    async fn launch_task(
        &self,
        instance_id: &str,
        task_id: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<InstallOutcome, String> {
        let store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;
        let version_id = instance.version_id();

        // 共享模式：assets/libraries 落全局共享目录
        let layout = GameLayout::for_instance(&instance);
        let launcher_settings = LauncherSettings::load();
        let settings = InstanceSettings::load_with_defaults(
            &instance.dir,
            &launcher_settings.instance_defaults,
        );

        // 优先读取安装时落地的合并 JSON；非原版实例缺文件时提示先安装
        let merged: serde_json::Value = if layout.version_json(&version_id).is_file() {
            let bytes = tokio::fs::read(layout.version_json(&version_id))
                .await
                .map_err(|error| error.to_string())?;
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?
        } else {
            if !matches!(instance.loader(), LoaderSpec::Vanilla) {
                return Err(format!("「{}」尚未安装完成，请先安装", instance.name));
            }
            let manifest = self.version.manifest().await.map_err(|e| e.to_string())?;
            self.version
                .resolve(&instance.mc_version, &manifest)
                .await
                .map_err(|error| error.to_string())?
        };

        // 版本 JSON 有 javaVersion 就用它；缺字段（老版本 / 部分整合包）
        // 时按 MC 版本推断，避免一律退回 8 导致 1.18+ 启动失败
        let required_major = merged
            .pointer("/javaVersion/majorVersion")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| u64::from(java::required_major_for_mc(&instance.mc_version)))
            as u32;
        // Java 运行时：实例指定优先；否则探测 → 缺失自动下载
        let runtime = if !settings.java_path.is_empty() {
            java::probe_java(std::path::Path::new(&settings.java_path)).map_err(|error| {
                format!("实例指定的 Java 不可用（{}）：{error}", settings.java_path)
            })?
        } else {
            match java::resolve_java(required_major) {
                Ok(runtime) => runtime,
                Err(_) => {
                    self.bus.publish(TaskEvent::StateChanged {
                        id: task_id,
                        state: TaskState::Running,
                        message: format!("未找到 Java {required_major}+，正在自动下载运行时…"),
                    });
                    let concurrency = LauncherSettings::load().download_concurrency;
                    let engine =
                        DownloadEngine::new(self.http.clone(), self.bus.clone(), concurrency);
                    match java::ensure_java(
                        &self.http,
                        &engine,
                        required_major,
                        task_id,
                        Some(cancel),
                    )
                    .await
                    .map_err(|error| error.to_string())?
                    {
                        Some(runtime) => runtime,
                        None => return Ok(InstallOutcome::Paused),
                    }
                }
            }
        };

        let (account, access_token) = self.current_account().await?;

        let mut options = LaunchOptions::offline(
            runtime.path.clone(),
            version_id.clone(),
            &account.name,
            settings.memory_mb,
        );
        options.width = settings.width;
        options.height = settings.height;
        options.extra_jvm_args = settings
            .jvm_args
            .split_whitespace()
            .map(str::to_string)
            .collect();
        options.extra_game_args = settings
            .game_args
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if settings.fullscreen {
            options.extra_game_args.push("--fullscreen".to_string());
        }
        options.account = account.clone();
        options.access_token = access_token;
        // 加载器实例的版本目录只有 JSON：客户端 jar 复用原版目录（不复制 25MB 副本）
        options.client_jar = if matches!(instance.loader(), LoaderSpec::Vanilla) {
            None
        } else {
            Some(layout.version_jar(&instance.mc_version))
        };

        let command =
            launch::build_command(&merged, &layout, &options).map_err(|error| error.to_string())?;

        let log_path = layout.root().join("logs").join(format!("{version_id}.log"));
        let mut game = launch::launch_game(&command, &log_path, false)
            .await
            .map_err(|error| error.to_string())?;
        let pid = game.pid();

        // 等待进程退出：非 0 视为异常，读日志尾部做诊断并广播（HMCL 的 onExit +
        // GameCrashWindow 思路；区别是我们只广播诊断，具体展示交给 UI 层）
        let crash_tx = self.crash_tx.clone();
        let instance_name = instance.name.clone();
        let crash_instance_id = instance_id.to_string();
        let log_path = log_path.clone();
        tokio::spawn(async move {
            let code = game.wait().await.ok().flatten();
            if code == Some(0) {
                return;
            }
            let tail = std::fs::read(&log_path)
                .map(|bytes| {
                    let start = bytes.len().saturating_sub(64 * 1024);
                    String::from_utf8_lossy(&bytes[start..]).to_string()
                })
                .unwrap_or_default();
            let (title, detail) = mcl_core::crash::analyze(&tail)
                .map(|hint| (hint.title, hint.detail))
                .unwrap_or_else(|| {
                    (
                        "游戏异常退出".to_string(),
                        format!(
                            "退出码：{}。可在管理页查看日志定位原因。",
                            code.map(|value| value.to_string())
                                .unwrap_or_else(|| "被终止".to_string())
                        ),
                    )
                });
            let _ = crash_tx.send(CrashNotice {
                instance_id: crash_instance_id,
                instance_name,
                title,
                detail,
                exit_code: code,
                log_path,
            });
        });

        // 记录最近游玩（失败仅忽略，不影响启动结果；列表据此把最近玩的排到最前）
        if let Ok(mut store) = FileInstanceStore::open_default() {
            let _ = store.touch_last_played(instance_id, now_unix());
        }

        Ok(InstallOutcome::Done(format!(
            "「{}」已启动（账号 {} · Java {} · PID {:?}）",
            instance.name, account.name, runtime.major, pid
        )))
    }

    /// 用 Modrinth `version_files/update` 按 SHA-1 检查模组更新。
    ///
    /// 返回的每行含最新版本号与下载页；不直接替换文件（下载/校验交给用户手动确认）。
    pub async fn check_mod_updates(&self, instance_id: &str) -> Result<Vec<ModUpdate>, String> {
        let store = FileInstanceStore::open_default().map_err(|error| error.to_string())?;
        let instance = store
            .get(instance_id)
            .ok_or_else(|| format!("实例不存在：{instance_id}"))?;
        let mods_dir = instance.dir.join("mods");
        let Ok(entries) = std::fs::read_dir(&mods_dir) else {
            return Ok(Vec::new());
        };

        let mut hashes = Vec::new();
        let mut files: HashMap<String, String> = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !(name.ends_with(".jar") || name.ends_with(".jar.disabled")) {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
            let mut hasher = Sha1::new();
            hasher.update(&bytes);
            let hash = format!("{:x}", hasher.finalize());
            files.insert(hash.clone(), name.trim_end_matches(".disabled").to_string());
            hashes.push(hash);
        }
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({ "hashes": hashes, "algorithm": "sha1" });
        let response: HashMap<String, serde_json::Value> = self
            .http
            .post_json("https://api.modrinth.com/v2/version_files/update", &body)
            .await
            .map_err(|error| error.to_string())?;

        let mut updates = Vec::new();
        for (hash, value) in response {
            let Some(file) = files.get(&hash) else {
                continue;
            };
            let latest = value
                .get("version_number")
                .and_then(|item| item.as_str())
                .unwrap_or_default();
            let url = value
                .pointer("/files/0/url")
                .and_then(|item| item.as_str())
                .unwrap_or_default();
            let latest_hash = value
                .pointer("/files/0/hashes/sha1")
                .and_then(|item| item.as_str());
            if latest.is_empty() || url.is_empty() || latest_hash == Some(hash.as_str()) {
                continue;
            }
            updates.push(ModUpdate {
                file: file.clone(),
                latest: latest.to_string(),
                url: url.to_string(),
            });
        }
        updates.sort_by(|left, right| left.file.cmp(&right.file));
        Ok(updates)
    }
}

/// 游戏异常退出通知
#[derive(Debug, Clone)]
pub struct CrashNotice {
    pub instance_id: String,
    pub instance_name: String,
    pub title: String,
    pub detail: String,
    /// 进程退出码（`None` = 被信号终止）
    pub exit_code: Option<i32>,
    pub log_path: PathBuf,
}

/// 模组更新条目（文件 → 最新版本与下载页）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModUpdate {
    pub file: String,
    pub latest: String,
    pub url: String,
}
