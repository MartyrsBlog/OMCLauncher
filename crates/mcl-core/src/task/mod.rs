//! 任务调度与事件总线
//!
//! 全局单例事件总线：领域服务发布事件，UI 桥接层订阅后经 `slint::Channel`
//! 投递到 UI 事件循环（见 `mcl-app`）。同进程零 IPC 开销。

use crate::download::DownloadTask;
use crate::models::TaskState;
use tokio::sync::broadcast;

/// 领域事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// 进度上报
    Progress {
        id: u64,
        downloaded: u64,
        total: u64,
    },
    /// 任务状态变化（`message` 为给用户看的结果/原因，如「已完成」「连接超时」）
    StateChanged {
        id: u64,
        state: TaskState,
        message: String,
    },
    /// 全部任务完成
    AllCompleted { instance_id: u64 },
}

/// 领域事件总线
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<TaskEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 订阅事件流（UI 桥接层 / 其它服务）
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.tx.subscribe()
    }

    /// 发布事件；无订阅者时静默丢弃（不阻塞业务）
    pub fn publish(&self, event: TaskEvent) {
        let _ = self.tx.send(event);
    }

    /// 当前订阅者数量（诊断用）
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

/// 任务调度器骨架：并发控制 + 依赖顺序
///
/// M0 仅保留调度参数与事件发布；M1 接入 tokio 任务池与分块下载执行。
pub struct TaskScheduler {
    bus: EventBus,
    max_concurrency: usize,
}

impl TaskScheduler {
    /// 默认并发度 8
    pub fn new(bus: EventBus, max_concurrency: usize) -> Self {
        Self {
            bus,
            max_concurrency: max_concurrency.max(1),
        }
    }

    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    pub fn set_concurrency(&mut self, value: usize) {
        self.max_concurrency = value.max(1);
    }

    /// 提交任务（M1 实现真正的入队与执行）
    pub fn submit(&self, task: &DownloadTask) {
        self.bus.publish(TaskEvent::Progress {
            id: task.id,
            downloaded: 0,
            total: task.file.size,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_subscriber() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        bus.publish(TaskEvent::StateChanged {
            id: 7,
            state: TaskState::Running,
            message: String::new(),
        });

        let received = rx.recv().await.expect("应收到事件");
        assert_eq!(
            received,
            TaskEvent::StateChanged {
                id: 7,
                state: TaskState::Running,
                message: String::new()
            }
        );
    }

    #[tokio::test]
    async fn publish_without_subscriber_does_not_panic() {
        let bus = EventBus::default();
        bus.publish(TaskEvent::AllCompleted { instance_id: 1 });
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn scheduler_clamps_concurrency_to_at_least_one() {
        let mut scheduler = TaskScheduler::new(EventBus::default(), 0);
        assert_eq!(scheduler.max_concurrency(), 1);
        scheduler.set_concurrency(16);
        assert_eq!(scheduler.max_concurrency(), 16);
        scheduler.set_concurrency(0);
        assert_eq!(scheduler.max_concurrency(), 1);
    }
}
