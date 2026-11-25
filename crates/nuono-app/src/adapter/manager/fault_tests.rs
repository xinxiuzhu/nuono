use super::*;
use crate::platform::{Action, UnifiedEvent, UnifiedSegment};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;

#[derive(Debug)]
struct TestAdapter;

#[async_trait]
impl PlatformAdapter for TestAdapter {
    fn platform_name(&self) -> &str {
        "test"
    }
    fn parse_incoming(&self, _: &str) -> Result<UnifiedEvent> {
        Err(NuonoError::Validation("test adapter has no codec".into()))
    }
    fn extract_self_id(&self, _: &str) -> Option<i64> {
        None
    }
    fn build_action(&self, _: &Action) -> Value {
        Value::Null
    }
    fn segments_to_protocol(&self, _: &[UnifiedSegment]) -> Value {
        Value::Null
    }
}

fn config(id: &str) -> AdapterConfig {
    AdapterConfig {
        id: id.into(),
        adapter_type: "test".into(),
        enabled: true,
        config: Value::Null,
    }
}

async fn registered(id: &str) -> Arc<AdapterManager> {
    let manager = Arc::new(AdapterManager::new());
    manager
        .register(config(id), Arc::new(TestAdapter))
        .await
        .unwrap();
    manager.start(id).await.unwrap();
    manager
}

struct DropSignal(Option<oneshot::Sender<()>>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn pending_task() -> (JoinHandle<()>, oneshot::Receiver<()>) {
    let (started, ready) = oneshot::channel();
    let (dropped, cleanup) = oneshot::channel();
    let guard = DropSignal(Some(dropped));
    let task = tokio::spawn(async move {
        let _guard = guard;
        let _ = started.send(());
        std::future::pending::<()>().await;
    });
    ready.await.unwrap();
    (task, cleanup)
}

struct GracefulTask {
    handle: JoinHandle<()>,
    stop: watch::Sender<bool>,
    stopping: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    dropped: oneshot::Receiver<()>,
}

fn graceful_task() -> GracefulTask {
    let (stop, mut stopped) = watch::channel(false);
    let (stopping, stopping_rx) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (dropped, cleanup) = oneshot::channel();
    let guard = DropSignal(Some(dropped));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        let _ = stopped.wait_for(|stopped| *stopped).await;
        let _ = stopping.send(());
        let _ = released.await;
    });
    GracefulTask {
        handle,
        stop,
        stopping: stopping_rx,
        release,
        dropped: cleanup,
    }
}

#[tokio::test]
async fn stop_without_signal_joins_task_drop_before_publishing_stopped() {
    let manager = registered("one").await;
    let (task, mut dropped) = pending_task().await;
    manager.attach_task("one", task, None).await;
    manager.stop("one").await.unwrap();
    assert!(dropped.try_recv().is_ok(), "task resources already dropped");
    assert_eq!(
        manager.get("one").await.unwrap().status,
        AdapterStatus::Stopped
    );
    assert!(
        manager
            .entry("one")
            .await
            .unwrap()
            .lifecycle
            .lock()
            .await
            .task
            .is_none()
    );
}

#[tokio::test]
async fn graceful_stop_waits_for_cleanup_and_cancelled_waiter_retains_same_handle() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release,
        mut dropped,
    } = graceful_task();
    let original = handle.id();
    manager.attach_task("one", handle, Some(stop)).await;
    let owner = manager.clone();
    let waiter = tokio::spawn(async move { owner.stop("one").await });
    stopping.await.unwrap();
    assert!(!waiter.is_finished());
    assert_eq!(
        manager.get("one").await.unwrap().status,
        AdapterStatus::Running
    );
    waiter.abort();
    let _ = waiter.await;
    let entry = manager.entry("one").await.unwrap();
    assert_eq!(
        original,
        entry
            .lifecycle
            .lock()
            .await
            .task
            .as_ref()
            .unwrap()
            .handle
            .id()
    );
    let owner = manager.clone();
    let retry = tokio::spawn(async move { owner.stop("one").await });
    tokio::task::yield_now().await;
    assert!(!retry.is_finished());
    assert!(dropped.try_recv().is_err());
    release.send(()).unwrap();
    retry.await.unwrap().unwrap();
    assert!(dropped.try_recv().is_ok());
    assert!(entry.lifecycle.lock().await.task.is_none());
}

#[tokio::test]
async fn closed_stop_receiver_still_allows_cleanup() {
    let manager = registered("one").await;
    let (stop, stopped) = watch::channel(false);
    let observed = stop.clone();
    let (cleanup_started, cleanup_running) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let (dropped, mut cleanup_dropped) = oneshot::channel();
    let guard = DropSignal(Some(dropped));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        drop(stopped);
        let _ = cleanup_started.send(());
        let _ = released.await;
    });
    cleanup_running.await.unwrap();
    manager.attach_task("one", handle, Some(stop)).await;
    let owner = manager.clone();
    let waiter = tokio::spawn(async move { owner.stop("one").await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !*observed.borrow() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop request recorded even without a receiver");
    assert!(!waiter.is_finished());
    assert!(cleanup_dropped.try_recv().is_err());
    release.send(()).unwrap();
    waiter.await.unwrap().unwrap();
    assert!(cleanup_dropped.try_recv().is_ok());
}

#[tokio::test]
async fn grace_deadline_survives_cancelled_waiter_and_expiry_aborts_uncooperative_task() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release: _release,
        mut dropped,
    } = graceful_task();
    manager.attach_task("one", handle, Some(stop)).await;
    let owner = manager.clone();
    let waiter = tokio::spawn(async move { owner.stop("one").await });
    stopping.await.unwrap();
    waiter.abort();
    let _ = waiter.await;
    let entry = manager.entry("one").await.unwrap();
    let mut lifecycle = entry.lifecycle.lock().await;
    let task = lifecycle.task.as_mut().unwrap();
    let initial = task.stop_deadline.unwrap();
    assert!(initial <= Instant::now() + GRACEFUL_STOP_TIMEOUT);
    // 直接将截止时间设为当前时刻以模拟超时，避免测试真实等待 5 秒。
    task.stop_deadline = Some(Instant::now());
    drop(lifecycle);
    tokio::time::timeout(Duration::from_secs(1), manager.stop("one"))
        .await
        .unwrap()
        .unwrap();
    assert!(dropped.try_recv().is_ok());
}

#[tokio::test]
async fn cancelled_remove_keeps_tombstone_until_cleanup_and_retry_removes_it() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release,
        mut dropped,
    } = graceful_task();
    manager.attach_task("one", handle, Some(stop)).await;
    let owner = manager.clone();
    let waiter = tokio::spawn(async move { owner.remove("one").await });
    stopping.await.unwrap();
    waiter.abort();
    let _ = waiter.await;
    assert!(
        manager
            .register(config("one"), Arc::new(TestAdapter))
            .await
            .is_err()
    );
    assert!(matches!(
        manager.start("one").await,
        Err(NuonoError::NotFound(_))
    ));
    assert!(manager.get("one").await.is_some());
    let owner = manager.clone();
    let retry = tokio::spawn(async move { owner.remove("one").await });
    tokio::task::yield_now().await;
    assert!(!retry.is_finished());
    release.send(()).unwrap();
    retry.await.unwrap().unwrap();
    assert!(dropped.try_recv().is_ok());
    assert!(manager.get("one").await.is_none());
    manager
        .register(config("one"), Arc::new(TestAdapter))
        .await
        .unwrap();
    manager.start("one").await.unwrap();
    assert_eq!(
        manager.get("one").await.unwrap().status,
        AdapterStatus::Running
    );
}

#[tokio::test]
async fn slow_cleanup_does_not_hold_registry_lock_or_block_other_adapters() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release,
        dropped: _dropped,
    } = graceful_task();
    manager.attach_task("one", handle, Some(stop)).await;
    let owner = manager.clone();
    let waiter = tokio::spawn(async move { owner.stop("one").await });
    stopping.await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        manager
            .register(config("two"), Arc::new(TestAdapter))
            .await
            .unwrap();
        manager.start("two").await.unwrap();
        assert_eq!(manager.get_all().await.len(), 2);
        manager.stop("two").await.unwrap();
        manager.remove("two").await.unwrap();
    })
    .await
    .expect("unrelated entries remain responsive during cleanup");
    assert!(!waiter.is_finished());
    release.send(()).unwrap();
    waiter.await.unwrap().unwrap();
}

#[tokio::test]
async fn replacement_waits_for_previous_cleanup_then_stop_cleans_current_task() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release,
        mut dropped,
    } = graceful_task();
    manager.attach_task("one", handle, Some(stop)).await;
    let (incoming, mut incoming_dropped) = pending_task().await;
    let owner = manager.clone();
    let replacing = tokio::spawn(async move { owner.attach_task("one", incoming, None).await });
    stopping.await.unwrap();
    assert!(!replacing.is_finished());
    assert!(incoming_dropped.try_recv().is_err());
    release.send(()).unwrap();
    replacing.await.unwrap();
    assert!(dropped.try_recv().is_ok());
    assert!(incoming_dropped.try_recv().is_err());
    manager.stop("one").await.unwrap();
    assert!(incoming_dropped.try_recv().is_ok());
}

#[tokio::test]
async fn cancelling_replacement_aborts_incoming_task_and_preserves_old_cleanup_barrier() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release,
        mut dropped,
    } = graceful_task();
    manager.attach_task("one", handle, Some(stop)).await;
    let (incoming, incoming_dropped) = pending_task().await;
    let owner = manager.clone();
    let replacing = tokio::spawn(async move { owner.attach_task("one", incoming, None).await });
    stopping.await.unwrap();
    replacing.abort();
    let _ = replacing.await;
    tokio::time::timeout(Duration::from_secs(1), incoming_dropped)
        .await
        .unwrap()
        .unwrap();
    assert!(dropped.try_recv().is_err());
    release.send(()).unwrap();
    manager.stop("one").await.unwrap();
    assert!(dropped.try_recv().is_ok());
}

#[tokio::test]
async fn attach_to_stopped_or_missing_entry_aborts_and_joins_incoming_task() {
    let manager = registered("one").await;
    manager.stop("one").await.unwrap();
    for id in ["one", "missing"] {
        let (incoming, mut dropped) = pending_task().await;
        manager.attach_task(id, incoming, None).await;
        assert!(dropped.try_recv().is_ok());
    }
    assert_eq!(
        manager.get("one").await.unwrap().status,
        AdapterStatus::Stopped
    );
}

#[tokio::test]
async fn stop_queued_after_replacement_cannot_finish_before_new_task_is_cleaned() {
    let manager = registered("one").await;
    let GracefulTask {
        handle,
        stop,
        stopping,
        release,
        dropped: _dropped,
    } = graceful_task();
    manager.attach_task("one", handle, Some(stop)).await;
    let (incoming, mut incoming_dropped) = pending_task().await;
    let owner = manager.clone();
    let replacing = tokio::spawn(async move { owner.attach_task("one", incoming, None).await });
    stopping.await.unwrap();
    let owner = manager.clone();
    let stopping = tokio::spawn(async move { owner.stop("one").await });
    tokio::task::yield_now().await;
    assert!(!stopping.is_finished());
    release.send(()).unwrap();
    replacing.await.unwrap();
    stopping.await.unwrap().unwrap();
    assert!(incoming_dropped.try_recv().is_ok());
    assert_eq!(
        manager.get("one").await.unwrap().status,
        AdapterStatus::Stopped
    );
}

#[tokio::test]
async fn dropping_manager_aborts_owned_tasks() {
    let manager = registered("one").await;
    let (task, dropped) = pending_task().await;
    manager.attach_task("one", task, None).await;
    drop(manager);
    tokio::time::timeout(Duration::from_secs(1), dropped)
        .await
        .unwrap()
        .unwrap();
}
