//! 游戏启动与进程管理
//!
//! 流程：合并后的版本 JSON → 占位符替换 → classpath 组装 → 进程创建 →
//! stdout/stderr 管道转发。
//!
//! 同时支持新格式（`arguments.jvm` / `arguments.game`，1.13+）与
//! 旧格式（`minecraftArguments`）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::error::{CoreError, CoreResult};
use crate::game::GameLayout;
use crate::models::Account;
use crate::version::service::{rules_allow, rules_allow_with};

/// 启动参数
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub java: PathBuf,
    pub memory_mb: u32,
    pub version_id: String,
    pub version_type: String,
    pub account: Account,
    /// 离线账号传 `"0"`，正版账号传真实 access token
    pub access_token: String,
    pub width: u32,
    pub height: u32,
    pub extra_jvm_args: Vec<String>,
    pub extra_game_args: Vec<String>,
    /// 客户端 jar 路径覆盖。
    ///
    /// 加载器实例（Fabric 等）的版本目录只有 JSON 没有 jar——按官方启动器语义，
    /// 客户端 jar 复用继承的原版版本（`versions/<mc>/<mc>.jar`），避免每实例
    /// 复制一份 25MB。`None` 时取 `versions/<version_id>/<version_id>.jar`。
    pub client_jar: Option<PathBuf>,
    /// 需启用的游戏特性（对应版本 JSON `rules.features` 的键）。
    ///
    /// 默认为空：1.20+ 的 `--quickPlay*` 参数（`is_quick_play_*`）在未提供对应
    /// 变量值前不得启用，否则游戏会以占位符字面量作为世界名/地址触发
    /// 自定义分辨率（`has_custom_resolution`）由 `build_command` 按 width/height 自动启用。
    pub features: HashSet<String>,
}

impl LaunchOptions {
    /// 离线启动配置（M1 端到端验证使用；正版走 AuthService 获取令牌）
    pub fn offline(
        java: PathBuf,
        version_id: impl Into<String>,
        username: impl Into<String>,
        memory_mb: u32,
    ) -> Self {
        let version_id = version_id.into();
        let username = username.into();

        Self {
            java,
            memory_mb,
            version_type: "release".to_string(),
            account: Account {
                id: format!("offline:{username}"),
                name: username.clone(),
                uuid: crate::auth::offline_uuid(&username),
                kind: crate::models::AccountKind::Offline,
                expires_at: None,
            },
            access_token: "0".to_string(),
            version_id,
            width: 854,
            height: 480,
            extra_jvm_args: Vec::new(),
            extra_game_args: Vec::new(),
            features: HashSet::new(),
            client_jar: None,
        }
    }
}

/// 构建完整启动命令（首元素为 java 路径）
pub fn build_command(
    merged: &Value,
    layout: &GameLayout,
    options: &LaunchOptions,
) -> CoreResult<Vec<String>> {
    let version_id = options.version_id.as_str();
    let natives = layout.natives_dir(version_id);

    let assets_index = merged
        .pointer("/assetIndex/id")
        .and_then(Value::as_str)
        .or_else(|| merged.get("assets").and_then(Value::as_str))
        .unwrap_or("legacy")
        .to_string();

    // 客户端 jar：加载器实例复用原版 jar（见 `LaunchOptions::client_jar`）
    let client_jar = options
        .client_jar
        .clone()
        .unwrap_or_else(|| layout.version_jar(version_id));

    let classpath = build_classpath(
        &classpath_entries(merged, layout, &client_jar),
        classpath_separator(),
    );

    let mut vars: HashMap<&str, String> = HashMap::new();
    vars.insert("auth_player_name", options.account.name.clone());
    vars.insert("version_name", version_id.to_string());
    vars.insert("game_directory", layout.root().display().to_string());
    vars.insert("assets_root", layout.assets_dir().display().to_string());
    vars.insert("assets_index_name", assets_index);
    vars.insert("auth_uuid", options.account.uuid.clone());
    vars.insert("auth_access_token", options.access_token.clone());
    vars.insert("auth_session", options.access_token.clone());
    vars.insert("user_type", "msa".to_string());
    vars.insert("version_type", options.version_type.clone());
    vars.insert("natives_directory", natives.display().to_string());
    vars.insert("launcher_name", "OMCLauncher".to_string());
    vars.insert("launcher_version", env!("CARGO_PKG_VERSION").to_string());
    vars.insert("classpath", classpath);
    vars.insert("classpath_separator", classpath_separator().to_string());
    vars.insert(
        "library_directory",
        layout.libraries_dir().display().to_string(),
    );
    vars.insert("resolution_width", options.width.to_string());
    vars.insert("resolution_height", options.height.to_string());
    vars.insert("clientid", String::new());
    vars.insert("auth_xuid", String::new());

    let mut command = Vec::with_capacity(32);
    command.push(options.java.display().to_string());
    command.extend(default_jvm_args(options.memory_mb));
    command.extend(options.extra_jvm_args.iter().cloned());

    // 特性集：显式声明 + 按启动选项推导（设置了分辨率 → 启用自定义分辨率特性）
    let mut features = options.features.clone();
    if options.width > 0 && options.height > 0 {
        features.insert("has_custom_resolution".to_string());
    }

    // JVM 参数：优先版本 JSON，缺失时使用等价默认值
    let mut jvm_args = collect_argument_list(merged, "jvm", &features);
    if jvm_args.is_empty() {
        jvm_args = vec![
            format!("-Djava.library.path={}", natives.display()),
            "-cp".to_string(),
            "${classpath}".to_string(),
        ];
    }
    command.extend(substitute_all(&jvm_args, &vars));

    let main_class = merged
        .get("mainClass")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Manifest("版本 JSON 缺少 mainClass".into()))?;
    command.push(main_class.to_string());

    // 游戏参数：新版 arguments.game，旧版 minecraftArguments
    let mut game_args = collect_argument_list(merged, "game", &features);
    if game_args.is_empty() {
        game_args = legacy_game_arguments(merged).unwrap_or_default();
    }
    command.extend(substitute_all(&game_args, &vars));
    command.extend(options.extra_game_args.iter().cloned());

    Ok(command)
}

/// 收集参数列表（支持字符串与 `{rules, value}` 两种元素形态；规则按特性集评估）
fn collect_argument_list(merged: &Value, section: &str, features: &HashSet<String>) -> Vec<String> {
    let mut collected = Vec::new();

    let Some(list) = merged
        .pointer(&format!("/arguments/{section}"))
        .and_then(Value::as_array)
    else {
        return collected;
    };

    for item in list {
        match item {
            Value::String(text) => collected.push(text.clone()),
            Value::Object(_) => {
                if !rules_allow_with(item.get("rules"), features) {
                    continue;
                }
                match item.get("value") {
                    Some(Value::String(text)) => collected.push(text.clone()),
                    Some(Value::Array(values)) => {
                        for value in values {
                            if let Some(text) = value.as_str() {
                                collected.push(text.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    collected
}

/// 旧版（1.12 及以前）的 `minecraftArguments` 字符串
fn legacy_game_arguments(merged: &Value) -> Option<Vec<String>> {
    merged
        .get("minecraftArguments")
        .and_then(Value::as_str)
        .map(|text| text.split_whitespace().map(str::to_string).collect())
}

/// classpath 条目：全部规则内的类库（文件存在者）+ 客户端 jar
fn classpath_entries(merged: &Value, layout: &GameLayout, client_jar: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();

    if let Some(libraries) = merged.get("libraries").and_then(Value::as_array) {
        for library in libraries {
            if !rules_allow(library.get("rules")) {
                continue;
            }
            let Some(path) = library
                .pointer("/downloads/artifact/path")
                .and_then(Value::as_str)
            else {
                continue;
            };
            let full = layout.libraries_dir().join(path);
            if full.is_file() {
                entries.push(full);
            }
        }
    }

    entries.push(client_jar.to_path_buf());
    entries
}

fn substitute_all(args: &[String], vars: &HashMap<&str, String>) -> Vec<String> {
    args.iter()
        .map(|argument| {
            let mut result = argument.clone();
            for (key, value) in vars {
                let placeholder = format!("${{{key}}}");
                if result.contains(&placeholder) {
                    result = result.replace(&placeholder, value);
                }
            }
            result
        })
        .collect()
}

/// 按指定分隔符拼装 classpath（Windows 为 `;`，其余平台为 `:`）
pub fn build_classpath(entries: &[PathBuf], separator: char) -> String {
    entries
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(&separator.to_string())
}

/// 当前平台的 classpath 分隔符
pub fn classpath_separator() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

/// natives 解压目录（每实例独立，避免多实例并行时互相覆盖）
pub fn natives_dir(instance_dir: &Path) -> PathBuf {
    instance_dir.join("natives")
}

/// 按内存档位生成开箱即用的 JVM 参数
pub fn default_jvm_args(memory_mb: u32) -> Vec<String> {
    let mut args = vec![
        format!("-Xmx{memory_mb}M"),
        format!("-Xms{memory_mb}M"),
        "-Dfile.encoding=UTF-8".to_string(),
    ];

    if memory_mb >= 4096 {
        args.push("-XX:+UseG1GC".to_string());
        args.push("-XX:MaxGCPauseMillis=50".to_string());
    }

    args
}

/// 内存分配合理性提示
pub fn memory_hint(memory_mb: u32) -> Option<&'static str> {
    match memory_mb {
        0..=1023 => Some("内存分配过低，1.20 及以上版本建议至少 2048MB"),
        1024..=2047 => Some("建议分配至少 2048MB，模组整合包建议 4096MB"),
        2048..=32767 => None,
        _ => Some("内存分配过高可能影响系统稳定性，建议不超过物理内存的 60%"),
    }
}

/// 正在运行的游戏进程
pub struct RunningGame {
    child: tokio::process::Child,
    pid: Option<u32>,
}

impl RunningGame {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 等待游戏退出，返回退出码
    pub async fn wait(&mut self) -> CoreResult<Option<i32>> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|error| CoreError::Unsupported(format!("等待进程失败：{error}")))?;
        Ok(status.code())
    }

    pub async fn kill(&mut self) -> CoreResult<()> {
        self.child
            .kill()
            .await
            .map_err(|error| CoreError::Unsupported(format!("结束进程失败：{error}")))
    }
}

/// 启动游戏进程，并把 stdout/stderr 转发到日志文件（可选同时回显到控制台）
pub async fn launch_game(
    command: &[String],
    log_path: &Path,
    echo: bool,
) -> CoreResult<RunningGame> {
    use std::process::Stdio;

    let (program, args) = command
        .split_first()
        .ok_or_else(|| CoreError::Unsupported("启动命令为空".into()))?;

    if let Some(parent) = log_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CoreError::Unsupported(format!("启动游戏进程失败：{error}")))?;

    let pid = child.id();

    if let Some(stdout) = child.stdout.take() {
        spawn_log_pump(stdout, log_path.to_path_buf(), echo, "stdout");
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_pump(stderr, log_path.to_path_buf(), echo, "stderr");
    }

    Ok(RunningGame { child, pid })
}

/// 日志泵：逐行读取 → 落盘（+ 可选回显）
fn spawn_log_pump<R>(reader: R, log_path: PathBuf, echo: bool, tag: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let mut log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
            .ok();

        while let Ok(Some(line)) = lines.next_line().await {
            if echo {
                println!("[{tag}] {line}");
            }
            if let Some(file) = log.as_mut() {
                let _ = file.write_all(format!("[{tag}] {line}\n").as_bytes()).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classpath_uses_platform_separator() {
        let entries = vec![
            PathBuf::from("/libs/a.jar"),
            PathBuf::from("/libs/b.jar"),
            PathBuf::from("/client.jar"),
        ];
        assert_eq!(
            build_classpath(&entries, ':'),
            "/libs/a.jar:/libs/b.jar:/client.jar"
        );
        assert_eq!(
            build_classpath(&entries, ';'),
            "/libs/a.jar;/libs/b.jar;/client.jar"
        );
    }

    #[test]
    fn classpath_separator_matches_target_os() {
        if cfg!(windows) {
            assert_eq!(classpath_separator(), ';');
        } else {
            assert_eq!(classpath_separator(), ':');
        }
    }

    #[test]
    fn natives_dir_is_instance_scoped() {
        assert_eq!(
            natives_dir(Path::new("/instances/fabric-1.20.1")),
            PathBuf::from("/instances/fabric-1.20.1/natives")
        );
    }

    #[test]
    fn jvm_args_use_equal_xmx_xms_and_g1_for_large_heap() {
        let small = default_jvm_args(2048);
        assert!(small.contains(&"-Xmx2048M".to_string()));
        assert!(small.contains(&"-Xms2048M".to_string()));
        assert!(!small.contains(&"-XX:+UseG1GC".to_string()));

        let large = default_jvm_args(8192);
        assert!(large.contains(&"-XX:+UseG1GC".to_string()));
        assert!(large.contains(&"-XX:MaxGCPauseMillis=50".to_string()));
    }

    #[test]
    fn memory_hint_covers_boundaries() {
        assert!(memory_hint(512).is_some());
        assert!(memory_hint(1500).is_some());
        assert!(memory_hint(2048).is_none());
        assert!(memory_hint(8192).is_none());
        assert!(memory_hint(65536).is_some());
    }

    #[tokio::test]
    async fn builds_modern_command_with_placeholders_substituted() {
        let root = std::env::temp_dir().join("mcl-launch-modern");
        let layout = GameLayout::new(&root);

        // 造出类库文件与客户端 jar，验证 classpath 只收录真实存在的文件
        let library = layout.libraries_dir().join("a/b/1/b-1.jar");
        tokio::fs::create_dir_all(library.parent().expect("父目录"))
            .await
            .expect("建目录");
        tokio::fs::write(&library, b"jar").await.expect("写文件");
        tokio::fs::create_dir_all(layout.version_dir("1.20.1"))
            .await
            .expect("建目录");
        tokio::fs::write(layout.version_jar("1.20.1"), b"jar")
            .await
            .expect("写文件");

        let merged = json!({
            "id": "1.20.1",
            "mainClass": "net.minecraft.client.main.Main",
            "assetIndex": { "id": "5" },
            "libraries": [
                { "name": "a:b:1", "downloads": { "artifact": { "path": "a/b/1/b-1.jar" } } },
                { "name": "missing:c:1", "downloads": { "artifact": { "path": "missing/c/1/c-1.jar" } } }
            ],
            "arguments": {
                "jvm": ["-Djava.library.path=${natives_directory}", "-cp", "${classpath}"],
                "game": ["--username", "${auth_player_name}", "--uuid", "${auth_uuid}",
                         "--accessToken", "${auth_access_token}", "--assetIndex", "${assets_index_name}"]
            }
        });

        let options =
            LaunchOptions::offline(PathBuf::from("/jdk17/bin/java"), "1.20.1", "Steve", 4096);
        let command = build_command(&merged, &layout, &options).expect("构建命令");

        // java 路径 → 内存参数 → JVM 参数 → main class → 游戏参数
        assert_eq!(command[0], "/jdk17/bin/java");
        assert!(command.contains(&"-Xmx4096M".to_string()));
        assert!(command.contains(&"net.minecraft.client.main.Main".to_string()));

        let classpath_arg = command
            .iter()
            .find(|argument| argument.contains("b-1.jar"))
            .expect("classpath 应包含类库");
        assert!(
            !classpath_arg.contains("c-1.jar"),
            "缺失的类库不应进入 classpath"
        );
        assert!(classpath_arg.contains("1.20.1.jar"), "客户端 jar 应在末位");

        assert!(command.contains(&"Steve".to_string()));
        assert!(command.contains(&"5".to_string()));
        assert!(
            !command.iter().any(|argument| argument.contains("${")),
            "内置占位符应全部替换：{command:?}"
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn builds_legacy_command_from_minecraft_arguments() {
        let root = std::env::temp_dir().join("mcl-launch-legacy");
        let layout = GameLayout::new(&root);

        let merged = json!({
            "id": "1.8.9",
            "mainClass": "net.minecraft.client.main.Main",
            "assets": "1.8",
            "minecraftArguments": "--username ${auth_player_name} --session ${auth_session} --gameDir ${game_directory}"
        });

        let options =
            LaunchOptions::offline(PathBuf::from("/jdk8/bin/java"), "1.8.9", "Alex", 2048);
        let command = build_command(&merged, &layout, &options).expect("构建命令");

        // 旧版无 arguments.jvm → 使用默认 JVM 参数
        assert!(command
            .iter()
            .any(|argument| argument.starts_with("-Djava.library.path=")));
        assert!(command.contains(&"Alex".to_string()));
        assert!(!command.iter().any(|argument| argument.contains("${")));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    /// 加载器实例：版本目录只有 JSON，客户端 jar 复用原版目录；
    /// classpath 必须使用覆盖路径而非不存在的 `versions/<fabric-id>/<fabric-id>.jar`
    #[tokio::test]
    async fn client_jar_override_replaces_loader_version_jar_in_classpath() {
        let root = std::env::temp_dir().join("mcl-launch-client-jar");
        let layout = GameLayout::new(&root);

        // 原版 jar 存在；fabric 版本目录故意不建 jar
        tokio::fs::create_dir_all(layout.version_dir("1.20.1"))
            .await
            .expect("建目录");
        tokio::fs::write(layout.version_jar("1.20.1"), b"jar")
            .await
            .expect("写文件");

        let merged = json!({
            "id": "fabric-loader-0.16.9-1.20.1",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "arguments": {
                "jvm": ["-cp", "${classpath}"],
                "game": ["--username", "${auth_player_name}"]
            }
        });

        let mut options = LaunchOptions::offline(
            PathBuf::from("/jdk17/bin/java"),
            "fabric-loader-0.16.9-1.20.1",
            "Steve",
            4096,
        );
        options.client_jar = Some(layout.version_jar("1.20.1"));

        let command = build_command(&merged, &layout, &options).expect("构建命令");
        let classpath = command
            .iter()
            .find(|argument| argument.contains("1.20.1.jar"))
            .expect("classpath 应包含原版 jar");
        assert!(
            classpath.contains("versions/1.20.1/1.20.1.jar"),
            "应指向原版目录：{classpath}"
        );
        assert!(
            !command
                .iter()
                .any(|argument| argument.contains("fabric-loader-0.16.9-1.20.1.jar")),
            "不得引用不存在的加载器版本 jar：{command:?}"
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    /// `features` 规则且无 `os` 条件，默认必须被排除，否则游戏以占位符字面量
    /// 作为世界名报「Could not find world with the provided identifier」。
    #[tokio::test]
    async fn quickplay_args_excluded_unless_feature_enabled() {
        let root = std::env::temp_dir().join("mcl-launch-quickplay");
        let layout = GameLayout::new(&root);

        let merged = json!({
            "id": "1.20.1",
            "mainClass": "net.minecraft.client.main.Main",
            "arguments": {
                "jvm": ["-cp", "${classpath}"],
                "game": [
                    "--username", "${auth_player_name}",
                    // 真实清单形态：features 规则、无 os 条件
                    { "rules": [ { "action": "allow", "features": { "is_quick_play_singleplayer": true } } ],
                      "value": ["--quickPlaySingleplayer", "${quickPlaySingleplayer}"] },
                    { "rules": [ { "action": "allow", "features": { "has_custom_resolution": true } } ],
                      "value": ["--width", "${resolution_width}", "--height", "${resolution_height}"] }
                ]
            }
        });

        let options =
            LaunchOptions::offline(PathBuf::from("/jdk17/bin/java"), "1.20.1", "Steve", 4096);
        let command = build_command(&merged, &layout, &options).expect("构建命令");

        assert!(
            !command.iter().any(|a| a.contains("quickPlay")),
            "未启用 quickplay 特性时不得注入 --quickPlay* 参数：{command:?}"
        );
        assert!(
            !command.iter().any(|a| a.contains("${")),
            "未启用的特性参数不得残留占位符：{command:?}"
        );
        // 自定义分辨率特性由 width/height 自动启用，分辨率参数保留
        assert!(command.contains(&"--width".to_string()));
        assert!(command.contains(&"854".to_string()));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
