use std::sync::Arc;

use nuono_plugin::{PluginTaskScope, RenderLimits, RenderPool};
use tokio::sync::oneshot;

fn pool(queue_capacity: usize) -> Arc<RenderPool> {
    Arc::new(
        RenderPool::new(RenderLimits {
            concurrency: 1,
            queue_capacity,
        })
        .unwrap(),
    )
}

#[tokio::test]
async fn cancelling_a_waiter_keeps_the_running_job_in_the_shared_quota() {
    let pool = pool(0);
    let first = Arc::new(PluginTaskScope::with_render_pool("first", pool.clone()));
    let second = PluginTaskScope::with_render_pool("second", pool);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let scope = first.clone();
    let waiter = tokio::spawn(async move {
        scope
            .render("card", move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    started_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let error = second.render("card", || Ok(())).await.unwrap_err();
    assert!(error.to_string().contains("稍后重试"));
    release_tx.send(()).unwrap();
    first.shutdown().await;
    assert_eq!(second.render("card", || Ok(42)).await.unwrap(), 42);
    second.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_queued_jobs_and_waits_for_the_running_job() {
    let scope = Arc::new(PluginTaskScope::with_render_pool("cards", pool(1)));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let running_scope = scope.clone();
    let running = tokio::spawn(async move {
        running_scope
            .render("running", move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    started_rx.await.unwrap();
    let queued = scope.render("queued", || -> anyhow::Result<()> {
        panic!("排队任务不应执行")
    });
    tokio::pin!(queued);
    assert!(futures::poll!(&mut queued).is_pending());
    let shutting_scope = scope.clone();
    let mut shutdown = tokio::spawn(async move { shutting_scope.shutdown().await });
    assert!(queued.await.unwrap_err().to_string().contains("停用"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut shutdown)
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    shutdown.await.unwrap();
    running.await.unwrap().unwrap();
    assert!(scope.render("late", || Ok(())).await.is_err());
}

#[tokio::test]
async fn panicking_job_returns_an_error_and_releases_its_slot() {
    let scope = PluginTaskScope::with_render_pool("cards", pool(0));
    let error = scope
        .render("broken", || -> anyhow::Result<()> {
            panic!("绘图错误")
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("panic"));
    assert_eq!(scope.render("healthy", || Ok(7)).await.unwrap(), 7);
    scope.shutdown().await;
}

#[test]
fn render_pool_rejects_invalid_limits() {
    assert!(
        RenderPool::new(RenderLimits {
            concurrency: 0,
            queue_capacity: 1
        })
        .is_err()
    );
    assert!(
        RenderPool::new(RenderLimits {
            concurrency: 1,
            queue_capacity: usize::MAX
        })
        .is_err()
    );
}
