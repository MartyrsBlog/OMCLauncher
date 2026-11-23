//! Java 运行时探测、选择与**自动下载**
//!
//! 探测链：`JAVA_HOME` → 常见安装目录（`/usr/lib/jvm`、`/usr/java`、`/opt/java`）
//! → `PATH` → **本启动器托管目录**（`<数据目录>/java/`）。每个候选执行
//! `java -version` 解析主版本号。
//!
//! 缺失时经 Mojang `java-runtime` 清单自动下载（[`ensure_java`]）：
//! 平台组件选择 → 清单展开（文件/链接/可执行位）→ 复用下载引擎
//! （镜像映射、并发、断点、SHA-1）→ 落地到托管目录后即可启动。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;

use crate::download::DownloadEngine;
use crate::error::{CoreError, CoreResult};
use crate::http::HttpClient;
use crate::models::FileEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaRuntime {
    pub path: PathBuf,
    pub major: u32,
    pub version_string: String,
}

/// 探测本机全部可用 Java 运行时
pub fn detect_javas() -> Vec<JavaRuntime> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(home) = std::env::var("JAVA_HOME") {
        if !home.is_empty() {
            candidates.push(PathBuf::from(home).join("bin").join(java_binary()));
        }
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    for root in java_roots(home.as_deref()) {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let base = entry.path();
                candidates.push(base.join("bin").join(java_binary()));
                // macOS 的 Contents/Home 布局
                candidates.push(base.join("Contents/Home/bin").join(java_binary()));
            }
        }
        // 有些布局把 bin 直接放在 root 下（如 ~/java/bin/java）
        candidates.push(root.join("bin").join(java_binary()));
    }

    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join(java_binary()));
        }
    }

    // 本启动器自动下载的运行时
    if let Ok(entries) = std::fs::read_dir(crate::platform::paths::java_dir()) {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("bin").join(java_binary()));
        }
    }

    candidates.retain(|candidate| candidate.is_file());
    candidates.sort();
    candidates.dedup();

    candidates
        .into_iter()
        .filter_map(|candidate| probe_java(&candidate).ok())
        .collect()
}

/// 需要扫描的 JDK 根目录：系统位置 + 各版本管理器/IDE 在用户目录下的默认位置。
///
/// 单独成函数是为了可测（`detect_javas` 依赖真实文件系统与环境变量）。
fn java_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![
        PathBuf::from("/usr/lib/jvm"),
        PathBuf::from("/usr/java"),
        PathBuf::from("/opt/java"),
        PathBuf::from("/Library/Java/JavaVirtualMachines"),
    ];
    if let Some(home) = home {
        for relative in [
            // IntelliJ IDEA 下载的 JDK
            ".jdks",
            // SDKMAN
            ".sdkman/candidates/java",
            // Gradle toolchain 自动下载
            ".gradle/jdks",
            // 常见的手动安装位置
            ".local/share/jvm",
            "java",
            "jdk",
            // macOS 用户级 JVM 目录
            "Library/Java/JavaVirtualMachines",
        ] {
            roots.push(home.join(relative));
        }
    }
    roots
}

fn java_binary() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// 执行 `java -version` 并解析主版本（版本信息写到 stderr）
pub fn probe_java(path: &Path) -> CoreResult<JavaRuntime> {
    let output = Command::new(path)
        .arg("-version")
        .output()
        .map_err(|error| {
            CoreError::JavaUnavailable(format!("{} 执行失败：{error}", path.display()))
        })?;

    let text = String::from_utf8_lossy(&output.stderr).to_string();
    let major = parse_major_version(&text).ok_or_else(|| {
        CoreError::JavaUnavailable(format!("{} 版本信息无法解析", path.display()))
    })?;

    Ok(JavaRuntime {
        path: path.to_path_buf(),
        major,
        version_string: text.lines().next().unwrap_or_default().to_string(),
    })
}

/// 解析版本字符串主版本号
///
/// 兼容两种形态：`openjdk version "25.0.4"` → 25；`java version "1.8.0_401"` → 8
pub fn parse_major_version(text: &str) -> Option<u32> {
    let start = text.find('"')? + 1;
    let rest = &text[start..];
    let end = rest.find('"')?;
    let version = &rest[..end];

    let first: u32 = version.split(['.', '_']).next()?.parse().ok()?;
    if first == 1 {
        // 1.8.0_401 → 8
        version.split('.').nth(1)?.parse().ok()
    } else {
        Some(first)
    }
}

/// 选择最合适的 Java：满足所需主版本前提下取最小者（避免过新版本引入兼容问题）
pub fn select_java(runtimes: &[JavaRuntime], required_major: u32) -> Option<JavaRuntime> {
    let mut candidates: Vec<&JavaRuntime> = runtimes
        .iter()
        .filter(|runtime| runtime.major >= required_major)
        .collect();
    candidates.sort_by_key(|runtime| runtime.major);
    candidates.first().map(|runtime| (*runtime).clone())
}

/// 一体化的「探测 + 选择」入口
pub fn resolve_java(required_major: u32) -> CoreResult<JavaRuntime> {
    let runtimes = detect_javas();
    select_java(&runtimes, required_major).ok_or_else(|| {
        CoreError::JavaUnavailable(format!(
            "未找到满足 Java {required_major}+ 的运行时（已探测 {} 个）",
            runtimes.len()
        ))
    })
}

// ───────────────────────── 自动下载─────────────────────────

/// Mojang 官方 Java 运行时清单（`launchermeta.mojang.com`，经下载源自动映射镜像）
pub const JAVA_RUNTIME_MANIFEST: &str =
    "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";

/// `java-runtime` 清单中的平台键（与通用 OS 命名不同）
fn runtime_platform_key() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "aarch64") {
            "windows-arm64"
        } else {
            "windows-x64"
        }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "mac-os-arm64"
        } else {
            "mac-os"
        }
    } else if cfg!(target_arch = "x86") {
        "linux-i386"
    } else {
        "linux"
    }
}

/// 解析运行时版本名的主版本：`17.0.8` → 17、`8u312-b07` → 8
fn parse_runtime_major(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 从 `all.json` 选择最合适的运行时组件：满足所需主版本前提下取最小
///
/// 返回 `(组件名, 组件清单 URL, 版本名)`，如 `("java-runtime-gamma", ..., "17.0.8")`。
pub fn select_runtime_component(
    all: &Value,
    platform: &str,
    required_major: u32,
) -> Option<(String, String, String)> {
    let components = all.get(platform)?.as_object()?;
    let mut best: Option<(u32, String, String, String)> = None;

    for (component, versions) in components {
        let Some(entry) = versions.as_array().and_then(|list| list.first()) else {
            continue;
        };
        let (Some(version_name), Some(manifest_url)) = (
            entry.pointer("/version/name").and_then(Value::as_str),
            entry.pointer("/manifest/url").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(major) = parse_runtime_major(version_name) else {
            continue;
        };
        if major < required_major {
            continue;
        }

        let better = best
            .as_ref()
            .map(|(best_major, ..)| major < *best_major)
            .unwrap_or(true);
        if better {
            best = Some((
                major,
                component.clone(),
                manifest_url.to_string(),
                version_name.to_string(),
            ));
        }
    }

    best.map(|(_, component, url, version)| (component, url, version))
}

/// 下载并落地一个 Java 运行时到 `<root>/<组件>/`
///
/// - 文件级 SHA-1 校验（复用下载引擎：镜像映射 / 并发 / 断点 / 校验）
/// - 按清单还原符号链接与可执行位（Windows 无此语义时忽略）
/// - 返回 `None` 表示被取消（暂停：已下载部分保留，再次调用即续传）
pub async fn install_runtime(
    http: &HttpClient,
    engine: &DownloadEngine,
    required_major: u32,
    root: &Path,
    task_id: u64,
    cancel: Option<Arc<AtomicBool>>,
) -> CoreResult<Option<JavaRuntime>> {
    let all: Value = http.get_json(JAVA_RUNTIME_MANIFEST).await?;
    let platform = runtime_platform_key();
    let (component, manifest_url, version_name) =
        select_runtime_component(&all, platform, required_major).ok_or_else(|| {
            CoreError::JavaUnavailable(format!(
                "官方 Java 运行时清单中没有满足 Java {required_major}+ 的 {platform} 组件"
            ))
        })?;

    let manifest: Value = http.get_json(&manifest_url).await?;
    let files = manifest
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| CoreError::JavaUnavailable("Java 运行时清单缺少 files 字段".into()))?;

    let mut entries = Vec::with_capacity(files.len());
    let mut executables: Vec<PathBuf> = Vec::new();
    let mut links: Vec<(PathBuf, String)> = Vec::new();

    for (relative, entry) in files {
        match entry.get("type").and_then(Value::as_str) {
            Some("file") => {
                let Some(raw) = entry.pointer("/downloads/raw") else {
                    continue;
                };
                let Some(url) = raw.get("url").and_then(Value::as_str) else {
                    continue;
                };
                entries.push(FileEntry {
                    path: format!("{component}/{relative}"),
                    url: url.to_string(),
                    sha1: raw.get("sha1").and_then(Value::as_str).map(str::to_string),
                    size: raw.get("size").and_then(Value::as_u64).unwrap_or(0),
                });
                if entry
                    .get("executable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    executables.push(root.join(&component).join(relative));
                }
            }
            Some("link") => {
                if let Some(target) = entry.get("target").and_then(Value::as_str) {
                    links.push((root.join(&component).join(relative), target.to_string()));
                }
            }
            _ => {}
        }
    }

    let stats = engine
        .fetch_many_cancellable(entries, root, task_id, cancel)
        .await;
    if stats.cancelled {
        return Ok(None);
    }
    if let Some((path, error)) = stats.failures.first() {
        return Err(CoreError::Unsupported(format!(
            "Java 运行时下载有 {} 个文件失败（首个：{path} → {error}）",
            stats.failures.len()
        )));
    }

    for (path, target) in links {
        create_symlink(&target, &path);
    }
    for path in executables {
        set_executable(&path);
    }

    // 探测一次：拿真实版本串并验证二进制可执行（失败则回退清单声明值）
    let java_path = root.join(&component).join("bin").join(java_binary());
    match probe_java(&java_path) {
        Ok(runtime) => Ok(Some(runtime)),
        Err(_) => Ok(Some(JavaRuntime {
            path: java_path,
            major: required_major,
            version_string: version_name,
        })),
    }
}

/// 一体化入口：先探测（含托管目录），缺失时自动下载。
///
/// 返回 `None` 表示下载被取消（暂停）。
pub async fn ensure_java(
    http: &HttpClient,
    engine: &DownloadEngine,
    required_major: u32,
    task_id: u64,
    cancel: Option<Arc<AtomicBool>>,
) -> CoreResult<Option<JavaRuntime>> {
    if let Some(found) = select_java(&detect_javas(), required_major) {
        return Ok(Some(found));
    }
    install_runtime(
        http,
        engine,
        required_major,
        &crate::platform::paths::java_dir(),
        task_id,
        cancel,
    )
    .await
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::os::unix::fs::symlink(target, path);
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, _path: &Path) {}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// 按 Minecraft 版本推断所需 Java 主版本（版本 JSON 缺 `javaVersion` 时的兜底）。
///
/// Mojang 的要求：1.17+ 需要 16/17，1.18–1.20.4 需要 17，1.20.5+ 需要 21。
/// 1.17 这里直接给 17（向下兼容），避免为老版本单独下载 16。
/// 解析不出数字（如 `latest`）按现代版本处理，给 21。
pub fn required_major_for_mc(mc_version: &str) -> u32 {
    let mut parts = mc_version.trim().split(['.', '-', '_']);
    let Some(major) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
        return 21;
    };
    if major > 1 {
        return 21;
    }
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    // 「1.20.1-forge」这类 id：patch 段先剥掉非数字后缀再解析
    let patch = parts
        .next()
        .map(|part| part.trim_end_matches(|ch: char| !ch.is_ascii_digit()))
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    match minor {
        0..=16 => 8,
        17..=19 => 17,
        20 if patch < 5 => 17,
        _ => 21,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_version_string() {
        assert_eq!(
            parse_major_version("openjdk version \"25.0.4.1\" 2026-08-18 LTS"),
            Some(25)
        );
        assert_eq!(
            parse_major_version("openjdk version \"17.0.9\" 2023-10-17"),
            Some(17)
        );
    }

    #[test]
    fn parses_legacy_version_string() {
        assert_eq!(
            parse_major_version("java version \"1.8.0_401\"\nJava(TM) SE Runtime"),
            Some(8)
        );
    }

    #[test]
    fn returns_none_for_garbage() {
        assert_eq!(parse_major_version("no version here"), None);
    }

    #[test]
    fn selects_lowest_satisfying_major() {
        let runtimes = vec![
            JavaRuntime {
                path: "/j25/bin/java".into(),
                major: 25,
                version_string: String::new(),
            },
            JavaRuntime {
                path: "/j17/bin/java".into(),
                major: 17,
                version_string: String::new(),
            },
            JavaRuntime {
                path: "/j21/bin/java".into(),
                major: 21,
                version_string: String::new(),
            },
            JavaRuntime {
                path: "/j8/bin/java".into(),
                major: 8,
                version_string: String::new(),
            },
        ];

        // 需要 17+ → 选 17（而非 21/25）
        let selected = select_java(&runtimes, 17).expect("应选中 17");
        assert_eq!(selected.major, 17);

        // 需要 22+ → 选 25（17/21 不满足）
        assert_eq!(select_java(&runtimes, 22).expect("应选中 25").major, 25);

        // 需要 26+ → 无解
        assert!(select_java(&runtimes, 26).is_none());
    }

    #[test]
    fn parses_runtime_major_from_component_names() {
        assert_eq!(parse_runtime_major("17.0.8"), Some(17));
        assert_eq!(parse_runtime_major("8u312-b07"), Some(8));
        assert_eq!(parse_runtime_major("21.0.1"), Some(21));
        assert_eq!(parse_runtime_major("no-digits"), None);
    }

    #[test]
    fn selects_smallest_component_satisfying_major() {
        let all = serde_json::json!({
            "linux": {
                "jre-legacy": [{ "manifest": { "url": "https://x/legacy.json" },
                                 "version": { "name": "8u312-b07" } }],
                "java-runtime-gamma": [{ "manifest": { "url": "https://x/gamma.json" },
                                         "version": { "name": "17.0.8" } }],
                "java-runtime-delta": [{ "manifest": { "url": "https://x/delta.json" },
                                         "version": { "name": "21.0.1" } }]
            }
        });

        // 需要 17+ → gamma（最小满足，而非 delta）
        let (component, url, version) =
            select_runtime_component(&all, "linux", 17).expect("应选中 gamma");
        assert_eq!(component, "java-runtime-gamma");
        assert_eq!(url, "https://x/gamma.json");
        assert_eq!(version, "17.0.8");

        // 需要 9+ → 17 是最小满足
        assert_eq!(
            select_runtime_component(&all, "linux", 9)
                .expect("应选中")
                .0,
            "java-runtime-gamma"
        );

        // 需要 25+ → 清单无解
        assert!(select_runtime_component(&all, "linux", 25).is_none());
        // 平台不存在 → 无解
        assert!(select_runtime_component(&all, "windows-x64", 17).is_none());
    }

    #[test]
    fn detects_java_on_current_machine() {
        // 目标机器已安装 Temurin 17/21/25；若在无 Java 环境运行则跳过
        let runtimes = detect_javas();
        if runtimes.is_empty() {
            eprintln!("跳过：本机未探测到 Java");
            return;
        }
        assert!(runtimes.iter().any(|runtime| runtime.major >= 17));
    }
}
