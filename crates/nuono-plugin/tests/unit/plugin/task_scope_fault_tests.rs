use super::{MAX_PLUGIN_ASYNC_TASKS, PluginTaskScope};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll},
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(2);

struct PendingDrop(Arc<AtomicUsize>);

impl Future for PendingDrop {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}

impl Drop for PendingDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn one_slot_scope(plugin: &str) -> Arc<PluginTaskScope> {
    let mut scope = PluginTaskScope::new(plugin);
    scope.async_slots = Arc::new(tokio::sync::Semaphore::new(1));
    Arc::new(scope)
}

async fn shutdown(scope: &PluginTaskScope) {
    tokio::time::timeout(DEADLINE, scope.shutdown())
        .await
        .expect("all task resources must leave the scope");
}

#[tokio::test]
async fn default_limit_counts_queued_futures_and_drops_rejections_immediately() {
    let scope = PluginTaskScope::new("task-capacity");
    let dropped = Arc::new(AtomicUsize::new(0));
    // 不主动让出执行权（yield）：验证即使是从未被 poll 过的排队任务也会占用并发配额。
    for _ in 0..MAX_PLUGIN_ASYNC_TASKS {
        assert!(scope.try_spawn("pending", PendingDrop(dropped.clone())));
    }
    assert!(!scope.try_spawn("rejected", PendingDrop(dropped.clone())));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    shutdown(&scope).await;
    assert_eq!(dropped.load(Ordering::SeqCst), MAX_PLUGIN_ASYNC_TASKS + 1);
    assert_eq!(
        scope.async_slots.available_permits(),
        MAX_PLUGIN_ASYNC_TASKS
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_admission_cannot_oversubscribe_the_scope() {
    let scope = Arc::new(PluginTaskScope::new("concurrent-capacity"));
    let accepted = AtomicUsize::new(0);
    let dropped = Arc::new(AtomicUsize::new(0));
    let start = std::sync::Barrier::new(8);
    std::thread::scope(|threads| {
        for _ in 0..8 {
            threads.spawn(|| {
                start.wait();
                for _ in 0..64 {
                    if scope.try_spawn("pending", PendingDrop(dropped.clone())) {
                        accepted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
        }
    });
    assert_eq!(accepted.load(Ordering::SeqCst), MAX_PLUGIN_ASYNC_TASKS);
    assert_eq!(dropped.load(Ordering::SeqCst), 512 - MAX_PLUGIN_ASYNC_TASKS);
    shutdown(&scope).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 512);
}

struct FinishWithBlockingDrop {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: mpsc::Receiver<()>,
    panic: bool,
}

impl Future for FinishWithBlockingDrop {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        assert!(!self.panic, "plugin future panic probe");
        Poll::Ready(())
    }
}

impl Drop for FinishWithBlockingDrop {
    fn drop(&mut self) {
        let _ = self.entered.take().unwrap().send(());
        let _ = self.release.recv();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_and_panic_release_capacity_only_after_future_destruction() {
    for panic in [false, true] {
        let scope = one_slot_scope("resource-destruction");
        let (entered, dropping) = tokio::sync::oneshot::channel();
        let (release, released) = mpsc::channel();
        assert!(scope.try_spawn(
            "finish",
            FinishWithBlockingDrop {
                entered: Some(entered),
                release: released,
                panic,
            },
        ));
        tokio::time::timeout(DEADLINE, dropping)
            .await
            .unwrap()
            .unwrap();
        assert!(!scope.try_spawn("too-early", async {}));
        assert_eq!(scope.async_slots.available_permits(), 0);
        release.send(()).unwrap();
        tokio::time::timeout(DEADLINE, scope.activity.wait())
            .await
            .unwrap();
        assert!(scope.try_spawn("capacity-returned", async {}));
        shutdown(&scope).await;
    }
}

#[tokio::test]
async fn cancellation_before_first_poll_reclaims_captures_and_capacity() {
    let scope = one_slot_scope("unpolled-cancellation");
    let dropped = Arc::new(AtomicUsize::new(0));
    assert!(scope.try_spawn("unpolled", PendingDrop(dropped.clone())));
    scope.abort_all();
    shutdown(&scope).await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(scope.async_slots.available_permits(), 1);
    assert!(!scope.try_spawn("closed", async {}));
}

struct ReenterOnDrop {
    scope: Weak<PluginTaskScope>,
    dropped: Arc<AtomicUsize>,
}

impl Future for ReenterOnDrop {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        panic!("rejected futures must never execute");
    }
}

impl Drop for ReenterOnDrop {
    fn drop(&mut self) {
        let scope = self.scope.upgrade().unwrap();
        assert!(!scope.try_spawn("drop-reentry", async {}));
        let dropped = self.dropped.clone();
        scope.on_shutdown(move || {
            dropped.fetch_add(1, Ordering::SeqCst);
        });
    }
}

#[tokio::test]
async fn full_and_closed_rejections_drop_plugin_futures_outside_the_tasks_lock() {
    for closed in [false, true] {
        let scope = one_slot_scope("reentrant-rejection");
        if closed {
            scope.abort_all();
        } else {
            assert!(scope.try_spawn("occupied", std::future::pending()));
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let rejected = ReenterOnDrop {
            scope: Arc::downgrade(&scope),
            dropped: dropped.clone(),
        };
        let worker_scope = scope.clone();
        let (finished, done) = tokio::sync::oneshot::channel();
        // 若出现锁重入死锁回归，应当触发外层超时失败，而不是卡死当前测试运行时。
        let worker = std::thread::spawn(move || {
            assert!(!worker_scope.try_spawn("rejected", rejected));
            let _ = finished.send(());
        });
        tokio::time::timeout(DEADLINE, done)
            .await
            .expect("future Drop must be able to re-enter task admission")
            .unwrap();
        worker.join().unwrap();
        shutdown(&scope).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn no_runtime_rejects_and_drops_the_future_without_panicking() {
    let scope = PluginTaskScope::new("no-runtime");
    assert!(scope.executor.is_none());
    let dropped = Arc::new(AtomicUsize::new(0));
    assert!(!scope.try_spawn("rejected", PendingDrop(dropped.clone())));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        scope.async_slots.available_permits(),
        MAX_PLUGIN_ASYNC_TASKS
    );
}

#[tokio::test]
async fn render_and_browser_keep_their_own_quota_when_async_capacity_is_full() {
    let scope = one_slot_scope("separate-work-classes");
    assert!(scope.try_spawn("ordinary-background", std::future::pending()));
    assert_eq!(scope.render("image", || Ok(42)).await.unwrap(), 42);
    let (done, receive) = tokio::sync::oneshot::channel();
    scope
        .spawn_browser_job(move || {
            let _ = done.send(());
        })
        .unwrap();
    tokio::time::timeout(DEADLINE, receive)
        .await
        .unwrap()
        .unwrap();
    shutdown(&scope).await;
}

#[tokio::test]
async fn cancelling_shutdown_does_not_lose_running_render_or_browser_work() {
    let scope = Arc::new(PluginTaskScope::new("cancelled-shutdown"));
    let (render_started, render_entered) = tokio::sync::oneshot::channel();
    let (release_render, render_released) = mpsc::channel();
    let render_scope = scope.clone();
    let render = tokio::spawn(async move {
        render_scope
            .render("blocked-image", move || {
                let _ = render_started.send(());
                let _ = render_released.recv();
                Ok(())
            })
            .await
    });
    let (browser_started, browser_entered) = tokio::sync::oneshot::channel();
    let (release_browser, browser_released) = mpsc::channel();
    scope
        .spawn_browser_job(move || {
            let _ = browser_started.send(());
            let _ = browser_released.recv();
        })
        .unwrap();
    tokio::time::timeout(DEADLINE, async {
        render_entered.await.unwrap();
        browser_entered.await.unwrap();
    })
    .await
    .unwrap();
    let mut first = Box::pin(scope.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut first)
            .await
            .is_err()
    );
    drop(first);
    assert!(scope.is_closed());
    let mut second = Box::pin(scope.shutdown());
    let mut concurrent = Box::pin(scope.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut concurrent)
            .await
            .is_err()
    );
    release_render.send(()).unwrap();
    tokio::time::timeout(DEADLINE, render)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    release_browser.send(()).unwrap();
    tokio::time::timeout(DEADLINE, async {
        tokio::join!(&mut second, &mut concurrent);
    })
    .await
    .unwrap();
    assert_eq!(scope.activity.active.load(Ordering::Acquire), 0);
    assert!(scope.tasks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_shutdown_waits_for_cleanup_started_by_abort_all() {
    let scope = Arc::new(PluginTaskScope::new("cleanup-barrier"));
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    scope.on_shutdown(move || {
        let _ = started.send(());
        let _ = released.recv();
    });
    let aborting = scope.clone();
    let worker = std::thread::spawn(move || aborting.abort_all());
    tokio::time::timeout(DEADLINE, entered)
        .await
        .unwrap()
        .unwrap();
    let mut waiting = Box::pin(scope.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    worker.join().unwrap();
    tokio::time::timeout(DEADLINE, waiting).await.unwrap();
}

#[tokio::test]
async fn shutting_down_an_old_instance_does_not_cancel_its_replacement() {
    let old = one_slot_scope("same-plugin");
    let replacement = one_slot_scope("same-plugin");
    let old_dropped = Arc::new(AtomicUsize::new(0));
    let new_dropped = Arc::new(AtomicUsize::new(0));
    assert!(old.try_spawn("old", PendingDrop(old_dropped.clone())));
    assert!(replacement.try_spawn("new", PendingDrop(new_dropped.clone())));
    shutdown(&old).await;
    assert_eq!(old_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(new_dropped.load(Ordering::SeqCst), 0);
    assert!(!replacement.is_closed());
    shutdown(&replacement).await;
    assert_eq!(new_dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn late_cleanup_panic_does_not_skip_other_cancellation_cleanup() {
    let scope = Arc::new(PluginTaskScope::new("late-cleanup-panic"));
    let count = Arc::new(AtomicUsize::new(0));
    let cancelling_scope = Arc::downgrade(&scope);
    struct CleanupOnDrop(Weak<PluginTaskScope>, Arc<AtomicUsize>);
    impl Drop for CleanupOnDrop {
        fn drop(&mut self) {
            let scope = self.0.upgrade().unwrap();
            scope.on_shutdown(|| panic!("late cleanup panic probe"));
            let count = self.1.clone();
            scope.on_shutdown(move || {
                count.fetch_add(1, Ordering::SeqCst);
            });
        }
    }
    let cleanup = CleanupOnDrop(cancelling_scope, count.clone());
    assert!(scope.try_spawn("cancel", async move {
        let _cleanup = cleanup;
        std::future::pending::<()>().await;
    }));
    shutdown(&scope).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
