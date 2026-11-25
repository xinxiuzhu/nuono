//! 宿主实际进入执行的插件回调进程内诊断统计。
//!
//! 统计数据在插件启停与重载期间保留，仅随宿主进程重启而清零。
//! 前置过滤、触发匹配、元数据读取以及插件自建后台任务均不计入回调统计。
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{PluginHost, PluginScopedTool};

pub use nuono_storage::plugins::MAX_SLOW_PLUGIN_THRESHOLD_MS;
pub const SLOW_PLUGIN_THRESHOLD_MS: u64 = nuono_storage::plugins::DEFAULT_SLOW_PLUGIN_THRESHOLD_MS;
const SLOW_WARNING_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = core::plugin_host::PluginSlowCall)]
pub struct PluginSlowCall {
    pub callback: String,
    pub duration_ms: f64,
    pub finished_at: String,
}

/// 当前进程生命周期内的插件回调统计快照。耗时统计涵盖所有已完成调用（含报错及 `.await` 等待时间），
/// 被取消的调用单独记数，满足 `calls = completed_calls + cancelled_calls + in_flight`。
/// 慢调用计数（`slow_calls`）仅统计已完成的调用，未完成的调用反映在 `in_flight` 中。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = core::plugin_host::PluginDiagnostics)]
pub struct PluginDiagnostics {
    pub calls: u64,
    pub completed_calls: u64,
    pub failures: u64,
    pub cancelled_calls: u64,
    pub in_flight: u64,
    pub total_duration_ms: f64,
    pub average_duration_ms: f64,
    pub max_duration_ms: f64,
    pub last_duration_ms: Option<f64>,
    pub slow_calls: u64,
    pub slow_threshold_ms: u64,
    pub last_slow: Option<PluginSlowCall>,
}

impl Default for PluginDiagnostics {
    fn default() -> Self {
        Self {
            calls: 0,
            completed_calls: 0,
            failures: 0,
            cancelled_calls: 0,
            in_flight: 0,
            total_duration_ms: 0.0,
            average_duration_ms: 0.0,
            max_duration_ms: 0.0,
            last_duration_ms: None,
            slow_calls: 0,
            slow_threshold_ms: SLOW_PLUGIN_THRESHOLD_MS,
            last_slow: None,
        }
    }
}

#[derive(Default)]
pub(super) struct DiagnosticsRegistry {
    plugins: Mutex<HashMap<String, Arc<PluginMetrics>>>,
}

impl DiagnosticsRegistry {
    pub(super) fn for_plugin(&self, name: &str) -> Arc<PluginMetrics> {
        let mut plugins = self
            .plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(metrics) = plugins.get(name) {
            return metrics.clone();
        }
        plugins
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(PluginMetrics::new(name)))
            .clone()
    }

    fn snapshot(&self, name: &str) -> PluginDiagnostics {
        let metrics = self
            .plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned();
        metrics
            .map(|metrics| metrics.snapshot())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct MetricsState {
    snapshot: PluginDiagnostics,
    last_warning: Option<Instant>,
}

pub(super) struct PluginMetrics {
    plugin_name: String,
    state: Mutex<MetricsState>,
}

impl PluginMetrics {
    fn new(name: &str) -> Self {
        Self {
            plugin_name: name.to_owned(),
            state: Mutex::new(MetricsState::default()),
        }
    }

    pub(super) fn snapshot(&self) -> PluginDiagnostics {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot
            .clone()
    }

    fn set_slow_threshold(&self, threshold_ms: u64) -> PluginDiagnostics {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.snapshot.slow_threshold_ms != threshold_ms {
            state.snapshot.slow_threshold_ms = threshold_ms;
            // 历史耗时统计仍保留参考价值，但由于原有的慢调用次数是按旧阈值统计的，更新阈值时需将慢调用计数清零重算。
            state.snapshot.slow_calls = 0;
            state.snapshot.last_slow = None;
            state.last_warning = None;
        }
        state.snapshot.clone()
    }

    pub(super) fn begin<'a>(&'a self, callback: &'a str) -> CallGuard<'a> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.snapshot.calls = state.snapshot.calls.saturating_add(1);
        state.snapshot.in_flight = state.snapshot.in_flight.saturating_add(1);
        CallGuard {
            metrics: self,
            callback,
            started: Instant::now(),
            finished: false,
        }
    }

    pub(super) async fn observe<F, T, E>(&self, callback: &str, future: F) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let guard = self.begin(callback);
        let result = nuono_plugin::core::log_context::scope(
            Some(self.plugin_name.clone()),
            Box::pin(future),
        )
        .await;
        guard.finish(result.is_err());
        result
    }

    /// 记录单次调用完成；若超过慢调用阈值则返回触发告警时的阈值快照。
    /// 锁内不执行任何 tracing 日志输出，且后续阈值变更不会篡改本次告警日志的字段。
    fn complete(
        &self,
        callback: &str,
        elapsed: Duration,
        failed: bool,
        now: Instant,
    ) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let duration_ms = elapsed.as_secs_f64() * 1_000.0;
        let snapshot = &mut state.snapshot;
        snapshot.in_flight = snapshot.in_flight.saturating_sub(1);
        snapshot.completed_calls = snapshot.completed_calls.saturating_add(1);
        snapshot.failures = snapshot.failures.saturating_add(u64::from(failed));
        snapshot.total_duration_ms += duration_ms;
        snapshot.average_duration_ms = snapshot.total_duration_ms / snapshot.completed_calls as f64;
        snapshot.max_duration_ms = snapshot.max_duration_ms.max(duration_ms);
        snapshot.last_duration_ms = Some(duration_ms);
        let threshold_ms = snapshot.slow_threshold_ms;
        if threshold_ms == 0 || elapsed < Duration::from_millis(threshold_ms) {
            return None;
        }
        snapshot.slow_calls = snapshot.slow_calls.saturating_add(1);
        snapshot.last_slow = Some(PluginSlowCall {
            callback: callback.chars().take(256).collect(),
            duration_ms,
            finished_at: crate::clock::format_wire_ts(chrono::Utc::now()),
        });
        if state
            .last_warning
            .is_some_and(|last| now.duration_since(last) < SLOW_WARNING_INTERVAL)
        {
            return None;
        }
        state.last_warning = Some(now);
        Some(threshold_ms)
    }

    fn cancelled(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.snapshot.in_flight = state.snapshot.in_flight.saturating_sub(1);
        state.snapshot.cancelled_calls = state.snapshot.cancelled_calls.saturating_add(1);
    }
}

pub(super) struct CallGuard<'a> {
    metrics: &'a PluginMetrics,
    callback: &'a str,
    started: Instant,
    finished: bool,
}

impl CallGuard<'_> {
    pub(super) fn finish(mut self, failed: bool) {
        self.record_completion(failed);
    }

    fn record_completion(&mut self, failed: bool) {
        self.finished = true;
        let elapsed = self.started.elapsed();
        if let Some(threshold_ms) =
            self.metrics
                .complete(self.callback, elapsed, failed, Instant::now())
        {
            tracing::warn!(
                target: "nuono::plugins",
                plugin = %self.metrics.plugin_name,
                callback = %self.callback.chars().take(256).collect::<String>(),
                duration_ms = elapsed.as_secs_f64() * 1_000.0,
                slow_threshold_ms = threshold_ms,
                "插件慢调用；异步等待也计入耗时"
            );
        }
    }
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            if std::thread::panicking() {
                // 仅在栈展开（unwinding）经过时记录一次失败完成，不拦截 panic，也不改变外层调用方的 catch_unwind 边界。
                self.record_completion(true);
            } else {
                self.metrics.cancelled();
            }
        }
    }
}

impl PluginHost {
    /// 汇总进程内所有插件（包括当前已停用插件的历史调用）的回调统计总数。
    pub(crate) fn callback_totals(&self) -> PluginDiagnostics {
        let metrics: Vec<_> = self
            .diagnostics
            .plugins
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect();
        let mut total = PluginDiagnostics::default();
        for metrics in metrics {
            let snapshot = metrics.snapshot();
            total.calls = total.calls.saturating_add(snapshot.calls);
            total.completed_calls = total
                .completed_calls
                .saturating_add(snapshot.completed_calls);
            total.in_flight = total.in_flight.saturating_add(snapshot.in_flight);
            total.failures = total.failures.saturating_add(snapshot.failures);
            total.cancelled_calls = total
                .cancelled_calls
                .saturating_add(snapshot.cancelled_calls);
            total.total_duration_ms += snapshot.total_duration_ms;
            total.max_duration_ms = total.max_duration_ms.max(snapshot.max_duration_ms);
        }
        if total.completed_calls > 0 {
            total.average_duration_ms = total.total_duration_ms / total.completed_calls as f64;
        }
        total
    }

    /// 获取指定插件的一致同步诊断快照；从未调用或不存在的插件返回全零默认快照。
    pub fn plugin_diagnostics(&self, plugin_name: &str) -> PluginDiagnostics {
        self.diagnostics.snapshot(plugin_name)
    }

    /// 更新指定插件在宿主侧的慢调用告警阈值；若持久化失败则保持原阈值与统计数据不变，成功后即时生效无需重启插件。
    pub async fn set_plugin_slow_threshold(
        &self,
        plugin_name: &str,
        slow_threshold_ms: u64,
    ) -> crate::error::Result<PluginDiagnostics> {
        if slow_threshold_ms > MAX_SLOW_PLUGIN_THRESHOLD_MS {
            return Err(crate::error::NuonoError::Validation(format!(
                "慢调用阈值必须为 0 至 {MAX_SLOW_PLUGIN_THRESHOLD_MS} 毫秒"
            )));
        }
        let _writer = self.diagnostic_policy_writer.lock().await;
        if !self.is_plugin_installed(plugin_name).await {
            return Err(crate::error::NuonoError::PluginNotFound(
                plugin_name.to_owned(),
            ));
        }
        if !self
            .db
            .set_plugin_slow_threshold(plugin_name, slow_threshold_ms)
            .await?
        {
            return Err(crate::error::NuonoError::PluginNotFound(
                plugin_name.to_owned(),
            ));
        }
        // 数据库提交成功后同步更新内存阈值（中间不跨越 .await），在释放写互斥锁前完成发布。
        Ok(self
            .diagnostic_metrics(plugin_name)
            .set_slow_threshold(slow_threshold_ms))
    }

    /// 在首次执行插件生命周期回调前加载慢调用阈值（包括已停用或校验被拒的候选插件）；
    /// 插件重载时会直接沿用内存中已有的诊断状态，不再重复查库覆盖。
    pub(super) async fn load_plugin_slow_threshold(
        &self,
        plugin_name: &str,
    ) -> crate::error::Result<()> {
        let _writer = self.diagnostic_policy_writer.lock().await;
        let threshold_ms = self
            .db
            .get_plugin_slow_threshold(plugin_name)
            .await?
            .unwrap_or(SLOW_PLUGIN_THRESHOLD_MS);
        self.diagnostic_metrics(plugin_name)
            .set_slow_threshold(threshold_ms);
        Ok(())
    }

    pub(super) fn diagnostic_metrics(&self, plugin_name: &str) -> Arc<PluginMetrics> {
        self.diagnostics.for_plugin(plugin_name)
    }
}

struct ObservedTriggerHandler {
    inner: Arc<dyn crate::core::TriggerHandler>,
    metrics: Arc<PluginMetrics>,
    callback: String,
    epoch: Option<super::InvocationEpoch>,
}

#[async_trait::async_trait]
impl crate::core::TriggerHandler for ObservedTriggerHandler {
    async fn handle(
        &self,
        bot: &crate::api::Bot,
        event: &crate::onebot::Message,
        data: crate::core::MatchData,
    ) -> anyhow::Result<()> {
        let call = self
            .metrics
            .observe(&self.callback, self.inner.handle(bot, event, data));
        match &self.epoch {
            Some(epoch) => epoch
                .run(call)
                .await
                .unwrap_or_else(|| Err(anyhow::anyhow!("插件调用代次已撤销"))),
            None => call.await,
        }
    }
}

struct ObservedCommandHandler {
    inner: Arc<dyn crate::core::CommandHandler>,
    metrics: Arc<PluginMetrics>,
    callback: String,
    epoch: Option<super::InvocationEpoch>,
}

#[async_trait::async_trait]
impl crate::core::CommandHandler for ObservedCommandHandler {
    async fn handle(
        &self,
        bot: &crate::api::Bot,
        event: &crate::plugin::Event<'_>,
        data: crate::core::MatchData,
    ) -> anyhow::Result<()> {
        let call = self
            .metrics
            .observe(&self.callback, self.inner.handle(bot, event, data));
        match &self.epoch {
            Some(epoch) => epoch
                .run(call)
                .await
                .unwrap_or_else(|| Err(anyhow::anyhow!("插件调用代次已撤销"))),
            None => call.await,
        }
    }
}

pub(super) fn observe_trigger(
    mut trigger: crate::core::Trigger,
    metrics: Arc<PluginMetrics>,
    epoch: Option<super::InvocationEpoch>,
) -> crate::core::Trigger {
    let callback = format!("trigger:{:?}:{}", trigger.trigger_type, trigger.pattern);
    // 跨平台 Event 触发器内部带有一个占位用的旧版 message handler，不能将其包装计入回调统计。
    if let Some(inner) = trigger.event_handler.take() {
        trigger.event_handler = Some(Arc::new(ObservedCommandHandler {
            inner,
            metrics,
            callback,
            epoch,
        }));
    } else {
        trigger.handler = Arc::new(ObservedTriggerHandler {
            inner: trigger.handler,
            metrics,
            callback,
            epoch,
        });
    }
    trigger
}

pub(super) fn observe_command(
    mut command: crate::core::Command,
    metrics: Arc<PluginMetrics>,
    epoch: Option<super::InvocationEpoch>,
) -> crate::core::Command {
    let callback = format!("command:{}", command.name);
    if let Some(inner) = command.handler.take() {
        command.handler = Some(Arc::new(ObservedCommandHandler {
            inner,
            metrics,
            callback,
            epoch,
        }));
    } else if let Some(inner) = command.message_handler.take() {
        command.message_handler = Some(Arc::new(ObservedTriggerHandler {
            inner,
            metrics,
            callback,
            epoch,
        }));
    }
    command
}

pub(super) fn observe_tool(
    inner: Arc<dyn super::AgentTool>,
    metrics: Arc<PluginMetrics>,
    epoch: Option<super::InvocationEpoch>,
) -> Arc<dyn super::AgentTool> {
    Arc::new(PluginScopedTool {
        scoped_name: inner.name().to_owned(),
        inner,
        diagnostics: Some(metrics),
        epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn callbacks_keep_distinct_log_owners_across_awaits() {
        use nuono_plugin::core::log_context;
        let first = PluginMetrics::new("DemoPlugin");
        let second = PluginMetrics::new("OtherPlugin");
        let check = |name: &'static str| async move {
            assert_eq!(log_context::current_plugin().as_deref(), Some(name));
            tokio::task::yield_now().await;
            assert_eq!(log_context::current_plugin().as_deref(), Some(name));
            Ok::<_, ()>(())
        };
        let (a, b) = tokio::join!(
            first.observe("on_message_listen", check("DemoPlugin")),
            second.observe("on_load", check("OtherPlugin")),
        );
        a.unwrap();
        b.unwrap();
        assert!(log_context::current_plugin().is_none());
    }

    #[test]
    fn zero_snapshot_and_serialized_contract_are_finite_and_do_not_create_entries() {
        let registry = DiagnosticsRegistry::default();
        let snapshot = registry.snapshot("unknown");
        assert_eq!(snapshot, PluginDiagnostics::default());
        assert!(registry.plugins.lock().unwrap().is_empty());
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["slow_threshold_ms"], 1_000);
        assert_eq!(json["average_duration_ms"], 0.0);
        assert!(json["last_duration_ms"].is_null());
        assert!(json["last_slow"].is_null());
    }

    #[test]
    fn duration_failure_and_slow_threshold_use_completed_calls_and_warning_is_rate_limited() {
        let metrics = PluginMetrics::new("threshold");
        let now = Instant::now();
        // 耗时未超过阈值的正常调用不会占用慢调用告警限流配额。
        std::mem::forget(metrics.begin("below"));
        assert_eq!(
            metrics.complete("below", Duration::from_micros(999_999), false, now),
            None
        );
        std::mem::forget(metrics.begin("at"));
        assert_eq!(
            metrics.complete("at", Duration::from_millis(1_000), true, now),
            Some(1_000)
        );
        std::mem::forget(metrics.begin("above"));
        assert_eq!(
            metrics.complete(
                "above",
                Duration::from_millis(2_000),
                false,
                now + Duration::from_secs(29)
            ),
            None
        );
        std::mem::forget(metrics.begin("later"));
        assert_eq!(
            metrics.complete(
                "later",
                Duration::from_millis(3_000),
                false,
                now + Duration::from_secs(30)
            ),
            Some(1_000)
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.calls, 4);
        assert_eq!(snapshot.completed_calls, 4);
        assert_eq!(snapshot.failures, 1);
        assert_eq!(snapshot.in_flight, 0);
        assert_eq!(snapshot.slow_calls, 3);
        assert_eq!(snapshot.max_duration_ms, 3_000.0);
        assert_eq!(snapshot.last_duration_ms, Some(3_000.0));
        assert!((snapshot.total_duration_ms - 6_999.999).abs() < 0.001);
        assert_eq!(
            snapshot.average_duration_ms,
            snapshot.total_duration_ms / 4.0
        );
        let last = snapshot.last_slow.unwrap();
        assert_eq!(last.callback, "later");
        assert!(last.finished_at.parse::<i64>().is_ok());
    }

    #[test]
    fn each_plugin_policy_is_independent_and_zero_disables_only_slow_recognition() {
        let registry = DiagnosticsRegistry::default();
        let network = registry.for_plugin("network");
        let local = registry.for_plugin("local");
        network.set_slow_threshold(30_000);
        let now = Instant::now();
        std::mem::forget(network.begin("fetch"));
        std::mem::forget(local.begin("command"));
        assert_eq!(
            network.complete("fetch", Duration::from_secs(2), false, now),
            None
        );
        assert_eq!(
            local.complete("command", Duration::from_secs(2), false, now),
            Some(1_000)
        );
        assert_eq!(network.snapshot().slow_calls, 0);
        assert_eq!(local.snapshot().slow_calls, 1);
        network.set_slow_threshold(0);
        std::mem::forget(network.begin("generate"));
        assert_eq!(
            network.complete("generate", Duration::from_secs(100), true, now),
            None
        );
        let snapshot = network.snapshot();
        assert_eq!(snapshot.slow_threshold_ms, 0);
        assert_eq!(snapshot.calls, 2);
        assert_eq!(snapshot.completed_calls, 2);
        assert_eq!(snapshot.failures, 1);
        assert_eq!(snapshot.total_duration_ms, 102_000.0);
        assert_eq!(snapshot.max_duration_ms, 100_000.0);
        assert_eq!(snapshot.slow_calls, 0);
        assert_eq!(snapshot.last_slow, None);
    }

    #[test]
    fn changed_threshold_clears_only_slow_state_and_completion_uses_current_policy() {
        let metrics = PluginMetrics::new("policy");
        let now = Instant::now();
        std::mem::forget(metrics.begin("old"));
        assert_eq!(
            metrics.complete("old", Duration::from_secs(2), true, now),
            Some(1_000)
        );
        let original = metrics.snapshot();
        assert_eq!(
            metrics.set_slow_threshold(1_000),
            original,
            "same-value save keeps history"
        );
        std::mem::forget(metrics.begin("in-flight"));
        let changed = metrics.set_slow_threshold(3_000);
        assert_eq!(changed.calls, 2);
        assert_eq!(changed.completed_calls, 1);
        assert_eq!(changed.failures, 1);
        assert_eq!(changed.in_flight, 1);
        assert_eq!(changed.total_duration_ms, original.total_duration_ms);
        assert_eq!(changed.slow_calls, 0);
        assert_eq!(changed.last_slow, None);
        assert_eq!(
            metrics.complete("in-flight", Duration::from_secs(2), false, now),
            None
        );
        std::mem::forget(metrics.begin("new"));
        assert_eq!(
            metrics.complete("new", Duration::from_secs(3), false, now),
            Some(3_000),
            "changed policy resets the warning budget"
        );
        assert_eq!(metrics.snapshot().slow_calls, 1);
        assert_eq!(metrics.snapshot().calls, 3);
        assert_eq!(metrics.snapshot().completed_calls, 3);
        assert_eq!(metrics.snapshot().in_flight, 0);
        metrics.set_slow_threshold(MAX_SLOW_PLUGIN_THRESHOLD_MS);
        std::mem::forget(metrics.begin("max"));
        assert_eq!(
            metrics.complete(
                "max",
                Duration::from_millis(MAX_SLOW_PLUGIN_THRESHOLD_MS),
                false,
                now
            ),
            Some(MAX_SLOW_PLUGIN_THRESHOLD_MS)
        );
    }

    #[tokio::test]
    async fn errors_and_caught_panics_count_once_and_preserve_the_error() {
        let metrics = PluginMetrics::new("failures");
        let error = metrics
            .observe("error", async { Err::<(), _>("exact error") })
            .await
            .unwrap_err();
        assert_eq!(error, "exact error");
        let result = metrics
            .observe(
                "caught",
                super::super::PluginHost::call_with_catch(async {
                    panic!("caught panic");
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                }),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("caught panic"));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.calls, 2);
        assert_eq!(snapshot.completed_calls, 2);
        assert_eq!(snapshot.failures, 2);
        assert_eq!(snapshot.cancelled_calls, 0);
        assert_eq!(snapshot.in_flight, 0);
    }

    #[tokio::test]
    async fn uncaught_panic_keeps_unwind_and_counts_failure_instead_of_cancellation() {
        let metrics = PluginMetrics::new("unwind");
        let result = std::panic::AssertUnwindSafe(metrics.observe("panic", async {
            panic!("original panic");
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        }))
        .catch_unwind()
        .await;
        assert!(result.is_err());
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.failures, 1);
        assert_eq!(snapshot.completed_calls, 1);
        assert_eq!(snapshot.cancelled_calls, 0);
        assert_eq!(snapshot.in_flight, 0);
    }

    #[tokio::test]
    async fn abort_cleans_in_flight_without_failure_or_completed_duration() {
        let metrics = Arc::new(PluginMetrics::new("cancelled"));
        let task_metrics = metrics.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            task_metrics
                .observe("pending", async {
                    entered_tx.send(()).unwrap();
                    std::future::pending::<Result<(), ()>>().await
                })
                .await
        });
        entered_rx.await.unwrap();
        assert_eq!(metrics.snapshot().in_flight, 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.calls, 1);
        assert_eq!(snapshot.cancelled_calls, 1);
        assert_eq!(snapshot.completed_calls, 0);
        assert_eq!(snapshot.failures, 0);
        assert_eq!(snapshot.in_flight, 0);
        assert_eq!(snapshot.total_duration_ms, 0.0);
        assert_eq!(snapshot.last_duration_ms, None);
    }

    #[tokio::test]
    async fn an_unpolled_callback_is_not_counted() {
        let metrics = PluginMetrics::new("unpolled");
        let future = metrics.observe("unused", async { Ok::<_, ()>(()) });
        drop(future);
        assert_eq!(metrics.snapshot(), PluginDiagnostics::default());
    }

    #[test]
    fn concurrent_calls_and_snapshots_keep_totals_consistent() {
        let registry = Arc::new(DiagnosticsRegistry::default());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let registry = registry.clone();
                scope.spawn(move || {
                    for i in 0..100 {
                        let metrics = registry.for_plugin("shared");
                        let guard = metrics.begin("parallel");
                        let snapshot = registry.snapshot("shared");
                        assert_eq!(
                            snapshot.calls,
                            snapshot.completed_calls
                                + snapshot.cancelled_calls
                                + snapshot.in_flight
                        );
                        if i % 4 == 0 {
                            drop(guard);
                        } else {
                            guard.finish(i % 4 == 1);
                        }
                    }
                });
            }
        });
        let snapshot = registry.snapshot("shared");
        assert_eq!(snapshot.calls, 800);
        assert_eq!(snapshot.completed_calls, 600);
        assert_eq!(snapshot.cancelled_calls, 200);
        assert_eq!(snapshot.failures, 200);
        assert_eq!(snapshot.in_flight, 0);
    }
}
