use crate::error::{NuonoError, Result};
use futures::FutureExt;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex as SyncMutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinSet;
use tracing::{info, warn};
use uuid::Uuid;

/// 任务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 等待执行
    Pending,
    /// 执行中
    Running,
    /// 已完成
    Completed,
    /// 失败
    Failed,
    /// 已取消
    Cancelled,
    /// 超时
    TimedOut,
}

/// 任务元数据
#[derive(Debug, Clone)]
pub struct TaskMetadata {
    /// 任务 ID
    pub task_id: String,
    /// 任务名称
    pub name: String,
    /// 插件名称
    pub plugin_name: String,
    /// 服务名称
    pub service_name: Option<String>,
    /// 优先级（数值越小优先级越高）
    pub priority: i8,
    /// 创建时间
    pub created_at: Instant,
    /// 开始执行时间
    pub started_at: Option<Instant>,
    /// 完成时间
    pub completed_at: Option<Instant>,
    /// 超时时间
    pub timeout: Option<Duration>,
    /// 当前状态
    pub status: TaskStatus,
    /// 错误信息
    pub error: Option<String>,
    /// 追踪 ID
    pub trace_id: String,
}

impl TaskMetadata {
    pub fn new(name: impl Into<String>, plugin_name: impl Into<String>, priority: i8) -> Self {
        Self {
            task_id: Uuid::new_v4().to_string(),
            name: name.into(),
            plugin_name: plugin_name.into(),
            service_name: None,
            priority,
            created_at: Instant::now(),
            started_at: None,
            completed_at: None,
            timeout: None,
            status: TaskStatus::Pending,
            error: None,
            trace_id: Uuid::new_v4().to_string(),
        }
    }

    pub fn with_service_name(mut self, service_name: impl Into<String>) -> Self {
        self.service_name = Some(service_name.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = trace_id.into();
        self
    }

    /// 获取任务执行时长
    pub fn duration(&self) -> Option<Duration> {
        match (self.started_at, self.completed_at) {
            (Some(start), Some(end)) => Some(end.duration_since(start)),
            (Some(start), None) => Some(start.elapsed()),
            _ => None,
        }
    }

    /// 检查是否超时
    pub fn is_timed_out(&self) -> bool {
        if let (Some(started), Some(timeout)) = (self.started_at, self.timeout) {
            started.elapsed() > timeout
        } else {
            false
        }
    }
}

/// 任务执行结果
#[derive(Debug, Clone)]
pub struct TaskResult {
    /// 任务元数据
    pub metadata: TaskMetadata,
    /// 执行结果
    pub output: Option<serde_json::Value>,
    /// 错误信息
    pub error: Option<String>,
}

/// 任务执行函数类型
pub type TaskFn = Box<
    dyn FnOnce() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send>,
        > + Send
        + 'static,
>;

/// 兼容的任务请求描述；执行器通过 submit* 接收任务，通过 cancel(task_id) 取消。
pub struct TaskRequest {
    /// 任务元数据
    pub metadata: TaskMetadata,
    /// 执行函数
    pub func: TaskFn,
    /// 旧字段，仅保留源码兼容；submit* 不使用此发送端。
    pub cancel_tx: Option<oneshot::Sender<()>>,
    /// 结果通知
    pub result_tx: Option<oneshot::Sender<TaskResult>>,
}

impl std::fmt::Debug for TaskRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskRequest")
            .field("metadata", &self.metadata)
            .field("func", &"<closure>")
            .finish()
    }
}

/// 带总容量上限（含排队中与运行中任务）的独立优先级任务执行器。
///
/// 默认最多容纳 1024 个未完成任务；队列已满或执行器已关闭时会立即拒绝新提交，不会额外排队等待。
/// 优先级数值越小越先执行，相同优先级不保证严格 FIFO 顺序。
/// `shutdown` 会等待已准入的任务排空；调用 `cancel` 或丢弃 `submit_and_wait` 返回的 Future 会取消对应任务；
/// 丢弃整个执行器则会取消所有未完成任务。取消操作仅中止当前异步任务，无法回滚已产生的外部副作用或脱离监管的自建协程。
pub struct TaskExecutor {
    shared: Arc<TaskTracking>,
    max_concurrency: usize,
    shutdown_complete_rx: watch::Receiver<bool>,
}

const DEFAULT_CAPACITY: usize = 1024;
const MAX_HISTORY: usize = 1000;

type QueueKey = (i8, String);

struct PendingTask {
    func: TaskFn,
    cancelled: watch::Receiver<bool>,
}

struct ActiveTask {
    metadata: TaskMetadata,
    cancel: watch::Sender<bool>,
    result_tx: Option<oneshot::Sender<TaskResult>>,
}

#[derive(Default)]
struct TaskState {
    // 取消排队中的任务时会直接从 BTreeMap 中移除对应条目，不留墓碑节点（tombstone）。
    pending: BTreeMap<QueueKey, PendingTask>,
    active: HashMap<String, ActiveTask>,
    history: VecDeque<TaskResult>,
    closing: bool,
}

struct TaskTracking {
    state: SyncMutex<TaskState>,
    wake: Notify,
    capacity: usize,
}

struct TaskOutcome {
    status: TaskStatus,
    output: Option<serde_json::Value>,
    error: Option<String>,
}

impl TaskOutcome {
    fn cancelled() -> Self {
        Self {
            status: TaskStatus::Cancelled,
            output: None,
            error: Some("任务已取消".into()),
        }
    }

    fn failed(message: &str) -> Self {
        Self {
            status: TaskStatus::Failed,
            output: None,
            error: Some(message.into()),
        }
    }
}

impl TaskTracking {
    fn lock(&self) -> MutexGuard<'_, TaskState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn finish(&self, task_id: &str, mut outcome: TaskOutcome) {
        let (result, result_tx) = {
            let mut state = self.lock();
            let Some(mut task) = state.active.remove(task_id) else {
                return;
            };
            // 任务完成与任务取消在同一把状态锁内串行化判定。
            if *task.cancel.borrow() {
                outcome = TaskOutcome::cancelled();
            }
            task.metadata.status = outcome.status;
            task.metadata.completed_at = Some(Instant::now());
            task.metadata.error = outcome.error.clone();
            let result = TaskResult {
                metadata: task.metadata,
                output: outcome.output,
                error: outcome.error,
            };
            state.history.push_back(result.clone());
            while state.history.len() > MAX_HISTORY {
                state.history.pop_front();
            }
            (result, task.result_tx)
        };
        let status = format!("{:?}", result.metadata.status).to_lowercase();
        metrics::counter!("nuono_tasks_completed_total",
            "plugin" => result.metadata.plugin_name.clone(), "status" => status.clone()
        )
        .increment(1);
        if let Some(duration) = result.metadata.duration() {
            metrics::histogram!("nuono_task_duration_seconds",
                "plugin" => result.metadata.plugin_name.clone(), "status" => status
            )
            .record(duration.as_secs_f64());
        }
        info!(task_id, task_name=%result.metadata.name, status=?result.metadata.status,
            error=?result.error, "任务执行完成");
        if let Some(result_tx) = result_tx {
            let _ = result_tx.send(result);
        }
        self.wake.notify_one();
    }

    fn cancel(&self, task_id: &str) -> bool {
        self.cancel_if_current(task_id, None)
    }

    fn cancel_if_current(&self, task_id: &str, expected: Option<&watch::Sender<bool>>) -> bool {
        let pending = {
            let mut state = self.lock();
            let Some(task) = state.active.get(task_id) else {
                return false;
            };
            if expected.is_some_and(|expected| !expected.same_channel(&task.cancel)) {
                return false;
            }
            if task.cancel.send_replace(true) {
                return false;
            }
            let key = (task.metadata.priority, task_id.to_owned());
            state.pending.remove(&key)
        };
        if let Some(pending) = pending {
            // 在锁外且归还容量许可之前销毁用户闭包捕获的环境，避免析构耗时延长锁持有时间或提前放行新任务。
            self.discard_pending(task_id, pending);
        }
        self.wake.notify_one();
        true
    }

    fn discard_pending(&self, task_id: &str, pending: PendingTask) {
        if std::panic::catch_unwind(AssertUnwindSafe(|| drop(pending))).is_err() {
            warn!(task_id, "已取消任务的闭包析构时发生 panic");
        }
        self.finish(task_id, TaskOutcome::cancelled());
    }

    fn close(&self) {
        self.lock().closing = true;
        self.wake.notify_one();
    }

    fn cancel_all(&self) {
        let pending = {
            let mut state = self.lock();
            state.closing = true;
            for task in state.active.values() {
                task.cancel.send_replace(true);
            }
            std::mem::take(&mut state.pending)
        };
        for ((_, task_id), pending) in pending {
            self.discard_pending(&task_id, pending);
        }
        self.wake.notify_one();
    }
}

/// `submit_and_wait` 调用方放弃等待时，同步取消尚在排队的闭包或正在执行的异步任务。
struct CancelOnDrop {
    tracking: Arc<TaskTracking>,
    task_id: Option<String>,
    cancellation: watch::Sender<bool>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(task_id) = self.task_id.take() {
            self.tracking
                .cancel_if_current(&task_id, Some(&self.cancellation));
        }
    }
}

/// 任务收尾守卫：即使工作协程被强制 abort 或在用户 Future 之外异常退出，也能确保状态与槽位计数被正确回收。
struct TaskCompletion {
    tracking: Arc<TaskTracking>,
    task_id: Option<String>,
}

impl TaskCompletion {
    fn finish(mut self, outcome: TaskOutcome) {
        if let Some(task_id) = self.task_id.take() {
            self.tracking.finish(&task_id, outcome);
        }
    }
}

impl Drop for TaskCompletion {
    fn drop(&mut self) {
        if let Some(task_id) = self.task_id.take() {
            self.tracking
                .finish(&task_id, TaskOutcome::failed("任务执行协程异常退出"));
        }
    }
}

impl TaskExecutor {
    pub fn new(max_concurrency: usize) -> Self {
        Self::with_capacity(max_concurrency, DEFAULT_CAPACITY)
    }

    /// `capacity` 为排队中与运行中任务的总容量上限；传入 0 时自动保底为 1（与并发度参数处理一致）。
    pub fn with_capacity(max_concurrency: usize, capacity: usize) -> Self {
        let max_concurrency = max_concurrency.max(1);
        let shared = Arc::new(TaskTracking {
            state: SyncMutex::new(TaskState::default()),
            wake: Notify::new(),
            capacity: capacity.max(1),
        });
        let (completed, shutdown_complete_rx) = watch::channel(false);
        let tracking = shared.clone();
        tokio::spawn(async move {
            Self::task_loop(tracking, max_concurrency).await;
            completed.send_replace(true);
        });
        Self {
            shared,
            max_concurrency,
            shutdown_complete_rx,
        }
    }

    async fn task_loop(tracking: Arc<TaskTracking>, max_concurrency: usize) {
        let mut running = JoinSet::new();
        loop {
            let (ready, drained) = {
                let mut state = tracking.lock();
                let mut ready = Vec::new();
                while running.len() + ready.len() < max_concurrency {
                    let Some(((_, task_id), pending)) = state.pending.pop_first() else {
                        break;
                    };
                    let task = state
                        .active
                        .get_mut(&task_id)
                        .expect("queued task must be tracked");
                    task.metadata.status = TaskStatus::Running;
                    task.metadata.started_at = Some(Instant::now());
                    ready.push((task_id, pending, task.metadata.clone()));
                }
                (ready, state.closing && state.active.is_empty())
            };
            for (task_id, pending, metadata) in ready {
                let completion = TaskCompletion {
                    tracking: tracking.clone(),
                    task_id: Some(task_id),
                };
                running.spawn(Self::execute_task(pending, metadata, completion));
            }
            if drained && running.is_empty() {
                break;
            }
            tokio::select! {
                _ = tracking.wake.notified() => {},
                completed = running.join_next(), if !running.is_empty() => {
                    if let Some(Err(error)) = completed {
                        warn!(?error, "任务执行协程异常退出");
                    }
                }
            }
        }
    }

    async fn execute_task(
        pending: PendingTask,
        metadata: TaskMetadata,
        completion: TaskCompletion,
    ) {
        info!(task_id=%metadata.task_id, task_name=%metadata.name,
            plugin=%metadata.plugin_name, "任务开始执行");
        metrics::counter!("nuono_tasks_started_total", "plugin" => metadata.plugin_name.clone())
            .increment(1);
        // 同步闭包调用与异步 Future 轮询均包裹在 catch_unwind 中；
        // 且在发布终态结果和释放并发槽位之前，先完整析构任务 Future。
        let outcome = {
            let PendingTask {
                func,
                mut cancelled,
            } = pending;
            let work = async move {
                let result = if let Some(timeout) = metadata.timeout {
                    match tokio::time::timeout(timeout, (func)()).await {
                        Ok(result) => result,
                        Err(_) => {
                            return TaskOutcome {
                                status: TaskStatus::TimedOut,
                                output: None,
                                error: Some("任务执行超时".into()),
                            };
                        }
                    }
                } else {
                    (func)().await
                };
                match result {
                    Ok(output) => TaskOutcome {
                        status: TaskStatus::Completed,
                        output: Some(output),
                        error: None,
                    },
                    Err(error) => TaskOutcome {
                        status: TaskStatus::Failed,
                        output: None,
                        error: Some(error.to_string()),
                    },
                }
            };
            AssertUnwindSafe(async {
                tokio::select! {
                    biased;
                    _ = cancelled.wait_for(|cancelled| *cancelled) => TaskOutcome::cancelled(),
                    outcome = work => outcome,
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| TaskOutcome::failed("任务执行时发生 panic"))
        };
        completion.finish(outcome);
    }

    fn admit(
        &self,
        mut metadata: TaskMetadata,
        func: TaskFn,
        result_tx: Option<oneshot::Sender<TaskResult>>,
    ) -> Result<(String, watch::Sender<bool>)> {
        let mut state = self.shared.lock();
        if state.closing {
            metrics::counter!("nuono_task_executor_rejected_total", "reason" => "closed")
                .increment(1);
            return Err(NuonoError::Conflict("任务执行器已关闭".into()));
        }
        if state.active.contains_key(&metadata.task_id) {
            metrics::counter!("nuono_task_executor_rejected_total", "reason" => "duplicate")
                .increment(1);
            return Err(NuonoError::Conflict("相同任务 ID 已在执行器中".into()));
        }
        if state.active.len() >= self.shared.capacity {
            metrics::counter!("nuono_task_executor_rejected_total", "reason" => "full")
                .increment(1);
            return Err(NuonoError::Conflict(format!(
                "任务执行器已达到 {} 项未结束任务上限",
                self.shared.capacity
            )));
        }
        // 调用方可能会复用旧任务的元数据克隆；每次新任务准入时都重置为全新的生命周期状态。
        metadata.status = TaskStatus::Pending;
        metadata.started_at = None;
        metadata.completed_at = None;
        metadata.error = None;
        let task_id = metadata.task_id.clone();
        let (cancel, cancelled) = watch::channel(false);
        state.pending.insert(
            (metadata.priority, task_id.clone()),
            PendingTask { func, cancelled },
        );
        state.active.insert(
            task_id.clone(),
            ActiveTask {
                metadata,
                cancel: cancel.clone(),
                result_tx,
            },
        );
        drop(state);
        self.shared.wake.notify_one();
        Ok((task_id, cancel))
    }

    /// 提交后台独立运行的任务；若队列已满或执行器已关闭则立即返回错误。
    pub async fn submit(&self, metadata: TaskMetadata, func: TaskFn) -> Result<String> {
        self.admit(metadata, func, None).map(|(task_id, _)| task_id)
    }

    /// 提交任务并等待结果；若调用方中途丢弃该等待 Future，对应任务也会随之取消（需要任务脱离调用方独立运行请用 `submit`）。
    pub async fn submit_and_wait(
        &self,
        metadata: TaskMetadata,
        func: TaskFn,
    ) -> Result<TaskResult> {
        let (result_tx, result_rx) = oneshot::channel();
        let (task_id, token) = self.admit(metadata, func, Some(result_tx))?;
        let mut cancellation = CancelOnDrop {
            tracking: self.shared.clone(),
            task_id: Some(task_id),
            cancellation: token,
        };
        let result = result_rx
            .await
            .map_err(|_| NuonoError::Internal("任务结果通道已关闭".into()));
        cancellation.task_id = None;
        result
    }

    /// 取消指定任务：尚在排队的任务会在返回前直接移除，正在运行的任务会收到取消通知并异步中止。
    /// 返回 `false` 表示任务此前已完成或已被取消。
    pub async fn cancel(&self, task_id: &str) -> bool {
        self.shared.cancel(task_id)
    }

    /// 停止接收新任务并等待所有已准入任务执行完毕；即使当前等待调用被取消，执行器也不会重新开放。
    pub async fn shutdown(&self) {
        self.shared.close();
        let mut completed = self.shutdown_complete_rx.clone();
        while !*completed.borrow() {
            if completed.changed().await.is_err() {
                break;
            }
        }
    }

    pub async fn get_task_status(&self, task_id: &str) -> Option<TaskMetadata> {
        self.shared
            .lock()
            .active
            .get(task_id)
            .map(|task| task.metadata.clone())
    }

    pub async fn active_tasks(&self) -> Vec<TaskMetadata> {
        self.shared
            .lock()
            .active
            .values()
            .map(|task| task.metadata.clone())
            .collect()
    }

    pub async fn task_history(&self, limit: Option<usize>) -> Vec<TaskResult> {
        let state = self.shared.lock();
        state
            .history
            .iter()
            .rev()
            .take(limit.unwrap_or(state.history.len()))
            .cloned()
            .collect()
    }

    pub async fn running_count(&self) -> usize {
        self.shared
            .lock()
            .active
            .values()
            .filter(|task| task.metadata.status == TaskStatus::Running)
            .count()
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    /// 所有统计计数均从同一次锁内状态快照计算得出，避免分次读取导致计数错位或下溢。
    pub async fn stats(&self) -> TaskStats {
        let state = self.shared.lock();
        let running = state
            .active
            .values()
            .filter(|task| task.metadata.status == TaskStatus::Running)
            .count();
        let mut stats = TaskStats {
            pending: state.active.len() - running,
            running,
            total_history: state.history.len(),
            ..TaskStats::default()
        };
        for result in &state.history {
            match result.metadata.status {
                TaskStatus::Completed => stats.completed += 1,
                TaskStatus::Failed => stats.failed += 1,
                TaskStatus::TimedOut => stats.timed_out += 1,
                TaskStatus::Cancelled => stats.cancelled += 1,
                _ => {}
            }
        }
        stats
    }
}

impl Drop for TaskExecutor {
    fn drop(&mut self) {
        self.shared.cancel_all();
    }
}

/// 终态计数（已完成/失败/取消/超时）仅统计当前保留的最近历史记录（最多 1000 条），而非进程启动以来的累计总数。
#[derive(Debug, Clone, Default)]
pub struct TaskStats {
    pub pending: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub cancelled: usize,
    pub total_history: usize,
}

#[cfg(test)]
#[path = "../../tests/unit/core/task_executor/fault_tests.rs"]
mod fault_tests;
#[cfg(test)]
#[path = "../../tests/unit/core/task_executor/lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Barrier, Mutex};

    #[tokio::test]
    async fn test_task_executor_basic() {
        let executor = TaskExecutor::new(2);

        let metadata = TaskMetadata::new("test_task", "test_plugin", 5);
        let task_id = metadata.task_id.clone();

        let func: TaskFn = Box::new(|| Box::pin(async { Ok(json!({"result": "success"})) }));

        let result_id = executor.submit(metadata, func).await.unwrap();
        assert_eq!(result_id, task_id);

        executor.shutdown().await;

        let stats = executor.stats().await;
        assert_eq!(stats.completed, 1);
    }

    #[tokio::test]
    async fn cancelled_pending_entries_never_accumulate_in_the_priority_queue() {
        let executor = TaskExecutor::with_capacity(1, 2);
        let (started, running) = oneshot::channel();
        let blocker = executor
            .submit(
                TaskMetadata::new("blocker", "test", 0),
                Box::new(move || {
                    Box::pin(async move {
                        let _ = started.send(());
                        std::future::pending().await
                    })
                }),
            )
            .await
            .unwrap();
        running.await.unwrap();
        for _ in 0..2048 {
            let id = executor
                .submit(
                    TaskMetadata::new("queued", "test", 1),
                    Box::new(|| {
                        Box::pin(async { panic!("cancelled pending work must not execute") })
                    }),
                )
                .await
                .unwrap();
            assert_eq!(executor.shared.lock().pending.len(), 1);
            assert!(executor.cancel(&id).await);
            let state = executor.shared.lock();
            assert!(
                state.pending.is_empty(),
                "cancel must physically remove its queue entry"
            );
            assert_eq!(state.active.len(), 1);
            assert!(state.history.len() <= MAX_HISTORY);
        }
        assert!(executor.cancel(&blocker).await);
        executor.shutdown().await;
        assert!(executor.shared.lock().active.is_empty());
    }

    #[tokio::test]
    async fn test_task_executor_priority() {
        let executor = TaskExecutor::new(1);
        let (blocker_tx, blocker_rx) = oneshot::channel();
        let execution_order = Arc::new(Mutex::new(Vec::new()));

        let blocker: TaskFn = Box::new(move || {
            Box::pin(async move {
                let _ = blocker_rx.await;
                Ok(json!({"task": "blocker"}))
            })
        });
        executor
            .submit(TaskMetadata::new("blocker", "test_plugin", 0), blocker)
            .await
            .unwrap();

        while executor.running_count().await == 0 {
            tokio::task::yield_now().await;
        }

        for priority in [5, 1, 3] {
            let execution_order = execution_order.clone();
            let func: TaskFn = Box::new(move || {
                Box::pin(async move {
                    execution_order.lock().await.push(priority);
                    Ok(json!({"priority": priority}))
                })
            });
            executor
                .submit(
                    TaskMetadata::new(format!("task_{priority}"), "test_plugin", priority),
                    func,
                )
                .await
                .unwrap();
        }

        let _ = blocker_tx.send(());
        executor.shutdown().await;

        assert_eq!(*execution_order.lock().await, vec![1, 3, 5]);
        assert_eq!(executor.stats().await.completed, 4);
    }

    #[tokio::test]
    async fn test_task_executor_runs_tasks_concurrently() {
        let executor = TaskExecutor::new(2);
        let barrier = Arc::new(Barrier::new(3));

        for i in 0..2 {
            let barrier = barrier.clone();
            let func: TaskFn = Box::new(move || {
                Box::pin(async move {
                    barrier.wait().await;
                    Ok(json!({"task": i}))
                })
            });
            executor
                .submit(
                    TaskMetadata::new(format!("task_{i}"), "test_plugin", 0),
                    func,
                )
                .await
                .unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), barrier.wait())
            .await
            .expect("两个任务应当同时开始执行");

        executor.shutdown().await;
        assert_eq!(executor.stats().await.completed, 2);
    }

    #[tokio::test]
    async fn test_task_executor_respects_max_concurrency() {
        let max_concurrency = 2;
        let executor = TaskExecutor::new(max_concurrency);
        let current = Arc::new(AtomicUsize::new(0));
        let observed_max = Arc::new(AtomicUsize::new(0));

        for i in 0..6 {
            let current = current.clone();
            let observed_max = observed_max.clone();
            let func: TaskFn = Box::new(move || {
                Box::pin(async move {
                    let running = current.fetch_add(1, Ordering::SeqCst) + 1;
                    observed_max.fetch_max(running, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    Ok(json!({"task": i}))
                })
            });
            executor
                .submit(
                    TaskMetadata::new(format!("task_{i}"), "test_plugin", 0),
                    func,
                )
                .await
                .unwrap();
        }

        executor.shutdown().await;

        assert_eq!(executor.stats().await.completed, 6);
        assert_eq!(observed_max.load(Ordering::SeqCst), max_concurrency);
    }
}
