//! CurseForge 内容检索（经 MCIM 镜像）。
//!
//! 官方端点需要 `x-api-key`，本项目不内置，统一走 MCIM 兼容端点
//! （`api.curseforge.com` → `mod.mcimirror.top/curseforge`）。
//!
//! 响应结构与官方一致，这里只做字段映射；镜像不计作者收益，UI 保留原文链接。

use serde_json::Value;

use super::{
    html_entities, strip_markdown, urlencode, ResourceDetail, ResourceFile, ResourceKind,
    ResourceQuery, ResourceSource, ResourceSummary, ResourceVersion, SearchPage, PAGE_SIZE,
};
use crate::error::{CoreError, CoreResult};
use crate::http::HttpClient;

/// MCIM 的 CurseForge 兼容端点（不带 key；官方端点会 403）
pub const API: &str = "https://mod.mcimirror.top/curseforge/v1";
/// 描述端点的公共代理（官方同构响应；镜像未代理 `/description` 时兜底）
pub const DESCRIPTION_FALLBACK: &str = "https://api.curse.tools/v1";
/// Minecraft 在 CurseForge 的 gameId
const GAME_ID: u32 = 432;

/// CF 的 classId：模组 / 整合包 / 资源包 / 光影
fn class_id(kind: ResourceKind) -> u32 {
    match kind {
        ResourceKind::Mod => 6,
        ResourceKind::Modpack => 4471,
        ResourceKind::ResourcePack => 12,
        ResourceKind::Shader => 6552,
    }
}

/// ModLoaderType：1 Forge / 4 Fabric / 5 Quilt / 6 NeoForge
fn loader_type(loader: &str) -> Option<u32> {
    match loader.to_ascii_lowercase().as_str() {
        "forge" => Some(1),
        "fabric" => Some(4),
        "quilt" => Some(5),
        "neoforge" => Some(6),
        _ => None,
    }
}

fn loader_name(kind: u64) -> Option<&'static str> {
    match kind {
        1 => Some("forge"),
        4 => Some("fabric"),
        5 => Some("quilt"),
        6 => Some("neoforge"),
        _ => None,
    }
}

/// 相关度 → 人气；下载量 → 总下载；最新 → 最后更新
fn sort_field(sort: &str) -> &'static str {
    match sort {
        "downloads" => "6",
        "newest" | "updated" => "3",
        _ => "2",
    }
}

fn data(value: &Value) -> CoreResult<&Value> {
    value
        .get("data")
        .ok_or_else(|| CoreError::Manifest("CurseForge 响应缺少 data".into()))
}

/// 搜索项目
pub async fn search(
    http: &HttpClient,
    kind: ResourceKind,
    query: &ResourceQuery,
) -> CoreResult<Vec<ResourceSummary>> {
    Ok(search_page(http, kind, query).await?.items)
}

/// 搜索项目并带上总命中数（`pagination.totalCount`）
pub async fn search_page(
    http: &HttpClient,
    kind: ResourceKind,
    query: &ResourceQuery,
) -> CoreResult<SearchPage> {
    let mut url = format!(
        "{API}/mods/search?gameId={GAME_ID}&classId={}&index={}&pageSize={PAGE_SIZE}&sortField={}&sortOrder=desc",
        class_id(kind),
        query.offset,
        sort_field(&query.sort)
    );
    if !query.text.trim().is_empty() {
        url.push_str("&searchFilter=");
        url.push_str(&urlencode(query.text.trim()));
    }
    if let Some(version) = query.mc_version.as_deref().filter(|v| !v.trim().is_empty()) {
        url.push_str("&gameVersion=");
        url.push_str(&urlencode(version.trim()));
    }
    if let Some(loader) = query.loader.as_deref().and_then(loader_type) {
        url.push_str(&format!("&modLoaderType={loader}"));
    }
    let value: Value = http.get_json(&url).await?;
    parse_search_page(&value)
}

/// 解析 `/mods/search` 响应；`data` 为条目、`pagination.totalCount` 为总数
pub fn parse_search_page(value: &Value) -> CoreResult<SearchPage> {
    let items = parse_search(value)?;
    let total_hits = value
        .pointer("/pagination/totalCount")
        .and_then(Value::as_u64)
        .map(|total| total as usize)
        .unwrap_or(items.len());
    Ok(SearchPage { items, total_hits })
}

/// 解析 `/mods/search` 响应
pub fn parse_search(value: &Value) -> CoreResult<Vec<ResourceSummary>> {
    let list = data(value)?
        .as_array()
        .ok_or_else(|| CoreError::Manifest("CurseForge 搜索 data 不是数组".into()))?;
    Ok(list
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_u64)?;
            let name = item.get("name").and_then(Value::as_str)?.to_string();
            let categories = item
                .get("categories")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|category| category.get("name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let indexes = item
                .get("latestFilesIndexes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut versions: Vec<String> = Vec::new();
            let mut loaders: Vec<String> = Vec::new();
            for index in &indexes {
                if let Some(version) = index.get("gameVersion").and_then(Value::as_str) {
                    if !versions.iter().any(|existing| existing == version) {
                        versions.push(version.to_string());
                    }
                }
                if let Some(loader) = index.get("modLoader").and_then(Value::as_u64) {
                    if let Some(name) = loader_name(loader) {
                        if !loaders.iter().any(|existing| existing == name) {
                            loaders.push(name.to_string());
                        }
                    }
                }
            }
            Some(ResourceSummary {
                id: id.to_string(),
                name,
                summary: item
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                icon_url: item
                    .pointer("/logo/url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
                    .map(str::to_string),
                downloads: item
                    .get("downloadCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                categories,
                loaders,
                versions,
                source: ResourceSource::CurseForge,
            })
        })
        .collect())
}

/// 项目详情（描述单独一次请求；失败时留空，不影响详情主体）
pub async fn project(http: &HttpClient, project_id: &str) -> CoreResult<ResourceDetail> {
    let project_url = format!("{API}/mods/{project_id}");
    let value: Value = http.get_json(&project_url).await?;
    let description = fetch_description(http, project_id).await;
    parse_project(&value, &description)
}

/// 项目描述：主走镜像，镜像未代理该端点时退到公共代理
///
/// 描述不是关键路径，两处都失败就留空，由 UI 显示空态。
async fn fetch_description(http: &HttpClient, project_id: &str) -> String {
    let mirrored = format!("{API}/mods/{project_id}/description");
    if let Ok(value) = http.get_json::<Value>(&mirrored).await {
        return parse_description(&value);
    }
    let fallback = format!("{DESCRIPTION_FALLBACK}/mods/{project_id}/description");
    match http.get_json::<Value>(&fallback).await {
        Ok(value) => parse_description(&value),
        Err(_) => String::new(),
    }
}

/// 解析 `/description` 响应（`{ "data": "<html>" }`）为可渲染的轻量 Markdown
pub fn parse_description(value: &Value) -> String {
    value
        .get("data")
        .and_then(Value::as_str)
        .map(html_to_markdown)
        .unwrap_or_default()
}

/// 描述 HTML → app-model 能渲染的轻量 Markdown
///
/// 只处理影响阅读的结构：`h1–h4` → `# `、`li` → `- `、`p/div/br` → 换行、
/// `<img>` 保留为 `![](url)`；其余标签剥掉。CurseForge 的富文本没有统一规范，
/// 这里做「够用」的降级，避免整段挤成一行。
pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            // 未闭合的 '<' 按正文处理
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let tag = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        let lower = tag.to_ascii_lowercase();
        let closing = lower.starts_with('/');
        let name = lower
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        match name {
            "h1" | "h2" | "h3" | "h4" => {
                out.push_str("\n\n");
                if !closing {
                    out.push_str("# ");
                }
            }
            "li" => out.push_str(if closing { "\n" } else { "\n- " }),
            "br" => out.push('\n'),
            "p" | "div" | "ul" | "ol" | "tr" => out.push_str("\n\n"),
            "img" if !closing => {
                if let Some(url) = attr_value(tag, "src") {
                    out.push_str("\n\n![](");
                    out.push_str(&url);
                    out.push_str(")\n");
                }
            }
            _ => {}
        }
    }
    out.push_str(rest);
    collapse_blank_lines(&html_entities(&out))
}

/// 取标签属性值（支持双引号 / 单引号 / 无引号）
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let start = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[start..];
    let mut chars = rest.chars();
    match chars.next()? {
        quote @ ('"' | '\'') => {
            let tail = &rest[1..];
            let end = tail.find(quote)?;
            Some(tail[..end].trim().to_string())
        }
        _ => {
            let end = rest
                .find(|ch: char| ch.is_whitespace())
                .unwrap_or(rest.len());
            Some(rest[..end].trim().to_string())
        }
    }
    .filter(|value| !value.is_empty())
}

/// 压掉连续空行（3 个以上换行 → 1 个空行），并去掉首尾空白
fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            if lines.last().map(|last| last.is_empty()).unwrap_or(true) {
                continue;
            }
            lines.push("");
        } else {
            lines.push(line);
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// 解析 `/mods/{id}` 响应；`description` 为轻量 Markdown（`body` 会再剥成纯文本）
pub fn parse_project(value: &Value, description: &str) -> CoreResult<ResourceDetail> {
    let item = data(value)?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Manifest("CurseForge 项目缺少 name".into()))?;
    Ok(ResourceDetail {
        id: item
            .get("id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string())
            .unwrap_or_default(),
        slug: item
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: name.to_string(),
        summary: item
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: strip_markdown(description),
        body_markdown: description.to_string(),
        downloads: item
            .get("downloadCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        followers: 0,
        updated: item
            .get("dateModified")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        categories: item
            .get("categories")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|category| category.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        icon_url: item
            .pointer("/logo/url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        source: ResourceSource::CurseForge,
        website_url: item
            .pointer("/links/websiteUrl")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        // CurseForge 不返回 SPDX 标识，只有 license 名称；留空由 UI 省略
        license: String::new(),
        // CF 用 gameVersions 里的 "Client"/"Server" 标记
        client_side: cf_side(item, "Client"),
        server_side: cf_side(item, "Server"),
        game_versions: item
            .get("latestFilesIndexes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|row| row.get("gameVersion").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        loaders: Vec::new(),
        links: Vec::new(),
    })
}

/// 项目文件（新→旧）
pub async fn versions(
    http: &HttpClient,
    project_id: &str,
    mc_version: Option<&str>,
    loader: Option<&str>,
) -> CoreResult<Vec<ResourceVersion>> {
    let mut url = format!("{API}/mods/{project_id}/files?pageSize=50");
    if let Some(version) = mc_version.filter(|v| !v.trim().is_empty()) {
        url.push_str("&gameVersion=");
        url.push_str(&urlencode(version.trim()));
    }
    if let Some(loader) = loader.and_then(loader_type) {
        url.push_str(&format!("&modLoaderType={loader}"));
    }
    let value: Value = http.get_json(&url).await?;
    parse_versions(&value)
}

/// 解析 `/mods/{id}/files` 响应；没有 `downloadUrl` 的文件跳过
/// （作者关闭了第三方分发，镜像同样不应绕过）。
pub fn parse_versions(value: &Value) -> CoreResult<Vec<ResourceVersion>> {
    let list = data(value)?
        .as_array()
        .ok_or_else(|| CoreError::Manifest("CurseForge 文件 data 不是数组".into()))?;
    let mut items: Vec<&Value> = list.iter().collect();
    items.sort_by_key(|item| {
        std::cmp::Reverse(
            item.get("fileDate")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
    });
    Ok(items
        .into_iter()
        .filter_map(|item| {
            let url = item.get("downloadUrl").and_then(Value::as_str)?;
            let filename = item.get("fileName").and_then(Value::as_str)?;
            let game_versions = item
                .get("gameVersions")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let loaders: Vec<String> = game_versions
                .iter()
                .filter(|value| {
                    matches!(
                        value.to_ascii_lowercase().as_str(),
                        "forge" | "fabric" | "quilt" | "neoforge"
                    )
                })
                .map(|value| value.to_ascii_lowercase())
                .collect();
            // `gameVersions` 里混着加载器与 `Client`/`Server` 标记，只留真正的游戏版本，
            // 否则版本表的「游戏版本」列会显示成 Client
            let versions: Vec<String> = game_versions
                .iter()
                .filter(|value| {
                    let lower = value.to_ascii_lowercase();
                    !loaders.contains(&lower) && lower != "client" && lower != "server"
                })
                .cloned()
                .collect();
            Some(ResourceVersion {
                id: item
                    .get("id")
                    .and_then(Value::as_u64)
                    .map(|id| id.to_string())?,
                version_number: item
                    .get("displayName")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(filename)
                    .to_string(),
                game_versions: versions,
                loaders,
                files: vec![ResourceFile {
                    filename: filename.to_string(),
                    url: url.to_string(),
                    sha1: item
                        .pointer("/hashes/sha1")
                        .and_then(Value::as_str)
                        .filter(|hash| !hash.is_empty())
                        .map(str::to_string),
                    size: item.get("fileLength").and_then(Value::as_u64).unwrap_or(0),
                }],
                date_published: item
                    .get("fileDate")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                version_type: match item.get("releaseType").and_then(Value::as_u64) {
                    Some(1) => "release".to_string(),
                    Some(2) => "beta".to_string(),
                    Some(3) => "alpha".to_string(),
                    _ => String::new(),
                },
                downloads: item
                    .get("downloadCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                // CF 的 changelog 不在文件列表响应里，逐文件请求不在本轮范围
                changelog: String::new(),
            })
        })
        .collect())
}

/// CF 描述是 HTML；先剥标签，再还原常见实体
/// CurseForge 的 `gameVersions` 里带 `Client` / `Server` 标记
fn cf_side(item: &Value, flag: &str) -> String {
    let has = item
        .get("gameVersions")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().any(|row| row.as_str() == Some(flag)))
        .unwrap_or(false);
    if has {
        "required".to_string()
    } else {
        String::new()
    }
}

pub fn html_to_text(html: &str) -> String {
    let stripped = strip_markdown(html);
    html_entities(&stripped)
}

/// 项目网页地址（优先使用 CF 返回的 websiteUrl）
pub fn project_url(kind: ResourceKind, slug: &str) -> String {
    let section = match kind {
        ResourceKind::Mod => "mc-mods",
        ResourceKind::Modpack => "modpacks",
        ResourceKind::ResourcePack => "texture-packs",
        ResourceKind::Shader => "shaders",
    };
    format!("https://www.curseforge.com/minecraft/{section}/{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_search_hits_with_loaders_and_versions() {
        let value = json!({
            "data": [{
                "id": 238222,
                "name": "Just Enough Items",
                "slug": "jei",
                "summary": "查看物品配方",
                "downloadCount": 400000000u64,
                "logo": {"url": "https://media.forgecdn.net/avatars/jei.png"},
                "categories": [{"name": "Map and Information"}],
                "latestFilesIndexes": [
                    {"gameVersion": "1.20.1", "modLoader": 1},
                    {"gameVersion": "1.20.1", "modLoader": 4},
                    {"gameVersion": "1.19.2", "modLoader": 1}
                ]
            }]
        });
        let rows = parse_search(&value).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "238222");
        assert_eq!(rows[0].downloads, 400_000_000);
        assert_eq!(rows[0].source, ResourceSource::CurseForge);
        assert_eq!(rows[0].loaders, ["forge", "fabric"]);
        assert_eq!(rows[0].versions, ["1.20.1", "1.19.2"]);
        assert_eq!(rows[0].categories, ["Map and Information"]);
    }

    #[test]
    fn reads_pagination_total_count() {
        let value = json!({
            "data": [],
            "pagination": { "index": 20, "pageSize": 20, "resultCount": 0, "totalCount": 8123 }
        });
        let page = parse_search_page(&value).unwrap();
        assert_eq!(page.total_hits, 8123);
        assert!(page.items.is_empty());
    }

    #[test]
    fn parses_html_description_to_text() {
        let value = json!({
            "data": "<p>Hello <strong>world</strong></p>\n<h2>配方查看</h2>\n<p>&amp; 更多</p>"
        });
        let text = parse_description(&value);
        assert!(text.contains("Hello world"), "{text}");
        assert!(text.contains("# 配方查看"), "{text}");
        assert!(text.contains("& 更多"), "{text}");
        assert!(!text.contains('<'), "{text}");
        // 缺 data 时留空，不 panic
        assert!(parse_description(&json!({})).is_empty());
    }

    #[test]
    fn converts_html_to_readable_markdown() {
        let html = "<p>说明</p><h2>控制</h2><ul><li>一</li><li>二</li></ul>\
                    <img src=\"https://i.imgur.com/a.png\" alt=\"\"><br>尾巴";
        let markdown = html_to_markdown(html);
        assert!(markdown.contains("说明"), "{markdown}");
        assert!(markdown.contains("# 控制"), "{markdown}");
        assert!(markdown.contains("- 一"), "{markdown}");
        assert!(markdown.contains("- 二"), "{markdown}");
        assert!(
            markdown.contains("![](https://i.imgur.com/a.png)"),
            "{markdown}"
        );
        assert!(markdown.contains("尾巴"), "{markdown}");
        assert!(!markdown.contains('<'), "{markdown}");
        // 连续空行压成一个
        assert!(!markdown.contains("\n\n\n"), "{markdown}");
    }

    #[test]
    fn parses_project_and_keeps_website_url() {
        let value = json!({
            "data": {
                "id": 238222,
                "name": "Just Enough Items",
                "slug": "jei",
                "summary": "查看物品配方",
                "downloadCount": 1u64,
                "dateModified": "2024-08-01T00:00:00Z",
                "links": {"websiteUrl": "https://www.curseforge.com/minecraft/mc-mods/jei"},
                "logo": {"url": "https://media.forgecdn.net/avatars/jei.png"},
                "categories": [{"name": "Map and Information"}]
            }
        });
        let detail = parse_project(&value, "配方查看器").unwrap();
        assert_eq!(detail.title, "Just Enough Items");
        assert_eq!(detail.source, ResourceSource::CurseForge);
        assert_eq!(
            detail.website_url.as_deref(),
            Some("https://www.curseforge.com/minecraft/mc-mods/jei")
        );
        assert_eq!(detail.body, "配方查看器");
        assert_eq!(
            super::super::project_url(ResourceKind::Mod, &detail),
            "https://www.curseforge.com/minecraft/mc-mods/jei"
        );
    }

    #[test]
    fn parses_files_sorted_newest_first_and_skips_missing_url() {
        let value = json!({
            "data": [
                {
                    "id": 1,
                    "displayName": "jei-1.20.1-15.2.0.27.jar",
                    "fileName": "jei-1.20.1-15.2.0.27.jar",
                    "fileDate": "2024-01-01T00:00:00Z",
                    "fileLength": 100u64,
                    "downloadUrl": "https://edge.forgecdn.net/files/1/1/jei.jar",
                    "gameVersions": ["1.20.1", "Forge"],
                    "hashes": {"sha1": "abc"}
                },
                {
                    "id": 2,
                    "displayName": "jei-1.20.1-15.3.0.jar",
                    "fileName": "jei-1.20.1-15.3.0.jar",
                    "fileDate": "2024-06-01T00:00:00Z",
                    "fileLength": 200u64,
                    "downloadUrl": "https://edge.forgecdn.net/files/2/2/jei.jar",
                    "gameVersions": ["1.20.1", "Forge"],
                    "hashes": {"sha1": "def"}
                },
                {
                    "id": 3,
                    "displayName": "disabled.jar",
                    "fileName": "disabled.jar",
                    "fileDate": "2024-07-01T00:00:00Z",
                    "downloadUrl": null
                }
            ]
        });
        let rows = parse_versions(&value).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "2", "新版本在前");
        assert_eq!(rows[0].game_versions, ["1.20.1"]);
        assert_eq!(rows[0].loaders, ["forge"]);
        assert_eq!(rows[0].files[0].sha1.as_deref(), Some("def"));
    }

    #[test]
    fn drops_client_server_markers_from_game_versions() {
        let value = json!({
            "data": [{
                "id": 7,
                "displayName": "jei-26.3-neoforge.jar",
                "fileName": "jei-26.3-neoforge.jar",
                "fileDate": "2026-11-21T00:00:00Z",
                "downloadUrl": "https://edge.forgecdn.net/files/7/7/jei.jar",
                "gameVersions": ["Client", "NeoForge", "Server", "26.3"]
            }]
        });
        let rows = parse_versions(&value).unwrap();
        assert_eq!(rows[0].game_versions, ["26.3"]);
        assert_eq!(rows[0].loaders, ["neoforge"]);
    }

    #[test]
    fn maps_class_ids_and_sort_fields() {
        assert_eq!(class_id(ResourceKind::Mod), 6);
        assert_eq!(class_id(ResourceKind::Modpack), 4471);
        assert_eq!(class_id(ResourceKind::ResourcePack), 12);
        assert_eq!(class_id(ResourceKind::Shader), 6552);
        assert_eq!(sort_field("downloads"), "6");
        assert_eq!(sort_field("newest"), "3");
        assert_eq!(sort_field("relevance"), "2");
        assert_eq!(loader_type("NeoForge"), Some(6));
        assert_eq!(loader_type("vanilla"), None);
    }

    #[test]
    fn decodes_basic_html_entities() {
        let text = html_to_text("<p>A &amp; B&nbsp;&nbsp;C<br>D</p>");
        assert!(text.contains("A & B"));
        assert!(text.contains("C"));
        assert!(text.contains("D"));
        assert!(!text.contains("&amp;"));
        assert!(!text.contains("&nbsp;"));
    }
}
