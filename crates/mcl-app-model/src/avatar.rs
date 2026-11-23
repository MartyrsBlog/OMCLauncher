//! 账号头像：把 Minecraft 皮肤渲染成头部头像。
//!
//! 三家参考实现的共同结论：
//! - 取 `sessionserver` 公开档案里的 `textures.SKIN` 地址（PCL2 `McSkinGetAddress`）；
//! - 头部 = `(8,8)` 的 8×8 面部 + `(40,8)` 的 8×8 帽层，按 alpha 合成
//!   （HMCL `TexturesLoader.drawAvatar` 的两段 drawImage、Modrinth `render_face` 的 over）；
//! - 像素风用最近邻放大（HMCL `setImageSmoothing(false)`），不做平滑。
//!
//! 缓存：`<数据目录>/cache/avatars/<uuid>.png`（64×64），TTL 24 小时。
//! 任何失败都返回 `None`，由 UI 回落到首字母。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use mcl_core::http::HttpClient;

use crate::icon::IconImage;

/// 输出边长。账号页/侧栏显示 32–40px，64 留高分屏余量。
pub const HEAD_SIZE: u32 = 64;
/// 皮肤缓存有效期：皮肤变更频率低，一天一次足够，也能避开 sessionserver 限流。
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 皮肤 PNG 上限（64×64 正常 < 20KB，超限说明拿到的是错误页）
const MAX_SKIN_BYTES: usize = 1024 * 1024;

/// 头像缓存目录：`<数据目录>/cache/avatars`
pub fn cache_dir() -> PathBuf {
    mcl_core::platform::paths::data_dir()
        .join("cache")
        .join("avatars")
}

fn cache_path(uuid: &str) -> PathBuf {
    cache_dir().join(format!("{uuid}.png"))
}

fn is_fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|age| age < TTL)
}

/// 拉取并渲染头像；命中缓存则不联网。仅接受 32 位十六进制 UUID。
pub async fn fetch(http: &HttpClient, uuid: &str) -> Option<IconImage> {
    if uuid.len() != 32 || !uuid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    let path = cache_path(uuid);
    if is_fresh(&path) {
        if let Ok(bytes) = tokio::fs::read(&path).await {
            if let Some(image) = decode(&bytes) {
                return Some(image);
            }
        }
    }

    let skin_url = skin_url(http, uuid).await?;
    let response = http.get(&skin_url).await.ok()?;
    let bytes = response.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > MAX_SKIN_BYTES {
        return None;
    }

    let png = render_head(&bytes)?;
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(&path, &png).await;
    decode(&png)
}

/// 从公开档案解析皮肤地址。`textures` 属性 = base64(JSON)。
async fn skin_url(http: &HttpClient, uuid: &str) -> Option<String> {
    let profile: serde_json::Value = http
        .get_json(&format!(
            "https://sessionserver.mojang.com/session/minecraft/profile/{uuid}"
        ))
        .await
        .ok()?;
    let encoded = profile
        .get("properties")?
        .as_array()?
        .iter()
        .find(|item| item.get("name").and_then(|name| name.as_str()) == Some("textures"))?
        .get("value")?
        .as_str()?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let textures: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let url = textures
        .get("textures")?
        .get("SKIN")?
        .get("url")?
        .as_str()?
        .to_string();
    // 官方档案历史上返回 http；与 PCL2 一样升级到 https
    let url = url.replace(
        "http://textures.minecraft.net/",
        "https://textures.minecraft.net/",
    );
    url.starts_with("https://").then_some(url)
}

/// 64×64 皮肤 PNG → 64×64 头部 PNG。
///
/// 兼容 1.8 之前的 64×32 皮肤（没有帽层区域，帽层视为全透明）。
pub fn render_head(skin_png: &[u8]) -> Option<Vec<u8>> {
    let skin = image::load_from_memory(skin_png).ok()?.to_rgba8();
    let (width, height) = skin.dimensions();
    if width < 64 || height < 32 {
        return None;
    }

    let mut head = image::RgbaImage::new(HEAD_SIZE, HEAD_SIZE);
    for y in 0..HEAD_SIZE {
        for x in 0..HEAD_SIZE {
            let source_x = x * 8 / HEAD_SIZE;
            let source_y = y * 8 / HEAD_SIZE;
            let face = *skin.get_pixel(8 + source_x, 8 + source_y);
            let hat = if height >= 64 {
                *skin.get_pixel(40 + source_x, 8 + source_y)
            } else {
                image::Rgba([0, 0, 0, 0])
            };
            head.put_pixel(x, y, over(hat, face));
        }
    }

    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(head)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// 帽层叠在面部之上的 alpha 合成（与 Modrinth `render_face` 的 `over` 一致）。
fn over(top: image::Rgba<u8>, bottom: image::Rgba<u8>) -> image::Rgba<u8> {
    let top_alpha = f32::from(top.0[3]) / 255.0;
    let bottom_alpha = f32::from(bottom.0[3]) / 255.0 * (1.0 - top_alpha);
    let alpha = top_alpha + bottom_alpha;
    if alpha <= 0.0 {
        return image::Rgba([0, 0, 0, 0]);
    }
    let channel = |index: usize| {
        ((f32::from(top.0[index]) * top_alpha + f32::from(bottom.0[index]) * bottom_alpha) / alpha)
            .round() as u8
    };
    image::Rgba([
        channel(0),
        channel(1),
        channel(2),
        (alpha * 255.0).round() as u8,
    ])
}

fn decode(bytes: &[u8]) -> Option<IconImage> {
    crate::resource_icon::decode_max(bytes, HEAD_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skin_png(width: u32, height: u32, face: [u8; 4], hat: [u8; 4]) -> Vec<u8> {
        let mut skin = image::RgbaImage::new(width, height);
        for y in 8..16 {
            for x in 8..16 {
                skin.put_pixel(x, y, image::Rgba(face));
            }
        }
        if height >= 64 {
            for y in 8..16 {
                for x in 40..48 {
                    skin.put_pixel(x, y, image::Rgba(hat));
                }
            }
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(skin)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode skin");
        out
    }

    #[test]
    fn opaque_hat_wins_over_face() {
        let merged = over(image::Rgba([255, 0, 0, 255]), image::Rgba([0, 255, 0, 255]));
        assert_eq!(merged, image::Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn transparent_hat_keeps_face() {
        let merged = over(image::Rgba([255, 0, 0, 0]), image::Rgba([0, 255, 0, 255]));
        assert_eq!(merged, image::Rgba([0, 255, 0, 255]));
    }

    #[test]
    fn half_transparent_hat_blends() {
        let merged = over(image::Rgba([255, 0, 0, 128]), image::Rgba([0, 0, 255, 255]));
        // 0.502 * 255 + 0.498 * 0 ≈ 128；蓝通道 0.498 * 255 ≈ 127
        assert_eq!(merged.0[0], 128);
        assert_eq!(merged.0[2], 127);
        assert_eq!(merged.0[3], 255);
    }

    #[test]
    fn renders_legacy_64x32_skin_without_hat_layer() {
        let png = skin_png(64, 32, [10, 20, 30, 255], [255, 0, 0, 255]);
        let head = render_head(&png).expect("legacy skin should render");
        let decoded = image::load_from_memory(&head).expect("png").to_rgba8();
        assert_eq!(decoded.dimensions(), (HEAD_SIZE, HEAD_SIZE));
        assert_eq!(decoded.get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn renders_modern_skin_with_hat_layer() {
        let png = skin_png(64, 64, [10, 20, 30, 255], [200, 0, 0, 255]);
        let head = render_head(&png).expect("skin should render");
        let decoded = image::load_from_memory(&head).expect("png").to_rgba8();
        assert_eq!(decoded.dimensions(), (HEAD_SIZE, HEAD_SIZE));
        assert_eq!(decoded.get_pixel(0, 0).0, [200, 0, 0, 255]);
    }

    #[test]
    fn rejects_tiny_or_garbage_images() {
        assert!(render_head(b"not a png").is_none());
        let small = skin_png(32, 32, [1, 2, 3, 255], [0, 0, 0, 0]);
        assert!(render_head(&small).is_none());
    }

    #[test]
    fn cache_path_lives_under_avatars_dir() {
        let path = cache_path("c4d9939d639547eabed4caadef5d6870");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("c4d9939d639547eabed4caadef5d6870.png")
        );
        assert!(path.starts_with(cache_dir()));
    }

    #[test]
    fn fetch_rejects_non_hex_uuid() {
        // 不联网：格式校验在发请求之前
        let http = HttpClient::with_default_sources().expect("http client");
        assert!(tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fetch(&http, "not-a-uuid"))
            .is_none());
    }
}
