//! 宿主 Agent 运行时管理的有界回复投递任务池。
use crate::error::{NuonoError, Result};
use futures::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe};
use tokio::sync::{Mutex, watch};

const MAX_REPLY_TASKS: usize = 32;

pub(super) struct ReplyTasks {
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for ReplyTasks {
    fn default() -> Self {
        Self {
            stop: watch::channel(false).0,
            tasks: Mutex::new(Vec::new()),
        }
    }
}

impl ReplyTasks {
    pub(super) async fn spawn(
        &self,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        self.spawn_settling(move |mut stop| async move {
            if *stop.borrow() {
                return;
            }
            tokio::select! {
                biased;
                _ = stop.changed() => {},
                () = future => {},
            }
        })
        .await
    }

    /// 投递任务自行响应停机信号，确保推理被取消后仍能完成状态持久化与收尾结算。
    pub(super) async fn spawn_settling<F, Fut>(&self, future: F) -> Result<()>
    where
        F: FnOnce(watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if *self.stop.borrow() {
            return Err(NuonoError::Conflict("Agent 回复投递已关闭".into()));
        }
        let mut tasks = self.tasks.lock().await;
        if *self.stop.borrow() {
            return Err(NuonoError::Conflict("Agent 回复投递已关闭".into()));
        }
        tasks.retain(|task| !task.is_finished());
        if tasks.len() >= MAX_REPLY_TASKS {
            return Err(NuonoError::Conflict(
                "Agent 回复任务已满，请稍后重试".into(),
            ));
        }
        let stop = self.stop.subscribe();
        tasks.push(tokio::spawn(async move {
            if AssertUnwindSafe(future(stop)).catch_unwind().await.is_err() {
                tracing::warn!("Agent 回复投递任务 panic，任务已退出");
            }
        }));
        Ok(())
    }

    pub(super) fn request_shutdown(&self) {
        self.stop.send_replace(true);
    }

    pub(super) async fn shutdown(&self) {
        self.request_shutdown();
        // 在 await 期间将任务句柄保留在共享列表中，防止当前等待方被取消时导致投递任务脱离监管、或后续 shutdown 漏等其结束。
        let mut tasks = self.tasks.lock().await;
        while let Some(task) = tasks.last_mut() {
            let _ = task.await;
            tasks.pop();
        }
    }
}

impl Drop for ReplyTasks {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    struct OnDrop(Arc<AtomicUsize>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn saturated_reply_delivery_is_bounded_and_shutdown_drops_every_future() {
        let scope = ReplyTasks::default();
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..MAX_REPLY_TASKS {
            let guard = OnDrop(dropped.clone());
            scope
                .spawn(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                })
                .await
                .unwrap();
        }
        let guard = OnDrop(dropped.clone());
        assert!(
            scope
                .spawn(async move {
                    let _guard = guard;
                    panic!("rejected delivery must never be polled");
                })
                .await
                .is_err()
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(2), scope.shutdown())
            .await
            .unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), MAX_REPLY_TASKS + 1);
        assert!(
            scope
                .spawn(async { panic!("closed delivery") })
                .await
                .is_err()
        );
        assert!(scope.tasks.lock().await.is_empty());
    }

    #[tokio::test]
    async fn cancelled_shutdown_waiter_can_be_resumed_without_losing_task_handles() {
        let scope = ReplyTasks::default();
        let dropped = Arc::new(AtomicUsize::new(0));
        let guard = OnDrop(dropped.clone());
        scope
            .spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })
            .await
            .unwrap();
        let mut shutdown = Box::pin(scope.shutdown());
        std::future::poll_fn(|cx| {
            assert!(shutdown.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(shutdown);
        assert!(*scope.stop.borrow());
        scope.shutdown().await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(scope.tasks.lock().await.is_empty());
    }

    #[tokio::test]
    async fn close_racing_with_registration_never_polls_new_work() {
        let scope = ReplyTasks::default();
        let registration = scope.tasks.lock().await;
        let mut spawn = Box::pin(scope.spawn(async { panic!("must stay unpolled") }));
        std::future::poll_fn(|cx| {
            assert!(spawn.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        scope.request_shutdown();
        drop(registration);
        assert!(spawn.await.is_err());
        scope.shutdown().await;
    }
}
