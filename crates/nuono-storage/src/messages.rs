//! 消息历史记录显示（读取统一多平台消息表 `nuono_messages`）；趋势聚合共享数据库实现。
use sea_orm::{EntityTrait, PaginatorTrait, QueryOrder, QuerySelect};

use crate::db::Database;
use crate::entity::messages;
use crate::error::Result;
pub use crate::statistics::{MessageTrend, MessageTrendRange};

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct MessageRecord {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub platform: String,
    pub bot_id: String,
    pub conversation_id: Option<String>,
    pub message_type: String,
    pub user_id: Option<String>,
    pub group_id: Option<String>,
    pub content: Option<String>,
    pub raw_message: Option<String>,
    pub direction: String,
    pub role: String,
    pub timestamp: Option<String>,
}

pub async fn get_messages(db: &Database, limit: u64) -> Result<Vec<MessageRecord>> {
    let rows = messages::Entity::find()
        .order_by_desc(messages::Column::Id)
        .limit(limit)
        .all(db.orm())
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let message_type = if r.native_group_id.is_some() {
                "group".to_owned()
            } else if r.platform == crate::conversation_store::PLATFORM_WEB {
                "web".to_owned()
            } else {
                "private".to_owned()
            };
            MessageRecord {
                id: r.id,
                platform: r.platform,
                bot_id: r.bot_instance_id.unwrap_or_default(),
                conversation_id: r.conversation_id,
                message_type,
                user_id: r.native_sender_id,
                group_id: r.native_group_id,
                content: Some(r.text_content),
                raw_message: r.raw_payload_json,
                direction: r.direction,
                role: r.role,
                timestamp: crate::clock::UtcMillis::from_millis(r.occurred_at_ms)
                    .map(crate::clock::format_wire_ts),
            }
        })
        .collect())
}

/// 按指定自然日窗口与时区偏移聚合消息趋势（返回完整连续时间桶）。
pub async fn get_message_trend(
    db: &Database,
    range: MessageTrendRange,
    tz_offset_minutes: i32,
) -> Result<Vec<MessageTrend>> {
    db.message_trend_rows(range, tz_offset_minutes).await
}

pub async fn count_messages(db: &Database) -> Result<i64> {
    let count = messages::Entity::find().count(db.orm()).await?;
    Ok(count as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_store::{
        DIRECTION_INBOUND, DIRECTION_OUTBOUND, ROLE_ASSISTANT, ROLE_USER,
    };
    use sea_orm::{ActiveModelTrait, Set};

    #[tokio::test]
    async fn history_keeps_descending_id_order_limits_and_nullable_projection() {
        let db = crate::test_support::test_db(2).await;
        let now = crate::clock::db_now();
        for (content, occurred_at_ms) in [
            ("older row", now.timestamp_millis()),
            ("", now.timestamp_millis() - 1000),
            (
                "newest row",
                (now - chrono::Duration::hours(1)).timestamp_millis(),
            ),
        ] {
            messages::ActiveModel {
                platform: Set("onebot".into()),
                bot_instance_id: Set(Some("history-bot".into())),
                direction: Set(DIRECTION_INBOUND.into()),
                role: Set(ROLE_USER.into()),
                text_content: Set(content.into()),
                content_blocks_json: Set("[]".into()),
                occurred_at_ms: Set(occurred_at_ms),
                created_at_ms: Set(now.timestamp_millis()),
                ..Default::default()
            }
            .insert(db.orm())
            .await
            .unwrap();
        }
        assert_eq!(count_messages(&db).await.unwrap(), 3);
        assert!(get_messages(&db, 0).await.unwrap().is_empty());
        let rows = get_messages(&db, 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].id > rows[1].id);
        assert_eq!(rows[0].content.as_deref(), Some("newest row"));
        assert_eq!(
            rows[0].timestamp.as_deref(),
            Some(crate::clock::format_wire_ts(now - chrono::Duration::hours(1)).as_str())
        );
        assert_eq!(rows[1].content.as_deref(), Some(""));
        assert!(rows[1].raw_message.is_none());
        assert!(rows[1].user_id.is_none());
        assert!(rows[1].group_id.is_none());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn trend_buckets_follow_local_calendar_boundaries_across_all_four_ranges() {
        use chrono::{FixedOffset, TimeZone, Utc};

        let db = crate::test_support::test_db(3).await;
        // 模拟东八区 2026-11-22 01:36:00（即 UTC 2026-11-21 17:36:00），验证跨 UTC 午夜边界绝不错位。
        let cst = FixedOffset::east_opt(8 * 3600).unwrap();
        let now_utc = cst
            .with_ymd_and_hms(2026, 11, 22, 1, 36, 0)
            .unwrap()
            .with_timezone(&Utc);
        let at_cst = |y, m, d, hh, mm| {
            cst.with_ymd_and_hms(y, m, d, hh, mm, 0)
                .unwrap()
                .with_timezone(&Utc)
        };

        for (direction, role, timestamp) in [
            // 今天 00:15（收）与 01:20（收）、01:25（发）
            (DIRECTION_INBOUND, ROLE_USER, at_cst(2026, 11, 22, 0, 15)),
            (DIRECTION_INBOUND, ROLE_USER, at_cst(2026, 11, 22, 1, 20)),
            (
                DIRECTION_OUTBOUND,
                ROLE_ASSISTANT,
                at_cst(2026, 11, 22, 1, 25),
            ),
            // 昨天 23:30（东八区，UTC 上仍是同一天 15:30，但必须归入昨天 23:00）
            (
                DIRECTION_OUTBOUND,
                ROLE_ASSISTANT,
                at_cst(2026, 11, 21, 23, 30),
            ),
            // 5 天前（在一周内）
            (DIRECTION_INBOUND, ROLE_USER, at_cst(2026, 11, 17, 10, 0)),
            // 20 天前（在一个月内、一周外）
            (
                DIRECTION_OUTBOUND,
                ROLE_ASSISTANT,
                at_cst(2026, 11, 2, 9, 0),
            ),
            // 40 天前（超出一个月窗口）
            (DIRECTION_INBOUND, ROLE_USER, at_cst(2026, 10, 10, 12, 0)),
        ] {
            let ts_ms = timestamp.timestamp_millis();
            messages::ActiveModel {
                platform: Set("onebot".into()),
                bot_instance_id: Set(Some("trend-bot".into())),
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

        let today = db
            .message_trend_rows_at(MessageTrendRange::Today, 480, now_utc)
            .await
            .unwrap();
        assert_eq!(today.len(), 2);
        assert_eq!(today[0].time, "2026-11-22 00:00");
        assert_eq!(today[0].received, 1);
        assert_eq!(today[0].sent, 0);
        assert_eq!(today[1].time, "2026-11-22 01:00");
        assert_eq!(today[1].received, 1);
        assert_eq!(today[1].sent, 1);

        let yesterday = db
            .message_trend_rows_at(MessageTrendRange::Yesterday, 480, now_utc)
            .await
            .unwrap();
        assert_eq!(yesterday.len(), 24);
        assert_eq!(yesterday[0].time, "2026-11-21 00:00");
        assert_eq!(yesterday[23].time, "2026-11-21 23:00");
        assert_eq!(yesterday[23].received, 0);
        assert_eq!(yesterday[23].sent, 1);
        assert_eq!(yesterday.iter().map(|r| r.sent).sum::<i64>(), 1);

        let week = db
            .message_trend_rows_at(MessageTrendRange::Week, 480, now_utc)
            .await
            .unwrap();
        assert_eq!(week.len(), 7);
        assert_eq!(week[0].time, "2026-11-16");
        assert_eq!(week[1].time, "2026-11-17");
        assert_eq!(week[1].received, 1);
        assert_eq!(week[1].sent, 0);
        assert_eq!(week[5].time, "2026-11-21");
        assert_eq!(week[5].received, 0);
        assert_eq!(week[5].sent, 1);
        assert_eq!(week[6].time, "2026-11-22");
        assert_eq!(week[6].received, 2);
        assert_eq!(week[6].sent, 1);
        assert_eq!(week.iter().map(|r| r.received).sum::<i64>(), 3);
        assert_eq!(week.iter().map(|r| r.sent).sum::<i64>(), 2);

        let month = db
            .message_trend_rows_at(MessageTrendRange::Month, 480, now_utc)
            .await
            .unwrap();
        assert_eq!(month.len(), 30);
        assert_eq!(month[0].time, "2026-10-24");
        assert_eq!(month[29].time, "2026-11-22");
        assert_eq!(month.iter().map(|r| r.received).sum::<i64>(), 3);
        assert_eq!(month.iter().map(|r| r.sent).sum::<i64>(), 3);

        db.shutdown().await.unwrap();
    }
}
