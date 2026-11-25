//! 数据库备份、恢复与迁移等长耗时文件操作的生命周期跟踪器。
//! 一旦操作开始执行，其生命周期独立于 HTTP 请求连接；注册与停机关闭互斥串行化，
//! 并持续持有 `restore_lock` 直至整个操作（含 `spawn_blocking` 线程任务）彻底结束。
use futures::FutureExt;
use nuono_core::{NuonoError, Result};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub(crate) struct DatabaseOperations {
    closed: Mutex<bool>,
    stop: CancellationToken,
    tasks: TaskTracker,
}

impl DatabaseOperations {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            closed: Mutex::new(false),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
        })
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        lock: Arc<AsyncMutex<()>>,
        operation: impl Future<Output = T> + Send + 'static,
    ) -> Result<T> {
        // 尚在排队等待互斥锁、还未真正开始执行的请求，在收到停机信号时可直接取消返回。
        let guard = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(closing()),
            guard = lock.lock_owned() => guard,
        };
        self.run_owned(guard, operation).await
    }

    pub(crate) async fn run_owned<T: Send + 'static>(
        &self,
        guard: tokio::sync::OwnedMutexGuard<()>,
        operation: impl Future<Output = T> + Send + 'static,
    ) -> Result<T> {
        let (send, receive) = oneshot::channel();
        {
            let closed = self
                .closed
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *closed {
                return Err(closing());
            }
            self.tasks.spawn(async move {
                let result = {
                    let _guard = guard;
                    AssertUnwindSafe(operation).catch_unwind().await
                };
                let result = result.map_err(|_| {
                    tracing::error!("数据库文件操作 panic，请检查备份或恢复状态");
                    NuonoError::Internal("数据库文件操作未正常完成".into())
                });
                // 客户端提前断开只会丢弃 oneshot 响应接收端，后台任务本身已完整执行完毕。
                let _ = send.send(result);
            });
        }
        receive
            .await
            .map_err(|_| NuonoError::Internal("数据库文件操作任务丢失".into()))?
    }

    pub(crate) fn close(&self) {
        let mut closed = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *closed = true;
        self.stop.cancel();
        self.tasks.close();
    }

    /// 等待所有已受理的数据库文件操作完成（必须在关闭数据库连接池及其依赖项之前调用）。
    pub(crate) async fn wait(&self) {
        let wait = self.tasks.wait();
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut wait => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    tracing::warn!(remaining = self.tasks.len(), "仍在等待数据库文件操作完成");
                }
            }
        }
    }
}

fn closing() -> NuonoError {
    NuonoError::Conflict("服务正在关闭，不能开始新的备份或恢复操作".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_request_retains_lock_until_blocking_work_and_cleanup_finish() {
        let operations = DatabaseOperations::new();
        let lock = Arc::new(AsyncMutex::new(()));
        let (started, ready) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let scope = operations.clone();
        let task_lock = lock.clone();
        let task_completed = completed.clone();
        let request = tokio::spawn(async move {
            scope
                .run(task_lock, async move {
                    tokio::task::spawn_blocking(move || {
                        started.send(()).unwrap();
                        blocked.recv().unwrap();
                    })
                    .await
                    .unwrap();
                    task_completed.store(true, Ordering::Release);
                })
                .await
        });
        ready.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(lock.try_lock().is_err());
        operations.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), operations.wait())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        operations.wait().await;
        assert!(completed.load(Ordering::Acquire));
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn close_rejects_queued_and_new_requests_and_waits_for_accepted_work() {
        let operations = DatabaseOperations::new();
        let lock = Arc::new(AsyncMutex::new(()));
        let guard = lock.clone().lock_owned().await;
        let queued = operations.run(lock.clone(), async { panic!("closed work ran") });
        tokio::pin!(queued);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut queued)
                .await
                .is_err()
        );
        operations.close();
        assert!(matches!(queued.await, Err(NuonoError::Conflict(_))));
        drop(guard);
        assert!(matches!(
            operations.run(lock, async {}).await,
            Err(NuonoError::Conflict(_))
        ));
        operations.wait().await;
    }

    #[tokio::test]
    async fn panicking_operation_reports_failure_and_releases_its_registration() {
        let operations = DatabaseOperations::new();
        let lock = Arc::new(AsyncMutex::new(()));
        assert!(matches!(
            operations
                .run(lock.clone(), async { panic!("fault injection") })
                .await,
            Err(NuonoError::Internal(_))
        ));
        operations.close();
        operations.wait().await;
        assert!(lock.try_lock().is_ok());
    }
}
