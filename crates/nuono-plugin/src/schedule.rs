//! 插件定时任务。任务由宿主回收，插件不必自己维护 JoinHandle 列表。
use std::{future::Future, time::Duration};

use anyhow::{Result, ensure};
use chrono::{DateTime, FixedOffset, NaiveTime, TimeZone, Utc};
use futures::future::{AbortHandle, Abortable};

use crate::PluginTaskScope;

/// 调度时间。每天执行使用显式 UTC 偏移，不依赖服务器所在时区。
#[derive(Clone, Debug)]
pub struct Schedule(Kind);

#[derive(Clone, Debug)]
enum Kind {
    Interval(Duration),
    Daily {
        time: NaiveTime,
        offset: FixedOffset,
    },
}

impl Schedule {
    /// 间隔执行，首次执行在一个完整间隔后。执行较慢时跳过错过的轮次，不并发补跑。
    pub fn every(interval: Duration) -> Result<Self> {
        ensure!(!interval.is_zero(), "任务间隔必须大于 0");
        ensure!(
            std::time::Instant::now().checked_add(interval).is_some(),
            "任务间隔过大"
        );
        Ok(Self(Kind::Interval(interval)))
    }

    /// 每天当地时间执行。北京时间 8 点：`Schedule::daily(8, 0, 8 * 3600)?`。
    /// 固定偏移不处理夏令时；重启或停机期间错过的任务不会补跑。
    pub fn daily(hour: u32, minute: u32, utc_offset_seconds: i32) -> Result<Self> {
        let time = NaiveTime::from_hms_opt(hour, minute, 0)
            .ok_or_else(|| anyhow::anyhow!("每天执行的时间无效"))?;
        let offset = FixedOffset::east_opt(utc_offset_seconds)
            .ok_or_else(|| anyhow::anyhow!("UTC 偏移无效"))?;
        Ok(Self(Kind::Daily { time, offset }))
    }

    fn next_daily(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let Kind::Daily { time, offset } = &self.0 else {
            unreachable!()
        };
        let date = now.with_timezone(offset).date_naive();
        let today = offset
            .from_local_datetime(&date.and_time(*time))
            .earliest()
            .expect("FixedOffset 映射到唯一本地时间")
            .with_timezone(&Utc);
        if today > now {
            today
        } else {
            today + chrono::Duration::days(1)
        }
    }
}

/// 单个任务的取消句柄。丢弃句柄不取消任务；禁用插件会统一取消全部任务。
#[derive(Clone, Debug)]
pub struct ScheduledTask(AbortHandle);

impl ScheduledTask {
    /// 取消包括当前正在执行的异步回调；不会撤销已经发出的消息或写入。
    pub fn cancel(&self) {
        self.0.abort();
    }
}

impl PluginTaskScope {
    /// 注册定时回调。回调返回错误时记录插件名、任务名，下一轮继续执行。
    /// 同一个任务不会重叠执行；同名任务仍是独立任务，应在 on_enable 为本轮启用注册一次。
    pub fn schedule<F, Fut>(
        &self,
        name: &str,
        schedule: Schedule,
        mut job: F,
    ) -> Result<ScheduledTask>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let first_interval_tick = match schedule.0 {
            Kind::Interval(period) => Some((
                tokio::time::Instant::now()
                    .checked_add(period)
                    .ok_or_else(|| anyhow::anyhow!("任务间隔超出单调时钟范围"))?,
                period,
            )),
            Kind::Daily { .. } => None,
        };
        let (handle, registration) = AbortHandle::new_pair();
        let plugin = self.plugin_name().to_string();
        let label = name.to_string();
        let task = async move {
            let mut interval = first_interval_tick.map(|(start, period)| {
                let mut timer = tokio::time::interval_at(start, period);
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                timer
            });
            loop {
                if let Some(timer) = &mut interval {
                    timer.tick().await;
                } else {
                    // 分段等待，重新读取墙上时钟，避免校时后长期沿用旧的等待时长。
                    let target = schedule.next_daily(Utc::now());
                    loop {
                        let remaining = target.signed_duration_since(Utc::now());
                        if remaining <= chrono::Duration::zero() {
                            break;
                        }
                        let sleep_for = remaining
                            .to_std()
                            .unwrap_or(Duration::ZERO)
                            .min(Duration::from_secs(30));
                        tokio::time::sleep(sleep_for).await;
                    }
                }
                if let Err(error) = job().await {
                    tracing::warn!(plugin = %plugin, task = %label, %error, "插件定时任务执行失败");
                }
            }
        };
        ensure!(
            self.try_spawn(name, async move {
                let _ = Abortable::new(task, registration).await;
            }),
            "插件已停用、后台任务容量已满或 runtime 不可用，不能注册定时任务"
        );
        Ok(ScheduledTask(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daily_uses_explicit_timezone_and_skips_current_minute() {
        let plan = Schedule::daily(8, 0, 8 * 3600).unwrap();
        let now = "2026-10-03T00:00:00Z".parse().unwrap();
        assert_eq!(
            plan.next_daily(now),
            "2026-10-04T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        let before = "2026-10-02T23:59:59Z".parse().unwrap();
        assert_eq!(plan.next_daily(before), now);
        assert!(Schedule::daily(24, 0, 0).is_err());
        assert!(Schedule::daily(8, 0, 86400).is_err());
        assert!(Schedule::every(Duration::ZERO).is_err());
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn interval_errors_continue_cancel_stops_and_closed_scope_rejects_work() {
        let scope = PluginTaskScope::new("clock-test");
        let count = Arc::new(AtomicUsize::new(0));
        let n = count.clone();
        let task = scope
            .schedule(
                "retry",
                Schedule::every(Duration::from_secs(10)).unwrap(),
                move || {
                    n.fetch_add(1, Ordering::SeqCst);
                    async { anyhow::bail!("模拟失败") }
                },
            )
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        for expected in 1..=2 {
            tokio::time::advance(Duration::from_secs(10)).await;
            tokio::task::yield_now().await;
            assert_eq!(count.load(Ordering::SeqCst), expected);
        }
        task.cancel();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        scope.shutdown().await;
        assert!(!scope.try_spawn("late", async { panic!("不应执行") }));
        assert!(
            scope
                .schedule(
                    "late",
                    Schedule::every(Duration::from_secs(1)).unwrap(),
                    || async { Ok(()) }
                )
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_running_callback_and_slow_jobs_do_not_overlap() {
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let scope = PluginTaskScope::new("slow-test");
        let running = Arc::new(AtomicUsize::new(0));
        let n = running.clone();
        scope
            .schedule(
                "slow",
                Schedule::every(Duration::from_secs(1)).unwrap(),
                move || {
                    let n = n.clone();
                    async move {
                        assert_eq!(n.fetch_add(1, Ordering::SeqCst), 0);
                        let _guard = Guard(n);
                        std::future::pending::<()>().await;
                        Ok(())
                    }
                },
            )
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(running.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(running.load(Ordering::SeqCst), 1);
        scope.shutdown().await;
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }
}
