//! 复制文本到系统剪贴板。
//!
//! Slint 没有公开的剪贴板 API，改为调用平台命令：Linux `wl-copy` / `xclip` / `xsel`，
//! macOS `pbcopy`，Windows `clip`。候选命令从 stdin 读入，写入后做有界探测：
//! 非零退出视为失败并换下一个候选，仍在运行或零退出视为成功。

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{CoreError, CoreResult};

/// 等待候选命令「立即退出」的有界时长：超过即视为常驻持有剪贴板
const EXIT_PROBE: Duration = Duration::from_millis(300);

enum ProbeOutcome {
    /// 已退出（成功或失败由退出码判定）
    Exited(std::process::ExitStatus),
    /// 探测期内仍在运行：常驻持有剪贴板
    Resident,
    /// 探测本身失败（读取进程状态出错）
    Failed(String),
}

/// 把 `text` 写入系统剪贴板；全部候选工具都不可用时返回可读错误
pub fn copy_text(text: &str) -> CoreResult<()> {
    try_copy_with(&clipboard_commands(), text, EXIT_PROBE)
}

/// 用给定候选命令依次尝试复制；依赖注入便于单测
fn try_copy_with(
    commands: &[(&'static str, Vec<&'static str>)],
    text: &str,
    probe: Duration,
) -> CoreResult<()> {
    let mut last_error: Option<String> = None;

    for (program, args) in commands {
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match spawned {
            Ok(child) => child,
            // 未安装或不可执行：换下一个候选
            Err(error) => {
                last_error = Some(format!("{program}: 未安装或不可执行（{error}）"));
                continue;
            }
        };

        // 写入失败通常是该命令启动即退（如无显示服务器），换下一个候选
        let write_result = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush()),
            None => Ok(()),
        };
        if let Err(error) = write_result {
            last_error = Some(format!("{program}: {error}"));
            // 子进程可能已退出，交给后台线程回收，避免僵尸
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            continue;
        }

        // 有界探测：立即退出（非零）= 失败；立即退出（零）= fork 型工具成功；
        // 仍在运行 = 常驻持有剪贴板，成功
        let outcome = {
            let started = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(exit)) => break ProbeOutcome::Exited(exit),
                    Ok(None) => {
                        if started.elapsed() >= probe {
                            break ProbeOutcome::Resident;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) => break ProbeOutcome::Failed(format!("{program}: {error}")),
                }
            }
        };

        match outcome {
            ProbeOutcome::Exited(exit) if !exit.success() => {
                last_error = Some(format!(
                    "{program}: 进程异常退出（{exit}）——工具可能不支持当前显示服务器"
                ));
                continue;
            }
            // 零退出（fork 型）或仍在运行（常驻型）：都视为成功
            ProbeOutcome::Exited(_) | ProbeOutcome::Resident => {
                // 常驻子进程交给后台线程回收，避免僵尸
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            ProbeOutcome::Failed(message) => {
                last_error = Some(message);
                continue;
            }
        }
    }

    Err(CoreError::Unsupported(format!(
        "无法写入系统剪贴板（{}）—— X11 请安装 xclip，Wayland 请安装 wl-copy",
        last_error.unwrap_or_else(|| "未找到可用工具".to_string())
    )))
}

/// 各平台候选命令（按优先级；纯函数，便于单测）
fn clipboard_commands() -> Vec<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "windows") {
        vec![("clip", vec![])]
    } else if cfg!(target_os = "macos") {
        vec![("pbcopy", vec![])]
    } else {
        vec![
            ("wl-copy", vec![]),
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_ordered_by_preference() {
        let commands = clipboard_commands();
        assert!(!commands.is_empty());

        if cfg!(target_os = "windows") {
            assert_eq!(commands[0].0, "clip");
        } else if cfg!(target_os = "macos") {
            assert_eq!(commands[0].0, "pbcopy");
        } else {
            // Wayland 优先；xclip 需显式指定 clipboard 选择区
            assert_eq!(commands[0].0, "wl-copy");
            assert_eq!(commands[1], ("xclip", vec!["-selection", "clipboard"]));
        }
    }

    #[cfg(unix)]
    #[test]
    fn failing_first_candidate_falls_through_to_next() {
        // wl-copy 在 X11 会话下启动即失败，应换下一个候选
        let commands = [
            ("sh", vec!["-c", "cat >/dev/null; exit 3"]),
            ("sh", vec!["-c", "cat >/dev/null"]),
        ];
        try_copy_with(&commands, "123456", EXIT_PROBE).expect("第二个候选应成功");
    }

    #[cfg(unix)]
    #[test]
    fn all_candidates_failing_reports_the_last_reason() {
        let commands = [
            ("sh", vec!["-c", "cat >/dev/null; exit 3"]),
            ("sh", vec!["-c", "cat >/dev/null; exit 4"]),
        ];
        let error =
            try_copy_with(&commands, "123456", EXIT_PROBE).expect_err("全部候选失败时应返回错误");
        let text = error.to_string();
        assert!(text.contains("sh"), "错误应点名失败的候选：{text}");
        assert!(text.contains("xclip"), "错误应给出安装指引：{text}");
    }

    #[cfg(unix)]
    #[test]
    fn resident_tool_counts_as_success() {
        // `sleep` 不会退出：探测期内仍在运行即视为常驻持有剪贴板
        let commands = [("sleep", vec!["2"])];
        try_copy_with(&commands, "123456", EXIT_PROBE).expect("常驻候选应视为成功");
    }
}
