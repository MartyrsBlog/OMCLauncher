//! 资源站检索与下载
//!
//! 首期接 **Modrinth**（开放 API、无需 key、四类资源全覆盖），CurseForge 走
//! **MCIM 镜像**（见 [`curseforge`]）。设计上把「站点 API」与「文件下载」分开：
//! 本模块只负责搜索/详情/文件清单，落盘一律交给
//! [`crate::download::DownloadEngine`]，从而复用镜像、校验与断点续传。

pub mod curseforge;

use serde_json::Value;

use crate::error::{CoreError, CoreResult};
use crate::http::HttpClient;
use crate::models::FileEntry;

const MODRINTH: &str = "https://api.modrinth.com/v2";
/// 单页条数上限（Modrinth 建议 ≤ 100）
pub const PAGE_SIZE: usize = 20;

/// 内容来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResourceSource {
    #[default]
    Modrinth,
    CurseForge,
}

impl ResourceSource {
    /// 设置/索引里使用的稳定 id
    pub fn id(self) -> &'static str {
        match self {
            ResourceSource::Modrinth => "modrinth",
            ResourceSource::CurseForge => "curseforge",
        }
    }

    /// UI 展示名
    pub fn label(self) -> &'static str {
        match self {
            ResourceSource::Modrinth => "Modrinth",
            ResourceSource::CurseForge => "CurseForge",
        }
    }

    pub fn from_id(id: &str) -> Self {
        match id {
            "curseforge" => ResourceSource::CurseForge,
            _ => ResourceSource::Modrinth,
        }
    }
}

/// 资源类别（对应 Modrinth 的 `project_type`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Mod,
    Modpack,
    ResourcePack,
    Shader,
}

impl ResourceKind {
    /// Modrinth `project_type` 取值
    pub fn project_type(self) -> &'static str {
        match self {
            ResourceKind::Mod => "mod",
            ResourceKind::Modpack => "modpack",
            ResourceKind::ResourcePack => "resourcepack",
            ResourceKind::Shader => "shader",
        }
    }

    /// 实例内的落地目录（整合包走 pack 流程，返回 `None`）
    pub fn target_dir(self) -> Option<&'static str> {
        match self {
            ResourceKind::Mod => Some("mods"),
            ResourceKind::ResourcePack => Some("resourcepacks"),
            ResourceKind::Shader => Some("shaderpacks"),
            ResourceKind::Modpack => None,
        }
    }
}

/// 检索条件
#[derive(Debug, Clone, Default)]
pub struct ResourceQuery {
    pub text: String,
    pub mc_version: Option<String>,
    pub loader: Option<String>,
    /// 运行环境：`client` / `server`（空 = 不限；CurseForge 不支持该维度）
    pub env: Option<String>,
    pub offset: usize,
    /// 排序：`relevance` / `downloads` / `newest` / `updated`（Modrinth `index`）
    pub sort: String,
}

/// 一页检索结果：条目 + 站点报告的总命中数（用于页码）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPage {
    pub items: Vec<ResourceSummary>,
    /// 命中总数。站点未返回时为当前页条数（不谎报更大的范围）
    pub total_hits: usize,
}

/// 搜索结果条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSummary {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub icon_url: Option<String>,
    pub downloads: u64,
    /// 站点分类（如 `optimization` / `fabric`）
    pub categories: Vec<String>,
    /// 支持的加载器（如 `fabric` / `quilt`）
    pub loaders: Vec<String>,
    /// 兼容的游戏版本（站点按新到旧返回）
    pub versions: Vec<String>,
    /// 内容来源（决定详情/版本/安装走哪套 API）
    pub source: ResourceSource,
}

/// 项目详情（Modrinth `/project/{id}`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDetail {
    pub id: String,
    /// URL slug（Modrinth 网页地址用，如 `sodium`）
    pub slug: String,
    pub title: String,
    pub summary: String,
    /// 长描述（markdown 已剥离为纯文本；列表/搜索摘要用）
    pub body: String,
    /// 长描述原文（Markdown / HTML）。详情页按块渲染标题、段落、列表、链接。
    pub body_markdown: String,
    pub downloads: u64,
    pub followers: u64,
    pub updated: String,
    pub categories: Vec<String>,
    /// 项目图标（Modrinth CDN；与搜索结果里的 `icon_url` 同源）
    pub icon_url: Option<String>,
    /// 内容来源
    pub source: ResourceSource,
    /// 站点返回的项目网页地址（CurseForge 用；Modrinth 由 slug 拼）
    pub website_url: Option<String>,
    /// 许可证标识（如 `MIT` / `Apache-2.0`；未知为空）
    pub license: String,
    /// 客户端 / 服务端支持（Modrinth 的 `client_side` / `server_side`）
    pub client_side: String,
    pub server_side: String,
    /// 支持的游戏版本（新→旧，用于「1.14 – 1.21.9」区间文案）
    pub game_versions: Vec<String>,
    /// 支持的加载器（如 `fabric` / `forge`）
    pub loaders: Vec<String>,
    /// 页内列出的外链（源代码 / 问题追踪 / Wiki / Discord）
    pub links: Vec<ResourceLink>,
}

/// 资源详情页列出的外链
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLink {
    /// 展示名（源代码 / 问题追踪 / Wiki / Discord）
    pub label: String,
    pub url: String,
}

/// 拉取项目详情
pub async fn project(http: &HttpClient, project_id: &str) -> CoreResult<ResourceDetail> {
    let url = format!("{MODRINTH}/project/{project_id}");
    let value: Value = http.get_json(&url).await?;
    parse_project(&value)
}

/// 按来源拉取项目详情
pub async fn project_from(
    http: &HttpClient,
    source: ResourceSource,
    project_id: &str,
) -> CoreResult<ResourceDetail> {
    match source {
        ResourceSource::Modrinth => project(http, project_id).await,
        ResourceSource::CurseForge => curseforge::project(http, project_id).await,
    }
}

/// 解析 `/project/{id}` 响应
pub fn parse_project(value: &Value) -> CoreResult<ResourceDetail> {
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Manifest("Modrinth 项目缺少 title".into()))?;
    Ok(ResourceDetail {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        slug: value
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: title.to_string(),
        summary: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: strip_markdown(
            value
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        body_markdown: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        downloads: value.get("downloads").and_then(Value::as_u64).unwrap_or(0),
        followers: value.get("followers").and_then(Value::as_u64).unwrap_or(0),
        icon_url: value
            .get("icon_url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        updated: value
            .get("updated")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        categories: strings(value.get("categories")),
        source: ResourceSource::Modrinth,
        website_url: None,
        license: value
            .pointer("/license/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        client_side: value
            .get("client_side")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        server_side: value
            .get("server_side")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        game_versions: strings(value.get("game_versions")),
        loaders: strings(value.get("loaders")),
        links: modrinth_links(value),
    })
}

/// Modrinth 项目的外链（字段为空的不列出）
fn modrinth_links(value: &Value) -> Vec<ResourceLink> {
    [
        ("源代码", "source_url"),
        ("问题追踪", "issues_url"),
        ("Wiki", "wiki_url"),
        ("Discord", "discord_url"),
    ]
    .into_iter()
    .filter_map(|(label, key)| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(|url| ResourceLink {
                label: label.to_string(),
                url: url.to_string(),
            })
    })
    .collect()
}

/// 极简 markdown / HTML 剥离：只保留可读文本
///
/// Modrinth 的 `body` 是 markdown，但大量项目在里面混写原始 HTML（`<center>`、`<img>`、
/// `<h1 id=...>`）。不剥掉这些标签，详情页会直接显示成一堆源码。
pub fn strip_markdown(text: &str) -> String {
    // ① 去掉 HTML 标签（保留标签之间的文字）
    let mut without_tags = String::with_capacity(text.len());
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => without_tags.push(ch),
            _ => {}
        }
    }

    // ② 逐行清理 markdown 记号
    let mut out: Vec<String> = Vec::new();
    for line in without_tags.lines() {
        let mut line = line.trim();
        line = line.trim_start_matches('#').trim();
        line = line.trim_start_matches('>').trim();
        line = line.trim_start_matches(['-', '*', '!']).trim();

        // 图片语法 `![alt](url)` 整行丢弃
        if line.starts_with('[') && line.contains("](") && line.ends_with(')') {
            continue;
        }
        // 链接 `[文字](url)` 只留文字
        let cleaned: String = line
            .chars()
            .filter(|ch| !matches!(ch, '`' | '*' | '_'))
            .collect();
        let cleaned = match (cleaned.find('['), cleaned.find("](")) {
            (Some(start), Some(mid)) if mid > start && cleaned.ends_with(')') => {
                cleaned[start + 1..mid].to_string()
            }
            _ => cleaned,
        };
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            // 连续空行只留一个
            if out.last().map(|last| !last.is_empty()).unwrap_or(false) {
                out.push(String::new());
            }
        } else {
            out.push(trimmed.to_string());
        }
    }
    out.join(
        "
",
    )
    .trim()
    .to_string()
}

/// 某个资源的可用版本
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceVersion {
    pub id: String,
    pub version_number: String,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    pub files: Vec<ResourceFile>,
    /// 发布日期（ISO 8601，可能为空）
    pub date_published: String,
    /// 版本类型：`release` / `beta` / `alpha`（未知为空）
    pub version_type: String,
    /// 该版本下载量（CurseForge 有、Modrinth 无 → 0）
    pub downloads: u64,
    /// 更新日志原文（Modrinth 随版本返回；CurseForge 需单文件请求，通常为空）
    pub changelog: String,
}

/// 可下载文件
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceFile {
    pub filename: String,
    pub url: String,
    pub sha1: Option<String>,
    pub size: u64,
}

impl ResourceFile {
    /// 交给下载引擎的条目（`path` 仅用于日志/缓存键）
    pub fn entry(&self) -> FileEntry {
        FileEntry {
            path: self.filename.clone(),
            url: self.url.clone(),
            sha1: self.sha1.clone(),
            size: self.size,
        }
    }
}

/// Modrinth facets：每个元素是一个「或」组，组之间是「与」
fn facets(kind: ResourceKind, query: &ResourceQuery) -> String {
    // facets 必须是「数组的数组」：[["project_type:mod"],["versions:1.20.1"]]
    let mut facets = vec![format!("[\"project_type:{}\"]", kind.project_type())];
    if let Some(version) = query.mc_version.as_deref().filter(|v| !v.trim().is_empty()) {
        facets.push(format!("[\"versions:{}\"]", version.trim()));
    }
    if let Some(loader) = query.loader.as_deref().filter(|v| !v.trim().is_empty()) {
        facets.push(format!("[\"categories:{}\"]", loader.trim()));
    }
    // 环境：required 与 optional 都算兼容
    match query.env.as_deref() {
        Some("client") => {
            facets.push("[\"client_side:required\",\"client_side:optional\"]".to_string())
        }
        Some("server") => {
            facets.push("[\"server_side:required\",\"server_side:optional\"]".to_string())
        }
        _ => {}
    }
    format!("[{}]", facets.join(","))
}

/// 搜索资源（按相关度/下载量，由 Modrinth 决定排序）
pub async fn search(
    http: &HttpClient,
    kind: ResourceKind,
    query: &ResourceQuery,
) -> CoreResult<Vec<ResourceSummary>> {
    Ok(search_page(http, kind, query).await?.items)
}

/// 搜索资源并带上总命中数（页码分页用）
pub async fn search_page(
    http: &HttpClient,
    kind: ResourceKind,
    query: &ResourceQuery,
) -> CoreResult<SearchPage> {
    let facets = facets(kind, query);
    let sort = if query.sort.is_empty() {
        "relevance"
    } else {
        query.sort.as_str()
    };
    let url = format!(
        "{MODRINTH}/search?query={}&limit={PAGE_SIZE}&offset={}&index={sort}&facets={}",
        urlencode(&query.text),
        query.offset,
        urlencode(&facets)
    );
    let value: Value = http.get_json(&url).await?;
    parse_search_page(&value)
}

/// 按来源搜索（Modrinth 官方 + MCIM 兜底；CurseForge 只走 MCIM）
pub async fn search_from(
    http: &HttpClient,
    source: ResourceSource,
    kind: ResourceKind,
    query: &ResourceQuery,
) -> CoreResult<Vec<ResourceSummary>> {
    Ok(search_from_page(http, source, kind, query).await?.items)
}

/// 按来源搜索并带上总命中数
pub async fn search_from_page(
    http: &HttpClient,
    source: ResourceSource,
    kind: ResourceKind,
    query: &ResourceQuery,
) -> CoreResult<SearchPage> {
    match source {
        ResourceSource::Modrinth => search_page(http, kind, query).await,
        ResourceSource::CurseForge => curseforge::search_page(http, kind, query).await,
    }
}

/// 某个项目在指定 MC 版本/加载器下的版本列表（新→旧）
pub async fn versions(
    http: &HttpClient,
    project_id: &str,
    mc_version: Option<&str>,
    loader: Option<&str>,
) -> CoreResult<Vec<ResourceVersion>> {
    let mut url = format!("{MODRINTH}/project/{project_id}/version");
    let mut params = Vec::new();
    if let Some(version) = mc_version {
        params.push(format!(
            "game_versions={}",
            urlencode(&format!("[\"{version}\"]"))
        ));
    }
    if let Some(loader) = loader {
        params.push(format!("loaders={}", urlencode(&format!("[\"{loader}\"]"))));
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    let value: Value = http.get_json(&url).await?;
    parse_versions(&value)
}

/// 按来源取版本列表
pub async fn versions_from(
    http: &HttpClient,
    source: ResourceSource,
    project_id: &str,
    mc_version: Option<&str>,
    loader: Option<&str>,
) -> CoreResult<Vec<ResourceVersion>> {
    match source {
        ResourceSource::Modrinth => versions(http, project_id, mc_version, loader).await,
        ResourceSource::CurseForge => {
            curseforge::versions(http, project_id, mc_version, loader).await
        }
    }
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 解析 `/search` 响应
/// 解析 `/search` 响应（条目）
pub fn parse_search(value: &Value) -> CoreResult<Vec<ResourceSummary>> {
    let hits = value
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Manifest("Modrinth 搜索响应缺少 hits".into()))?;
    Ok(hits
        .iter()
        .filter_map(|hit| {
            Some(ResourceSummary {
                id: hit.get("project_id").and_then(Value::as_str)?.to_string(),
                name: hit.get("title").and_then(Value::as_str)?.to_string(),
                summary: hit
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                icon_url: hit
                    .get("icon_url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                downloads: hit.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                categories: string_list(hit, "categories"),
                loaders: string_list(hit, "loaders"),
                versions: string_list(hit, "versions"),
                source: ResourceSource::Modrinth,
            })
        })
        .collect())
}

/// 解析 `/search` 响应（条目 + 总命中数）
pub fn parse_search_page(value: &Value) -> CoreResult<SearchPage> {
    let items = parse_search(value)?;
    // 缺字段时按「本页即全部」处理，避免页码凭空放大
    let total_hits = value
        .get("total_hits")
        .and_then(Value::as_u64)
        .map(|total| total as usize)
        .unwrap_or(items.len());
    Ok(SearchPage { items, total_hits })
}

/// 解析 `/project/{id}/version` 响应；只保留首个主文件
pub fn parse_versions(value: &Value) -> CoreResult<Vec<ResourceVersion>> {
    let list = value
        .as_array()
        .ok_or_else(|| CoreError::Manifest("Modrinth 版本响应格式异常".into()))?;
    Ok(list
        .iter()
        .filter_map(|item| {
            let files = item
                .get("files")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .filter(|file| file.get("primary").and_then(Value::as_bool).unwrap_or(true))
                        .filter_map(|file| {
                            Some(ResourceFile {
                                filename: file.get("filename")?.as_str()?.to_string(),
                                url: file.get("url")?.as_str()?.to_string(),
                                sha1: file
                                    .pointer("/hashes/sha1")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                                size: file.get("size").and_then(Value::as_u64).unwrap_or(0),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if files.is_empty() {
                return None;
            }
            Some(ResourceVersion {
                id: item.get("id").and_then(Value::as_str)?.to_string(),
                version_number: item
                    .get("version_number")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                game_versions: strings(item.get("game_versions")),
                loaders: strings(item.get("loaders")),
                files,
                date_published: item
                    .get("date_published")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                version_type: item
                    .get("version_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                downloads: item.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                changelog: item
                    .get("changelog")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect())
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 把资源文件下载到实例的对应目录，返回落盘路径
///
/// - 目录由 [`ResourceKind::target_dir`] 决定（整合包不支持，需走 pack 流程）；
/// - 文件名做净化（拒绝路径分隔符与 `..`），避免远端文件名穿越目录；
/// - 已存在且 SHA-1 一致时由下载引擎直接跳过。
pub async fn install_file(
    engine: &crate::download::DownloadEngine,
    kind: ResourceKind,
    instance_dir: &std::path::Path,
    file: &ResourceFile,
) -> CoreResult<std::path::PathBuf> {
    let dir_name = kind
        .target_dir()
        .ok_or_else(|| CoreError::Unsupported("整合包请走 pack 安装流程（.mrpack）".into()))?;
    install_file_to(engine, &instance_dir.join(dir_name), file).await
}

/// 安装到显式目录（用户在安装弹窗里选择的路径）。
///
/// 与 [`install_file`] 的区别只有目标目录：不再按类别拼 `mods` / `resourcepacks`。
pub async fn install_file_to(
    engine: &crate::download::DownloadEngine,
    target_dir: &std::path::Path,
    file: &ResourceFile,
) -> CoreResult<std::path::PathBuf> {
    let safe_name = sanitize_filename(&file.filename)?;
    // 用户可能选一个新的空目录，先建出来
    std::fs::create_dir_all(target_dir).map_err(|error| {
        CoreError::Unsupported(format!(
            "无法创建安装目录 {}：{error}",
            target_dir.display()
        ))
    })?;
    let dest = target_dir.join(&safe_name);
    engine.fetch(&file.entry(), &dest).await?;
    Ok(dest)
}

/// 文件名净化：只保留最后一段，拒绝空名与 `..`
pub fn sanitize_filename(name: &str) -> CoreResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains(['/', '\\'])
        || trimmed == ".."
        || trimmed.starts_with('.')
    {
        return Err(CoreError::Unsupported(format!("非法的资源文件名：{name}")));
    }
    Ok(trimmed.to_string())
}

/// 最小百分号编码：保留 RFC 3986 非保留字符
pub fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 项目网页地址：CurseForge 用站点返回的 websiteUrl（缺 slug 时按分类拼），
/// Modrinth 用分类前缀 + slug（缺 slug 时退回 id）
pub fn project_url(kind: ResourceKind, detail: &ResourceDetail) -> String {
    if detail.source == ResourceSource::CurseForge {
        if let Some(url) = detail.website_url.as_deref().filter(|url| !url.is_empty()) {
            return url.to_string();
        }
        return curseforge::project_url(kind, detail.slug.as_str());
    }
    let path = if detail.slug.is_empty() {
        detail.id.as_str()
    } else {
        detail.slug.as_str()
    };
    match kind {
        ResourceKind::Mod => format!("https://modrinth.com/mod/{path}"),
        ResourceKind::Modpack => format!("https://modrinth.com/modpack/{path}"),
        ResourceKind::ResourcePack => format!("https://modrinth.com/resourcepack/{path}"),
        ResourceKind::Shader => format!("https://modrinth.com/shader/{path}"),
    }
}

/// 还原 HTML 常见实体（CurseForge 描述是 HTML，剥标签后会剩下这些）
pub fn html_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_search_hits() {
        let value = json!({
            "hits": [{
                "project_id": "sodium",
                "title": "Sodium",
                "description": "现代渲染优化",
                "icon_url": "https://cdn.modrinth.com/a.png",
                "downloads": 123456,
                "categories": ["optimization", "fabric"],
                "loaders": ["fabric", "quilt"],
                "versions": ["1.20.1", "1.20"]
            }]
        });
        let rows = parse_search(&value).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "sodium");
        assert_eq!(rows[0].downloads, 123456);
        assert_eq!(
            rows[0].icon_url.as_deref(),
            Some("https://cdn.modrinth.com/a.png")
        );
        assert_eq!(rows[0].categories, ["optimization", "fabric"]);
        assert_eq!(rows[0].loaders, ["fabric", "quilt"]);
        assert_eq!(rows[0].versions, ["1.20.1", "1.20"]);
    }

    #[test]
    fn builds_loader_and_env_facets() {
        let query = ResourceQuery {
            text: "sodium".into(),
            mc_version: Some("1.20.1".into()),
            loader: Some("fabric".into()),
            env: Some("client".into()),
            offset: 40,
            sort: "downloads".into(),
        };
        assert_eq!(
            facets(ResourceKind::Mod, &query),
            "[[\"project_type:mod\"],[\"versions:1.20.1\"],[\"categories:fabric\"],[\"client_side:required\",\"client_side:optional\"]]"
        );

        let query = ResourceQuery {
            env: Some("server".into()),
            ..ResourceQuery::default()
        };
        assert_eq!(
            facets(ResourceKind::Shader, &query),
            "[[\"project_type:shader\"],[\"server_side:required\",\"server_side:optional\"]]"
        );
    }

    #[test]
    fn reads_total_hits_for_pager() {
        let value = json!({ "hits": [], "total_hits": 79910u64 });
        let page = parse_search_page(&value).unwrap();
        assert_eq!(page.total_hits, 79910);
        assert!(page.items.is_empty());

        // 缺字段时按本页条数兜底，不凭空放大页数
        let value = json!({ "hits": [] });
        assert_eq!(parse_search_page(&value).unwrap().total_hits, 0);
    }

    #[test]
    fn parses_versions_and_prefers_primary_file() {
        let value = json!([{
            "id": "v1",
            "version_number": "1.2.3",
            "game_versions": ["1.20.1"],
            "loaders": ["fabric"],
            "files": [
                {"filename": "a.jar", "url": "https://cdn/a.jar", "size": 10, "primary": false,
                 "hashes": {"sha1": "aa"}},
                {"filename": "b.jar", "url": "https://cdn/b.jar", "size": 20, "primary": true,
                 "hashes": {"sha1": "bb"}}
            ]
        }]);
        let rows = parse_versions(&value).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].files.len(), 1);
        assert_eq!(rows[0].files[0].filename, "b.jar");
        assert_eq!(rows[0].files[0].sha1.as_deref(), Some("bb"));
    }

    #[test]
    fn skips_versions_without_files() {
        let value = json!([{ "id": "v2", "version_number": "1.0", "files": [] }]);
        assert!(parse_versions(&value).unwrap().is_empty());
    }

    #[test]
    fn encodes_query_and_facets() {
        assert_eq!(urlencode("c++ 优化"), "c%2B%2B%20%E4%BC%98%E5%8C%96");
        assert_eq!(
            urlencode("[[\"project_type:mod\"]]"),
            "%5B%5B%22project_type%3Amod%22%5D%5D"
        );
    }

    #[test]
    fn parses_project_and_strips_markdown() {
        let value = json!({
            "id": "AANobbMI",
            "slug": "sodium",
            "title": "Sodium",
            "description": "现代渲染优化",
            "body": "# Sodium\n\n**更快**的渲染引擎，见 [文档](https://example.com)。\n\n- 特性一\n- 特性二",
            "downloads": 259256196,
            "followers": 12345,
            "updated": "2024-05-01T12:00:00Z",
            "categories": ["optimization", "fabric"]
        });
        let detail = parse_project(&value).unwrap();
        assert_eq!(detail.title, "Sodium");
        assert_eq!(detail.followers, 12345);
        assert!(detail.body.contains("更快的渲染引擎"));
        assert!(detail.body.contains("文档"));
        assert!(!detail.body.contains('*'));
        assert!(!detail.body.contains('#'));
        assert_eq!(detail.categories.len(), 2);
    }

    #[test]
    fn sanitizes_filenames() {
        assert!(sanitize_filename("sodium-0.5.8.jar").is_ok());
        assert!(sanitize_filename("../../etc/passwd").is_err());
        assert!(sanitize_filename("dir/a.jar").is_err());
        assert!(sanitize_filename("   ").is_err());
    }

    #[test]
    fn kind_maps_to_target_dir() {
        assert_eq!(ResourceKind::Mod.target_dir(), Some("mods"));
        assert_eq!(ResourceKind::Shader.target_dir(), Some("shaderpacks"));
        assert_eq!(ResourceKind::Modpack.target_dir(), None);
        assert_eq!(ResourceKind::ResourcePack.project_type(), "resourcepack");
    }
}
