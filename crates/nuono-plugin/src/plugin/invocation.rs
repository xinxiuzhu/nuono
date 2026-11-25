//! 由宿主签发、仅在归属插件一致的受管任务间传递的调用租约（Invocation Lease）。
//! SDK 仅负责透传该不透明租约守卫，不感知宿主侧的并发限流器状态或持久化细节。

use std::{cell::RefCell, future::Future, mem::DropGuard, sync::Arc};

#[derive(Clone)]
pub(super) struct Invocation {
    plugin: String,
    _guard: Arc<dyn Send + Sync>,
}

tokio::task_local! {
    static CURRENT: Invocation;
}

thread_local! {
    // 同步阻塞任务无法读取 Tokio 的 `task_local`，因此改用线程局部变量兜底；
    // 通过 `DropGuard` 将生命周期严格限制在单次阻塞闭包执行期间（含 panic 栈展开），确保复用的工作线程不会残留租约。
    static BLOCKING: RefCell<Option<Invocation>> = const { RefCell::new(None) };
}

/// 携带宿主签发的不透明租约守卫执行一次受准入控制的插件回调。
/// 同属该插件的 SDK 受管子任务（`ctx.spawn` / 渲染 / 浏览器任务）会共同持有该租约直至全部完成或被取消；
/// 插件自行通过 `tokio::spawn` 派生的裸任务则不会隐式继承。注意：子任务继承仅延长当前租约的释放时机，并不授予额外的新准入配额。
#[doc(hidden)]
pub async fn with_invocation_guard<F>(
    plugin: &str,
    guard: Arc<dyn Send + Sync>,
    future: F,
) -> F::Output
where
    F: Future,
{
    CURRENT
        .scope(
            Invocation {
                plugin: plugin.to_owned(),
                _guard: guard,
            },
            future,
        )
        .await
}

pub(super) fn capture(plugin: &str) -> Option<Invocation> {
    match CURRENT.try_with(|current| (current.plugin == plugin).then(|| current.clone())) {
        // 若当前处于明确的异步 task_local 作用域中（哪怕属于其他插件），优先以此为准，
        // 绝不回退去误拿外层线程残留的另一个插件的阻塞作用域租约。
        Ok(current) => current,
        Err(_) => BLOCKING.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|current| current.plugin == plugin)
                .cloned()
        }),
    }
}

pub(super) async fn inherit<F: Future>(invocation: Option<Invocation>, future: F) -> F::Output {
    match invocation {
        Some(invocation) => CURRENT.scope(invocation, future).await,
        None => future.await,
    }
}

pub(super) fn run_blocking<T>(invocation: Option<Invocation>, job: impl FnOnce() -> T) -> T {
    let previous = BLOCKING.with(|slot| slot.replace(invocation));
    let _restore = DropGuard::new(previous, |previous| {
        BLOCKING.with(|slot| {
            slot.replace(previous);
        });
    });
    job()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RenderLimits, RenderPool, plugin::PluginTaskScope};
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::sync::oneshot;

    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn lease() -> (Arc<dyn Send + Sync>, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        (Arc::new(Lease(drops.clone())), drops)
    }

    #[test]
    fn nested_blocking_panic_restores_outer_invocation_and_releases_both_leases() {
        let (outer_guard, outer_drops) = lease();
        let (inner_guard, inner_drops) = lease();
        run_blocking(
            Some(Invocation {
                plugin: "outer".into(),
                _guard: outer_guard,
            }),
            || {
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_blocking(
                        Some(Invocation {
                            plugin: "inner".into(),
                            _guard: inner_guard,
                        }),
                        || {
                            assert!(capture("inner").is_some());
                            assert!(capture("outer").is_none());
                            panic!("synthetic blocking panic");
                        },
                    );
                }));
                assert!(panic.is_err());
                assert!(capture("outer").is_some());
                assert!(capture("inner").is_none());
                assert_eq!(inner_drops.load(Ordering::SeqCst), 1);
                assert_eq!(outer_drops.load(Ordering::SeqCst), 0);
            },
        );
        assert!(capture("outer").is_none());
        assert_eq!(outer_drops.load(Ordering::SeqCst), 1);
    }

    async fn dropped(drops: &AtomicUsize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("managed invocation lease was not released");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    fn render_scope(plugin: &str) -> Arc<PluginTaskScope> {
        Arc::new(PluginTaskScope::with_render_pool(
            plugin,
            Arc::new(
                RenderPool::new(RenderLimits {
                    concurrency: 1,
                    queue_capacity: 0,
                })
                .unwrap(),
            ),
        ))
    }

    #[tokio::test]
    async fn managed_grandchild_keeps_lease_after_parent_callback_and_task_finish() {
        let scope = Arc::new(PluginTaskScope::new("lease-parent"));
        let (guard, drops) = lease();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        with_invocation_guard("lease-parent", guard, async {
            let child_scope = scope.clone();
            assert!(scope.try_spawn("child", async move {
                assert!(child_scope.try_spawn("grandchild", async move {
                    started_tx.send(()).unwrap();
                    let _ = release_rx.await;
                }));
            }));
        })
        .await;
        started_rx.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        dropped(&drops).await;
        scope.shutdown().await;
    }

    #[tokio::test]
    async fn another_plugins_task_does_not_inherit_lease() {
        let scope = PluginTaskScope::new("lease-other");
        let (guard, drops) = lease();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        with_invocation_guard("lease-owner", guard, async {
            assert!(scope.try_spawn("other", async move {
                started_tx.send(()).unwrap();
                let _ = release_rx.await;
            }));
        })
        .await;
        started_rx.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        scope.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_managed_task_releases_lease_on_shutdown() {
        let scope = PluginTaskScope::new("lease-cancel");
        let (guard, drops) = lease();
        let (started_tx, started_rx) = oneshot::channel();
        with_invocation_guard("lease-cancel", guard, async {
            assert!(scope.try_spawn("pending", async move {
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            }));
        })
        .await;
        started_rx.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        scope.shutdown().await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn caught_managed_panic_releases_lease() {
        let scope = PluginTaskScope::new("lease-panic");
        let (guard, drops) = lease();
        with_invocation_guard("lease-panic", guard, async {
            assert!(scope.try_spawn("panics", async { panic!("synthetic managed panic") }));
        })
        .await;
        dropped(&drops).await;
        scope.shutdown().await;
    }

    #[tokio::test]
    async fn raw_tokio_task_does_not_implicitly_inherit_lease() {
        let scope = Arc::new(PluginTaskScope::new("lease-raw"));
        let (guard, drops) = lease();
        let (release_tx, release_rx) = oneshot::channel();
        with_invocation_guard("lease-raw", guard, async {
            let scope = scope.clone();
            tokio::spawn(async move {
                assert!(capture("lease-raw").is_none());
                assert!(scope.try_spawn("from-raw", async move {
                    let _ = release_rx.await;
                }));
            })
            .await
            .unwrap();
        })
        .await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        scope.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_render_receipt_keeps_lease_until_blocking_job_exits() {
        let scope = render_scope("lease-render-cancel");
        let (guard, drops) = lease();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = scope.clone();
        let receipt = tokio::spawn(async move {
            with_invocation_guard("lease-render-cancel", guard, async move {
                worker
                    .render("blocking", move || {
                        started_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        Ok(())
                    })
                    .await
            })
            .await
        });
        started_rx.await.unwrap();
        receipt.abort();
        let _ = receipt.await;
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        dropped(&drops).await;
        scope.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_render_propagates_lease_to_managed_descendant() {
        let scope = render_scope("lease-render-child");
        let (guard, drops) = lease();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        with_invocation_guard("lease-render-child", guard, async {
            let child_scope = scope.clone();
            scope
                .render("with-child", move || {
                    assert!(child_scope.try_spawn("after-render", async move {
                        started_tx.send(()).unwrap();
                        let _ = release_rx.await;
                    }));
                    Ok(())
                })
                .await
                .unwrap();
        })
        .await;
        started_rx.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        dropped(&drops).await;
        scope.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_job_keeps_lease_and_propagates_to_managed_descendant() {
        let scope = Arc::new(PluginTaskScope::new("lease-browser"));
        let (guard, drops) = lease();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (child_tx, child_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        with_invocation_guard("lease-browser", guard, async {
            let child_scope = scope.clone();
            scope
                .spawn_browser_job(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert!(child_scope.try_spawn("after-browser", async move {
                        child_tx.send(()).unwrap();
                        let _ = finish_rx.await;
                    }));
                })
                .unwrap();
        })
        .await;
        started_rx.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        child_rx.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        finish_tx.send(()).unwrap();
        dropped(&drops).await;
        scope.shutdown().await;
    }
}
