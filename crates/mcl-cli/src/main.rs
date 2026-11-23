//! OMCLauncher CLI —— 核心链路验证工具
//!
//! 用途：
//! 1. M1 端到端验证（列版本 → 安装 → 启动真实游戏）
//! 2. CI 冒烟测试（无需 GUI）
//! 3. 高级用户脚本化
//!
//! 用法：
//! ```text
//! mcl-cli doctor                                   环境自检
//! mcl-cli remote [--limit N] [--kind release]      列出远端版本
//! mcl-cli install <版本> [--dir X] [--skip-assets] 安装版本
//! mcl-cli launch  <版本> [--dir X] [--name 玩家名] [--memory MB] [--quiet] 启动游戏
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcl_core::account::AccountStore;
use mcl_core::auth::microsoft::MicrosoftAuth;
use mcl_core::credential::{self, TokenKind};
use mcl_core::download::DownloadEngine;
use mcl_core::game::GameLayout;
use mcl_core::http::HttpClient;
use mcl_core::instance::{FileInstanceStore, InstanceStore};
use mcl_core::java;
use mcl_core::launch::{self, LaunchOptions};
use mcl_core::loader::forge::Flavor;
use mcl_core::models::{Account, AccountKind, VersionKind};
use mcl_core::task::{EventBus, TaskEvent};
use mcl_core::version::service::{InstallOptions, VersionService};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    let result = match command {
        "doctor" => cmd_doctor(),
        "remote" => cmd_remote(&args).await,
        "loader" => cmd_loader(&args).await,
        "search" => cmd_search(&args).await,
        "install" => cmd_install(&args).await,
        "fabric" => cmd_fabric(&args).await,
        "java" => cmd_java(&args).await,
        "launch" => cmd_launch(&args).await,
        "account" => cmd_account(&args).await,
        "login" => cmd_login(&args).await,
        "instances" => cmd_instances(),
        "repair" => cmd_repair(&args).await,
        "gc" => cmd_gc(&args),
        "pack" => cmd_pack(&args).await,
        "bench-download" => cmd_bench_download(&args).await,
        _ => {
            print_help();
            Ok(())
        }
    };

    if let Err(error) = result {
        eprintln!("\n[错误] {error}");
        std::process::exit(1);
    }
}

type CliResult = Result<(), Box<dyn std::error::Error>>;

fn print_help() {
    println!(
        "OMCLauncher CLI {version}

用法：
  mcl-cli doctor                                     环境自检（Java / 数据目录 / 下载源）
  mcl-cli remote [--limit N] [--kind release]        列出远端版本
  mcl-cli install <版本> [--dir 目录] [--skip-assets] [--shared]
                                                     安装版本（--shared 复用全局资源/类库）
  mcl-cli fabric  <游戏版本> [--dir 目录] [--loader 版本] [--skip-assets] [--shared]
                                                     安装 Fabric（原版 + Fabric）
  mcl-cli java    list | install <主版本>             Java 运行时：列出 / 下载（F-LAUNCH-3）
  mcl-cli launch  <版本> [--dir 目录] [--name 玩家名] [--memory MB] [--shared] [--dry-run] [--quiet]
                                                     启动游戏（缺 Java 自动下载；--dry-run 仅打印命令）
  mcl-cli account list | add-offline <用户名> | use <账号ID> | remove <账号ID>
                                                    账号管理
  mcl-cli login [--client-id X]                      微软登录（设备码流程）
  mcl-cli pack    list | install <精选ID> | import <文件.mrpack> [--skip-assets]
                                                    整合包：列出精选 / 安装 / 导入（F-VER-4）
  mcl-cli instances                                  列出已登记实例（维护命令参考）
  mcl-cli repair  <实例ID>                           修复实例（按本地清单校验并补齐文件，F-VER-7）
  mcl-cli gc [--apply]                               共享目录回收（默认 dry-run，F-VER-3 修订项）
  mcl-cli bench-download [--files N] [--concurrency C] [--qps Q]
                                                    下载吞吐实验

示例：
  mcl-cli account add-offline Steve
  mcl-cli install 1.20.1
  mcl-cli launch 1.20.1 --memory 4096

微软登录的 client_id 由开发者在构建期注入（用户零配置）：
  MCL_MSA_CLIENT_ID=<client_id> cargo build --release
调试可临时用同名环境变量或 --client-id 覆盖；
注册步骤见 README 的「微软登录」一节",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn data_dir() -> PathBuf {
    mcl_core::platform::paths::data_dir()
}

/// 实例目录：默认 `<数据目录>/minecraft`（等价 .minecraft）
fn game_dir(args: &[String]) -> PathBuf {
    arg_value(args, "--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("minecraft"))
}

/// 游戏布局：`--shared` 时 assets/libraries 落 `<数据目录>/shared`（跨实例复用）
fn game_layout(args: &[String]) -> GameLayout {
    let dir = game_dir(args);
    if has_flag(args, "--shared") {
        GameLayout::with_shared(dir, data_dir().join("shared"))
    } else {
        GameLayout::new(dir)
    }
}

/// 客户端 jar 自动探测：加载器版本 ID（`fabric-loader-0.19.5-1.20.1`）以原版
/// 版本号（`1.20.1`）结尾——在 versions 目录中找**最长后缀匹配**且存在 jar 的版本。
fn detect_client_jar(layout: &GameLayout, version_id: &str) -> Option<PathBuf> {
    let mut candidates: Vec<(usize, PathBuf)> = std::fs::read_dir(layout.root().join("versions"))
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !version_id.ends_with(&name) {
                return None;
            }
            let jar = entry.path().join(format!("{name}.jar"));
            jar.is_file().then_some((name.len(), jar))
        })
        .collect();

    candidates.sort_by_key(|(length, _)| std::cmp::Reverse(*length));
    candidates.into_iter().next().map(|(_, jar)| jar)
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == key)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|argument| argument == flag)
}

fn build_services() -> Result<(Arc<HttpClient>, VersionService), Box<dyn std::error::Error>> {
    let http = Arc::new(HttpClient::with_default_sources()?);
    let service = VersionService::new(http.clone(), data_dir().join("meta"));
    Ok((http, service))
}

// ───────────────────────────── doctor ─────────────────────────────

fn cmd_doctor() -> CliResult {
    println!("=== OMCLauncher 环境自检 ===");
    println!("版本      : {}", env!("CARGO_PKG_VERSION"));
    println!(
        "平台      : {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("数据目录  : {}", data_dir().display());

    println!("\n--- Java 运行时 ---");
    let runtimes = java::detect_javas();
    if runtimes.is_empty() {
        println!("未探测到 Java（F-LAUNCH-3 将触发自动下载）");
    } else {
        for runtime in &runtimes {
            println!("  Java {:<3} {}", runtime.major, runtime.path.display());
        }
        for required in [8u32, 17, 21] {
            match java::select_java(&runtimes, required) {
                Some(selected) => println!("  需要 Java {required}+ → {}", selected.path.display()),
                None => println!("  需要 Java {required}+ → 无可用运行时"),
            }
        }
    }

    println!("\n--- 下载源 ---");
    let http = HttpClient::with_default_sources()?;
    for source in http.sources() {
        println!("  [优先级 {}] {}", source.priority(), source.name());
    }
    Ok(())
}

// ───────────────────────────── remote ─────────────────────────────

async fn cmd_remote(args: &[String]) -> CliResult {
    let limit: usize = arg_value(args, "--limit")
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let kind_filter = arg_value(args, "--kind");

    let (_, service) = build_services()?;
    let started = Instant::now();
    let manifest = service.manifest().await?;
    println!(
        "版本清单：{} 个版本（耗时 {}ms）",
        manifest.len(),
        started.elapsed().as_millis()
    );

    if let Some(latest) = mcl_core::version::service::latest_release(&manifest) {
        println!("最新正式版：{}（{}）", latest.id, latest.release_time);
    }

    println!("\n{:<24} {:<10} 发布时间", "版本", "类型");
    for summary in manifest.iter().take(limit) {
        if let Some(filter) = &kind_filter {
            let kind_matches = matches!(
                (filter.as_str(), summary.kind),
                ("release", VersionKind::Release) | ("snapshot", VersionKind::Snapshot)
            );
            if !kind_matches {
                continue;
            }
        }
        println!(
            "{:<24} {:<10} {}",
            summary.id,
            summary.kind.label(),
            summary.release_time
        );
    }
    Ok(())
}

// ───────────────────────────── install ─────────────────────────────

fn spawn_progress_printer(bus: &EventBus) {
    use std::io::Write;

    let mut receiver = bus.subscribe();
    tokio::spawn(async move {
        let mut last_print = Instant::now() - Duration::from_secs(1);
        while let Ok(event) = receiver.recv().await {
            if let TaskEvent::Progress {
                downloaded, total, ..
            } = event
            {
                if last_print.elapsed() < Duration::from_millis(400) {
                    continue;
                }
                last_print = Instant::now();
                let percent = if total > 0 {
                    downloaded as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                print!(
                    "\r  下载进度 {:>5.1}%   {:>7.1} / {:.1} MB",
                    percent,
                    downloaded as f64 / 1_048_576.0,
                    total as f64 / 1_048_576.0
                );
                let _ = std::io::stdout().flush();
            }
        }
    });
}

async fn cmd_install(args: &[String]) -> CliResult {
    let id = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("请指定版本号，例如：mcl-cli install 1.20.1")?;

    let layout = game_layout(args);
    let options = InstallOptions {
        include_assets: !has_flag(args, "--skip-assets"),
        ..Default::default()
    };

    let (http, service) = build_services()?;
    let bus = EventBus::default();
    spawn_progress_printer(&bus);
    let engine = DownloadEngine::new(http, bus, 16);

    println!("安装 {} → {}", id, layout.root().display());
    println!(
        "资源对象：{}",
        if options.include_assets {
            "下载（约 600MB）"
        } else {
            "跳过（--skip-assets）"
        }
    );

    let started = Instant::now();
    let report = service.install(&id, &layout, &engine, &options, 1).await?;
    println!();

    println!("--- 安装结果 ---");
    println!(
        "文件数    : {}（跳过已存在 {}）",
        report.files, report.skipped
    );
    println!("下载量    : {:.1} MB", report.bytes as f64 / 1_048_576.0);
    println!("natives   : 解压 {} 个文件", report.natives_extracted);
    println!("耗时      : {:.1}s", started.elapsed().as_secs_f64());

    if report.is_complete() {
        println!("状态      : ✅ 完成");
        Ok(())
    } else {
        println!("状态      : ❌ 有 {} 个文件失败", report.failures.len());
        for (path, error) in report.failures.iter().take(10) {
            println!("  {path}：{error}");
        }
        Err("安装未完成".into())
    }
}

// ───────────────────────────── fabric ─────────────────────────────

/// 安装 Fabric（原版 + Fabric 最新稳定版），
async fn cmd_fabric(args: &[String]) -> CliResult {
    let mc = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("用法：mcl-cli fabric <游戏版本> [--dir 目录] [--loader 版本] [--skip-assets]")?;

    let layout = game_layout(args);
    let options = InstallOptions {
        include_assets: !has_flag(args, "--skip-assets"),
        ..Default::default()
    };
    let loader_version = arg_value(args, "--loader");

    let (http, service) = build_services()?;
    let bus = EventBus::default();
    spawn_progress_printer(&bus);
    let engine = DownloadEngine::new(http, bus, 16);

    println!("安装 Fabric（{mc}） → {}", layout.root().display());
    let started = Instant::now();
    let (loader, report) = service
        .install_fabric(
            &mc,
            loader_version.as_deref(),
            None,
            &layout,
            &engine,
            &options,
            1,
        )
        .await?;
    println!();

    println!("--- 安装结果 ---");
    println!("加载器    : Fabric {loader}");
    println!(
        "版本标识  : {}",
        mcl_core::loader::fabric::version_id(&mc, &loader)
    );
    println!(
        "文件数    : {}（跳过已存在 {}）",
        report.files, report.skipped
    );
    println!("下载量    : {:.1} MB", report.bytes as f64 / 1_048_576.0);
    println!("natives   : 解压 {} 个文件", report.natives_extracted);
    println!("耗时      : {:.1}s", started.elapsed().as_secs_f64());

    if report.is_complete() {
        println!("状态      : ✅ 完成");
        Ok(())
    } else {
        println!("状态      : ❌ 有 {} 个文件失败", report.failures.len());
        for (path, error) in report.failures.iter().take(10) {
            println!("  {path}：{error}");
        }
        Err("安装未完成".into())
    }
}

// ───────────────────────────── java ─────────────────────────────

/// Java 运行时管理：列出（含托管目录）/ 下载官方运行时
async fn cmd_java(args: &[String]) -> CliResult {
    let action = args.get(1).map(String::as_str).unwrap_or("list");

    match action {
        "list" => {
            let runtimes = java::detect_javas();
            if runtimes.is_empty() {
                println!(
                    "未探测到 Java 运行时（含托管目录 {}）",
                    data_dir().join("java").display()
                );
            }
            for runtime in runtimes {
                println!(
                    "Java {:<3} {:<64} {}",
                    runtime.major,
                    runtime.path.display(),
                    runtime.version_string
                );
            }
        }
        "install" => {
            let major: u32 = args.get(2).and_then(|value| value.parse().ok()).ok_or(
                "用法：mcl-cli java install <主版本>（如 8 / 17 / 21）——从 Mojang 官方清单下载到托管目录",
            )?;
            let (http, _) = build_services()?;
            let bus = EventBus::default();
            spawn_progress_printer(&bus);
            let engine = DownloadEngine::new(http.clone(), bus, 32);
            let root = data_dir().join("java");

            println!("下载 Java {major} 运行时 → {}", root.display());
            let runtime = java::install_runtime(&http, &engine, major, &root, 0, None)
                .await?
                .ok_or("下载被取消")?;
            println!("✅ Java {} 就绪：{}", runtime.major, runtime.path.display());
            println!("   {}", runtime.version_string);
        }
        other => {
            return Err(format!("未知子命令：{other}（可用：list / install <主版本>）").into())
        }
    }
    Ok(())
}

/// `mcl-cli search <mod|modpack|resourcepack|shader> <关键词> [--mc 1.20.1] [--loader fabric]`
///
/// 打 Modrinth 真实接口，用于验证资源站检索链路（不进 GUI）。
async fn cmd_search(args: &[String]) -> CliResult {
    use mcl_core::resource::{self, ResourceKind, ResourceQuery};

    let kind = match args.get(1).map(String::as_str) {
        Some("mod") => ResourceKind::Mod,
        Some("modpack") => ResourceKind::Modpack,
        Some("resourcepack") => ResourceKind::ResourcePack,
        Some("shader") => ResourceKind::Shader,
        other => {
            return Err(format!("未知类别：{other:?}（mod/modpack/resourcepack/shader）").into())
        }
    };
    let text = args.get(2).cloned().unwrap_or_default();
    let query = ResourceQuery {
        text,
        mc_version: arg_value(args, "--mc"),
        loader: arg_value(args, "--loader"),
        env: arg_value(args, "--env"),
        offset: 0,
        sort: arg_value(args, "--sort").unwrap_or_else(|| "relevance".to_string()),
    };

    let (http, _) = build_services()?;
    let started = Instant::now();
    let rows = resource::search(&http, kind, &query).await?;
    println!(
        "检索到 {} 条（{} ms）",
        rows.len(),
        started.elapsed().as_millis()
    );
    for row in rows.iter().take(10) {
        println!("  {:<28} ↓{:<10} {}", row.name, row.downloads, row.id);
        if !row.summary.is_empty() {
            println!("    {}", row.summary.chars().take(60).collect::<String>());
        }
    }
    if let Some(first) = rows.first() {
        let versions = resource::versions(
            &http,
            &first.id,
            query.mc_version.as_deref(),
            query.loader.as_deref(),
        )
        .await?;
        println!("\n{} 的可用版本：{} 个", first.name, versions.len());
        for version in versions.iter().take(3) {
            let file = &version.files[0];
            println!(
                "  {} · {} · {} B · sha1 {:?}",
                version.version_number,
                file.filename,
                file.size,
                file.sha1.is_some()
            );
        }

        // `--install <实例目录>`：把首个版本落盘，验证下载链路
        if let (Some(dir), Some(version)) = (arg_value(args, "--install"), versions.first()) {
            let root = std::path::PathBuf::from(&dir);
            std::fs::create_dir_all(&root)?;
            let engine = DownloadEngine::new(
                mcl_core::http::HttpClient::with_default_sources()?.into(),
                EventBus::default(),
                8,
            );
            let path = resource::install_file(&engine, kind, &root, &version.files[0]).await?;
            println!("\n已安装 → {}", path.display());
        }
    }
    Ok(())
}

// ───────────────────────────── loader ─────────────────────────────

/// `mcl-cli loader <mc> <forge|neoforge> <版本> [--dir 实例目录] [--skip-assets]`
///
/// 自实现安装链路（镜像下载 + 本地 processor），用于验证 Forge/NeoForge 流水线。
async fn cmd_loader(args: &[String]) -> CliResult {
    let mc = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("用法：mcl-cli loader <MC 版本> <forge|neoforge> <加载器版本> [--dir X]")?;
    let kind = args
        .get(2)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("缺少加载器类型：forge | neoforge")?;
    let loader_version = args
        .get(3)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("缺少加载器版本，例如 47.2.0 / 20.4.237")?;

    let flavor = match kind.as_str() {
        "forge" => Flavor::Forge,
        "neoforge" => Flavor::NeoForge,
        other => return Err(format!("未知加载器：{other}（可用：forge / neoforge）").into()),
    };

    let root = arg_value(args, "--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("e2e").join(format!("{mc}-{kind}")));
    std::fs::create_dir_all(&root)?;
    let layout = GameLayout::new(&root);
    println!("实例目录：{}", root.display());

    let (http, service) = build_services()?;
    let bus = EventBus::default();
    spawn_progress_printer(&bus);
    let engine = DownloadEngine::new(http.clone(), bus, 16);
    let options = InstallOptions {
        include_assets: !has_flag(args, "--skip-assets"),
        cancel: None,
    };

    if !layout.version_jar(&mc).is_file() {
        println!("安装原版 {mc} …");
        service.install(&mc, &layout, &engine, &options, 0).await?;
    }

    // 与 GUI 一致：没有满足要求的运行时就自动下载（数据目录清空后正是这个场景）
    let runtime = match java::resolve_java(17) {
        Ok(runtime) => runtime,
        Err(_) => {
            println!("未找到 Java 17+，正在自动下载运行时…");
            java::ensure_java(&http, &engine, 17, 0, None)
                .await?
                .ok_or("Java 运行时下载被取消")?
        }
    };
    println!("使用 Java {}（{}）", runtime.major, runtime.path.display());

    let started = Instant::now();
    // 走与 GUI 相同的入口：installer → 合并版本 JSON → 补资源/类库
    let (id, _report) = service
        .install_forge(
            flavor,
            &mc,
            &loader_version,
            Some(&format!("{}-{}", mc, flavor.slug())),
            &layout,
            &engine,
            &options,
            0,
            &runtime.path,
            None,
        )
        .await?;
    println!(
        "✅ {} 安装完成（耗时 {:.1}s）：{}",
        id,
        started.elapsed().as_secs_f64(),
        layout.version_json(&id).display()
    );
    Ok(())
}

// ───────────────────────────── launch ─────────────────────────────

async fn cmd_launch(args: &[String]) -> CliResult {
    let id = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("请指定版本号，例如：mcl-cli launch 1.20.1")?;

    let layout = game_layout(args);
    let memory: u32 = arg_value(args, "--memory")
        .and_then(|value| value.parse().ok())
        .unwrap_or(4096);
    let echo = !has_flag(args, "--quiet");

    // 账号解析：`--name` 创建临时离线账号；否则使用账号存储中的当前账号
    let store = AccountStore::open_default()?;
    let account: Account = match arg_value(args, "--name") {
        Some(name) => Account {
            id: format!("offline:{name}"),
            name: name.clone(),
            uuid: mcl_core::auth::offline_uuid(&name),
            kind: AccountKind::Offline,
            expires_at: None,
        },
        None => store.current().cloned().ok_or(
            "当前没有账号，请先执行：mcl-cli account add-offline <用户名>（或 mcl-cli login）",
        )?,
    };

    // 令牌：离线账号使用占位符；正版账号从系统钥匙串读取
    let access_token = match account.kind {
        AccountKind::Offline => "0".to_string(),
        _ => credential::load_token(&account.id, TokenKind::MinecraftAccess)?
            .ok_or("该账号令牌不在系统钥匙串中（可能已过期或换了机器），请重新登录")?,
    };
    println!("账号      : {}（{}）", account.name, account.kind.label());

    let (http, service) = build_services()?;

    // 优先使用安装时落地的合并 JSON；否则现场解析（需要网络）
    let merged: serde_json::Value = if layout.version_json(&id).is_file() {
        serde_json::from_slice(&tokio::fs::read(layout.version_json(&id)).await?)?
    } else {
        println!("未找到已安装版本，现场解析版本 JSON…");
        let manifest = service.manifest().await?;
        service.resolve(&id, &manifest).await?
    };

    let required_major = merged
        .pointer("/javaVersion/majorVersion")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(8) as u32;
    // Java 运行时：探测 → 缺失自动下载
    let runtime = match java::resolve_java(required_major) {
        Ok(runtime) => runtime,
        Err(_) => {
            println!("未找到 Java {required_major}+，自动下载官方运行时…");
            let bus = EventBus::default();
            spawn_progress_printer(&bus);
            let engine = DownloadEngine::new(http.clone(), bus, 32);
            java::ensure_java(&http, &engine, required_major, 0, None)
                .await?
                .ok_or("Java 运行时下载被取消")?
        }
    };
    println!(
        "Java 要求 : {required_major}+  →  {}",
        runtime.path.display()
    );
    println!("Java 版本 : {}", runtime.version_string);

    let mut options = LaunchOptions::offline(runtime.path.clone(), &id, &account.name, memory);
    options.account = account.clone();
    options.access_token = access_token;
    options.version_type = merged
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("release")
        .to_string();

    // 加载器实例（Fabric 等）：版本目录只有 JSON，客户端 jar 复用原版目录（自动探测）
    if !layout.version_jar(&id).is_file() {
        match detect_client_jar(&layout, &id) {
            Some(jar) => {
                println!("客户端 jar: 复用 {}", jar.display());
                options.client_jar = Some(jar);
            }
            None => println!("警告     : 未找到客户端 jar（实例可能未安装完成）"),
        }
    }

    if let Some(hint) = launch::memory_hint(memory) {
        println!("内存提示 : {hint}");
    }

    let command = launch::build_command(&merged, &layout, &options)?;
    println!("启动命令 : {} 个参数（java {memory}MB）", command.len());

    if has_flag(args, "--dry-run") {
        println!("--- 启动命令（--dry-run，不实际启动）---");
        for (index, argument) in command.iter().enumerate() {
            println!("[{index:>2}] {argument}");
        }
        return Ok(());
    }

    let log_path = layout
        .root()
        .join("logs")
        .join(format!("{id}-{}.log", now_secs()));
    let mut game = launch::launch_game(&command, &log_path, echo).await?;

    println!("进程 PID  : {:?}", game.pid());
    println!("日志文件  : {}", log_path.display());
    println!("等待游戏退出…（Ctrl+C 可中断，中断不会结束游戏进程）\n");

    let code = game.wait().await?;
    println!("\n游戏已退出，退出码：{code:?}");

    Ok(())
}

// ───────────────────────────── account ─────────────────────────────

/// 账号管理
async fn cmd_account(args: &[String]) -> CliResult {
    let action = args.get(1).map(String::as_str).unwrap_or("list");
    let mut store = AccountStore::open_default()?;

    match action {
        "list" => {
            if store.list().is_empty() {
                println!("暂无账号。添加离线账号：mcl-cli account add-offline <用户名>");
            } else {
                println!("{:<24} {:<16} {:<8} 当前", "账号 ID", "名称", "类型");
                for account in store.list() {
                    let is_current =
                        store.current().map(|a| a.id.as_str()) == Some(account.id.as_str());
                    println!(
                        "{:<24} {:<16} {:<8} {}",
                        account.id,
                        account.name,
                        account.kind.label(),
                        if is_current { "✔" } else { "" }
                    );
                }
            }
        }
        "add-offline" => {
            let username = args
                .get(2)
                .ok_or("用法：mcl-cli account add-offline <用户名>")?;
            let account = store.add_offline(username)?;
            store.set_current(&account.id)?;
            println!(
                "已添加并切换为当前账号：{}（UUID {}，类型 {}）",
                account.name,
                account.uuid,
                account.kind.label()
            );
            println!("提示：离线账号无法联机进入正版服务器，且部分服务器会校验账号");
        }
        "use" => {
            let key = args
                .get(2)
                .ok_or("用法：mcl-cli account use <账号ID或名称>")?;
            let target = store
                .list()
                .iter()
                .find(|account| account.id == *key || account.name == *key)
                .map(|account| account.id.clone())
                .ok_or_else(|| format!("未找到账号：{key}"))?;
            store.set_current(&target)?;
            println!("当前账号已切换为 {target}");
        }
        "remove" => {
            let key = args
                .get(2)
                .ok_or("用法：mcl-cli account remove <账号ID或名称>")?;
            let target = store
                .list()
                .iter()
                .find(|account| account.id == *key || account.name == *key)
                .map(|account| account.id.clone());

            match target {
                Some(id) => {
                    store.remove(&id)?;
                    // 同步清理钥匙串中的令牌
                    let _ = credential::delete_token(&id, TokenKind::MinecraftAccess);
                    let _ = credential::delete_token(&id, TokenKind::MsaRefresh);
                    println!("已删除账号 {id}（含钥匙串令牌）");
                }
                None => println!("未找到账号：{key}"),
            }
        }
        other => {
            return Err(format!("未知子命令：{other}（可用：list/add-offline/use/remove）").into())
        }
    }

    println!("存储位置：{}", store.path().display());
    Ok(())
}

// ────────────────────────────── login ──────────────────────────────

/// 微软登录
///
/// `client_id` 解析顺序：`--client-id` 参数 → `MCL_MSA_CLIENT_ID` 环境变量 →
/// 启动器设置（`settings.json`）。**无内置默认值**：公共 ID 路线经真机验证不可行
/// （第一方应用无法由用户授权），注册步骤。
async fn cmd_login(args: &[String]) -> CliResult {
    let client_id = arg_value(args, "--client-id")
        .or_else(MicrosoftAuth::resolve_client_id)
        .ok_or(
            "未配置 client_id（开发者配置项）：请用 --client-id 参数 / \
             MCL_MSA_CLIENT_ID 环境变量 / 构建期注入（注册与注入步骤见 \
             README 的「微软登录」一节）",
        )?;

    let auth = MicrosoftAuth::new(client_id)?;

    println!("正在申请设备码…");
    let device = auth.request_device_code().await?;
    println!("\n  1. 在浏览器打开：{}", device.verification_uri);
    println!("  2. 输入代码：{}\n", device.user_code);
    if !device.message.is_empty() {
        println!("{}", device.message);
    }
    println!("等待你在浏览器完成授权…");

    let msa = auth.poll_msa_token(&device).await?;
    println!("✅ 微软授权成功，继续 Xbox Live → XSTS → Minecraft 认证链…");

    let session = auth
        .login_with_msa(&msa.access_token, msa.refresh_token.clone())
        .await?;
    println!(
        "✅ 认证链完成：{}（{}）",
        session.account.name, session.account.uuid
    );

    let mut store = AccountStore::open_default()?;
    store.upsert(session.account.clone())?;
    store.set_current(&session.account.id)?;

    match credential::save_token(
        &session.account.id,
        TokenKind::MinecraftAccess,
        &session.access_token,
    ) {
        Ok(()) => println!("🔐 Minecraft 令牌已写入系统钥匙串"),
        Err(error) => println!("⚠ 系统钥匙串不可用（{error}），令牌仅本次会话有效"),
    }
    if let Some(refresh_token) = &msa.refresh_token {
        if credential::save_token(&session.account.id, TokenKind::MsaRefresh, refresh_token).is_ok()
        {
            println!("🔐 续期令牌已写入系统钥匙串");
        }
    }

    println!("现在可用该账号启动游戏：mcl-cli launch 1.20.1");
    Ok(())
}

// ─────────────────── pack───────────────────

/// 整合包命令：`list` / `install <精选ID>` / `import <本地 .mrpack>`
async fn cmd_pack(args: &[String]) -> CliResult {
    match args.get(1).map(String::as_str) {
        Some("list") => cmd_pack_list(),
        Some("install") => cmd_pack_install(args).await,
        Some("import") => cmd_pack_import(args).await,
        other => Err(format!(
            "未知子命令：{}（可用：list / install <精选ID> / import <文件.mrpack>）",
            other.unwrap_or("(空)")
        )
        .into()),
    }
}

fn cmd_pack_list() -> CliResult {
    println!(
        "内置精选整合包（{} 个，随启动器更新）：",
        mcl_core::pack::CURATED_PACKS.len()
    );
    for pack in mcl_core::pack::CURATED_PACKS {
        println!("  {:<32} MC {} · {}", pack.id, pack.mc_version, pack.loader);
        println!(
            "      {}（清单 {:.0} KB，依赖文件按需下载）",
            pack.summary,
            pack.size as f64 / 1024.0
        );
    }
    println!("\n安装：mcl-cli pack install <ID>");
    Ok(())
}

async fn cmd_pack_install(args: &[String]) -> CliResult {
    let id = args
        .get(2)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("请指定精选整合包 ID，例如：mcl-cli pack install fabulously-optimized-1.20.1（pack list 查看）")?;
    let pack = mcl_core::pack::curated_pack(&id)
        .ok_or_else(|| format!("未知的精选整合包：{id}（mcl-cli pack list 查看）"))?;

    let options = pack_install_options(args);
    let (http, service) = build_services()?;
    let bus = EventBus::default();
    spawn_progress_printer(&bus);
    let engine = DownloadEngine::new(http, bus, 64);
    let mut store = FileInstanceStore::open_default()?;

    println!(
        "安装精选整合包「{}」（MC {} · {}）",
        pack.name, pack.mc_version, pack.loader
    );
    if !options.include_assets {
        println!("资源对象：跳过（--skip-assets；后续可用 mcl-cli repair <实例ID> 补齐）");
    }

    let started = Instant::now();
    let report =
        mcl_core::pack::install_curated_pack(&service, &mut store, &engine, pack, &options, 1)
            .await?;
    println!();
    print_pack_report(&report, started);

    if report.is_complete() {
        Ok(())
    } else {
        Err("整合包安装未完成".into())
    }
}

async fn cmd_pack_import(args: &[String]) -> CliResult {
    let path = args
        .get(2)
        .filter(|value| !value.starts_with("--"))
        .map(PathBuf::from)
        .ok_or("请指定 .mrpack 文件路径，例如：mcl-cli pack import ~/下载/my-pack.mrpack")?;
    if !path.is_file() {
        return Err(format!("文件不存在：{}", path.display()).into());
    }

    let options = pack_install_options(args);
    let (http, service) = build_services()?;
    let bus = EventBus::default();
    spawn_progress_printer(&bus);
    let engine = DownloadEngine::new(http, bus, 64);
    let mut store = FileInstanceStore::open_default()?;

    println!("导入整合包：{}", path.display());
    let started = Instant::now();
    let report =
        mcl_core::pack::install_mrpack(&service, &mut store, &engine, &path, &options, 1).await?;
    println!();
    print_pack_report(&report, started);

    if report.is_complete() {
        Ok(())
    } else {
        Err("整合包导入未完成".into())
    }
}

fn pack_install_options(args: &[String]) -> InstallOptions {
    InstallOptions {
        include_assets: !has_flag(args, "--skip-assets"),
        ..Default::default()
    }
}

fn print_pack_report(report: &mcl_core::pack::PackInstallReport, started: Instant) {
    println!("--- 整合包安装结果 ---");
    println!(
        "实例      : {}（{}）",
        report.instance.name, report.instance.id
    );
    println!("目录      : {}", report.instance.dir.display());
    println!(
        "加载器    : {}（{} 个文件）",
        report.loader, report.loader_files
    );
    println!(
        "清单文件  : {}（完好跳过 {}，重下 {}）",
        report.files,
        report.skipped,
        report.redownloaded()
    );
    println!("下载量    : {:.1} MB", report.bytes as f64 / 1_048_576.0);
    println!("overrides : 解压 {} 个文件", report.overrides);
    println!("耗时      : {:.1}s", started.elapsed().as_secs_f64());

    if !report.issues.is_empty() {
        println!("清单问题  : {} 条", report.issues.len());
        for (path, reason) in report.issues.iter().take(5) {
            println!("  ! {path}：{reason}");
        }
    }
    if report.cancelled {
        println!("状态      : ⏸ 已暂停（重跑即续传）");
    } else if report.failures.is_empty() {
        println!("状态      : ✅ 完成");
    } else {
        println!("状态      : ❌ 有 {} 个文件失败", report.failures.len());
        for (path, error) in report.failures.iter().take(10) {
            println!("  ✗ {path}：{error}");
        }
    }
}

// ─────────────────── instances / repair / gc（维护）───────────────────

/// 列出已登记实例（维护命令的参考：实例 ID / 目录 / 共享模式）
fn cmd_instances() -> CliResult {
    let store = FileInstanceStore::open_default()?;
    let instances = store.list();
    if instances.is_empty() {
        println!("（instances.json 中没有已登记的实例）");
        return Ok(());
    }

    println!("已登记实例（{} 个）：", instances.len());
    for instance in instances {
        println!(
            "  {:<30} {:<10} {:<8} {}",
            instance.id,
            instance.mc_version,
            instance.loader().label(),
            if instance.shared { "共享" } else { "隔离" }
        );
        println!("      {}", instance.dir.display());
    }
    Ok(())
}

/// 修复实例：以实例已落地的版本 JSON 为基准全量校验 SHA-1，
/// 缺失/损坏文件重新下载（完好文件按哈希跳过，不产生流量）。
async fn cmd_repair(args: &[String]) -> CliResult {
    let id = args
        .get(1)
        .filter(|value| !value.starts_with("--"))
        .cloned()
        .ok_or("请指定实例 ID，例如：mcl-cli repair vanilla-1.20.1（用 mcl-cli instances 查看）")?;

    let store = FileInstanceStore::open_default()?;
    let instance = store
        .get(&id)
        .ok_or_else(|| format!("实例不存在：{id}（用 mcl-cli instances 查看）"))?;

    let layout = GameLayout::for_instance(&instance);
    let (http, service) = build_services()?;
    let bus = EventBus::default();
    spawn_progress_printer(&bus);
    let engine = DownloadEngine::new(http, bus, 64);

    println!(
        "修复实例「{}」（{}）",
        instance.name,
        instance.dir.display()
    );
    println!(
        "校验基准：{}",
        layout.version_json(&instance.version_id()).display()
    );

    let started = Instant::now();
    let report = service
        .repair(
            &instance.version_id(),
            &instance.mc_version,
            &layout,
            &engine,
            &InstallOptions::default(),
            1,
        )
        .await?;
    println!();

    let re_downloaded = report.files.saturating_sub(report.skipped);
    println!("--- 修复结果 ---");
    println!(
        "校验文件  : {}（完好跳过 {}）",
        report.files, report.skipped
    );
    println!(
        "重新下载  : {} 个 / {:.1} MB",
        re_downloaded,
        report.bytes as f64 / 1_048_576.0
    );
    println!("耗时      : {:.1}s", started.elapsed().as_secs_f64());

    if report.is_complete() {
        println!("状态      : ✅ 完成");
        Ok(())
    } else {
        println!("状态      : ❌ 有 {} 个文件失败", report.failures.len());
        for (path, error) in report.failures.iter().take(10) {
            println!("  {path}：{error}");
        }
        Err("修复未完成".into())
    }
}

/// 共享目录回收：**默认 dry-run**（只展示将清理什么），
/// 确认无误后加 `--apply` 执行；无实例登记时由 core 拒绝（防误删）。
fn cmd_gc(args: &[String]) -> CliResult {
    let store = FileInstanceStore::open_default()?;
    let instances = store.list();
    let shared_root = data_dir().join("shared");

    let plan = mcl_core::maintenance::plan_shared_gc(&shared_root, &instances)?;

    println!("共享目录：{}", shared_root.display());
    println!(
        "已登记实例：{} 个（共享模式 {} 个）",
        instances.len(),
        instances.iter().filter(|instance| instance.shared).count()
    );
    if !plan.unknown_indexes.is_empty() {
        println!(
            "注意：{} 个资源索引读不到（{}）——对应的资源对象已保守保留",
            plan.unknown_indexes.len(),
            plan.unknown_indexes.join(", ")
        );
    }

    if plan.is_empty() {
        println!("✅ 没有可清理的文件");
        return Ok(());
    }

    println!(
        "可清理 {} 个文件（{:.1} MB）：",
        plan.candidates.len(),
        plan.total_mb()
    );
    for path in plan.candidates.iter().take(20) {
        println!("  - {}", path.display());
    }
    if plan.candidates.len() > 20 {
        println!("  …（其余 {} 个）", plan.candidates.len() - 20);
    }

    if !has_flag(args, "--apply") {
        println!("\n（dry-run：未删除任何文件；确认后加 --apply 执行）");
        return Ok(());
    }

    let report = mcl_core::maintenance::apply_shared_gc(&plan);
    println!(
        "✅ 已清理 {} 个文件，释放 {:.1} MB",
        report.removed,
        report.removed_mb()
    );
    if !report.failures.is_empty() {
        println!("⚠ {} 个文件删除失败：", report.failures.len());
        for (path, error) in report.failures.iter().take(10) {
            println!("  ✗ {path}：{error}");
        }
    }
    Ok(())
}

// ─────────────────────────── bench-download ───────────────────────────

/// 下载吞吐实验（可复现）：固定样本集，扫描「并发度 × 请求节流」
///
/// 设计要点：样本下载到独立临时目录（不复用缓存），保证每次都真实传输；
/// 样本按路径排序后截取，保证不同配置下载的是同一批文件。
async fn cmd_bench_download(args: &[String]) -> CliResult {
    let files: usize = arg_value(args, "--files")
        .and_then(|value| value.parse().ok())
        .unwrap_or(300);
    let concurrency: usize = arg_value(args, "--concurrency")
        .and_then(|value| value.parse().ok())
        .unwrap_or(16);
    let qps: f64 = arg_value(args, "--qps")
        .and_then(|value| value.parse().ok())
        .unwrap_or(mcl_core::http::DEFAULT_QPS);
    let version = arg_value(args, "--version").unwrap_or_else(|| "1.20.1".to_string());

    let http = Arc::new(HttpClient::with_default_sources()?.with_qps(qps));
    let service = VersionService::new(http.clone(), data_dir().join("meta"));

    let manifest = service.manifest().await?;
    let merged = service.resolve(&version, &manifest).await?;

    let index_url = merged
        .pointer("/assetIndex/url")
        .and_then(serde_json::Value::as_str)
        .ok_or("版本 JSON 缺少 assetIndex.url")?
        .to_string();

    let mut objects = service.asset_objects(&index_url).await?;
    objects.sort_by(|left, right| left.path.cmp(&right.path));
    objects.truncate(files);

    let total_bytes: u64 = objects.iter().map(|entry| entry.size).sum();
    let sample_dir = std::env::temp_dir().join(format!("mcl-bench-{}", now_secs()));

    let engine = DownloadEngine::new(http, EventBus::default(), concurrency);
    let started = Instant::now();
    let stats = engine.fetch_many(objects, &sample_dir, 0).await;
    let elapsed = started.elapsed().as_secs_f64();

    println!(
        "并发={:<3} QPS={:<5} 文件={:<4} 数据={:>7.1}MB 耗时={:>6.1}s 吞吐={:>5.2}MB/s 文件率={:>5.1}/s 失败={}",
        concurrency,
        if qps <= 0.0 {
            "不限".to_string()
        } else {
            format!("{qps:.0}")
        },
        stats.files,
        total_bytes as f64 / 1_048_576.0,
        elapsed,
        total_bytes as f64 / 1_048_576.0 / elapsed,
        stats.files as f64 / elapsed,
        stats.failures.len()
    );

    if !stats.failures.is_empty() {
        for (path, error) in stats.failures.iter().take(3) {
            println!("    失败样例：{path} → {error}");
        }
    }

    let _ = tokio::fs::remove_dir_all(&sample_dir).await;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
