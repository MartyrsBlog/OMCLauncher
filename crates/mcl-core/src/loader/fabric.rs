//! Fabric 加载器安装
//!
//! 流程（全部经 `DownloadSource` 自动映射到 BMCLAPI `/fabric-meta`、`/maven`）：
//! 1. `GET /v2/versions/loader/{mc}` → 加载器版本列表（取最新 stable）
//! 2. `GET /v2/versions/loader/{mc}/{loader}/profile/json` → 启动器档案 JSON
//!    （`inheritsFrom` 原版、含 `mainClass`/`arguments.jvm`/Maven 类库）
//! 3. 与原版 JSON 合并（[`merge_inherited`]）后，把只有 Maven 坐标、没有
//!    `downloads` 的类库补全为下载条目（[`inject_maven_downloads`]）
//! 4. 交给 [`VersionService::install_merged`] 走通用安装流水线

use serde_json::{json, Value};

use crate::error::{CoreError, CoreResult};
use crate::http::HttpClient;

/// Fabric Meta API 基址（BMCLAPI 映射为 `/fabric-meta`）
pub const FABRIC_META: &str = "https://meta.fabricmc.net/v2/versions/loader";

/// Fabric 支持的游戏版本清单（`/v2/versions/game`）
///
/// 用于在安装前判断「该 MC 版本是否有 Fabric」——直接请求
/// `/v2/versions/loader/{mc}` 在不支持的版本上返回 HTTP 400，
/// 会被误报成网络故障。
pub const FABRIC_GAME_VERSIONS: &str = "https://meta.fabricmc.net/v2/versions/game";

/// 加载器版本条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricLoaderVersion {
    pub version: String,
    pub stable: bool,
}

/// 实例在游戏目录中的版本标识（社区惯例命名，与原版 jar 分离存放）
pub fn version_id(mc_version: &str, loader_version: &str) -> String {
    format!("fabric-loader-{loader_version}-{mc_version}")
}

/// 拉取指定游戏版本的可用加载器列表（新→旧）
pub async fn loader_versions(
    http: &HttpClient,
    mc_version: &str,
) -> CoreResult<Vec<FabricLoaderVersion>> {
    let url = format!("{FABRIC_META}/{mc_version}");
    let value: Value = http.get_json(&url).await?;
    parse_loader_versions(&value, mc_version)
}

/// 拉取 Fabric 官方支持的全部游戏版本（新→旧）
pub async fn game_versions(http: &HttpClient) -> CoreResult<Vec<String>> {
    let value: Value = http.get_json(FABRIC_GAME_VERSIONS).await?;
    parse_game_versions(&value)
}

/// 解析 `/v2/versions/game` 响应
///
/// 形如 `[{"version":"1.21.1","stable":true}, ...]`；缺字段的条目跳过。
pub fn parse_game_versions(value: &Value) -> CoreResult<Vec<String>> {
    let list = value
        .as_array()
        .ok_or_else(|| CoreError::Manifest("Fabric 游戏版本清单格式异常".into()))?;
    let versions: Vec<String> = list
        .iter()
        .filter_map(|item| item.get("version").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if versions.is_empty() {
        return Err(CoreError::Manifest("Fabric 游戏版本清单为空".into()));
    }
    Ok(versions)
}

/// 最新 stable 版本（无 stable 时取列表首个；列表为空则报错）
pub async fn latest_stable(http: &HttpClient, mc_version: &str) -> CoreResult<String> {
    let versions = loader_versions(http, mc_version).await?;
    versions
        .iter()
        .find(|item| item.stable)
        .or_else(|| versions.first())
        .map(|item| item.version.clone())
        .ok_or_else(|| CoreError::Manifest(format!("{mc_version} 没有可用的 Fabric 加载器版本")))
}

/// 获取启动器档案 JSON（`inheritsFrom` 原版）
pub async fn profile_json(
    http: &HttpClient,
    mc_version: &str,
    loader_version: &str,
) -> CoreResult<Value> {
    let url = format!("{FABRIC_META}/{mc_version}/{loader_version}/profile/json");
    let value: Value = http.get_json(&url).await?;
    if value.get("mainClass").is_none() || value.get("libraries").is_none() {
        return Err(CoreError::Manifest(format!(
            "Fabric {loader_version} 的档案 JSON 格式异常"
        )));
    }
    Ok(value)
}

/// 解析 `/v2/versions/loader/{mc}` 响应（新→旧）
pub fn parse_loader_versions(
    value: &Value,
    mc_version: &str,
) -> CoreResult<Vec<FabricLoaderVersion>> {
    let list = value
        .as_array()
        .ok_or_else(|| CoreError::Manifest("Fabric 加载器列表格式异常".into()))?;

    let mut versions = Vec::with_capacity(list.len());
    for item in list {
        let Some(loader) = item.get("loader") else {
            continue;
        };
        let Some(version) = loader.get("version").and_then(Value::as_str) else {
            continue;
        };
        versions.push(FabricLoaderVersion {
            version: version.to_string(),
            stable: loader
                .get("stable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }

    if versions.is_empty() {
        return Err(CoreError::Manifest(format!(
            "{mc_version} 没有可用的 Fabric 加载器版本"
        )));
    }
    Ok(versions)
}

/// Maven 坐标 → 仓库内相对路径（含文件名）
///
/// `net.fabricmc:fabric-loader:0.16.9` →
/// `net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar`
/// 支持可选的 classifier 第 4 段：`group:artifact:version:classifier`
pub fn maven_relative_path(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let (group, artifact, version) = (parts[0], parts[1], parts[2]);
    if group.is_empty() || artifact.is_empty() || version.is_empty() {
        return None;
    }

    let file = match parts.get(3) {
        Some(classifier) if !classifier.is_empty() => {
            format!("{artifact}-{version}-{classifier}.jar")
        }
        _ => format!("{artifact}-{version}.jar"),
    };
    Some(format!(
        "{}/{artifact}/{version}/{file}",
        group.replace('.', "/")
    ))
}

/// 为缺少 `downloads` 的 Maven 类库（Fabric 档案形态 `{name, url}`）补全下载信息。
///
/// 产物：`downloads.artifact = { path, url, size: 0 }`。
/// `size = 0` 表示未知大小——下载引擎对此走单流直连（不做 Range/分块）；
/// `sha1` 缺失时跳过校验（Fabric Maven 无集中清单，完整性依赖 HTTPS）。
///
/// 已有 `downloads.artifact` 的条目（原版类库）保持不变；返回补全条数。
pub fn inject_maven_downloads(merged: &mut Value) -> usize {
    let Some(libraries) = merged.get_mut("libraries").and_then(Value::as_array_mut) else {
        return 0;
    };

    let mut injected = 0;
    for library in libraries.iter_mut() {
        if library.pointer("/downloads/artifact").is_some() {
            continue;
        }
        let Some(name) = library.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(base) = library.get("url").and_then(Value::as_str) else {
            continue;
        };
        let Some(path) = maven_relative_path(name) else {
            continue;
        };

        let url = if base.ends_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        };
        let Some(object) = library.as_object_mut() else {
            continue;
        };
        object.insert(
            "downloads".to_string(),
            json!({ "artifact": { "path": path, "url": url, "size": 0 } }),
        );
        injected += 1;
    }
    injected
}

/// 解析 Maven `.sha1` 侧车文本（纯十六进制，可能带文件名后缀列）
///
/// 仅接受 40 位十六进制摘要；其余（HTML 错误页、空文件等）返回 `None`。
pub fn parse_sha1_sidecar(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?.to_lowercase();
    (hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit())).then_some(hash)
}

/// 为缺少校验的 Maven 类库并发获取 `.sha1` 侧车。
///
/// `/maven` 不代理侧车（404）——`HttpClient` 的源 failover 天然处理（官方源
/// 优先命中），单个失败仅跳过该条（完整性退回 HTTPS 信任）。返回补全条数。
pub async fn fill_maven_checksums(http: &std::sync::Arc<HttpClient>, merged: &mut Value) -> usize {
    let Some(libraries) = merged.get_mut("libraries").and_then(Value::as_array_mut) else {
        return 0;
    };

    let targets: Vec<(usize, String)> = libraries
        .iter()
        .enumerate()
        .filter_map(|(index, library)| {
            let artifact = library.pointer("/downloads/artifact")?;
            if artifact.get("sha1").and_then(Value::as_str).is_some() {
                return None;
            }
            let url = artifact.get("url").and_then(Value::as_str)?;
            Some((index, format!("{url}.sha1")))
        })
        .collect();
    if targets.is_empty() {
        return 0;
    }

    let mut set = tokio::task::JoinSet::new();
    for (index, url) in targets {
        let http = http.clone();
        set.spawn(async move {
            let text = http.get_text(&url).await.ok()?;
            parse_sha1_sidecar(&text).map(|hash| (index, hash))
        });
    }

    let mut filled = 0;
    while let Some(joined) = set.join_next().await {
        let Ok(Some((index, hash))) = joined else {
            continue;
        };
        if let Some(artifact) = libraries[index].pointer_mut("/downloads/artifact") {
            artifact["sha1"] = Value::String(hash);
            filled += 1;
        }
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_id_matches_community_naming() {
        assert_eq!(
            version_id("1.20.1", "0.16.9"),
            "fabric-loader-0.16.9-1.20.1"
        );
    }

    #[test]
    fn maven_relative_path_handles_plain_and_classifier_coordinates() {
        assert_eq!(
            maven_relative_path("net.fabricmc:fabric-loader:0.16.9").as_deref(),
            Some("net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar")
        );
        // 版本号含 '+'（Mixin 常见形态）
        assert_eq!(
            maven_relative_path("net.fabricmc:sponge-mixin:0.15.4+mixin.0.8.7").as_deref(),
            Some(
                "net/fabricmc/sponge-mixin/0.15.4+mixin.0.8.7/sponge-mixin-0.15.4+mixin.0.8.7.jar"
            )
        );
        // classifier 形式
        assert_eq!(
            maven_relative_path("org.lwjgl:lwjgl:3.3.1:natives-linux").as_deref(),
            Some("org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1-natives-linux.jar")
        );
        // 非法坐标
        assert_eq!(maven_relative_path("broken"), None);
        assert_eq!(maven_relative_path("a::1.0"), None);
    }

    #[test]
    fn inject_maven_downloads_fills_fabric_libraries_only() {
        let mut merged = json!({
            "libraries": [
                // 原版形态：已有 downloads，保持不变
                {
                    "name": "org.ow2.asm:asm:9.5",
                    "downloads": { "artifact": { "path": "org/ow2/asm/asm/9.5/asm-9.5.jar",
                                                  "url": "https://libraries.minecraft.net/org/ow2/asm/asm/9.5/asm-9.5.jar",
                                                  "sha1": "abc", "size": 123 } }
                },
                // Fabric 形态：仅 name + url
                { "name": "net.fabricmc:fabric-loader:0.16.9", "url": "https://maven.fabricmc.net/" },
                // 无 url：跳过（无法定位仓库）
                { "name": "net.fabricmc:intermediary:1.20.1" }
            ]
        });

        assert_eq!(inject_maven_downloads(&mut merged), 1);

        let libs = merged["libraries"].as_array().expect("libraries");
        // 原版条目未被改动
        assert_eq!(libs[0]["downloads"]["artifact"]["sha1"], "abc");
        // Fabric 条目补全出正确的 url 与 path
        assert_eq!(
            libs[1]["downloads"]["artifact"]["url"],
            "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
        );
        assert_eq!(
            libs[1]["downloads"]["artifact"]["path"],
            "net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
        );
        assert_eq!(libs[1]["downloads"]["artifact"]["size"], 0);
        // 无 url 条目保持原样
        assert!(libs[2].pointer("/downloads/artifact").is_none());
    }

    #[test]
    fn parse_loader_versions_reads_real_shape() {
        // 取自 meta.fabricmc.net 的真实响应形态（截断）
        let value = json!([
            {
                "loader": { "separator": ".", "build": 9, "maven": "net.fabricmc:fabric-loader:0.16.9", "version": "0.16.9", "stable": true },
                "intermediary": { "maven": "net.fabricmc:intermediary:1.20.1", "version": "1.20.1", "stable": true },
                "launcherMeta": { "version": 2, "mainClass": { "client": "net.fabricmc.loader.impl.launch.knot.KnotClient" } }
            },
            {
                "loader": { "version": "0.16.10-beta.1", "stable": false },
                "intermediary": { "version": "1.20.1" }
            }
        ]);

        let versions = parse_loader_versions(&value, "1.20.1").expect("解析");
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, "0.16.9");
        assert!(versions[0].stable);
        assert!(!versions[1].stable);
    }

    #[test]
    fn parse_sha1_sidecar_accepts_valid_and_rejects_junk() {
        assert_eq!(
            parse_sha1_sidecar("ff9e65cffca4a67f31523e1807fe0855940fcbfa\n").as_deref(),
            Some("ff9e65cffca4a67f31523e1807fe0855940fcbfa")
        );
        // 带文件名后缀列（部分仓库形态）与大小写混合
        assert_eq!(
            parse_sha1_sidecar("FF9E65CFFCA4A67F31523E1807FE0855940FCBFA  artifact.jar").as_deref(),
            Some("ff9e65cffca4a67f31523e1807fe0855940fcbfa")
        );
        // 错误页/空内容/长度错误 → 拒绝
        assert_eq!(parse_sha1_sidecar("Not Found"), None);
        assert_eq!(parse_sha1_sidecar(""), None);
        assert_eq!(parse_sha1_sidecar("abc123"), None);
    }

    #[test]
    fn parse_loader_versions_rejects_empty_and_malformed() {
        assert!(parse_loader_versions(&json!([]), "1.20.1").is_err());
        assert!(parse_loader_versions(&json!({ "error": "nope" }), "1.20.1").is_err());
        // 条目缺 loader.version 的跳过，全空时报错
        assert!(parse_loader_versions(&json!([{ "loader": {} }]), "1.20.1").is_err());
    }

    #[test]
    fn parse_game_versions_keeps_order_and_skips_bad_entries() {
        let value = json!([
            { "version": "1.21.1", "stable": true },
            { "version": "1.21.2-pre1", "stable": false },
            { "stable": true }
        ]);
        let versions = parse_game_versions(&value).expect("解析");
        assert_eq!(versions, vec!["1.21.1", "1.21.2-pre1"]);
    }

    #[test]
    fn parse_game_versions_rejects_empty() {
        assert!(parse_game_versions(&json!([])).is_err());
        assert!(parse_game_versions(&json!({ "error": "nope" })).is_err());
    }
}
