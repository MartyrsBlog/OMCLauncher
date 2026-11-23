//! 内置实例图标：与 HMCL 的 `GameInstanceIconType` 一一对应。
//!
//! 素材取自 HMCL 的 `assets/img/*@2x.png`（64×64，GPL-3.0-or-later，见
//! `assets/icons/NOTICE.md`）。HMCL 本身**不做运行时合成**——它把每个加载器
//! 映射到一张预渲染 PNG；早期版本从客户端 jar 取贴图在 32px 下做等距投影，
//! 采样精度不足导致边缘粗糙，已废弃。

use std::path::Path;

use mcl_core::models::LoaderSpec;

/// 解码后的 RGBA8 图标（零 GUI 依赖，由适配层转成具体框架的图像类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 内置图标种类（与 HMCL `GameInstanceIconType` 对齐，含用户可选装饰图标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    Grass,
    Chest,
    Chicken,
    Command,
    Optifine,
    CraftTable,
    Fabric,
    Forge,
    NeoForge,
    Furnace,
    Quilt,
    AprilFools,
    CleanRoom,
    LegacyFabric,
}

impl IconKind {
    /// 全部图标（顺序与 HMCL 枚举一致，便于将来做图标选择器）
    pub const ALL: [IconKind; 14] = [
        IconKind::Grass,
        IconKind::Chest,
        IconKind::Chicken,
        IconKind::Command,
        IconKind::Optifine,
        IconKind::CraftTable,
        IconKind::Fabric,
        IconKind::Forge,
        IconKind::NeoForge,
        IconKind::Furnace,
        IconKind::Quilt,
        IconKind::AprilFools,
        IconKind::CleanRoom,
        IconKind::LegacyFabric,
    ];

    /// 稳定标识（用于配置持久化与 UI 传值）
    pub fn id(self) -> &'static str {
        match self {
            IconKind::Grass => "grass",
            IconKind::Chest => "chest",
            IconKind::Chicken => "chicken",
            IconKind::Command => "command",
            IconKind::Optifine => "optifine",
            IconKind::CraftTable => "craft_table",
            IconKind::Fabric => "fabric",
            IconKind::Forge => "forge",
            IconKind::NeoForge => "neoforge",
            IconKind::Furnace => "furnace",
            IconKind::Quilt => "quilt",
            IconKind::AprilFools => "april_fools",
            IconKind::CleanRoom => "cleanroom",
            IconKind::LegacyFabric => "legacyfabric",
        }
    }

    /// 从稳定标识解析（未知标识返回 `None`）
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    /// 加载器 → 默认图标（与 HMCL `getIconType(ModLoaderType)` 一致）
    pub fn for_loader(loader: &LoaderSpec) -> Self {
        match loader {
            LoaderSpec::Vanilla => IconKind::Grass,
            LoaderSpec::Fabric(_) => IconKind::Fabric,
            LoaderSpec::Forge(_) => IconKind::Forge,
            LoaderSpec::NeoForge(_) => IconKind::NeoForge,
            LoaderSpec::Quilt(_) => IconKind::Quilt,
        }
    }

    /// 加载器字符串标识 → 图标（安装向导与组件页用）
    pub fn for_loader_id(id: &str) -> Self {
        match id {
            "fabric" => IconKind::Fabric,
            "forge" => IconKind::Forge,
            "neoforge" => IconKind::NeoForge,
            "quilt" => IconKind::Quilt,
            "optifine" => IconKind::Optifine,
            "more" => IconKind::Command,
            _ => IconKind::Grass,
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            IconKind::Grass => include_bytes!("../assets/icons/grass.png"),
            IconKind::Chest => include_bytes!("../assets/icons/chest.png"),
            IconKind::Chicken => include_bytes!("../assets/icons/chicken.png"),
            IconKind::Command => include_bytes!("../assets/icons/command.png"),
            IconKind::Optifine => include_bytes!("../assets/icons/optifine.png"),
            IconKind::CraftTable => include_bytes!("../assets/icons/craft_table.png"),
            IconKind::Fabric => include_bytes!("../assets/icons/fabric.png"),
            IconKind::Forge => include_bytes!("../assets/icons/forge.png"),
            IconKind::NeoForge => include_bytes!("../assets/icons/neoforge.png"),
            IconKind::Furnace => include_bytes!("../assets/icons/furnace.png"),
            IconKind::Quilt => include_bytes!("../assets/icons/quilt.png"),
            IconKind::AprilFools => include_bytes!("../assets/icons/april_fools.png"),
            IconKind::CleanRoom => include_bytes!("../assets/icons/cleanroom.png"),
            IconKind::LegacyFabric => include_bytes!("../assets/icons/legacyfabric.png"),
        }
    }
}

/// 取内置图标
pub fn builtin_icon(kind: IconKind) -> Option<IconImage> {
    decode_rgba8(kind.bytes())
}

/// 设置实例自定义图标：校验为可解码 PNG 后复制为 `<实例目录>/icon.png`。
pub fn set_instance_icon(instance_dir: &Path, source: &Path) -> Result<(), String> {
    let bytes = std::fs::read(source).map_err(|error| format!("读取图片失败：{error}"))?;
    if decode_rgba8(&bytes).is_none() {
        return Err("仅支持 8-bit PNG 图片".to_string());
    }
    std::fs::write(instance_dir.join("icon.png"), bytes)
        .map_err(|error| format!("写入图标失败：{error}"))
}

/// 删除自定义图标（下一次刷新回落到内置加载器图标）。
pub fn clear_instance_icon(instance_dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(instance_dir.join("icon.png")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("删除图标失败：{error}")),
    }
}

/// 实例图标：显式选择的内置图标 > 实例根 `icon.png` > 加载器默认图标。
///
/// `custom_kind` 来自 `InstanceSettings::icon`（`IconKind::id`），空或未知时忽略。
pub fn instance_icon(
    instance_dir: &Path,
    loader: &LoaderSpec,
    custom_kind: Option<&str>,
) -> Option<IconImage> {
    if let Some(kind) = custom_kind.and_then(IconKind::from_id) {
        return builtin_icon(kind);
    }
    let custom = instance_dir.join("icon.png");
    if let Ok(bytes) = std::fs::read(&custom) {
        if let Some(icon) = decode_rgba8(&bytes) {
            return Some(icon);
        }
    }
    builtin_icon(IconKind::for_loader(loader))
}

/// 解码 PNG 并统一转成 8-bit RGBA。
///
/// 用户自定义图标可能是索引色 / 灰度 PNG，统一经 `EXPAND` 展开。
fn decode_rgba8(bytes: &[u8]) -> Option<IconImage> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let size = reader.output_buffer_size().unwrap_or(0);
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }

    let data = &buffer[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => {
            let (pixels, _) = data.as_chunks::<3>();
            pixels
                .iter()
                .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
                .collect()
        }
        png::ColorType::Grayscale => data
            .iter()
            .flat_map(|&value| [value, value, value, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => {
            let (pixels, _) = data.as_chunks::<2>();
            pixels
                .iter()
                .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                .collect()
        }
        _ => return None,
    };
    Some(IconImage {
        width: info.width,
        height: info.height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_png(rgba: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("写入 PNG 头");
            writer.write_image_data(&rgba).expect("写入像素");
        }
        bytes
    }

    #[test]
    fn decodes_rgba_png() {
        let icon = decode_rgba8(&rgba_png([12, 34, 56, 255])).expect("应当解码成功");
        assert_eq!((icon.width, icon.height), (1, 1));
        assert_eq!(icon.rgba, vec![12, 34, 56, 255]);
    }

    #[test]
    fn rejects_non_png_bytes() {
        assert!(decode_rgba8(b"not a png").is_none());
    }

    #[test]
    fn every_builtin_icon_decodes() {
        for kind in IconKind::ALL {
            let icon = builtin_icon(kind).unwrap_or_else(|| panic!("{kind:?} 应可解码"));
            // 大图（列表/首页展示）用 64px 素材，其余 32px；都必须是正方形 RGBA
            assert_eq!(icon.width, icon.height, "{kind:?} 应为正方形");
            assert!(
                matches!(icon.width, 32 | 64),
                "{kind:?} 尺寸应为 32 或 64，实际 {}",
                icon.width
            );
            assert_eq!(
                icon.rgba.len() as u32,
                icon.width * icon.height * 4,
                "{kind:?} 像素长度"
            );
        }
    }

    #[test]
    fn loader_maps_to_expected_icon() {
        assert_eq!(IconKind::for_loader(&LoaderSpec::Vanilla), IconKind::Grass);
        assert_eq!(
            IconKind::for_loader(&LoaderSpec::Fabric("0.16.9".into())),
            IconKind::Fabric
        );
        assert_eq!(
            IconKind::for_loader(&LoaderSpec::NeoForge("21.1.0".into())),
            IconKind::NeoForge
        );
        assert_eq!(IconKind::for_loader_id("quilt"), IconKind::Quilt);
        assert_eq!(IconKind::for_loader_id("optifine"), IconKind::Optifine);
    }

    #[test]
    fn icon_ids_round_trip() {
        for kind in IconKind::ALL {
            assert_eq!(IconKind::from_id(kind.id()), Some(kind));
        }
        assert_eq!(IconKind::from_id("nope"), None);
    }

    #[test]
    fn custom_icon_takes_priority() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mcl-icon-{stamp}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");

        // 没有 icon.png → 回落到内置图标
        let builtin = instance_icon(&dir, &LoaderSpec::Vanilla, None).expect("内置图标");
        assert_eq!((builtin.width, builtin.height), (64, 64));

        std::fs::write(dir.join("icon.png"), rgba_png([1, 2, 3, 255])).expect("写 icon.png");
        let icon = instance_icon(&dir, &LoaderSpec::Vanilla, None).expect("应读到自定义图标");
        assert_eq!((icon.width, icon.height), (1, 1));
        assert_eq!(icon.rgba, vec![1, 2, 3, 255]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn set_and_clear_custom_icon() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mcl-icon-set-{stamp}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let source = dir.join("source.png");
        std::fs::write(&source, rgba_png([9, 8, 7, 255])).expect("写源图");

        set_instance_icon(&dir, &source).expect("设置成功");
        assert_eq!(
            instance_icon(&dir, &LoaderSpec::Vanilla, None)
                .expect("应读到自定义图标")
                .rgba,
            vec![9, 8, 7, 255]
        );

        clear_instance_icon(&dir).expect("清除成功");
        assert_eq!(
            instance_icon(&dir, &LoaderSpec::Vanilla, None)
                .expect("应回落到内置图标")
                .width,
            64
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_non_png_icon() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mcl-icon-bad-{stamp}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let source = dir.join("bad.txt");
        std::fs::write(&source, b"not an image").expect("写文件");

        assert!(set_instance_icon(&dir, &source).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
