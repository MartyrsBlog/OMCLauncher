//! 诊断包导出：把日志与系统信息打包成 zip，便于用户手动反馈。
//!
//! 只包含日志与启动器自述信息，不包含账号令牌与设置（代理 URL 可能含凭据）。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 导出诊断包到 `dest_dir`，返回 zip 路径。
pub fn export_diagnostics(
    logs_dir: &Path,
    dest_dir: &Path,
    system_info: &str,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dest_dir.join(format!("diagnostics-{stamp}.zip"));

    let file = std::fs::File::create(&path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("system.txt", options)?;
    zip.write_all(system_info.as_bytes())?;

    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let source = entry.path();
            if !source.is_file() {
                continue;
            }
            let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // 单个日志最多 8MB，避免诊断包失控
            let Ok(metadata) = std::fs::metadata(&source) else {
                continue;
            };
            if metadata.len() > 8 * 1024 * 1024 {
                continue;
            }
            zip.start_file(format!("logs/{name}"), options)?;
            zip.write_all(&std::fs::read(&source)?)?;
        }
    }

    zip.finish()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn exports_logs_and_system_info() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("mcl-diag-{stamp}"));
        let logs = base.join("logs");
        let dest = base.join("exports");
        std::fs::create_dir_all(&logs).expect("建日志目录");
        std::fs::write(logs.join("launcher.2026-11-02.log"), b"hello log").expect("写日志");

        let path = export_diagnostics(&logs, &dest, "version=0.1.0").expect("导出成功");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).unwrap();

        let mut system = String::new();
        archive
            .by_name("system.txt")
            .unwrap()
            .read_to_string(&mut system)
            .unwrap();
        assert_eq!(system, "version=0.1.0");

        let mut log = String::new();
        archive
            .by_name("logs/launcher.2026-11-02.log")
            .unwrap()
            .read_to_string(&mut log)
            .unwrap();
        assert_eq!(log, "hello log");

        let _ = std::fs::remove_dir_all(base);
    }
}
