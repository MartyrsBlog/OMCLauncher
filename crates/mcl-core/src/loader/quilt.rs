//! Quilt 加载器
//!
//! 与 Fabric 同构：meta 提供版本列表与 `profile/json`（`inheritsFrom` 指向原版），
//! 拿到后补全 Maven 下载信息即可复用通用安装流水线。

use serde_json::Value;

use crate::error::{CoreError, CoreResult};
use crate::http::HttpClient;

/// Quilt Meta API 基址
pub const QUILT_META: &str = "https://meta.quiltmc.org/v3/versions/loader";

/// 实例在游戏目录中的版本标识（与 Quilt 官方 profile 的 `id` 一致）
pub fn version_id(mc_version: &str, loader_version: &str) -> String {
    format!("quilt-loader-{loader_version}-{mc_version}")
}

/// 拉取指定游戏版本的可用加载器版本（新→旧）
pub async fn loader_versions(http: &HttpClient, mc_version: &str) -> CoreResult<Vec<String>> {
    let url = format!("{QUILT_META}/{mc_version}");
    let value: Value = http.get_json(&url).await?;
    parse_loader_versions(&value, mc_version)
}

/// 解析 `/v3/versions/loader/{mc}` 响应
///
/// 形如 `[{"loader":{"version":"0.26.0"},"intermediary":{...}}, ...]`；
/// 缺 `loader.version` 的条目跳过。
pub fn parse_loader_versions(value: &Value, mc_version: &str) -> CoreResult<Vec<String>> {
    let list = value
        .as_array()
        .ok_or_else(|| CoreError::Manifest("Quilt 加载器清单格式异常".into()))?;
    let versions: Vec<String> = list
        .iter()
        .filter_map(|item| {
            item.get("loader")
                .and_then(|loader| loader.get("version"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .collect();
    if versions.is_empty() {
        return Err(CoreError::Manifest(format!(
            "{mc_version} 没有可用的 Quilt 加载器版本"
        )));
    }
    Ok(versions)
}

/// 最新加载器版本（Quilt 列表首个即最新）
pub async fn latest(http: &HttpClient, mc_version: &str) -> CoreResult<String> {
    loader_versions(http, mc_version)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::Manifest(format!("{mc_version} 没有可用的 Quilt 加载器版本")))
}

/// 获取启动器档案 JSON（`inheritsFrom` 原版）
pub async fn profile_json(
    http: &HttpClient,
    mc_version: &str,
    loader_version: &str,
) -> CoreResult<Value> {
    let url = format!("{QUILT_META}/{mc_version}/{loader_version}/profile/json");
    let value: Value = http.get_json(&url).await?;
    if value.get("mainClass").is_none() || value.get("libraries").is_none() {
        return Err(CoreError::Manifest(format!(
            "Quilt {loader_version} 的档案 JSON 格式异常"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_loader_versions_in_order() {
        let value = json!([
            { "loader": { "version": "0.26.0" } },
            { "loader": { "version": "0.25.4" } },
            { "loader": {} },
        ]);
        assert_eq!(
            parse_loader_versions(&value, "1.20.1").unwrap(),
            vec!["0.26.0".to_string(), "0.25.4".to_string()]
        );
    }

    #[test]
    fn rejects_empty_or_malformed() {
        assert!(parse_loader_versions(&json!([]), "1.20.1").is_err());
        assert!(parse_loader_versions(&json!({ "error": "nope" }), "1.20.1").is_err());
    }

    #[test]
    fn version_id_matches_quilt_profile_id() {
        assert_eq!(version_id("1.20.1", "0.26.0"), "quilt-loader-0.26.0-1.20.1");
    }
}
