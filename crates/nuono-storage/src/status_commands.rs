//! 状态卡片统计所需的精简命令触发元数据落盘。
//!
//! 调用方仅传入已声明的命令名称或匹配模式，绝不落盘运行时命令参数、消息正文、密钥凭据或插件回调执行结果。

use sea_orm::{ActiveModelTrait, Set};

use crate::Database;
use crate::audit::CATEGORY_COMMAND;
use crate::entity::audit_events;
use crate::error::{NuonoError, Result};

/// 兼容既有命令日志表结构，将命令名限制在 100 个字符以内。
pub const MAX_STATUS_COMMAND_CHARS: usize = 100;
/// 原样保留平台原生机器人 ID 与用户 ID，并给予显著高于普通平台 ID 的字节上限（1024 字节）；
/// 超长 ID 直接报错拒绝而非静默截断，防止不同用户的超长 ID 被截断成相同前缀而在状态统计中混淆合并。
const MAX_STATUS_ID_BYTES: usize = 1024;

pub async fn record_status_command(
    db: &Database,
    bot_id: &str,
    user_id: &str,
    command: &str,
) -> Result<()> {
    for (name, value) in [("bot_id", bot_id), ("user_id", user_id)] {
        if value.trim().is_empty()
            || value.len() > MAX_STATUS_ID_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(NuonoError::InvalidInput(format!(
                "状态指令统计的 {name} 为空、过长或含控制字符"
            )));
        }
    }
    let command: String = command
        .chars()
        .take(MAX_STATUS_COMMAND_CHARS)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if command.trim().is_empty() {
        return Err(NuonoError::InvalidInput(
            "状态指令统计的 command 为空".into(),
        ));
    }
    audit_events::ActiveModel {
        category: Set(CATEGORY_COMMAND.to_string()),
        occurred_at_ms: Set(crate::clock::db_now().timestamp_millis()),
        bot_id: Set(Some(bot_id.to_owned())),
        actor_label: Set(Some(user_id.to_owned())),
        action: Set(command),
        result: Set("ok".to_string()),
        details_json: Set("{}".to_string()),
        ..Default::default()
    }
    .insert(db.orm())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};

    #[tokio::test]
    async fn entry_persists_only_declared_metadata_and_timestamp() {
        let db = crate::test_support::test_db(2).await;
        let before = crate::clock::db_now();
        record_status_command(&db, "bot-a", "actor-b", "weather:forecast")
            .await
            .unwrap();
        let row = audit_events::Entity::find()
            .filter(audit_events::Column::Category.eq(CATEGORY_COMMAND))
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.bot_id.as_deref(), Some("bot-a"));
        assert_eq!(row.actor_label.as_deref(), Some("actor-b"));
        assert_eq!(row.action, "weather:forecast");
        assert!(row.occurred_at_ms >= before.timestamp_millis());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_identifiers_are_preserved_and_unicode_names_fit_schema() {
        let db = crate::test_support::test_db(2).await;
        let bot_id = format!("native-account-{}", "a".repeat(240));
        let user_id = format!("native-user-{}", "b".repeat(240));
        let command = format!("示例工具:{}", "公开指令".repeat(80));
        record_status_command(&db, &bot_id, &user_id, &command)
            .await
            .unwrap();
        let row = audit_events::Entity::find()
            .filter(audit_events::Column::Category.eq(CATEGORY_COMMAND))
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.bot_id.as_deref(), Some(bot_id.as_str()));
        assert_eq!(row.actor_label.as_deref(), Some(user_id.as_str()));
        assert_eq!(row.action.chars().count(), MAX_STATUS_COMMAND_CHARS);
        assert!(command.starts_with(&row.action));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_identity_is_not_truncated_into_an_existing_scope() {
        let db = crate::test_support::test_db(2).await;
        for identity in [
            String::new(),
            "x".repeat(MAX_STATUS_ID_BYTES + 1),
            "native\0user".into(),
        ] {
            assert!(
                record_status_command(&db, "bot-a", &identity, "plugin:command")
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            audit_events::Entity::find()
                .filter(audit_events::Column::Category.eq(CATEGORY_COMMAND))
                .count(db.orm())
                .await
                .unwrap(),
            0
        );
        db.shutdown().await.unwrap();
    }
}
