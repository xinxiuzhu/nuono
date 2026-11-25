//! 日志的插件归属，随 Future 的轮询和受管阻塞任务进入、退出。
//! 不用于权限判断；同一个依赖可同时为不同插件工作。

use std::future::Future;

tokio::task_local! {
    static PLUGIN: Option<String>;
}

/// 当前执行工作的插件名；没有明确来源时返回 `None`。
pub fn current_plugin() -> Option<String> {
    PLUGIN.try_with(Clone::clone).ok().flatten()
}

/// 为一次异步调用设置来源，挂起、完成、取消和 panic 时恢复外层上下文。
pub async fn scope<F: Future>(plugin: Option<String>, future: F) -> F::Output {
    PLUGIN.scope(plugin, future).await
}

/// 为同步工作设置来源，退出或 panic 后不会污染复用的工作线程。
pub fn scope_blocking<T>(plugin: Option<String>, job: impl FnOnce() -> T) -> T {
    PLUGIN.sync_scope(plugin, job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interleaved_plugins_and_nested_scopes_restore_their_owners() {
        let run = |name: &'static str| async move {
            scope(Some(name.into()), async {
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                    assert_eq!(current_plugin().as_deref(), Some(name));
                }
                scope(Some("nested".into()), async {
                    tokio::task::yield_now().await;
                    assert_eq!(current_plugin().as_deref(), Some("nested"));
                })
                .await;
                scope_blocking(None, || assert!(current_plugin().is_none()));
                assert_eq!(current_plugin().as_deref(), Some(name));
            })
            .await;
            assert!(current_plugin().is_none());
        };
        tokio::join!(run("DemoPlugin"), run("AnotherPlugin"));
    }

    #[test]
    fn blocking_panic_and_nested_contexts_restore_the_worker() {
        scope_blocking(Some("outer".into()), || {
            let result = std::panic::catch_unwind(|| {
                scope_blocking(Some("inner".into()), || {
                    assert_eq!(current_plugin().as_deref(), Some("inner"));
                    panic!("fixture panic");
                });
            });
            assert!(result.is_err());
            assert_eq!(current_plugin().as_deref(), Some("outer"));
        });
        assert!(current_plugin().is_none());
    }

    #[tokio::test]
    async fn cancelled_scope_does_not_leave_an_owner_on_the_executor() {
        let future = scope(Some("cancelled".into()), async {
            assert_eq!(current_plugin().as_deref(), Some("cancelled"));
            std::future::pending::<()>().await;
        });
        let mut future = Box::pin(future);
        assert!(matches!(
            futures::poll!(future.as_mut()),
            std::task::Poll::Pending
        ));
        assert!(current_plugin().is_none());
        drop(future);
        assert!(current_plugin().is_none());
    }

    #[tokio::test]
    async fn managed_tasks_grandchildren_and_render_workers_use_their_own_plugin() {
        use crate::{RenderLimits, RenderPool, plugin::PluginTaskScope};
        use std::sync::Arc;
        use tokio::sync::oneshot;

        let mut pending = Vec::new();
        for name in ["DemoPlugin", "AnotherPlugin"] {
            let tasks = Arc::new(PluginTaskScope::with_render_pool(
                name,
                Arc::new(
                    RenderPool::new(RenderLimits {
                        concurrency: 1,
                        queue_capacity: 1,
                    })
                    .unwrap(),
                ),
            ));
            let child_tasks = tasks.clone();
            let (sender, receiver) = oneshot::channel();
            scope(Some("CallerPlugin".into()), async {
                assert!(tasks.try_spawn("dependent request", async move {
                    tokio::task::yield_now().await;
                    let child = current_plugin();
                    let rendered = child_tasks
                        .render("image", || Ok(current_plugin()))
                        .await
                        .unwrap();
                    assert!(child_tasks.try_spawn("grandchild", async move {
                        tokio::task::yield_now().await;
                        sender.send((child, rendered, current_plugin())).unwrap();
                    }));
                }));
            })
            .await;
            pending.push((name, tasks, receiver));
        }
        for (name, tasks, receiver) in pending {
            let (child, rendered, grandchild) =
                tokio::time::timeout(std::time::Duration::from_secs(5), receiver)
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(child.as_deref(), Some(name));
            assert_eq!(rendered.as_deref(), Some(name));
            assert_eq!(grandchild.as_deref(), Some(name));
            tasks.shutdown().await;
        }
        assert!(current_plugin().is_none());
    }

    #[tokio::test]
    async fn browser_jobs_and_shutdown_callbacks_have_a_scoped_owner() {
        let tasks = crate::plugin::PluginTaskScope::new("BrowserPlugin");
        let (worker_tx, worker_rx) = tokio::sync::oneshot::channel();
        tasks
            .spawn_browser_job(move || {
                worker_tx.send(current_plugin()).unwrap();
            })
            .unwrap();
        let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
        tasks.on_shutdown(move || {
            cleanup_tx.send(current_plugin()).unwrap();
        });
        assert_eq!(worker_rx.await.unwrap().as_deref(), Some("BrowserPlugin"));
        tasks.shutdown().await;
        assert_eq!(cleanup_rx.await.unwrap().as_deref(), Some("BrowserPlugin"));
        assert!(current_plugin().is_none());
    }
}
