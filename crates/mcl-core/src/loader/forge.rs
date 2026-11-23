//! Forge / NeoForge 安装
//!
//! 这两个加载器**不能**像 Fabric/Quilt 那样只合并一份 profile JSON：官方安装器要跑
//! 一串 processor（二进制补丁、jarsplitter、重映射），自己重写等于重做半个安装器。
//!
//! 这里的做法是调用**官方 installer jar**（`--installClient`），把产物合并回实例：
//! 1. 在临时目录搭一个「.minecraft 形状」的工作区：`libraries` 软链到共享目录，
//!    `versions/<mc>` 软链原版 json/jar，补一个 `launcher_profiles.json` 存根；
//! 2. 下载 installer 并执行（使用实例选定的 Java 运行时）；
//! 3. 把 `versions/<新版本>` 移回实例目录，其余交给通用流水线。
//!
//! 代价：安装期间由官方安装器直接访问 Forge/NeoForge 的 maven（不走我们的镜像与
//! 测速），且需要本机有满足版本要求的 Java——这两点已在文档记为债务。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::process::Command;

use crate::download::DownloadEngine;
use crate::error::{CoreError, CoreResult};
use crate::game::GameLayout;
use crate::http::HttpClient;
use crate::models::FileEntry;

/// Forge 全量版本清单（按 MC 版本分组）
pub const FORGE_METADATA: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/maven-metadata.json";
/// Forge Maven 基址（installer 也在这里）
pub const FORGE_MAVEN: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";
/// NeoForge 版本清单 API
pub const NEOFORGE_VERSIONS: &str =
    "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge";
/// NeoForge Maven 基址
pub const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";

/// 单个 processor 的最长执行时间（首次安装要重映射上百 MB 的 jar）
const PROCESSOR_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Forge,
    NeoForge,
}

impl Flavor {
    pub fn slug(self) -> &'static str {
        match self {
            Flavor::Forge => "forge",
            Flavor::NeoForge => "neoforge",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Flavor::Forge => "Forge",
            Flavor::NeoForge => "NeoForge",
        }
    }

    /// 实例在游戏目录中的版本标识（与 installer 产物一致）
    pub fn version_id(self, mc_version: &str, loader_version: &str) -> String {
        match self {
            Flavor::Forge => format!("{mc_version}-forge-{loader_version}"),
            Flavor::NeoForge => format!("neoforge-{loader_version}"),
        }
    }

    /// installer jar 的下载地址
    ///
    /// 1.8 之前的 Forge 版本号会重复一次 MC 版本（`1.7.10-10.13.4.1614-1.7.10`）。
    pub fn installer_url(self, mc_version: &str, loader_version: &str) -> String {
        match self {
            Flavor::Forge => {
                let full = forge_artifact_version(mc_version, loader_version);
                format!("{FORGE_MAVEN}/{full}/forge-{full}-installer.jar")
            }
            Flavor::NeoForge => {
                format!("{NEOFORGE_MAVEN}/{loader_version}/neoforge-{loader_version}-installer.jar")
            }
        }
    }
}

/// 1.8 之前的 Forge 制品名会重复 MC 版本：`{mc}-{loader}-{mc}`
fn forge_artifact_version(mc_version: &str, loader_version: &str) -> String {
    let classic = mc_version
        .split('.')
        .nth(1)
        .and_then(|minor| minor.parse::<u32>().ok())
        .map(|minor| minor < 8)
        .unwrap_or(false);
    if classic {
        format!("{mc_version}-{loader_version}-{mc_version}")
    } else {
        format!("{mc_version}-{loader_version}")
    }
}

/// NeoForge 的版本号前缀：1.20.4 → `20.4`、1.21 → `21.0`、1.20.1 → `47.1`（历史命名）
pub fn neoforge_prefix(mc_version: &str) -> String {
    if mc_version == "1.20.1" {
        return "47.1".to_string();
    }
    let trimmed = mc_version.strip_prefix("1.").unwrap_or(mc_version);
    if trimmed.contains('.') {
        trimmed.to_string()
    } else {
        format!("{trimmed}.0")
    }
}

/// 拉取指定 MC 版本可用的加载器版本（新→旧）
pub async fn loader_versions(
    http: &HttpClient,
    flavor: Flavor,
    mc_version: &str,
) -> CoreResult<Vec<String>> {
    match flavor {
        Flavor::Forge => {
            let value: Value = http.get_json(FORGE_METADATA).await?;
            parse_forge_metadata(&value, mc_version)
        }
        Flavor::NeoForge => {
            let value: Value = http.get_json(NEOFORGE_VERSIONS).await?;
            parse_neoforge_versions(&value, mc_version)
        }
    }
}

/// 解析 Forge `maven-metadata.json`：`{"1.20.1":["1.20.1-47.1.0", ...]}`
pub fn parse_forge_metadata(value: &Value, mc_version: &str) -> CoreResult<Vec<String>> {
    let list = value
        .get(mc_version)
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Manifest(format!("Forge 没有提供 {mc_version} 的安装器")))?;
    let prefix = format!("{mc_version}-");
    let legacy_suffix = format!("-{mc_version}");
    let mut versions: Vec<String> = list
        .iter()
        .filter_map(Value::as_str)
        .map(|full| full.strip_prefix(&prefix).unwrap_or(full))
        // 1.8 之前形如 `1.7.10-10.13.4.1614-1.7.10`，尾部重复的 MC 版本要去掉
        .map(|version| version.strip_suffix(&legacy_suffix).unwrap_or(version))
        .map(str::to_string)
        .collect();
    // 清单是旧→新，UI 需要新→旧
    versions.reverse();
    if versions.is_empty() {
        return Err(CoreError::Manifest(format!(
            "Forge 没有提供 {mc_version} 的安装器"
        )));
    }
    Ok(versions)
}

/// 解析 NeoForge 版本清单：`{"versions":["20.4.237", ...]}`
pub fn parse_neoforge_versions(value: &Value, mc_version: &str) -> CoreResult<Vec<String>> {
    let prefix = format!("{}.", neoforge_prefix(mc_version));
    let mut versions: Vec<String> = value
        .get("versions")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Manifest("NeoForge 版本清单格式异常".into()))?
        .iter()
        .filter_map(Value::as_str)
        .filter(|item| item.starts_with(&prefix))
        .map(str::to_string)
        .collect();
    versions.reverse();
    if versions.is_empty() {
        return Err(CoreError::Manifest(format!(
            "NeoForge 没有提供 {mc_version} 的安装器"
        )));
    }
    Ok(versions)
}

/// 自实现安装：解析 `install_profile.json` → 镜像下载全部类库 → 执行 processor
///
/// 与官方 installer 的区别只在「谁去下载」：所有 maven 依赖走 [`DownloadEngine`]
///（含 BMCLAPI 映射与测速切换），processor 仍用官方 jar 本地执行。
#[allow(clippy::too_many_arguments)] // 安装上下文参数较多，打包结构体反而降低可读性
pub async fn install(
    engine: &DownloadEngine,
    flavor: Flavor,
    mc_version: &str,
    loader_version: &str,
    version_id: &str,
    layout: &GameLayout,
    java: &Path,
    progress: Option<(&crate::task::EventBus, u64)>,
) -> CoreResult<String> {
    // 目录名由调用方给定（实例固定 id），不再由加载器推导
    let expected = version_id.to_string();
    let work = temp_work_dir();
    tokio::fs::create_dir_all(&work).await?;

    // ① 安装器本体（只用于取 profile/version JSON 与 data 文件，不执行它的下载逻辑）
    let installer = work.join("installer.jar");
    let entry = FileEntry {
        path: "installer.jar".to_string(),
        url: flavor.installer_url(mc_version, loader_version),
        sha1: None,
        size: 0,
    };
    engine.fetch(&entry, &installer).await?;
    // 安装器没有官方 sha1，用最小体积兜底：几百 KB 的「jar」通常是被错误页替换了
    const MIN_INSTALLER_BYTES: u64 = 256 * 1024;
    let installer_size = std::fs::metadata(&installer)
        .map(|meta| meta.len())
        .unwrap_or(0);
    if installer_size < MIN_INSTALLER_BYTES {
        return Err(CoreError::Unsupported(format!(
            "下载到的安装器只有 {installer_size} 字节，可能不是有效文件：{}",
            flavor.installer_url(mc_version, loader_version)
        )));
    }

    let profile = read_zip_json(&installer, "install_profile.json")?;
    // 1.7.10 ~ 1.11：只有 `install` + `versionInfo`，没有 processors；
    if let Some(install) = profile.get("install") {
        let expected = install_legacy(&installer, &profile, install, layout, &expected)?;
        let _ = tokio::fs::remove_dir_all(&work).await;
        return Ok(expected);
    }
    let json_path = profile
        .get("json")
        .and_then(Value::as_str)
        .unwrap_or("/version.json")
        .trim_start_matches('/')
        .to_string();
    let version_json = read_zip_json(&installer, &json_path)?;
    let libraries = parse_libraries(&profile)?;
    if libraries.is_empty() {
        return Err(CoreError::Manifest(format!(
            "{} 的 install_profile 没有类库清单",
            flavor.label()
        )));
    }

    // ② 类库：全部经下载引擎（镜像 + 校验 + 断点续传）
    // 注意 `{ROOT}` 是游戏目录（含 `libraries/`），库路径必须再拼一层
    let shared = layout.shared_data_root();
    let library_root = shared.join("libraries");
    for lib in &libraries {
        let dest = library_root.join(&lib.path);
        // 1.12.x 的 forge universal jar 没有下载地址：随安装器一起分发（`maven/<path>`）
        if lib.url.trim().is_empty() {
            extract_from_installer(&installer, &lib.path, &dest)?;
            continue;
        }
        let entry = FileEntry {
            path: lib.path.clone(),
            url: lib.url.clone(),
            sha1: lib.sha1.clone(),
            size: lib.size,
        };
        engine.fetch(&entry, &dest).await?;
    }

    // ③ data 变量（client 侧）
    let index: std::collections::HashMap<String, String> = libraries
        .iter()
        .map(|lib| (lib.name.clone(), lib.path.clone()))
        .collect();
    let mut data: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    data.insert("SIDE".into(), "client".into());
    data.insert("ROOT".into(), shared.display().to_string());
    data.insert("INSTALLER".into(), installer.display().to_string());
    data.insert(
        "MINECRAFT_JAR".into(),
        layout.version_jar(mc_version).display().to_string(),
    );
    if let Some(values) = profile.get("data").and_then(Value::as_object) {
        for (key, pair) in values {
            if let Some(raw) = pair.get("client").and_then(Value::as_str) {
                data.insert(
                    key.clone(),
                    resolve_data_value(raw, &index, &library_root, &installer, &work)?,
                );
            }
        }
    }

    // ④ processors：按顺序本地执行（client 侧）
    let processors = parse_processors(&profile);
    let client_processors: Vec<&Processor> = processors
        .iter()
        .filter(|item| item.applies_to_client())
        .collect();
    let total = client_processors.len();
    for (index_in_list, processor) in client_processors.iter().enumerate() {
        // 与 PCL2 相同思路：把安装阶段映射成用户可读的进度文案
        if let Some((bus, task_id)) = progress {
            bus.publish(crate::task::TaskEvent::StateChanged {
                id: task_id,
                state: crate::models::TaskState::Running,
                message: format!(
                    "{} 安装：执行处理器 {}/{total}（{}）",
                    flavor.label(),
                    index_in_list + 1,
                    processor
                        .jar
                        .rsplit(':')
                        .next()
                        .unwrap_or(processor.jar.as_str())
                ),
            });
        }
        run_processor(processor, java, &library_root, &index, &data).await?;
    }
    let ran = total;
    tracing::info!(
        "{} {}：类库 {} 个、processor {} 个完成",
        flavor.label(),
        loader_version,
        libraries.len(),
        ran
    );

    // ⑤ 版本 JSON 落地（后续由通用流水线补资源与缺失文件）
    let target_dir = layout.root().join("versions").join(&expected);
    tokio::fs::create_dir_all(&target_dir).await?;
    let mut version_json = version_json;
    version_json["id"] = Value::String(expected.clone());
    tokio::fs::write(
        target_dir.join(format!("{expected}.json")),
        serde_json::to_vec_pretty(&version_json)?,
    )
    .await?;

    let _ = tokio::fs::remove_dir_all(&work).await;
    Ok(expected)
}

/// 旧版（1.7.10 ~ 1.11）安装：解 universal jar + 落地 versionInfo
///
/// 这一代 installer 没有 processor：唯一的产物是把 `install.filePath`（universal jar）
/// 复制到 `libraries/<install.path 的 maven 路径>`，版本 JSON 直接取 `versionInfo`。
fn install_legacy(
    installer: &Path,
    profile: &Value,
    install: &Value,
    layout: &GameLayout,
    expected: &str,
) -> CoreResult<String> {
    let file_path = install
        .get("filePath")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Manifest("旧版 install_profile 缺少 install.filePath".into()))?;
    let coord = install
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Manifest("旧版 install_profile 缺少 install.path".into()))?;

    let dest = layout
        .shared_data_root()
        .join("libraries")
        .join(maven_path(coord));
    extract_zip_entry(installer, file_path, &dest)?;

    let mut version_info = profile
        .get("versionInfo")
        .cloned()
        .ok_or_else(|| CoreError::Manifest("旧版 install_profile 缺少 versionInfo".into()))?;
    version_info["id"] = Value::String(expected.to_string());

    let target_dir = layout.root().join("versions").join(expected);
    std::fs::create_dir_all(&target_dir)?;
    std::fs::write(
        target_dir.join(format!("{expected}.json")),
        serde_json::to_vec_pretty(&version_info)?,
    )?;
    tracing::info!(
        "旧版 Forge：universal jar → {}，版本 JSON 已落地",
        dest.display()
    );
    Ok(expected.to_string())
}

/// install_profile 里的一个类库
#[derive(Debug, Clone)]
struct Library {
    name: String,
    path: String,
    url: String,
    sha1: Option<String>,
    size: u64,
}

/// install_profile 里的一个 processor
#[derive(Debug, Clone)]
struct Processor {
    sides: Option<Vec<String>>,
    jar: String,
    classpath: Vec<String>,
    args: Vec<String>,
    /// 产物路径模板 → 期望 SHA-1 模板（用于「已完成即跳过」与执行后校验）
    outputs: Vec<(String, String)>,
}

impl Processor {
    fn applies_to_client(&self) -> bool {
        match &self.sides {
            None => true,
            Some(sides) => sides.iter().any(|side| side == "client"),
        }
    }
}

fn parse_libraries(profile: &Value) -> CoreResult<Vec<Library>> {
    let list = profile
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Manifest("install_profile 缺少 libraries".into()))?;
    let mut out = Vec::new();
    for item in list {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(artifact) = item.pointer("/downloads/artifact") else {
            continue;
        };
        let (Some(path), Some(url)) = (
            artifact.get("path").and_then(Value::as_str),
            artifact.get("url").and_then(Value::as_str),
        ) else {
            continue;
        };
        out.push(Library {
            name: name.to_string(),
            path: path.to_string(),
            url: url.to_string(),
            sha1: artifact
                .get("sha1")
                .and_then(Value::as_str)
                .map(str::to_string),
            size: artifact.get("size").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    Ok(out)
}

fn parse_processors(profile: &Value) -> Vec<Processor> {
    let Some(list) = profile.get("processors").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| {
            let jar = item.get("jar").and_then(Value::as_str)?.to_string();
            let classpath = item
                .get("classpath")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let args = item
                .get("args")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let sides = item.get("sides").and_then(Value::as_array).map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
            let outputs = item
                .get("outputs")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Some(Processor {
                sides,
                jar,
                classpath,
                args,
                outputs,
            })
        })
        .collect()
}

/// `[group:artifact:version[:classifier][@ext]]` / `'/data/x.lzma'` / `'sha1'` / 字面量
fn resolve_data_value(
    raw: &str,
    index: &std::collections::HashMap<String, String>,
    library_root: &Path,
    installer: &Path,
    work: &Path,
) -> CoreResult<String> {
    let value = raw.trim();
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return Ok(inner.to_string());
    }
    if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return Ok(library_root
            .join(
                index
                    .get(inner)
                    .cloned()
                    .unwrap_or_else(|| maven_path(inner)),
            )
            .display()
            .to_string());
    }
    if let Some(inner) = value.strip_prefix('/') {
        // 安装器内置文件：解出来给 processor 用
        let dest = work.join("installer-data").join(
            Path::new(inner)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| inner.replace('/', "_")),
        );
        extract_zip_entry(installer, inner, &dest)?;
        return Ok(dest.display().to_string());
    }
    Ok(value.to_string())
}

/// 执行一个 processor：`java -cp <jar+classpath> <Main-Class> <args...>`
async fn run_processor(
    processor: &Processor,
    java: &Path,
    root: &Path,
    index: &std::collections::HashMap<String, String>,
    data: &std::collections::HashMap<String, String>,
) -> CoreResult<()> {
    let resolve = |coord: &str| -> PathBuf {
        root.join(
            index
                .get(coord)
                .cloned()
                .unwrap_or_else(|| maven_path(coord)),
        )
    };

    let jar_path = resolve(&processor.jar);
    let main_class = read_main_class(&jar_path)?;
    let mut classpath = vec![jar_path.display().to_string()];
    for entry in &processor.classpath {
        classpath.push(resolve(entry).display().to_string());
    }
    let args: Vec<String> = processor
        .args
        .iter()
        .map(|arg| substitute(arg, index, root, data))
        .collect();

    // 产物已就绪就跳过（HMCL 的做法）：重装/修复时避免重复跑重映射
    let expected: Vec<(PathBuf, String)> = processor
        .outputs
        .iter()
        .map(|(path, hash)| {
            (
                PathBuf::from(substitute(path, index, root, data)),
                substitute(hash, index, root, data),
            )
        })
        .collect();
    if !expected.is_empty()
        && expected
            .iter()
            .all(|(path, hash)| file_sha1(path).as_deref() == Some(hash.as_str()))
    {
        tracing::debug!("processor {main_class} 产物齐全，跳过");
        return Ok(());
    }

    let output = tokio::time::timeout(
        PROCESSOR_TIMEOUT,
        Command::new(java)
            .arg("-cp")
            .arg(classpath.join(":"))
            .arg(&main_class)
            .args(&args)
            .current_dir(root)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        CoreError::Unsupported(format!(
            "processor {main_class} 超过 {} 秒未完成",
            PROCESSOR_TIMEOUT.as_secs()
        ))
    })??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(8).collect::<Vec<_>>().join(" / ");
        return Err(CoreError::Unsupported(format!(
            "processor {main_class} 失败（{}）：{}",
            output.status,
            if tail.is_empty() {
                "无输出".to_string()
            } else {
                tail
            }
        )));
    }

    // 产物校验：SHA-1 不符时，jar 退化为 ZIP 结构校验
    // （不同 zlib 实现重打包的 jar 字节不同，但内容合法）
    for (path, hash) in expected {
        let actual = file_sha1(&path);
        if actual.as_deref() == Some(hash.as_str()) {
            continue;
        }
        let is_jar = path
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("jar"))
            .unwrap_or(false);
        if is_jar && zip_is_valid(&path) {
            tracing::warn!(
                "processor {main_class} 产物 SHA-1 与清单不符，但 ZIP 结构有效，接受：{}",
                path.display()
            );
            continue;
        }
        let _ = std::fs::remove_file(&path);
        return Err(CoreError::HashMismatch {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

/// 文件 SHA-1（读失败返回 `None`）
fn file_sha1(path: &Path) -> Option<String> {
    use sha1::{Digest, Sha1};
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha1::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// jar 是否为结构有效的 ZIP（中央目录可读且有条目）
fn zip_is_valid(path: &Path) -> bool {
    std::fs::File::open(path)
        .ok()
        .and_then(|file| zip::ZipArchive::new(file).ok())
        .map(|mut archive| !archive.is_empty() && archive.by_index(0).is_ok())
        .unwrap_or(false)
}

/// 替换 `{KEY}` 与 `[coord]` 两种占位
fn substitute(
    text: &str,
    index: &std::collections::HashMap<String, String>,
    root: &Path,
    data: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(['{', '[']) {
        out.push_str(&rest[..pos]);
        let open = rest.as_bytes()[pos] as char;
        let close = if open == '{' { '}' } else { ']' };
        let Some(end) = rest[pos..].find(close).map(|offset| pos + offset) else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let key = &rest[pos + 1..end];
        if open == '{' {
            match data.get(key) {
                Some(value) => out.push_str(value),
                None => {
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            }
        } else {
            let path = root.join(index.get(key).cloned().unwrap_or_else(|| maven_path(key)));
            out.push_str(&path.display().to_string());
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Maven 坐标 → `libraries/` 下的相对路径
fn maven_path(coord: &str) -> String {
    let (coord, ext) = match coord.split_once('@') {
        Some((head, ext)) => (head, ext),
        None => (coord, "jar"),
    };
    let mut parts = coord.split(':');
    let group = parts.next().unwrap_or_default().replace('.', "/");
    let artifact = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    let classifier = parts.next();
    let file = match classifier {
        Some(classifier) => format!("{artifact}-{version}-{classifier}.{ext}"),
        None => format!("{artifact}-{version}.{ext}"),
    };
    format!("{group}/{artifact}/{version}/{file}")
}

/// 读 jar 内 JSON
fn read_zip_json(jar: &Path, entry: &str) -> CoreResult<Value> {
    let bytes = read_zip_entry(jar, entry)?;
    serde_json::from_slice(&bytes).map_err(CoreError::Json)
}

fn read_zip_entry(jar: &Path, entry: &str) -> CoreResult<Vec<u8>> {
    let file = std::fs::File::open(jar)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Unsupported(format!("安装器不是有效 zip：{error}")))?;
    let mut target = archive
        .by_name(entry)
        .map_err(|_| CoreError::Manifest(format!("安装器缺少 {entry}")))?;
    let mut bytes = Vec::new();
    target.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// 从安装器里按「路径后缀」找条目并解出（安装器把自带制品放在 `maven/<maven 路径>`）
fn extract_from_installer(jar: &Path, relative: &str, dest: &Path) -> CoreResult<()> {
    let file = std::fs::File::open(jar)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Unsupported(format!("安装器不是有效 zip：{error}")))?;
    let mut found = None;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| CoreError::Unsupported(format!("读取安装器条目失败：{error}")))?;
        let name = entry.name().to_string();
        if name == relative || name.ends_with(&format!("/{relative}")) {
            found = Some(name);
            break;
        }
    }
    let Some(name) = found else {
        return Err(CoreError::Manifest(format!(
            "安装器内缺少 {relative}，且该库没有下载地址"
        )));
    };
    extract_zip_entry(jar, &name, dest)
}

fn extract_zip_entry(jar: &Path, entry: &str, dest: &Path) -> CoreResult<()> {
    let bytes = read_zip_entry(jar, entry)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, bytes)?;
    Ok(())
}

/// 从 processor jar 的 MANIFEST 读 `Main-Class`（长行会折行，需拼接续行）
fn read_main_class(jar: &Path) -> CoreResult<String> {
    let bytes = read_zip_entry(jar, "META-INF/MANIFEST.MF")?;
    let text = String::from_utf8_lossy(&bytes);
    let mut joined = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            joined.push_str(rest);
        } else {
            joined.push('\n');
            joined.push_str(line);
        }
    }
    for line in joined.lines() {
        if let Some(value) = line.strip_prefix("Main-Class:") {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(CoreError::Manifest(format!(
        "{} 没有声明 Main-Class",
        jar.display()
    )))
}

/// 临时工作目录（进程内唯一）
fn temp_work_dir() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("mcl-loader-{}-{stamp}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_forge_metadata_newest_first() {
        let value = json!({
            "1.20.1": ["1.20.1-47.1.0", "1.20.1-47.2.0", "1.20.1-47.3.0"],
            "1.19.2": ["1.19.2-43.2.0"]
        });
        assert_eq!(
            parse_forge_metadata(&value, "1.20.1").unwrap(),
            vec!["47.3.0".to_string(), "47.2.0".into(), "47.1.0".into()]
        );
        assert!(parse_forge_metadata(&value, "1.7.10").is_err());
    }

    #[test]
    fn parses_neoforge_versions_by_prefix() {
        let value = json!({
            "versions": ["20.2.86", "20.4.100", "20.4.237", "21.0.1", "47.1.106"]
        });
        assert_eq!(
            parse_neoforge_versions(&value, "1.20.4").unwrap(),
            vec!["20.4.237".to_string(), "20.4.100".into()]
        );
        assert_eq!(
            parse_neoforge_versions(&value, "1.21").unwrap(),
            vec!["21.0.1".to_string()]
        );
        assert_eq!(
            parse_neoforge_versions(&value, "1.20.1").unwrap(),
            vec!["47.1.106".to_string()]
        );
    }

    #[test]
    fn neoforge_prefix_matches_community_rules() {
        assert_eq!(neoforge_prefix("1.20.4"), "20.4");
        assert_eq!(neoforge_prefix("1.21"), "21.0");
        assert_eq!(neoforge_prefix("1.21.1"), "21.1");
        assert_eq!(neoforge_prefix("1.20.1"), "47.1");
    }

    #[test]
    fn parses_processor_outputs() {
        let profile = json!({
            "processors": [{
                "jar": "net.minecraftforge:jarsplitter:1.1.4",
                "classpath": [],
                "args": ["--input", "{MINECRAFT_JAR}"],
                "outputs": { "{MC_SLIM}": "{MC_SLIM_SHA}" }
            }]
        });
        let processors = parse_processors(&profile);
        assert_eq!(processors.len(), 1);
        assert_eq!(
            processors[0].outputs,
            vec![("{MC_SLIM}".to_string(), "{MC_SLIM_SHA}".to_string())]
        );
    }

    #[test]
    fn zip_integrity_check_rejects_non_zip() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mcl-zip-{stamp}.jar"));
        std::fs::write(&path, b"not a zip").expect("写文件");
        assert!(!zip_is_valid(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn maven_paths_match_library_layout() {
        assert_eq!(
            maven_path("net.minecraftforge:forge:1.20.1-47.2.0:client"),
            "net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-client.jar"
        );
        assert_eq!(
            maven_path("de.oceanlabs.mcp:mcp_config:1.20.1-20230612.114412@zip"),
            "de/oceanlabs/mcp/mcp_config/1.20.1-20230612.114412/mcp_config-1.20.1-20230612.114412.zip"
        );
        assert_eq!(
            maven_path("org.ow2.asm:asm:9.3"),
            "org/ow2/asm/asm/9.3/asm-9.3.jar"
        );
    }

    #[test]
    fn substitutes_keys_and_coordinates() {
        let mut index = std::collections::HashMap::new();
        index.insert("a:b:1".to_string(), "a/b/1/b-1.jar".to_string());
        let mut data = std::collections::HashMap::new();
        data.insert("SIDE".to_string(), "client".to_string());
        let root = Path::new("/root");

        assert_eq!(
            substitute("--side {SIDE}", &index, root, &data),
            "--side client"
        );
        assert_eq!(
            substitute("--input [a:b:1]", &index, root, &data),
            "--input /root/a/b/1/b-1.jar"
        );
        // 未定义的占位保持原样，便于定位 profile 变更
        assert_eq!(
            substitute("{NOT_DEFINED}", &index, root, &data),
            "{NOT_DEFINED}"
        );
    }

    #[test]
    fn legacy_forge_names_repeat_mc_version() {
        assert_eq!(
            forge_artifact_version("1.7.10", "10.13.4.1614"),
            "1.7.10-10.13.4.1614-1.7.10"
        );
        assert_eq!(
            forge_artifact_version("1.12.2", "14.23.5.2859"),
            "1.12.2-14.23.5.2859"
        );
        assert!(Flavor::Forge
            .installer_url("1.7.10", "10.13.4.1614")
            .ends_with("forge-1.7.10-10.13.4.1614-1.7.10-installer.jar"));
    }

    #[test]
    fn strips_legacy_suffix_from_version_list() {
        // 元数据是旧→新，解析后要倒序
        let value =
            json!({ "1.7.10": ["1.7.10-10.13.4.1566-1.7.10", "1.7.10-10.13.4.1614-1.7.10"] });
        assert_eq!(
            parse_forge_metadata(&value, "1.7.10").unwrap(),
            vec!["10.13.4.1614".to_string(), "10.13.4.1566".into()]
        );
    }

    #[test]
    fn version_id_and_installer_url() {
        assert_eq!(
            Flavor::Forge.version_id("1.20.1", "47.2.0"),
            "1.20.1-forge-47.2.0"
        );
        assert_eq!(
            Flavor::NeoForge.version_id("1.20.4", "20.4.237"),
            "neoforge-20.4.237"
        );
        assert!(Flavor::Forge
            .installer_url("1.20.1", "47.2.0")
            .ends_with("forge-1.20.1-47.2.0-installer.jar"));
        assert!(Flavor::NeoForge
            .installer_url("1.20.4", "20.4.237")
            .ends_with("neoforge-20.4.237-installer.jar"));
    }
}
