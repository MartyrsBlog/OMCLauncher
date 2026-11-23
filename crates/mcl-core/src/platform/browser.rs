//! 用系统默认程序打开 URL 或本地路径。
//!
//! 调用平台标准命令（xdg-open / open / start），`spawn` 成功即视为已打开；
//! 仅当命令不存在或无法启动时返回错误。

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{CoreError, CoreResult};

/// 在系统默认浏览器打开 `url`
///
/// 失败时返回带原始链接的可读错误（调用方应把链接展示给用户兜底）。
pub fn open_in_browser(url: &str) -> CoreResult<()> {
    spawn_opener(url).map_err(|error| {
        CoreError::Unsupported(format!("无法自动打开浏览器（{error}），请手动访问 {url}"))
    })
}

/// 用系统默认程序打开本地路径（目录 → 文件管理器，文件 → 关联程序）
pub fn open_path(path: &Path) -> CoreResult<()> {
    let target = path.to_string_lossy().into_owned();
    spawn_opener(&target)
        .map_err(|error| CoreError::Unsupported(format!("无法打开 {target}（{error}）")))
}

/// 以平台标准「打开器」命令启动目标（URL 或路径）
fn spawn_opener(target: &str) -> Result<(), String> {
    let (program, args) = open_command(target);
    match Command::new(program)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // xdg-open / open 把请求转交给桌面环境后很快退出；用后台线程回收，
            // 避免长驻进程反复打开累积僵尸进程
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(error) => Err(format!("{program}: {error}")),
    }
}

/// 各平台「打开 URL」的标准命令（纯函数，便于单测）
fn open_command(url: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "windows") {
        // `start` 是 cmd 内建命令；第一个空参数是窗口标题占位
        // （否则带引号的 URL 会被当成标题而不打开）
        (
            "cmd",
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                url.to_string(),
            ],
        )
    } else if cfg!(target_os = "macos") {
        ("open", vec![url.to_string()])
    } else {
        // Linux/BSD：xdg-utils 的标准入口，X11 / Wayland 均适用
        ("xdg-open", vec![url.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_mapping_matches_target_platform() {
        let url = "https://www.microsoft.com/link";
        let (program, args) = open_command(url);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();

        if cfg!(target_os = "windows") {
            assert_eq!(program, "cmd");
            assert_eq!(args, ["/C", "start", "", url]);
        } else if cfg!(target_os = "macos") {
            assert_eq!(program, "open");
            assert_eq!(args, [url]);
        } else {
            assert_eq!(program, "xdg-open");
            assert_eq!(args, [url]);
        }
    }
}
