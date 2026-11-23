use std::io::Read;
use std::path::{Path, PathBuf};

/// 实例详情页日志显示的最大行数（尾部）。
pub const LOG_TAIL_LINES: usize = 200;
/// 日志只读文件尾部这么多字节（防超大日志占用内存）。
pub const LOG_READ_LIMIT: u64 = 512 * 1024;

/// 模组目录扫描结果。
///
/// 框架无关：这里保留原始字节数，格式化与排序展示交给 UI 壳，
/// 便于将来换壳时复用同一份扫描逻辑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModItem {
    /// 文件名（已去掉 `.disabled` 后缀）。
    pub file_name: String,
    /// 原始字节数。
    pub size_bytes: u64,
    /// 启用状态（禁用 = 磁盘上以 `.disabled` 结尾）。
    pub enabled: bool,
}

/// 读取实例 `mods/` 目录 → 模组列表 + 提示文案。
///
/// 只统计 `.jar`（含 `.jar.disabled`），忽略 `.part` 等下载中间文件。
pub fn scan_mods(instance_dir: &Path) -> (Vec<ModItem>, String) {
    let mods_dir = instance_dir.join("mods");
    let Ok(entries) = std::fs::read_dir(&mods_dir) else {
        return (
            Vec::new(),
            format!(
                "mods 目录尚未创建（点「打开文件夹」会自动创建）：{}",
                mods_dir.display()
            ),
        );
    };

    let mut rows: Vec<ModItem> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let raw = entry.file_name().to_string_lossy().into_owned();
            let (display, enabled) = match raw.strip_suffix(".disabled") {
                Some(stripped) => (stripped.to_string(), false),
                None => (raw, true),
            };
            if !display.to_ascii_lowercase().ends_with(".jar") {
                return None;
            }
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or_default();
            Some(ModItem {
                file_name: display,
                size_bytes: size,
                enabled,
            })
        })
        .collect();
    rows.sort_by_key(|row| row.file_name.to_ascii_lowercase());

    let hint = if rows.is_empty() {
        format!(
            "还没有模组：把 .jar 放进 {} 后点「刷新」",
            mods_dir.display()
        )
    } else {
        let enabled = rows.iter().filter(|row| row.enabled).count();
        format!("{} 个模组 · {} 个启用", rows.len(), enabled)
    };
    (rows, hint)
}

/// 从 jar 读取模组版本号。
///
/// 优先 `fabric.mod.json` 的 `version`，其次 `META-INF/MANIFEST.MF` 的
/// `Implementation-Version`（Forge / NeoForge 常写在这里）。只读这两个条目并限制
/// 读取大小，不解压整个 jar——扫描上百个模组也只花几十毫秒。
///
/// 模板占位（`${file.jarVersion}`）视为无效，返回 `None`。
pub fn mod_version(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    if let Ok(entry) = archive.by_name("fabric.mod.json") {
        let mut text = String::new();
        if entry.take(128 * 1024).read_to_string(&mut text).is_ok() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(version) = value.get("version").and_then(|value| value.as_str()) {
                    let version = version.trim();
                    if !version.is_empty() && !version.starts_with("${") {
                        return Some(version.to_string());
                    }
                }
            }
        }
    }

    if let Ok(entry) = archive.by_name("META-INF/MANIFEST.MF") {
        let mut text = String::new();
        if entry.take(64 * 1024).read_to_string(&mut text).is_ok() {
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("Implementation-Version:") {
                    let value = value.trim();
                    if !value.is_empty() && !value.starts_with("${") {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }

    None
}

/// 读取实例最近一次启动日志的尾部（最多 [`LOG_TAIL_LINES`] 行）→（行, 提示）。
pub fn read_recent_log(instance_dir: &Path) -> (Vec<String>, String) {
    let logs_dir = instance_dir.join("logs");
    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return (Vec::new(), "暂无日志：启动一次游戏后生成".to_string());
    };

    // 取修改时间最新的 .log
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, path));
        }
    }
    let Some((_, path)) = newest else {
        return (Vec::new(), "暂无日志：启动一次游戏后生成".to_string());
    };

    let (text, truncated) = read_tail(&path, LOG_READ_LIMIT).unwrap_or_default();
    let mut lines: Vec<&str> = text.lines().collect();
    // 从文件中间截断时，丢掉可能不完整的首行
    if truncated && !lines.is_empty() {
        lines.remove(0);
    }
    let tail: Vec<String> = lines
        .iter()
        .rev()
        .take(LOG_TAIL_LINES)
        .rev()
        .map(|line| (*line).to_string())
        .collect();

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let size = if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    };
    let hint = format!("{name} · {size} · 最近 {} 行", tail.len());
    (tail, hint)
}

/// 读取文件尾部最多 `limit` 字节（文件较小时整读）→（文本, 是否发生截断）。
pub fn read_tail(path: &Path, limit: u64) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    Ok((String::from_utf8_lossy(&buffer).into_owned(), start > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_mods_reads_jars_and_marks_disabled() {
        let dir = std::env::temp_dir().join("mcl-mod-detail");
        let _ = std::fs::remove_dir_all(&dir);
        let mods = dir.join("mods");
        std::fs::create_dir_all(&mods).expect("建目录");
        std::fs::write(mods.join("sodium.jar"), vec![0u8; 4096]).expect("写");
        std::fs::write(mods.join("lithium.jar.disabled"), vec![0u8; 1024]).expect("写");
        std::fs::write(mods.join("partial.jar.part"), b"x").expect("写");
        std::fs::write(mods.join("readme.txt"), b"x").expect("写");

        let (rows, hint) = scan_mods(&dir);
        assert_eq!(
            rows.len(),
            2,
            "只统计 jar（含 .disabled），忽略 .part 与其它文件"
        );
        assert_eq!(rows[0].file_name, "lithium.jar");
        assert!(!rows[0].enabled, ".disabled 后缀应识别为禁用态");
        assert_eq!(rows[0].size_bytes, 1024);
        assert_eq!(rows[1].file_name, "sodium.jar");
        assert!(rows[1].enabled);
        assert!(hint.contains("2 个模组"), "提示应含数量：{hint}");

        // mods 目录不存在时给出可操作提示
        let missing = std::env::temp_dir().join("mcl-mod-detail-missing");
        let _ = std::fs::remove_dir_all(&missing);
        let (rows, hint) = scan_mods(&missing);
        assert!(rows.is_empty());
        assert!(hint.contains("打开文件夹"), "应提示创建路径的方式：{hint}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_recent_log_returns_tail_lines_of_newest_file() {
        let dir = std::env::temp_dir().join("mcl-log-tail");
        let _ = std::fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).expect("建目录");

        let content: String = (1..=300).map(|index| format!("line {index}\n")).collect();
        std::fs::write(logs.join("1.20.1.log"), &content).expect("写日志");

        let (lines, hint) = read_recent_log(&dir);
        assert_eq!(lines.len(), LOG_TAIL_LINES, "只保留尾部固定行数");
        assert_eq!(lines[0], "line 101");
        assert_eq!(lines.last().expect("非空"), "line 300");
        assert!(hint.contains("1.20.1.log"), "提示应含日志文件名：{hint}");

        // 无 logs 目录时给出空态提示
        let missing = std::env::temp_dir().join("mcl-log-tail-missing");
        let _ = std::fs::remove_dir_all(&missing);
        let (lines, hint) = read_recent_log(&missing);
        assert!(lines.is_empty());
        assert!(hint.contains("暂无日志"), "空态提示：{hint}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mod_version_reads_fabric_metadata() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("mcl-mod-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let jar = dir.join("sodium-fabric-0.5.8.jar");

        {
            let file = std::fs::File::create(&jar).expect("create jar");
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("fabric.mod.json", options).expect("entry");
            zip.write_all(br#"{"id":"sodium","version":"0.5.8","name":"Sodium"}"#)
                .expect("write");
            zip.finish().expect("finish");
        }

        assert_eq!(mod_version(&jar).as_deref(), Some("0.5.8"));

        // 非 zip 文件不应 panic
        let broken = dir.join("broken.jar");
        std::fs::write(&broken, b"not a zip").expect("write");
        assert_eq!(mod_version(&broken), None);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
