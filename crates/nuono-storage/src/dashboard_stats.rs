//! 仪表盘数据库统计计数及命令/消息统计查询（基于 `nuono_messages`、`nuono_audit_events`、`nuono_groups`、`nuono_users`）。
//! 机器人的实时在线与可用状态由宿主结合持久化连接记录与运行时活跃连接汇总得出。

use std::collections::HashMap;

use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect};

use crate::audit::CATEGORY_COMMAND;
use crate::conversation_store::{DIRECTION_INBOUND, DIRECTION_OUTBOUND};
use crate::db::Database;
use crate::entity::{audit_events, groups, messages, users};
use crate::error::Result;

/// 数据库持久化统计计数（不依赖宿主的运行时机器人连接注册表）。
#[derive(Debug, Clone)]
pub struct DashboardCounts {
    pub total_messages: i64,
    pub user_count: i64,
    pub group_count: i64,
}

pub async fn get_dashboard_counts(db: &Database) -> Result<DashboardCounts> {
    let total_messages = messages::Entity::find().count(db.orm()).await?;
    let user_count = users::orm::Entity::find()
        .filter(users::orm::Column::AccountKind.eq(users::ACCOUNT_KIND_REGISTERED))
        .count(db.orm())
        .await?;
    let group_count = groups::Entity::find().count(db.orm()).await?;

    Ok(DashboardCounts {
        total_messages: total_messages as i64,
        user_count: user_count as i64,
        group_count: group_count as i64,
    })
}

/// 命令/消息统计。每个字段都有真实的表来源，见各注释。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct CommandStats {
    /// `nuono_messages` 里 `direction = "inbound"` 的行数。
    pub receive: u64,
    /// `nuono_messages` 里 `direction = "outbound"` 的行数。
    pub send: u64,
    /// `nuono_audit_events` 中命令分类行数。
    pub command: u64,
    /// `nuono_groups` 行数。
    pub groups: u64,
    /// `nuono_users` 行数。
    pub users: u64,
    /// 最近 60 秒内的消息总数（近似 QPS 窗口，不区分方向）。
    pub current_qps: u64,
    /// 最近 24 小时里，每秒消息量的峰值（不区分方向）。
    pub max_qps: u64,
    /// 收发消息总数。
    pub total_count: u64,
}

pub async fn get_command_stats(db: &Database) -> Result<CommandStats> {
    get_command_stats_with_clock(db, chrono::Utc::now).await
}

async fn get_command_stats_with_clock(
    db: &Database,
    now: impl FnOnce() -> chrono::DateTime<chrono::Utc>,
) -> Result<CommandStats> {
    let receive = messages::Entity::find()
        .filter(messages::Column::Direction.eq(DIRECTION_INBOUND))
        .count(db.orm())
        .await?;
    let send = messages::Entity::find()
        .filter(messages::Column::Direction.eq(DIRECTION_OUTBOUND))
        .count(db.orm())
        .await?;
    let command = audit_events::Entity::find()
        .filter(audit_events::Column::Category.eq(CATEGORY_COMMAND))
        .count(db.orm())
        .await?;
    let groups = groups::Entity::find().count(db.orm()).await?;
    let users = users::orm::Entity::find().count(db.orm()).await?;

    let now = now();
    let current_qps = messages::Entity::find()
        .filter(
            messages::Column::OccurredAtMs
                .gte((now - chrono::Duration::seconds(60)).timestamp_millis()),
        )
        .count(db.orm())
        .await?;

    // 峰值按秒分桶在内存里算：只查最近 24 小时这一有界窗口的毫秒时间戳（只选一列），
    // 逐秒计数取最大，不拉全库。
    let recent: Vec<i64> = messages::Entity::find()
        .select_only()
        .column(messages::Column::OccurredAtMs)
        .filter(
            messages::Column::OccurredAtMs
                .gte((now - chrono::Duration::hours(24)).timestamp_millis()),
        )
        .into_tuple()
        .all(db.orm())
        .await?;

    let mut per_second: HashMap<i64, u64> = HashMap::new();
    for ts_ms in recent {
        *per_second.entry(ts_ms.div_euclid(1000)).or_default() += 1;
    }
    let max_qps = per_second.values().copied().max().unwrap_or(0);

    Ok(CommandStats {
        receive,
        send,
        command,
        groups,
        users,
        current_qps,
        max_qps,
        total_count: receive + send,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_store::{ROLE_ASSISTANT, ROLE_USER};
    use chrono::Duration;
    use sea_orm::{ActiveModelTrait, Set};

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        "2026-10-04T12:00:00Z".parse().unwrap()
    }

    async fn insert_message(
        db: &Database,
        direction: &str,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        let ts_ms = timestamp.timestamp_millis();
        let role = if direction == DIRECTION_OUTBOUND {
            ROLE_ASSISTANT
        } else {
            ROLE_USER
        };
        messages::ActiveModel {
            platform: Set("onebot".into()),
            bot_instance_id: Set(Some("stats-bot".into())),
            direction: Set(direction.into()),
            role: Set(role.into()),
            text_content: Set(String::new()),
            content_blocks_json: Set("[]".into()),
            occurred_at_ms: Set(ts_ms),
            created_at_ms: Set(ts_ms),
            ..Default::default()
        }
        .insert(db.orm())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn empty_tables_report_zero_counts_and_zero_peak() {
        let db = crate::test_support::test_db(2).await;
        let counts = get_dashboard_counts(&db).await.unwrap();
        assert_eq!(counts.total_messages, 0);
        assert_eq!(counts.user_count, 0);
        assert_eq!(counts.group_count, 0);
        let stats = get_command_stats(&db).await.unwrap();
        assert_eq!(
            serde_json::to_value(stats).unwrap(),
            serde_json::json!({
                "receive": 0, "send": 0, "command": 0, "groups": 0, "users": 0,
                "current_qps": 0, "max_qps": 0, "total_count": 0
            })
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn history_direction_totals_and_timestamp_windows_keep_their_distinct_scopes() {
        let db = crate::test_support::test_db(2).await;
        let now = fixed_now();
        for (direction, timestamp) in [
            (
                DIRECTION_INBOUND,
                now - Duration::seconds(10) + Duration::milliseconds(100),
            ),
            (
                DIRECTION_OUTBOUND,
                now - Duration::seconds(10) + Duration::milliseconds(900),
            ),
            (
                DIRECTION_INBOUND,
                now - Duration::seconds(10) + Duration::milliseconds(500),
            ),
            (DIRECTION_OUTBOUND, now - Duration::seconds(60)),
            (DIRECTION_INBOUND, now - Duration::seconds(61)),
            (DIRECTION_INBOUND, now - Duration::hours(24)),
            (
                DIRECTION_OUTBOUND,
                now - Duration::hours(24) + Duration::milliseconds(100),
            ),
            (
                DIRECTION_INBOUND,
                now - Duration::hours(24) + Duration::milliseconds(200),
            ),
            (
                DIRECTION_INBOUND,
                now - Duration::hours(24) + Duration::milliseconds(900),
            ),
            // 当前时间窗口查询只校验时间下界（>= 窗口起点），未限制上界。
            (DIRECTION_INBOUND, now + Duration::seconds(5)),
        ] {
            insert_message(&db, direction, timestamp).await;
        }
        // 24 小时之前的历史流量尖峰应计入消息总量，但不得影响最近 24 小时的峰值 QPS。
        for _ in 0..5 {
            insert_message(
                &db,
                DIRECTION_INBOUND,
                now - Duration::hours(24) - Duration::seconds(1),
            )
            .await;
        }

        let stats = get_command_stats_with_clock(&db, || now).await.unwrap();
        assert_eq!(stats.receive, 12);
        assert_eq!(stats.send, 3);
        assert_eq!(stats.total_count, 15);
        assert_eq!(stats.current_qps, 5);
        assert_eq!(stats.max_qps, 4);
        let counts = get_dashboard_counts(&db).await.unwrap();
        assert_eq!(counts.total_messages, 15);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn counts_come_from_real_tables_including_disabled_and_nullable_rows() {
        let db = crate::test_support::test_db(2).await;
        let now = fixed_now();
        for (name, status) in [("active-user", "active"), ("disabled-user", "disabled")] {
            users::orm::ActiveModel {
                username: Set(Some(name.into())),
                username_key: Set(Some(name.into())),
                email: Set(None),
                email_verified_at_ms: Set(None),
                password_hash: Set(Some("stats-test-password-hash".into())),
                role: Set("user".into()),
                status: Set(status.into()),
                totp_enabled: Set(false),
                created_at_ms: Set(now.timestamp_millis()),
                updated_at_ms: Set(now.timestamp_millis()),
                ..Default::default()
            }
            .insert(db.orm())
            .await
            .unwrap();
        }
        for (id, enabled) in [
            ("active-group", Some(true)),
            ("disabled-group", Some(false)),
            ("legacy-group", None),
        ] {
            let group = db
                .ensure_group(&crate::groups::GroupIdentity::onebot(id), id)
                .await
                .unwrap();
            nuono_app::groups::GroupStore::toggle_group(&db, group.id, enabled.unwrap_or(true))
                .await
                .unwrap();
        }
        for timestamp in [Some(now), None] {
            audit_events::ActiveModel {
                category: Set(CATEGORY_COMMAND.to_string()),
                occurred_at_ms: Set(timestamp.map_or(0, |ts| ts.timestamp_millis())),
                bot_id: Set(Some("stats-bot".into())),
                actor_label: Set(Some("stats-user".into())),
                action: Set("/stats".into()),
                result: Set("ok".into()),
                details_json: Set("{}".into()),
                ..Default::default()
            }
            .insert(db.orm())
            .await
            .unwrap();
        }
        insert_message(&db, DIRECTION_INBOUND, now).await;
        insert_message(&db, DIRECTION_OUTBOUND, now - Duration::hours(48)).await;

        let counts = get_dashboard_counts(&db).await.unwrap();
        assert_eq!(counts.total_messages, 2);
        assert_eq!(counts.user_count, 2);
        assert_eq!(counts.group_count, 3);
        let stats = get_command_stats_with_clock(&db, || now).await.unwrap();
        assert_eq!(stats.command, 2);
        assert_eq!(stats.users, 2);
        assert_eq!(stats.groups, 3);
        assert_eq!(stats.receive, 1);
        assert_eq!(stats.send, 1);
        assert_eq!(stats.total_count, 2);
        assert_eq!(stats.current_qps, 1);
        assert_eq!(stats.max_qps, 1);
        db.shutdown().await.unwrap();
    }
}
