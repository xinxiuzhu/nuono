//! 从有界插件集合中采集可公开展示在系统状态卡片上的运行指标。

use super::*;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use nuono_plugin::status::{PluginStatusMetric, PluginStatusRequest, validate_status_metrics};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use tokio::time::{Instant, timeout, timeout_at};

pub const MAX_PLUGIN_STATUS_ROWS: usize = 24;
const MAX_PLUGIN_STATUS_CANDIDATES: usize = 256;
const STATUS_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatusAvailability {
    Available,
    TimedOut,
    Failed,
    Invalid,
    Busy,
}

#[derive(Debug, Clone)]
pub struct PluginStatusRow {
    pub name: String,
    pub display_name: String,
    pub metrics: Vec<PluginStatusMetric>,
    pub availability: PluginStatusAvailability,
}

#[derive(Clone, Copy)]
struct StatusBudget {
    lock_wait: Duration,
    callback: Duration,
    total: Duration,
}

impl Default for StatusBudget {
    fn default() -> Self {
        Self {
            lock_wait: Duration::from_millis(100),
            callback: Duration::from_millis(400),
            total: Duration::from_millis(1500),
        }
    }
}

impl PluginHost {
    /// 从当前全局启用的插件中收集公开聚合指标。
    /// 未提供状态指标的插件将被忽略；采集失败仅标记为不可用，绝不把插件内部错误文本暴露到公开状态卡片上。
    pub async fn status_rows(&self) -> Vec<PluginStatusRow> {
        self.status_rows_at(Utc::now()).await
    }

    /// 使用与框架状态快照相同的时间戳。
    pub async fn status_rows_at(&self, generated_at: DateTime<Utc>) -> Vec<PluginStatusRow> {
        let budget = StatusBudget::default();
        let deadline = Instant::now() + budget.total;
        // 先释放 plugin_registry 读锁，再去获取 plugins 集合锁（生命周期变更路径可能按相反顺序拿这两把锁，避免死锁）。
        let enabled = match timeout(budget.lock_wait, self.plugin_registry.read()).await {
            Ok(registry) => {
                // 按名称排序只保留有界数量的候选插件名，避免单次请求无上限克隆整个注册表；
                // 候选探测配额与最终可见行数上限分开计算，使默认不产出状态行的插件不会挤占卡片展示名额。
                let mut names = BTreeSet::new();
                for (name, config) in registry.iter() {
                    if config.enabled {
                        names.insert(name.clone());
                        if names.len() > MAX_PLUGIN_STATUS_CANDIDATES {
                            names.pop_last();
                        }
                    }
                }
                names.into_iter().collect()
            }
            Err(_) => return vec![],
        };
        collect_rows(
            &self.plugins,
            enabled,
            &PluginStatusRequest { generated_at },
            budget,
            deadline,
        )
        .await
    }
}

async fn collect_rows(
    plugins: &RwLock<Vec<super::HostedPlugin>>,
    mut enabled: Vec<String>,
    request: &PluginStatusRequest,
    budget: StatusBudget,
    deadline: Instant,
) -> Vec<PluginStatusRow> {
    enabled.sort();
    enabled.dedup();
    enabled.truncate(MAX_PLUGIN_STATUS_CANDIDATES);
    let mut rows: BTreeMap<String, PluginStatusRow> = enabled
        .into_iter()
        .map(|name| {
            let row = PluginStatusRow {
                display_name: name.clone(),
                name: name.clone(),
                metrics: vec![],
                availability: PluginStatusAvailability::Busy,
            };
            (name, row)
        })
        .collect();
    if rows.is_empty() {
        return vec![];
    }
    let lock_deadline = deadline.min(Instant::now() + budget.lock_wait);
    let Ok(plugins) = timeout_at(lock_deadline, plugins.read()).await else {
        return rows.into_values().take(MAX_PLUGIN_STATUS_ROWS).collect();
    };
    let candidates: Vec<_> = plugins
        .iter()
        .filter(|loaded| rows.contains_key(loaded.name()))
        .map(|loaded| (rows[loaded.name()].clone(), loaded.invocation()))
        .collect();
    drop(plugins);
    let matched: HashSet<_> = candidates.iter().map(|(row, _)| row.name.clone()).collect();
    for (name, row) in &mut rows {
        if !matched.contains(name) {
            row.availability = PluginStatusAvailability::Failed;
        }
    }
    let mut callbacks: Vec<futures::future::BoxFuture<'_, PluginStatusRow>> =
        Vec::with_capacity(candidates.len());
    for (mut row, invocation) in candidates {
        callbacks.push(
            async move {
                let Ok(Some(plugin)) = timeout(budget.lock_wait, invocation.read()).await else {
                    return row;
                };
                let metadata = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let display_name = plugin.display_name();
                    if display_name.len() > 128 || display_name.chars().any(char::is_control) {
                        row.name.clone()
                    } else {
                        plugin_display_name(&row.name, display_name)
                    }
                }));
                let Ok(display_name) = metadata else {
                    row.availability = PluginStatusAvailability::Failed;
                    return row;
                };
                row.display_name = display_name;
                let result = timeout(
                    budget.callback,
                    invocation.run(
                        AssertUnwindSafe(async { plugin.status_metrics(request).await })
                            .catch_unwind(),
                    ),
                )
                .await;
                match result {
                    Err(_) => row.availability = PluginStatusAvailability::TimedOut,
                    Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Err(_)))) => {
                        row.availability = PluginStatusAvailability::Failed
                    }
                    Ok(Some(Ok(Ok(metrics)))) => {
                        if validate_status_metrics(&metrics).is_ok() {
                            row.metrics = metrics;
                            row.availability = PluginStatusAvailability::Available;
                        } else {
                            row.availability = PluginStatusAvailability::Invalid;
                        }
                    }
                }
                row
            }
            .boxed(),
        );
    }
    let mut pending = futures::stream::iter(callbacks).buffer_unordered(STATUS_CONCURRENCY);
    while let Ok(Some(row)) = timeout_at(deadline, pending.next()).await {
        if row.availability == PluginStatusAvailability::Available && row.metrics.is_empty() {
            rows.remove(&row.name);
        } else {
            rows.insert(row.name.clone(), row);
        }
    }
    rows.into_values().take(MAX_PLUGIN_STATUS_ROWS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    enum Behavior {
        Metrics,
        Empty,
        Pending,
        Panic,
        Invalid,
        MetadataPanic,
    }

    struct StatusPlugin {
        name: String,
        behavior: Behavior,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Plugin for StatusPlugin {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            if matches!(self.behavior, Behavior::MetadataPanic) {
                panic!("untrusted metadata");
            }
            "公开统计"
        }
        fn version(&self) -> &str {
            "1"
        }
        fn description(&self) -> &str {
            "status fixture"
        }
        async fn status_metrics(
            &self,
            _: &PluginStatusRequest,
        ) -> anyhow::Result<Vec<PluginStatusMetric>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                Behavior::Metrics => Ok(vec![PluginStatusMetric::new("账号总量", "123")]),
                Behavior::Empty => Ok(vec![]),
                Behavior::Pending => std::future::pending().await,
                Behavior::Panic => panic!("private panic text"),
                Behavior::Invalid => Ok(vec![PluginStatusMetric::new("secret\n", "invalid")]),
                Behavior::MetadataPanic => unreachable!(),
            }
        }
    }

    fn plugin(
        name: &str,
        behavior: Behavior,
        calls: &Arc<AtomicUsize>,
    ) -> super::super::HostedPlugin {
        LoadedPlugin {
            plugin: Box::new(StatusPlugin {
                name: name.into(),
                behavior,
                calls: calls.clone(),
            }),
        }
        .into()
    }

    fn budget() -> StatusBudget {
        StatusBudget {
            lock_wait: Duration::from_millis(10),
            callback: Duration::from_millis(20),
            total: Duration::from_millis(200),
        }
    }

    async fn collect(
        plugins: &RwLock<Vec<super::HostedPlugin>>,
        names: &[&str],
        budget: StatusBudget,
    ) -> Vec<PluginStatusRow> {
        collect_rows(
            plugins,
            names.iter().map(|name| (*name).to_owned()).collect(),
            &PluginStatusRequest {
                generated_at: Utc::now(),
            },
            budget,
            Instant::now() + budget.total,
        )
        .await
    }

    #[tokio::test]
    async fn public_status_omits_disabled_and_empty_plugins() {
        let calls = Arc::new(AtomicUsize::new(0));
        let plugins = RwLock::new(vec![
            plugin("enabled", Behavior::Metrics, &calls),
            plugin("disabled", Behavior::Metrics, &calls),
            plugin("empty", Behavior::Empty, &calls),
        ]);
        let rows = collect(&plugins, &["enabled", "empty"], budget()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "enabled");
        assert_eq!(rows[0].metrics[0].value, "123");
        assert_eq!(rows[0].availability, PluginStatusAvailability::Available);
    }

    #[tokio::test]
    async fn public_status_empty_plugins_do_not_hide_a_later_metrics_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut names: Vec<_> = (0..MAX_PLUGIN_STATUS_ROWS)
            .map(|index| format!("empty{index:02}"))
            .collect();
        let mut loaded: Vec<_> = names
            .iter()
            .map(|name| plugin(name, Behavior::Empty, &calls))
            .collect();
        names.push("z_provider".into());
        loaded.push(plugin("z_provider", Behavior::Metrics, &calls));
        let rows = collect(
            &RwLock::new(loaded),
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            budget(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), MAX_PLUGIN_STATUS_ROWS + 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "z_provider");
        assert_eq!(rows[0].metrics[0].value, "123");
    }

    #[tokio::test]
    async fn public_status_applies_visible_row_cap_after_collecting_metrics() {
        let calls = Arc::new(AtomicUsize::new(0));
        let names: Vec<_> = (0..32).map(|index| format!("provider{index:02}")).collect();
        let loaded = names
            .iter()
            .map(|name| plugin(name, Behavior::Metrics, &calls))
            .collect();
        let rows = collect(
            &RwLock::new(loaded),
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            budget(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 32);
        assert_eq!(rows.len(), MAX_PLUGIN_STATUS_ROWS);
        assert_eq!(rows.last().unwrap().name, "provider23");
        assert!(
            rows.iter()
                .all(|row| row.availability == PluginStatusAvailability::Available)
        );
    }

    #[tokio::test]
    async fn public_status_bounds_examined_candidates_even_when_all_are_empty() {
        let calls = Arc::new(AtomicUsize::new(0));
        let names: Vec<_> = (0..MAX_PLUGIN_STATUS_CANDIDATES + 16)
            .map(|index| format!("empty{index:03}"))
            .collect();
        let loaded = names
            .iter()
            .map(|name| plugin(name, Behavior::Empty, &calls))
            .collect();
        let rows = collect(
            &RwLock::new(loaded),
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            budget(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), MAX_PLUGIN_STATUS_CANDIDATES);
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn public_status_isolates_panic_timeout_and_invalid_metrics() {
        let calls = Arc::new(AtomicUsize::new(0));
        let plugins = RwLock::new(vec![
            plugin("panic", Behavior::Panic, &calls),
            plugin("pending", Behavior::Pending, &calls),
            plugin("invalid", Behavior::Invalid, &calls),
            plugin("metadata", Behavior::MetadataPanic, &calls),
        ]);
        let rows = collect(
            &plugins,
            &["panic", "pending", "invalid", "metadata"],
            budget(),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row.metrics.is_empty()));
        let states: BTreeMap<_, _> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.availability))
            .collect();
        assert_eq!(states["panic"], PluginStatusAvailability::Failed);
        assert_eq!(states["metadata"], PluginStatusAvailability::Failed);
        assert_eq!(states["pending"], PluginStatusAvailability::TimedOut);
        assert_eq!(states["invalid"], PluginStatusAvailability::Invalid);
    }

    #[tokio::test]
    async fn public_status_does_not_wait_for_a_lifecycle_write_lock() {
        let calls = Arc::new(AtomicUsize::new(0));
        let plugins = RwLock::new(vec![plugin("busy", Behavior::Metrics, &calls)]);
        let lease = plugins.write().await;
        let rows = collect(&plugins, &["busy"], budget()).await;
        assert_eq!(rows[0].availability, PluginStatusAvailability::Busy);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(lease);
    }

    #[tokio::test]
    async fn public_status_bounds_total_time_concurrency_and_rows() {
        let calls = Arc::new(AtomicUsize::new(0));
        let names: Vec<_> = (0..32).map(|index| format!("p{index:02}")).collect();
        let plugins = RwLock::new(
            names
                .iter()
                .map(|name| plugin(name, Behavior::Pending, &calls))
                .collect(),
        );
        let budget = StatusBudget {
            callback: Duration::from_secs(1),
            total: Duration::from_millis(20),
            ..budget()
        };
        let rows = collect(
            &plugins,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
            budget,
        )
        .await;
        assert_eq!(rows.len(), MAX_PLUGIN_STATUS_ROWS);
        assert_eq!(calls.load(Ordering::SeqCst), STATUS_CONCURRENCY);
        assert!(
            rows.iter()
                .all(|row| row.availability == PluginStatusAvailability::Busy)
        );
        assert!(
            plugins.try_write().is_ok(),
            "status collection released its lease"
        );
    }
}
