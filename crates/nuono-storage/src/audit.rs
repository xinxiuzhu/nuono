use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect,
};

use crate::db::Database;
use crate::entity::audit_events;
use crate::error::Result;

mod privacy;
pub use privacy::diagnostic_event_data;

pub(crate) const CATEGORY_EVENT: &str = "event";
pub(crate) const CATEGORY_COMMAND: &str = "command";

pub(crate) async fn insert_event_on<C: ConnectionTrait>(
    conn: &C,
    event_type: &str,
    bot_id: Option<&str>,
    actor: Option<&str>,
    data: serde_json::Value,
    occurred_at: crate::clock::UtcMillis,
) -> Result<()> {
    let data = diagnostic_event_data(event_type, data).to_string();
    let am = audit_events::ActiveModel {
        category: Set(CATEGORY_EVENT.to_string()),
        occurred_at_ms: Set(occurred_at.timestamp_millis()),
        actor_label: Set(actor.map(String::from)),
        bot_id: Set(bot_id.map(String::from)),
        action: Set(event_type.to_string()),
        result: Set("ok".to_string()),
        details_json: Set(data),
        ..Default::default()
    };
    am.insert(conn).await?;
    Ok(())
}

/// 记录一条审计/运行事件。
///
/// `actor` 是必填位置参数而不是可选后缀，为的是让「这件事是谁干的」在每个调用点都必须
/// 表态。留成默认值的话，新增调用点会静默落成无主事件，而审计里最没用的记录就是
/// 「发生了角色变更，不知道谁改的」。
///
/// 系统自身触发的事件传 [`SYSTEM_ACTOR`]（即 `None`），不要传 `Some("system")` ——
/// 那样无法和一个真叫 system 的账号区分。
pub async fn record_event(
    db: &Database,
    event_type: &str,
    bot_id: Option<&str>,
    actor: Option<&str>,
    data: serde_json::Value,
) -> Result<()> {
    insert_event_on(
        db.orm(),
        event_type,
        bot_id,
        actor,
        data,
        crate::clock::db_now(),
    )
    .await
}

/// 无人类主体的事件用这个常量，读起来比裸 `None` 明确。
///
/// 适用于调度任务、到期扫描、运行时统计 —— 这些是程序自己按时间触发的，没有操作者。
pub const SYSTEM_ACTOR: Option<&str> = None;

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct CommandLogRecord {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub bot_id: String,
    pub user_id: String,
    pub command: String,
    pub timestamp: Option<String>,
}

pub async fn get_command_logs(db: &Database, limit: u64) -> Result<Vec<CommandLogRecord>> {
    let (rows, _) = get_command_logs_page(db, limit, None).await?;
    Ok(rows)
}

/// 命令日志的游标分页查询：`before_id` 为 `None` 取第一页，返回 `(本页, 是否还有更旧)`。
///
/// 与事件日志同一套 limit+1 判尾，不扫全表 count。
pub async fn get_command_logs_page(
    db: &Database,
    limit: u64,
    before_id: Option<i64>,
) -> Result<(Vec<CommandLogRecord>, bool)> {
    let mut select =
        audit_events::Entity::find().filter(audit_events::Column::Category.eq(CATEGORY_COMMAND));
    if let Some(before) = before_id {
        select = select.filter(audit_events::Column::Id.lt(before));
    }
    let rows: Vec<audit_events::Model> = select
        .order_by_desc(audit_events::Column::Id)
        .limit(limit.saturating_add(1))
        .all(db.orm())
        .await?;

    let has_more = rows.len() as u64 > limit;
    let page: Vec<CommandLogRecord> = rows
        .into_iter()
        .take(limit as usize)
        .map(command_log_record)
        .collect();
    Ok((page, has_more))
}

fn command_log_record(r: audit_events::Model) -> CommandLogRecord {
    CommandLogRecord {
        id: r.id,
        bot_id: r.bot_id.unwrap_or_default(),
        user_id: r.actor_label.unwrap_or_default(),
        command: r.action,
        timestamp: crate::clock::UtcMillis::from_millis(r.occurred_at_ms)
            .map(crate::clock::format_wire_ts),
    }
}

/// 导出的全量查询：按 id 升序，给一个明确的安全上限防止误伤。
pub async fn get_command_logs_for_export(db: &Database) -> Result<Vec<CommandLogRecord>> {
    let rows = audit_events::Entity::find()
        .filter(audit_events::Column::Category.eq(CATEGORY_COMMAND))
        .order_by_asc(audit_events::Column::Id)
        .limit(100_000)
        .all(db.orm())
        .await?;
    Ok(rows.into_iter().map(command_log_record).collect())
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct EventLogRecord {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub event_type: String,
    pub bot_id: Option<String>,
    pub data: Option<String>,
    pub timestamp: Option<String>,
    /// 操作者用户名，系统事件为 `None`。
    pub actor: Option<String>,
}

pub use nuono_app::agent::store::ProviderHealthRecord;

#[derive(Debug, Clone, Default)]
pub struct EventLogFilter {
    pub event_type: Option<String>,
    pub run_id: Option<i64>,
    /// 游标分页：只取 id 小于该值的行（配合 id 倒序）。
    pub before_id: Option<i64>,
}

pub async fn get_event_logs(db: &Database, limit: u64) -> Result<Vec<EventLogRecord>> {
    get_event_logs_filtered(db, EventLogFilter::default(), limit).await
}

pub async fn get_event_logs_filtered(
    db: &Database,
    filter: EventLogFilter,
    limit: u64,
) -> Result<Vec<EventLogRecord>> {
    let (rows, _) = get_event_logs_page(db, filter, limit).await?;
    Ok(rows)
}

fn normalize_event_type(filter: &EventLogFilter) -> Option<String> {
    filter
        .event_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// run_id 过滤等价于原 `data.contains("\"run_id\":{run_id}")`：在 JSON 文本里做子串匹配。
fn run_id_pattern(filter: &EventLogFilter) -> Option<String> {
    filter.run_id.map(|rid| format!("%\"run_id\":{rid}%"))
}

fn event_logs_select(filter: &EventLogFilter) -> sea_orm::Select<audit_events::Entity> {
    let mut select =
        audit_events::Entity::find().filter(audit_events::Column::Category.eq(CATEGORY_EVENT));
    if let Some(event_type) = normalize_event_type(filter) {
        select = select.filter(audit_events::Column::Action.eq(event_type));
    }
    if let Some(pattern) = run_id_pattern(filter) {
        select = select.filter(audit_events::Column::DetailsJson.like(pattern));
    }
    if let Some(before) = filter.before_id {
        select = select.filter(audit_events::Column::Id.lt(before));
    }
    select
}

fn event_log_record(r: audit_events::Model) -> EventLogRecord {
    let data = serde_json::from_str(&r.details_json)
        .map(|value| diagnostic_event_data(&r.action, value).to_string())
        .unwrap_or_else(|_| "{}".into());
    EventLogRecord {
        id: r.id,
        event_type: r.action,
        bot_id: r.bot_id,
        data: Some(data),
        timestamp: crate::clock::UtcMillis::from_millis(r.occurred_at_ms)
            .map(crate::clock::format_wire_ts),
        actor: r.actor_label,
    }
}

/// 游标分页查询：返回 `(本页记录, 是否还有更旧的数据)`。
///
/// 多取一条（limit+1）来判断 has_more，而不是 `count(*)` —— 后者在过滤条件下
/// 要扫全表，翻页成本随数据量线性上升。
pub async fn get_event_logs_page(
    db: &Database,
    filter: EventLogFilter,
    limit: u64,
) -> Result<(Vec<EventLogRecord>, bool)> {
    let rows: Vec<audit_events::Model> = event_logs_select(&filter)
        .order_by_desc(audit_events::Column::Id)
        .limit(limit.saturating_add(1))
        .all(db.orm())
        .await?;

    let has_more = rows.len() as u64 > limit;
    let page: Vec<EventLogRecord> = rows
        .into_iter()
        .take(limit as usize)
        .map(event_log_record)
        .collect();
    Ok((page, has_more))
}

/// 导出的全量查询：按 id 升序（时间正序），给一个明确的安全上限防止误伤。
pub async fn get_event_logs_for_export(
    db: &Database,
    filter: EventLogFilter,
) -> Result<Vec<EventLogRecord>> {
    let rows = event_logs_select(&filter)
        .order_by_asc(audit_events::Column::Id)
        .limit(100_000)
        .all(db.orm())
        .await?;

    Ok(rows.into_iter().map(event_log_record).collect())
}

/// 删除超过保留天数的命令日志。
pub async fn delete_command_logs_older_than(db: &Database, days: u64) -> Result<u64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
    let result = audit_events::Entity::delete_many()
        .filter(audit_events::Column::Category.eq(CATEGORY_COMMAND))
        .filter(audit_events::Column::OccurredAtMs.lt(cutoff.timestamp_millis()))
        .exec(db.orm())
        .await?;
    Ok(result.rows_affected)
}

/// 删除超过保留天数的事件日志。
pub async fn delete_event_logs_older_than(db: &Database, days: u64) -> Result<u64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
    let result = audit_events::Entity::delete_many()
        .filter(audit_events::Column::Category.eq(CATEGORY_EVENT))
        .filter(audit_events::Column::OccurredAtMs.lt(cutoff.timestamp_millis()))
        .exec(db.orm())
        .await?;
    Ok(result.rows_affected)
}

pub async fn list_provider_health(db: &Database, limit: u64) -> Result<Vec<ProviderHealthRecord>> {
    let events = get_event_logs(db, limit.clamp(50, 1000)).await?;
    let mut health = std::collections::HashMap::<(String, String), ProviderHealthRecord>::new();

    for event in events {
        if !matches!(
            event.event_type.as_str(),
            "agent.provider_attempt"
                | "agent.provider_selected"
                | "agent.provider_failed"
                | "agent.provider_timeout"
        ) {
            continue;
        }

        let Some(data) = event
            .data
            .as_deref()
            .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        else {
            continue;
        };
        let Some(provider) = data.get("provider").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(model) = data.get("model").and_then(|v| v.as_str()) else {
            continue;
        };

        let status = data.get("status").and_then(|v| v.as_str()).unwrap_or(
            match event.event_type.as_str() {
                "agent.provider_failed" => "failed",
                "agent.provider_timeout" => "timeout",
                "agent.provider_selected" => "selected",
                _ => "succeeded",
            },
        );
        let error = data
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| data.get("error_kind").and_then(|v| v.as_str()))
            .map(ToOwned::to_owned);
        let elapsed_ms = data.get("elapsed_ms").and_then(|v| v.as_u64());

        let record = health
            .entry((provider.to_string(), model.to_string()))
            .or_insert_with(|| ProviderHealthRecord {
                provider: provider.to_string(),
                model: model.to_string(),
                attempts: 0,
                successes: 0,
                failures: 0,
                timeouts: 0,
                selected: 0,
                success_rate: 0.0,
                last_status: None,
                last_error: None,
                last_elapsed_ms: None,
                last_seen: None,
            });

        match event.event_type.as_str() {
            "agent.provider_attempt" => {
                record.attempts += 1;
                record.successes += 1;
            }
            "agent.provider_failed" => {
                record.attempts += 1;
                record.failures += 1;
            }
            "agent.provider_timeout" => {
                record.attempts += 1;
                record.timeouts += 1;
            }
            "agent.provider_selected" => {
                record.selected += 1;
            }
            _ => {}
        }

        if record.last_seen.is_none() && event.event_type != "agent.provider_selected" {
            record.last_status = Some(status.to_string());
            record.last_error = error;
            record.last_elapsed_ms = elapsed_ms;
            record.last_seen = event.timestamp;
        }
    }

    let mut records = health.into_values().collect::<Vec<_>>();
    for record in &mut records {
        if record.attempts > 0 {
            record.success_rate = record.successes as f64 / record.attempts as f64;
        }
    }
    records.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    Ok(records)
}

#[async_trait::async_trait]
impl nuono_app::action::ActionAudit for Database {
    async fn record(
        &self,
        action: &nuono_app::transport::TransportAction,
        authority: &nuono_app::action::ActionAuthority,
        phase: &str,
        error: Option<&nuono_domain::ActionError>,
    ) -> std::result::Result<(), nuono_domain::ActionError> {
        // 只有实际执行完成（completed）的消息发送动作才计入发送历史；started/failed 阶段或
        // 非发消息动作不得虚增发送计数。历史表仅记录接收方路由元数据，不保存媒体二进制、消息正文或原始参数。
        if phase == "completed"
            && error.is_none()
            && let Some(mut metadata) =
                outbound_message_metadata(&action.action, action.bot.platform())
        {
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "platform".into(),
                    serde_json::Value::String(action.bot.platform().as_str().to_owned()),
                );
                let instance_id = action.bot.instance_id.as_str();
                if !instance_id.is_empty() {
                    obj.insert(
                        "bot_instance_id".into(),
                        serde_json::Value::String(instance_id.to_owned()),
                    );
                }
            }
            // 历史入队失败由内部指标统一计数；消息本身已发送成功，此处不重试、也不逐条刷警告日志。
            let _ = self
                .save_message_history(
                    action.bot.account_id.value().as_str(),
                    &metadata,
                    "outbound",
                )
                .await;
        }
        // 审计日志仅记录动作类型与路由元数据，避免将私聊内容或平台鉴权密钥写入审计表。
        let kind = match &action.action {
            nuono_domain::OutboundAction::SendMessage { .. } => "send_message",
            nuono_domain::OutboundAction::DeleteMessage { .. } => "delete_message",
            nuono_domain::OutboundAction::Query { .. } => "query",
            nuono_domain::OutboundAction::RawPlatform { .. } => "raw_platform",
            _ => "other",
        };
        record_event(self,"action.execution",Some(action.bot.account_id.value().as_str()),Some(&authority.actor()),serde_json::json!({"instance_id":action.bot.instance_id,"platform":action.bot.platform(),"operation":kind,"phase":phase,"error_kind":error.map(|e|format!("{:?}",e.kind))})).await.map_err(|e|nuono_domain::ActionError::new(nuono_domain::ActionErrorKind::Rejected,format!("action audit unavailable: {}",e.kind())))
    }
}

/// 仅提取出站消息的接收方路由元数据，绝不直接序列化原始动作或消息内容载荷。
fn outbound_message_metadata(
    action: &nuono_domain::OutboundAction,
    platform: &nuono_domain::Platform,
) -> Option<serde_json::Value> {
    use nuono_domain::{ConversationRef, DeliveryTarget, OutboundAction, Platform};
    use serde_json::json;

    fn target_metadata(target: &DeliveryTarget) -> serde_json::Value {
        match target {
            DeliveryTarget::Actor { actor } => {
                json!({"message_type":"private", "user_id":actor.value(), "message":"", "raw_message":""})
            }
            DeliveryTarget::Conversation { conversation } => match conversation {
                ConversationRef::Direct { participant, .. } => {
                    json!({"message_type":"private", "user_id":participant.value(), "message":"", "raw_message":""})
                }
                ConversationRef::Group { group } => {
                    json!({"message_type":"group", "group_id":group.value(), "message":"", "raw_message":""})
                }
                ConversationRef::Channel { channel, .. } => {
                    channel_metadata(json!(channel.value()))
                }
                _ => json!({"message_type":"unknown", "message":"", "raw_message":""}),
            },
            DeliveryTarget::Channel { channel, .. } => channel_metadata(json!(channel.value())),
            _ => json!({"message_type":"unknown", "message":"", "raw_message":""}),
        }
    }

    fn channel_metadata(id: serde_json::Value) -> serde_json::Value {
        // message_history 表没有独立的 channel_id 列，因此将频道 ID 同时写入 group_id 槽位与 JSON 元数据，与入站消息兼容层保持一致。
        json!({"message_type":"channel", "group_id":id, "channel_id":id, "message":"", "raw_message":""})
    }

    fn recipient(params: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
        match params.get(key)? {
            serde_json::Value::String(value) if !value.trim().is_empty() => Some(json!(value)),
            serde_json::Value::Number(value) if value.is_i64() || value.is_u64() => {
                Some(json!(value))
            }
            _ => None,
        }
    }

    fn platform_recipient(
        platform: &Platform,
        action: &str,
        params: &serde_json::Value,
        message_type: &str,
    ) -> Option<serde_json::Value> {
        // 仅匹配插件 SDK 已知的发送消息接收方字段名；特定平台的参数别名严格按平台与动作匹配，不能套用到其他平台适配器或未知动作上。
        let keys: &[&str] = match (platform, action, message_type) {
            (Platform::Feishu, _, "private") => &["chat_id", "channel_id", "user_id"],
            (Platform::Feishu, _, "group") => &["chat_id", "channel_id", "group_id"],
            (Platform::Feishu, _, "channel") => &["chat_id", "channel_id"],
            (Platform::Discord, _, "group") => &["channel_id", "group_id", "chat_id"],
            (Platform::Discord, _, "channel") => &["channel_id", "group_id", "chat_id"],
            (Platform::QqOfficial, "send_private_msg", _) => &["user_id", "openid"],
            (Platform::QqOfficial, "send_group_msg", _) => &["group_openid", "group_id"],
            (Platform::QqOfficial, _, "private") => &["openid", "user_id"],
            (Platform::QqOfficial, _, "group") => &["group_openid", "group_id"],
            (Platform::Telegram, _, "private") => &["user_id", "chat_id"],
            (Platform::Telegram, _, "group") => &["group_id", "chat_id"],
            (Platform::Telegram, _, "channel") => &["chat_id", "channel_id"],
            (_, _, "private") => &["user_id"],
            (_, _, "group") => &["group_id"],
            (_, _, "channel") => &["channel_id"],
            _ => return None,
        };
        keys.iter().find_map(|key| recipient(params, key))
    }

    match action {
        OutboundAction::SendMessage { target, .. } => Some(target_metadata(target)),
        OutboundAction::RawPlatform { action, params } => {
            let message_type = match action.as_str() {
                "send_group_msg" => "group",
                "send_private_msg" => "private",
                "send_guild_channel_msg" => "channel",
                "send_msg" => match params
                    .get("message_type")
                    .and_then(serde_json::Value::as_str)
                {
                    Some("private") => "private",
                    Some("group") => "group",
                    Some("channel") | Some("guild") => "channel",
                    None if recipient(params, "group_id").is_some() => "group",
                    None if recipient(params, "user_id").is_some() => "private",
                    _ => return None,
                },
                _ => return None,
            };
            let recipient = platform_recipient(platform, action, params, message_type)?;
            match message_type {
                "group" => Some(json!({
                    "message_type":"group", "group_id":recipient,
                    "message":"", "raw_message":""
                })),
                "private" => Some(json!({
                    "message_type":"private", "user_id":recipient,
                    "message":"", "raw_message":""
                })),
                "channel" => Some(channel_metadata(recipient)),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "../tests/unit/outbound_stats_tests.rs"]
mod outbound_stats_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, DbBackend, QueryTrait, Statement};

    async fn test_db() -> Database {
        crate::test_support::test_db(2).await
    }

    async fn insert_events(db: &Database, n: i32) {
        for i in 0..n {
            record_event(
                db,
                "test.audit_page",
                None,
                Some("tester"),
                serde_json::json!({ "seq": i }),
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn sqlite_audit_pagination_retention_and_memory_scope_use_indexes() {
        let db = test_db().await;
        if db.backend() != DbBackend::Sqlite {
            db.shutdown().await.unwrap();
            return;
        }
        db.ensure_ai_module().await.unwrap();
        let mut page = event_logs_select(&EventLogFilter {
            event_type: Some("test.audit_page".into()),
            before_id: Some(100),
            ..Default::default()
        })
        .order_by_desc(audit_events::Column::Id)
        .limit(10)
        .build(DbBackend::Sqlite);
        page.sql = format!("EXPLAIN QUERY PLAN {}", page.sql);
        for (query, index) in [
            (page, "idx_nuono_audit_events_category_action_id"),
            (
                Statement::from_string(
                    DbBackend::Sqlite,
                    "EXPLAIN QUERY PLAN DELETE FROM nuono_audit_events WHERE category='event' AND occurred_at_ms < 100",
                ),
                "idx_nuono_audit_events_category_time",
            ),
            (
                Statement::from_string(
                    DbBackend::Sqlite,
                    "EXPLAIN QUERY PLAN SELECT id FROM nuono_agent_memories WHERE scope_type='user' AND scope_key='scope'",
                ),
                "idx_agent_memories_unique_key",
            ),
            (
                Statement::from_string(
                    DbBackend::Sqlite,
                    "EXPLAIN QUERY PLAN SELECT id FROM nuono_messages WHERE conversation_id='conv' ORDER BY id DESC LIMIT 20",
                ),
                "idx_nuono_messages_conversation",
            ),
        ] {
            let plan: Vec<String> = db
                .orm()
                .query_all_raw(query)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.try_get("", "detail").unwrap())
                .collect();
            assert!(
                plan.iter()
                    .any(|detail| detail.contains("SEARCH") && detail.contains(index)),
                "{plan:?}"
            );
            assert!(
                !plan.iter().any(|detail| detail.contains("TEMP B-TREE")),
                "{plan:?}"
            );
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn event_logs_paginate_by_before_id_without_overlap() {
        let db = test_db().await;
        insert_events(&db, 5).await;

        let (page1, has1) = get_event_logs_page(&db, EventLogFilter::default(), 2)
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert!(has1);
        assert!(page1[0].id > page1[1].id);

        let before = page1[1].id;
        let (page2, has2) = get_event_logs_page(
            &db,
            EventLogFilter {
                before_id: Some(before),
                ..Default::default()
            },
            2,
        )
        .await
        .unwrap();
        assert_eq!(page2.len(), 2);
        assert!(has2);
        assert!(page2.iter().all(|r| r.id < before));

        let before2 = page2[1].id;
        let (page3, has3) = get_event_logs_page(
            &db,
            EventLogFilter {
                before_id: Some(before2),
                ..Default::default()
            },
            2,
        )
        .await
        .unwrap();
        assert_eq!(page3.len(), 1);
        assert!(!has3);

        // 三页合起来恰好是全部 5 条且无重复。
        let mut ids: Vec<i64> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 5);
    }

    #[tokio::test]
    async fn export_orders_ascending_and_respects_filter() {
        let db = test_db().await;
        insert_events(&db, 3).await;
        record_event(
            &db,
            "test.audit_other",
            None,
            Some("tester"),
            serde_json::json!({}),
        )
        .await
        .unwrap();

        let all = get_event_logs_for_export(&db, EventLogFilter::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.windows(2).all(|w| w[0].id < w[1].id));

        let filtered = get_event_logs_for_export(
            &db,
            EventLogFilter {
                event_type: Some("test.audit_page".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(filtered.len(), 3);
        assert!(filtered.iter().all(|r| r.event_type == "test.audit_page"));
    }

    #[tokio::test]
    async fn retention_deletes_only_old_rows() {
        let db = test_db().await;
        // 一条 30 天前的旧日志（直接落库以控制时间戳）。
        let old_ts = crate::clock::db_now() - chrono::Duration::days(30);
        let am = audit_events::ActiveModel {
            category: Set(CATEGORY_EVENT.to_string()),
            occurred_at_ms: Set(old_ts.timestamp_millis()),
            action: Set("test.audit_old".to_string()),
            bot_id: Set(None),
            details_json: Set("{}".to_string()),
            actor_label: Set(None),
            result: Set("ok".to_string()),
            ..Default::default()
        };
        audit_events::Entity::insert(am)
            .exec(db.orm())
            .await
            .unwrap();
        insert_events(&db, 2).await;

        let removed = delete_event_logs_older_than(&db, 7).await.unwrap();
        assert_eq!(removed, 1);

        let remaining = get_event_logs(&db, 100).await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|r| r.event_type == "test.audit_page"));

        // 更大的保留窗口不误删：新数据仍在。
        let removed2 = delete_event_logs_older_than(&db, 7).await.unwrap();
        assert_eq!(removed2, 0);
    }

    #[tokio::test]
    async fn command_logs_paginate_without_overlap() {
        let db = test_db().await;
        for i in 0..5 {
            let am = audit_events::ActiveModel {
                category: Set(CATEGORY_COMMAND.to_string()),
                occurred_at_ms: Set(crate::clock::db_now().timestamp_millis()),
                bot_id: Set(Some("bot1".to_string())),
                actor_label: Set(Some("tester".to_string())),
                action: Set(format!("/cmd{i}")),
                result: Set("ok".to_string()),
                details_json: Set("{}".to_string()),
                ..Default::default()
            };
            audit_events::Entity::insert(am)
                .exec(db.orm())
                .await
                .unwrap();
        }

        let (page1, has1) = get_command_logs_page(&db, 2, None).await.unwrap();
        assert_eq!(page1.len(), 2);
        assert!(has1);
        assert!(page1[0].id > page1[1].id);

        let (page2, has2) = get_command_logs_page(&db, 2, Some(page1[1].id))
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert!(has2);
        assert!(page2.iter().all(|r| r.id < page1[1].id));

        let (page3, has3) = get_command_logs_page(&db, 2, Some(page2[1].id))
            .await
            .unwrap();
        assert_eq!(page3.len(), 1);
        assert!(!has3);

        let mut ids: Vec<i64> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .map(|r| r.id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 5);

        // 导出升序且包含全部。
        let export = get_command_logs_for_export(&db).await.unwrap();
        assert_eq!(export.len(), 5);
        assert!(export.windows(2).all(|w| w[0].id < w[1].id));

        db.shutdown().await.unwrap();
    }
}
