//! AI 对话历史持久化（基于核心表 `nuono_conversations` 与 `nuono_messages`）。

use std::collections::HashMap;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::Database;
use crate::conversation_store::{
    CONVERSATION_STATUS_ACTIVE, ConversationMessageRow, DIRECTION_INBOUND, DIRECTION_OUTBOUND,
    NewMessage, ROLE_ASSISTANT, append_message_on, decode_message_payload, encode_message_payload,
    ensure_session_conversation_on, find_session_conversation_on,
};
use crate::entity::{conversations, messages};
use crate::error::Result;

impl Database {
    #[tracing::instrument(skip(self, content), err(Debug))]
    pub async fn save_ai_conversation(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        super::account_state::lock_private_account_on(&tx, session_id).await?;
        let conv = ensure_session_conversation_on(&tx, session_id).await?;
        let (db_role, text_content, content_blocks_json) = encode_message_payload(role, content)?;
        let direction = if db_role == ROLE_ASSISTANT {
            DIRECTION_OUTBOUND
        } else {
            DIRECTION_INBOUND
        };
        let now_ms = crate::clock::db_now().timestamp_millis();
        append_message_on(
            &tx,
            NewMessage {
                conversation_id: Some(conv.id),
                platform: conv.platform,
                bot_instance_id: None,
                native_message_id: None,
                reply_to_native_message_id: None,
                sender_user_id: (direction == DIRECTION_INBOUND)
                    .then_some(conv.owner_user_id)
                    .flatten(),
                sender_identity_id: None,
                native_sender_id: None,
                native_group_id: None,
                sender_nickname: None,
                direction: direction.to_owned(),
                role: db_role.to_owned(),
                text_content,
                content_blocks_json,
                raw_payload_json: None,
                occurred_at_ms: now_ms,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn list_ai_conversations(&self) -> Result<Vec<ConversationMessageRow>> {
        let convs = conversations::Entity::find()
            .filter(conversations::Column::BotInstanceId.is_null())
            .filter(conversations::Column::Status.eq(CONVERSATION_STATUS_ACTIVE))
            .filter(
                conversations::Column::NativeChatId
                    .not_like(format!("{}%", nuono_app::agent::ACCOUNT_CHAT_PREFIX)),
            )
            .all(self.orm())
            .await?;
        if convs.is_empty() {
            return Ok(Vec::new());
        }
        let session_by_id: HashMap<String, String> = convs
            .into_iter()
            .map(|conv| (conv.id, conv.native_chat_id))
            .collect();
        let conv_ids: Vec<String> = session_by_id.keys().cloned().collect();
        let rows = messages::Entity::find()
            .filter(messages::Column::ConversationId.is_in(conv_ids))
            .order_by_asc(messages::Column::Id)
            .all(self.orm())
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let conversation_id = row.conversation_id.clone()?;
                let session_id = session_by_id.get(&conversation_id)?.clone();
                let (role, content) = decode_message_payload(&row);
                Some(ConversationMessageRow {
                    id: row.id,
                    conversation_id,
                    session_id,
                    role,
                    content,
                    timestamp: crate::clock::UtcMillis::from_millis(row.occurred_at_ms),
                })
            })
            .collect())
    }

    /// 取某会话最近 `limit` 条 AI 对话，按 id 升序返回（数据库侧过滤与截断）。
    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn recent_ai_conversations(
        &self,
        session_id: &str,
        limit: u64,
    ) -> Result<Vec<ConversationMessageRow>> {
        let Some(conv) = find_session_conversation_on(self.orm(), session_id).await? else {
            return Ok(Vec::new());
        };
        let mut rows = messages::Entity::find()
            .filter(messages::Column::ConversationId.eq(&conv.id))
            .order_by_desc(messages::Column::Id)
            .limit(limit)
            .all(self.orm())
            .await?;
        rows.reverse();
        Ok(rows
            .into_iter()
            .map(|row| {
                let (role, content) = decode_message_payload(&row);
                ConversationMessageRow {
                    id: row.id,
                    conversation_id: conv.id.clone(),
                    session_id: conv.native_chat_id.clone(),
                    role,
                    content,
                    timestamp: crate::clock::UtcMillis::from_millis(row.occurred_at_ms),
                }
            })
            .collect())
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn delete_ai_conversation_session(&self, session_id: &str) -> Result<()> {
        let tx = self.begin_write().await?;
        let convs = conversations::Entity::find()
            .filter(conversations::Column::BotInstanceId.is_null())
            .filter(conversations::Column::NativeChatId.eq(session_id))
            .all(&tx)
            .await?;
        let ids: Vec<String> = convs.into_iter().map(|c| c.id).collect();
        if !ids.is_empty() {
            messages::Entity::delete_many()
                .filter(messages::Column::ConversationId.is_in(ids.clone()))
                .exec(&tx)
                .await?;
            conversations::Entity::delete_many()
                .filter(conversations::Column::Id.is_in(ids))
                .exec(&tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
