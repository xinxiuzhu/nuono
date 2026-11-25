//! Agent 任务执行所有权管理，以及带重试上限、可观测的中断清理机制。
use super::{
    run::{AgentRunReply, AgentRunTask},
    runtime::AgentRuntime,
};
use nuono_error::{NuonoError, Result};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::{Notify, oneshot, watch};

struct CleanupFailure {
    message: String,
    execution_owner: Option<String>,
}

#[derive(Default)]
struct State {
    closing: bool,
    next: u64,
    operations: usize,
    jobs: HashMap<u64, Option<tokio::task::JoinHandle<()>>>,
    failures: HashMap<String, CleanupFailure>,
}
pub(super) struct AgentLifecycle {
    state: Mutex<State>,
    changed: Notify,
    stop: watch::Sender<bool>,
}
impl Default for AgentLifecycle {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            stop: watch::channel(false).0,
        }
    }
}
struct Operation(Arc<AgentLifecycle>);
impl Drop for Operation {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .operations -= 1;
        self.0.changed.notify_waiters();
    }
}
struct Job {
    owner: Arc<AgentLifecycle>,
    id: u64,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.owner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .remove(&self.id);
        self.owner.changed.notify_waiters();
    }
}
impl AgentLifecycle {
    pub(super) fn ensure_open(&self) -> Result<()> {
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).closing {
            Err(NuonoError::Conflict("Agent 运行时正在关闭".into()))
        } else {
            Ok(())
        }
    }
    pub(super) async fn operation<T>(
        self: &Arc<Self>,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let mut stop = self.stop.subscribe();
        let _operation = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closing {
                return Err(NuonoError::Conflict("Agent 运行时正在关闭".into()));
            }
            state.operations += 1;
            Operation(self.clone())
        };
        tokio::select! {
            biased;
            _ = stop.changed() => Err(NuonoError::Conflict("Agent 执行因运行时关闭而中断".into())),
            result = CatchUnwind::new(future) => result.unwrap_or_else(|_| Err(NuonoError::Internal("Agent 执行发生 panic，任务已中断".into()))),
        }
    }
    /// 启动受管后台任务（即使在运行时关闭期间仍允许派生中断清理任务）。
    /// 清理任务会继续持有原有的执行许可与会话占位，直到落盘完成，因此不会产生无限堆积的清理队列。
    pub(super) fn spawn(self: &Arc<Self>, future: impl Future<Output = ()> + Send + 'static) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.record_failure(
                "runtime-stopped".into(),
                "异步运行时已退出，遗留执行需在启动时核对".into(),
            );
            drop(future);
            return;
        };
        let id = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let id = state.next;
            state.next = state.next.wrapping_add(1);
            // 在 spawn 前先插入占位，但切勿在持有锁期间调用 spawn（避免 runtime 销毁或立即丢弃未轮询任务时触发 Job::drop 导致重入死锁）。
            state.jobs.insert(id, None);
            id
        };
        let owner = self.clone();
        let job = Job {
            owner: owner.clone(),
            id,
        };
        let handle = runtime.spawn(async move {
            let _job = job;
            if CatchUnwind::new(future).await.is_err() {
                owner.record_failure(format!("job:{id}"), "Agent 收尾任务发生 panic".into());
            }
        });
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = state.jobs.get_mut(&id) {
            *slot = Some(handle);
        }
        // 若任务在锁外已火速完成并由 Job::drop 移除，此处直接忽略已完成的句柄即可。
    }

    pub(super) fn record_failure(&self, id: String, message: String) {
        self.record_cleanup_failure(id, message, None);
    }
    pub(super) fn record_run_failure(&self, id: String, owner: String, message: String) {
        self.record_cleanup_failure(id, message, Some(owner));
    }
    fn record_cleanup_failure(&self, id: String, message: String, execution_owner: Option<String>) {
        tracing::error!(run_id = %id, %message, "Agent interruption cleanup failed");
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // 最多保留最近 64 条清理失败记录；更早或未知所有者的遗留任务需通过显式取消或启动时对账处理，不可盲目假定可直接中断。
        if state.failures.len() >= 64
            && !state.failures.contains_key(&id)
            && let Some(entry) = state.failures.keys().min().cloned()
        {
            state.failures.remove(&entry);
        }
        state.failures.insert(
            id,
            CleanupFailure {
                message,
                execution_owner,
            },
        );
    }
    pub(super) fn failed_owner(&self, id: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
            .get(id)
            .and_then(|failure| failure.execution_owner.clone())
    }
    pub(super) fn clear_failure(&self, id: &str) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
            .remove(id);
    }
    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closing = true;
        self.stop.send_replace(true);
    }
    pub(super) async fn wait(&self) -> Result<()> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.operations == 0 && state.jobs.is_empty() {
                    return if state.failures.is_empty() {
                        Ok(())
                    } else {
                        Err(NuonoError::Internal(format!(
                            "{} 个 Agent 任务中断状态未能持久化：{}；下次会话请求或启动时将重新核对",
                            state.failures.len(),
                            state
                                .failures
                                .values()
                                .next()
                                .map(|failure| failure.message.as_str())
                                .unwrap_or_default()
                        )))
                    };
                }
            }
            notified.await;
        }
    }
    pub(super) fn manage(self: &Arc<Self>, future: AgentRunTask) -> AgentRunTask {
        let (start, started) = oneshot::channel();
        let (cancel, mut cancelled) = watch::channel(false);
        let (reply, result) = oneshot::channel();
        let mut stop = self.stop.subscribe();
        let closed = *stop.borrow();
        self.spawn(async move {
            // 即便调用方尚未开始轮询返回的句柄，该内部 Future 也已持有会话锁与执行槽位，确保 drop 时能正常走完清理。
            let mut future = Some(future);
            let ready = if closed { false } else { tokio::select! {
                biased;
                _ = stop.changed() => false,
                _ = cancelled.changed() => false,
                start = started => start.is_ok(),
            }};
            let outcome = if ready {
                tokio::select! {
                    biased;
                    _ = stop.changed() => Err(NuonoError::Conflict("Agent 执行因运行时关闭而中断".into())),
                    _ = cancelled.changed() => Err(NuonoError::Conflict("Agent 执行等待者已取消".into())),
                    result = CatchUnwind::new(future.take().expect("owned task")) => result.unwrap_or_else(|_| Err(NuonoError::Internal("Agent 执行发生 panic，任务已中断".into()))),
                }
            } else { Err(NuonoError::Conflict("Agent 任务在开始执行前已中断".into())) };
            // 在通知调用方完成之前先释放内部 Future，确保 RunGuard::drop 已先注册后台清理任务。
            drop(future);
            let _ = reply.send(outcome);
        });
        Box::pin(ManagedReply {
            start: Some(start),
            result,
            cancel,
        })
    }
}
struct ManagedReply {
    start: Option<oneshot::Sender<()>>,
    result: oneshot::Receiver<Result<AgentRunReply>>,
    cancel: watch::Sender<bool>,
}
impl Future for ManagedReply {
    type Output = Result<AgentRunReply>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
        Pin::new(&mut self.result).poll(cx).map(|reply| {
            reply.unwrap_or_else(|_| Err(NuonoError::Internal("Agent 受管执行任务异常退出".into())))
        })
    }
}
impl Drop for ManagedReply {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

/// 在异步轮询边界捕获 panic，且不暴露 panic 载荷。
pub(super) struct CatchUnwind<F>(Pin<Box<F>>);
impl<F> CatchUnwind<F> {
    fn new(future: F) -> Self {
        Self(Box::pin(future))
    }
}
impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::result::Result<F::Output, ()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.as_mut().poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}
impl AgentRuntime {
    /// 等待所有进行中的 Agent 任务及后台中断清理任务全部结束。
    pub async fn wait_for_idle(&self) -> Result<()> {
        self.lifecycle.wait().await
    }
    /// 停止接收新任务，并向所有运行中及尚未轮询的任务广播中断信号。
    pub fn request_shutdown(&self) {
        self.lifecycle.close();
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.request_shutdown();
        self.lifecycle.wait().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn tracked_job_aborted_before_first_poll_releases_its_registration() {
        let lifecycle = Arc::new(AgentLifecycle::default());
        lifecycle.spawn(std::future::pending());
        let abort = lifecycle
            .state
            .lock()
            .unwrap()
            .jobs
            .values()
            .next()
            .unwrap()
            .as_ref()
            .unwrap()
            .abort_handle();
        abort.abort();
        tokio::time::timeout(std::time::Duration::from_secs(1), lifecycle.wait())
            .await
            .unwrap()
            .unwrap();
    }
}
