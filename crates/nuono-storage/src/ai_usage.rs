//! 模型服务商上报的 AI 用量事件异步有界落盘队列，并在服务关闭时排空待写数据。
use crate::{
    db::Database,
    entity::ai_usage_events,
    error::{NuonoError, Result},
};
use chrono::{DateTime, Duration, Utc};
use nuono_ai::usage::{UsageEvent, UsageObserver};
use sea_orm::{
    AccessMode, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, IsolationLevel,
    QueryResult, Set, Statement, TransactionTrait, sea_query::OnConflict,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 4096;
const TOKEN_FIELDS: [&str; 5] = [
    "input",
    "output",
    "total",
    "cached_input",
    "reasoning_output",
];

#[derive(Debug)]
enum Command {
    Record(Box<UsageEvent>),
    Flush(oneshot::Sender<()>),
}

#[derive(Debug)]
pub(crate) struct UsageWriter {
    tx: Mutex<Option<mpsc::Sender<Command>>>,
    handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    lost: Arc<AtomicU64>,
}
impl UsageWriter {
    pub(crate) fn start(conn: DatabaseConnection, ai_ready: Arc<AtomicBool>) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel(QUEUE_CAPACITY);
        let lost = Arc::new(AtomicU64::new(0));
        let worker_lost = lost.clone();
        let handle = tokio::spawn(async move {
            let mut retry_after = None;
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Record(event) => {
                        if retry_after.is_some_and(|at| std::time::Instant::now() < at) {
                            worker_lost.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let result =
                            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                if !ai_ready.load(Ordering::Acquire) {
                                    crate::db::migrations::ensure_ai_module(&conn).await?;
                                    ai_ready.store(true, Ordering::Release);
                                }
                                insert(&conn, *event).await
                            })
                            .await;
                        if !matches!(&result, Ok(Ok(()))) {
                            worker_lost.fetch_add(1, Ordering::Relaxed);
                            // 单条格式非法的记录直接丢弃，不能影响后续正常记录的写入；
                            // 只有遇到数据库不可用或超时故障时才进入退避冷却。
                            if !matches!(&result, Ok(Err(NuonoError::Validation(_)))) {
                                retry_after = Some(
                                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                                );
                            }
                            tracing::warn!("AI 用量记录写入失败；统计不包含此请求");
                        }
                    }
                    Command::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            handle: tokio::sync::Mutex::new(Some(handle)),
            lost,
        })
    }
    async fn flush(&self) -> Result<()> {
        let tx = self.tx.lock().ok().and_then(|tx| tx.clone());
        if let Some(tx) = tx {
            let (done, completed) = oneshot::channel();
            tx.send(Command::Flush(done))
                .await
                .map_err(|_| NuonoError::DatabaseQuery("AI 用量记录队列已关闭".into()))?;
            completed
                .await
                .map_err(|_| NuonoError::DatabaseQuery("AI 用量记录队列已退出".into()))?;
        }
        Ok(())
    }
    pub(crate) async fn shutdown(&self) -> Result<()> {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        // 在 await 等待期间将任务句柄保留在共享状态中：
        // 这样多个并发调用方可以共同等待同一次排空完成，且单个调用方中途取消也不会中断后台等待。
        let mut handle = self.handle.lock().await;
        if let Some(worker) = handle.as_mut() {
            let result = worker.await;
            *handle = None;
            result.map_err(|_| NuonoError::DatabaseQuery("等待 AI 用量记录任务失败".into()))?;
        }
        Ok(())
    }
}
impl UsageObserver for UsageWriter {
    fn observe(&self, event: UsageEvent) {
        let accepted = self
            .tx
            .lock()
            .ok()
            .and_then(|tx| {
                tx.as_ref()
                    .map(|tx| tx.try_send(Command::Record(Box::new(event))).is_ok())
            })
            .unwrap_or(false);
        if !accepted {
            self.lost.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("AI 用量记录队列不可用；统计不包含此请求");
        }
    }
}

fn integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| NuonoError::Validation("AI 用量计数超过存储范围".into()))
}
async fn insert(conn: &DatabaseConnection, event: UsageEvent) -> Result<()> {
    let started = integer(event.started_at_ms)?;
    let day = DateTime::from_timestamp_millis(started)
        .ok_or_else(|| NuonoError::Validation("AI 用量时间无效".into()))?
        .format("%Y-%m-%d")
        .to_string();
    for (value, limit) in [
        (&event.request_id, 64),
        (&event.model, 256),
        (&event.context.name, 512),
        (&event.context.provider, 256),
        (&event.context.source, 64),
    ] {
        if value.len() > limit || value.contains('\0') {
            return Err(NuonoError::Validation("AI 用量元数据无效".into()));
        }
    }
    if event.request_id.is_empty() {
        return Err(NuonoError::Validation("AI 用量请求编号缺失".into()));
    }
    let usage = event.usage;
    let row = ai_usage_events::ActiveModel {
        request_id: Set(event.request_id),
        started_at_ms: Set(started),
        day: Set(day),
        model_id: Set(event.context.model_id),
        model: Set(event.model),
        name: Set(event.context.name),
        provider: Set(event.context.provider),
        source: Set(event.context.source),
        operation: Set(event.operation.as_str().into()),
        outcome: Set(event.outcome.as_str().into()),
        elapsed_ms: Set(integer(event.elapsed_ms)?),
        input_tokens: Set(usage.input_tokens.map(integer).transpose()?),
        output_tokens: Set(usage.output_tokens.map(integer).transpose()?),
        total_tokens: Set(usage.total_tokens.map(integer).transpose()?),
        cached_input_tokens: Set(usage.cached_input_tokens.map(integer).transpose()?),
        reasoning_output_tokens: Set(usage.reasoning_output_tokens.map(integer).transpose()?),
    };
    ai_usage_events::Entity::insert(row)
        .on_conflict(
            OnConflict::column(ai_usage_events::Column::RequestId)
                .do_nothing()
                .to_owned(),
        )
        .try_insert()
        .exec(conn)
        .await?;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct UsageCounts {
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub cancelled: u64,
    pub known_usage_requests: u64,
    pub unknown_usage_requests: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub input_reported_requests: u64,
    pub output_reported_requests: u64,
    pub total_reported_requests: u64,
    pub cached_input_reported_requests: u64,
    pub reasoning_output_reported_requests: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct DailyUsage {
    pub date: String,
    #[serde(flatten)]
    pub counts: UsageCounts,
}
#[derive(Clone, Debug, Serialize)]
pub struct ModelUsage {
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    #[serde(default)]
    pub model_id: Option<i64>,
    pub model: String,
    pub name: String,
    pub provider: String,
    pub operation: String,
    pub source: String,
    #[serde(flatten)]
    pub counts: UsageCounts,
}
#[derive(Clone, Debug, Serialize)]
pub struct UsageSummary {
    pub days: u32,
    pub from: String,
    pub to: String,
    pub totals: UsageCounts,
    pub daily: Vec<DailyUsage>,
    pub models: Vec<ModelUsage>,
    pub unrecorded_requests: u64,
}
fn parsed(row: &QueryResult, field: &str) -> Result<u64> {
    let value: String = row.try_get("", field)?;
    value
        .parse()
        .map_err(|_| NuonoError::DatabaseQuery("AI 用量聚合超过计数范围".into()))
}
fn optional(row: &QueryResult, field: &str) -> Result<Option<u64>> {
    let value: Option<String> = row.try_get("", field)?;
    value
        .map(|v| {
            v.parse()
                .map_err(|_| NuonoError::DatabaseQuery("AI 用量聚合超过计数范围".into()))
        })
        .transpose()
}
fn counts(row: &QueryResult) -> Result<UsageCounts> {
    Ok(UsageCounts {
        requests: parsed(row, "requests")?,
        successes: parsed(row, "successes")?,
        failures: parsed(row, "failures")?,
        cancelled: parsed(row, "cancelled")?,
        known_usage_requests: parsed(row, "known_usage_requests")?,
        unknown_usage_requests: parsed(row, "unknown_usage_requests")?,
        input_tokens: optional(row, "input_tokens")?,
        output_tokens: optional(row, "output_tokens")?,
        total_tokens: optional(row, "total_tokens")?,
        cached_input_tokens: optional(row, "cached_input_tokens")?,
        reasoning_output_tokens: optional(row, "reasoning_output_tokens")?,
        input_reported_requests: parsed(row, "input_reported_requests")?,
        output_reported_requests: parsed(row, "output_reported_requests")?,
        total_reported_requests: parsed(row, "total_reported_requests")?,
        cached_input_reported_requests: parsed(row, "cached_input_reported_requests")?,
        reasoning_output_reported_requests: parsed(row, "reasoning_output_reported_requests")?,
    })
}
fn aggregate_sql(group: &str) -> String {
    let any = TOKEN_FIELDS
        .iter()
        .map(|field| format!("{field}_tokens IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut columns = vec!["CAST(COUNT(*) AS TEXT) AS requests".into()];
    for (name, predicate) in [
        ("successes", "outcome='succeeded'".into()),
        ("failures", "outcome='failed'".into()),
        ("cancelled", "outcome='cancelled'".into()),
        ("known_usage_requests", format!("({any})")),
        ("unknown_usage_requests", format!("NOT ({any})")),
    ] {
        columns.push(format!(
            "CAST(COALESCE(SUM(CASE WHEN {predicate} THEN 1 ELSE 0 END),0) AS TEXT) AS {name}"
        ));
    }
    for field in TOKEN_FIELDS {
        columns.push(format!(
            "CAST(SUM({field}_tokens) AS TEXT) AS {field}_tokens"
        ));
        columns.push(format!(
            "CAST(COUNT({field}_tokens) AS TEXT) AS {field}_reported_requests"
        ));
    }
    let prefix = if group.is_empty() {
        String::new()
    } else {
        format!("{group}, ")
    };
    let suffix = if group.is_empty() {
        String::new()
    } else {
        format!(" GROUP BY {group}")
    };
    format!(
        "SELECT {prefix}{} FROM nuono_ai_usage_events WHERE started_at_ms >= $1 AND started_at_ms <= $2{suffix}",
        columns.join(", ")
    )
}
impl Database {
    pub fn ai_usage_observer(&self) -> Arc<dyn UsageObserver> {
        self.ai_usage.clone()
    }
    pub async fn flush_ai_usage(&self) -> Result<()> {
        self.ai_usage.flush().await
    }
    pub async fn ai_usage_summary(&self, days: u32) -> Result<UsageSummary> {
        self.ai_usage_summary_at(days, Utc::now()).await
    }
    pub async fn ai_usage_summary_at(&self, days: u32, now: DateTime<Utc>) -> Result<UsageSummary> {
        if !matches!(days, 7 | 30 | 90) {
            return Err(NuonoError::Validation("统计天数只支持 7、30 或 90".into()));
        }
        self.flush_ai_usage().await?;
        let from = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            - Duration::days(i64::from(days) - 1);
        if !self.is_ai_module_installed().await? {
            let daily = (0..days)
                .map(|day| DailyUsage {
                    date: (from + Duration::days(i64::from(day)))
                        .format("%Y-%m-%d")
                        .to_string(),
                    counts: UsageCounts::default(),
                })
                .collect();
            return Ok(UsageSummary {
                days,
                from: from.format("%Y-%m-%d").to_string(),
                to: now.format("%Y-%m-%d").to_string(),
                totals: UsageCounts::default(),
                daily,
                models: Vec::new(),
                unrecorded_requests: self.ai_usage.lost.load(Ordering::Relaxed),
            });
        }
        let tx = if self.backend() == DbBackend::Postgres {
            self.orm()
                .begin_with_config(
                    Some(IsolationLevel::RepeatableRead),
                    Some(AccessMode::ReadOnly),
                )
                .await?
        } else {
            self.orm().begin().await?
        };
        if self.backend() == DbBackend::Postgres {
            tx.execute_unprepared("SET LOCAL statement_timeout = '10s'")
                .await?;
        }
        let values = vec![
            from.timestamp_millis().into(),
            now.timestamp_millis().into(),
        ];
        let all = tx
            .query_one_raw(Statement::from_sql_and_values(
                self.backend(),
                aggregate_sql(""),
                values.clone(),
            ))
            .await?
            .ok_or_else(|| NuonoError::DatabaseQuery("AI 用量汇总缺失".into()))?;
        let totals = counts(&all)?;
        let mut daily = BTreeMap::new();
        for day in 0..days {
            daily.insert(
                (from + Duration::days(i64::from(day)))
                    .format("%Y-%m-%d")
                    .to_string(),
                UsageCounts::default(),
            );
        }
        for row in tx
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                aggregate_sql("day"),
                values.clone(),
            ))
            .await?
        {
            daily.insert(row.try_get("", "day")?, counts(&row)?);
        }
        let mut models = Vec::new();
        let rows = tx
            .query_all_raw(Statement::from_sql_and_values(
                self.backend(),
                format!(
                    "{} LIMIT 2001",
                    aggregate_sql("model_id, model, name, provider, operation, source")
                ),
                values,
            ))
            .await?;
        if rows.len() > 2000 {
            return Err(NuonoError::Validation(
                "当前范围的模型用量分组超过 2000 项，请缩短统计范围后重试".into(),
            ));
        }
        for row in rows {
            models.push(ModelUsage {
                model_id: row.try_get("", "model_id")?,
                model: row.try_get("", "model")?,
                name: row.try_get("", "name")?,
                provider: row.try_get("", "provider")?,
                operation: row.try_get("", "operation")?,
                source: row.try_get("", "source")?,
                counts: counts(&row)?,
            });
        }
        models.sort_by(|a, b| {
            b.counts
                .requests
                .cmp(&a.counts.requests)
                .then(a.model.cmp(&b.model))
                .then(a.source.cmp(&b.source))
                .then(a.operation.cmp(&b.operation))
        });
        tx.commit().await?;
        Ok(UsageSummary {
            days,
            from: crate::clock::format_wire_ts(from),
            to: crate::clock::format_wire_ts(now),
            totals,
            daily: daily
                .into_iter()
                .map(|(date, counts)| DailyUsage { date, counts })
                .collect(),
            models,
            unrecorded_requests: self.ai_usage.lost.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/ai_usage/tests.rs"]
mod tests;
