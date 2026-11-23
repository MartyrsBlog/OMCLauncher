//! `.mrpack`（Modrinth 整合包）解析
//!
//! 格式（Modrinth 官方规范 `formatVersion: 1`）：
//! - zip 内含 `modrinth.index.json`（清单）+ `overrides/`（公共覆盖文件）
//!   + `client-overrides/`（仅客户端覆盖；`server-overrides/` 本项目忽略）；
//! - 清单 `files[]` 给出直链与哈希（sha1/sha512），`env.client == "unsupported"`
//!   的文件跳过（仅服务端需要）；
//! - `dependencies` 指明 `minecraft` 与加载器版本（`fabric-loader` / `quilt-loader` / …）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{CoreError, CoreResult};
use crate::models::{FileEntry, LoaderSpec};

/// 清单中的单个下载项（已按客户端视角过滤）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFile {
    /// 相对实例根的路径（如 `mods/sodium.jar`）
    pub path: String,
    pub url: String,
    pub sha1: Option<String>,
    pub size: u64,
}

/// 解析后的整合包清单
#[derive(Debug, Clone)]
pub struct ModpackIndex {
    pub name: String,
    pub version_id: String,
    pub summary: String,
    pub mc_version: String,
    pub loader: LoaderSpec,
    pub files: Vec<PackFile>,
    /// 无法安装的清单条目（如缺下载地址）——不静默丢弃，随报告一起呈现
    pub issues: Vec<(String, String)>,
}

impl ModpackIndex {
    /// 转成下载引擎的文件清单（路径相对实例根）
    pub fn file_entries(&self) -> Vec<FileEntry> {
        self.files
            .iter()
            .map(|file| FileEntry {
                path: file.path.clone(),
                url: file.url.clone(),
                sha1: file.sha1.clone(),
                size: file.size,
            })
            .collect()
    }

    /// 需要下载的总体积（清单标注值，展示用）
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

/// 读取 `.mrpack` 中的 `modrinth.index.json` 并解析
pub fn read_mrpack(path: &Path) -> CoreResult<ModpackIndex> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Manifest(format!("整合包不是有效的 zip：{error}")))?;

    let mut index_file = archive.by_name("modrinth.index.json").map_err(|_| {
        CoreError::Manifest("整合包缺少 modrinth.index.json（不是 Modrinth 格式）".into())
    })?;
    let mut text = String::new();
    index_file.read_to_string(&mut text)?;
    parse_index(&text)
}

/// 解析 `modrinth.index.json` 文本（独立函数便于测试与复用）
pub fn parse_index(text: &str) -> CoreResult<ModpackIndex> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| CoreError::Manifest(format!("整合包清单解析失败：{error}")))?;

    let format_version = value
        .get("formatVersion")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if format_version != 1 {
        return Err(CoreError::Manifest(format!(
            "不支持的整合包格式版本 {format_version}（仅支持 formatVersion 1）"
        )));
    }

    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("整合包")
        .to_string();
    let version_id = value
        .get("versionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let dependencies = value.get("dependencies").and_then(Value::as_object);
    let mc_version = dependencies
        .and_then(|map| map.get("minecraft"))
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Manifest("整合包清单缺少 dependencies.minecraft".into()))?
        .to_string();
    let loader = parse_loader(dependencies);

    let mut files = Vec::new();
    let mut issues = Vec::new();
    if let Some(list) = value.get("files").and_then(Value::as_array) {
        for item in list {
            // 仅服务端需要的文件：跳过
            if item.pointer("/env/client").and_then(Value::as_str) == Some("unsupported") {
                continue;
            }
            let Some(path) = item.get("path").and_then(Value::as_str) else {
                continue;
            };
            let Some(url) = item
                .get("downloads")
                .and_then(Value::as_array)
                .and_then(|urls| urls.first())
                .and_then(Value::as_str)
            else {
                issues.push((path.to_string(), "清单缺少下载地址".to_string()));
                continue;
            };
            files.push(PackFile {
                path: path.to_string(),
                url: url.to_string(),
                sha1: item
                    .pointer("/hashes/sha1")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                size: item.get("fileSize").and_then(Value::as_u64).unwrap_or(0),
            });
        }
    }

    Ok(ModpackIndex {
        name,
        version_id,
        summary,
        mc_version,
        loader,
        files,
        issues,
    })
}

/// 依赖表 → 加载器规格（无加载器依赖时视为原版）
fn parse_loader(dependencies: Option<&serde_json::Map<String, Value>>) -> LoaderSpec {
    let get = |key: &str| {
        dependencies
            .and_then(|map| map.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    if let Some(version) = get("fabric-loader") {
        return LoaderSpec::Fabric(version);
    }
    if let Some(version) = get("quilt-loader") {
        return LoaderSpec::Quilt(version);
    }
    if let Some(version) = get("forge") {
        return LoaderSpec::Forge(version);
    }
    if let Some(version) = get("neoforge") {
        return LoaderSpec::NeoForge(version);
    }
    LoaderSpec::Vanilla
}

/// 把 `.mrpack` 内的 `overrides/` 与 `client-overrides/` 复制到实例根，返回复制文件数
///
/// 路径经 `ZipFile::enclosed_name` 过滤（防目录穿越）；同名文件会被覆盖
///（规范语义：overrides 优先于下载文件）。
pub fn extract_overrides(mrpack_path: &Path, dest_root: &Path) -> CoreResult<usize> {
    let file = std::fs::File::open(mrpack_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Manifest(format!("整合包不是有效的 zip：{error}")))?;

    let mut copied = 0;
    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        // 防目录穿越：`enclosed_name` 只返回包内的安全相对路径
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        let relative = enclosed
            .strip_prefix("overrides")
            .or_else(|_| enclosed.strip_prefix("client-overrides"));
        let Ok(relative) = relative else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = dest_root.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut output)?;
        copied += 1;
    }
    Ok(copied)
}

/// 把实例导出为本地 `.mrpack`：`files` 为空，`mods/`、`config/`、`options.txt`
/// 全部放进 `overrides/`。任何启动器都能安装；代价是不能按远端版本做增量更新。
pub fn export_local_mrpack(
    name: &str,
    mc_version: &str,
    loader: &LoaderSpec,
    instance_dir: &Path,
    dest_dir: &Path,
) -> CoreResult<PathBuf> {
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    std::fs::create_dir_all(dest_dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let safe_name: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let path = dest_dir.join(format!("{safe_name}-{stamp}.mrpack"));

    let file = std::fs::File::create(&path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut dependencies = json!({ "minecraft": mc_version });
    match loader {
        LoaderSpec::Fabric(version) => dependencies["fabric-loader"] = json!(version),
        LoaderSpec::Quilt(version) => dependencies["quilt-loader"] = json!(version),
        LoaderSpec::Forge(version) => dependencies["forge"] = json!(version),
        LoaderSpec::NeoForge(version) => dependencies["neoforge"] = json!(version),
        LoaderSpec::Vanilla => {}
    }
    let index = json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "1.0.0",
        "name": name,
        "summary": "本地导出的整合包（mods/config 位于 overrides）",
        "files": [],
        "dependencies": dependencies,
    });

    zip.start_file("modrinth.index.json", options)
        .map_err(|error| CoreError::Unsupported(error.to_string()))?;
    zip.write_all(&serde_json::to_vec_pretty(&index)?)?;

    for folder in ["mods", "config"] {
        add_tree(
            &mut zip,
            &instance_dir.join(folder),
            &format!("overrides/{folder}"),
            options,
        )?;
    }
    let options_txt = instance_dir.join("options.txt");
    if options_txt.is_file() {
        zip.start_file("overrides/options.txt", options)
            .map_err(|error| CoreError::Unsupported(error.to_string()))?;
        zip.write_all(&std::fs::read(options_txt)?)?;
    }

    zip.finish()
        .map_err(|error| CoreError::Unsupported(error.to_string()))?;
    Ok(path)
}

fn add_tree(
    zip: &mut zip::ZipWriter<std::fs::File>,
    source: &Path,
    prefix: &str,
    options: zip::write::SimpleFileOptions,
) -> CoreResult<()> {
    if !source.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(source)?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            add_tree(zip, &path, &format!("{prefix}/{name}"), options)?;
        } else if path.is_file() {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            zip.start_file(format!("{prefix}/{name}"), options)
                .map_err(|error| CoreError::Unsupported(error.to_string()))?;
            zip.write_all(&std::fs::read(&path)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_local_mrpack_with_overrides() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mcl-mrpack-export-{stamp}"));
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        std::fs::create_dir_all(dir.join("config/sub")).unwrap();
        std::fs::write(dir.join("mods/a.jar"), b"jar").unwrap();
        std::fs::write(dir.join("config/sub/x.txt"), b"cfg").unwrap();
        std::fs::write(dir.join("options.txt"), b"lang:zh_CN").unwrap();

        let dest = dir.join("exports");
        let path = export_local_mrpack(
            "Test Pack",
            "1.20.1",
            &LoaderSpec::Fabric("0.16.9".into()),
            &dir,
            &dest,
        )
        .expect("导出成功");

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut index = String::new();
        archive
            .by_name("modrinth.index.json")
            .unwrap()
            .read_to_string(&mut index)
            .unwrap();
        assert!(index.contains("\"fabric-loader\""));
        assert!(index.contains("\"files\": []"));

        let mut jar = String::new();
        archive
            .by_name("overrides/mods/a.jar")
            .unwrap()
            .read_to_string(&mut jar)
            .unwrap();
        assert_eq!(jar, "jar");
        assert!(archive.by_name("overrides/config/sub/x.txt").is_ok());
        assert!(archive.by_name("overrides/options.txt").is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_index_and_skips_server_only_files() {
        let text = r#"{
            "formatVersion": 1,
            "name": "Test Pack",
            "versionId": "1.2.3",
            "summary": "演示",
            "dependencies": { "minecraft": "1.20.1", "fabric-loader": "0.14.23" },
            "files": [
                { "path": "mods/a.jar", "downloads": ["https://example.invalid/a.jar"],
                  "hashes": { "sha1": "aaaa" }, "fileSize": 10 },
                { "path": "mods/server-only.jar", "env": { "client": "unsupported" },
                  "downloads": ["https://example.invalid/s.jar"], "fileSize": 1 },
                { "path": "mods/no-url.jar", "downloads": [], "fileSize": 0 }
            ]
        }"#;

        let index = parse_index(text).expect("解析");
        assert_eq!(index.name, "Test Pack");
        assert_eq!(index.version_id, "1.2.3");
        assert_eq!(index.mc_version, "1.20.1");
        assert_eq!(index.loader, LoaderSpec::Fabric("0.14.23".into()));
        assert_eq!(
            index.files.len(),
            1,
            "server-only 与缺地址条目不应进入下载清单"
        );
        assert_eq!(index.files[0].path, "mods/a.jar");
        assert_eq!(index.total_bytes(), 10);
        assert_eq!(index.issues.len(), 1, "缺下载地址的条目应进入 issues");
    }

    #[test]
    fn rejects_missing_minecraft_dependency_and_unknown_format() {
        let missing = r#"{ "formatVersion": 1, "files": [] }"#;
        assert!(parse_index(missing).is_err());

        let future = r#"{ "formatVersion": 2, "dependencies": { "minecraft": "1.20.1" } }"#;
        let error = parse_index(future).expect_err("应拒绝未知格式版本");
        assert!(error.to_string().contains("formatVersion"));
    }

    #[test]
    fn reads_zip_index_and_extracts_client_overrides() {
        let dir = std::env::temp_dir().join("mcl-pack-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        let mrpack = dir.join("demo.mrpack");

        {
            let file = std::fs::File::create(&mrpack).expect("建 zip");
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            use std::io::Write;

            writer
                .start_file("modrinth.index.json", options)
                .expect("写清单");
            writer
                .write_all(
                    br#"{ "formatVersion": 1, "name": "Zip Pack",
                          "dependencies": { "minecraft": "1.20.1", "fabric-loader": "0.15.0" } }"#,
                )
                .expect("写清单内容");
            writer
                .start_file("overrides/config/a.txt", options)
                .expect("写");
            writer.write_all(b"config-a").expect("写");
            writer
                .start_file("client-overrides/options.txt", options)
                .expect("写");
            writer.write_all(b"options").expect("写");
            writer
                .start_file("server-overrides/server.properties", options)
                .expect("写");
            writer.write_all(b"server").expect("写");
            writer.finish().expect("收尾");
        }

        let index = read_mrpack(&mrpack).expect("读取");
        assert_eq!(index.name, "Zip Pack");
        assert_eq!(index.loader, LoaderSpec::Fabric("0.15.0".into()));

        let dest = dir.join("instance");
        std::fs::create_dir_all(&dest).expect("建实例目录");
        let copied = extract_overrides(&mrpack, &dest).expect("解压 overrides");
        assert_eq!(copied, 2, "overrides 与 client-overrides 各 1 个");
        assert_eq!(
            std::fs::read_to_string(dest.join("config/a.txt")).expect("读"),
            "config-a"
        );
        assert!(dest.join("options.txt").is_file());
        assert!(
            !dest.join("server.properties").exists(),
            "server-overrides 应被忽略"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
