use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mcl_core::models::TaskState;
use mcl_core::pack::PackInstallReport;
use mcl_core::version::service::InstallReport;

/// 任务类型（任务中心重试/续传所需：知道"要做什么"才能重新执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    /// 下载并安装 Java 运行时（首次启动「立即下载」用它）
    InstallJava { major: u32 },
    /// 新建实例并安装：实例名 / 图标 / 父目录都由安装设置页给定。
    ///
    /// `loader` 为 `None` 时只装原版；`Some((kind, loader_version))` 时同时装加载器。
    InstallVersion {
        version: String,
        /// 实例显示名（`None` = 「版本 + 加载器」默认名）
        name: Option<String>,
        /// 内置图标 id（`None` / 空 = 跟随加载器）
        icon: Option<String>,
        /// 父目录（`None` = 设置里的默认实例根）
        root: Option<std::path::PathBuf>,
        /// `(fabric/forge/neoforge/quilt, 加载器版本)`
        loader: Option<(String, String)>,
    },
    /// 给已有实例安装/更换加载器（组件安装）
    InstallLoader {
        instance_id: String,
        instance_name: String,
        /// fabric / quilt / forge / neoforge
        kind: String,
        loader_version: String,
    },
    /// 启动实例（任务化的意义：Java 运行时自动下载的进度/暂停经任务行呈现）
    Launch {
        instance_id: String,
        instance_name: String,
    },
    /// 修复实例
    Repair {
        instance_id: String,
        instance_name: String,
    },
    /// 安装内置精选整合包
    InstallPack(String),
    /// 导入本地 `.mrpack`
    ImportPack(String),
}

/// 任务重要性（借鉴 HMCL 的 `TaskSignificance`）
///
/// - `Major`：有下载进度、用户主动发起（安装/整合包）→ 进浮动汇总条
/// - `Moderate`：有进度但属于「顺手做」的事（启动时的 Java 下载、修复）→ 只进列表
/// - `Minor`：纯后台动作 → 列表置灰
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSignificance {
    Major,
    Moderate,
    Minor,
}

impl TaskKind {
    /// 该任务在 UI 上的分量
    pub fn significance(&self) -> TaskSignificance {
        match self {
            TaskKind::InstallVersion { .. }
            | TaskKind::InstallLoader { .. }
            | TaskKind::InstallPack(_)
            | TaskKind::InstallJava { .. } => TaskSignificance::Major,
            TaskKind::ImportPack(_) | TaskKind::Launch { .. } => TaskSignificance::Moderate,
            TaskKind::Repair { .. } => TaskSignificance::Minor,
        }
    }

    /// 任务行标题（纯展示逻辑，UI 壳直接取用）。
    pub fn title(&self) -> String {
        match self {
            TaskKind::InstallVersion {
                version, loader, ..
            } => match loader {
                Some((kind, loader_version)) => format!(
                    "安装 {version} + {} {loader_version}",
                    crate::LoaderKind::from_id(kind)
                        .map(|kind| kind.label())
                        .unwrap_or("加载器")
                ),
                None => format!("安装 {version}（原版）"),
            },
            TaskKind::InstallLoader {
                instance_name,
                kind,
                loader_version,
                ..
            } => {
                let label = crate::LoaderKind::from_id(kind)
                    .map(|kind| kind.label())
                    .unwrap_or("加载器");
                if loader_version.is_empty() {
                    format!("为 {instance_name} 安装 {label} 最新版")
                } else {
                    format!("为 {instance_name} 安装 {label} {loader_version}")
                }
            }
            TaskKind::InstallJava { major } => format!("下载 Java {major}"),
            TaskKind::Launch { instance_name, .. } => format!("启动 {instance_name}"),
            TaskKind::Repair { instance_name, .. } => format!("修复 {instance_name}"),
            TaskKind::InstallPack(pack_id) => {
                let name = mcl_core::pack::curated_pack(pack_id)
                    .map(|pack| pack.name)
                    .unwrap_or(pack_id.as_str());
                format!("安装整合包 {name}")
            }
            TaskKind::ImportPack(path) => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                format!("导入整合包 {name}")
            }
        }
    }
}

/// 一次任务执行的结果（区分「完成 / 被暂停 / 失败」三态，驱动任务行与续传）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Done(String),
    Paused,
    Failed(String),
}

/// 任务注册表：任务中心数据源（状态 / 取消旗标 / 重试所需的任务类型）。
///
/// 跨线程共享（UI 回调与 tokio 任务都访问）；内层 `Mutex` 临界区极短，
/// 不与任何网络/磁盘操作交叉。
#[derive(Default)]
pub struct TaskRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    next_id: u64,
    entries: HashMap<u64, TaskEntry>,
}

struct TaskEntry {
    kind: TaskKind,
    state: TaskState,
    /// 当前运行代的取消旗标（仅运行中有效；暂停/失败/完成后清空）
    cancel: Option<Arc<AtomicBool>>,
}

impl TaskRegistry {
    pub fn register(&self, kind: TaskKind) -> u64 {
        let mut inner = self.inner.lock().expect("任务表锁未被毒化");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.entries.insert(
            id,
            TaskEntry {
                kind,
                state: TaskState::Pending,
                cancel: None,
            },
        );
        id
    }

    /// 开始一次执行代：登记新的取消旗标并置为运行中。
    pub fn start_run(&self, id: u64) -> Option<Arc<AtomicBool>> {
        let mut inner = self.inner.lock().expect("任务表锁未被毒化");
        let entry = inner.entries.get_mut(&id)?;
        let flag = Arc::new(AtomicBool::new(false));
        entry.cancel = Some(flag.clone());
        entry.state = TaskState::Running;
        Some(flag)
    }

    /// 请求取消（运行中才有意义）；返回是否实际发出请求。
    pub fn request_cancel(&self, id: u64) -> bool {
        let mut inner = self.inner.lock().expect("任务表锁未被毒化");
        match inner
            .entries
            .get_mut(&id)
            .and_then(|entry| entry.cancel.as_ref())
        {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn kind(&self, id: u64) -> Option<TaskKind> {
        self.inner
            .lock()
            .expect("任务表锁未被毒化")
            .entries
            .get(&id)
            .map(|entry| entry.kind.clone())
    }

    pub fn state(&self, id: u64) -> Option<TaskState> {
        self.inner
            .lock()
            .expect("任务表锁未被毒化")
            .entries
            .get(&id)
            .map(|entry| entry.state)
    }

    pub fn set_state(&self, id: u64, state: TaskState) {
        if let Some(entry) = self
            .inner
            .lock()
            .expect("任务表锁未被毒化")
            .entries
            .get_mut(&id)
        {
            entry.state = state;
            if matches!(
                state,
                TaskState::Paused | TaskState::Done | TaskState::Failed
            ) {
                entry.cancel = None;
            }
        }
    }

    /// 清除已完成/失败任务，返回被清除的 id（UI 同步移除行）。
    pub fn clear_finished(&self) -> Vec<u64> {
        let mut inner = self.inner.lock().expect("任务表锁未被毒化");
        let finished: Vec<u64> = inner
            .entries
            .iter()
            .filter(|(_, entry)| matches!(entry.state, TaskState::Done | TaskState::Failed))
            .map(|(id, _)| *id)
            .collect();
        for id in &finished {
            inner.entries.remove(id);
        }
        finished
    }
}

/// 安装报告 → 任务结果三态（完成 / 暂停 / 失败）。
pub fn report_to_outcome(
    report: &InstallReport,
    instance_name: &str,
    loader: Option<&str>,
) -> InstallOutcome {
    if report.cancelled {
        return InstallOutcome::Paused;
    }
    if report.is_complete() {
        let loader_note = loader
            .map(|loader| format!("Fabric {loader}，"))
            .unwrap_or_default();
        InstallOutcome::Done(format!(
            "「{instance_name}」安装完成（{loader_note}{} 个文件，{:.0} MB）",
            report.files,
            report.bytes as f64 / 1_048_576.0
        ))
    } else {
        InstallOutcome::Failed(format!(
            "「{instance_name}」有 {} 个文件失败",
            report.failures.len()
        ))
    }
}

/// 整合包安装报告 → 任务结果三态。
pub fn pack_report_to_outcome(report: &PackInstallReport) -> InstallOutcome {
    if report.cancelled {
        return InstallOutcome::Paused;
    }
    if !report.failures.is_empty() {
        return InstallOutcome::Failed(format!(
            "整合包「{}」有 {} 个文件失败（可在详情页用「修复」补齐）",
            report.instance.name,
            report.failures.len()
        ));
    }
    InstallOutcome::Done(format!(
        "整合包「{}」安装完成（{} 个文件 / {:.1} MB，overrides {} 个）",
        report.instance.name,
        report.files,
        report.bytes as f64 / 1_048_576.0,
        report.overrides
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_are_major_and_launch_is_moderate() {
        assert_eq!(
            TaskKind::InstallVersion {
                version: "1.20.1".into(),
                name: None,
                icon: None,
                root: None,
                loader: None
            }
            .significance(),
            TaskSignificance::Major
        );
        assert_eq!(
            TaskKind::InstallVersion {
                version: "1.20.1".into(),
                name: None,
                icon: None,
                root: None,
                loader: Some(("forge".into(), "47.2.0".into()))
            }
            .significance(),
            TaskSignificance::Major
        );
        assert_eq!(
            TaskKind::Launch {
                instance_id: "a".into(),
                instance_name: "A".into()
            }
            .significance(),
            TaskSignificance::Moderate
        );
        assert_eq!(
            TaskKind::Repair {
                instance_id: "a".into(),
                instance_name: "A".into()
            }
            .significance(),
            TaskSignificance::Minor
        );
    }

    #[test]
    fn registry_tracks_states_and_clears_finished() {
        let registry = TaskRegistry::default();
        let id = registry.register(TaskKind::InstallVersion {
            version: "1.20.1".into(),
            name: None,
            icon: None,
            root: None,
            loader: None,
        });

        assert_eq!(registry.state(id), Some(TaskState::Pending));
        assert_eq!(
            registry.kind(id),
            Some(TaskKind::InstallVersion {
                version: "1.20.1".into(),
                name: None,
                icon: None,
                root: None,
                loader: None
            })
        );

        let cancel = registry.start_run(id).expect("运行中应有取消旗标");
        assert_eq!(registry.state(id), Some(TaskState::Running));
        assert!(!cancel.load(Ordering::Relaxed));

        assert!(registry.request_cancel(id));
        assert!(cancel.load(Ordering::Relaxed));

        // 终态清理取消旗标，再取消应返回 false
        registry.set_state(id, TaskState::Done);
        assert!(!registry.request_cancel(id));

        assert_eq!(registry.clear_finished(), vec![id]);
        assert_eq!(registry.state(id), None);
        assert!(registry.clear_finished().is_empty());
    }

    #[test]
    fn cancelled_report_maps_to_paused() {
        let report = InstallReport {
            cancelled: true,
            ..Default::default()
        };
        assert_eq!(
            report_to_outcome(&report, "1.20.1", None),
            InstallOutcome::Paused
        );
    }

    #[test]
    fn failed_report_maps_to_failed_with_count() {
        let report = InstallReport {
            files: 10,
            failures: vec![("a.jar".into(), "timeout".into())],
            ..Default::default()
        };
        match report_to_outcome(&report, "1.20.1", None) {
            InstallOutcome::Failed(message) => {
                assert!(message.contains("1 个文件失败"), "{message}");
            }
            other => panic!("应为失败态：{other:?}"),
        }
    }
}
