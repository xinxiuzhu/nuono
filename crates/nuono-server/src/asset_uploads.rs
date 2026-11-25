//! 图片与角色素材上传的全局并发准入控制（覆盖从读取请求体、图片解码到落盘发布的完整生命周期）。
//! 一旦上传任务被 `run` 受理，即便客户端中途断开 HTTP 连接，后台任务也会独立执行完毕并完成清理。

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use futures::FutureExt;
use nuono_core::{NuonoError, error::Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio_util::{
    sync::CancellationToken,
    task::{TaskTracker, task_tracker::TaskTrackerToken},
};

const DEFAULT_CAPACITY: usize = 2;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct AssetUploads {
    slots: Arc<Semaphore>,
    closed: Mutex<bool>,
    stop: CancellationToken,
    activity: TaskTracker,
    #[cfg(test)]
    jobs: Arc<std::sync::atomic::AtomicUsize>,
}

impl AssetUploads {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(capacity)),
            closed: Mutex::new(false),
            stop: CancellationToken::new(),
            activity: TaskTracker::new(),
            #[cfg(test)]
            jobs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// 尝试申请上传并发槽位（无空闲槽位时立即拒绝，不排队等待）。
    /// 返回的共享租约贯穿请求体读取、图片解码、等待结果及最终落盘与清理全过程。
    pub(crate) fn admit(self: &Arc<Self>) -> Result<UploadLease> {
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *closed {
            metrics::counter!("nuono_asset_upload_rejected_total", "reason" => "closed")
                .increment(1);
            return Err(NuonoError::Conflict("图片上传已关闭".into()));
        }
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            metrics::counter!("nuono_asset_upload_rejected_total", "reason" => "full").increment(1);
            NuonoError::Overloaded("图片上传繁忙，请稍后重试".into())
        })?;
        // 用同一把 closed 互斥锁串行化准入与停机关闭。
        // 在读取请求体阶段（尚未 spawn 后台任务前）就登记 activity token，确保停机等待不会漏掉正在读请求体的连接。
        let activity = self.activity.token();
        Ok(UploadLease(Arc::new(LeaseState {
            owner: self.clone(),
            _permit: permit,
            _activity: activity,
            read_deadline: tokio::time::Instant::now() + BODY_TIMEOUT,
        })))
    }

    /// 在独立后台任务中执行解码与持久化。
    /// 调用方取消 await 只会丢弃 HTTP 响应等待端，已受理的文件写入和回滚清理仍会持有租约直到完成。
    pub(crate) async fn run<T, F, O>(&self, lease: UploadLease, operation: O) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
        O: FnOnce(UploadLease) -> F + Send + 'static,
    {
        let (send, receive) = oneshot::channel();
        {
            let closed = self
                .closed
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *closed {
                metrics::counter!("nuono_asset_upload_rejected_total", "reason" => "closed")
                    .increment(1);
                drop(closed);
                drop(operation);
                drop(lease);
                return Err(NuonoError::Conflict("图片上传已关闭".into()));
            }
            if !std::ptr::eq(self, lease.0.owner.as_ref()) {
                drop(closed);
                drop(operation);
                drop(lease);
                return Err(NuonoError::Conflict("图片上传凭据不属于当前服务".into()));
            }
            let operation_lease = lease.clone();
            let operation = async move { operation(operation_lease).await };
            let work = RetainedFuture {
                future: Box::pin(AssertUnwindSafe(operation).catch_unwind()),
                lease: Some(lease),
                #[cfg(test)]
                _job: JobCount::new(self.jobs.clone()),
            };
            self.activity.spawn(async move {
                let completed = work.await;
                let value = completed.value.map_err(|_| {
                    tracing::error!("图片上传处理任务 panic");
                    NuonoError::Internal("图片上传处理失败".into())
                });
                // 已完成的计算结果同样包裹着租约；若 HTTP 调用方已断开，
                // 发送失败时结果会先析构，随后才释放并发槽位。
                let _ = send.send(Retained {
                    value,
                    _lease: completed._lease,
                });
            });
        }
        receive
            .await
            .map_err(|_| NuonoError::Internal("图片上传任务异常退出".into()))?
            .value
    }

    pub(crate) fn close(&self) {
        let first = {
            let mut closed = self
                .closed
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            !std::mem::replace(&mut *closed, true)
        };
        if first {
            // 在锁外触发取消令牌以唤醒正在读取请求体的连接；
            // 已进入 `run` 的后台任务不监听该令牌，会继续跑完文件与数据库操作。
            self.stop.cancel();
            self.activity.close();
        }
    }

    /// 关闭上传准入，并等待所有正在读请求体的连接、后台处理 Future、阻塞线程池任务及待回收结果全部结束。
    /// 即便 HTTP 请求被客户端提前丢弃，该停机屏障也不会提前放行。
    pub(crate) async fn wait(&self) {
        self.close();
        self.activity.wait().await;
    }

    #[cfg(test)]
    pub(crate) fn active_jobs(&self) -> usize {
        self.jobs.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn available_slots(&self) -> usize {
        self.slots.available_permits()
    }
}

impl Drop for AssetUploads {
    fn drop(&mut self) {
        self.close();
    }
}

struct LeaseState {
    owner: Arc<AssetUploads>,
    // 字段声明顺序决定析构顺序：先归还并发信号量 `_permit`，再释放停机活动计数 `_activity`。
    _permit: OwnedSemaphorePermit,
    _activity: TaskTrackerToken,
    read_deadline: tokio::time::Instant,
}

#[derive(Clone)]
pub(crate) struct UploadLease(Arc<LeaseState>);

impl UploadLease {
    pub(crate) async fn cancelled(&self) {
        self.0.owner.stop.cancelled().await;
    }

    /// 读取请求体使用从准入时刻起算的绝对超时时间，不会随每个 multipart 字段重置；
    /// 该超时仅约束请求体读取阶段，不影响已受理的落盘写入。
    pub(crate) async fn read<F: Future>(&self, read: F) -> Result<F::Output> {
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(NuonoError::Conflict("图片上传已关闭".into())),
            _ = tokio::time::sleep_until(self.0.read_deadline) => Err(NuonoError::Timeout("读取图片上传内容超时".into())),
            result = read => Ok(result),
        }
    }

    /// 在 Tokio `spawn_blocking` 线程池排队与执行期间持续持有上传租约；
    /// 即便异步调用方提前取消等待，也不会在阻塞任务真正跑完前归还并发槽位。
    pub(crate) async fn blocking<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let work = BlockingWork {
            job: Some(job),
            lease: self.clone(),
        };
        let completed = tokio::task::spawn_blocking(move || work.run())
            .await
            .map_err(|error| {
                tracing::error!(%error, "图片上传阻塞任务异常退出");
                NuonoError::Internal("图片处理失败".into())
            })?;
        Ok(completed.value)
    }
}

// 结构体字段顺序确保业务载荷（value / job / future）先于 `_lease` 声明并先析构：
// 无论是任务取消、panic 还是结果无人接收，都会先销毁闭包捕获的资源，最后才释放并发槽位与停机屏障。
struct Retained<T> {
    value: T,
    _lease: UploadLease,
}

struct BlockingWork<F> {
    job: Option<F>,
    lease: UploadLease,
}

impl<F> BlockingWork<F> {
    fn run<T>(mut self) -> Retained<T>
    where
        F: FnOnce() -> T,
    {
        let value = self.job.take().expect("blocking upload job consumed once")();
        Retained {
            value,
            _lease: self.lease,
        }
    }
}

struct RetainedFuture<F> {
    future: Pin<Box<F>>,
    lease: Option<UploadLease>,
    #[cfg(test)]
    _job: JobCount,
}

impl<F: Future> Future for RetainedFuture<F> {
    type Output = Retained<F::Output>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.future.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(value) => Poll::Ready(Retained {
                value,
                _lease: self.lease.take().expect("upload future completed once"),
            }),
        }
    }
}

#[cfg(test)]
struct JobCount(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl JobCount {
    fn new(count: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count)
    }
}

#[cfg(test)]
impl Drop for JobCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
#[path = "../tests/unit/asset_uploads/tests.rs"]
mod tests;
