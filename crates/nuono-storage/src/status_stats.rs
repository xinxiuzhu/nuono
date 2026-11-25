//! 核心状态卡片的数据聚合查询（有明确时间窗口边界，不读取任何消息正文）。
//!
//! 按 UTC+08（北京时间）自然日切分天粒度。状态统计读取已落盘的 `nuono_messages` 与 `nuono_audit_events`，
//! 异步消息写入队列中的最新消息可能会有短暂延迟才会在统计中可见。

use std::collections::HashMap;

use chrono::{DateTime, Days, Duration, NaiveDate, Utc};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, QueryResult,
    Statement, Value,
};

use crate::{Database, entity::bot_instances, error::NuonoError, error::Result};

const TREND_DAYS: u64 = 14;

/// 状态卡片统计汇总：包含当前机器人今日统计、全量机器人今日统计，以及最近 14 天的每日活动趋势。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusStats {
    pub current: StatusScopeStats,
    pub all: StatusScopeStats,
    /// 最近 14 天全量机器人的每日活动趋势（按日期从早到晚排列，无消息的日期也会补零）。
    pub daily: Vec<StatusDailyActivity>,
}

/// 自今日 UTC+08 零点起至查询时间点 `now` 为止的单日累计活动统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusScopeStats {
    pub received: u64,
    pub sent: u64,
    pub commands: u64,
    /// 有过入站消息交互的去重 `(bot_instance_id, native_sender_id)` 活跃用户数。
    pub active_users: u64,
    /// 收到过群消息的去重 `(bot_instance_id, native_group_id)` 活跃群组数。
    pub active_groups: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusDailyActivity {
    pub date: NaiveDate,
    pub received: u64,
    pub sent: u64,
    /// 当前触发状态查询命令的机器人账号在该日发送的消息数。
    pub current_sent: u64,
    pub active_users: u64,
}

/// 仅在数据库侧执行聚合计数查询，消息正文与用户敏感信息绝不会读出数据库。
/// 晚于 `now` 的未来记录以及空/纯空白的 `native_sender_id`/`native_group_id` 均不计入对应指标。
pub async fn get_status_stats(
    db: &Database,
    bot_id: &str,
    now: DateTime<Utc>,
) -> Result<StatusStats> {
    let today = now
        .naive_utc()
        .checked_add_signed(Duration::hours(8))
        .ok_or_else(invalid_date_range)?
        .date();
    let first_day = today
        .checked_sub_days(Days::new(TREND_DAYS - 1))
        .ok_or_else(invalid_date_range)?;
    let today_start = utc_midnight(today)?;
    let trend_start = utc_midnight(first_day)?;

    let bot_keys = resolve_bot_keys(db, bot_id).await?;
    let current = scope_stats(db, Some(&bot_keys), today_start, now).await?;
    let all = scope_stats(db, None, today_start, now).await?;
    let mut buckets = daily_stats(db, &bot_keys, trend_start, now).await?;
    let daily = (0..TREND_DAYS)
        .map(|offset| {
            let date = first_day
                .checked_add_days(Days::new(offset))
                .ok_or_else(invalid_date_range)?;
            Ok(buckets.remove(&date).unwrap_or(StatusDailyActivity {
                date,
                ..Default::default()
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(StatusStats {
        current,
        all,
        daily,
    })
}

/// 解析目标机器人对应的全部标识键（同时涵盖传入的原生 Bot ID 与 `nuono_bot_instances` 的 UUIDv7 主键）。
async fn resolve_bot_keys(db: &Database, bot_id: &str) -> Result<Vec<String>> {
    let mut keys = vec![bot_id.to_owned()];
    let rows = bot_instances::Entity::find()
        .filter(
            Condition::any()
                .add(bot_instances::Column::Id.eq(bot_id))
                .add(bot_instances::Column::NativeBotId.eq(bot_id)),
        )
        .all(db.orm())
        .await?;
    for row in rows {
        if !keys.contains(&row.id) {
            keys.push(row.id);
        }
        if let Some(native) = row.native_bot_id
            && !keys.contains(&native)
        {
            keys.push(native);
        }
    }
    Ok(keys)
}

fn invalid_date_range() -> NuonoError {
    NuonoError::Validation("状态统计日期超出支持范围".into())
}

fn utc_midnight(date: NaiveDate) -> Result<DateTime<Utc>> {
    date.and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.checked_sub_signed(Duration::hours(8)))
        .map(|midnight| midnight.and_utc())
        .ok_or_else(invalid_date_range)
}

/// 构造时间窗口过滤 SQL 片段与按 UTC+08 分桶的日期表达式（统一使用 `$1`、`$2` 位置占位符）。
fn sql_window(backend: DbBackend) -> Result<(String, String)> {
    let day = match backend {
        DbBackend::Postgres => {
            "to_char((to_timestamp(occurred_at_ms / 1000.0) AT TIME ZONE 'UTC') + INTERVAL '8 hours', 'YYYY-MM-DD')"
        }
        DbBackend::Sqlite => "date(occurred_at_ms / 1000.0, 'unixepoch', '+8 hours')",
        other => {
            return Err(NuonoError::NotImplemented(format!(
                "状态统计不支持后端 {other:?}"
            )));
        }
    };
    Ok((
        "occurred_at_ms >= $1 AND occurred_at_ms <= $2".into(),
        day.into(),
    ))
}

fn bot_in_clause(column: &str, start_index: usize, count: usize) -> String {
    if count == 1 {
        return format!("{column} = ${start_index}");
    }
    let placeholders: Vec<String> = (start_index..start_index + count)
        .map(|idx| format!("${idx}"))
        .collect();
    format!("{column} IN ({})", placeholders.join(", "))
}

fn window_values(
    start: DateTime<Utc>,
    now: DateTime<Utc>,
    bot_keys: Option<&[String]>,
) -> Vec<Value> {
    let mut values = vec![
        start.timestamp_millis().into(),
        now.timestamp_millis().into(),
    ];
    if let Some(keys) = bot_keys {
        for key in keys {
            values.push(key.clone().into());
        }
    }
    values
}

async fn scope_stats(
    db: &Database,
    bot_keys: Option<&[String]>,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<StatusScopeStats> {
    let (window, _) = sql_window(db.backend())?;
    let (msg_scope, audit_scope) = match bot_keys {
        Some(keys) => (
            format!(" AND {}", bot_in_clause("bot_instance_id", 3, keys.len())),
            format!(" AND {}", bot_in_clause("bot_id", 3, keys.len())),
        ),
        None => (String::new(), String::new()),
    };
    let sql = format!(
        "WITH messages AS (
            SELECT bot_instance_id, native_sender_id, native_group_id, direction
            FROM nuono_messages
            WHERE {window}{msg_scope} AND direction IN ('inbound', 'outbound')
         )
         SELECT
            COALESCE(SUM(CASE WHEN direction = 'inbound' THEN 1 ELSE 0 END), 0) AS received,
            COALESCE(SUM(CASE WHEN direction = 'outbound' THEN 1 ELSE 0 END), 0) AS sent,
            (SELECT COUNT(*) FROM nuono_audit_events WHERE category = 'command' AND occurred_at_ms >= $1 AND occurred_at_ms <= $2{audit_scope}) AS commands,
            (SELECT COUNT(*) FROM (
                SELECT bot_instance_id, native_sender_id FROM messages
                WHERE direction = 'inbound' AND native_sender_id IS NOT NULL AND TRIM(native_sender_id) <> ''
                GROUP BY bot_instance_id, native_sender_id
             ) AS active_user_ids) AS active_users,
            (SELECT COUNT(*) FROM (
                SELECT bot_instance_id, native_group_id FROM messages
                WHERE direction = 'inbound' AND native_group_id IS NOT NULL AND TRIM(native_group_id) <> ''
                GROUP BY bot_instance_id, native_group_id
             ) AS active_group_ids) AS active_groups
         FROM messages"
    );
    let values = window_values(start, now, bot_keys);
    let row = db
        .orm()
        .query_one_raw(Statement::from_sql_and_values(db.backend(), sql, values))
        .await?
        .ok_or_else(|| NuonoError::DatabaseQuery("状态聚合查询未返回结果".into()))?;
    Ok(StatusScopeStats {
        received: count(&row, "received")?,
        sent: count(&row, "sent")?,
        commands: count(&row, "commands")?,
        active_users: count(&row, "active_users")?,
        active_groups: count(&row, "active_groups")?,
    })
}

async fn daily_stats(
    db: &Database,
    bot_keys: &[String],
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<HashMap<NaiveDate, StatusDailyActivity>> {
    let (window, day) = sql_window(db.backend())?;
    let current_bot_cond = bot_in_clause("bot_instance_id", 3, bot_keys.len());
    let sql = format!(
        "WITH messages AS (
            SELECT {day} AS day, bot_instance_id, native_sender_id, direction
            FROM nuono_messages
            WHERE {window} AND direction IN ('inbound', 'outbound')
         ), activity AS (
            SELECT day,
                SUM(CASE WHEN direction = 'inbound' THEN 1 ELSE 0 END) AS received,
                SUM(CASE WHEN direction = 'outbound' THEN 1 ELSE 0 END) AS sent,
                SUM(CASE WHEN direction = 'outbound' AND {current_bot_cond} THEN 1 ELSE 0 END) AS current_sent
            FROM messages GROUP BY day
         ), active_senders AS (
            SELECT day, COUNT(*) AS active_users FROM (
                SELECT day, bot_instance_id, native_sender_id FROM messages
                WHERE direction = 'inbound' AND native_sender_id IS NOT NULL AND TRIM(native_sender_id) <> ''
                GROUP BY day, bot_instance_id, native_sender_id
            ) AS identities GROUP BY day
         )
         SELECT activity.day, received, sent, current_sent, COALESCE(active_users, 0) AS active_users
         FROM activity LEFT JOIN active_senders ON activity.day = active_senders.day ORDER BY activity.day"
    );
    let rows = db
        .orm()
        .query_all_raw(Statement::from_sql_and_values(
            db.backend(),
            sql,
            window_values(start, now, Some(bot_keys)),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            let day: String = row.try_get("", "day")?;
            let date = NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                .map_err(|_| NuonoError::DatabaseQuery("状态聚合返回无效日期".into()))?;
            Ok((
                date,
                StatusDailyActivity {
                    date,
                    received: count(&row, "received")?,
                    sent: count(&row, "sent")?,
                    current_sent: count(&row, "current_sent")?,
                    active_users: count(&row, "active_users")?,
                },
            ))
        })
        .collect()
}

fn count(row: &QueryResult, column: &str) -> Result<u64> {
    let value: i64 = row.try_get("", column)?;
    u64::try_from(value).map_err(|_| NuonoError::DatabaseQuery("状态聚合返回负数".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::CATEGORY_COMMAND;
    use crate::conversation_store::{
        DIRECTION_INBOUND, DIRECTION_OUTBOUND, ROLE_ASSISTANT, ROLE_USER,
    };
    use crate::entity::{audit_events, messages};
    use sea_orm::{ActiveModelTrait, EntityTrait, PaginatorTrait, QueryOrder, Set};

    fn instant(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    async fn message(
        db: &Database,
        bot: &str,
        direction: &str,
        user: Option<&str>,
        group: Option<&str>,
        timestamp: DateTime<Utc>,
    ) {
        let ts_ms = timestamp.timestamp_millis();
        let role = if direction == DIRECTION_OUTBOUND {
            ROLE_ASSISTANT
        } else {
            ROLE_USER
        };
        messages::ActiveModel {
            platform: Set("onebot".into()),
            bot_instance_id: Set(Some(bot.into())),
            direction: Set(direction.into()),
            role: Set(role.into()),
            native_sender_id: Set(user.map(str::to_owned)),
            native_group_id: Set(group.map(str::to_owned)),
            text_content: Set("private content must never be selected".into()),
            content_blocks_json: Set("[]".into()),
            occurred_at_ms: Set(ts_ms),
            created_at_ms: Set(ts_ms),
            ..Default::default()
        }
        .insert(db.orm())
        .await
        .unwrap();
    }

    async fn command(db: &Database, bot: &str, timestamp: Option<DateTime<Utc>>) {
        audit_events::ActiveModel {
            category: Set(CATEGORY_COMMAND.to_string()),
            occurred_at_ms: Set(timestamp.map_or(0, |ts| ts.timestamp_millis())),
            bot_id: Set(Some(bot.into())),
            actor_label: Set(Some("shared-user".into())),
            action: Set("nuono状态".into()),
            result: Set("ok".into()),
            details_json: Set("{}".into()),
            ..Default::default()
        }
        .insert(db.orm())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn empty_history_zero_fills_fourteen_calendar_days() {
        let db = crate::test_support::test_db(2).await;
        let stats = get_status_stats(&db, "bot-a", instant("2026-10-14T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(stats.current, StatusScopeStats::default());
        assert_eq!(stats.all, StatusScopeStats::default());
        assert_eq!(stats.daily.len(), 14);
        assert_eq!(stats.daily[0].date.to_string(), "2026-10-01");
        assert_eq!(stats.daily[13].date.to_string(), "2026-10-14");
        assert!(stats.daily.iter().all(|day| day.received == 0
            && day.sent == 0
            && day.current_sent == 0
            && day.active_users == 0));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scopes_count_received_identities_per_bot_and_ignore_invalid_rows() {
        let db = crate::test_support::test_db(2).await;
        let now = instant("2026-10-14T02:00:00Z");
        for bot in ["bot-a", "bot-b"] {
            for _ in 0..2 {
                message(
                    &db,
                    bot,
                    DIRECTION_INBOUND,
                    Some("shared-user"),
                    Some("shared-group"),
                    now,
                )
                .await;
            }
            command(&db, bot, Some(now)).await;
        }
        for direction in [DIRECTION_INBOUND, DIRECTION_OUTBOUND] {
            message(
                &db,
                "bot-b",
                direction,
                Some("shared-user"),
                Some("shared-group"),
                now,
            )
            .await;
        }
        // 出站消息（outbound）的目标接收人或目标群不计入活跃发送用户或活跃群组。
        message(
            &db,
            "bot-a",
            DIRECTION_OUTBOUND,
            Some("recipient"),
            Some("outbound"),
            now,
        )
        .await;
        // 缺少有效发送者或群号的入站消息虽然不计入活跃用户/群，但仍应计入接收消息总数。
        for identity in [None, Some("   ")] {
            message(&db, "bot-a", DIRECTION_INBOUND, identity, identity, now).await;
        }
        message(
            &db,
            "bot-a",
            DIRECTION_OUTBOUND,
            None,
            None,
            now + Duration::seconds(1),
        )
        .await;
        command(&db, "bot-a", None).await;
        command(&db, "bot-a", Some(now + Duration::seconds(1))).await;

        let stats = get_status_stats(&db, "bot-a", now).await.unwrap();
        assert_eq!(
            stats.current,
            StatusScopeStats {
                received: 4,
                sent: 1,
                commands: 1,
                active_users: 1,
                active_groups: 1,
            }
        );
        assert_eq!(
            stats.all,
            StatusScopeStats {
                received: 7,
                sent: 2,
                commands: 2,
                active_users: 2,
                active_groups: 2,
            }
        );
        assert_eq!(
            stats.daily[13],
            StatusDailyActivity {
                date: now.date_naive(),
                received: 7,
                sent: 2,
                current_sent: 1,
                active_users: 2,
            }
        );
        assert!(
            stats.daily[..13]
                .iter()
                .all(|day| day.received == 0 && day.sent == 0)
        );
        let missing = get_status_stats(&db, "unknown-bot", now).await.unwrap();
        assert_eq!(missing.current, StatusScopeStats::default());
        assert_eq!(missing.all, stats.all);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn utc_plus_eight_midnight_and_trend_boundaries_are_exact() {
        let db = crate::test_support::test_db(2).await;
        // 这是UTC+08的10月15日，而UTC时间仍然是10月14日。
        let now = instant("2026-10-14T16:00:01Z");
        let midnight = instant("2026-10-14T16:00:00Z");
        let first_midnight = instant("2026-10-01T16:00:00Z");
        for timestamp in [
            first_midnight - Duration::seconds(1),
            first_midnight,
            midnight - Duration::seconds(1),
            midnight,
            now,
            now + Duration::seconds(1),
        ] {
            message(
                &db,
                "bot-a",
                DIRECTION_INBOUND,
                Some("user"),
                Some("group"),
                timestamp,
            )
            .await;
            command(&db, "bot-a", Some(timestamp)).await;
        }
        let stats = get_status_stats(&db, "bot-a", now).await.unwrap();
        assert_eq!(stats.current.received, 2);
        assert_eq!(stats.current.commands, 2);
        assert_eq!(stats.current.active_users, 1);
        assert_eq!(stats.daily[0].date.to_string(), "2026-10-02");
        assert_eq!(stats.daily[0].received, 1);
        assert_eq!(stats.daily[12].date.to_string(), "2026-10-14");
        assert_eq!(stats.daily[12].received, 1);
        assert_eq!(stats.daily[13].date.to_string(), "2026-10-15");
        assert_eq!(stats.daily[13].received, 2);
        assert_eq!(stats.daily.iter().map(|day| day.received).sum::<u64>(), 4);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn millisecond_future_records_do_not_enter_the_snapshot() {
        let db = crate::test_support::test_db(2).await;
        let now = instant("2026-10-14T16:00:00.000100Z");
        for timestamp in [
            now - Duration::milliseconds(1),
            now,
            now + Duration::milliseconds(1),
        ] {
            message(
                &db,
                "bot-a",
                DIRECTION_INBOUND,
                Some("user"),
                Some("group"),
                timestamp,
            )
            .await;
            command(&db, "bot-a", Some(timestamp)).await;
        }
        let stats = get_status_stats(&db, "bot-a", now).await.unwrap();
        assert_eq!(stats.current.received, 1);
        assert_eq!(stats.current.commands, 1);
        assert_eq!(stats.all, stats.current);
        assert_eq!(stats.daily[12].received, 1);
        assert_eq!(stats.daily[13].received, 1);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sqlite_windows_use_timestamp_indexes_and_preserve_millisecond_boundaries() {
        let db = crate::test_support::test_db(2).await;
        if db.backend() != DbBackend::Sqlite {
            db.shutdown().await.unwrap();
            return;
        }
        let now = instant("2026-10-14T16:00:00.000100Z");
        let start = instant("2026-10-01T16:00:00Z");
        let (window, _) = sql_window(db.backend()).unwrap();
        for (sql, index) in [
            (
                format!("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM nuono_messages WHERE {window}"),
                "idx_nuono_messages_occurred",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM nuono_audit_events WHERE category = 'command' AND occurred_at_ms >= $1 AND occurred_at_ms <= $2".to_string(),
                "idx_nuono_audit_events_category_time",
            ),
        ] {
            let plans = db
                .orm()
                .query_all_raw(Statement::from_sql_and_values(
                    db.backend(),
                    sql,
                    window_values(start, now, None),
                ))
                .await
                .unwrap();
            assert!(
                plans.iter().any(|row| {
                    let detail: String = row.try_get("", "detail").unwrap();
                    detail.contains("SEARCH") && detail.contains(index)
                }),
                "status window did not use expected timestamp range index {index}"
            );
        }
        for timestamp in [
            "2026-10-02T05:59:59.999+14:00", // 早于 14 天窗口首个 UTC+08 自然日零点 1 毫秒。
            "2026-10-02T06:00:00+14:00",     // 恰好落在首个 UTC+08 自然日零点边界上。
            "2026-10-14T02:00:00.000-14:00", // 负时区偏移表示的当前时刻（恰好等于 now）。
            "2026-10-14T16:00:00.000Z",      // 与上一条相同 UTC 毫秒时间戳。
            "2026-10-14T02:00:00.001-14:00", // 晚于 now 1 毫秒的未来记录（应被排除）。
        ] {
            let ts_ms = instant(timestamp).timestamp_millis();
            db.orm()
                .execute_raw(Statement::from_sql_and_values(
                    db.backend(),
                    "INSERT INTO nuono_messages \
                     (platform, bot_instance_id, native_sender_id, direction, role, text_content, content_blocks_json, occurred_at_ms, created_at_ms) \
                     VALUES ('onebot', 'bot-a', 'user', 'inbound', 'user', '', '[]', ?, ?)",
                    [ts_ms.into(), ts_ms.into()],
                ))
                .await
                .unwrap();
        }
        let stats = get_status_stats(&db, "bot-a", now).await.unwrap();
        assert_eq!(stats.current.received, 2);
        assert_eq!(stats.daily[0].received, 1);
        assert_eq!(stats.daily[13].received, 2);
        assert_eq!(stats.daily.iter().map(|day| day.received).sum::<u64>(), 3);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn saved_history_preserves_native_string_and_integer_identities() {
        let db = crate::test_support::test_db(2).await;
        let native_bot = "native-bot-".repeat(8);
        let native_identity = "opaque-open-id-".repeat(8);
        let native_message = "native-message-id-".repeat(8);
        let identities = [
            serde_json::json!(native_identity),
            serde_json::json!(-123),
            serde_json::json!(u64::MAX),
            serde_json::json!("   "),
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(12.5),
        ];
        for (index, identity) in identities.iter().enumerate() {
            let message_id = if index == 0 {
                serde_json::json!(native_message)
            } else {
                identity.clone()
            };
            db.save_message_history(
                &native_bot,
                &serde_json::json!({
                    "message_type": "group",
                    "message_id": message_id,
                    "user_id": identity,
                    "group_id": identity,
                    "message": "test",
                }),
                "receive",
            )
            .await
            .unwrap();
        }
        // 轮询等待异步历史写入协程将队列中的所有记录真正落库，避免在测试里靠固定 sleep 时间盲猜。
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if messages::Entity::find().count(db.orm()).await.unwrap()
                    == identities.len() as u64
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("history writer did not flush in time");
        let rows = messages::Entity::find()
            .order_by_asc(messages::Column::Id)
            .all(db.orm())
            .await
            .unwrap();
        let expected = [
            Some(native_identity),
            Some("-123".to_owned()),
            Some(u64::MAX.to_string()),
            None,
            None,
            None,
            None,
        ];
        for (index, (row, identity)) in rows.iter().zip(expected).enumerate() {
            let message_id = if index == 0 {
                Some(native_message.clone())
            } else {
                identity.clone()
            };
            assert_eq!(row.native_message_id, message_id);
            assert_eq!(row.native_sender_id, identity);
            assert_eq!(row.native_group_id, identity);
        }
        let stats = get_status_stats(&db, &native_bot, Utc::now())
            .await
            .unwrap();
        assert_eq!(stats.daily.iter().map(|day| day.received).sum::<u64>(), 7);
        assert_eq!(
            stats.daily.iter().map(|day| day.active_users).sum::<u64>(),
            3
        );
        db.shutdown().await.unwrap();
    }
}
