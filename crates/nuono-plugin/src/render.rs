//! 多个插件共用的 CPU 绘图配额，不绑定具体绘图库。
use std::sync::{Arc, LazyLock};

use anyhow::{Result, ensure};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 限制同时绘图和等待绘图的数量，避免小内存机器堆积任务。
#[derive(Debug, Clone, Copy)]
pub struct RenderLimits {
    pub concurrency: usize,
    pub queue_capacity: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            concurrency: std::thread::available_parallelism().map_or(1, |n| n.get().min(2)),
            queue_capacity: 16,
        }
    }
}

/// 宿主进程共享的绘图池；实际计算在线程里执行，插件可使用任意绘图库。
#[derive(Debug)]
pub struct RenderPool {
    slots: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
    limits: RenderLimits,
}

impl RenderPool {
    pub fn new(limits: RenderLimits) -> Result<Self> {
        ensure!(limits.concurrency > 0, "绘图并发数必须大于 0");
        let capacity = limits
            .concurrency
            .checked_add(limits.queue_capacity)
            .filter(|n| *n <= Semaphore::MAX_PERMITS)
            .ok_or_else(|| anyhow::anyhow!("绘图队列容量超出范围"))?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(limits.concurrency)),
            admitted: Arc::new(Semaphore::new(capacity)),
            limits,
        })
    }

    pub fn limits(&self) -> RenderLimits {
        self.limits
    }

    /// 所有默认插件任务作用域使用同一个配额，插件数量不会放大绘图并发数。
    pub fn shared() -> Arc<Self> {
        static POOL: LazyLock<Arc<RenderPool>> = LazyLock::new(|| {
            Arc::new(RenderPool::new(RenderLimits::default()).expect("默认绘图配额有效"))
        });
        POOL.clone()
    }

    pub(crate) fn admit(&self) -> Result<OwnedSemaphorePermit> {
        self.admitted
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("绘图任务较多，请稍后重试"))
    }

    pub(crate) async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        self.slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("绘图池已关闭"))
    }
}
