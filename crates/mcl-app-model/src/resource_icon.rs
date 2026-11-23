//! 资源站封面（Modrinth 图标）的拉取、缓存与解码。
//!
//! 与 [`crate::icon`]（内置图标，编译期嵌入）不同，这里处理的是**远程图片**：
//! JPEG 4% / GIF 2%（各取首帧）。CDN 不支持格式协商（`?fm=png`、`Accept` 都无效），
//! 因此解码放在本地做。
//!
//! 缓存：`<数据目录>/cache/icons/<url 的 sha1>`，与下载内容缓存分开，避免被
//! `cache/objects` 的 512MB 淘汰策略误删（封面很小，重下代价低但不该反复拉）。

use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use mcl_core::http::HttpClient;

use crate::icon::IconImage;

/// 解码后的最长边。卡片显示 44px、详情页 64px，96 足够 2× 屏。
pub const ICON_MAX: u32 = 96;
/// 单张封面上限（正常 ≤ 200KB，超限说明拿到的是错误页或异常图）
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// 封面缓存上限。单张 ≤ 200 KB，64 MB 可存 300+ 张，足够覆盖常用搜索结果。
pub const CACHE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// 封面缓存目录：`<数据目录>/cache/icons`
pub fn cache_dir() -> PathBuf {
    mcl_core::platform::paths::data_dir()
        .join("cache")
        .join("icons")
}

/// 按 LRU（mtime）裁剪封面缓存，返回删除的字节数。
///
/// 上限 64MB，超出时删到 **90%**（避免每次启动都刚好越界再删一轮）。
/// 与下载内容缓存同样的「每进程一次」策略，由壳在启动时调用。
pub fn prune_cache() -> u64 {
    prune_dir(&cache_dir(), CACHE_LIMIT_BYTES)
}

fn prune_dir(dir: &Path, limit: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().ok()?;
            Some((mtime, meta.len(), entry.path()))
        })
        .collect();

    let total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    if total <= limit {
        return 0;
    }

    files.sort_by_key(|(mtime, _, _)| *mtime);
    let target = limit / 10 * 9;
    let mut remaining = total;
    let mut removed = 0u64;
    for (_, size, path) in files {
        if remaining <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            remaining = remaining.saturating_sub(size);
            removed += size;
        }
    }
    removed
}

/// 缓存文件路径：URL 的 SHA-1（十六进制，40 字符）
fn cache_path(url: &str) -> PathBuf {
    let mut hasher = Sha1::new();
    hasher.update(url.as_bytes());
    cache_dir().join(format!("{:x}", hasher.finalize()))
}

/// 拉取并解码封面。命中本地缓存则不联网；任何失败都返回 `None`，
/// 由调用方回落到类目语义图标。
pub async fn fetch(http: &HttpClient, url: &str) -> Option<IconImage> {
    fetch_sized(http, url, ICON_MAX).await
}

/// 与 [`fetch`] 相同，但按调用方给的最大边解码（简介配图需要 800+）。
/// 缓存里存的是原图字节，因此不同尺寸只影响解码，不会重复下载。
pub async fn fetch_sized(http: &HttpClient, url: &str, max_side: u32) -> Option<IconImage> {
    // 只接受 HTTPS：icon_url 来自站点响应，仍做一次协议收敛，避免被 file:// 之类利用
    if !url.starts_with("https://") {
        return None;
    }

    let path = cache_path(url);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => {
            let response = http.get(url).await.ok()?;
            let bytes = response.bytes().await.ok()?;
            if bytes.is_empty() || bytes.len() > MAX_BYTES {
                return None;
            }
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&path, &bytes).await;
            bytes.to_vec()
        }
    };

    decode_max(&bytes, max_side)
}

/// 已编码图片 → RGBA8（等比缩放到 [`ICON_MAX`] 以内）
pub fn decode(bytes: &[u8]) -> Option<IconImage> {
    decode_max(bytes, ICON_MAX)
}

/// 已编码图片 → RGBA8，等比缩放到 `max_side` 以内。
///
/// 支持 PNG / WebP（启用特性见 `Cargo.toml`）。首页 Hero 背景也走这里，
/// 因此比封面用的上限更大。
pub fn decode_max(bytes: &[u8], max_side: u32) -> Option<IconImage> {
    let max_side = max_side.max(1);
    let image = image::load_from_memory(bytes).ok()?;
    // `thumbnail` 在小图上会**放大**到给定边界（resize_dimensions 不做 1 倍裁剪），
    // 因此只在超出上限时才缩放。thumbnail 走近邻采样，96px 下与 Lanczos 无肉眼差异。
    let image = if image.width() > max_side || image.height() > max_side {
        image.thumbnail(max_side, max_side)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Some(IconImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            let data = vec![200u8; (width * height * 4) as usize];
            writer.write_image_data(&data).expect("png data");
        }
        out
    }

    #[test]
    fn decode_png_and_scale_within_limit() {
        let icon = decode(&tiny_png(200, 100)).expect("PNG 应能解码");
        assert_eq!(icon.width, ICON_MAX);
        assert_eq!(icon.height, ICON_MAX / 2);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }

    #[test]
    fn decode_keeps_small_images_untouched() {
        let icon = decode(&tiny_png(40, 40)).expect("PNG 应能解码");
        assert_eq!((icon.width, icon.height), (40, 40));
    }

    #[test]
    fn decode_max_honours_custom_limit() {
        let icon = decode_max(&tiny_png(200, 100), 1440).expect("PNG 应能解码");
        assert_eq!((icon.width, icon.height), (200, 100));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"not an image at all").is_none());
    }

    #[test]
    fn prune_keeps_cache_under_limit() {
        let dir = std::env::temp_dir().join(format!("mcl-icon-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for index in 0..5 {
            std::fs::write(dir.join(format!("f{index}")), vec![0u8; 100]).expect("write");
        }

        // 500B 总量、上限 300B → 删到 270B 以下
        let removed = prune_dir(&dir, 300);
        assert!(removed >= 200, "应删除最早的若干张，实际 {removed}");
        let left: u64 = std::fs::read_dir(&dir)
            .expect("read")
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum();
        assert!(left <= 300, "剩余 {left} 应不超上限");

        // 未超上限时不动
        assert_eq!(prune_dir(&dir, 10_000), 0);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn cache_path_is_url_addressed_and_stable() {
        let url = "https://cdn.modrinth.com/data/AANobbMI/icon.png";
        let path = cache_path(url);
        assert_eq!(path, cache_path(url));
        assert_eq!(
            path.file_name().map(|name| name.len()),
            Some(40),
            "SHA-1 十六进制固定 40 字符"
        );
        assert_ne!(
            path,
            cache_path("https://cdn.modrinth.com/data/OTHER/icon.png")
        );
    }
}
