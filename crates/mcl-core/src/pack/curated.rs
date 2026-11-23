//! 内置精选整合包清单
//!
//! 设计：**静态维护、随启动器更新**——清单中的下载直链与 SHA-1 在发布时钉死，
//! 不依赖 Modrinth API（离线可用；CDN 文件长期保留，旧版本 URL 不会失效）。
//!
//! 维护方式：新增/更新条目时用 Modrinth API 取 `files[].url` 与 `files[].hashes.sha1`：
//! ```text
//! curl -s 'https://api.modrinth.com/v2/project/<slug>/version' | jq '.[0].files[0]'
//! ```
//! 可直接跑 `mcl-cli pack install <id>` 验证能否端到端安装。

/// 精选整合包条目
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuratedPack {
    /// 稳定 ID（UI / CLI / 任务重试引用）
    pub id: &'static str,
    pub name: &'static str,
    /// 一句话简介（中文）
    pub summary: &'static str,
    pub mc_version: &'static str,
    /// 加载器展示标签（版本以清单内 dependencies 为准）
    pub loader: &'static str,
    /// `.mrpack` 直链（Modrinth CDN）
    pub url: &'static str,
    /// `.mrpack` 的 SHA-1（下载校验，防篡改/断链）
    pub sha1: &'static str,
    /// `.mrpack` 体积（字节；清单列表展示用）
    pub size: u64,
}

pub const CURATED_PACKS: &[CuratedPack] = &[
    CuratedPack {
        id: "fabulously-optimized-1.20.1",
        name: "Fabulously Optimized",
        summary: "性能优化整合包（1.20.1）：帧率大幅提升、保留原版体验，新手首选",
        mc_version: "1.20.1",
        loader: "Fabric 0.14.23",
        url: "https://cdn.modrinth.com/data/1KVo5zza/versions/2ZbcYfCj/Fabulously.Optimized-5.4.1.mrpack",
        sha1: "3be3b9159e8d5fc1d4fc48530d20758a3600087d",
        size: 58_960,
    },
    CuratedPack {
        id: "better-mc-bmc2-1.20.1",
        name: "Better MC [FABRIC] - BMC2",
        summary: "冒险内容整合包（1.20.1）：新增群系/结构/装备，适合长期生存游玩",
        mc_version: "1.20.1",
        loader: "Fabric",
        url: "https://cdn.modrinth.com/data/shFhR8Vx/versions/M5BnAIQy/BMC2%20%5BFABRIC%5D%201.20.1%20v40.mrpack",
        sha1: "326da0b267496f5d952118b7f8aa2bddcb7fc269",
        size: 66_273_847,
    },
    CuratedPack {
        id: "fabulously-optimized-latest",
        name: "Fabulously Optimized（最新版）",
        summary: "性能优化整合包（跟随最新 MC 版本，自动下载所需 Java）",
        mc_version: "26.2",
        loader: "Fabric",
        url: "https://cdn.modrinth.com/data/1KVo5zza/versions/ssWn7YI0/Fabulously.Optimized-v14.1.0.mrpack",
        sha1: "a972f29de7a636e21ebbcff505ffdb335da6e3e1",
        size: 167_784,
    },
];

/// 按 ID 查条目
pub fn curated_pack(id: &str) -> Option<&'static CuratedPack> {
    CURATED_PACKS.iter().find(|pack| pack.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn curated_entries_are_well_formed() {
        assert!(!CURATED_PACKS.is_empty());

        let mut ids = HashSet::new();
        for pack in CURATED_PACKS {
            assert!(ids.insert(pack.id), "ID 重复：{}", pack.id);
            assert!(
                pack.url.starts_with("https://") && pack.url.ends_with(".mrpack"),
                "下载直链应为 https 的 .mrpack：{}",
                pack.url
            );
            assert_eq!(pack.sha1.len(), 40, "SHA-1 应为 40 位：{}", pack.sha1);
            assert!(
                pack.sha1.chars().all(|ch| ch.is_ascii_hexdigit()),
                "SHA-1 应为十六进制：{}",
                pack.sha1
            );
            assert!(
                !pack.summary.is_empty() && !pack.mc_version.is_empty(),
                "简介与 MC 版本不能为空：{}",
                pack.id
            );
        }
    }

    #[test]
    fn lookup_by_id() {
        let first = CURATED_PACKS.first().expect("非空");
        assert_eq!(
            curated_pack(first.id).map(|pack| pack.name),
            Some(first.name)
        );
        assert!(curated_pack("not-a-pack").is_none());
    }
}
