//! 启动日志崩溃诊断。
//!
//! 只做高置信度的特征匹配：命中才给建议，避免把正常日志误判成崩溃。
//! 真正的进程退出码监控需要等 `launch_game` 暴露 wait 句柄，另行补充。

/// 诊断结果：标题 + 可执行建议
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashHint {
    pub title: String,
    pub detail: String,
}

/// 扫描日志尾部（调用方传入最近若干行），返回首个高置信度诊断。
pub fn analyze(log: &str) -> Option<CrashHint> {
    const RULES: &[(&str, &str, &str)] = &[
        (
            "java.lang.OutOfMemoryError",
            "内存不足（OutOfMemoryError）",
            "在「游戏设置 → 内存上限」调高内存（模组越多需要越大），并确认使用的是 64 位 Java。",
        ),
        (
            "UnsupportedClassVersionError",
            "Java 版本过低",
            "该实例需要更高版本的 Java：在「游戏设置 → Java」指定可执行文件，或让启动器自动下载对应版本。",
        ),
        (
            "Missing or unsupported mandatory dependencies",
            "模组依赖不完整",
            "缺少前置模组或版本不匹配：对照整合包说明补齐依赖，再检查 mods 目录里是否有重复/旧版本。",
        ),
        (
            "ModResolutionException",
            "模组依赖解析失败",
            "通常是前置缺失或版本冲突；先禁用最近添加的模组，再逐个启用以定位。",
        ),
        (
            "Mixin apply failed",
            "模组注入失败（Mixin）",
            "常见于模组与游戏/加载器版本不匹配；禁用最近更新的模组后重试。",
        ),
        (
            "java.lang.NoSuchMethodError",
            "模组/库版本不匹配",
            "某个模组调用了不存在的 API：检查模组是否适配当前游戏版本与加载器版本。",
        ),
        (
            "Pixel format not accelerated",
            "图形驱动不支持所需 OpenGL",
            "更新显卡驱动；远程桌面、虚拟机和部分核显环境可能无法提供游戏要求的 OpenGL 版本。",
        ),
        (
            "Failed to create GLFW window",
            "窗口/图形初始化失败",
            "优先更新显卡驱动；若在远程桌面或虚拟机中运行，尝试本机直接启动。",
        ),
        (
            "A fatal error has been detected by the Java Runtime Environment",
            "JVM 崩溃（hs_err）",
            "查看实例 logs 目录下的 hs_err 文件；常见原因是显卡驱动或本地库冲突。",
        ),
    ];

    RULES.iter().find_map(|(needle, title, detail)| {
        log.contains(needle).then(|| CrashHint {
            title: (*title).to_string(),
            detail: (*detail).to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_out_of_memory() {
        let hint = analyze("...\njava.lang.OutOfMemoryError: Java heap space\n...").unwrap();
        assert!(hint.title.contains("内存"));
        assert!(hint.detail.contains("内存上限"));
    }

    #[test]
    fn detects_java_version_and_mod_conflicts() {
        assert!(analyze("java.lang.UnsupportedClassVersionError: x")
            .unwrap()
            .title
            .contains("Java"));
        assert!(analyze("ModResolutionException: missing dependency")
            .unwrap()
            .title
            .contains("依赖"));
        assert!(analyze("Mixin apply failed")
            .unwrap()
            .title
            .contains("Mixin"));
    }

    #[test]
    fn clean_log_returns_none() {
        assert!(analyze("[12:00:00] [main/INFO] Setting user\nDone (2.3s)!").is_none());
    }
}
