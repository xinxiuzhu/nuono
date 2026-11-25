//! 适配器连接监督器：指数退避重连的统一实现。
//!
//! 各平台适配器的连接生命周期（连接 → 事件循环 → 断开）各不相同，但"断开后
//! 按指数退避重连、稳定运行后重置退避"的策略应当一致。server 侧把单次连接
//! 会话封装为闭包交给 [`supervise`]，避免每个平台手写一份退避循环。

use std::future::Future;
use std::time::{Duration, Instant};

/// 重连退避策略。
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    /// 初始退避间隔。
    pub initial: Duration,
    /// 退避上限。
    pub max: Duration,
    /// 会话持续超过该时长视为"稳定"，下次失败从 initial 重新开始。
    pub stable_after: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// 永续监督一个连接会话闭包。
///
/// `session` 每次调用代表一次完整的连接生命周期；返回 `Ok(())` 表示正常结束
/// （仍会重连），返回 `Err` 表示连接失败。本函数永不返回，调用方应将其 spawn
/// 为后台任务并持有 JoinHandle 以便停止。
pub async fn supervise<F, Fut>(name: &str, policy: BackoffPolicy, mut session: F) -> !
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut backoff = policy.initial;
    loop {
        let started = Instant::now();
        let result = session().await;

        if started.elapsed() > policy.stable_after {
            backoff = policy.initial;
        }
        match result {
            Ok(()) => {
                tracing::warn!(adapter = name, retry_in = ?backoff, "适配器会话结束，准备重连")
            }
            Err(e) => {
                tracing::warn!(adapter = name, error = %e, retry_in = ?backoff, "适配器会话失败，准备重连")
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(policy.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn supervisor_retries_failed_sessions() {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        let policy = BackoffPolicy {
            initial: Duration::from_millis(5),
            max: Duration::from_millis(20),
            stable_after: Duration::from_secs(3600),
        };

        let handle = tokio::spawn(async move {
            supervise("test", policy, move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("boom"))
                }
            })
            .await
        });

        // 退避序列 5ms+10ms+20ms+20ms…，100ms 内至少完成 4 次尝试
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        assert!(attempts.load(Ordering::SeqCst) >= 4);
    }
}
