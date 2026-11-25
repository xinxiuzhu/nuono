use super::*;
use crate::plugin::PluginTaskScope;
use std::{
    sync::{
        Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

fn event() -> Event {
    Event::new(EventType::MessageReceived, 1, serde_json::json!({}))
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("event bus operation did not complete")
}

struct Counter {
    calls: Arc<AtomicUsize>,
    priority: i32,
}

impl Counter {
    fn handler(calls: &Arc<AtomicUsize>, priority: i32) -> Arc<dyn EventHandler> {
        Arc::new(Self {
            calls: calls.clone(),
            priority,
        })
    }
}

#[async_trait]
impl EventHandler for Counter {
    async fn handle(&self, _: &Event) -> anyhow::Result<EventResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(EventResult::Continue)
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::MessageReceived]
    }
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct BlockingHandler {
    started: Notify,
    release: Semaphore,
    dropped: Arc<AtomicUsize>,
}

impl BlockingHandler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            release: Semaphore::new(0),
            dropped: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl EventHandler for BlockingHandler {
    async fn handle(&self, _: &Event) -> anyhow::Result<EventResult> {
        let _resource = DropCounter(self.dropped.clone());
        self.started.notify_one();
        self.release.acquire().await.unwrap().forget();
        Ok(EventResult::Continue)
    }

    fn priority(&self) -> i32 {
        100
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::MessageReceived]
    }
}

#[tokio::test]
async fn shutdown_fences_a_handler_already_cloned_into_a_dispatch_snapshot() {
    let bus = Arc::new(EventBus::new());
    let first = BlockingHandler::new();
    bus.subscribe(first.clone());
    let scope = Arc::new(PluginTaskScope::new("old-instance"));
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let old_calls = Arc::new(AtomicUsize::new(0));
    scoped_bus.subscribe(Counter::handler(&old_calls, 0));
    let later_calls = Arc::new(AtomicUsize::new(0));
    bus.subscribe(Counter::handler(&later_calls, -1));
    let publisher = tokio::spawn({
        let bus = bus.clone();
        async move { bus.publish(event()).await }
    });
    within(first.started.notified()).await;
    within(scope.shutdown()).await;
    assert_eq!(bus.subscriber_count(EventType::MessageReceived), 2);
    first.release.add_permits(1);
    within(publisher).await.unwrap();
    assert_eq!(old_calls.load(Ordering::SeqCst), 0);
    assert_eq!(later_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_cancels_an_active_handler_and_releases_its_resources() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("pending-handler"));
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let pending = BlockingHandler::new();
    scoped_bus.subscribe(pending.clone());
    let later_calls = Arc::new(AtomicUsize::new(0));
    bus.subscribe(Counter::handler(&later_calls, -1));
    let publisher = tokio::spawn({
        let bus = bus.clone();
        async move { bus.publish(event()).await }
    });
    within(pending.started.notified()).await;
    within(scope.shutdown()).await;
    assert_eq!(pending.dropped.load(Ordering::SeqCst), 1);
    within(publisher).await.unwrap();
    assert_eq!(later_calls.load(Ordering::SeqCst), 1);
    assert_eq!(bus.subscriber_count(EventType::MessageReceived), 1);
}

#[tokio::test]
async fn cancelling_the_publisher_cleans_up_the_managed_handler_and_returns_capacity() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("cancelled-publisher"));
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let pending = BlockingHandler::new();
    scoped_bus.subscribe(pending.clone());
    let publisher = tokio::spawn({
        let bus = bus.clone();
        async move { bus.publish(event()).await }
    });
    within(pending.started.notified()).await;
    publisher.abort();
    assert!(within(publisher).await.unwrap_err().is_cancelled());
    within(async {
        while pending.dropped.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    for _ in 0..256 {
        assert!(scope.try_spawn("capacity-returned", std::future::pending()));
    }
    assert!(!scope.try_spawn("full", std::future::pending()));
    within(scope.shutdown()).await;
}

enum Outcome {
    Continue,
    Stop,
    Panic,
    Error,
}

struct OrderedHandler {
    name: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
    priority: i32,
    outcome: Outcome,
}

#[async_trait]
impl EventHandler for OrderedHandler {
    async fn handle(&self, _: &Event) -> anyhow::Result<EventResult> {
        self.order.lock().unwrap().push(self.name);
        match self.outcome {
            Outcome::Continue => Ok(EventResult::Continue),
            Outcome::Stop => Ok(EventResult::Stop),
            Outcome::Panic => panic!("synthetic event handler panic"),
            Outcome::Error => anyhow::bail!("synthetic event handler error"),
        }
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::MessageReceived]
    }
}

#[tokio::test]
async fn panics_and_errors_do_not_escape_or_skip_subscribers_and_stop_keeps_its_order() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("panic-handler"));
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let order = Arc::new(Mutex::new(Vec::new()));
    for (name, priority, outcome, scoped) in [
        ("after-stop", -1, Outcome::Continue, false),
        ("global-panic", 5, Outcome::Panic, false),
        ("scoped-panic", 4, Outcome::Panic, true),
        ("error", 3, Outcome::Error, true),
        ("continue", 2, Outcome::Continue, true),
        ("stop", 1, Outcome::Stop, true),
    ] {
        let handler = Arc::new(OrderedHandler {
            name,
            order: order.clone(),
            priority,
            outcome,
        });
        if scoped {
            scoped_bus.subscribe(handler);
        } else {
            bus.subscribe(handler);
        }
    }
    within(bus.publish(event())).await;
    assert_eq!(
        *order.lock().unwrap(),
        ["global-panic", "scoped-panic", "error", "continue", "stop"]
    );
    within(scope.shutdown()).await;
}

#[tokio::test]
async fn full_scope_rejects_the_handler_without_blocking_other_subscribers() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("full-handler"));
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let scoped_calls = Arc::new(AtomicUsize::new(0));
    scoped_bus.subscribe(Counter::handler(&scoped_calls, 0));
    let later_calls = Arc::new(AtomicUsize::new(0));
    bus.subscribe(Counter::handler(&later_calls, -1));
    for _ in 0..256 {
        assert!(scope.try_spawn("occupy", std::future::pending()));
    }
    within(bus.publish(event())).await;
    assert_eq!(scoped_calls.load(Ordering::SeqCst), 0);
    assert_eq!(later_calls.load(Ordering::SeqCst), 1);
    within(scope.shutdown()).await;
}

#[tokio::test]
async fn subscriptions_do_not_keep_the_scope_alive_or_survive_its_drop() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("released-handler"));
    let weak = Arc::downgrade(&scope);
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let calls = Arc::new(AtomicUsize::new(0));
    scoped_bus.subscribe(Counter::handler(&calls, 0));
    assert_eq!(bus.subscriber_count(EventType::MessageReceived), 1);
    drop(scope);
    assert!(weak.upgrade().is_none());
    assert_eq!(bus.subscriber_count(EventType::MessageReceived), 0);
    scoped_bus.subscribe(Counter::handler(&calls, 0));
    within(bus.publish(event())).await;
    assert_eq!(bus.subscriber_count(EventType::MessageReceived), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_pending_publisher_does_not_retain_the_scope() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("released-pending-handler"));
    let weak = Arc::downgrade(&scope);
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let pending = BlockingHandler::new();
    scoped_bus.subscribe(pending.clone());
    let publisher = tokio::spawn({
        let bus = bus.clone();
        async move { bus.publish(event()).await }
    });
    within(pending.started.notified()).await;
    drop(scope);
    assert!(weak.upgrade().is_none());
    within(publisher).await.unwrap();
    assert_eq!(pending.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(bus.subscriber_count(EventType::MessageReceived), 0);
}

struct SelfShutdown {
    scope: Weak<PluginTaskScope>,
    after_shutdown: Arc<AtomicUsize>,
}

#[async_trait]
impl EventHandler for SelfShutdown {
    async fn handle(&self, _: &Event) -> anyhow::Result<EventResult> {
        self.scope.upgrade().unwrap().shutdown().await;
        self.after_shutdown.fetch_add(1, Ordering::SeqCst);
        Ok(EventResult::Continue)
    }

    fn event_types(&self) -> Vec<EventType> {
        vec![EventType::MessageReceived]
    }
}

#[tokio::test]
async fn handler_shutdown_cancels_itself_without_a_self_join_deadlock() {
    let bus = Arc::new(EventBus::new());
    let scope = Arc::new(PluginTaskScope::new("self-shutdown"));
    let scoped_bus = EventBus::for_plugin(bus.clone(), &scope);
    let after_shutdown = Arc::new(AtomicUsize::new(0));
    scoped_bus.subscribe(Arc::new(SelfShutdown {
        scope: Arc::downgrade(&scope),
        after_shutdown: after_shutdown.clone(),
    }));
    let later_calls = Arc::new(AtomicUsize::new(0));
    bus.subscribe(Counter::handler(&later_calls, -1));
    within(bus.publish(event())).await;
    within(scope.shutdown()).await;
    assert_eq!(after_shutdown.load(Ordering::SeqCst), 0);
    assert_eq!(later_calls.load(Ordering::SeqCst), 1);
}
