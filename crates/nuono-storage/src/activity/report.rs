use super::*;
use sea_orm::{AccessMode, IsolationLevel, QueryResult, Statement};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityFilter {
    pub days: u32,
    pub platform: String,
    pub user_id: String,
    pub scope_id: String,
    pub plugin: String,
    pub user_key: String,
    pub scope_key: String,
}
impl Default for ActivityFilter {
    fn default() -> Self {
        Self {
            days: 30,
            platform: String::new(),
            user_id: String::new(),
            scope_id: String::new(),
            plugin: String::new(),
            user_key: String::new(),
            scope_key: String::new(),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, utoipa::ToSchema)]
pub struct ActivityCounts {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub messages: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub plugin_calls: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub active_users: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub active_groups: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub commands: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub rules: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub listeners: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub events: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub replies: i64,
}
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct ActivityDay {
    pub date: String,
    #[serde(flatten)]
    pub counts: ActivityCounts,
}
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct ActivityBreakdown {
    pub key: String,
    pub label: String,
    pub platform: String,
    pub namespace: String,
    pub scene: String,
    #[serde(flatten)]
    pub counts: ActivityCounts,
}
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ActivityReport {
    pub from: String,
    pub to: String,
    pub days: u32,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub dau: i64,
    pub totals: ActivityCounts,
    pub daily: Vec<ActivityDay>,
    pub platforms: Vec<ActivityBreakdown>,
    pub groups: Vec<ActivityBreakdown>,
    pub users: Vec<ActivityBreakdown>,
    pub plugins: Vec<ActivityBreakdown>,
    pub unrecorded: u64,
}
const COUNTS: &str =
    "CAST(COALESCE(SUM(CASE WHEN kind = 'message' THEN count ELSE 0 END),0) AS BIGINT) AS messages,
 CAST(COALESCE(SUM(CASE WHEN kind <> 'message' THEN count ELSE 0 END),0) AS BIGINT) AS plugin_calls,
 COUNT(DISTINCT user_key) AS active_users,
 COUNT(DISTINCT CASE WHEN scene <> 'private' THEN scope_key END) AS active_groups,
 CAST(COALESCE(SUM(CASE WHEN kind = 'command' THEN count ELSE 0 END),0) AS BIGINT) AS commands,
 CAST(COALESCE(SUM(CASE WHEN kind = 'rule' THEN count ELSE 0 END),0) AS BIGINT) AS rules,
 CAST(COALESCE(SUM(CASE WHEN kind = 'listener' THEN count ELSE 0 END),0) AS BIGINT) AS listeners,
 CAST(COALESCE(SUM(CASE WHEN kind = 'event' THEN count ELSE 0 END),0) AS BIGINT) AS events,
 CAST(COALESCE(SUM(CASE WHEN kind = 'reply' THEN count ELSE 0 END),0) AS BIGINT) AS replies";
fn counts(row: &QueryResult) -> Result<ActivityCounts> {
    Ok(ActivityCounts {
        messages: row.try_get("", "messages")?,
        plugin_calls: row.try_get("", "plugin_calls")?,
        active_users: row.try_get("", "active_users")?,
        active_groups: row.try_get("", "active_groups")?,
        commands: row.try_get("", "commands")?,
        rules: row.try_get("", "rules")?,
        listeners: row.try_get("", "listeners")?,
        events: row.try_get("", "events")?,
        replies: row.try_get("", "replies")?,
    })
}
impl Database {
    pub async fn activity_report(&self, filter: ActivityFilter) -> Result<ActivityReport> {
        self.activity.flush().await?;
        self.activity_report_at(filter, crate::clock::db_now().into())
            .await
    }
    pub(super) async fn activity_report_at(
        &self,
        filter: ActivityFilter,
        now: DateTime<Utc>,
    ) -> Result<ActivityReport> {
        if !(1..=90).contains(&filter.days) {
            return Err(NuonoError::Validation("统计天数须为 1–90 天".into()));
        }
        let from = (now - Duration::days(i64::from(filter.days) - 1))
            .format("%Y-%m-%d")
            .to_string();
        let to = now.format("%Y-%m-%d").to_string();
        let mut values: Vec<sea_orm::Value> = vec![from.clone().into(), to.clone().into()];
        let mut predicate = "day >= $1 AND day <= $2".to_string();
        for (column, value, limit) in [
            ("platform", &filter.platform, 64),
            ("user_id", &filter.user_id, 512),
            ("scope_id", &filter.scope_id, 512),
            ("plugin", &filter.plugin, 128),
            ("user_key", &filter.user_key, 64),
            ("scope_key", &filter.scope_key, 64),
        ] {
            if value.len() > limit || value.chars().any(char::is_control) {
                return Err(NuonoError::Validation(
                    "统计筛选标识过长或含控制字符".into(),
                ));
            }
            if !value.is_empty() {
                values.push(value.clone().into());
                predicate.push_str(&format!(" AND {column} = ${}", values.len()));
            }
        }
        let tx = if self.backend() == sea_orm::DbBackend::Postgres {
            self.orm()
                .begin_with_config(
                    Some(IsolationLevel::RepeatableRead),
                    Some(AccessMode::ReadOnly),
                )
                .await?
        } else {
            self.orm().begin().await?
        };
        let statement =
            |sql: String| Statement::from_sql_and_values(self.backend(), sql, values.clone());
        let total = tx
            .query_one_raw(statement(format!(
                "SELECT {COUNTS} FROM nuono_activity_daily WHERE {predicate}"
            )))
            .await?
            .ok_or_else(|| NuonoError::DatabaseQuery("统计汇总缺失".into()))?;
        let mut days = std::collections::BTreeMap::new();
        for offset in 0..filter.days {
            days.insert(
                (now - Duration::days(i64::from(offset)))
                    .format("%Y-%m-%d")
                    .to_string(),
                ActivityCounts::default(),
            );
        }
        for row in tx.query_all_raw(statement(format!("SELECT day, {COUNTS} FROM nuono_activity_daily WHERE {predicate} GROUP BY day ORDER BY day"))).await? {
            days.insert(row.try_get("", "day")?, counts(&row)?);
        }
        let mut dimensions = Vec::new();
        // SQL 列名与片段仅取自下方固定的内置表；外部传入的参数值全部走预编译绑定。
        for (key, label, platform, namespace, scene, extra) in [
            ("platform", "platform", "platform", "''", "''", ""),
            (
                "scope_key",
                "scope_id",
                "platform",
                "''",
                "scene",
                " AND scene <> 'private'",
            ),
            ("user_key", "user_id", "platform", "namespace", "''", ""),
            ("plugin", "plugin", "''", "''", "''", " AND plugin <> ''"),
        ] {
            let selectors = format!(
                "{key} AS key, {label} AS label, {platform} AS platform, {namespace} AS namespace, {scene} AS scene"
            );
            let grouping = [key, label, platform, namespace, scene]
                .into_iter()
                .filter(|value| *value != "''")
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {selectors}, {COUNTS} FROM nuono_activity_daily WHERE {predicate}{extra} GROUP BY {grouping} ORDER BY plugin_calls DESC, messages DESC, key ASC LIMIT 50"
            );
            let mut rows = Vec::new();
            for row in tx.query_all_raw(statement(sql)).await? {
                rows.push(ActivityBreakdown {
                    key: row.try_get("", "key")?,
                    label: row.try_get("", "label")?,
                    platform: row.try_get("", "platform")?,
                    namespace: row.try_get("", "namespace")?,
                    scene: row.try_get("", "scene")?,
                    counts: counts(&row)?,
                });
            }
            dimensions.push(rows);
        }
        tx.commit().await?;
        let daily: Vec<_> = days
            .into_iter()
            .map(|(date, counts)| ActivityDay { date, counts })
            .collect();
        let dau = daily.last().map_or(0, |day| day.counts.active_users);
        let mut dimensions = dimensions.into_iter();
        Ok(ActivityReport {
            from,
            to,
            days: filter.days,
            dau,
            totals: counts(&total)?,
            daily,
            platforms: dimensions.next().unwrap_or_default(),
            groups: dimensions.next().unwrap_or_default(),
            users: dimensions.next().unwrap_or_default(),
            plugins: dimensions.next().unwrap_or_default(),
            unrecorded: self.activity.lost.load(Ordering::Relaxed),
        })
    }
}
