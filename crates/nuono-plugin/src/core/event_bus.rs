use async_trait::async_trait;
use chrono::{DateTime, Local};
use dashmap::DashMap;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventType {
    MessageReceived,
    MessageSent,
    BotConnected,
    BotDisconnected,
    CommandTriggered,
    PluginEvent(String),
    LogAdded,
    // 配置变更事件
    PluginEnabledChanged,
    ServiceEnabledChanged,
    PluginConfigChanged,
    GlobalConfigChanged,
    // 审计事件
    AuditEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub timestamp: DateTime<Local>,
    pub event_type: EventType,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub bot_id: i64,
    pub data: Value,
    pub metadata: HashMap<String, String>,
}

impl Event {
    pub fn new(event_type: EventType, bot_id: i64, data: Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Local::now(),
            event_type,
            bot_id,
            data,
            metadata: HashMap::new(),
        }
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum EventResult {
    Continue,
    Stop,
}

#[async_trait]
pub trait EventHandler: Send + Sync {
    async fn handle(&self, event: &Event) -> anyhow::Result<EventResult>;
    fn priority(&self) -> i32 {
        0
    }
    fn event_types(&self) -> Vec<EventType>;
}

#[derive(Debug)]
struct HandlerFailure {
    reason: &'static str,
    message: String,
}

impl HandlerFailure {
    fn error(reason: &'static str, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            reason,
            message: message.into(),
        })
    }
}

impl std::fmt::Display for HandlerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HandlerFailure {}

async fn call_handler(handler: &dyn EventHandler, event: &Event) -> anyhow::Result<EventResult> {
    // 捕获插件事件处理 Future 在构造和轮询阶段的 panic，
    // 确保单个订阅者崩溃不会中断发布者或阻碍其余订阅者继续执行。
    std::panic::AssertUnwindSafe(async { handler.handle(event).await })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(HandlerFailure::error("panic", "事件处理器发生 panic")))
}

struct ScopedHandler {
    handler: Arc<dyn EventHandler>,
    scope: std::sync::Weak<crate::plugin::PluginTaskScope>,
    plugin: String,
}

#[async_trait]
impl EventHandler for ScopedHandler {
    async fn handle(&self, event: &Event) -> anyhow::Result<EventResult> {
        let scope = self.scope.upgrade().ok_or_else(|| {
            HandlerFailure::error(
                "scope_released",
                format!("插件 {} 的任务作用域已释放", self.plugin),
            )
        })?;
        if scope.is_closed() {
            return Err(HandlerFailure::error(
                "scope_closed",
                format!("插件 {} 已停用，事件处理器未执行", self.plugin),
            ));
        }
        // 准备阶段的事件不补发，也不能让发布者等待尚未开放的业务任务。
        if !scope.business_is_running() {
            return Ok(EventResult::Continue);
        }
        let (mut sender, receiver) = tokio::sync::oneshot::channel();
        let handler = self.handler.clone();
        let event = event.clone();
        // 将实际的事件处理任务纳入插件的 `PluginTaskScope` 管理，而非仅做静态启用状态检查；
        // 这样即便当前包装句柄来自旧的派发快照，插件停用关闭（`shutdown`）时也能中止并等待其退出。
        let admitted = scope.try_spawn("event_bus_handler", async move {
            let result = tokio::select! {
                biased;
                _ = sender.closed() => return,
                result = call_handler(handler.as_ref(), &event) => result,
            };
            let _ = sender.send(result);
        });
        let closed = scope.is_closed();
        // 订阅者与发布者都不应长期持有 `Arc<PluginTaskScope>` 强引用；
        // 尤其要避免被遗忘的挂起发布任务形成“作用域 -> 子任务 -> 作用域”的引用环。
        drop(scope);
        if !admitted {
            return Err(HandlerFailure::error(
                if closed {
                    "scope_closed"
                } else {
                    "scope_rejected"
                },
                format!(
                    "插件 {} 的事件处理器未获准启动：作用域已停用、任务容量已满或宿主 runtime 不可用",
                    self.plugin
                ),
            ));
        }
        receiver.await.map_err(|_| {
            HandlerFailure::error(
                "cancelled",
                format!("插件 {} 的事件处理器已取消", self.plugin),
            )
        })?
    }

    fn priority(&self) -> i32 {
        self.handler.priority()
    }

    fn event_types(&self) -> Vec<EventType> {
        self.handler.event_types()
    }
}

pub struct EventBus {
    scope: Option<(
        Arc<EventBus>,
        std::sync::Weak<crate::plugin::PluginTaskScope>,
    )>,
    subscribers: DashMap<EventType, Vec<Arc<dyn EventHandler>>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscribers", &self.subscribers.len())
            .finish()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscribers: DashMap::new(),
            scope: None,
        }
    }

    #[doc(hidden)]
    pub fn for_plugin(parent: Arc<Self>, tasks: &Arc<crate::plugin::PluginTaskScope>) -> Self {
        Self {
            subscribers: DashMap::new(),
            scope: Some((parent, Arc::downgrade(tasks))),
        }
    }

    pub fn subscribe(&self, handler: Arc<dyn EventHandler>) {
        if let Some((parent, scope)) = &self.scope {
            if let Some(scope) = scope.upgrade() {
                let handler: Arc<dyn EventHandler> = Arc::new(ScopedHandler {
                    handler,
                    scope: Arc::downgrade(&scope),
                    plugin: scope.plugin_name().to_owned(),
                });
                parent.subscribe(handler.clone());
                let bus = Arc::downgrade(parent);
                let handler = Arc::downgrade(&handler);
                scope.on_shutdown(move || {
                    if let (Some(bus), Some(handler)) = (bus.upgrade(), handler.upgrade()) {
                        for mut subscribers in bus.subscribers.iter_mut() {
                            subscribers.retain(|candidate| !Arc::ptr_eq(candidate, &handler));
                        }
                    }
                });
            }
            return;
        }
        for event_type in handler.event_types() {
            let mut handlers = self.subscribers.entry(event_type).or_default();
            handlers.push(handler.clone());
            // 按优先级排序：优先级高的在前
            handlers.sort_by_key(|handler| std::cmp::Reverse(handler.priority()));
        }
    }

    pub fn subscriber_count(&self, event_type: EventType) -> usize {
        if let Some((parent, _)) = &self.scope {
            return parent.subscriber_count(event_type);
        }
        self.subscribers
            .get(&event_type)
            .map(|h| h.len())
            .unwrap_or(0)
    }

    pub async fn publish(&self, event: Event) {
        if let Some((_, scope)) = &self.scope
            && scope
                .upgrade()
                .is_none_or(|scope| !scope.business_is_running())
        {
            return;
        }
        let bus = self
            .scope
            .as_ref()
            .map_or(self, |(parent, _)| parent.as_ref());
        bus.dispatch(event).await;
    }

    async fn dispatch(&self, event: Event) {
        // 先克隆 handler 列表并释放分片锁，避免 handler 内部调用 subscribe()
        // 写同一分片时死锁（不能持有 DashMap guard 跨 await）。
        let handlers: Vec<Arc<dyn EventHandler>> = match self.subscribers.get(&event.event_type) {
            Some(handlers) if !handlers.is_empty() => handlers.clone(),
            _ => return,
        };
        tracing::debug!(
            "EventBus publish: type={:?}, handlers={}, event_id={}",
            event.event_type,
            handlers.len(),
            event.id
        );
        for handler in handlers.iter() {
            match call_handler(handler.as_ref(), &event).await {
                Ok(EventResult::Stop) => break,
                Ok(EventResult::Continue) => continue,
                Err(error) => {
                    let reason = error
                        .downcast_ref::<HandlerFailure>()
                        .map_or("handler_error", |failure| failure.reason);
                    metrics::counter!("nuono_event_bus_handler_failures_total", "reason" => reason)
                        .increment(1);
                    tracing::error!(event_id = %event.id, reason, error = %error, "事件订阅处理失败，继续后续订阅");
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/core/event_bus/fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Count(Arc<AtomicUsize>);
    #[async_trait]
    impl EventHandler for Count {
        async fn handle(&self, _: &Event) -> anyhow::Result<EventResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(EventResult::Continue)
        }
        fn event_types(&self) -> Vec<EventType> {
            vec![EventType::BotConnected]
        }
    }

    #[tokio::test]
    async fn prepared_subscribers_do_not_run_or_block_publishers() {
        let parent = Arc::new(EventBus::new());
        let scope = Arc::new(
            crate::PluginTaskScope::new("prepared-events")
                .with_business_gate(crate::BusinessGate::paused()),
        );
        let bus = EventBus::for_plugin(parent.clone(), &scope);
        let calls = Arc::new(AtomicUsize::new(0));
        bus.subscribe(Arc::new(Count(calls.clone())));
        let event = Event::new(EventType::BotConnected, 1, serde_json::json!({}));
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            parent.publish(event.clone()),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        scope.open_business();
        parent.publish(event).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        scope.shutdown().await;
    }
}
