//! 版本服务
//!
//! 流水线：清单拉取（带离线缓存兜底）→ 继承链解析合并 → 安装计划生成
//!（OS 规则过滤 / natives 分类器 / 资源索引展开）→ 交给下载引擎执行。

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::download::DownloadEngine;
use crate::error::{CoreError, CoreResult};
use crate::game::GameLayout;
use crate::http::HttpClient;
use crate::models::{FileEntry, VersionSummary};
use crate::version::manifest::merge_inherited;

/// meta 系加载器：档案 JSON 同构，安装流程一致
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaLoader {
    Fabric,
    Quilt,
}

impl MetaLoader {
    pub fn label(self) -> &'static str {
        match self {
            MetaLoader::Fabric => "Fabric",
            MetaLoader::Quilt => "Quilt",
        }
    }
}

/// 官方版本清单
pub const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
/// 资源对象基址（BMCLAPI 会映射为 `/assets`）
pub const RESOURCES_BASE: &str = "https://resources.download.minecraft.net";

#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// 是否下载资源对象（1.20.1 约 620MB）；关闭时仅装 jar/类库/natives/索引，
    /// 用于快速验证启动链路
    pub include_assets: bool,
    /// 协作式取消旗标：`Some` 且置位时停止派发新文件，
    /// 已下载部分保留（哈希跳过 / `.part` 续传），再次调用同一安装即续传
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            include_assets: true,
            cancel: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct InstallReport {
    pub version_id: String,
    pub files: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub natives_extracted: usize,
    pub failures: Vec<(String, String)>,
    /// 是否被取消（暂停）：已下载部分保留，再次调用即续传
    pub cancelled: bool,
}

impl InstallReport {
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty() && !self.cancelled
    }
}

/// natives 解压规则
#[derive(Debug, Clone)]
pub struct NativeExtract {
    pub file: FileEntry,
    pub excludes: Vec<String>,
}

/// 安装计划
#[derive(Debug, Default, Clone)]
pub struct InstallPlan {
    pub files: Vec<FileEntry>,
    pub natives: Vec<NativeExtract>,
    pub main_class: String,
    pub asset_index_id: String,
    pub total_bytes: u64,
}

pub struct VersionService {
    http: Arc<HttpClient>,
    cache_dir: std::path::PathBuf,
}

impl VersionService {
    pub fn new(http: Arc<HttpClient>, cache_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            http,
            cache_dir: cache_dir.into(),
        }
    }

    /// 远端版本清单；网络失败时回退本地缓存
    pub async fn manifest(&self) -> CoreResult<Vec<VersionSummary>> {
        let cache = self.cache_dir.join("version_manifest_v2.json");

        match self.http.get_json::<Value>(MANIFEST_URL).await {
            Ok(value) => {
                tokio::fs::create_dir_all(&self.cache_dir).await?;
                tokio::fs::write(&cache, serde_json::to_vec(&value)?).await?;
                parse_manifest(&value)
            }
            Err(error) => {
                if cache.is_file() {
                    tracing::warn!("清单拉取失败，使用本地缓存：{error}");
                    let bytes = tokio::fs::read(&cache).await?;
                    parse_manifest(&serde_json::from_slice(&bytes)?)
                } else {
                    Err(error)
                }
            }
        }
    }

    /// 版本 JSON（带本地缓存与 SHA-1 校验）
    pub async fn version_json(&self, summary: &VersionSummary) -> CoreResult<Value> {
        let cache = self.cache_dir.join(format!("{}.json", summary.id));

        if cache.is_file() {
            let valid = match &summary.sha1 {
                Some(expected) => crate::util::verify_sha1(&cache, expected).await,
                None => true,
            };
            if valid {
                return Ok(serde_json::from_slice(&tokio::fs::read(&cache).await?)?);
            }
        }

        let value: Value = self.http.get_json(&summary.url).await?;
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        tokio::fs::write(&cache, serde_json::to_vec(&value)?).await?;
        Ok(value)
    }

    /// 解析 `inheritsFrom` 继承链并合并
    pub async fn resolve(&self, id: &str, manifest: &[VersionSummary]) -> CoreResult<Value> {
        let mut chain: Vec<Value> = Vec::new();
        let mut current = id.to_string();

        for _ in 0..8 {
            let summary = manifest
                .iter()
                .find(|summary| summary.id == current)
                .ok_or_else(|| CoreError::Manifest(format!("版本 {current} 不在清单中")))?;

            let json = self.version_json(summary).await?;
            let parent = json
                .get("inheritsFrom")
                .and_then(Value::as_str)
                .map(str::to_string);
            chain.push(json);

            match parent {
                Some(parent_id) => current = parent_id,
                None => {
                    chain.reverse();
                    let (child, parents) = chain.split_last().expect("chain 非空");
                    return Ok(merge_inherited(child, parents));
                }
            }
        }

        Err(CoreError::Manifest(format!(
            "{id} 的继承链过深，疑似循环引用"
        )))
    }

    /// 生成安装计划
    ///
    /// `client_jar_id` 为客户端 jar 的归属版本目录名：原版安装即版本号自身；
    /// 加载器实例传原版版本号（Fabric 版本目录只放 JSON，jar 复用原版，避免副本）。
    pub async fn plan(
        &self,
        client_jar_id: &str,
        merged: &Value,
        options: &InstallOptions,
    ) -> CoreResult<InstallPlan> {
        let mut plan = InstallPlan {
            main_class: merged
                .get("mainClass")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ..Default::default()
        };

        // ① 客户端 jar（装入归属版本目录，见 `client_jar_id` 说明）
        if let Some(client) = merged.pointer("/downloads/client") {
            if let Some(entry) = download_entry(
                client,
                format!("versions/{client_jar_id}/{client_jar_id}.jar"),
            ) {
                plan.files.push(entry);
            }
        }

        // ② 类库（含 OS 规则过滤与 natives 分类器）
        if let Some(libraries) = merged.get("libraries").and_then(Value::as_array) {
            for library in libraries {
                if !rules_allow(library.get("rules")) {
                    continue;
                }

                if let Some(artifact) = library.pointer("/downloads/artifact") {
                    if let Some(path) = artifact.get("path").and_then(Value::as_str) {
                        if let Some(entry) = download_entry(artifact, format!("libraries/{path}")) {
                            plan.files.push(entry);
                        }
                    }
                }

                let Some(natives) = library.get("natives").and_then(Value::as_object) else {
                    continue;
                };
                let Some(template) = natives.get(native_os_key()).and_then(Value::as_str) else {
                    continue;
                };
                let classifier = template.replace("${arch}", native_arch_bits());

                let Some(download) = library
                    .pointer("/downloads/classifiers")
                    .and_then(|classifiers| classifiers.get(&classifier))
                else {
                    continue;
                };
                let Some(path) = download.get("path").and_then(Value::as_str) else {
                    continue;
                };
                let Some(entry) = download_entry(download, format!("libraries/{path}")) else {
                    continue;
                };

                let excludes = library
                    .pointer("/extract/exclude")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["META-INF/".to_string()]);

                plan.natives.push(NativeExtract {
                    file: entry.clone(),
                    excludes,
                });
                plan.files.push(entry);
            }
        }

        // ③ 资源索引与资源对象
        if let Some(index) = merged.get("assetIndex") {
            let index_id = index
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            plan.asset_index_id = index_id.clone();

            if let Some(entry) = download_entry(index, format!("assets/indexes/{index_id}.json")) {
                let index_url = entry.url.clone();
                plan.files.push(entry);
                if options.include_assets {
                    plan.files.extend(self.asset_objects(&index_url).await?);
                }
            }
        }

        // ④ 日志配置（缺失会降级为无日志配置，不影响启动）
        if let Some(logging) = merged.pointer("/logging/client/file") {
            if let Some(file_id) = logging.get("id").and_then(Value::as_str) {
                if let Some(entry) =
                    download_entry(logging, format!("assets/log_configs/{file_id}"))
                {
                    plan.files.push(entry);
                }
            }
        }

        plan.total_bytes = plan.files.iter().map(|entry| entry.size).sum();
        Ok(plan)
    }

    /// 展开资源索引为文件清单（内容寻址：路径与哈希一一对应，天然去重）
    pub async fn asset_objects(&self, index_url: &str) -> CoreResult<Vec<FileEntry>> {
        let index: Value = self.http.get_json(index_url).await?;
        let objects = index
            .get("objects")
            .and_then(Value::as_object)
            .ok_or_else(|| CoreError::Manifest("资源索引缺少 objects 字段".into()))?;

        let mut entries = Vec::with_capacity(objects.len());
        let mut seen_hashes = std::collections::HashSet::with_capacity(objects.len());

        for object in objects.values() {
            let hash = object
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if hash.len() < 2 {
                continue;
            }
            // 内容寻址：多个资源名可能共享同一对象，按 hash 去重避免重复下载
            if !seen_hashes.insert(hash.to_string()) {
                continue;
            }

            let size = object.get("size").and_then(Value::as_u64).unwrap_or(0);
            let prefix = &hash[..2];

            entries.push(FileEntry {
                path: format!("assets/objects/{prefix}/{hash}"),
                url: format!("{RESOURCES_BASE}/{prefix}/{hash}"),
                sha1: Some(hash.to_string()),
                size,
            });
        }

        Ok(entries)
    }

    /// 完整安装（清单 → 继承链解析 → 通用流水线）
    pub async fn install(
        &self,
        id: &str,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
    ) -> CoreResult<InstallReport> {
        let manifest = self.manifest().await?;
        let merged = self.resolve(id, &manifest).await?;
        self.install_merged(id, id, &merged, layout, engine, options, task_id)
            .await
    }

    /// 安装"已合并的版本 JSON"
    ///
    /// 流水线：安装计划 → 下载（命中跳过/分块/校验）→ natives 解压 →
    /// 合并 JSON 落地（`versions/<id>/<id>.json`）
    #[allow(clippy::too_many_arguments)] // 值与借用参数各半，打包结构体反而降低可读性
    pub async fn install_merged(
        &self,
        id: &str,
        client_jar_id: &str,
        merged: &Value,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
    ) -> CoreResult<InstallReport> {
        let plan = self.plan(client_jar_id, merged, options).await?;

        // 文件分流：`libraries/`、`assets/` 前缀走共享根
        //（共享目录或实例根，取决于布局），其余（客户端 jar 等）落实例根
        let (shared_files, instance_files): (Vec<_>, Vec<_>) =
            plan.files.clone().into_iter().partition(|entry| {
                entry.path.starts_with("libraries/") || entry.path.starts_with("assets/")
            });

        let mut groups = vec![(layout.root().to_path_buf(), instance_files)];
        if !shared_files.is_empty() {
            groups.push((layout.shared_data_root().to_path_buf(), shared_files));
        }

        let stats = engine
            .fetch_many_multi(groups, task_id, options.cancel.clone())
            .await;

        let mut report = InstallReport {
            version_id: id.to_string(),
            files: stats.files,
            skipped: stats.skipped,
            bytes: stats.bytes,
            failures: stats.failures.clone(),
            natives_extracted: 0,
            cancelled: stats.cancelled,
        };

        if !report.is_complete() {
            return Ok(report);
        }

        let natives_dir = layout.natives_dir(id);
        for native in &plan.natives {
            // natives 是类库文件（`libraries/` 前缀）→ 位于共享写入根
            let jar = layout.shared_data_root().join(&native.file.path);
            report.natives_extracted += extract_natives(&jar, &natives_dir, &native.excludes)?;
        }

        let version_dir = layout.version_dir(id);
        tokio::fs::create_dir_all(&version_dir).await?;
        tokio::fs::write(layout.version_json(id), serde_json::to_vec_pretty(merged)?).await?;

        Ok(report)
    }

    /// 修复实例：以实例**已落地的合并 JSON** 为基准做全量校验，
    /// 缺失/损坏的文件重新下载（引擎的"命中跳过"按 SHA-1 判定，只补差异）。
    ///
    /// 与 `install` 的区别：不拉取版本清单、不重解继承链——本地 JSON 就是该实例的
    /// "标准答案"，因此修复在清单源不可达时同样可用；JSON 缺失时视为不可修复
    /// （返回可读错误，提示重新安装）。
    pub async fn repair(
        &self,
        version_id: &str,
        client_jar_id: &str,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
    ) -> CoreResult<InstallReport> {
        let json_path = layout.version_json(version_id);
        let bytes = tokio::fs::read(&json_path).await.map_err(|error| {
            CoreError::Manifest(format!(
                "实例缺少版本 JSON（{}）：{error}；请重新安装该实例",
                json_path.display()
            ))
        })?;
        let merged: Value = serde_json::from_slice(&bytes)?;

        self.install_merged(
            version_id,
            client_jar_id,
            &merged,
            layout,
            engine,
            options,
            task_id,
        )
        .await
    }

    /// 安装 Fabric 实例：原版 + Fabric 档案合并 → 通用流水线。
    ///
    /// 版本 JSON 落于 `versions/fabric-loader-<loader>-<mc>/<同名的>.json`；
    /// 客户端 jar 复用原版目录（`client_jar_id = mc_version`，不复制副本）。
    /// 返回 `(加载器版本, 安装报告)`，供调用方落实例元数据。
    /// meta 系加载器（Fabric / Quilt）：profile JSON + Maven 类库，二者流程完全同构
    #[allow(clippy::too_many_arguments)] // 与 install_merged 一致：值与借用各半
    pub async fn install_meta(
        &self,
        kind: MetaLoader,
        mc_version: &str,
        loader_version: Option<&str>,
        // Some = 实例固定 id（GUI）；None = 沿用加载器推导命名（CLI / 整合包）
        version_id: Option<&str>,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
    ) -> CoreResult<(String, InstallReport)> {
        use crate::loader::{fabric, quilt};

        let loader = match loader_version {
            Some(version) if !version.trim().is_empty() => version.trim().to_string(),
            _ => match kind {
                MetaLoader::Fabric => fabric::latest_stable(&self.http, mc_version).await?,
                MetaLoader::Quilt => quilt::latest(&self.http, mc_version).await?,
            },
        };

        let manifest = self.manifest().await?;
        let vanilla = self.resolve(mc_version, &manifest).await?;
        let profile = match kind {
            MetaLoader::Fabric => fabric::profile_json(&self.http, mc_version, &loader).await?,
            MetaLoader::Quilt => quilt::profile_json(&self.http, mc_version, &loader).await?,
        };

        // 子级档案覆盖 mainClass；jvm 参数与类库追加在原版之后
        let mut merged = merge_inherited(&profile, &[vanilla]);
        let version_id = version_id
            .map(str::to_string)
            .unwrap_or_else(|| match kind {
                MetaLoader::Fabric => fabric::version_id(mc_version, &loader),
                MetaLoader::Quilt => quilt::version_id(mc_version, &loader),
            });
        merged["id"] = Value::String(version_id.clone());
        merged["type"] = Value::String("release".to_string());

        // 两类加载器的类库都只有 Maven 坐标（无 downloads）→ 补全并取 .sha1 侧车
        let injected = fabric::inject_maven_downloads(&mut merged);
        let verified = fabric::fill_maven_checksums(&self.http, &mut merged).await;
        tracing::info!(
            "{} {loader}：类库下载信息补全 {injected} 条、侧车校验 {verified} 条",
            kind.label()
        );

        let report = self
            .install_merged(
                &version_id,
                mc_version,
                &merged,
                layout,
                engine,
                options,
                task_id,
            )
            .await?;
        Ok((loader, report))
    }

    /// Fabric 安装（保留旧入口，CLI 与整合包流程在用）
    #[allow(clippy::too_many_arguments)]
    pub async fn install_fabric(
        &self,
        mc_version: &str,
        loader_version: Option<&str>,
        version_id: Option<&str>,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
    ) -> CoreResult<(String, InstallReport)> {
        self.install_meta(
            MetaLoader::Fabric,
            mc_version,
            loader_version,
            version_id,
            layout,
            engine,
            options,
            task_id,
        )
        .await
    }

    /// Forge / NeoForge：调用官方 installer，再把产物合并进通用流水线
    ///
    /// installer 会自己下载并执行 processor；这一步失败时错误信息里带安装器输出尾部。
    #[allow(clippy::too_many_arguments)] // 同上
    pub async fn install_forge(
        &self,
        flavor: crate::loader::forge::Flavor,
        mc_version: &str,
        loader_version: &str,
        version_id: Option<&str>,
        layout: &GameLayout,
        engine: &DownloadEngine,
        options: &InstallOptions,
        task_id: u64,
        java: &Path,
        progress: Option<(&crate::task::EventBus, u64)>,
    ) -> CoreResult<(String, InstallReport)> {
        use crate::loader::forge;

        // ① 原版文件必须就位：installer 要读原版 JSON 与客户端 jar
        if !layout.version_jar(mc_version).is_file() {
            self.install(mc_version, layout, engine, options, task_id)
                .await?;
        }

        // ② 目标版本 JSON 已存在且补丁签名一致 → 跳过安装器
        let expected = version_id
            .map(str::to_string)
            .unwrap_or_else(|| flavor.version_id(mc_version, loader_version));
        let signature = format!("{}:{}:{}", flavor.label(), mc_version, loader_version);
        let target_json = layout
            .root()
            .join("versions")
            .join(&expected)
            .join(format!("{expected}.json"));
        let version_id =
            if target_json.is_file() && merged_signature(&target_json) == Some(signature.clone()) {
                tracing::info!("{expected} 已是最新补丁组合，跳过安装器");
                expected
            } else {
                forge::install(
                    engine,
                    flavor,
                    mc_version,
                    loader_version,
                    &expected,
                    layout,
                    java,
                    progress,
                )
                .await?
            };

        // ③ 读产物 JSON，与原版合并后走通用流水线补齐资源与缺失类库
        let produced_path = layout
            .root()
            .join("versions")
            .join(&version_id)
            .join(format!("{version_id}.json"));
        let produced: Value = serde_json::from_slice(&tokio::fs::read(&produced_path).await?)?;
        let manifest = self.manifest().await?;
        let vanilla = self.resolve(mc_version, &manifest).await?;
        let mut merged = merge_inherited(&produced, &[vanilla]);
        merged["id"] = Value::String(version_id.clone());
        // 补丁签名：下次安装/修复用它判断能否跳过安装器
        merged["_mcl_patches"] = Value::String(signature);

        let report = self
            .install_merged(
                &version_id,
                mc_version,
                &merged,
                layout,
                engine,
                options,
                task_id,
            )
            .await?;
        Ok((version_id, report))
    }
}

/// 读版本 JSON 里的补丁签名（不存在/损坏返回 `None`）
fn merged_signature(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("_mcl_patches")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 解析清单 `versions` 数组
pub fn parse_manifest(value: &Value) -> CoreResult<Vec<VersionSummary>> {
    let versions = value
        .get("versions")
        .cloned()
        .ok_or_else(|| CoreError::Manifest("清单缺少 versions 字段".into()))?;
    serde_json::from_value(versions).map_err(|error| CoreError::Manifest(error.to_string()))
}

/// 最新正式版
pub fn latest_release(versions: &[VersionSummary]) -> Option<&VersionSummary> {
    versions
        .iter()
        .find(|summary| matches!(summary.kind, crate::models::VersionKind::Release))
}

/// 当前平台在 `natives` 映射中的键
pub fn native_os_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

/// natives 分类器的位宽占位符取值（`${arch}` → 32/64）
pub fn native_arch_bits() -> &'static str {
    if cfg!(target_pointer_width = "64") {
        "64"
    } else {
        "32"
    }
}

/// 评估 `rules` 数组（空特性集；libraries 场景通用入口）
pub fn rules_allow(rules: Option<&Value>) -> bool {
    rules_allow_with(rules, &std::collections::HashSet::new())
}

/// 评估 `rules` 数组（libraries 与 arguments 通用，支持特性开关）
///
/// - 无 `rules` 字段 → 允许
/// - `rules` 为空数组 → 允许（无约束条件；真实清单中不出现，属防御性处理）
/// - 带 `rules` → 初始「不允许」，按顺序应用**匹配当前平台与已启用特性**的规则 action
/// - 单条规则匹配条件：`os` 缺失或匹配当前平台，**且** `features` 缺失或其全部
///   键都在 `features` 集合中——两者同时满足才应用该条 action
///
/// 关键点 1：真实清单中 45 个带 rules 的库全部是 allow-only 形态
/// （如 `[{"action":"allow","os":{"name":"linux"}}]`），因此初始值必须是 `false`，
/// 否则 macOS/Windows 专属库会被误装到 Linux 上。
///
/// `--quickPlaySingleplayer ${quickPlaySingleplayer}` 等参数带
/// `{"features":{"is_quick_play_singleplayer":true}}` 规则且**没有 `os` 条件**。
/// 旧实现把「无 os 条件」当作「无条件放行」，导致这些参数被原样注入命令行，
/// 游戏以占位符字面量为世界名触发
/// 「Failed to quick play: Could not find world with the provided identifier」。
/// 修复：features 条件必须对照已启用特性集评估，默认（空集）一律排除。
pub fn rules_allow_with(
    rules: Option<&Value>,
    features: &std::collections::HashSet<String>,
) -> bool {
    let Some(Value::Array(rules)) = rules else {
        return true;
    };
    if rules.is_empty() {
        return true;
    }

    let mut allowed = false;
    for rule in rules {
        if !rule_matches(rule, features) {
            continue;
        }
        match rule.get("action").and_then(Value::as_str) {
            Some("disallow") => allowed = false,
            _ => allowed = true,
        }
    }
    allowed
}

/// 单条规则是否适用：os 与 features 条件都须通过（缺失视为通过）
fn rule_matches(rule: &Value, features: &std::collections::HashSet<String>) -> bool {
    if let Some(os) = rule.get("os") {
        if !os_matches(os) {
            return false;
        }
    }

    if let Some(Value::Object(declared)) = rule.get("features") {
        return declared.keys().all(|name| features.contains(name));
    }

    true
}

fn os_matches(os: &Value) -> bool {
    if let Some(name) = os.get("name").and_then(Value::as_str) {
        if name != native_os_key() {
            return false;
        }
    }
    if let Some(arch) = os.get("arch").and_then(Value::as_str) {
        let current = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "arm64",
            "x86" => "x86",
            other => other,
        };
        if arch != current {
            return false;
        }
    }
    true
}

fn download_entry(download: &Value, dest_rel: String) -> Option<FileEntry> {
    let url = download.get("url")?.as_str()?.to_string();
    let sha1 = download
        .get("sha1")
        .and_then(Value::as_str)
        .map(str::to_string);
    let size = download.get("size").and_then(Value::as_u64).unwrap_or(0);
    Some(FileEntry {
        path: dest_rel,
        url,
        sha1,
        size,
    })
}

/// 解压 natives（同步 IO；单文件体积小，放在启动前一次性执行）
fn extract_natives(jar: &Path, out_dir: &Path, excludes: &[String]) -> CoreResult<usize> {
    let file = std::fs::File::open(jar).map_err(|error| {
        CoreError::Unsupported(format!("打开 natives 归档失败 {}：{error}", jar.display()))
    })?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Unsupported(format!("解析 natives 归档失败：{error}")))?;

    std::fs::create_dir_all(out_dir)?;
    let mut extracted = 0usize;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| CoreError::Unsupported(format!("读取归档条目失败：{error}")))?;

        let name = entry.name().to_string();
        if entry.is_dir()
            || name.contains("..")
            || name.starts_with('/')
            || excludes.iter().any(|exclude| name.starts_with(exclude))
        {
            continue;
        }

        let dest = out_dir.join(&name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut output)?;
        extracted += 1;
    }

    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rules_allow_defaults_to_true_without_rules() {
        // 无 rules 字段 → 允许
        assert!(rules_allow(None));
        // 空 rules 数组 → 允许（防御性；真实数据不含此形态）
        assert!(rules_allow(Some(&json!([]))));
    }

    #[test]
    fn allow_only_rules_exclude_other_platforms() {
        // 真实数据形态：allow-only。非当前平台的库必须被排除
        let linux_only = json!([{ "action": "allow", "os": { "name": "linux" } }]);
        let macos_only = json!([{ "action": "allow", "os": { "name": "osx" } }]);
        let windows_only = json!([{ "action": "allow", "os": { "name": "windows" } }]);

        let expected_linux = cfg!(target_os = "linux");
        let expected_macos = cfg!(target_os = "macos");
        let expected_windows = cfg!(target_os = "windows");

        assert_eq!(rules_allow(Some(&linux_only)), expected_linux);
        assert_eq!(rules_allow(Some(&macos_only)), expected_macos);
        assert_eq!(rules_allow(Some(&windows_only)), expected_windows);
    }

    #[test]
    fn allow_all_rule_matches_every_platform() {
        let allow_all = json!([{ "action": "allow" }]);
        assert!(rules_allow(Some(&allow_all)));
    }

    #[test]
    fn rules_respect_current_platform_name() {
        // 只允许 windows 的库，在当前平台（非 Windows）应被过滤
        let windows_only = json!([{ "action": "allow", "os": { "name": "windows" } }]);
        assert_eq!(
            rules_allow(Some(&windows_only)),
            cfg!(target_os = "windows")
        );

        // 当前平台的规则应放行
        let current = json!([{ "action": "allow", "os": { "name": native_os_key() } }]);
        assert!(rules_allow(Some(&current)));
    }

    #[test]
    fn disallow_rule_after_allow_wins() {
        let rules = json!([
            { "action": "allow" },
            { "action": "disallow", "os": { "name": native_os_key() } }
        ]);
        assert!(!rules_allow(Some(&rules)), "后置 disallow 应生效");
    }

    #[test]
    fn feature_rules_excluded_unless_enabled() {
        // 真实形态（1.20+ quickplay）：features 规则、无 os 条件
        let quickplay =
            json!([{ "action": "allow", "features": { "is_quick_play_singleplayer": true } }]);

        // 默认（空特性集）必须排除——旧实现把「无 os 条件」当无条件放行，
        // 导致游戏收到 ${quickPlaySingleplayer} 字面量并报
        // 「Could not find world with the provided identifier」
        assert!(!rules_allow(Some(&quickplay)));

        // 显式启用后放行
        let mut features = std::collections::HashSet::new();
        features.insert("is_quick_play_singleplayer".to_string());
        assert!(rules_allow_with(Some(&quickplay), &features));
    }

    #[test]
    fn os_and_features_must_both_match() {
        // 当前平台 + 未启用特性 → 排除；两个条件是「与」关系
        let rule = json!([{
            "action": "allow",
            "os": { "name": native_os_key() },
            "features": { "is_quick_play_multiplayer": true }
        }]);
        assert!(!rules_allow(Some(&rule)));
    }

    #[test]
    fn parse_manifest_extracts_versions() {
        let value = json!({
            "latest": { "release": "1.20.1" },
            "versions": [
                { "id": "1.20.1", "type": "release", "releaseTime": "2023-06-12T13:25:51+00:00",
                  "url": "https://example.invalid/1.20.1.json", "sha1": "abc" },
                { "id": "1.20.2", "type": "snapshot", "releaseTime": "2023-08-01T00:00:00+00:00",
                  "url": "https://example.invalid/1.20.2.json" }
            ]
        });

        let versions = parse_manifest(&value).expect("解析失败");
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].id, "1.20.1");
        assert_eq!(
            latest_release(&versions).map(|v| v.id.as_str()),
            Some("1.20.1")
        );
    }

    #[test]
    fn native_arch_bits_is_64_on_this_machine() {
        assert_eq!(native_arch_bits(), "64");
    }
}
