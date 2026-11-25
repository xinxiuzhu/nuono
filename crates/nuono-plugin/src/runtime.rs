//! 将受管后台任务与适配器操作派发到创建时捕获的宿主 Tokio 运行时上执行。
use crate::error::{NuonoError, Result};
use futures::future::BoxFuture;
use std::sync::Arc;

/// 运行在所捕获 Tokio 运行时上的受管任务句柄。
pub trait RuntimeTask: Send + Sync {
    fn abort(&self);
    fn is_finished(&self) -> bool;
    fn join(self: Box<Self>) -> BoxFuture<'static, Result<()>>;
}
trait TaskSpawner: Send + Sync {
    fn spawn(&self, future: BoxFuture<'static, ()>) -> Box<dyn RuntimeTask>;
    fn spawn_blocking(&self, job: Box<dyn FnOnce() + Send>) -> Box<dyn RuntimeTask>;
}
struct TokioSpawner(tokio::runtime::Handle);
struct TokioTask(tokio::task::JoinHandle<()>);
impl TaskSpawner for TokioSpawner {
    fn spawn(&self, future: BoxFuture<'static, ()>) -> Box<dyn RuntimeTask> {
        Box::new(TokioTask(self.0.spawn(future)))
    }
    fn spawn_blocking(&self, job: Box<dyn FnOnce() + Send>) -> Box<dyn RuntimeTask> {
        Box::new(TokioTask(self.0.spawn_blocking(job)))
    }
}
impl RuntimeTask for TokioTask {
    fn abort(&self) {
        self.0.abort();
    }
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
    fn join(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let mut task = self;
            (&mut task.0)
                .await
                .map_err(|e| NuonoError::Internal(format!("plugin runtime task failed: {e}")))
        })
    }
}
impl Drop for TokioTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 捕获并绑定某个服务或任务作用域所属的宿主 Tokio 运行时。
#[derive(Clone)]
pub struct RuntimeExecutor(Arc<dyn TaskSpawner>);

impl std::fmt::Debug for RuntimeExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeExecutor").finish_non_exhaustive()
    }
}
impl RuntimeExecutor {
    pub fn current() -> Option<Self> {
        tokio::runtime::Handle::try_current()
            .ok()
            .map(|handle| Self(Arc::new(TokioSpawner(handle))))
    }
    pub fn spawn(&self, future: BoxFuture<'static, ()>) -> Box<dyn RuntimeTask> {
        self.0.spawn(future)
    }
    pub(crate) fn spawn_blocking(&self, job: Box<dyn FnOnce() + Send>) -> Box<dyn RuntimeTask> {
        self.0.spawn_blocking(job)
    }
    pub async fn run<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let task = self.spawn(Box::pin(async move {
            let _ = sender.send(future.await);
        }));
        task.join().await?;
        receiver
            .await
            .map_err(|e| NuonoError::Internal(format!("plugin runtime receipt lost: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_stays_on_the_capturing_runtime() {
        let host = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("captured-host")
            .enable_all()
            .build()
            .unwrap();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let executor = host.block_on(async { RuntimeExecutor::current().unwrap() });
        caller.block_on(async {
            let thread = executor
                .run(async { Ok(std::thread::current().name().unwrap().to_owned()) })
                .await
                .unwrap();
            assert_eq!(thread, "captured-host");
        });
    }

    #[tokio::test]
    async fn cancelling_a_receipt_aborts_the_dispatched_operation() {
        struct NotifyDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let executor = RuntimeExecutor::current().unwrap();
        let receipt = tokio::spawn(async move {
            executor
                .run(async move {
                    let _guard = NotifyDrop(Some(dropped_tx));
                    started_tx.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                })
                .await
        });
        started_rx.await.unwrap();
        receipt.abort();
        let _ = receipt.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }
}
