//! 下载引擎
//!
//! - **多源策略**：默认 failover，竞速仅限官方源或用户显式开启（ 修订项，
//!   避免对 BMCLAPI 等公益镜像放大负载）
//! - **分块**：单文件 HTTP Range 并行，块大小自适应
//! - **续传**：`.part` 临时文件 + 已下载区间记录

pub mod engine;

pub use engine::{DownloadEngine, FetchOutcome, FetchStats};

use crate::models::FileEntry;

/// 下载任务标识
pub type TaskId = u64;

/// 下载源抽象
pub trait DownloadSource: Send + Sync {
    fn name(&self) -> &'static str;

    /// 将官方 URL 映射为该源的实际 URL
    fn resolve(&self, official_url: &str) -> String;

    /// 优先级，数值越小越优先
    fn priority(&self) -> u8 {
        100
    }
}

/// 官方源
pub struct OfficialSource;

impl DownloadSource for OfficialSource {
    fn name(&self) -> &'static str {
        "官方"
    }

    fn resolve(&self, official_url: &str) -> String {
        official_url.to_string()
    }

    fn priority(&self) -> u8 {
        0
    }
}

/// BMCLAPI 镜像源：把 Mojang 官方域名映射到镜像域名。
///
/// 依据 BMCLAPI 使用规范：默认仅故障切换、标识来源。
pub struct BmclapiSource {
    pub base: String,
}

impl Default for BmclapiSource {
    fn default() -> Self {
        Self {
            base: "https://bmclapi2.bangbang93.com".to_string(),
        }
    }
}

impl BmclapiSource {
    /// 官方域名 → 镜像域名的前缀替换表
    const HOST_MAP: &'static [(&'static str, &'static str)] = &[
        (
            "https://piston-meta.mojang.com",
            "https://bmclapi2.bangbang93.com",
        ),
        (
            "https://piston-data.mojang.com",
            "https://bmclapi2.bangbang93.com",
        ),
        (
            "https://launchermeta.mojang.com",
            "https://bmclapi2.bangbang93.com",
        ),
        (
            "https://libraries.minecraft.net",
            "https://bmclapi2.bangbang93.com/maven",
        ),
        (
            "https://resources.download.minecraft.net",
            "https://bmclapi2.bangbang93.com/assets",
        ),
        // Fabric：maven 与 meta 两个域名（BMCLAPI 文档映射表）
        (
            "https://maven.fabricmc.net",
            "https://bmclapi2.bangbang93.com/maven",
        ),
        (
            "https://meta.fabricmc.net",
            "https://bmclapi2.bangbang93.com/fabric-meta",
        ),
        // Forge / NeoForge：官方 maven 映射到镜像的 /maven
        // `neoforged/releases` 必须排在裸域名之前（前缀匹配按顺序进行）
        (
            "https://maven.neoforged.net/releases",
            "https://bmclapi2.bangbang93.com/maven",
        ),
        (
            "https://maven.neoforged.net",
            "https://bmclapi2.bangbang93.com/maven",
        ),
        (
            "https://maven.minecraftforge.net",
            "https://bmclapi2.bangbang93.com/maven",
        ),
    ];
}

impl DownloadSource for BmclapiSource {
    fn name(&self) -> &'static str {
        "BMCLAPI"
    }

    fn resolve(&self, official_url: &str) -> String {
        for (official, mirror_path) in Self::HOST_MAP {
            if let Some(rest) = official_url.strip_prefix(*official) {
                // base 与映射后缀拼接
                let suffix = mirror_path
                    .strip_prefix("https://bmclapi2.bangbang93.com")
                    .unwrap_or("");
                return format!("{}{}{}", self.base, suffix, rest);
            }
        }
        official_url.to_string()
    }

    fn priority(&self) -> u8 {
        10
    }
}

/// MCIM 内容镜像：只映射 Modrinth / CurseForge 的 API 与文件 CDN。
///
/// 与 [`BmclapiSource`] 分开：BMCLAPI 覆盖 Mojang 游戏文件，MCIM 覆盖模组站内容，
/// 两者域名不重叠，未知 URL 原样返回。作为最后一级故障切换源（公益服务，
/// 见 <https://www.mcimirror.top/guide/rules/policy>）。
pub struct McimSource {
    pub base: String,
}

impl Default for McimSource {
    fn default() -> Self {
        Self {
            base: "https://mod.mcimirror.top".to_string(),
        }
    }
}

impl DownloadSource for McimSource {
    fn name(&self) -> &'static str {
        "MCIM"
    }

    fn resolve(&self, official_url: &str) -> String {
        // 顺序敏感：api.curseforge.com 必须先于 edge.forgecdn.net 判断
        const HOST_MAP: &[(&str, &str)] = &[
            (
                "https://api.modrinth.com/v2",
                "https://mod.mcimirror.top/modrinth/v2",
            ),
            (
                "https://api.curseforge.com/v1",
                "https://mod.mcimirror.top/curseforge/v1",
            ),
            // edge.forgecdn.net 走镜像 302；mediafilez.forgecdn.net 明确不镜像
            ("https://edge.forgecdn.net", "https://mod.mcimirror.top"),
            ("https://cdn.modrinth.com", "https://mod.mcimirror.top"),
        ];
        for (official, mirror) in HOST_MAP {
            if let Some(rest) = official_url.strip_prefix(official) {
                let suffix = mirror
                    .strip_prefix("https://mod.mcimirror.top")
                    .unwrap_or("");
                return format!("{}{}{}", self.base, suffix, rest);
            }
        }
        official_url.to_string()
    }

    fn priority(&self) -> u8 {
        30
    }
}

/// 单个文件的下载任务
#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub id: TaskId,
    pub file: FileEntry,
    /// 分块大小
    pub chunk_size: u64,
}

/// 将文件切分为 HTTP Range 区间（**包含端点**）。
///
/// 边界语义：
/// - `total == 0` → 返回空（空文件无需分块）
/// - `chunk == 0` → 退化为单块 `[(0, total-1)]`
/// - 最后一块自动收窄，保证不越界
pub fn split_ranges(total: u64, chunk: u64) -> Vec<(u64, u64)> {
    if total == 0 {
        return Vec::new();
    }
    if chunk == 0 {
        return vec![(0, total - 1)];
    }

    let mut ranges = Vec::new();
    let mut start = 0u64;
    while start < total {
        let end = (start + chunk - 1).min(total - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

/// 自适应块大小：默认 4MB，链路速度快时升档
pub fn adaptive_chunk_size(observed_speed_mbps: f64) -> u64 {
    const MB: u64 = 1024 * 1024;
    if observed_speed_mbps >= 40.0 {
        16 * MB
    } else if observed_speed_mbps >= 15.0 {
        8 * MB
    } else {
        4 * MB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ranges_covers_whole_file_without_gaps() {
        let ranges = split_ranges(10, 4);
        assert_eq!(ranges, vec![(0, 3), (4, 7), (8, 9)]);
        // 区间连续且总长等于文件大小
        let covered: u64 = ranges.iter().map(|(s, e)| e - s + 1).sum();
        assert_eq!(covered, 10);
    }

    #[test]
    fn split_ranges_edge_cases() {
        assert!(split_ranges(0, 4).is_empty(), "空文件无需分块");
        assert_eq!(split_ranges(10, 0), vec![(0, 9)], "块大小为 0 时退化为单块");
        assert_eq!(split_ranges(4, 4), vec![(0, 3)]);
        assert_eq!(split_ranges(5, 4), vec![(0, 3), (4, 4)]);
    }

    #[test]
    fn adaptive_chunk_size_tiers() {
        const MB: u64 = 1024 * 1024;
        assert_eq!(adaptive_chunk_size(5.0), 4 * MB);
        assert_eq!(adaptive_chunk_size(20.0), 8 * MB);
        assert_eq!(adaptive_chunk_size(80.0), 16 * MB);
    }

    #[test]
    fn bmclapi_maps_known_hosts_and_passes_through_others() {
        let src = BmclapiSource::default();
        assert_eq!(
            src.resolve("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"),
            "https://bmclapi2.bangbang93.com/mc/game/version_manifest_v2.json"
        );
        assert_eq!(
            src.resolve("https://libraries.minecraft.net/org/ow2/asm/asm/9.5/asm-9.5.jar"),
            "https://bmclapi2.bangbang93.com/maven/org/ow2/asm/asm/9.5/asm-9.5.jar"
        );
        // Forge / NeoForge：官方 maven → 镜像 /maven（含 releases 子路径）
        assert_eq!(
            src.resolve("https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar"),
            "https://bmclapi2.bangbang93.com/maven/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar"
        );
        assert_eq!(
            src.resolve("https://maven.neoforged.net/releases/net/neoforged/neoforge/20.4.237/neoforge-20.4.237-installer.jar"),
            "https://bmclapi2.bangbang93.com/maven/net/neoforged/neoforge/20.4.237/neoforge-20.4.237-installer.jar"
        );
        // Fabric：maven 与 meta 域名分别映射到 /maven 与 /fabric-meta
        assert_eq!(
            src.resolve("https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"),
            "https://bmclapi2.bangbang93.com/maven/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar"
        );
        assert_eq!(
            src.resolve("https://meta.fabricmc.net/v2/versions/loader/1.20.1"),
            "https://bmclapi2.bangbang93.com/fabric-meta/v2/versions/loader/1.20.1"
        );
        // 未知域名原样返回
        let custom = "https://example.com/mod.jar";
        assert_eq!(src.resolve(custom), custom);
    }

    #[test]
    fn official_source_has_lowest_priority() {
        let official = OfficialSource;
        let mirror = BmclapiSource::default();
        let content = McimSource::default();
        assert!(official.priority() < mirror.priority());
        assert!(mirror.priority() < content.priority());
        assert_eq!(official.name(), "官方");
        assert_eq!(mirror.name(), "BMCLAPI");
        assert_eq!(content.name(), "MCIM");
    }

    #[test]
    fn mcim_maps_content_hosts_only() {
        let src = McimSource::default();
        assert_eq!(
            src.resolve("https://api.modrinth.com/v2/project/sodium"),
            "https://mod.mcimirror.top/modrinth/v2/project/sodium"
        );
        assert_eq!(
            src.resolve("https://cdn.modrinth.com/data/AANobbMI/icon.png"),
            "https://mod.mcimirror.top/data/AANobbMI/icon.png"
        );
        assert_eq!(
            src.resolve("https://api.curseforge.com/v1/mods/238222"),
            "https://mod.mcimirror.top/curseforge/v1/mods/238222"
        );
        assert_eq!(
            src.resolve("https://edge.forgecdn.net/files/1234/56/ExampleMod.jar"),
            "https://mod.mcimirror.top/files/1234/56/ExampleMod.jar"
        );
        // 明确不镜像 mediafilez；Mojang 域名也不该被改写
        let mediafilez = "https://mediafilez.forgecdn.net/files/1/2/a.jar";
        assert_eq!(src.resolve(mediafilez), mediafilez);
        let mojang = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
        assert_eq!(src.resolve(mojang), mojang);
    }
}
