//! OMCLauncher 的 Slint 适配层。
//!
//! 职责边界：本文件只做**绑定**——把 `mcl-app-model` 的应用层数据
//! 转成 Slint 行模型、把 UI 回调转成命令、把 tokio 侧的事件桥接到 Slint 事件循环。
//! 业务逻辑在 `mcl-core`，与框架无关的展示/编排逻辑在 `mcl-app-model`。
//!
//! 硬性约束：`slint` 只允许出现在本 crate；将来新增 `mcl-app-gpui` 时同样只依赖
//! `mcl-app-model`，两边不共享任何 Slint 类型。
//!
//! 线程模型：
//! - UI 事件循环（Slint）持有所有 Model 与组件
//! - tokio 运行时承载网络/下载/进程任务
//! - 唯一跨线程通道：`EventBus`（tokio 侧）→ `slint::invoke_from_event_loop`（UI 侧）

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcl_app_model::{
    add_game_directory, builtin_icon, clear_instance_icon, cover_cache_dir, dir_size,
    export_diagnostics, fetch_avatar_head, format_size, game_directories, instance_by_id,
    instance_icon, instance_matches, instance_summary, last_played_label, migrate_legacy_instances,
    mod_file_path, mod_version, prune_cover_cache, prune_resource_cache, read_recent_log,
    remove_game_directory, resource_cache_dir, scan_all_roots, scan_mods,
    set_default_game_directory, set_instance_icon, IconImage, IconKind, InstallOutcome, LoaderKind,
    Services, TaskKind, TaskSignificance,
};
use mcl_core::account::AccountStore;
use mcl_core::auth::microsoft::MicrosoftAuth;
use mcl_core::credential::{self, TokenKind};
use mcl_core::game::GameLayout;
use mcl_core::instance::{FileInstanceStore, InstanceStore};
use mcl_core::models::{LoaderSpec, TaskState, VersionSummary};
use mcl_core::platform;
use mcl_core::settings::LauncherSettings;
use mcl_core::task::TaskEvent;
use slint::{Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

const VERSION_LIST_LIMIT: usize = 300;

/// 版本清单内存缓存：搜索与筛选在 UI 线程同步完成，
/// 避免每敲一个字符都重新走一遍异步清单请求。
type VersionCache = Arc<Mutex<Vec<VersionSummary>>>;

/// 确认弹窗待执行操作（移除实例 / 批量应用默认内存）
enum PendingAction {
    Remove(String),
    /// 多选批量移除（实例库 02c）
    RemoveMany(Vec<String>),
    ApplyDefaults(u32),
}

/// 启动（或继续/重试）一次任务执行：登记取消旗标 → 发布运行态 → 派发异步执行
fn spawn_task_run(
    handle: &tokio::runtime::Handle,
    services: &Arc<Services>,
    weak: &slint::Weak<AppWindow>,
    task_id: u64,
) {
    let Some(kind) = services.tasks.kind(task_id) else {
        return;
    };
    let Some(cancel) = services.tasks.start_run(task_id) else {
        return;
    };
    services.bus.publish(TaskEvent::StateChanged {
        id: task_id,
        state: TaskState::Running,
        message: String::new(),
    });

    let services = services.clone();
    let weak = weak.clone();
    handle.spawn(async move {
        let outcome = services.run_install(&kind, task_id, cancel).await;

        let (state, message, toast) = match outcome {
            Ok(InstallOutcome::Done(message)) => (TaskState::Done, message.clone(), message),
            Ok(InstallOutcome::Paused) => (
                TaskState::Paused,
                "已暂停（已下载部分保留，点「继续」续传）".to_string(),
                "任务已暂停".to_string(),
            ),
            Ok(InstallOutcome::Failed(message)) | Err(message) => (
                TaskState::Failed,
                message.clone(),
                format!("任务失败：{message}"),
            ),
        };

        services.tasks.set_state(task_id, state);
        services.bus.publish(TaskEvent::StateChanged {
            id: task_id,
            state,
            message,
        });

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_toast(SharedString::from(toast));
                if matches!(state, TaskState::Done) {
                    refresh_instances(&ui);
                    // 组件安装/修复完成后同步刷新打开中的管理页（图标、组件、设置）
                    ui.invoke_detail_refresh();
                }
            }
        });
    });
}

/// 初始化日志：`<数据目录>/logs/launcher.log`，超过 5MB 时轮转为 `launcher.log.1`。
///
/// 文件不可写时静默降级（不阻塞启动）；用户可在设置页打开日志目录或导出诊断包。
fn init_logging() {
    let logs_dir = platform::paths::data_dir().join("logs");
    if std::fs::create_dir_all(&logs_dir).is_err() {
        return;
    }
    let path = logs_dir.join("launcher.log");
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&path, logs_dir.join("launcher.log.1"));
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn main() -> Result<(), slint::PlatformError> {
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "启动器启动");

    // 服务初始化失败（例如网络栈异常）不阻塞 UI，仅提示
    let services = match Services::new() {
        Ok(services) => Arc::new(services),
        Err(error) => {
            eprintln!("[错误] 服务初始化失败：{error}");
            std::process::exit(1);
        }
    };

    let runtime = tokio::runtime::Runtime::new().expect("创建 tokio 运行时");

    let ui = AppWindow::new()?;
    let (account_name, account_subtitle, account_initial, greeting_name) = current_account_parts();
    ui.set_account_name(SharedString::from(account_name));
    ui.set_account_subtitle(SharedString::from(account_subtitle));
    ui.set_account_initial(SharedString::from(account_initial));
    ui.set_greeting_name(SharedString::from(greeting_name));

    // 首次启动的 Java 状态行 + 首页「启动环境」四行
    ui.set_home_account_line(SharedString::from(account_line_text()));
    let boot_settings = LauncherSettings::load();
    ui.set_home_memory_line(SharedString::from(format!(
        "{} MB（全局默认）",
        boot_settings.instance_defaults.memory_mb
    )));
    ui.set_home_update_banner(SharedString::default());
    ui.set_home_source_line(SharedString::from(download_source_line(
        boot_settings.download_source.as_str(),
    )));
    // 探测会起子进程，放后台，避免拖慢启动
    {
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let (hint, ready) = java_hint_text();
            let java_line = java_line_text();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_java_hint(SharedString::from(hint));
                    ui.set_java_ready(ready);
                    ui.set_home_java_line(SharedString::from(java_line));
                }
            });
        });
    }
    // 首页 Hero：编译期嵌入、启动时解码一次（Slint 未启用图片格式特性）
    let hero = hero_image();
    ui.set_has_hero(hero.size().width > 0);
    ui.set_hero_image(hero);
    let version_cache: VersionCache = Arc::new(Mutex::new(Vec::new()));

    // Fabric 官方支持的游戏版本清单：启动时预取，供版本页徽标与安装向导使用
    runtime.spawn({
        let services = Arc::clone(&services);
        let rows_services = Arc::clone(&services);
        let weak = ui.as_weak();
        let cache = Arc::clone(&version_cache);
        async move {
            services.prefetch_fabric_support().await;
            // 清单到位后重画一次版本页，补上 Fabric 徽标
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    refresh_version_rows(&ui, &rows_services, &cache);
                }
            });
        }
    });

    // 游戏异常退出：后台等待进程退出，诊断结果写回首页横幅
    {
        let mut crashes = services.subscribe_crashes();
        let weak = ui.as_weak();
        runtime.spawn(async move {
            while let Ok(notice) = crashes.recv().await {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_crash_banner(SharedString::from(format!(
                            "「{}」{}：{}",
                            notice.instance_name, notice.title, notice.detail
                        )));
                        // 与 HMCL 的 GameCrashWindow 一样：异常退出直接弹出诊断窗口
                        ui.set_crash(CrashState {
                            open: true,
                            title: SharedString::from(notice.title.clone()),
                            detail: SharedString::from(notice.detail.clone()),
                            instance_id: SharedString::from(notice.instance_id.clone()),
                        });
                    }
                });
            }
        });
    }
    migrate_legacy_instances();
    ui.set_instances(ModelRc::new(VecModel::from(instance_rows())));
    ui.set_current_instance_label(SharedString::from(current_instance_label()));
    ui.set_current_instance_name(SharedString::from(current_instance_name()));
    refresh_instance_view(&ui);
    // 左列与目录管理页共用这份数据；漏掉这一步会让「目录」组和 03b 全空
    refresh_directories(&ui);
    ui.set_versions(ModelRc::new(VecModel::from(Vec::<VersionRow>::new())));
    ui.set_tasks(ModelRc::new(VecModel::from(Vec::<TaskRow>::new())));
    ui.set_accounts(ModelRc::new(VecModel::from(account_rows())));
    spawn_account_avatars(&ui, runtime.handle());
    // 内置加载器图标（64×64 素材，安装向导与组件页共用）
    ui.set_loader_icons(loader_icons());
    let settings = LauncherSettings::load();
    set_download_concurrency(&ui, settings.download_concurrency as i32);
    refresh_cache_size(&runtime, &ui);
    ui.set_memory_hint(SharedString::from(memory_hint(
        settings.instance_defaults.memory_mb,
    )));
    update_settings(&ui, |model| {
        model.proxy_url = SharedString::from(settings.proxy_url.clone());
        model.download_source = SharedString::from(settings.download_source.clone());
        model.default_memory = settings.instance_defaults.memory_mb as i32;
        model.defaults_status = SharedString::default();
        model.data_dir = SharedString::from(platform::paths::data_dir().display().to_string());
        model.logs_dir = SharedString::from(
            platform::paths::data_dir()
                .join("logs")
                .display()
                .to_string(),
        );
    });

    bridge_events(&runtime, &ui, &services);

    load_remote_versions(runtime.handle(), &ui, &services, &version_cache);
    wire_callbacks(&runtime, &ui, &services, &version_cache);

    let result = ui.run();
    tracing::info!("启动器退出");
    result
}

/// 事件桥：tokio 侧 `EventBus` → Slint 事件循环
///
/// 任务中心完整化：进度按任务 id 分发到对应行，速率在本任务内做
/// 差分计算（跨线程只传 `Weak` 与更新闭包——Model 非 Send）。
fn bridge_events(runtime: &tokio::runtime::Runtime, ui: &AppWindow, services: &Arc<Services>) {
    let mut receiver = services.bus.subscribe();
    let weak = ui.as_weak();
    let registry = Arc::clone(services);

    runtime.spawn(async move {
        // task_id → (上次字节数, 上次时刻)：下载速率差分
        let mut speed_tracker: HashMap<u64, (u64, std::time::Instant)> = HashMap::new();

        while let Ok(event) = receiver.recv().await {
            match event {
                TaskEvent::Progress {
                    id,
                    downloaded,
                    total,
                } => {
                    let speed = {
                        let now = std::time::Instant::now();
                        let entry = speed_tracker.entry(id).or_insert_with(|| (downloaded, now));
                        let elapsed = now.duration_since(entry.1).as_secs_f64();
                        let delta = downloaded.saturating_sub(entry.0);
                        // 采样窗口 ≥0.5s 才更新速率，避免抖动（事件本身 250ms 节流）
                        let text = (elapsed >= 0.5 && delta > 0)
                            .then(|| format!("{:.1} MB/s", delta as f64 / 1_048_576.0 / elapsed));
                        if elapsed >= 0.5 {
                            *entry = (downloaded, now);
                        }
                        text
                    };

                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            apply_task_progress(&ui, id, downloaded, total, speed);
                        }
                    });
                }
                TaskEvent::StateChanged { id, state, message } => {
                    let weak = weak.clone();
                    // Java 运行时补齐后刷新首页首次启动那一行
                    let java_done = matches!(state, TaskState::Done)
                        && matches!(registry.tasks.kind(id), Some(TaskKind::InstallJava { .. }));
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            apply_task_state(&ui, id, state, &message);
                            if java_done {
                                refresh_java_hint(&ui);
                            }
                        }
                    });
                }
                _ => {}
            }
        }
    });
}

/// 按 id 定位任务行并原地更新（任务数量级为个位数，线性查找足够）
/// 聚合任务状态到右下角悬浮入口；没有未完成任务时隐藏。
fn refresh_download_summary(ui: &AppWindow) {
    let tasks = ui.get_tasks();
    let Some(vec) = tasks.as_any().downcast_ref::<VecModel<TaskRow>>() else {
        return;
    };

    let mut failed = 0usize;
    let mut active = 0usize;
    let mut paused = 0usize;
    let mut done = 0usize;
    let mut sum_speed = 0.0f64;
    let mut volume = 0.0f64;

    for index in 0..vec.row_count() {
        let Some(row) = vec.row_data(index) else {
            continue;
        };
        // 启动/修复等次要任务不进统计（HMCL 的 TaskSignificance 思路）
        if row.minor {
            continue;
        }
        volume += row.total_mb as f64 * row.progress as f64;
        match row.state.as_str() {
            "失败" => failed += 1,
            "完成" => done += 1,
            "已暂停" => paused += 1,
            _ => active += 1,
        }
        if let Ok(value) = row.speed.trim_end_matches(" MB/s").parse::<f64>() {
            sum_speed += value;
        }
    }

    let count = vec.row_count();
    let volume_text = if volume >= 1024.0 {
        format!("{:.2} GB", volume / 1024.0)
    } else {
        format!("{volume:.0} MB")
    };
    let hint = if count == 0 {
        "所有下载都会显示在这里，支持暂停与断点续传".to_string()
    } else if active > 0 {
        format!("{active} 个任务进行中 · 当前速度 {sum_speed:.1} MB/s · 镜像源自动选择")
    } else if failed > 0 {
        format!("{failed} 个任务失败 · 已下载的部分不会丢弃，可直接重试")
    } else {
        format!("没有进行中的任务 · 已完成 {done} 个")
    };
    let settings = ui.get_settings();
    let source = match settings.download_source.as_str() {
        "official" => "仅官方源",
        "mirror" => "仅 BMCLAPI 镜像",
        _ => "自动（官方优先）",
    };
    ui.set_tasks_page(TasksPageModel {
        running: active as i32,
        paused: paused as i32,
        done: done as i32,
        failed: failed as i32,
        volume: SharedString::from(volume_text),
        hint: SharedString::from(hint),
        source_hint: SharedString::from(format!(
            "下载源：{source} · 并发：{} · 缓存上限 512 MB · 失败任务不会删除已校验文件",
            settings.download_concurrency
        )),
    });
}

fn update_task_row(ui: &AppWindow, id: u64, update: impl FnOnce(TaskRow) -> TaskRow) {
    let tasks = ui.get_tasks();
    let Some(vec) = tasks.as_any().downcast_ref::<VecModel<TaskRow>>() else {
        return;
    };
    for index in 0..vec.row_count() {
        if let Some(row) = vec.row_data(index) {
            if row.id == id as i32 {
                vec.set_row_data(index, update(row));
                return;
            }
        }
    }
}

/// 进度事件 → 任务行（进度条 + 已下载量 + 速率）
fn apply_task_progress(
    ui: &AppWindow,
    id: u64,
    downloaded: u64,
    total: u64,
    speed: Option<String>,
) {
    update_task_row(ui, id, |mut row| {
        row.progress = if total > 0 {
            (downloaded as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        row.percent = SharedString::from(format!("{:.0}%", row.progress * 100.0));
        row.total_mb = (total as f64 / 1_048_576.0) as f32;
        let speed_hint = speed
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        row.detail = SharedString::from(format!(
            "已下载 {:.1} / {:.1} MB{speed_hint}",
            downloaded as f64 / 1_048_576.0,
            total as f64 / 1_048_576.0
        ));
        if let Some(speed) = speed {
            row.speed = SharedString::from(speed);
        }
        row
    });
    refresh_download_summary(ui);
}

/// 状态事件 → 任务行（状态标签 + 操作按钮 + 结果原因）
fn apply_task_state(ui: &AppWindow, id: u64, state: TaskState, message: &str) {
    update_task_row(ui, id, |mut row| {
        let (label, action) = match state {
            TaskState::Pending => ("等待中", "等待"),
            TaskState::Running => ("下载中", "暂停"),
            TaskState::Paused => ("已暂停", "继续"),
            TaskState::Done => ("完成", "完成"),
            TaskState::Failed => ("失败", "重试"),
        };
        row.state = SharedString::from(label);
        row.action = SharedString::from(action);

        if !message.is_empty() {
            row.detail = SharedString::from(message.to_string());
        }
        match state {
            TaskState::Done => {
                row.progress = 1.0;
                row.percent = SharedString::from("100%");
                row.speed = SharedString::default();
            }
            TaskState::Paused | TaskState::Failed => {
                row.speed = SharedString::default();
            }
            _ => {}
        }
        row
    });
    refresh_download_summary(ui);
}

/// 新任务行（进度 0、状态等待中）
fn add_task_row(ui: &AppWindow, id: u64, kind: &TaskKind) {
    let tasks = ui.get_tasks();
    let Some(vec) = tasks.as_any().downcast_ref::<VecModel<TaskRow>>() else {
        return;
    };
    vec.push(TaskRow {
        id: id as i32,
        title: SharedString::from(kind.title()),
        detail: SharedString::from("正在准备…"),
        speed: SharedString::default(),
        progress: 0.0,
        state: SharedString::from("等待中"),
        action: SharedString::from("暂停"),
        // 只有「安装/整合包」这类主任务进浮动汇总条
        minor: kind.significance() != TaskSignificance::Major,
        percent: SharedString::from("0%"),
        total_mb: 0.0,
    });
    refresh_download_summary(ui);
}

/// 拉取远端版本清单并填充列表（失败时提示，不阻塞界面）
fn load_remote_versions(
    handle: &tokio::runtime::Handle,
    ui: &AppWindow,
    services: &Arc<Services>,
    cache: &VersionCache,
) {
    let services = services.clone();
    let weak = ui.as_weak();
    let cache = Arc::clone(cache);

    handle.spawn(async move {
        let outcome = services.version.manifest().await;

        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match outcome {
                Ok(manifest) => {
                    if let Ok(mut guard) = cache.lock() {
                        *guard = manifest;
                    }
                    refresh_version_rows(&ui, &services, &cache);
                }
                Err(error) => {
                    ui.set_toast(SharedString::from(format!("版本清单拉取失败：{error}")));
                }
            }
        });
    });
}

/// 安装向导底部预览：新建实例 / 组件安装
/// 下载量格式化：258929242 → "258.9M"
/// 资源类目 → 卡片图标语义（1 模组 / 2 资源包 / 3 光影 / 4 整合包）
fn resource_kind(category: i32) -> i32 {
    match category {
        2 => 2,
        3 => 3,
        4 => 4,
        _ => 1,
    }
}

/// 加载器 id → 展示名
fn loader_display(id: &str) -> String {
    match id {
        "fabric" => "Fabric".to_string(),
        "quilt" => "Quilt".to_string(),
        "forge" => "Forge".to_string(),
        "neoforge" => "NeoForge".to_string(),
        "liteloader" => "LiteLoader".to_string(),
        other => other.to_string(),
    }
}

/// 卡片底部标签：最多两个加载器 + 最新兼容版本
fn resource_tags(row: &mcl_core::resource::ResourceSummary) -> String {
    // Modrinth 的搜索命中把加载器放在 categories 里（与 optimization 等标签混在一起）
    const LOADERS: [&str; 5] = ["fabric", "quilt", "forge", "neoforge", "liteloader"];
    let mut parts: Vec<String> = row
        .categories
        .iter()
        .filter(|category| LOADERS.contains(&category.as_str()))
        .take(2)
        .map(|id| loader_display(id))
        .collect();
    // versions 按从旧到新返回；取最后一个正式版（跳过 rc / snapshot / pre）
    if let Some(version) = row.versions.iter().rev().find(|version| {
        !version.contains("rc") && !version.contains("snapshot") && !version.contains("pre")
    }) {
        parts.push(version.clone());
    }
    if parts.is_empty() {
        return row
            .categories
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
    }
    parts.join(" · ")
}

/// 详情页底部：`安装位置：实例 / mods`
///
/// 安装目录由类别决定（mods / resourcepacks / shaderpacks），按钮不再重复，
/// 目录信息保留在 `resource_dir` 的调用处（列表行与安装提示用）。
fn resource_target_label(_category: i32) -> String {
    "安装".to_string()
}

/// 详情页版本筛选提示
fn detail_mc_hint(mc: &str, loader: Option<&str>) -> String {
    if mc.is_empty() {
        return "显示全部版本".to_string();
    }
    match loader {
        Some(loader) => format!("仅显示 {mc} · {}", loader_display(loader)),
        None => format!("仅显示 {mc}"),
    }
}

/// 逐张拉取资源封面（Modrinth CDN）。命中本地缓存时几乎是同步返回；
/// 每拿到一张就改对应行——网格只存下标，因此不会重建列表、不会跳滚动位置。
fn spawn_resource_covers(
    weak: slint::Weak<AppWindow>,
    services: Arc<Services>,
    handle: tokio::runtime::Handle,
    items: Vec<(usize, String, String)>,
) {
    if items.is_empty() {
        return;
    }
    handle.spawn(async move {
        let mut set = tokio::task::JoinSet::new();
        for (index, id, url) in items {
            let services = Arc::clone(&services);
            set.spawn(async move { (index, id, services.resource_icon(&url).await) });
        }
        while let Some(joined) = set.join_next().await {
            let Ok((index, id, icon)) = joined else {
                continue;
            };
            let Some(icon) = icon else {
                continue;
            };
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                // slint::Image 不是 Send，只能在 UI 线程构造
                let image = to_slint_image(icon);
                let model = ui.get_resources();
                let Some(vec) = model.as_any().downcast_ref::<VecModel<ResourceCard>>() else {
                    return;
                };
                let Some(mut row) = vec.row_data(index) else {
                    return;
                };
                // 期间可能已经发起了新的检索，行被换掉就不写回
                if row.id.as_str() != id {
                    return;
                }
                row.icon = image;
                row.has_icon = true;
                vec.set_row_data(index, row);
            });
        }
    });
}

fn format_downloads(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.0}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn install_preview(
    mode: &str,
    target_name: &str,
    version: &str,
    loader: &str,
    selected: &str,
) -> String {
    let label = LoaderKind::from_id(loader)
        .map(|kind| kind.label())
        .unwrap_or("加载器");
    if mode == "component" {
        if selected.is_empty() {
            format!("将为实例「{target_name}」安装 {label}（选择版本后确定）")
        } else {
            format!("将为实例「{target_name}」安装 {label} {selected}")
        }
    } else if loader != "vanilla" {
        if selected.is_empty() {
            format!("将创建实例：{version} · {label}（选择加载器版本后确定）")
        } else {
            format!("将创建实例：{version} · {label} {selected}")
        }
    } else {
        format!("将创建实例：{version} · 原版")
    }
}

/// 按当前筛选与搜索词重建版本行（同步，数据来自内存缓存）
fn refresh_version_rows(ui: &AppWindow, services: &Arc<Services>, cache: &VersionCache) {
    let Ok(guard) = cache.lock() else {
        return;
    };
    let page = ui.get_versions_page();
    let filter = page.filter.to_string();
    let query = page.query.trim().to_lowercase();

    let mut matched = 0usize;
    let mut rows: Vec<VersionRow> = Vec::new();
    for summary in guard.iter() {
        if filter != "all" && summary.kind.label() != filter {
            continue;
        }
        if !query.is_empty() && !summary.id.to_lowercase().contains(&query) {
            continue;
        }
        matched += 1;
        if rows.len() >= VERSION_LIST_LIMIT {
            continue;
        }
        rows.push(VersionRow {
            id: SharedString::from(summary.id.clone()),
            name: SharedString::from(summary.id.clone()),
            kind: SharedString::from(summary.kind.label()),
            info: SharedString::from({
                let date = &summary.release_time[..10.min(summary.release_time.len())];
                let kind = summary.kind.label();
                if kind == "release" {
                    date.to_string()
                } else {
                    format!("{} · {date}", version_kind_zh(kind))
                }
            }),
            // 清单尚未取到时不打标（不知道 ≠ 不支持）
            fabric: services.fabric_support_cached(&summary.id).unwrap_or(false),
        });
    }

    let scope = match filter.as_str() {
        "release" => "正式版",
        "snapshot" => "快照",
        _ => "版本",
    };
    let mut page = page;
    page.installed_mc = SharedString::from(
        current_instance()
            .map(|instance| instance.mc_version)
            .unwrap_or_default(),
    );
    page.count_hint = SharedString::from(if rows.len() < matched {
        format!(
            "匹配 {matched} 个{scope} · 显示前 {} 个，搜索可精确定位",
            rows.len()
        )
    } else {
        format!("共 {matched} 个{scope}")
    });
    page.mc_choices = ModelRc::new(VecModel::from(
        std::iter::once(SharedString::from("全部"))
            .chain(rows.iter().take(40).map(|row| row.name.clone()))
            .collect::<Vec<_>>(),
    ));
    ui.set_versions_page(page);
    ui.set_versions(ModelRc::new(VecModel::from(rows)));
}

/// 图片 / 目录选择：`rfd` 的系统原生对话框
/// （Windows IFileOpenDialog / macOS NSOpenPanel / Linux GTK3）。
///
/// 三端语义一致：取消返回 `None`（上层静默），选了就返回路径。
/// 函数是阻塞的，调用方负责放到工作线程。
fn pick_image_file() -> Option<std::path::PathBuf> {
    rfd::FileDialog::new()
        .set_title("选择实例图标")
        .add_filter("图片", &["png", "jpg", "jpeg", "webp"])
        .pick_file()
}

fn pick_directory() -> Option<std::path::PathBuf> {
    rfd::FileDialog::new()
        .set_title("选择实例目录")
        .pick_folder()
}

/// 在 UI 线程弹提示（自带 2.6s 自动隐藏；`slint::Timer` 不是 Send，只能在 UI 线程建）
fn show_toast_ui(ui: &AppWindow, message: String) {
    ui.set_toast(SharedString::from(message));
    let weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(2600), move || {
        if let Some(window) = weak.upgrade() {
            window.set_toast(SharedString::default());
        }
    });
}

/// 设置里的下载源 → 安装摘要里的文案
fn download_source_hint() -> String {
    match LauncherSettings::load().download_source.as_str() {
        "official" => "仅官方源".to_string(),
        "mirror" => "仅镜像（BMCLAPI）".to_string(),
        _ => "自动（官方优先）".to_string(),
    }
}

/// 按「内容来源」分发检索：全部 = Modrinth + CurseForge 依次取回后拼接。
///
/// 全部模式下 CurseForge 走 MCIM 镜像，镜像不稳时只返回 Modrinth 的结果，
/// 不让整体失败（失败语义由单源模式承担）。
/// 一页检索结果：条目 + 站点总数 + 折算后的页数
struct ResourceSearchOutcome {
    rows: Vec<mcl_core::resource::ResourceSummary>,
    total: usize,
    pages: usize,
}

/// 按「内容来源」分发检索：全部 = Modrinth + CurseForge 各自取同一页后拼接。
///
/// 全部模式下 CurseForge 走 MCIM 镜像，镜像不稳时只返回 Modrinth 的结果，
/// 不让整体失败（失败语义由单源模式承担）。两站各取一页，因此「全部」的
/// 页数取较大的那一站，行数最多为 `2 × PAGE_SIZE`。
#[allow(clippy::too_many_arguments)]
async fn search_by_source(
    services: &Arc<Services>,
    source: &str,
    kind: &str,
    text: &str,
    page: usize,
    sort: &str,
    mc: &str,
    loader: &str,
    env: &str,
) -> Result<ResourceSearchOutcome, String> {
    use mcl_core::resource::{ResourceSource, PAGE_SIZE};

    let offset = page.saturating_sub(1) * PAGE_SIZE;
    if source != "all" {
        let source = ResourceSource::from_id(source);
        let result = services
            .resource_search_page(source, kind, text, offset, sort, mc, loader, env)
            .await?;
        return Ok(ResourceSearchOutcome {
            total: result.total_hits,
            pages: mcl_app_model::page_count(result.total_hits, PAGE_SIZE),
            rows: result.items,
        });
    }

    let modrinth = services
        .resource_search_page(
            ResourceSource::Modrinth,
            kind,
            text,
            offset,
            sort,
            mc,
            loader,
            env,
        )
        .await?;
    let mut total = modrinth.total_hits;
    let mut pages = mcl_app_model::page_count(modrinth.total_hits, PAGE_SIZE);
    let mut rows = modrinth.items;
    // CurseForge 检索没有环境维度，环境筛选只作用于 Modrinth
    if let Ok(more) = services
        .resource_search_page(
            ResourceSource::CurseForge,
            kind,
            text,
            offset,
            sort,
            mc,
            loader,
            "",
        )
        .await
    {
        total += more.total_hits;
        pages = pages.max(mcl_app_model::page_count(more.total_hits, PAGE_SIZE));
        rows.extend(more.items);
    }
    Ok(ResourceSearchOutcome { rows, total, pages })
}

/// 筛选展示名 → 站点加载器 id（空 = 不过滤）
fn loader_id(label: &str) -> &'static str {
    match label {
        "Fabric" => "fabric",
        "Forge" => "forge",
        "NeoForge" => "neoforge",
        "Quilt" => "quilt",
        _ => "",
    }
}

/// 环境展示名 → Modrinth 取值（空 = 不过滤）
fn env_id(label: &str) -> &'static str {
    match label {
        "客户端" => "client",
        "服务端" => "server",
        _ => "",
    }
}

/// 发起一次下载中心检索：回填列表、计数与页码（两处入口共用：筛选变更 / 换页）
#[allow(clippy::too_many_arguments)]
fn spawn_resource_search(
    weak: slint::Weak<AppWindow>,
    services: Arc<Services>,
    handle: tokio::runtime::Handle,
    text: String,
    category: i32,
    sort: String,
    mc: String,
    source: String,
    loader: String,
    env: String,
    page: usize,
) {
    let kind = match category {
        2 => "resourcepack",
        3 => "shader",
        4 => "modpack",
        _ => "mod",
    };
    let icons_handle = handle.clone();
    handle.spawn(async move {
        let result = search_by_source(
            &services,
            &source,
            kind,
            &text,
            page,
            &sort,
            &mc,
            loader_id(&loader),
            env_id(&env),
        )
        .await;
        let merged = source == "all";
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            match result {
                Ok(outcome) => {
                    // 页码文案与夹取统一走 model 层，和版本表一致
                    let mut pager = mcl_app_model::Pager::new(
                        page,
                        outcome.total,
                        mcl_core::resource::PAGE_SIZE,
                        "个结果",
                    )
                    .with_pages(outcome.pages);
                    if merged {
                        pager = pager.with_hint(format!(
                            "共 {} 个结果 · 两站各取 {} 个",
                            mcl_app_model::thousands(outcome.total),
                            mcl_core::resource::PAGE_SIZE
                        ));
                    }
                    model.resource_pager = PagerState {
                        page: pager.page as i32,
                        pages: pager.pages as i32,
                        hint: SharedString::from(pager.hint),
                        label: SharedString::from(pager.label),
                    };
                    model.resource_hint = SharedString::from(format!(
                        "{} 个结果",
                        mcl_app_model::thousands(outcome.total)
                    ));
                    let installed: Vec<String> = current_instance()
                        .map(|instance| services.resource_installed(&instance.id))
                        .unwrap_or_default();
                    let cards: Vec<ResourceCard> = outcome
                        .rows
                        .iter()
                        .map(|row| ResourceCard {
                            id: SharedString::from(row.id.clone()),
                            name: SharedString::from(row.name.clone()),
                            summary: SharedString::from(row.summary.clone()),
                            downloads: SharedString::from(format_downloads(row.downloads)),
                            installed: installed.iter().any(|id| id == &row.id),
                            tags: SharedString::from(resource_tags(row)),
                            kind: resource_kind(category),
                            icon: slint::Image::default(),
                            has_icon: false,
                            cover_url: SharedString::from(row.icon_url.clone().unwrap_or_default()),
                        })
                        .collect();
                    ui.set_resources(ModelRc::new(VecModel::from(cards)));
                    // 封面：先出结果再逐张回填，避免列表等网络
                    let covers: Vec<(usize, String, String)> = outcome
                        .rows
                        .iter()
                        .enumerate()
                        .filter_map(|(index, row)| {
                            row.icon_url
                                .as_ref()
                                .map(|url| (index, row.id.clone(), url.clone()))
                        })
                        .collect();
                    spawn_resource_covers(
                        ui.as_weak(),
                        Arc::clone(&services),
                        icons_handle.clone(),
                        covers,
                    );
                }
                Err(error) => {
                    model.resource_hint = SharedString::from(format!("检索失败：{error}"));
                    model.resource_pager = PagerState {
                        page: 1,
                        pages: 1,
                        hint: SharedString::default(),
                        label: SharedString::default(),
                    };
                    ui.set_resources(ModelRc::new(VecModel::from(Vec::<ResourceCard>::new())));
                }
            }
            ui.set_versions_page(model);
        });
    });
}

/// 版本类型的中文标签（列表徽标与安装向导共用）
fn version_kind_zh(label: &str) -> &'static str {
    match label {
        "release" => "正式版",
        "snapshot" => "快照",
        _ => "旧版",
    }
}

/// 安装弹窗打开后待确认的一次安装
struct PendingInstall {
    source: mcl_core::resource::ResourceSource,
    kind: String,
    project_id: String,
    project_name: String,
    // None = 安装最新匹配版本（列表入口）
    version_id: Option<String>,
    instance_id: String,
    instance_name: String,
    // 展示用：`1.20.1 · Fabric`
    instance_info: String,
    // 实例内的默认资源目录（决定弹窗是「安装」还是「保存」）
    default_dir: std::path::PathBuf,
    subdir: &'static str,
}

/// 资源类别对应的实例内目录
fn resource_subdir(kind: &str) -> &'static str {
    match kind {
        "resourcepack" => "resourcepacks",
        "shader" => "shaderpacks",
        _ => "mods",
    }
}

/// 弹窗里的提示语：实例内说明会安装，实例外只确认是否保存
fn install_hint(inside: bool, instance_name: &str, subdir: &str) -> String {
    if inside {
        format!("默认是实例的 {subdir} 目录；安装后下次启动游戏即可加载。")
    } else {
        format!("该路径不在「{instance_name}」内，是否保存？")
    }
}

fn open_install_dialog(ui: &AppWindow, pending: &PendingInstall, subtitle: String) {
    let mut state = ui.get_install_target();
    state.open = true;
    state.title = SharedString::from(format!("安装 {}", pending.project_name));
    state.subtitle = SharedString::from(subtitle);
    state.instance = SharedString::from(format!(
        "{} · {}",
        pending.instance_name, pending.instance_info
    ));
    state.path = SharedString::from(pending.default_dir.display().to_string());
    state.hint = SharedString::from(install_hint(true, &pending.instance_name, pending.subdir));
    state.inside = true;
    ui.set_install_target(state);
}

fn wire_callbacks(
    runtime: &tokio::runtime::Runtime,
    ui: &AppWindow,
    services: &Arc<Services>,
    version_cache: &VersionCache,
) {
    // UI 线程独占：Timer 非 Send/Sync，使用 Rc（跨线程只传 Weak 与 Model 更新闭包）
    let toast_timer = std::rc::Rc::new(Timer::default());

    // 启动实例：任务化（Java 自动下载的进度/暂停经任务行呈现并可见）
    ui.on_launch_instance({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move |id| {
            let instance_id = id.to_string();
            let instance_name = FileInstanceStore::open_default()
                .ok()
                .and_then(|store| store.get(&instance_id))
                .map(|instance| instance.name)
                .unwrap_or_else(|| instance_id.clone());

            let kind = TaskKind::Launch {
                instance_id,
                instance_name: instance_name.clone(),
            };
            let task_id = services.tasks.register(kind.clone());
            if let Some(ui) = weak.upgrade() {
                add_task_row(&ui, task_id, &kind);
            }
            show_toast(&weak, &toast_timer, format!("正在启动 {instance_name}…"));
            spawn_task_run(&handle, &services, &weak, task_id);
        }
    });

    // ── 实例库：行尾菜单与多选批量──
    ui.on_reveal_instance({
        move |id| {
            if let Some(instance) = instance_by_id(id.as_str()) {
                let _ = platform::browser::open_path(&instance.dir);
            }
        }
    });

    ui.on_toggle_select({
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let id = id.to_string();
            let mut ids = selected_ids(&ui);
            if let Some(index) = ids.iter().position(|item| item == &id) {
                ids.remove(index);
            } else {
                ids.push(id);
            }
            let mut page = ui.get_instances_page();
            // 勾选即进入多选态；清空仍停留在多选态，由「取消」退出
            page.selecting = true;
            ui.set_instances_page(page);
            apply_selection(&ui, ids);
        }
    });

    ui.on_start_select({
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.selecting = true;
            page.selected_count = 0;
            page.selected_ids = ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
            ui.set_instances_page(page);
            apply_selection(&ui, vec![id.to_string()]);
        }
    });

    ui.on_cancel_select({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.selecting = false;
            page.selected_count = 0;
            page.selected_ids = ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
        }
    });

    ui.on_launch_selected({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let ids = selected_ids(&ui);
            if ids.is_empty() {
                return;
            }
            // 退出多选态后再逐个派发启动任务
            let mut page = ui.get_instances_page();
            page.selecting = false;
            page.selected_count = 0;
            page.selected_ids = ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
            for id in ids {
                ui.invoke_launch_instance(SharedString::from(id));
            }
        }
    });

    ui.on_export_selected({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let ids = selected_ids(&ui);
            if ids.is_empty() {
                return;
            }
            let dest = platform::paths::data_dir().join("exports");
            let mut done = 0usize;
            let mut failed = Vec::new();
            for id in &ids {
                let Some(instance) = instance_by_id(id) else {
                    continue;
                };
                match mcl_core::pack::mrpack::export_local_mrpack(
                    &instance.name,
                    &instance.mc_version,
                    instance.loader(),
                    &instance.dir,
                    &dest,
                ) {
                    Ok(_) => done += 1,
                    Err(error) => failed.push(format!("{}：{error}", instance.name)),
                }
            }
            let _ = platform::browser::open_path(&dest);
            let mut page = ui.get_instances_page();
            page.selecting = false;
            page.selected_count = 0;
            page.selected_ids = ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
            let message = if failed.is_empty() {
                format!("已导出 {done} 个整合包到 {}", dest.display())
            } else {
                format!("已导出 {done} 个，{} 个失败", failed.len())
            };
            show_toast(&weak, &toast_timer, message);
        }
    });

    // ── 首页首次启动：登录 / 下载 Java / 导入整合包 ──
    ui.on_open_update({
        move || {
            // 更新横幅暂无数据源；入口先指向项目仓库的 releases
            let _ = platform::browser::open_in_browser("https://github.com/");
        }
    });

    ui.on_open_login({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_current_page(5);
            }
        }
    });

    ui.on_download_java({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move || {
            // 21 是当前主流实例需要的大版本；已有更高版本时 ensure_java 会直接复用
            let kind = TaskKind::InstallJava { major: 21 };
            let task_id = services.tasks.register(kind.clone());
            if let Some(ui) = weak.upgrade() {
                add_task_row(&ui, task_id, &kind);
            }
            show_toast(&weak, &toast_timer, "开始下载 Java 21…".to_string());
            spawn_task_run(&handle, &services, &weak, task_id);
        }
    });

    ui.on_pick_import_pack({
        let weak = ui.as_weak();

        move || {
            let weak = weak.clone();
            // 原生对话框阻塞，放到独立线程
            std::thread::spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_title("选择整合包")
                    .add_filter("Minecraft 整合包", &["mrpack", "zip"])
                    .pick_file();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let Some(path) = picked else {
                        return;
                    };
                    ui.invoke_import_pack(SharedString::from(path.display().to_string()));
                });
            });
        }
    });

    // 安装原版：登记任务 → 建行 → 入队执行（进度/暂停/重试全走任务中心）
    // 版本类型筛选（正式版 / 快照 / 全部），直接从内存缓存重建
    ui.on_set_version_filter({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let cache = Arc::clone(version_cache);

        move |filter| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_versions_page();
            page.filter = filter;
            ui.set_versions_page(page);
            refresh_version_rows(&ui, &services, &cache);
        }
    });

    // 版本搜索：同步过滤缓存，不等网络
    ui.on_set_version_query({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let cache = Arc::clone(version_cache);

        move |query| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_versions_page();
            page.query = query;
            ui.set_versions_page(page);
            refresh_version_rows(&ui, &services, &cache);
        }
    });

    // 安装向导：选择加载器与加载器版本
    // 首页「切换实例」：写入设置并刷新（当前实例置顶）
    ui.on_select_instance({
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut settings = LauncherSettings::load();
            settings.selected_instance = id.to_string();
            if settings.save().is_ok() {
                refresh_instances(&ui);
            }
        }
    });

    // ── 下载中心：分类 / 检索 / 安装 ──
    ui.on_set_category({
        let weak = ui.as_weak();
        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            // 已在资源分类之间切换时页面实例不变，init 不会重跑，需要显式重查；
            // 从「游戏版本」切进来时页面会新建，init 已经会拉第 1 页，避免重复请求。
            let was_resource = model.category != 0;
            model.category = value;
            model.resource_hint = SharedString::default();
            model.resource_pager = PagerState {
                page: 1,
                pages: 1,
                hint: SharedString::default(),
                label: SharedString::default(),
            };
            model.target_hint = SharedString::from(
                current_instance()
                    .map(|instance| format!("安装到：{}", instance.name))
                    .unwrap_or_else(|| "未选择实例".to_string()),
            );
            ui.set_versions_page(model.clone());
            // 切分类时清空上一次的结果，避免张冠李戴
            ui.set_resources(ModelRc::new(VecModel::from(Vec::<ResourceCard>::new())));
            if was_resource && value != 0 {
                ui.invoke_search_resources(
                    model.resource_query.clone(),
                    value,
                    model.resource_sort.clone(),
                    model.resource_mc.clone(),
                    model.resource_source.clone(),
                    model.resource_loader.clone(),
                    model.resource_env.clone(),
                    1,
                );
            }
        }
    });

    ui.on_search_resources({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();

        move |text: SharedString,
              category: i32,
              sort: SharedString,
              mc: SharedString,
              source: SharedString,
              loader: SharedString,
              env: SharedString,
              page: i32| {
            // 页码从 1 起；筛选变更时 UI 传 1，越界由 paging 负责夹取
            spawn_resource_search(
                weak.clone(),
                Arc::clone(&services),
                handle.clone(),
                text.trim().to_string(),
                category,
                sort.to_string(),
                mc.to_string(),
                source.to_string(),
                loader.to_string(),
                env.to_string(),
                page.max(1) as usize,
            );
        }
    });

    // 页码：沿用模型里已生效的筛选条件，只换页
    ui.on_goto_resource_page({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();

        move |page| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let model = ui.get_versions_page();
            spawn_resource_search(
                ui.as_weak(),
                Arc::clone(&services),
                handle.clone(),
                model.resource_query.to_string(),
                model.category,
                model.resource_sort.to_string(),
                model.resource_mc.to_string(),
                model.resource_source.to_string(),
                model.resource_loader.to_string(),
                model.resource_env.to_string(),
                page.max(1) as usize,
            );
        }
    });

    // 详情面板：当前项目的 id（关闭前一直有效）
    let detail_project = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    // 详情页已拉到的版本（更新日志/类型筛选直接复用，不重复请求）。
    // 用 Arc<Mutex>：要在异步任务与 UI 闭包之间共享。
    let detail_versions: Arc<Mutex<Vec<mcl_core::resource::ResourceVersion>>> =
        Arc::new(Mutex::new(Vec::new()));

    ui.on_open_resource({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();
        let detail_project = detail_project.clone();
        // move 闭包会拿走外层变量，这里先克隆一份给本回调用
        let detail_versions = Arc::clone(&detail_versions);

        move |id, category| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let id = id.to_string();
            // 名称/简介从当前列表取，省一次网络请求
            let cards = ui.get_resources();
            let mut title = id.clone();
            let mut summary = String::new();
            let mut cover = slint::Image::default();
            let mut has_cover = false;
            let mut cover_url = String::new();
            for index in 0..cards.row_count() {
                if let Some(card) = cards.row_data(index) {
                    if card.id == id {
                        title = card.name.to_string();
                        summary = card.summary.to_string();
                        cover = card.icon.clone();
                        has_cover = card.has_icon;
                        cover_url = card.cover_url.to_string();
                        break;
                    }
                }
            }
            *detail_project.borrow_mut() = id.clone();
            if let Ok(mut guard) = detail_versions.lock() {
                guard.clear();
            }

            let mut model = ui.get_versions_page();
            model.detail_open = true;
            model.detail_title = SharedString::from(title.clone());
            model.detail_summary = SharedString::from(summary);
            model.detail_url = SharedString::default();
            model.detail_wiki = SharedString::default();
            model.detail_stats = SharedString::default();
            model.detail_categories = SharedString::default();
            model.detail_tab = 0;
            model.detail_loading = true;
            // 新项目从第 1 页开始
            model.version_pager = PagerState {
                page: 1,
                pages: 1,
                hint: SharedString::default(),
                label: SharedString::default(),
            };
            model.detail_blocks = ModelRc::new(VecModel::from(Vec::<DescriptionBlockRow>::new()));
            model.detail_links = ModelRc::new(VecModel::from(Vec::<ResourceLinkRow>::new()));
            model.detail_stats_line = SharedString::default();
            model.detail_platform = SharedString::default();
            model.detail_loader = SharedString::default();
            model.detail_site_action = SharedString::default();
            model.changelog_version = SharedString::default();
            model.changelog_date = SharedString::default();
            model.changelog_kind = SharedString::default();
            model.changelog_lines = ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
            model.changelog_empty = SharedString::default();
            model.changelog_earlier = ModelRc::new(VecModel::from(Vec::<ChangelogRow>::new()));
            model.changelog_choices = ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
            model.version_type = SharedString::from("all");
            model.detail_icon = cover;
            model.detail_has_icon = has_cover;
            ui.set_versions_page(model);
            ui.set_current_page(7);
            ui.set_res_versions(ModelRc::new(VecModel::from(
                Vec::<ResourceVersionRow>::new(),
            )));

            // 列表里封面还没加载完时（例如直接从外部打开详情），补拉一次
            if !has_cover && !cover_url.is_empty() {
                let weak_cover = weak.clone();
                let services_cover = Arc::clone(&services);
                let handle_cover = handle.clone();
                let expect_title = title.clone();
                handle_cover.spawn(async move {
                    let Some(icon) = services_cover.resource_icon(&cover_url).await else {
                        return;
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak_cover.upgrade() else {
                            return;
                        };
                        // 期间可能已经切到别的项目
                        let mut model = ui.get_versions_page();
                        if model.detail_title.as_str() != expect_title {
                            return;
                        }
                        let image = to_slint_image(icon);
                        model.detail_icon = image;
                        model.detail_has_icon = true;
                        ui.set_versions_page(model);
                    });
                });
            }

            let weak = weak.clone();
            let services = Arc::clone(&services);
            let (mc, loader) = current_instance()
                .map(|instance| {
                    let loader = match instance.loader() {
                        LoaderSpec::Fabric(_) => Some("fabric".to_string()),
                        LoaderSpec::Quilt(_) => Some("quilt".to_string()),
                        LoaderSpec::Forge(_) => Some("forge".to_string()),
                        LoaderSpec::NeoForge(_) => Some("neoforge".to_string()),
                        LoaderSpec::Vanilla => None,
                    };
                    (instance.mc_version, loader)
                })
                .unwrap_or_default();
            let detail_source = mcl_core::resource::ResourceSource::from_id(
                ui.get_versions_page().resource_source.as_str(),
            );
            {
                let mut model = ui.get_versions_page();
                model.detail_source = SharedString::from(detail_source.id());
                model.detail_kind = resource_kind(category);
                // 版本标签的过滤行要显示实例的真实条件；写入 detail-mc，
                // 不动下载中心列表自己的 resource-mc
                model.detail_mc = SharedString::from(mc.clone());
                model.detail_loader =
                    SharedString::from(loader.as_deref().map(loader_display).unwrap_or_default());
                model.detail_target = SharedString::from(resource_target_label(category));
                model.detail_mc_hint = SharedString::from(detail_mc_hint(&mc, loader.as_deref()));
                ui.set_versions_page(model);
            }
            let services_detail = Arc::clone(&services);
            let detail_versions_call = Arc::clone(&detail_versions);
            let id_detail = id.clone();
            // 详情接口自带封面地址：封面尚未加载时用它补拉
            let cover_weak_call = weak.clone();
            let cover_services_call = Arc::clone(&services);
            let cover_handle_call = handle.clone();
            let image_handle_call = handle.clone();
            handle.spawn(async move {
                let detail = services_detail
                    .resource_detail(detail_source, &id_detail)
                    .await
                    .ok();
                // 一次取全量，三个筛选在本地做（HMCL / Modrinth 桌面端同思路）。
                // 否则下拉候选只剩当前实例那一个版本，筛选等于失效。
                let rows = services
                    .resource_versions(detail_source, &id, "", None)
                    .await
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    if let Some(detail) = detail {
                        let kind = match category {
                            2 => mcl_core::resource::ResourceKind::ResourcePack,
                            3 => mcl_core::resource::ResourceKind::Shader,
                            4 => mcl_core::resource::ResourceKind::Modpack,
                            _ => mcl_core::resource::ResourceKind::Mod,
                        };
                        let url = mcl_core::resource::project_url(kind, &detail);
                        let mut model = ui.get_versions_page();
                        model.detail_title = SharedString::from(detail.title.clone());
                        model.detail_summary = SharedString::from(detail.summary.clone());
                        model.detail_url = SharedString::from(url);
                        // MC 百科只有搜索入口：用项目标题搜
                        model.detail_wiki = SharedString::from(format!(
                            "https://search.mcmod.cn/s?key={}",
                            mcl_core::resource::urlencode(&detail.title)
                        ));
                        model.detail_stats = SharedString::from(format!(
                            "{} 下载 · {} 关注 · 更新于 {}",
                            format_downloads(detail.downloads),
                            detail.followers,
                            detail.updated.get(..10).unwrap_or(&detail.updated)
                        ));
                        model.detail_categories = SharedString::from(detail.categories.join(" / "));
                        model.detail_loading = false;
                        // ── 三标签──
                        model.detail_source_label = SharedString::from(detail.source.label());
                        model.detail_site_action = SharedString::from(detail.source.label());
                        // Modrinth 有关注数（★），CurseForge 没有
                        let mut stats =
                            format!("↓ {}", mcl_app_model::short_count(detail.downloads));
                        if detail.followers > 0 {
                            stats.push_str(&format!(
                                "  ·  ★ {}",
                                mcl_app_model::short_count(detail.followers)
                            ));
                        }
                        let updated = mcl_app_model::iso_relative_label(&detail.updated);
                        if !updated.is_empty() {
                            stats.push_str(&format!("  ·  更新于 {updated}"));
                        }
                        if !detail.license.is_empty() {
                            stats.push_str(&format!("  ·  {}", detail.license));
                        }
                        model.detail_stats_line = SharedString::from(stats);
                        let side =
                            mcl_app_model::side_label(&detail.client_side, &detail.server_side);
                        let loader_text = detail
                            .loaders
                            .iter()
                            .map(|id| mcl_app_model::loader_label(id))
                            .filter(|name| !name.is_empty())
                            .collect::<Vec<_>>()
                            .join(" / ");
                        let range = mcl_app_model::version_range(&detail.game_versions);
                        let platform = [loader_text.clone(), side, range]
                            .into_iter()
                            .filter(|part| !part.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        model.detail_platform = SharedString::from(platform);
                        let mut image_jobs: Vec<(usize, String)> = Vec::new();
                        let block_rows: Vec<DescriptionBlockRow> =
                            mcl_app_model::description_blocks(&detail.body_markdown)
                                .into_iter()
                                .enumerate()
                                .map(|(index, block)| {
                                    // 最多取前 8 张配图，避免长文一次拉几十张
                                    if block.kind == mcl_app_model::DescriptionBlockKind::Image
                                        && block.text.starts_with("http")
                                        && image_jobs.len() < 8
                                    {
                                        image_jobs.push((index, block.text.clone()));
                                    }
                                    DescriptionBlockRow {
                                        kind: SharedString::from(block.kind.id()),
                                        text: SharedString::from(block.text.clone()),
                                        image_url: SharedString::from(block.text),
                                        image: slint::Image::default(),
                                        has_image: false,
                                    }
                                })
                                .collect();
                        model.detail_blocks = ModelRc::new(VecModel::from(block_rows));
                        model.detail_links = ModelRc::new(VecModel::from(
                            detail
                                .links
                                .iter()
                                .take(4)
                                .map(|link| ResourceLinkRow {
                                    label: SharedString::from(link.label.clone()),
                                    url: SharedString::from(link.url.clone()),
                                })
                                .collect::<Vec<_>>(),
                        ));
                        let needs_cover = !model.detail_has_icon;
                        ui.set_versions_page(model);

                        // 正文配图：逐张异步拉取，解码后在 UI 线程写回对应行
                        for (index, url) in image_jobs {
                            let weak_img = weak.clone();
                            let services_img = Arc::clone(&services_detail);
                            let handle_img = image_handle_call.clone();
                            handle_img.spawn(async move {
                                let Some(icon) = services_img.resource_image(&url).await else {
                                    return;
                                };
                                let _ = slint::invoke_from_event_loop(move || {
                                    let Some(ui) = weak_img.upgrade() else {
                                        return;
                                    };
                                    let page = ui.get_versions_page();
                                    let Some(model) = page
                                        .detail_blocks
                                        .as_any()
                                        .downcast_ref::<VecModel<DescriptionBlockRow>>()
                                    else {
                                        return;
                                    };
                                    if let Some(mut row) = model.row_data(index) {
                                        row.image = to_slint_image(icon);
                                        row.has_image = true;
                                        model.set_row_data(index, row);
                                    }
                                });
                            });
                        }

                        if needs_cover {
                            if let Some(url) = detail.icon_url.clone() {
                                let handle_now = cover_handle_call.clone();
                                let services_now = Arc::clone(&cover_services_call);
                                let weak_now = cover_weak_call.clone();
                                let title_now = detail.title.clone();
                                handle_now.spawn(async move {
                                    let Some(icon) = services_now.resource_icon(&url).await else {
                                        return;
                                    };
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(ui) = weak_now.upgrade() else {
                                            return;
                                        };
                                        let mut model = ui.get_versions_page();
                                        if model.detail_title.as_str() != title_now {
                                            return;
                                        }
                                        model.detail_icon = to_slint_image(icon);
                                        model.detail_has_icon = true;
                                        ui.set_versions_page(model);
                                    });
                                });
                            }
                        }
                    }
                    {
                        if let Ok(mut guard) = detail_versions_call.lock() {
                            *guard = rows;
                        }
                    }
                    let guard = detail_versions_call.lock().ok();
                    if let Some(guard) = guard {
                        apply_detail_versions(&ui, &guard, None);
                    }
                });
            });
        }
    });

    // 安装弹窗：待确认的那一次安装（取消即丢弃）
    let pending_install: Rc<RefCell<Option<PendingInstall>>> = Rc::new(RefCell::new(None));

    ui.on_install_version({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();
        let detail_project = detail_project.clone();
        let pending_install = pending_install.clone();

        move |version_id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(instance) = current_instance() else {
                show_toast(&weak, &toast_timer, "请先创建一个实例".to_string());
                return;
            };
            let project = detail_project.borrow().clone();
            if project.is_empty() {
                return;
            }
            let project_name = ui.get_versions_page().detail_title.to_string();
            // 来源在进异步前取出：`ui` 不是 Send，不能跨 await 持有
            let source = mcl_core::resource::ResourceSource::from_id(
                ui.get_versions_page().detail_source.as_str(),
            );
            let kind = match ui.get_versions_page().category {
                2 => "resourcepack",
                3 => "shader",
                4 => "modpack",
                _ => "mod",
            }
            .to_string();
            let version_id = version_id.to_string();

            // 整合包：下载 .mrpack 后交给导入流程（会创建新实例），不弹目录窗
            if kind == "modpack" {
                let weak = weak.clone();
                let services = Arc::clone(&services);
                let handle_task = handle.clone();
                show_toast(&weak, &toast_timer, "开始下载…".to_string());
                handle.spawn(async move {
                    let result = services
                        .resource_install_version(
                            source,
                            &kind,
                            &project,
                            &project_name,
                            &version_id,
                            &instance.id,
                            None,
                        )
                        .await;
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else {
                            return;
                        };
                        match result {
                            Ok(path) => {
                                let task = TaskKind::ImportPack(path);
                                let task_id = services.tasks.register(task.clone());
                                add_task_row(&ui, task_id, &task);
                                spawn_task_run(&handle_task, &services, &weak, task_id);
                                ui.set_toast(SharedString::from("整合包已下载，正在导入…"));
                            }
                            Err(error) => {
                                ui.set_toast(SharedString::from(format!("安装失败：{error}")));
                            }
                        }
                    });
                });
                return;
            }

            // 版本号用于副标题：在已拉到的版本表里反查
            let version_label = {
                let rows = ui.get_res_versions();
                let mut label = version_id.clone();
                for index in 0..rows.row_count() {
                    if let Some(row) = rows.row_data(index) {
                        if row.id == version_id {
                            label = row.version.to_string();
                            break;
                        }
                    }
                }
                label
            };
            let subdir = resource_subdir(&kind);
            let pending = PendingInstall {
                source,
                kind,
                project_id: project,
                project_name,
                version_id: Some(version_id),
                instance_id: instance.id.clone(),
                instance_name: instance.name.clone(),
                instance_info: format!("{} · {}", instance.mc_version, instance.loader().name()),
                default_dir: instance.dir.join(subdir),
                subdir,
            };
            open_install_dialog(
                &ui,
                &pending,
                format!("{} · {}", source.label(), version_label),
            );
            *pending_install.borrow_mut() = Some(pending);
        }
    });

    ui.on_install_resource({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();
        let pending_install = pending_install.clone();

        move |project_id, category| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(instance) = current_instance() else {
                show_toast(
                    &weak,
                    &toast_timer,
                    "请先创建一个实例，再安装资源".to_string(),
                );
                return;
            };
            let kind = match category {
                2 => "resourcepack",
                3 => "shader",
                4 => "modpack",
                _ => "mod",
            }
            .to_string();
            let project_id = project_id.to_string();
            // 名字写入已装索引，供「检查更新」直接展示（否则只能看到项目 id）
            let project_name = {
                let cards = ui.get_resources();
                let mut name = project_id.clone();
                for index in 0..cards.row_count() {
                    if let Some(card) = cards.row_data(index) {
                        if card.id == project_id {
                            name = card.name.to_string();
                            break;
                        }
                    }
                }
                name
            };
            let source = mcl_core::resource::ResourceSource::from_id(
                ui.get_versions_page().resource_source.as_str(),
            );

            // 整合包：下载后走导入向导，不弹目录窗
            if kind == "modpack" {
                let weak = weak.clone();
                let services = Arc::clone(&services);
                let handle_task = handle.clone();
                show_toast(
                    &weak,
                    &toast_timer,
                    format!("开始为「{}」下载…", instance.name),
                );
                handle.spawn(async move {
                    let result = services
                        .resource_install(
                            source,
                            &kind,
                            &project_id,
                            &project_name,
                            &instance.id,
                            None,
                        )
                        .await;
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else {
                            return;
                        };
                        match result {
                            Ok(path) => {
                                let task = TaskKind::ImportPack(path);
                                let task_id = services.tasks.register(task.clone());
                                add_task_row(&ui, task_id, &task);
                                spawn_task_run(&handle_task, &services, &weak, task_id);
                                ui.set_toast(SharedString::from("整合包已下载，正在导入…"));
                            }
                            Err(error) => {
                                ui.set_toast(SharedString::from(format!("安装失败：{error}")));
                            }
                        }
                    });
                });
                return;
            }

            let subdir = resource_subdir(&kind);
            let pending = PendingInstall {
                source,
                kind,
                project_id,
                project_name,
                version_id: None,
                instance_id: instance.id.clone(),
                instance_name: instance.name.clone(),
                instance_info: format!("{} · {}", instance.mc_version, instance.loader().name()),
                default_dir: instance.dir.join(subdir),
                subdir,
            };
            open_install_dialog(&ui, &pending, format!("{} · 最新匹配版本", source.label()));
            *pending_install.borrow_mut() = Some(pending);
        }
    });

    // ── 安装弹窗：浏览目录 / 确认 / 取消 ──
    ui.on_browse_install_target({
        let weak = ui.as_weak();
        let pending_install = pending_install.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let current = ui.get_install_target().path.to_string();
            // Rc 不是 Send：把需要的字段先拷出来，再进线程
            let info = pending_install.borrow().as_ref().map(|pending| {
                (
                    pending.default_dir.clone(),
                    pending.subdir.to_string(),
                    pending.instance_name.clone(),
                )
            });
            let Some((default_dir, subdir, instance_name)) = info else {
                return;
            };
            let weak_call = weak.clone();
            std::thread::spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_title("选择安装目录")
                    .set_directory(&current)
                    .pick_folder();
                let Some(path) = picked else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_call.upgrade() else {
                        return;
                    };
                    let inside = path == default_dir;
                    let mut state = ui.get_install_target();
                    state.path = SharedString::from(path.display().to_string());
                    state.inside = inside;
                    state.hint = SharedString::from(install_hint(inside, &instance_name, &subdir));
                    ui.set_install_target(state);
                });
            });
        }
    });

    ui.on_confirm_install_target({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();
        let pending_install = pending_install.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(pending) = pending_install.borrow_mut().take() else {
                return;
            };
            let target = std::path::PathBuf::from(ui.get_install_target().path.to_string());
            let inside = target == pending.default_dir;
            // FnMut 闭包不能把外层 Arc move 出去，每次调用克隆一份
            let services = Arc::clone(&services);

            // 先关弹窗，再开始下载
            let mut state = ui.get_install_target();
            state.open = false;
            ui.set_install_target(state);

            let weak_call = weak.clone();
            handle.spawn(async move {
                let target_arg = Some(target.as_path());
                let result = match &pending.version_id {
                    Some(version) => {
                        services
                            .resource_install_version(
                                pending.source,
                                &pending.kind,
                                &pending.project_id,
                                &pending.project_name,
                                version,
                                &pending.instance_id,
                                target_arg,
                            )
                            .await
                    }
                    None => {
                        services
                            .resource_install(
                                pending.source,
                                &pending.kind,
                                &pending.project_id,
                                &pending.project_name,
                                &pending.instance_id,
                                target_arg,
                            )
                            .await
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_call.upgrade() else {
                        return;
                    };
                    match result {
                        Ok(path) => {
                            let message = if inside {
                                format!("已安装 → {path}")
                            } else {
                                format!("已保存 → {path}")
                            };
                            ui.set_toast(SharedString::from(message));
                            if inside {
                                refresh_instances(&ui);
                            }
                        }
                        Err(error) => {
                            ui.set_toast(SharedString::from(format!("操作失败：{error}")));
                        }
                    }
                });
            });
        }
    });

    ui.on_cancel_install_target({
        let weak = ui.as_weak();
        let pending_install = pending_install.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            pending_install.borrow_mut().take();
            let mut state = ui.get_install_target();
            state.open = false;
            ui.set_install_target(state);
        }
    });

    ui.on_open_install({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let cache = Arc::clone(version_cache);
        let handle = runtime.handle().clone();

        move |version| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let version = version.to_string();
            // 类型与日期只用于副标题；来自内存缓存（未命中就留空）
            let (kind, date) = cache
                .lock()
                .ok()
                .and_then(|guard| {
                    guard
                        .iter()
                        .find(|summary| summary.id == version)
                        .map(|summary| {
                            (
                                version_kind_zh(summary.kind.label()).to_string(),
                                summary.release_time[..10.min(summary.release_time.len())]
                                    .to_string(),
                            )
                        })
                })
                .unwrap_or_default();
            // 清单未取到时按“允许尝试”处理，取到后才有确定结论
            let available = services.fabric_support_cached(&version).unwrap_or(true);

            let root = LauncherSettings::load().default_instances_root();
            let name = version.clone();
            let subject = if kind.is_empty() {
                date.clone()
            } else if date.is_empty() {
                kind.clone()
            } else {
                format!("{kind} · {date}")
            };

            // 8 个内置图标 + 自定义入口（与实例设置共用同一套素材）
            let (first, second) = icon_choices();
            let mut choices = first;
            choices.extend(second.into_iter().take(1));

            let state = InstallState {
                open: false,
                mode: SharedString::from("new"),
                title: SharedString::from(format!("安装 {version}")),
                target: SharedString::default(),
                target_name: SharedString::default(),
                version: SharedString::from(version.clone()),
                kind: SharedString::from(kind),
                loader: SharedString::from("vanilla"),
                fabric_available: available,
                loader_versions: ModelRc::new(VecModel::from(Vec::<LoaderRow>::new())),
                selected: SharedString::default(),
                status: SharedString::from(if available {
                    ""
                } else {
                    "Fabric 官方清单中没有该版本，只能安装原版"
                }),
                loading: false,
                failed: false,
                preview: SharedString::from(install_preview("new", "", &version, "vanilla", "")),
                subject: SharedString::from(subject),
                name_draft: SharedString::from(name.clone()),
                location: SharedString::from(root.join(&name).display().to_string()),
                root: SharedString::default(),
                icon_kind: SharedString::from(IconKind::Grass.id()),
                icon_choices: ModelRc::new(VecModel::from(choices)),
                icon: builtin_image(IconKind::Grass),
                icon_is_file: false,
                has_icon: true,
                loaders: ModelRc::new(VecModel::from(vec![
                    InstallLoaderRow {
                        id: SharedString::from("fabric"),
                        name: SharedString::from("Fabric"),
                        hint: SharedString::from(if available { "" } else { "不支持该版本" }),
                        enabled: available,
                    },
                    InstallLoaderRow {
                        id: SharedString::from("forge"),
                        name: SharedString::from("Forge"),
                        hint: SharedString::default(),
                        enabled: true,
                    },
                    InstallLoaderRow {
                        id: SharedString::from("neoforge"),
                        name: SharedString::from("NeoForge"),
                        hint: SharedString::default(),
                        enabled: true,
                    },
                    InstallLoaderRow {
                        id: SharedString::from("quilt"),
                        name: SharedString::from("Quilt"),
                        hint: SharedString::default(),
                        enabled: true,
                    },
                ])),
                source_hint: SharedString::from(download_source_hint()),
            };
            ui.set_install(state);
            ui.set_current_page(9);

            // 各加载器的支持性来自官方 / 镜像清单：拉到空列表即「不支持该版本」，
            // 不在本地按版本号猜规则。清单同时进缓存，点选时直接出结果。
            let weak = weak.clone();
            let services = Arc::clone(&services);
            handle.spawn(async move {
                let mut marks: Vec<(String, bool, String)> = Vec::new();
                for kind in [
                    LoaderKind::Fabric,
                    LoaderKind::Forge,
                    LoaderKind::NeoForge,
                    LoaderKind::Quilt,
                ] {
                    let result = services.loader_versions_cached(kind, &version).await;
                    // 空清单 = 该加载器没有为这个版本发布安装器；请求失败单独提示，不冒充「不支持」
                    let (supported, hint) = match result {
                        Ok(rows) if !rows.is_empty() => (true, String::new()),
                        Ok(_) => (false, format!("不支持 {version}")),
                        Err(_) => (false, "版本清单获取失败".to_string()),
                    };
                    marks.push((kind.id().to_string(), supported, hint));
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let mut state = ui.get_install();
                    // 期间可能已经换了版本
                    if state.version.as_str() != version || state.mode != "new" {
                        return;
                    }
                    let rows: Vec<InstallLoaderRow> = marks
                        .into_iter()
                        .map(|(id, enabled, hint)| InstallLoaderRow {
                            name: SharedString::from(loader_display(&id)),
                            id: SharedString::from(id),
                            hint: SharedString::from(hint),
                            enabled,
                        })
                        .collect();
                    state.loaders = ModelRc::new(VecModel::from(rows));
                    ui.set_install(state);
                });
            });
        }
    });

    // ── 资源详情：三标签 / 版本筛选 / 更多版本 ──
    ui.on_set_detail_tab({
        let weak = ui.as_weak();
        let versions = Arc::clone(&detail_versions);

        move |tab| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            model.detail_tab = tab;
            ui.set_versions_page(model);
            // 另两个标签的数据由缓存重放，不重新请求
            if tab != 0 {
                if let Ok(guard) = versions.lock() {
                    if !guard.is_empty() {
                        apply_detail_versions(&ui, &guard, None);
                    }
                }
            }
        }
    });

    ui.on_select_changelog({
        let weak = ui.as_weak();
        let versions = Arc::clone(&detail_versions);

        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let id = id.to_string();
            // 选中的版本会被提到最前，回到第 1 页看结果
            let mut model = ui.get_versions_page();
            let mut pager = model.version_pager.clone();
            pager.page = 1;
            model.version_pager = pager;
            ui.set_versions_page(model);
            if let Ok(guard) = versions.lock() {
                // 下拉给的是版本号，按版本号反查 id
                let selected = guard
                    .iter()
                    .find(|row| row.version_number == id)
                    .map(|row| row.id.clone());
                apply_detail_versions(&ui, &guard, selected.as_deref());
            }
        }
    });

    ui.on_set_version_type({
        let weak = ui.as_weak();
        let versions = Arc::clone(&detail_versions);

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            model.version_type = value.clone();
            // 换筛选条件回到第 1 页
            let mut pager = model.version_pager.clone();
            pager.page = 1;
            model.version_pager = pager;
            ui.set_versions_page(model);
            if let Ok(guard) = versions.lock() {
                apply_detail_versions(&ui, &guard, None);
            }
        }
    });

    ui.on_set_detail_mc({
        let weak = ui.as_weak();
        let versions = Arc::clone(&detail_versions);

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            model.detail_mc = value.clone();
            let mut pager = model.version_pager.clone();
            pager.page = 1;
            model.version_pager = pager;
            ui.set_versions_page(model);
            if let Ok(guard) = versions.lock() {
                apply_detail_versions(&ui, &guard, None);
            }
        }
    });

    ui.on_set_detail_loader({
        let weak = ui.as_weak();
        let versions = Arc::clone(&detail_versions);

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            model.detail_loader = value.clone();
            let mut pager = model.version_pager.clone();
            pager.page = 1;
            model.version_pager = pager;
            ui.set_versions_page(model);
            if let Ok(guard) = versions.lock() {
                apply_detail_versions(&ui, &guard, None);
            }
        }
    });

    ui.on_install_recommended({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let rows = ui.get_res_versions();
            if let Some(first) = rows.row_data(0) {
                ui.invoke_install_version(first.id);
            }
        }
    });

    // 版本表换页：只重排已缓存的版本，不重新请求（详情打开时已一次取全量）
    ui.on_goto_version_page({
        let weak = ui.as_weak();
        let versions = Arc::clone(&detail_versions);

        move |page| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            let mut pager = model.version_pager.clone();
            pager.page = page.max(1);
            model.version_pager = pager;
            ui.set_versions_page(model);
            if let Ok(guard) = versions.lock() {
                apply_detail_versions(&ui, &guard, None);
            }
        }
    });

    // 组件安装：给已有实例装 Fabric
    ui.on_open_component({
        let weak = ui.as_weak();

        move |loader_id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // 参数是加载器标识；实例取当前详情页的那一个
            let Some(kind) = LoaderKind::from_id(loader_id.as_str()) else {
                ui.set_toast(SharedString::from("原版是基础版本，无需安装组件"));
                return;
            };
            let detail = ui.get_detail();
            let Some(instance) = instance_by_id(detail.id.as_str()) else {
                ui.set_toast(SharedString::from("实例不存在或已被移除"));
                return;
            };

            let state = InstallState {
                open: true,
                mode: SharedString::from("component"),
                title: SharedString::from(format!("为「{}」安装 {}", instance.name, kind.label())),
                target: SharedString::from(instance.id.clone()),
                target_name: SharedString::from(instance.name.clone()),
                version: SharedString::from(instance.mc_version.clone()),
                kind: SharedString::default(),
                loader: SharedString::from(kind.id()),
                fabric_available: true,
                loader_versions: ModelRc::new(VecModel::from(Vec::<LoaderRow>::new())),
                selected: SharedString::default(),
                status: SharedString::default(),
                loading: false,
                failed: false,
                preview: SharedString::from(install_preview(
                    "component",
                    &instance.name,
                    &instance.mc_version,
                    kind.id(),
                    "",
                )),
                subject: SharedString::default(),
                name_draft: SharedString::default(),
                location: SharedString::default(),
                root: SharedString::default(),
                icon_kind: SharedString::default(),
                icon_choices: ModelRc::new(VecModel::from(Vec::<IconChoice>::new())),
                icon: slint::Image::default(),
                icon_is_file: false,
                has_icon: false,
                loaders: ModelRc::new(VecModel::from(Vec::<InstallLoaderRow>::new())),
                source_hint: SharedString::default(),
            };
            ui.set_install(state);
            // 复用「选择加载器」的拉取逻辑（含支持性判断与重试）
            ui.invoke_select_loader(SharedString::from(kind.id()));
        }
    });

    ui.on_close_install({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let mut state = ui.get_install();
                state.open = false;
                ui.set_install(state);
            }
        }
    });

    // ── 安装设置页：实例名 / 图标 / 取消 ──
    ui.on_set_install_name({
        let weak = ui.as_weak();
        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = ui.get_install();
            let name = value.trim().to_string();
            let root = if state.root.is_empty() {
                LauncherSettings::load().default_instances_root()
            } else {
                std::path::PathBuf::from(state.root.to_string())
            };
            state.name_draft = SharedString::from(name.clone());
            state.location = SharedString::from(root.join(&name).display().to_string());
            ui.set_install(state);
        }
    });

    ui.on_pick_install_icon({
        let weak = ui.as_weak();
        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = ui.get_install();
            let id = id.to_string();
            // 空 = 跟随加载器；否则用内置图标
            let kind = if id.is_empty() {
                IconKind::for_loader_id(state.loader.as_str())
            } else {
                IconKind::from_id(&id).unwrap_or(IconKind::Grass)
            };
            state.icon_kind = SharedString::from(id);
            state.icon = builtin_image(kind);
            state.has_icon = true;
            ui.set_install(state);
        }
    });

    ui.on_pick_install_icon_file({
        let weak = ui.as_weak();

        move || {
            let weak = weak.clone();
            // 原生对话框是阻塞调用，放到独立线程；不占 UI 线程
            std::thread::spawn(move || {
                let picked = pick_image_file();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    // 取消：什么都不做
                    let Some(path) = picked else {
                        return;
                    };
                    // 图片在 UI 线程解码（slint::Image 不是 Send）
                    match slint::Image::load_from_path(&path) {
                        Ok(image) => {
                            let mut state = ui.get_install();
                            state.icon_kind =
                                SharedString::from(format!("file:{}", path.display()));
                            state.icon_is_file = true;
                            state.icon = image;
                            state.has_icon = true;
                            ui.set_install(state);
                        }
                        Err(_) => show_toast_ui(
                            &ui,
                            "这个文件不是可用的图片（支持 PNG / JPG / WebP）".to_string(),
                        ),
                    }
                });
            });
        }
    });

    ui.on_change_install_location({
        let weak = ui.as_weak();

        move || {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let picked = pick_directory();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let Some(path) = picked else {
                        return;
                    };
                    let mut state = ui.get_install();
                    let name = state.name_draft.to_string();
                    state.location = SharedString::from(path.join(&name).display().to_string());
                    state.root = SharedString::from(path.display().to_string());
                    ui.set_install(state);
                });
            });
        }
    });

    ui.on_cancel_install({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let mut state = ui.get_install();
                state.open = false;
                ui.set_install(state);
                ui.set_current_page(2);
            }
        }
    });

    // ── 资源来源切换：改模型后按新来源重拉当前检索 ──
    ui.on_set_resource_source({
        let weak = ui.as_weak();
        move |source| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            model.resource_source = source.clone();
            // CurseForge 检索没有环境维度：切过去时清掉环境筛选，避免「显示已选却不生效」
            if source.as_str() == "curseforge" {
                model.resource_env = SharedString::default();
            }
            ui.set_versions_page(model.clone());
            ui.invoke_search_resources(
                model.resource_query.clone(),
                model.category,
                model.resource_sort.clone(),
                model.resource_mc.clone(),
                source,
                model.resource_loader.clone(),
                model.resource_env.clone(),
                1,
            );
        }
    });

    ui.on_select_loader({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();

        move |loader| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = ui.get_install();
            let loader = loader.to_string();
            // 不可用的加载器卡片是禁用的；这里再兜一层，防止键盘/脚本误触
            if loader == "fabric" && !state.fabric_available {
                return;
            }

            let Some(kind) = LoaderKind::from_id(&loader) else {
                // vanilla / 未知标识：只切状态，不拉版本列表
                state.loader = SharedString::from(loader.clone());
                state.loading = false;
                state.failed = false;
                state.status = SharedString::default();
                let (mode, target, version) = (
                    state.mode.to_string(),
                    state.target_name.to_string(),
                    state.version.to_string(),
                );
                state.preview =
                    SharedString::from(install_preview(&mode, &target, &version, &loader, ""));
                ui.set_install(state);
                return;
            };

            // 换加载器一定重拉（各家版本号互不相通）
            let need_fetch =
                state.loader != loader || state.loader_versions.row_count() == 0 || state.failed;
            let mode = state.mode.to_string();
            let target_name = state.target_name.to_string();
            let version = state.version.to_string();
            state.loader = SharedString::from(loader.clone());
            state.selected = SharedString::default();
            state.failed = false;
            state.loading = need_fetch;
            state.status = SharedString::from(if need_fetch {
                format!("正在获取 {} 加载器版本…", kind.label())
            } else {
                format!("{} 个版本", state.loader_versions.row_count())
            });
            state.preview =
                SharedString::from(install_preview(&mode, &target_name, &version, &loader, ""));
            ui.set_install(state);

            if !need_fetch {
                return;
            }
            let weak = weak.clone();
            let services = Arc::clone(&services);
            handle.spawn(async move {
                let result = services.loader_versions_cached(kind, &version).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let mut state = ui.get_install();
                    // 请求期间用户可能换了加载器或游戏版本
                    if state.loader != kind.id() || state.version != version {
                        return;
                    }
                    state.loading = false;
                    match result {
                        Ok(rows) => {
                            let selected = rows
                                .first()
                                .map(|row| row.version.clone())
                                .unwrap_or_default();
                            let ui_rows: Vec<LoaderRow> = rows
                                .iter()
                                .map(|row| LoaderRow {
                                    version: SharedString::from(row.version.clone()),
                                    tag: SharedString::from(row.tag.clone()),
                                })
                                .collect();
                            state.status =
                                SharedString::from(format!("{} 个版本，默认选最新", rows.len()));
                            state.selected = SharedString::from(selected.clone());
                            state.loader_versions = ModelRc::new(VecModel::from(ui_rows));
                            state.failed = false;
                            state.preview = SharedString::from(install_preview(
                                &mode,
                                &target_name,
                                &version,
                                kind.id(),
                                &selected,
                            ));
                        }
                        Err(error) => {
                            state.failed = true;
                            state.status = SharedString::from(format!("获取失败：{error}"));
                        }
                    }
                    ui.set_install(state);
                });
            });
        }
    });

    ui.on_select_loader_version({
        let weak = ui.as_weak();
        move |version| {
            if let Some(ui) = weak.upgrade() {
                let mut state = ui.get_install();
                state.selected = version;
                let game_version = state.version.to_string();
                let loader = state.loader.to_string();
                let selected = state.selected.to_string();
                let mode = state.mode.to_string();
                let target_name = state.target_name.to_string();
                state.preview = SharedString::from(install_preview(
                    &mode,
                    &target_name,
                    &game_version,
                    &loader,
                    &selected,
                ));
                ui.set_install(state);
            }
        }
    });

    ui.on_confirm_install({
        let weak = ui.as_weak();
        let services: Arc<Services> = Arc::clone(services);
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let state = ui.get_install();
            let version = state.version.to_string();
            let loader = state.loader.to_string();
            let selected = state.selected.to_string();
            let kind = LoaderKind::from_id(&loader);

            // 组件安装：给已有实例装/换加载器，不新建实例
            if state.mode == "component" {
                let target = state.target.to_string();
                let Some(instance) = instance_by_id(&target) else {
                    show_toast(&weak, &toast_timer, "实例不存在或已被移除".to_string());
                    return;
                };
                let Some(loader_kind) = kind else {
                    show_toast(&weak, &toast_timer, "请先选择加载器".to_string());
                    return;
                };
                if selected.is_empty() {
                    show_toast(&weak, &toast_timer, "请先选择加载器版本".to_string());
                    return;
                }
                let task = TaskKind::InstallLoader {
                    instance_id: target,
                    instance_name: instance.name.clone(),
                    kind: loader_kind.id().to_string(),
                    loader_version: selected.clone(),
                };
                let mut state = ui.get_install();
                state.open = false;
                ui.set_install(state);

                let task_id = services.tasks.register(task.clone());
                add_task_row(&ui, task_id, &task);
                show_toast(
                    &weak,
                    &toast_timer,
                    format!(
                        "开始为「{}」安装 {} {selected}（进度见下载中心）",
                        instance.name,
                        loader_kind.label()
                    ),
                );
                spawn_task_run(&handle, &services, &weak, task_id);
                return;
            }

            let name = state.name_draft.trim().to_string();
            let icon = state.icon_kind.to_string();
            let root = state.root.trim().to_string();
            let loader_spec = match kind {
                Some(loader_kind) => {
                    if selected.is_empty() {
                        show_toast(
                            &weak,
                            &toast_timer,
                            format!("请先选择 {} 版本", loader_kind.label()),
                        );
                        return;
                    }
                    Some((loader_kind.id().to_string(), selected.clone()))
                }
                None => None,
            };
            let label = match &loader_spec {
                Some((_, loader_version)) => format!(
                    "{version} + {} {loader_version}",
                    kind.map(|kind| kind.label()).unwrap_or("加载器")
                ),
                None => version.clone(),
            };
            let task = TaskKind::InstallVersion {
                version: version.clone(),
                name: (!name.is_empty()).then(|| name.clone()),
                icon: (!icon.is_empty()).then(|| icon.clone()),
                root: (!root.is_empty()).then(|| std::path::PathBuf::from(&root)),
                loader: loader_spec,
            };

            let mut state = ui.get_install();
            state.open = false;
            ui.set_install(state);

            let task_id = services.tasks.register(task.clone());
            add_task_row(&ui, task_id, &task);
            show_toast(
                &weak,
                &toast_timer,
                format!("开始安装 {label}（进度见下载中心）"),
            );
            spawn_task_run(&handle, &services, &weak, task_id);
        }
    });

    // 导入本地 .mrpack
    ui.on_import_pack({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move |path| {
            let path = path.trim().to_string();
            if path.is_empty() {
                show_toast(&weak, &toast_timer, "请先粘贴 .mrpack 文件路径".to_string());
                return;
            }

            let kind = TaskKind::ImportPack(path.clone());
            let task_id = services.tasks.register(kind.clone());
            if let Some(ui) = weak.upgrade() {
                add_task_row(&ui, task_id, &kind);
                update_versions_page(&ui, |model| model.import_path = SharedString::default());
            }
            show_toast(&weak, &toast_timer, format!("开始导入整合包：{path}"));
            spawn_task_run(&handle, &services, &weak, task_id);
        }
    });

    // 移除实例：先弹确认框（游戏文件保留；破坏性操作不做无确认执行）
    let pending_remove: Rc<RefCell<Option<PendingAction>>> = Rc::new(RefCell::new(None));

    ui.on_remove_instance({
        let weak = ui.as_weak();
        let pending = pending_remove.clone();

        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let name = instance_by_id(&id)
                .map(|instance| instance.name)
                .unwrap_or_else(|| id.to_string());
            *pending.borrow_mut() = Some(PendingAction::Remove(id.to_string()));

            let mut confirm = ui.get_confirm();
            confirm.open = true;
            confirm.title = SharedString::from("移除实例？");
            confirm.message = SharedString::from(format!("「{name}」的实例记录将被移除。"));
            confirm.danger = true;
            confirm.checkbox_text = SharedString::from("同时删除游戏文件（不可恢复）");
            confirm.checked = false;
            ui.set_confirm(confirm);
        }
    });

    ui.on_remove_selected({
        let weak = ui.as_weak();
        let pending = pending_remove.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let ids = selected_ids(&ui);
            if ids.is_empty() {
                return;
            }
            *pending.borrow_mut() = Some(PendingAction::RemoveMany(ids.clone()));
            let mut confirm = ui.get_confirm();
            confirm.open = true;
            confirm.title = SharedString::from(format!("移除 {} 个实例？", ids.len()));
            confirm.message = SharedString::from("这些实例的记录将被移除。");
            confirm.danger = true;
            confirm.checkbox_text = SharedString::from("同时删除游戏文件（不可恢复）");
            confirm.checked = false;
            ui.set_confirm(confirm);
        }
    });

    ui.on_confirm_cancel({
        let weak = ui.as_weak();
        let pending = pending_remove.clone();

        move || {
            pending.borrow_mut().take();
            if let Some(ui) = weak.upgrade() {
                let mut confirm = ui.get_confirm();
                confirm.open = false;
                ui.set_confirm(confirm);
            }
        }
    });

    ui.on_confirm_accept({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let pending = pending_remove.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut confirm = ui.get_confirm();
            let delete_files = confirm.checked;
            confirm.open = false;
            confirm.checked = false;
            ui.set_confirm(confirm);

            let Some(action) = pending.borrow_mut().take() else {
                return;
            };
            match action {
                PendingAction::Remove(id) => match FileInstanceStore::open_default() {
                    Ok(mut store) => {
                        let dir_path = store.get(&id).map(|instance| instance.dir);
                        match store.remove(&id) {
                            Ok(()) => {
                                let mut suffix = String::new();
                                match dir_path {
                                    Some(path) if delete_files => {
                                        // 只允许删除实例根目录内的路径，避免异常数据删到别处
                                        if path.starts_with(platform::paths::instances_dir()) {
                                            match std::fs::remove_dir_all(&path) {
                                                Ok(()) => suffix = "，游戏文件已删除".to_string(),
                                                Err(error) => {
                                                    suffix = format!("，但删除文件失败：{error}")
                                                }
                                            }
                                        } else {
                                            suffix = format!(
                                                "，拒绝删除实例目录之外的路径：{}",
                                                path.display()
                                            );
                                        }
                                    }
                                    Some(path) => {
                                        suffix = format!("，游戏文件保留在 {}", path.display())
                                    }
                                    None => {}
                                }
                                refresh_instances(&ui);
                                // 从实例管理页移除时退回实例库，避免停留在已失效的详情
                                if ui.get_current_page() == 6 {
                                    ui.set_current_page(1);
                                }
                                show_toast(
                                    &weak,
                                    &toast_timer,
                                    format!("实例已从启动器移除{suffix}"),
                                );
                            }
                            Err(error) => {
                                show_toast(&weak, &toast_timer, format!("移除失败：{error}"));
                            }
                        }
                    }
                    Err(error) => {
                        show_toast(&weak, &toast_timer, format!("实例存储打开失败：{error}"));
                    }
                },
                PendingAction::RemoveMany(ids) => match FileInstanceStore::open_default() {
                    Ok(mut store) => {
                        let mut removed = 0usize;
                        let mut failed = Vec::new();
                        for id in ids {
                            let dir_path = store.get(&id).map(|instance| instance.dir);
                            match store.remove(&id) {
                                Ok(()) => {
                                    removed += 1;
                                    // 与单条移除同一条保护：只删实例根目录内的路径
                                    if delete_files {
                                        if let Some(path) = dir_path {
                                            if path.starts_with(platform::paths::instances_dir()) {
                                                let _ = std::fs::remove_dir_all(&path);
                                            }
                                        }
                                    }
                                }
                                Err(error) => failed.push(format!("{id}：{error}")),
                            }
                        }
                        // 清空多选态并刷新列表
                        let mut page = ui.get_instances_page();
                        page.selecting = false;
                        page.selected_count = 0;
                        page.selected_ids =
                            ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
                        ui.set_instances_page(page);
                        refresh_instances(&ui);
                        if ui.get_current_page() == 6 {
                            ui.set_current_page(1);
                        }
                        let message = if failed.is_empty() {
                            format!("已移除 {removed} 个实例")
                        } else {
                            format!("已移除 {removed} 个，{} 个失败", failed.len())
                        };
                        show_toast(&weak, &toast_timer, message);
                    }
                    Err(error) => {
                        show_toast(&weak, &toast_timer, format!("实例存储打开失败：{error}"));
                    }
                },
                PendingAction::ApplyDefaults(memory_mb) => {
                    let mut applied = 0usize;
                    if let Ok(store) = FileInstanceStore::open_default() {
                        for instance in store.snapshot().instances() {
                            let mut settings =
                                mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
                            settings.memory_mb = memory_mb;
                            if settings.save(&instance.dir).is_ok() {
                                applied += 1;
                            }
                        }
                    }
                    update_settings(&ui, |model| {
                        model.defaults_status = SharedString::from(format!(
                            "已应用到 {applied} 个实例（{memory_mb} MB）"
                        ));
                    });
                    ui.invoke_detail_refresh();
                    show_toast(
                        &weak,
                        &toast_timer,
                        format!("已把默认内存 {memory_mb} MB 应用到 {applied} 个实例"),
                    );
                }
            }
        }
    });

    // 崩溃弹窗：关闭 / 打开日志目录 / 导出报告
    ui.on_close_crash({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let mut crash = ui.get_crash();
                crash.open = false;
                ui.set_crash(crash);
            }
        }
    });

    ui.on_open_crash_log({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let crash = ui.get_crash();
            let Some(instance) = instance_by_id(crash.instance_id.as_str()) else {
                show_toast(&weak, &toast_timer, "实例已被移除".to_string());
                return;
            };
            let logs = GameLayout::new(instance.dir.clone()).root().join("logs");
            if let Err(error) = platform::browser::open_path(&logs) {
                show_toast(&weak, &toast_timer, error.to_string());
            }
        }
    });

    ui.on_export_crash_report({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let crash = ui.get_crash();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let dest = platform::paths::data_dir().join("exports");
            let _ = std::fs::create_dir_all(&dest);
            let path = dest.join(format!("crash-{stamp}.txt"));

            let mut body = format!("{}\n\n{}\n\n", crash.title, crash.detail);
            if let Some(instance) = instance_by_id(crash.instance_id.as_str()) {
                let log = GameLayout::new(instance.dir.clone())
                    .root()
                    .join("logs")
                    .join(format!("{}.log", instance.version_id()));
                if let Ok(bytes) = std::fs::read(&log) {
                    let start = bytes.len().saturating_sub(64 * 1024);
                    body.push_str("===== 日志末尾 64KB =====\n");
                    body.push_str(&String::from_utf8_lossy(&bytes[start..]));
                }
            }

            match std::fs::write(&path, body) {
                Ok(()) => {
                    let _ = platform::browser::open_path(&dest);
                    show_toast(
                        &weak,
                        &toast_timer,
                        format!("崩溃报告已导出：{}", path.display()),
                    );
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("导出失败：{error}")),
            }
        }
    });

    // 实例改名：内联编辑，保存时校验并落库
    ui.on_detail_start_rename({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut detail = ui.get_detail();
            detail.renaming = true;
            detail.name_draft = detail.name.clone();
            ui.set_detail(detail);
        }
    });

    ui.on_detail_cancel_rename({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let mut detail = ui.get_detail();
                detail.renaming = false;
                ui.set_detail(detail);
            }
        }
    });

    ui.on_detail_save_rename({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            let id = detail.id.to_string();
            let draft = detail.name_draft.to_string();
            match FileInstanceStore::open_default() {
                Ok(mut store) => match store.rename(&id, &draft) {
                    Ok(()) => {
                        let trimmed = draft.trim();
                        let mut detail = ui.get_detail();
                        detail.name = SharedString::from(trimmed);
                        detail.name_draft = SharedString::from(trimmed);
                        detail.renaming = false;
                        ui.set_detail(detail);
                        refresh_instances(&ui);
                        show_toast(&weak, &toast_timer, "实例已改名".to_string());
                    }
                    Err(error) => show_toast(&weak, &toast_timer, format!("改名失败：{error}")),
                },
                Err(error) => show_toast(&weak, &toast_timer, format!("实例存储打开失败：{error}")),
            }
        }
    });

    // 游戏设置：把详情页当前值落盘到 `<实例目录>/settings.json`
    ui.on_game_settings_save({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match save_instance_settings(&ui) {
                Ok(()) => show_toast(
                    &weak,
                    &toast_timer,
                    "游戏设置已保存，下次启动生效".to_string(),
                ),
                Err(error) => show_toast(&weak, &toast_timer, format!("保存失败：{error}")),
            }
        }
    });

    ui.on_game_settings_reset({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // 「恢复默认」回到全局默认值（而不是代码内置常量）
            let defaults = mcl_core::instance_settings::InstanceSettings::from_defaults(
                &LauncherSettings::load().instance_defaults,
            );
            let mut detail = ui.get_detail();
            detail.memory_mb = defaults.memory_mb as i32;
            detail.game_width = defaults.width as i32;
            detail.game_height = defaults.height as i32;
            detail.fullscreen = defaults.fullscreen;
            detail.jvm_args = SharedString::from(defaults.jvm_args);
            detail.game_args = SharedString::from(defaults.game_args);
            detail.java_path = SharedString::from(defaults.java_path);
            ui.set_detail(detail);

            match save_instance_settings(&ui) {
                Ok(()) => show_toast(&weak, &toast_timer, "已恢复默认设置".to_string()),
                Err(error) => show_toast(&weak, &toast_timer, format!("保存失败：{error}")),
            }
        }
    });

    ui.on_set_instance_icon({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |path| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            let Some(instance) = instance_by_id(detail.id.as_str()) else {
                show_toast(&weak, &toast_timer, "实例不存在".to_string());
                return;
            };
            let path = path.trim().to_string();
            if path.is_empty() {
                show_toast(&weak, &toast_timer, "请先填写 PNG 图片路径".to_string());
                return;
            }
            match set_instance_icon(&instance.dir, Path::new(&path)) {
                Ok(()) => {
                    // 自定义图片优先，清掉内置图标选择避免回退时互相覆盖
                    let mut settings =
                        mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
                    if !settings.icon.is_empty() {
                        settings.icon.clear();
                        let _ = settings.save(&instance.dir);
                    }
                    let mut detail = ui.get_detail();
                    detail.settings_status = SharedString::from("图标已更新");
                    detail.icon_kind = SharedString::default();
                    ui.set_detail(detail);
                    refresh_instances(&ui);
                    show_toast(&weak, &toast_timer, "实例图标已更新".to_string());
                }
                Err(error) => show_toast(&weak, &toast_timer, error),
            }
        }
    });

    ui.on_reset_instance_icon({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            let Some(instance) = instance_by_id(detail.id.as_str()) else {
                show_toast(&weak, &toast_timer, "实例不存在".to_string());
                return;
            };
            let cleared_file = clear_instance_icon(&instance.dir);
            let cleared_kind = mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
            let mut cleared_kind = cleared_kind;
            cleared_kind.icon.clear();
            let _ = cleared_kind.save(&instance.dir);
            match cleared_file {
                Ok(()) => {
                    let mut detail = ui.get_detail();
                    detail.settings_status = SharedString::from("已跟随加载器图标");
                    detail.icon_kind = SharedString::default();
                    ui.set_detail(detail);
                    refresh_instances(&ui);
                    show_toast(&weak, &toast_timer, "已恢复默认图标".to_string());
                }
                Err(error) => show_toast(&weak, &toast_timer, error),
            }
        }
    });

    // 选择内置图标（HMCL 同款 14 个素材）；空 id = 跟随加载器
    ui.on_pick_instance_icon({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let services = Arc::clone(services);

        move |kind| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            let Some(instance) = instance_by_id(detail.id.as_str()) else {
                show_toast(&weak, &toast_timer, "实例不存在".to_string());
                return;
            };
            let kind = kind.to_string();
            if !kind.is_empty() && mcl_app_model::IconKind::from_id(&kind).is_none() {
                show_toast(&weak, &toast_timer, "未知的内置图标".to_string());
                return;
            }
            let mut settings = mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
            settings.icon = kind.clone();
            if let Err(error) = settings.save(&instance.dir) {
                show_toast(&weak, &toast_timer, format!("保存失败：{error}"));
                return;
            }
            // 内置图标优先于 icon.png；清掉旧的自定义图片，避免切回时残留
            let _ = clear_instance_icon(&instance.dir);

            let id = instance.id.clone();
            show_instance_detail(&ui, &id, &services);
            let mut detail = ui.get_detail();
            detail.settings_status = SharedString::from("图标已更新");
            ui.set_detail(detail);
            refresh_instances(&ui);
            show_toast(
                &weak,
                &toast_timer,
                if kind.is_empty() {
                    "已跟随加载器图标".to_string()
                } else {
                    "实例图标已更新".to_string()
                },
            );
        }
    });

    // Java 管理：扫描本机运行时，选中后写入实例 Java 路径（保存生效）
    ui.on_scan_java({
        let weak = ui.as_weak();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let rows: Vec<JavaRow> = mcl_core::java::detect_javas()
                .into_iter()
                .map(|runtime| JavaRow {
                    path: SharedString::from(runtime.path.display().to_string()),
                    label: SharedString::from(format!(
                        "Java {} · {}",
                        runtime.major,
                        runtime.path.display()
                    )),
                })
                .collect();

            let mut detail = ui.get_detail();
            detail.settings_status = SharedString::from(format!("检测到 {} 个 Java", rows.len()));
            detail.java_options = ModelRc::new(VecModel::from(rows));
            ui.set_detail(detail);
        }
    });

    ui.on_use_java({
        let weak = ui.as_weak();

        move |path| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut detail = ui.get_detail();
            detail.java_path = path;
            detail.settings_status = SharedString::from("已选择 Java，点击保存生效");
            ui.set_detail(detail);
        }
    });

    ui.on_export_modpack({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            let Some(instance) = instance_by_id(detail.id.as_str()) else {
                show_toast(&weak, &toast_timer, "实例不存在".to_string());
                return;
            };
            let dest = platform::paths::data_dir().join("exports");
            match mcl_core::pack::mrpack::export_local_mrpack(
                &instance.name,
                &instance.mc_version,
                instance.loader(),
                &instance.dir,
                &dest,
            ) {
                Ok(path) => {
                    let _ = platform::browser::open_path(&dest);
                    let mut detail = ui.get_detail();
                    detail.settings_status =
                        SharedString::from(format!("已导出：{}", path.display()));
                    ui.set_detail(detail);
                    show_toast(
                        &weak,
                        &toast_timer,
                        format!("整合包已导出：{}", path.display()),
                    );
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("导出失败：{error}")),
            }
        }
    });

    // 模组更新检查：按 SHA-1 问 Modrinth，结果以列表呈现（下载交给用户确认）
    ui.on_check_mod_updates({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let id = ui.get_detail().id.to_string();
            let services = Arc::clone(&services);
            {
                let mut detail = ui.get_detail();
                detail.mods_update_hint = SharedString::from("正在检查…");
                ui.set_detail(detail);
            }

            let weak = weak.clone();
            let task_handle = handle.clone();
            task_handle.spawn(async move {
                let result = services.check_mod_updates(&id).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    let mut detail = ui.get_detail();
                    match result {
                        Ok(updates) => {
                            let rows: Vec<ModUpdateRow> = updates
                                .into_iter()
                                .map(|item| ModUpdateRow {
                                    file: SharedString::from(item.file),
                                    latest: SharedString::from(item.latest),
                                    url: SharedString::from(item.url),
                                })
                                .collect();
                            detail.mods_update_hint = if rows.is_empty() {
                                SharedString::from("没有发现可更新的模组")
                            } else {
                                SharedString::from(format!(
                                    "{} 个模组可更新（点击「打开」查看下载页）",
                                    rows.len()
                                ))
                            };
                            detail.mod_updates = ModelRc::new(VecModel::from(rows));
                        }
                        Err(error) => {
                            detail.mods_update_hint =
                                SharedString::from(format!("检查失败：{error}"));
                        }
                    }
                    ui.set_detail(detail);
                });
            });
        }
    });

    ui.on_open_mod_update({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |url| {
            if url.is_empty() {
                return;
            }
            if let Err(error) = platform::browser::open_in_browser(&url) {
                show_toast(&weak, &toast_timer, format!("打开下载页失败：{error}"));
            }
        }
    });

    // 资源详情的外链（站点页 / MC 百科 / 正文链接）
    ui.on_open_external({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |url| {
            if url.is_empty() {
                return;
            }
            if let Err(error) = platform::browser::open_in_browser(&url) {
                show_toast(&weak, &toast_timer, format!("打开链接失败：{error}"));
            }
        }
    });

    // 首页空态 → 版本列表
    ui.on_start_install({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_current_page(2);
            }
        }
    });

    // 扫描目录：遍历全部游戏目录根，收养未登记但结构完整的实例
    ui.on_scan_instances({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match FileInstanceStore::open_default() {
                Ok(mut store) => {
                    let known = store.snapshot().known_ids();
                    let roots: Vec<std::path::PathBuf> = LauncherSettings::load()
                        .game_directories
                        .iter()
                        .map(std::path::PathBuf::from)
                        .collect();
                    let mut added = 0usize;
                    for instance in scan_all_roots(&roots, &known) {
                        if store.create(instance).is_ok() {
                            added += 1;
                        }
                    }
                    let total = store.list().len();
                    refresh_instances(&ui);
                    let message = if added > 0 {
                        format!("已扫描：发现 {added} 个新实例，共 {total} 个")
                    } else {
                        format!("已扫描，共 {total} 个实例")
                    };
                    show_toast(&weak, &toast_timer, message);
                }
                Err(error) => {
                    show_toast(&weak, &toast_timer, format!("实例存储打开失败：{error}"));
                }
            }
        }
    });

    // 刷新按钮：只重读实例存储与目录列表，不触发文件系统扫描
    ui.on_refresh_instances({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            refresh_instances(&ui);
            show_toast(&weak, &toast_timer, "已刷新实例列表".to_string());
        }
    });

    // 实例库筛选：搜索词 / 加载器 / 清除（过滤在 Rust 侧完成）
    ui.on_set_instance_query({
        let weak = ui.as_weak();
        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.query = value;
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
        }
    });

    ui.on_set_instance_filter({
        let weak = ui.as_weak();
        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.filter = value;
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
        }
    });

    ui.on_clear_filters({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.query = SharedString::default();
            page.filter = SharedString::from("all");
            page.view = SharedString::from("all");
            page.directory = SharedString::default();
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
            refresh_directories(&ui);
        }
    });

    // 视图筛选：全部 / 最近游玩
    ui.on_set_instance_view({
        let weak = ui.as_weak();
        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.view = value;
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
        }
    });

    // 目录筛选：选中某个游戏目录（空 = 全部目录）
    ui.on_set_instance_directory({
        let weak = ui.as_weak();
        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut page = ui.get_instances_page();
            page.directory = value;
            ui.set_instances_page(page);
            refresh_instance_view(&ui);
            refresh_directories(&ui);
        }
    });

    // ── 游戏目录管理页（03b）────────────────────────────────────────
    ui.on_open_game_directories({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_current_page(8);
            ui.set_show_directory_add(false);
            ui.set_directories_hint(SharedString::default());
            ui.set_directories_hint_tone(0);
            refresh_directories(&ui);
        }
    });

    ui.on_set_game_directory_default({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        move |path| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match set_default_game_directory(path.as_str()) {
                Ok(()) => {
                    refresh_directories(&ui);
                    show_toast(&weak, &toast_timer, "已设为默认游戏目录".to_string());
                }
                Err(error) => show_toast(&weak, &toast_timer, error),
            }
        }
    });

    ui.on_remove_game_directory({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        move |path| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match remove_game_directory(path.as_str()) {
                Ok(()) => {
                    refresh_directories(&ui);
                    refresh_instances(&ui);
                    show_toast(
                        &weak,
                        &toast_timer,
                        "已移除游戏目录（文件保留）".to_string(),
                    );
                }
                Err(error) => show_toast(&weak, &toast_timer, error),
            }
        }
    });

    ui.on_open_game_directory({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        move |path| {
            let root = std::path::PathBuf::from(path.to_string());
            if let Err(error) = std::fs::create_dir_all(&root) {
                show_toast(&weak, &toast_timer, format!("目录创建失败：{error}"));
                return;
            }
            if let Err(error) = platform::browser::open_path(&root) {
                show_toast(&weak, &toast_timer, error.to_string());
            }
        }
    });

    ui.on_begin_add_game_directory({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_new_directory_path(SharedString::default());
            ui.set_directories_hint(SharedString::default());
            ui.set_directories_hint_tone(0);
            ui.set_show_directory_add(true);
        }
    });

    ui.on_confirm_add_game_directory({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let path = ui.get_new_directory_path().to_string();
            match add_game_directory(&path) {
                Ok(()) => {
                    ui.set_show_directory_add(false);
                    ui.set_new_directory_path(SharedString::default());
                    ui.set_directories_hint(SharedString::from(
                        "已添加游戏目录，新实例将落在默认目录",
                    ));
                    ui.set_directories_hint_tone(1);
                    refresh_directories(&ui);
                    refresh_instances(&ui);
                }
                Err(error) => {
                    ui.set_directories_hint(SharedString::from(error));
                    ui.set_directories_hint_tone(0);
                }
            }
        }
    });

    ui.on_cancel_add_game_directory({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_show_directory_add(false);
            ui.set_directories_hint(SharedString::default());
            ui.set_directories_hint_tone(0);
        }
    });

    // 任务操作：运行中 → 暂停；暂停/失败 → 继续/重试
    ui.on_toggle_task({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move |id| {
            let task_id = id as u64;
            match services.tasks.state(task_id) {
                Some(TaskState::Running) => {
                    if services.tasks.request_cancel(task_id) {
                        show_toast(
                            &weak,
                            &toast_timer,
                            "正在暂停…（当前文件下载完后停止，已完成部分保留）".to_string(),
                        );
                    }
                }
                Some(TaskState::Paused | TaskState::Failed) => {
                    // 续传/重试：重新执行同一任务（哈希命中跳过 + .part 续传使重复执行代价很小）
                    spawn_task_run(&handle, &services, &weak, task_id);
                }
                _ => {}
            }
        }
    });

    // 全部暂停：对运行中的任务逐个置取消旗标（当前文件完成后停）
    ui.on_pause_all_tasks({
        let weak = ui.as_weak();
        let services = services.clone();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let tasks = ui.get_tasks();
            let Some(vec) = tasks.as_any().downcast_ref::<VecModel<TaskRow>>() else {
                return;
            };
            let mut paused = 0;
            for index in 0..vec.row_count() {
                if let Some(row) = vec.row_data(index) {
                    if row.state.as_str() == "下载中"
                        && services.tasks.request_cancel(row.id as u64)
                    {
                        paused += 1;
                    }
                }
            }
            let message = if paused == 0 {
                "没有正在下载的任务".to_string()
            } else {
                format!("已请求暂停 {paused} 个任务")
            };
            show_toast(&weak, &toast_timer, message);
        }
    });

    // 清空已完成/失败的任务行（注册表与 UI 同步清理）
    ui.on_clear_finished_tasks({
        let weak = ui.as_weak();
        let services = services.clone();

        move || {
            let cleared = services.tasks.clear_finished();
            if cleared.is_empty() {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let tasks = ui.get_tasks();
            if let Some(vec) = tasks.as_any().downcast_ref::<VecModel<TaskRow>>() {
                // 从后往前删除，避免索引位移
                for index in (0..vec.row_count()).rev() {
                    if let Some(row) = vec.row_data(index) {
                        if cleared.contains(&(row.id as u64)) {
                            vec.remove(index);
                        }
                    }
                }
            }
            refresh_download_summary(&ui);
        }
    });

    wire_settings_callbacks(ui, &toast_timer, &pending_remove);
    wire_instance_detail_callbacks(runtime, ui, services, &toast_timer);
    wire_account_callbacks(runtime, ui, &toast_timer);
}

/// 设置页回调：下载并发数保存
///
/// 并发数在**每次下载任务开始时**从设置读取（见 `Services::install`），
/// 因此修改后无需重启，下一个任务即生效。
fn wire_settings_callbacks(
    ui: &AppWindow,
    toast_timer: &std::rc::Rc<Timer>,
    pending_remove: &Rc<RefCell<Option<PendingAction>>>,
) {
    ui.on_save_download_concurrency({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut settings = LauncherSettings::load();
            // UI 输入已由 SpinBox 约束在 1~256，这里防御性再收敛一次
            let clamped = settings.set_download_concurrency(value.max(1) as usize);
            set_download_concurrency(&ui, clamped as i32);

            match settings.save() {
                Ok(()) => show_toast(
                    &weak,
                    &toast_timer,
                    format!("下载并发数已保存：{clamped}（下一个下载任务生效）"),
                ),
                Err(error) => show_toast(&weak, &toast_timer, format!("设置保存失败：{error}")),
            }
        }
    });

    // 新建实例默认内存（全局默认；未落 settings.json 的实例生效）
    ui.on_save_default_memory({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut settings = LauncherSettings::load();
            let clamped = settings
                .instance_defaults
                .set_memory_mb(value.max(1024) as u32);
            match settings.save() {
                Ok(()) => {
                    update_settings(&ui, |model| {
                        model.default_memory = clamped as i32;
                        model.defaults_status =
                            SharedString::from(format!("已保存：{clamped} MB（新实例生效）"));
                    });
                    ui.set_memory_hint(SharedString::from(memory_hint(clamped)));
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("设置保存失败：{error}")),
            }
        }
    });

    // 把默认内存批量写入所有实例（覆盖各实例的 memory 字段，需二次确认）
    ui.on_apply_defaults_all({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let pending = pending_remove.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let memory_mb = ui.get_settings().default_memory.max(1024) as u32;
            let count = FileInstanceStore::open_default()
                .map(|store| store.list().len())
                .unwrap_or(0);
            if count == 0 {
                show_toast(&weak, &toast_timer, "还没有实例".to_string());
                return;
            }

            *pending.borrow_mut() = Some(PendingAction::ApplyDefaults(memory_mb));
            let mut confirm = ui.get_confirm();
            confirm.open = true;
            confirm.title = SharedString::from("应用到全部实例？");
            confirm.message = SharedString::from(format!(
                "将把 {count} 个实例的内存上限改为 {memory_mb} MB，其他设置保持不变。"
            ));
            confirm.danger = false;
            confirm.checkbox_text = SharedString::default();
            confirm.checked = false;
            ui.set_confirm(confirm);
        }
    });

    // 代理：客户端在启动时构造，保存后需重启生效
    ui.on_save_proxy({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let url = value.trim().to_string();
            let mut settings = LauncherSettings::load();
            settings.proxy_url = url.clone();
            match settings.save() {
                Ok(()) => {
                    update_settings(&ui, |model| model.proxy_url = SharedString::from(url));
                    show_toast(
                        &weak,
                        &toast_timer,
                        "代理已保存，重启启动器后生效".to_string(),
                    );
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("设置保存失败：{error}")),
            }
        }
    });

    // 下载源偏好：官方优先 / 镜像优先（重启生效，客户端在启动时构造）
    ui.on_save_download_source({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move |value| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let value = value.to_string();
            let mut settings = LauncherSettings::load();
            settings.download_source = value.clone();
            settings.normalize_download_source();
            let normalized = settings.download_source.clone();
            match settings.save() {
                Ok(()) => {
                    update_settings(&ui, |model| {
                        model.download_source = SharedString::from(normalized.clone());
                    });
                    let label = match normalized.as_str() {
                        "official" => "官方源优先",
                        "mirror" => "镜像优先（BMCLAPI）",
                        _ => "自动（官方优先，失败切镜像）",
                    };
                    show_toast(
                        &weak,
                        &toast_timer,
                        format!("下载源已设为{label}，重启启动器后生效"),
                    );
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("设置保存失败：{error}")),
            }
        }
    });

    ui.on_open_data_dir({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let dir = platform::paths::data_dir();
            let _ = std::fs::create_dir_all(&dir);
            if let Err(error) = platform::browser::open_path(&dir) {
                show_toast(&weak, &toast_timer, format!("打开数据目录失败：{error}"));
            }
        }
    });

    ui.on_open_log_dir({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let dir = platform::paths::data_dir().join("logs");
            let _ = std::fs::create_dir_all(&dir);
            if let Err(error) = platform::browser::open_path(&dir) {
                show_toast(&weak, &toast_timer, format!("打开日志目录失败：{error}"));
            }
        }
    });

    // 诊断包：日志 + 系统自述（不含账号令牌与设置），导出后打开所在目录
    ui.on_export_diagnostics({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let data = platform::paths::data_dir();
            let logs = data.join("logs");
            let dest = data.join("exports");
            let info = format!(
                "OMCLauncher {}\nOS: {} {}\n数据目录: {}\n日志目录: {}\n",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH,
                data.display(),
                logs.display()
            );
            match export_diagnostics(&logs, &dest, &info) {
                Ok(path) => {
                    if let Some(ui) = weak.upgrade() {
                        update_settings(&ui, |model| {
                            model.diag_status =
                                SharedString::from(format!("已导出：{}", path.display()));
                        });
                    }
                    let _ = platform::browser::open_path(&dest);
                    show_toast(
                        &weak,
                        &toast_timer,
                        format!("诊断包已导出：{}", path.display()),
                    );
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("导出失败：{error}")),
            }
        }
    });

    // 共享目录回收：**两步确认**——先算计划并展示，
    // 再点一次才真正删除（防误触）；无实例登记时由 core 直接拒绝
    {
        let armed = std::rc::Rc::new(std::cell::Cell::new(false));
        ui.on_run_shared_gc({
            let weak = ui.as_weak();
            let armed = armed.clone();

            move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let shared_root = platform::paths::shared_dir();
                let instances = FileInstanceStore::open_default()
                    .map(|store| store.list())
                    .unwrap_or_default();

                let plan = match mcl_core::maintenance::plan_shared_gc(&shared_root, &instances) {
                    Ok(plan) => plan,
                    Err(error) => {
                        armed.set(false);
                        set_gc_armed(&ui, false);
                        set_gc_status(&ui, SharedString::from(error.to_string()));
                        return;
                    }
                };

                if !armed.get() {
                    if plan.is_empty() {
                        set_gc_status(&ui, SharedString::from("共享目录很干净：没有可清理的文件"));
                        return;
                    }
                    armed.set(true);
                    set_gc_armed(&ui, true);
                    let unknown_note = if plan.unknown_indexes.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "（{} 个资源索引读不到，对应资源已保守保留）",
                            plan.unknown_indexes.len()
                        )
                    };
                    set_gc_status(
                        &ui,
                        SharedString::from(format!(
                            "将清理 {} 个文件（{:.1} MB）{unknown_note}——再点一次确认执行",
                            plan.candidates.len(),
                            plan.total_mb()
                        )),
                    );
                    return;
                }

                armed.set(false);
                set_gc_armed(&ui, false);
                let report = mcl_core::maintenance::apply_shared_gc(&plan);
                let message = if report.failures.is_empty() {
                    format!(
                        "共享目录已清理：{} 个文件，释放 {:.1} MB",
                        report.removed,
                        report.removed_mb()
                    )
                } else {
                    format!(
                        "清理完成：删除 {} 个，{} 个失败（详见日志）",
                        report.removed,
                        report.failures.len()
                    )
                };
                set_gc_status(&ui, SharedString::from(message));
            }
        });
    }

    // 微软登录 client_id 不作为用户设置暴露：它是开发者的应用身份
    //（构建期注入 / 运行时环境变量，见 mcl-core::auth::microsoft），
    // 让终端用户去注册 Azure 应用是产品设计错误
}

fn wire_instance_detail_callbacks(
    runtime: &tokio::runtime::Runtime,
    ui: &AppWindow,
    services: &Arc<Services>,
    toast_timer: &std::rc::Rc<Timer>,
) {
    // 打开详情（实例库「详情」按钮）
    ui.on_open_instance({
        let weak = ui.as_weak();
        let services = Arc::clone(services);

        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            show_instance_detail(&ui, &id, &services);
            ui.set_current_page(6);
        }
    });

    // 返回实例库（顺带刷新列表：详情期间可能启动过游戏，最近游玩顺序会变）
    // 资源详情返回：回到下载中心（与 app-window 的 page-parent(7) = 2 对齐）
    ui.on_resource_back({
        let weak = ui.as_weak();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut model = ui.get_versions_page();
            model.detail_open = false;
            ui.set_versions_page(model);
            ui.set_current_page(2);
        }
    });

    ui.on_detail_back({
        let weak = ui.as_weak();

        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_current_page(1);
                refresh_instances(&ui);
            }
        }
    });

    // 详情页启动：与实例库启动同一任务化路径（复用任务中心与事件桥）
    ui.on_detail_launch({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let instance_id = ui.get_detail().id.to_string();
            if instance_id.is_empty() {
                return;
            }
            let instance_name = ui.get_detail().name.to_string();

            let kind = TaskKind::Launch {
                instance_id,
                instance_name: instance_name.clone(),
            };
            let task_id = services.tasks.register(kind.clone());
            add_task_row(&ui, task_id, &kind);
            show_toast(&weak, &toast_timer, format!("正在启动 {instance_name}…"));
            spawn_task_run(&handle, &services, &weak, task_id);
        }
    });

    // 修复实例：任务化（校验/下载进度、暂停、重试全走任务中心）
    ui.on_detail_repair({
        let weak = ui.as_weak();
        let services = services.clone();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let instance_id = ui.get_detail().id.to_string();
            if instance_id.is_empty() {
                return;
            }
            let instance_name = ui.get_detail().name.to_string();

            let kind = TaskKind::Repair {
                instance_id,
                instance_name: instance_name.clone(),
            };
            let task_id = services.tasks.register(kind.clone());
            add_task_row(&ui, task_id, &kind);
            show_toast(
                &weak,
                &toast_timer,
                format!("开始修复 {instance_name}（进度见下载中心）"),
            );
            spawn_task_run(&handle, &services, &weak, task_id);
        }
    });

    // 打开实例文件夹（顺带创建 mods 目录，方便直接拖入模组）
    ui.on_detail_open_folder({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(instance) = instance_by_id(&ui.get_detail().id) else {
                return;
            };
            let _ = std::fs::create_dir_all(instance.dir.join("mods"));
            if let Err(error) = platform::browser::open_path(&instance.dir) {
                show_toast(&weak, &toast_timer, error.to_string());
            }
        }
    });

    // 刷新（重读模组与日志）
    ui.on_detail_refresh({
        let weak = ui.as_weak();
        let services = Arc::clone(services);

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            show_instance_detail(&ui, &ui.get_detail().id, &services);
        }
    });

    // 启用 / 禁用模组
    ui.on_toggle_mod({
        let weak = ui.as_weak();
        let services = Arc::clone(services);
        let toast_timer = toast_timer.clone();

        move |file_name| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(path) = mod_file_path(&ui.get_detail().id, &file_name) else {
                return;
            };
            let disabling = !path.to_string_lossy().ends_with(".disabled");

            let target = if disabling {
                path.with_file_name(format!("{file_name}.disabled"))
            } else {
                match path.file_name().and_then(|name| name.to_str()) {
                    Some(name) => match name.strip_suffix(".disabled") {
                        Some(stripped) => path.with_file_name(stripped),
                        None => return,
                    },
                    None => return,
                }
            };

            match std::fs::rename(&path, &target) {
                Ok(()) => {
                    let message = if disabling {
                        format!("已禁用 {file_name}")
                    } else {
                        format!("已启用 {file_name}")
                    };
                    show_toast(&weak, &toast_timer, message);
                    show_instance_detail(&ui, &ui.get_detail().id, &services);
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("操作失败：{error}")),
            }
        }
    });

    // 删除模组文件（模组可从来源重新下载，风险低于删整个实例）
    ui.on_filter_mods({
        let weak = ui.as_weak();
        move |query| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            let Some(instance) = instance_by_id(detail.id.as_str()) else {
                return;
            };
            let (rows, _) = mod_detail(&instance.dir);
            let mut detail = ui.get_detail();
            detail.mods = ModelRc::new(VecModel::from(filter_mod_rows(rows, query.as_str())));
            detail.mod_query = query;
            ui.set_detail(detail);
        }
    });

    ui.on_remove_mod({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let services = Arc::clone(services);

        move |file_name| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(path) = mod_file_path(&ui.get_detail().id, &file_name) else {
                return;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    show_toast(&weak, &toast_timer, format!("已删除 {file_name}"));
                    show_instance_detail(&ui, &ui.get_detail().id, &services);
                }
                Err(error) => show_toast(&weak, &toast_timer, format!("删除失败：{error}")),
            }
        }
    });

    // 仅重读日志（不动模组列表）
    ui.on_detail_reload_log({
        let weak = ui.as_weak();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(instance) = instance_by_id(&ui.get_detail().id) else {
                return;
            };
            let (lines, hint, diagnosis) = recent_log_rows(&instance.dir);
            let mut detail = ui.get_detail();
            detail.log_lines = ModelRc::new(VecModel::from(lines));
            detail.log_hint = SharedString::from(hint);
            detail.log_diagnosis = SharedString::from(diagnosis);
            ui.set_detail(detail);
        }
    });
}

/// 组装实例详情页数据（摘要 / 组件 / 模组 / 日志 / 设置）
fn show_instance_detail(ui: &AppWindow, id: &str, services: &Arc<Services>) {
    let Some(instance) = instance_by_id(id) else {
        return;
    };
    let (mods, mods_hint) = mod_detail(&instance.dir);
    let (log_lines, log_hint, log_diagnosis) = recent_log_rows(&instance.dir);

    let info = format!(
        "Minecraft {} · {} · {}",
        instance.mc_version,
        instance.loader().label(),
        if instance.shared {
            "共享资源"
        } else {
            "独立目录"
        }
    );

    // 图标：内置选择 > 自定义 icon.png > 加载器默认
    let settings_for_icon = mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
    let (icon, has_icon) = match instance_icon(
        &instance.dir,
        instance.loader(),
        Some(settings_for_icon.icon.as_str()),
    ) {
        Some(image) => (to_slint_image(image), true),
        None => (slint::Image::default(), false),
    };
    let icon_text = instance
        .name
        .chars()
        .next()
        .map(|ch| ch.to_string())
        .unwrap_or_default();

    // 一次性填充页面模型：详情页的状态收敛为一个结构（R6）
    // 实例没有自己的 settings.json 时才使用全局默认值
    let launcher_settings = LauncherSettings::load();
    let settings = mcl_core::instance_settings::InstanceSettings::load_with_defaults(
        &instance.dir,
        &launcher_settings.instance_defaults,
    );

    // 版本 JSON 里的 Java 主版本要求（与启动时的自动下载判断一致）
    let java_major =
        std::fs::read(GameLayout::new(instance.dir.clone()).version_json(&instance.version_id()))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .pointer("/javaVersion/majorVersion")
                    .and_then(serde_json::Value::as_u64)
            })
            .map(|major| major as u32);
    let java_requirement = java_major
        .map(|major| format!("该版本建议 Java {major}+"))
        .unwrap_or_else(|| {
            format!(
                "按版本要求 Java {}+",
                mcl_core::java::required_major_for_mc(&instance.mc_version)
            )
        });

    // 组件列表：原版始终存在；四种加载器按「已装 / 未装 / 不支持」展示
    let fabric_supported = services
        .fabric_support_cached(&instance.mc_version)
        .unwrap_or(true);
    let mut components = vec![ComponentRow {
        id: SharedString::from("vanilla"),
        name: SharedString::from("原版"),
        // 基础版本没有独立版本号，状态列显示「基础版本」
        version: SharedString::default(),
        state: SharedString::from("基础版本"),
        action: SharedString::default(),
        enabled: false,
        icon: builtin_image(IconKind::Grass),
        has_icon: true,
    }];
    for kind in LoaderKind::ALL {
        // 该实例当前装的就是这个加载器时，版本号来自实例本身
        let installed = match (instance.loader(), kind) {
            (LoaderSpec::Fabric(version), LoaderKind::Fabric) => Some(version.clone()),
            (LoaderSpec::Quilt(version), LoaderKind::Quilt) => Some(version.clone()),
            (LoaderSpec::Forge(version), LoaderKind::Forge) => Some(version.clone()),
            (LoaderSpec::NeoForge(version), LoaderKind::NeoForge) => Some(version.clone()),
            _ => None,
        };
        // Fabric 的可用性有官方清单兜底；其余加载器点了才拉列表
        let supported = kind != LoaderKind::Fabric || fabric_supported;
        components.push(ComponentRow {
            id: SharedString::from(kind.id()),
            name: SharedString::from(kind.label()),
            // 已安装只显示版本号（不再出现「已安装」字样）
            version: SharedString::from(installed.clone().unwrap_or_default()),
            state: SharedString::from(if supported {
                "未安装"
            } else {
                "该版本不支持"
            }),
            action: SharedString::from(if installed.is_some() {
                "更换"
            } else if supported {
                "安装"
            } else {
                ""
            }),
            enabled: supported,
            icon: builtin_image(IconKind::for_loader_id(kind.id())),
            has_icon: true,
        });
    }

    // 同一实例刷新时保留当前标签页；切换到别的实例才回到概览
    let current = ui.get_detail();
    let tab = if current.id == instance.id.as_str() {
        current.tab
    } else {
        0
    };
    let java_mode = if settings.java_path.trim().is_empty() {
        "auto".to_string()
    } else if current.id == instance.id.as_str() && current.java_mode == "detected" {
        "detected".to_string()
    } else {
        "custom".to_string()
    };
    let info_rows = vec![
        InfoRow {
            label: SharedString::from("游戏版本"),
            value: SharedString::from(instance.mc_version.clone()),
        },
        InfoRow {
            label: SharedString::from("加载器"),
            value: SharedString::from(instance.loader().label()),
        },
        InfoRow {
            label: SharedString::from("内存上限"),
            value: SharedString::from(format!("{} MB", settings.memory_mb)),
        },
        InfoRow {
            label: SharedString::from("Java 要求"),
            value: SharedString::from(java_requirement.clone()),
        },
        InfoRow {
            label: SharedString::from("Java"),
            value: SharedString::from(if settings.java_path.trim().is_empty() {
                "自动选择".to_string()
            } else {
                settings.java_path.clone()
            }),
        },
        InfoRow {
            label: SharedString::from("游戏目录"),
            value: SharedString::from(instance.dir.display().to_string()),
        },
        InfoRow {
            label: SharedString::from("模组"),
            value: SharedString::from(format!("{} 个", mods.len())),
        },
    ];

    ui.set_detail(DetailPageModel {
        id: SharedString::from(instance.id.clone()),
        name: SharedString::from(instance.name.clone()),
        info: SharedString::from(info),
        // 200px 左列放不下完整摘要，只留「版本 · 加载器名」
        rail_info: SharedString::from(format!(
            "{} · {}",
            instance.mc_version,
            instance.loader().name()
        )),
        mc_version: SharedString::from(instance.mc_version.clone()),
        icon,
        has_icon,
        icon_text: SharedString::from(icon_text),
        tab,
        mod_query: SharedString::default(),
        info_rows: ModelRc::new(VecModel::from(info_rows)),
        components: ModelRc::new(VecModel::from(components)),
        mods: ModelRc::new(VecModel::from(mods)),
        mods_hint: SharedString::from(mods_hint),
        log_lines: ModelRc::new(VecModel::from(log_lines)),
        log_hint: SharedString::from(log_hint),
        log_diagnosis: SharedString::from(log_diagnosis),
        renaming: false,
        name_draft: SharedString::from(instance.name.clone()),
        memory_mb: settings.memory_mb as i32,
        game_width: settings.width as i32,
        game_height: settings.height as i32,
        fullscreen: settings.fullscreen,
        jvm_args: SharedString::from(settings.jvm_args),
        game_args: SharedString::from(settings.game_args),
        java_path: SharedString::from(settings.java_path),
        java_mode: SharedString::from(java_mode),
        java_requirement: SharedString::from(java_requirement),
        icon_path_draft: SharedString::default(),
        icon_kind: SharedString::from(settings.icon.clone()),
        icon_choices_a: ModelRc::new(VecModel::from(icon_choices().0)),
        icon_choices_b: ModelRc::new(VecModel::from(icon_choices().1)),
        settings_status: SharedString::default(),
        java_options: ModelRc::new(VecModel::from(Vec::<JavaRow>::new())),
        mod_updates: ModelRc::new(VecModel::from(Vec::<ModUpdateRow>::new())),
        mods_update_hint: SharedString::default(),
    });
    refresh_mod_versions(ui);
}

/// 把详情页当前值写入 `<实例目录>/settings.json`。
fn save_instance_settings(ui: &AppWindow) -> Result<(), String> {
    let detail = ui.get_detail();
    let instance = instance_by_id(detail.id.as_str()).ok_or("实例不存在")?;
    // 基于已有设置修改：图标选择不在表单里，整体重建会把它清掉
    let mut settings = mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
    settings.memory_mb = detail.memory_mb.max(1) as u32;
    settings.width = detail.game_width.max(1) as u32;
    settings.height = detail.game_height.max(1) as u32;
    settings.fullscreen = detail.fullscreen;
    settings.jvm_args = detail.jvm_args.to_string();
    settings.game_args = detail.game_args.to_string();
    settings.java_path = detail.java_path.to_string();
    settings
        .save(&instance.dir)
        .map_err(|error| error.to_string())?;

    let mut detail = ui.get_detail();
    detail.settings_status = SharedString::from("已保存，下次启动生效");
    ui.set_detail(detail);
    Ok(())
}

/// Slint 行模型：模组（扫描逻辑在 `mcl-app-model::scan_mods`）
fn mod_detail(instance_dir: &Path) -> (Vec<ModRow>, String) {
    let (items, hint) = scan_mods(instance_dir);
    // 已装索引：project_id → { file }，反查得到每个 jar 的来源
    let installed: Vec<String> =
        std::fs::read_to_string(instance_dir.join(".mcl").join("installed.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value.as_object().map(|map| {
                    map.values()
                        .filter_map(|entry| entry.get("file")?.as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default();
    let rows: Vec<ModRow> = items
        .into_iter()
        .map(|item| {
            let version = cached_mod_version(instance_dir, &item.file_name).unwrap_or_default();
            let source = if installed.iter().any(|file| file == &item.file_name) {
                "Modrinth"
            } else {
                "手动"
            };
            ModRow {
                file_name: SharedString::from(item.file_name),
                source: SharedString::from(source),
                size: SharedString::from(format_size(item.size_bytes)),
                enabled: item.enabled,
                version: SharedString::from(version),
            }
        })
        .collect();
    (rows, hint)
}

/// 按文件名筛选模组行（大小写不敏感）
fn filter_mod_rows(rows: Vec<ModRow>, query: &str) -> Vec<ModRow> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|row| row.file_name.to_lowercase().contains(&needle))
        .collect()
}

/// 模组版本缓存：`<实例目录>/<文件名>` → 版本号。
/// 读版本需要解包 jar，缓存后刷新 / 切换标签不再重复扫描。
fn mod_version_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_mod_version(instance_dir: &Path, file_name: &str) -> Option<String> {
    let key = format!("{}/{}", instance_dir.display(), file_name);
    mod_version_cache().lock().ok()?.get(&key).cloned()
}

/// 后台补齐「版本」列：未命中缓存的模组在独立线程读 `fabric.mod.json` / `MANIFEST.MF`，
/// 逐行写回模型（`set_row_data`），不重建详情页、不阻塞 UI。
fn refresh_mod_versions(ui: &AppWindow) {
    let detail = ui.get_detail();
    let Some(instance) = instance_by_id(&detail.id) else {
        return;
    };
    let Some(rows) = detail.mods.as_any().downcast_ref::<VecModel<ModRow>>() else {
        return;
    };

    let mut missing: Vec<String> = Vec::new();
    for index in 0..rows.row_count() {
        if let Some(row) = rows.row_data(index) {
            let name = row.file_name.to_string();
            if row.version.is_empty() && !missing.contains(&name) {
                missing.push(name);
            }
        }
    }
    if missing.is_empty() {
        return;
    }

    let dir = instance.dir.clone();
    let instance_id = detail.id.to_string();
    let weak = ui.as_weak();

    // 纯文件 IO + 解包，用独立线程即可，不占用 tokio 的阻塞线程池
    std::thread::spawn(move || {
        let mut found: Vec<(String, String)> = Vec::new();
        for name in &missing {
            let mut path = dir.join("mods").join(name);
            if !path.is_file() {
                path = dir.join("mods").join(format!("{name}.disabled"));
            }
            if let Some(version) = mod_version(&path) {
                found.push((name.clone(), version));
            }
        }
        if found.is_empty() {
            return;
        }

        if let Ok(mut cache) = mod_version_cache().lock() {
            for (name, version) in &found {
                cache.insert(format!("{}/{}", dir.display(), name), version.clone());
            }
        }

        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let detail = ui.get_detail();
            if detail.id.as_str() != instance_id {
                return;
            }
            let Some(rows) = detail.mods.as_any().downcast_ref::<VecModel<ModRow>>() else {
                return;
            };
            let versions: HashMap<&str, &str> = found
                .iter()
                .map(|(name, version)| (name.as_str(), version.as_str()))
                .collect();
            for index in 0..rows.row_count() {
                let Some(mut row) = rows.row_data(index) else {
                    continue;
                };
                if row.version.is_empty() {
                    if let Some(version) = versions.get(row.file_name.as_str()) {
                        row.version = SharedString::from(*version);
                        rows.set_row_data(index, row);
                    }
                }
            }
        });
    });
}

/// Slint 行模型：最近一次启动日志的尾部（读取逻辑在 `mcl-app-model::read_recent_log`）
fn recent_log_rows(instance_dir: &Path) -> (Vec<SharedString>, String, String) {
    let (lines, hint) = read_recent_log(instance_dir);
    let diagnosis = mcl_core::crash::analyze(&lines.join("\n"))
        .map(|item| format!("{} —— {}", item.title, item.detail))
        .unwrap_or_default();
    let rows: Vec<SharedString> = lines.into_iter().map(SharedString::from).collect();
    (rows, hint, diagnosis)
}

fn wire_account_callbacks(
    runtime: &tokio::runtime::Runtime,
    ui: &AppWindow,
    toast_timer: &std::rc::Rc<Timer>,
) {
    // 添加离线账号：加即切换为当前（与 CLI 语义一致）
    ui.on_add_offline_account({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let handle = runtime.handle().clone();

        move |username| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let name = username.trim().to_string();
            if name.is_empty() {
                show_toast(&weak, &toast_timer, "用户名不能为空".to_string());
                return;
            }

            let result = AccountStore::open_default().and_then(|mut store| {
                let account = store.add_offline(&name)?;
                store.set_current(&account.id)?;
                Ok(account)
            });

            match result {
                Ok(account) => {
                    set_new_username(&ui, SharedString::default());
                    refresh_accounts(&ui, &handle);
                    show_toast(
                        &weak,
                        &toast_timer,
                        format!(
                            "已添加并切换为当前账号：{}（离线模式无法进入需正版验证的服务器）",
                            account.name
                        ),
                    );
                }
                Err(error) => {
                    show_toast(&weak, &toast_timer, format!("添加账号失败：{error}"));
                }
            }
        }
    });

    // 切换当前账号
    ui.on_use_account({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let handle = runtime.handle().clone();

        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match AccountStore::open_default().and_then(|mut store| store.set_current(&id)) {
                Ok(()) => {
                    refresh_accounts(&ui, &handle);
                    show_toast(&weak, &toast_timer, "当前账号已切换".to_string());
                }
                Err(error) => {
                    show_toast(&weak, &toast_timer, format!("切换失败：{error}"));
                }
            }
        }
    });

    // 删除账号：同步清理钥匙串令牌
    ui.on_remove_account({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();
        let handle = runtime.handle().clone();

        move |id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let result =
                AccountStore::open_default().and_then(|mut store| store.remove(&id).map(|_| ()));

            let _ = credential::delete_token(&id, TokenKind::MinecraftAccess);
            let _ = credential::delete_token(&id, TokenKind::MsaRefresh);

            match result {
                Ok(()) => {
                    refresh_accounts(&ui, &handle);
                    show_toast(
                        &weak,
                        &toast_timer,
                        "账号已删除（含钥匙串令牌）".to_string(),
                    );
                }
                Err(error) => {
                    show_toast(&weak, &toast_timer, format!("删除失败：{error}"));
                }
            }
        }
    });

    // 微软登录：设备码流程。client_id 解析顺序：运行时环境变量
    // MCL_MSA_CLIENT_ID → 构建期 option_env! 注入（无内置默认值：公共 ID 路线
    // 经真机验证不可行——第一方应用无法由用户授权，见 mcl-core::auth::microsoft）。
    // 未配置时直接给出注册指引，不把用户送进注定失败的浏览器授权流程。
    // 注意：async 任务内不得直接触碰 UI（Timer/Model 非 Send），一律经
    // `invoke_from_event_loop` 回 UI 线程；完成态 toast 不自动清除（与
    // launch/install 回调同款模式）
    ui.on_microsoft_login({
        let weak = ui.as_weak();
        let handle = runtime.handle().clone();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(client_id) = MicrosoftAuth::resolve_client_id() else {
                // 这是开发者配置项（用户的账号凭据与它无关）：未注入时应提示开发者，
                // 而不是要求用户去注册 Azure 应用
                if let Some(ui) = weak.upgrade() {
                    set_login_status(
                        &ui,
                        SharedString::from(
                            "微软登录尚未启用（开发者配置项）：需要开发者注册 Azure 应用，\
                         并在构建时注入 client_id（构建期环境变量 MCL_MSA_CLIENT_ID，\
                         或运行时同名环境变量用于调试）。\n\
                         注册与注入步骤见 README 的「微软登录」一节；\
                         配置完成前可使用离线账号。",
                        ),
                    );
                }
                show_toast(
                    &weak,
                    &toast_timer,
                    "微软登录未配置：需开发者注入 client_id（详见文档）".to_string(),
                );
                return;
            };

            let weak = weak.clone();
            let task_handle = handle.clone();
            task_handle.spawn(async move {
                let status = |weak: &slint::Weak<AppWindow>, text: String| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            set_login_status(&ui, SharedString::from(text));
                        }
                    });
                };

                let auth = match MicrosoftAuth::new(client_id) {
                    Ok(auth) => auth,
                    Err(error) => {
                        status(&weak, format!("微软登录初始化失败：{error}"));
                        return;
                    }
                };

                status(&weak, "正在申请设备码…".to_string());
                let device = match auth.request_device_code().await {
                    Ok(device) => device,
                    Err(error) => {
                        status(&weak, format!("申请设备码失败：{error}"));
                        return;
                    }
                };

                // 拿到设备码后自动打开系统浏览器（设备码流程的官方姿势），并把
                // 验证码/链接交给账号页展示（可复制、可重开）；打开失败不阻塞轮询
                let opened = platform::browser::open_in_browser(&device.verification_uri).is_ok();
                show_device_code(
                    &weak,
                    device.user_code.clone(),
                    device.verification_uri.clone(),
                    if opened {
                        "已在浏览器打开授权页面：输入下方验证码完成登录，本页会自动刷新".to_string()
                    } else {
                        "请点「打开浏览器」访问授权页面，并输入下方验证码".to_string()
                    },
                );

                let msa = match auth.poll_msa_token(&device).await {
                    Ok(msa) => msa,
                    Err(error) => {
                        clear_device_code(&weak);
                        status(&weak, format!("授权未完成：{error}"));
                        return;
                    }
                };

                status(
                    &weak,
                    "微软授权成功，正在完成 Minecraft 认证链…".to_string(),
                );
                let session = match auth
                    .login_with_msa(&msa.access_token, msa.refresh_token.clone())
                    .await
                {
                    Ok(session) => session,
                    Err(error) => {
                        clear_device_code(&weak);
                        status(&weak, format!("认证链失败：{error}"));
                        return;
                    }
                };

                let mut message = format!("✅ {} 登录成功", session.account.name);
                if let Ok(mut store) = AccountStore::open_default() {
                    let _ = store.upsert(session.account.clone());
                    let _ = store.set_current(&session.account.id);
                }
                match credential::save_token(
                    &session.account.id,
                    TokenKind::MinecraftAccess,
                    &session.access_token,
                ) {
                    Ok(()) => message.push_str("，令牌已写入系统钥匙串"),
                    Err(_) => message.push_str("，但系统钥匙串不可用，令牌仅本次会话有效"),
                }
                if let Some(refresh_token) = &session.refresh_token {
                    let _ = credential::save_token(
                        &session.account.id,
                        TokenKind::MsaRefresh,
                        refresh_token,
                    );
                }

                clear_device_code(&weak);
                status(&weak, String::new());
                let avatar_handle = tokio::runtime::Handle::current();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        refresh_accounts(&ui, &avatar_handle);
                        ui.set_toast(SharedString::from(message));
                    }
                });
            });
        }
    });

    // 设备码辅助操作：手动（重新）打开浏览器 / 一键复制验证码
    ui.on_open_login_url({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let url = ui.get_accounts_page().login_url.to_string();
            if url.is_empty() {
                return;
            }
            if let Err(error) = platform::browser::open_in_browser(&url) {
                show_toast(&weak, &toast_timer, error.to_string());
            }
        }
    });

    ui.on_copy_login_code({
        let weak = ui.as_weak();
        let toast_timer = toast_timer.clone();

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let code = ui.get_accounts_page().login_code.to_string();
            if code.is_empty() {
                return;
            }
            match platform::clipboard::copy_text(&code) {
                Ok(()) => show_toast(
                    &weak,
                    &toast_timer,
                    "验证码已复制，到浏览器粘贴即可".to_string(),
                ),
                Err(error) => show_toast(&weak, &toast_timer, error.to_string()),
            }
        }
    });
}

/// 把设备码与授权链接同步到账号页（tokio 侧 → UI 线程）
fn show_device_code(ui: &slint::Weak<AppWindow>, code: String, url: String, status: String) {
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            set_login_code(&ui, SharedString::from(code));
            set_login_url(&ui, SharedString::from(url));
            set_login_status(&ui, SharedString::from(status));
        }
    });
}

/// 清空账号页的设备码卡片（流程终态：验证码已使用或已失效）
fn clear_device_code(ui: &slint::Weak<AppWindow>) {
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            set_login_code(&ui, SharedString::default());
            set_login_url(&ui, SharedString::default());
        }
    });
}

/// 账号卡片文案：读取账号存储中的当前账号
/// 当前账号的展示信息：名称 / 侧栏副标题 / 头像占位字符 / 首页问候名（未登录为空）
fn current_account_parts() -> (String, String, String, String) {
    match AccountStore::open_default().ok().and_then(|store| {
        store
            .current()
            .map(|account| (account.name.clone(), account.kind.label()))
    }) {
        Some((name, kind)) => {
            let initial = name
                .chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| "?".to_string());
            let subtitle = if kind == "微软" {
                format!("{kind}账号 · 已登录")
            } else {
                format!("{kind}账号")
            };
            let greeting = name.clone();
            (name, subtitle, initial, greeting)
        }
        None => (
            "未登录".to_string(),
            "点击添加账号".to_string(),
            "?".to_string(),
            String::new(),
        ),
    }
}

/// 读取全部账号并转换为 UI 行模型（账号管理页）
fn account_rows() -> Vec<AccountRow> {
    let Ok(store) = AccountStore::open_default() else {
        return Vec::new();
    };

    store
        .list()
        .iter()
        .map(|account| {
            // 副标题模板自带「<类型>账号 · 」前缀（accounts.slint），这里只放附加信息：
            // 微软账号不再展示令牌寿命（24h 令牌按天取整会恒为「0 天后过期」，
            let info = match account.kind {
                mcl_core::models::AccountKind::Microsoft => String::new(),
                _ => match account.uuid.get(..8) {
                    Some(prefix) => format!("UUID {prefix}"),
                    None => String::new(),
                },
            };

            AccountRow {
                id: SharedString::from(account.id.clone()),
                name: SharedString::from(account.name.clone()),
                kind: SharedString::from(account.kind.label()),
                info: SharedString::from(info),
                is_current: store.current().map(|c| c.id.as_str()) == Some(account.id.as_str()),
                // 皮肤头像由 spawn_account_avatars 异步补齐，先用首字母占位
                has_avatar: false,
                avatar: slint::Image::default(),
                initial: SharedString::from(account.name.chars().next().unwrap_or('?').to_string()),
            }
        })
        .collect()
}

/// 刷新账号列表与左上角账号卡片（任何账号变更后调用）
fn refresh_accounts(ui: &AppWindow, handle: &tokio::runtime::Handle) {
    ui.set_accounts(ModelRc::new(VecModel::from(account_rows())));
    let (name, subtitle, initial, greeting) = current_account_parts();
    ui.set_account_name(SharedString::from(name));
    ui.set_account_subtitle(SharedString::from(subtitle));
    ui.set_account_initial(SharedString::from(initial));
    ui.set_greeting_name(SharedString::from(greeting));
    // 行与侧栏先用首字母，皮肤头像到位后替换
    ui.set_account_avatar_ready(false);
    ui.set_account_avatar(slint::Image::default());
    spawn_account_avatars(ui, handle);
}

/// 头像请求的常驻 HTTP 客户端（只用于公开档案与皮肤 PNG，独立于下载引擎）
fn avatar_http() -> Option<&'static mcl_core::http::HttpClient> {
    static HTTP: std::sync::OnceLock<Option<mcl_core::http::HttpClient>> =
        std::sync::OnceLock::new();
    HTTP.get_or_init(|| mcl_core::http::HttpClient::with_default_sources().ok())
        .as_ref()
}

/// 拉取微软账号的皮肤头像并写回行模型；失败保持首字母占位。
fn spawn_account_avatars(ui: &AppWindow, handle: &tokio::runtime::Handle) {
    let accounts: Vec<(String, String)> = AccountStore::open_default()
        .map(|store| {
            store
                .list()
                .iter()
                .filter(|account| account.kind == mcl_core::models::AccountKind::Microsoft)
                .map(|account| (account.id.clone(), account.uuid.clone()))
                .collect()
        })
        .unwrap_or_default();
    if accounts.is_empty() {
        return;
    }

    let weak = ui.as_weak();
    handle.spawn(async move {
        let Some(http) = avatar_http() else {
            return;
        };
        for (id, uuid) in accounts {
            let Some(icon) = fetch_avatar_head(http, &uuid).await else {
                continue;
            };
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                let image = to_slint_image(icon);
                let model = ui.get_accounts();
                if let Some(vec) = model.as_any().downcast_ref::<VecModel<AccountRow>>() {
                    for index in 0..vec.row_count() {
                        let Some(mut row) = vec.row_data(index) else {
                            continue;
                        };
                        if row.id.as_str() != id {
                            continue;
                        }
                        row.avatar = image.clone();
                        row.has_avatar = true;
                        vec.set_row_data(index, row);
                        break;
                    }
                }
                // 侧栏只显示当前账号
                let current = AccountStore::open_default()
                    .ok()
                    .and_then(|store| store.current().map(|account| account.id.clone()));
                if current.as_deref() == Some(id.as_str()) {
                    ui.set_account_avatar(image);
                    ui.set_account_avatar_ready(true);
                }
            });
        }
    });
}

/// 实例列表（读取 `instances.json`；最近游玩排前，未装成的标注状态）
/// RGBA8 → Slint 图像
fn to_slint_image(icon: IconImage) -> slint::Image {
    slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &icon.rgba,
            icon.width,
            icon.height,
        ),
    )
}

/// 首页 Hero 背景：编译期嵌入 WebP，运行时解码为 RGBA。
///
/// 用 WebP 而不是 PNG：同一张 1440×450 图 PNG 818 KB、WebP 55 KB，
/// 直接决定 release 产物能不能守住 20MB 预算。
/// 不用 `@image-url`：workspace 的 slint 关闭了 default-features，
/// 运行时没有图片解码器，解码统一走 `mcl-app-model::decode_image`。
fn hero_image() -> slint::Image {
    static ENCODED: &[u8] = include_bytes!("../ui/assets/home-hero.webp");
    mcl_app_model::decode_image(ENCODED, 1440)
        .map(to_slint_image)
        .unwrap_or_default()
}

/// 内置图标（解码失败时返回空图，调用方用 has_icon 判断）
fn builtin_image(kind: IconKind) -> slint::Image {
    builtin_icon(kind).map(to_slint_image).unwrap_or_default()
}

/// 内置图标选择器：两行 × 7，顺序与 `IconKind::ALL` 一致
fn icon_choices() -> (Vec<IconChoice>, Vec<IconChoice>) {
    let build = |slice: &[IconKind]| -> Vec<IconChoice> {
        slice
            .iter()
            .map(|kind| IconChoice {
                id: SharedString::from(kind.id()),
                icon: builtin_image(*kind),
            })
            .collect()
    };
    (build(&IconKind::ALL[..7]), build(&IconKind::ALL[7..]))
}

/// 安装向导与组件页共用的加载器图标
fn loader_icons() -> LoaderIcons {
    LoaderIcons {
        vanilla: builtin_image(IconKind::Grass),
        fabric: builtin_image(IconKind::Fabric),
        forge: builtin_image(IconKind::Forge),
        neoforge: builtin_image(IconKind::NeoForge),
        quilt: builtin_image(IconKind::Quilt),
        optifine: builtin_image(IconKind::Optifine),
        more: builtin_image(IconKind::Command),
    }
}

fn instance_rows() -> Vec<InstanceRow> {
    let Ok(store) = FileInstanceStore::open_default() else {
        return Vec::new();
    };
    let mut instances = store.snapshot().instances().to_vec();
    instances.sort_by(|left, right| {
        right
            .last_played
            .cmp(&left.last_played)
            .then(left.name.cmp(&right.name))
    });

    let mut rows: Vec<InstanceRow> = instances
        .into_iter()
        .map(|instance| {
            // 安装完成标志：版本 JSON 落地（对原版与加载器实例通用）
            let installed = GameLayout::new(&instance.dir)
                .version_json(&instance.version_id())
                .is_file();
            // 标题已含的字段不再在副标题重复（自动命名实例原先会出现整行重复）
            let loader_label = instance.loader().label();
            let loader_name = instance.loader().name();
            let version_line = format!("{} · {}", instance.mc_version, loader_label);
            let mc_line = format!("Minecraft {}", instance.mc_version);
            let loader_id = match instance.loader() {
                LoaderSpec::Vanilla => "vanilla",
                LoaderSpec::Fabric(_) => "fabric",
                LoaderSpec::Quilt(_) => "quilt",
                LoaderSpec::Forge(_) => "forge",
                LoaderSpec::NeoForge(_) => "neoforge",
            };
            let summary = instance_summary(
                &instance.name,
                &instance.mc_version,
                &loader_label,
                installed,
            );
            // 实例图标：内置选择 > 自定义 icon.png > 加载器默认
            let icon_settings = mcl_core::instance_settings::InstanceSettings::load(&instance.dir);
            let (icon, has_icon) = match instance_icon(
                &instance.dir,
                instance.loader(),
                Some(icon_settings.icon.as_str()),
            ) {
                Some(image) => (to_slint_image(image), true),
                None => (slint::Image::default(), false),
            };
            let icon_text = instance
                .name
                .chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_default();

            InstanceRow {
                id: SharedString::from(instance.id),
                name: SharedString::from(instance.name),
                info: SharedString::from(summary),
                icon,
                has_icon,
                icon_text: SharedString::from(icon_text),
                selected: false,
                installed,
                last_played: SharedString::from(last_played_label(instance.last_played)),
                version_line: SharedString::from(version_line),
                mc_line: SharedString::from(mc_line),
                loader_line: SharedString::from(loader_label),
                loader: SharedString::from(loader_id),
                dir: SharedString::from(instance.dir.to_string_lossy().to_string()),
                recent: instance.last_played.is_some(),
                // 状态在选中实例确定后统一回填（见下方）
                status: SharedString::default(),
                status_tone: 0,
                loader_name: SharedString::from(loader_name),
                dir_label: SharedString::from(abbreviate_dir(&instance.dir)),
                mc_version: SharedString::from(instance.mc_version),
                checked: false,
            }
        })
        .collect();

    // 当前实例置顶：首页 hero 直接渲染 instances[0]
    let selected_id = LauncherSettings::load().selected_instance;
    let index = rows
        .iter()
        .position(|row| !selected_id.is_empty() && row.id == selected_id)
        .unwrap_or(0);
    if let Some(row) = rows.get_mut(index) {
        row.selected = true;
    }
    if index != 0 && index < rows.len() {
        let row = rows.remove(index);
        rows.insert(0, row);
    }

    for row in &mut rows {
        let (status, tone) = if row.selected {
            ("当前实例", 1)
        } else if row.installed {
            ("已安装", 2)
        } else {
            ("未安装", 3)
        };
        row.status = SharedString::from(status);
        row.status_tone = tone;
    }
    rows
}

/// 当前选中的实例 id（设置里的值失效时回落到最近游玩的实例）
fn current_instance() -> Option<mcl_core::models::Instance> {
    let store = FileInstanceStore::open_default().ok()?;
    let selected = LauncherSettings::load().selected_instance;
    if !selected.is_empty() {
        if let Some(instance) = store.get(&selected) {
            return Some(instance);
        }
    }
    let mut instances = store.snapshot().instances().to_vec();
    instances.sort_by_key(|instance| std::cmp::Reverse(instance.last_played));
    instances.into_iter().next()
}

/// 顶栏右侧的当前实例胶囊文案：`名称 · 游戏版本`
fn current_instance_label() -> String {
    current_instance()
        .map(|instance| format!("{} · {}", instance.name, instance.mc_version))
        .unwrap_or_default()
}

/// 当前实例名（详情页顶栏「安装到：<名字>」用）
fn current_instance_name() -> String {
    current_instance()
        .map(|instance| instance.name)
        .unwrap_or_default()
}

/// 用已拉到的版本填充「版本表 + 更新日志」两个标签。
///
/// `type_filter` 为 all / release / beta / alpha；筛选在本地做，不重复请求。
fn apply_detail_versions(
    ui: &AppWindow,
    rows: &[mcl_core::resource::ResourceVersion],
    selected: Option<&str>,
) {
    let page = ui.get_versions_page();
    let type_filter = page.version_type.to_string();
    let mc_filter = page.detail_mc.to_string();
    let loader_filter = page.detail_loader.to_string();

    let mut filtered: Vec<&mcl_core::resource::ResourceVersion> = rows
        .iter()
        .filter(|row| type_filter == "all" || row.version_type == type_filter)
        .filter(|row| {
            mc_filter.is_empty()
                || row
                    .game_versions
                    .iter()
                    .any(|version| version == &mc_filter)
        })
        .filter(|row| {
            loader_filter.is_empty()
                || row
                    .loaders
                    .iter()
                    .any(|id| mcl_app_model::loader_label(id) == loader_filter)
        })
        .collect();
    // 选中某个版本时把它提到最前，复用「当前 + 更早」的渲染
    if let Some(id) = selected {
        if let Some(index) = filtered.iter().position(|row| row.id == id) {
            let picked = filtered.remove(index);
            filtered.insert(0, picked);
        }
    }

    // 版本表
    // 页码与文案统一由 model 层的 paging 生成（与下载中心同一套规则）
    let pager = mcl_app_model::Pager::new(
        page.version_pager.page.max(0) as usize,
        filtered.len(),
        mcl_app_model::VERSION_PAGE_SIZE,
        "个版本",
    );
    let (start, end) = pager.bounds(filtered.len(), mcl_app_model::VERSION_PAGE_SIZE);
    let table: Vec<ResourceVersionRow> = filtered[start..end]
        .iter()
        .enumerate()
        .map(|(index, row)| ResourceVersionRow {
            id: SharedString::from(row.id.clone()),
            version: SharedString::from(row.version_number.clone()),
            info: SharedString::from(format!(
                "{} · {}",
                row.game_versions.first().cloned().unwrap_or_default(),
                loader_display(row.loaders.first().map(String::as_str).unwrap_or(""))
            )),
            size: SharedString::from(format!(
                "{:.1} MB",
                row.files.first().map(|file| file.size).unwrap_or(0) as f64 / 1_048_576.0
            )),
            // 只给首页首行标「推荐」
            recommended: pager.page == 1 && index == 0,
            game_version: SharedString::from(
                row.game_versions.first().cloned().unwrap_or_default(),
            ),
            kind: SharedString::from(row.version_type.clone()),
            date: SharedString::from(mcl_app_model::date_only(&row.date_published)),
            downloads: SharedString::from(mcl_app_model::short_count(row.downloads)),
        })
        .collect();
    ui.set_res_versions(ModelRc::new(VecModel::from(table)));

    // 更新日志：当前 = 最新一条；更早的版本列出其后 6 条
    let current = filtered.first();
    let mut model = ui.get_versions_page();
    // 夹页后的真实页码要写回模型，UI 的按钮可用态才与实际一致
    model.version_pager = PagerState {
        page: pager.page as i32,
        pages: pager.pages as i32,
        hint: SharedString::from(pager.hint),
        label: SharedString::from(pager.label),
    };
    model.changelog_version = SharedString::from(
        current
            .map(|row| row.version_number.clone())
            .unwrap_or_default(),
    );
    model.changelog_date = SharedString::from(
        current
            .map(|row| mcl_app_model::date_only(&row.date_published))
            .unwrap_or_default(),
    );
    model.changelog_kind = SharedString::from(
        current
            .map(|row| row.version_type.clone())
            .unwrap_or_default(),
    );
    let lines: Vec<SharedString> = current
        .map(|row| changelog_lines(&row.changelog))
        .unwrap_or_default()
        .into_iter()
        .map(SharedString::from)
        .collect();
    model.changelog_empty = SharedString::from(if lines.is_empty() {
        if current.is_some() {
            "该版本没有提供更新日志。".to_string()
        } else {
            "没有匹配筛选条件的版本。".to_string()
        }
    } else {
        String::new()
    });
    model.changelog_lines = ModelRc::new(VecModel::from(lines));
    let earlier: Vec<ChangelogRow> = filtered
        .iter()
        .skip(1)
        .take(6)
        .map(|row| ChangelogRow {
            id: SharedString::from(row.id.clone()),
            version: SharedString::from(row.version_number.clone()),
            date: SharedString::from(mcl_app_model::date_only(&row.date_published)),
            summary: SharedString::from(
                changelog_lines(&row.changelog)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "无更新日志".to_string()),
            ),
        })
        .collect();
    model.changelog_earlier = ModelRc::new(VecModel::from(earlier));
    model.changelog_choices = ModelRc::new(VecModel::from(
        filtered
            .iter()
            .map(|row| SharedString::from(row.version_number.clone()))
            .collect::<Vec<_>>(),
    ));

    // 筛选候选：全部 + 去重后的游戏版本 / 加载器（保持站点给出的新→旧顺序）
    let mut mc_choices: Vec<SharedString> = vec![SharedString::from("全部")];
    let mut seen_mc: Vec<String> = Vec::new();
    let mut loader_choices: Vec<SharedString> = vec![SharedString::from("全部")];
    let mut seen_loader: Vec<String> = Vec::new();
    for row in rows {
        for version in &row.game_versions {
            if seen_mc.len() < 24 && !seen_mc.contains(version) {
                seen_mc.push(version.clone());
                mc_choices.push(SharedString::from(version.clone()));
            }
        }
        for id in &row.loaders {
            let label = mcl_app_model::loader_label(id);
            if !label.is_empty() && !seen_loader.contains(&label) {
                seen_loader.push(label.clone());
                loader_choices.push(SharedString::from(label));
            }
        }
    }
    model.detail_mc_choices = ModelRc::new(VecModel::from(mc_choices));
    model.detail_loader_choices = ModelRc::new(VecModel::from(loader_choices));

    let total = rows.len();
    model.version_count_hint = SharedString::from(if filtered.len() == total {
        format!("{total} 个版本")
    } else {
        format!("{} / {total} 个版本", filtered.len())
    });
    ui.set_versions_page(model);
}

/// 更新日志原文 → 逐行文本（去掉 Markdown 记号，最多 14 行）。
///
fn changelog_lines(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(mcl_app_model::description_blocks)
        .map(|block| match block.kind {
            mcl_app_model::DescriptionBlockKind::Bullet => format!("· {}", block.text),
            _ => block.text,
        })
        .filter(|line| !line.trim().is_empty())
        .take(14)
        .collect()
}

fn abbreviate_dir(dir: &Path) -> String {
    let text = dir.display().to_string();
    // 与 paths::data_dir 同一套取法（HOME / USERPROFILE），不引第三方 home crate
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if !home.is_empty() {
        if let Some(rest) = text.strip_prefix(home.as_str()) {
            return format!("~{rest}");
        }
    }
    text
}

/// 首页「启动环境」卡片的默认内存提示
fn memory_hint(memory_mb: u32) -> String {
    if memory_mb.is_multiple_of(1024) {
        format!("内存 {} GB · 使用全局默认", memory_mb / 1024)
    } else {
        format!("内存 {memory_mb} MB · 使用全局默认")
    }
}

/// 首页「启动环境 · 账号」：`名字 · 微软`
fn account_line_text() -> String {
    let (name, subtitle, _, _) = current_account_parts();
    if name == "未登录" {
        return "未登录".to_string();
    }
    // subtitle 形如「微软账号 · 已登录」/「离线账号」
    let kind = subtitle
        .split('账')
        .next()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .unwrap_or("本地");
    format!("{name} · {kind}")
}

/// 首页「启动环境 · Java」：`21.0.5 · 自动检测`
fn java_line_text() -> String {
    let runtimes = mcl_core::java::detect_javas();
    match runtimes.iter().max_by_key(|runtime| runtime.major) {
        Some(runtime) => {
            // 没有引号再退回第一个数字开头的词（避免把构建日期当成版本）
            let text = runtime.version_string.trim();
            let version = text
                .split('"')
                .nth(1)
                .map(str::to_string)
                .or_else(|| {
                    text.split_whitespace()
                        .find(|part| {
                            part.chars().next().is_some_and(|ch| ch.is_ascii_digit())
                                && part.contains('.')
                        })
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("Java {}", runtime.major));
            format!("{version} · 自动检测")
        }
        None => "未检测到".to_string(),
    }
}

/// 首页「启动环境 · 下载源」
fn download_source_line(source: &str) -> String {
    match source {
        "official" => "仅官方源".to_string(),
        "mirror" => "仅镜像 · BMCLAPI".to_string(),
        _ => "自动 · 官方优先".to_string(),
    }
}

/// 探测本机 Java 并写入首页首次启动的状态行。
///
/// 只做本地探测（共享的 detect_javas），不联网；下载动作由「立即下载」触发。
fn java_hint_text() -> (String, bool) {
    let runtimes = mcl_core::java::detect_javas();
    match runtimes.iter().map(|runtime| runtime.major).max() {
        Some(major) if major >= 21 => (format!("已检测到 Java {major}，可直接开始"), true),
        Some(major) => (
            format!("当前 Java {major} 版本偏低，将自动下载 Java 21"),
            false,
        ),
        None => ("Java 运行时未检测到，将自动下载 Java 21".to_string(), false),
    }
}

fn refresh_java_hint(ui: &AppWindow) {
    let (hint, ready) = java_hint_text();
    ui.set_java_hint(SharedString::from(hint));
    ui.set_java_ready(ready);
}

/// 当前多选集合（页面模型的 selected-ids 是唯一事实源）
fn selected_ids(ui: &AppWindow) -> Vec<String> {
    ui.get_instances_page()
        .selected_ids
        .iter()
        .map(|id| id.to_string())
        .collect()
}

/// 写回多选集合：更新计数并重建视图（行的 checked 由视图重建时回填）
fn apply_selection(ui: &AppWindow, ids: Vec<String>) {
    let mut page = ui.get_instances_page();
    page.selected_count = ids.len() as i32;
    page.selected_ids = ModelRc::new(VecModel::from(
        ids.iter()
            .map(|id| SharedString::from(id.as_str()))
            .collect::<Vec<_>>(),
    ));
    page.selecting = !ids.is_empty() || page.selecting;
    ui.set_instances_page(page);
    refresh_instance_view(ui);
}

/// 刷新实例列表（安装 / 启动 / 移除后调用）
fn refresh_instances(ui: &AppWindow) {
    ui.set_instances(ModelRc::new(VecModel::from(instance_rows())));
    ui.set_current_instance_label(SharedString::from(current_instance_label()));
    ui.set_current_instance_name(SharedString::from(current_instance_name()));
    refresh_instance_view(ui);
    refresh_directories(ui);
}

/// 按当前搜索词与加载器筛选重建实例库视图（过滤规则在 mcl-app-model::filter）
fn refresh_instance_view(ui: &AppWindow) {
    let rows = ui.get_instances();
    let page = ui.get_instances_page();
    let query = page.query.to_string();
    let filter = page.filter.to_string();
    let view = page.view.to_string();
    let directory = page.directory.to_string();

    let total = rows.row_count();
    let selected: std::collections::HashSet<String> = selected_ids(ui).into_iter().collect();
    let mut view_rows = Vec::new();
    // 分组计数（左列角标）基于当前目录过滤后的全集
    let mut count_recent = 0i32;
    let mut count_vanilla = 0i32;
    let mut count_fabric = 0i32;
    let mut count_forge = 0i32;
    let mut count_neoforge = 0i32;
    let mut count_quilt = 0i32;
    for index in 0..total {
        if let Some(row) = rows.row_data(index) {
            if !directory.is_empty() && !row.dir.starts_with(directory.as_str()) {
                continue;
            }
            if row.recent {
                count_recent += 1;
            }
            match row.loader.to_string().as_str() {
                "vanilla" => count_vanilla += 1,
                "fabric" => count_fabric += 1,
                "forge" => count_forge += 1,
                "neoforge" => count_neoforge += 1,
                "quilt" => count_quilt += 1,
                _ => {}
            }
            let view_ok = view != "recent" || row.recent;
            if view_ok
                && instance_matches(&query, &filter, &row.name, &row.version_line, &row.loader)
            {
                let mut row = row;
                row.checked = selected.contains(row.id.as_str());
                view_rows.push(row);
            }
        }
    }
    let matched = view_rows.len();

    ui.set_instance_view(ModelRc::new(VecModel::from(view_rows)));

    let mut page = ui.get_instances_page();
    page.count_all = total as i32;
    page.count_recent = count_recent;
    page.count_vanilla = count_vanilla;
    page.count_fabric = count_fabric;
    page.count_forge = count_forge;
    page.count_neoforge = count_neoforge;
    page.count_quilt = count_quilt;
    page.count_hint = SharedString::from(if total == 0 {
        "还没有实例 · 点右上角「新建实例」开始".to_string()
    } else if matched == total {
        format!("共 {total} 个实例")
    } else {
        format!("共 {total} 个实例 · 显示 {matched} 个")
    });
    page.empty_hint = SharedString::from(if total == 0 {
        "新建一个实例，或导入已有的整合包。".to_string()
    } else {
        "换个关键词，或把左列筛选切回「全部」。".to_string()
    });
    ui.set_instances_page(page);
}

/// 重建游戏目录模型（实例库左列 + 目录管理页共用；首项为「全部目录」）
fn refresh_directories(ui: &AppWindow) {
    let page = ui.get_instances_page();
    let directory = page.directory.to_string();
    let mut items: Vec<DirectoryItem> = Vec::new();
    let real = game_directories();
    let total: usize = real.iter().map(|dir| dir.instance_count).sum();
    items.push(DirectoryItem {
        label: SharedString::from("全部目录"),
        path: SharedString::default(),
        count: total as i32,
        is_default: false,
        selected: directory.is_empty(),
    });
    for dir in real {
        items.push(DirectoryItem {
            label: SharedString::from(dir.label.clone()),
            path: SharedString::from(dir.path.clone()),
            count: dir.instance_count as i32,
            is_default: dir.is_default,
            selected: dir.path == directory,
        });
    }
    ui.set_directories(ModelRc::new(VecModel::from(items)));
}

/// 更新设置页模型（Slint 结构体属性是值语义，必须 get-modify-set）
fn update_settings(ui: &AppWindow, apply: impl FnOnce(&mut SettingsPageModel)) {
    let mut model = ui.get_settings();
    apply(&mut model);
    ui.set_settings(model);
}

fn set_download_concurrency(ui: &AppWindow, value: i32) {
    update_settings(ui, |model| model.download_concurrency = value);
}

fn set_gc_status(ui: &AppWindow, text: impl Into<SharedString>) {
    let text = text.into();
    update_settings(ui, |model| model.gc_status = text);
}

fn set_gc_armed(ui: &AppWindow, armed: bool) {
    update_settings(ui, |model| model.gc_armed = armed);
}

/// 后台统计下载缓存占用（数千文件的一次性遍历，放 UI 线程会拖慢首帧）
fn refresh_cache_size(runtime: &tokio::runtime::Runtime, ui: &AppWindow) {
    let weak = ui.as_weak();
    runtime.spawn_blocking(move || {
        // 封面 / 资源元数据缓存按同一策略裁剪（每进程一次）
        let _ = prune_cover_cache();
        let _ = prune_resource_cache();
        let bytes = dir_size(&mcl_core::download::engine::cache_root())
            + dir_size(&cover_cache_dir())
            + dir_size(&resource_cache_dir());
        let text = format_size(bytes);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                update_settings(&ui, |model| model.cache_size = SharedString::from(text));
            }
        });
    });
}

/// 更新版本页模型（Slint 结构体属性是值语义，必须 get-modify-set）
fn update_versions_page(ui: &AppWindow, apply: impl FnOnce(&mut VersionsPageModel)) {
    let mut model = ui.get_versions_page();
    apply(&mut model);
    ui.set_versions_page(model);
}

/// 更新账号页模型（Slint 结构体属性是值语义，必须 get-modify-set）
fn update_accounts_page(ui: &AppWindow, apply: impl FnOnce(&mut AccountsPageModel)) {
    let mut model = ui.get_accounts_page();
    apply(&mut model);
    ui.set_accounts_page(model);
}

fn set_new_username(ui: &AppWindow, value: impl Into<SharedString>) {
    let value = value.into();
    update_accounts_page(ui, |model| model.new_username = value);
}

fn set_login_status(ui: &AppWindow, value: impl Into<SharedString>) {
    let value = value.into();
    update_accounts_page(ui, |model| model.login_status = value);
}

fn set_login_code(ui: &AppWindow, value: impl Into<SharedString>) {
    let value = value.into();
    update_accounts_page(ui, |model| model.login_code = value);
}

fn set_login_url(ui: &AppWindow, value: impl Into<SharedString>) {
    let value = value.into();
    update_accounts_page(ui, |model| model.login_url = value);
}

fn show_toast(ui: &slint::Weak<AppWindow>, timer: &Timer, message: String) {
    let Some(window) = ui.upgrade() else {
        return;
    };
    window.set_toast(SharedString::from(message));

    let weak = ui.clone();
    timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(2600),
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_toast(SharedString::default());
            }
        },
    );
}
