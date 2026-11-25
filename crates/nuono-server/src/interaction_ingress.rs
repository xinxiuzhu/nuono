//! 聊天平台入站事件的并发控制与分发（交互回调 ACK 确认与挂起会话等待唤醒直接放行，不占用常规业务并发槽位）。
use async_trait::async_trait;
use nuono_app::platform::UnifiedEvent;
use nuono_core::{
    core::{EventKernel, RuntimeActionSink, RuntimeBotRoute},
    error::{NuonoError, Result},
};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const MAX_IN_FLIGHT: usize = 32;
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct RuntimeIngress {
    inner: Arc<IngressTasks>,
}

#[cfg(test)]
pub(crate) struct IngressGuard(RuntimeIngress);

#[cfg(test)]
impl Drop for IngressGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct IngressTasks {
    slots: Arc<Semaphore>,
    tracker: TaskTracker,
    cancelled: CancellationToken,
    registration: Mutex<()>,
}

impl Drop for IngressTasks {
    fn drop(&mut self) {
        // 入站消费者停止或销毁时立即触发取消令牌，防止后台已 spawn 的插件事件处理任务继续孤立运行。
        self.cancelled.cancel();
        self.slots.close();
        self.tracker.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressOutcome {
    Accepted,
    WaitingDelivered,
}

#[async_trait]
trait Dispatch: Send + Sync {
    async fn deliver_waiting(&self, event: &UnifiedEvent) -> Result<bool>;
    async fn process(&self, event: UnifiedEvent) -> Result<()>;
}

struct KernelDispatch {
    platform: String,
    kernel: Arc<EventKernel>,
    sink: Option<Arc<dyn RuntimeActionSink>>,
    route: Option<RuntimeBotRoute>,
}

#[async_trait]
impl Dispatch for KernelDispatch {
    async fn deliver_waiting(&self, event: &UnifiedEvent) -> Result<bool> {
        self.kernel
            .try_deliver_waiting_unified_event(
                &self.platform,
                event,
                self.sink.clone(),
                self.route.as_ref(),
            )
            .await
    }

    async fn process(&self, event: UnifiedEvent) -> Result<()> {
        self.kernel
            .process_unified_event_with_route(
                &self.platform,
                event,
                self.sink.clone(),
                self.route.as_ref(),
            )
            .await
            .map(|_| ())
    }
}

impl RuntimeIngress {
    /// 在监听到停止信号或取消令牌时立即中止消费者 Future，以便外层调用方等待底层平台事件生产端退出。
    #[cfg(test)]
    pub(crate) async fn run_until_stopped(
        &self,
        mut stop: tokio::sync::watch::Receiver<bool>,
        work: impl std::future::Future<Output = ()>,
    ) {
        tokio::select! {
            biased;
            _ = self.inner.cancelled.cancelled() => {},
            _ = async {
                while !*stop.borrow_and_update() {
                    if stop.changed().await.is_err() {
                        break;
                    }
                }
            } => {},
            _ = work => {},
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(IngressTasks {
                slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
                tracker: TaskTracker::new(),
                cancelled: CancellationToken::new(),
                registration: Mutex::new(()),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn worker_guard(&self) -> IngressGuard {
        IngressGuard(self.clone())
    }

    pub(crate) async fn submit(
        &self,
        platform: &str,
        kernel: Arc<EventKernel>,
        event: UnifiedEvent,
        sink: Option<Arc<dyn RuntimeActionSink>>,
        route: Option<RuntimeBotRoute>,
    ) -> Result<IngressOutcome> {
        let dispatch = Arc::new(KernelDispatch {
            platform: platform.into(),
            kernel,
            sink: sink.clone(),
            route,
        });
        self.submit_with(platform, dispatch, event, sink).await
    }

    async fn submit_with(
        &self,
        platform: &str,
        dispatch: Arc<dyn Dispatch>,
        event: UnifiedEvent,
        sink: Option<Arc<dyn RuntimeActionSink>>,
    ) -> Result<IngressOutcome> {
        if self.inner.cancelled.is_cancelled() {
            return Err(NuonoError::Conflict("平台入站处理已关闭".into()));
        }
        // `interaction_context` 来自已认证的本地平台适配器；日志中严禁打印该字段，以免泄露其中可能包含的平台回调凭据。
        if let UnifiedEvent::Notice(notice) = &event
            && notice.notice_type == "interaction"
            && let Some(sink) = sink
        {
            let acknowledgement = serde_json::json!({"action":"ack_interaction","params":{
                "interaction_id":notice.raw["interaction_id"],
                "interaction_context":notice.raw.get("interaction_context").cloned().unwrap_or_default()
            }});
            let result = tokio::select! {
                biased;
                _ = self.inner.cancelled.cancelled() => return Err(NuonoError::Conflict("平台入站处理已关闭".into())),
                result = tokio::time::timeout(ACK_TIMEOUT, sink.call_action(acknowledgement)) => result,
            };
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(platform, kind = error.kind(), "交互确认失败"),
                Err(_) => tracing::warn!(platform, "交互确认超时"),
            }
        }
        let delivered = tokio::select! {
            biased;
            _ = self.inner.cancelled.cancelled() => return Err(NuonoError::Conflict("平台入站处理已关闭".into())),
            result = dispatch.deliver_waiting(&event) => result?,
        };
        if delivered {
            return Ok(IngressOutcome::WaitingDelivered);
        }
        let permit = self
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| NuonoError::Conflict("平台入站并发已达上限，请稍后重试".into()))?;
        // 用同一把互斥锁串行化任务 spawn 与停机关闭，确保停机等待不会漏掉刚派发的任务。
        let _registration = self.inner.registration.lock().unwrap();
        if self.inner.cancelled.is_cancelled() {
            return Err(NuonoError::Conflict("平台入站处理已关闭".into()));
        }
        let cancelled = self.inner.cancelled.clone();
        let platform = platform.to_owned();
        self.inner.tracker.spawn(async move {
            let _permit = permit;
            tokio::select! {
                biased;
                _ = cancelled.cancelled() => {},
                result = dispatch.process(event) => {
                    if let Err(error) = result {
                        tracing::warn!(platform, error = %error, "平台事件处理失败");
                    }
                }
            }
        });
        Ok(IngressOutcome::Accepted)
    }

    pub(crate) fn cancel(&self) {
        let _registration = self.inner.registration.lock().unwrap();
        self.inner.cancelled.cancel();
        self.inner.slots.close();
        self.inner.tracker.close();
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel();
        self.inner.tracker.wait().await;
    }
}

#[cfg(test)]
#[path = "../tests/unit/interaction_ingress/tests.rs"]
mod tests;
