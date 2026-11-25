//! 按陪伴角色卡维度隔离的私聊对话查询与隐私数据清理；作用域始终绑定系统账号与角色卡，绝不使用外部聊天平台的用户 ID。
use std::collections::HashMap;

use super::Database;
use crate::{
    conversation_store::{
        ConversationMessageRow, decode_message_payload, matching_scope_conversation_ids_on,
    },
    entity::{ai_runs, conversations, messages},
    error::Result,
};
use nuono_app::agent::companion_scope_key;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

impl Database {
    pub async fn companion_conversations(
        &self,
        user_id: i64,
        card_id: &str,
        limit: u64,
    ) -> Result<Vec<ConversationMessageRow>> {
        self.companion_card(user_id, card_id).await?;
        let prefix = format!("{}:context:", companion_scope_key(user_id, card_id));
        let convs = conversations::Entity::find()
            .filter(conversations::Column::BotInstanceId.is_null())
            .filter(conversations::Column::NativeChatId.starts_with(prefix))
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
        let mut rows = messages::Entity::find()
            .filter(messages::Column::ConversationId.is_in(conv_ids))
            .order_by_desc(messages::Column::Id)
            .limit(limit.min(200))
            .all(self.orm())
            .await?;
        rows.reverse();
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
}

impl Database {
    /// 执行陪伴角色私聊与记忆数据的隐私删除（即使在恢复旧数据库快照后也可安全重放）。
    /// 调用方需先在数据库外部持久化该删除指令并隔离活跃执行协程，本事务负责与对话准入及结算互斥。
    pub async fn apply_companion_deletion(
        &self,
        deletion: &nuono_app::companion_data::CompanionDeletion,
    ) -> Result<()> {
        use crate::entity::{
            account_assistant, agent_memories, agent_memory_graphs, agent_runs, assistant_turns,
            users::orm as users,
        };
        use nuono_app::companion_data::DeletionKind;
        use sea_orm::{ActiveModelTrait, Condition, DbBackend, Set, sea_query::Expr};
        deletion.validate()?;
        let ai_installed = self.is_ai_module_installed().await?;
        let tx = self.begin_write().await?;
        let mut user = users::Entity::find_by_id(deletion.user_id);
        if self.backend() == DbBackend::Postgres {
            user = user.lock_exclusive();
        }
        if user.one(&tx).await?.is_none() {
            return Ok(());
        }
        if !ai_installed {
            if !matches!(deletion.kind, DeletionKind::Forget { .. }) {
                let through_ms = deletion.through.timestamp_millis();
                for scope in &deletion.scope_keys {
                    let conv_ids = matching_scope_conversation_ids_on(&tx, scope).await?;
                    if !conv_ids.is_empty() {
                        messages::Entity::delete_many()
                            .filter(messages::Column::ConversationId.is_in(conv_ids.clone()))
                            .filter(messages::Column::OccurredAtMs.lte(through_ms))
                            .exec(&tx)
                            .await?;
                        ai_runs::Entity::delete_many()
                            .filter(ai_runs::Column::ConversationId.is_in(conv_ids.clone()))
                            .filter(ai_runs::Column::CreatedAtMs.lte(through_ms))
                            .exec(&tx)
                            .await?;
                        conversations::Entity::update_many()
                            .filter(conversations::Column::Id.is_in(conv_ids))
                            .col_expr(
                                conversations::Column::SummaryText,
                                Expr::value(Option::<String>::None),
                            )
                            .col_expr(
                                conversations::Column::Title,
                                Expr::value(Option::<String>::None),
                            )
                            .exec(&tx)
                            .await?;
                    }
                }
            }
            tx.commit().await?;
            return Ok(());
        }
        let mut account = account_assistant::Entity::find_by_id(deletion.user_id)
            .one(&tx)
            .await?;
        let mut epochs = match &account {
            Some(row) => row.context_epochs()?,
            None => std::collections::BTreeMap::new(),
        };
        for scope in &deletion.scope_keys {
            let entry = epochs.entry(scope.clone()).or_insert(0);
            *entry = (*entry).max(deletion.epoch());
            let old_turns = assistant_turns::Entity::find()
                .filter(assistant_turns::Column::UserId.eq(deletion.user_id))
                .filter(assistant_turns::Column::ScopeKey.eq(scope))
                .filter(
                    assistant_turns::Column::CreatedAt.lte((deletion.through).timestamp_millis()),
                )
                .all(&tx)
                .await?;
            // 仍在运行中的对话轮次退还一次当日预扣额度；脱敏后的对话回执仅保留幂等去重与额度统计元数据。
            for turn in &old_turns {
                if turn.status == "running" {
                    if let Some(row) = account
                        .as_mut()
                        .filter(|row| row.usage_day == turn.quota_day)
                    {
                        let mut keys: Vec<String> = serde_json::from_str(&row.request_ids)?;
                        keys.retain(|key| key != &format!("turn:{}", turn.id));
                        row.request_ids = serde_json::to_string(&keys)?;
                    }
                    assistant_turns::ActiveModel {
                        id: Set(turn.id.clone()),
                        status: Set("interrupted".into()),
                        failure: Set(Some("interrupted".into())),
                        ..Default::default()
                    }
                    .update(&tx)
                    .await?;
                }
                agent_runs::Entity::delete_by_id(nuono_app::assistant::assistant_run_id(
                    deletion.user_id,
                    &turn.id,
                ))
                .exec(&tx)
                .await?;
            }
            // 早期的通用私聊运行检查点即使没有关联 turn 回执，也包含私人对话内容，需一并清除。
            agent_runs::Entity::delete_many()
                .filter(
                    Condition::any()
                        .add(agent_runs::Column::SessionId.eq(scope))
                        .add(
                            agent_runs::Column::SessionId.starts_with(format!("{scope}:context:")),
                        ),
                )
                .filter(agent_runs::Column::UpdatedAt.lte((deletion.through).timestamp_millis()))
                .exec(&tx)
                .await?;
            // 工具执行回执与审批记录保留原始操作者与执行结果状态，
            // 但清空私聊上下文、提示词及工具输入输出载荷（包括因账号合并迁移过来的历史记录）。
            use crate::entity::{
                agent_tool_approvals as approvals, agent_tool_receipts as receipts,
            };
            receipts::Entity::update_many()
                .filter(
                    Condition::any()
                        .add(receipts::Column::SessionId.eq(scope))
                        .add(receipts::Column::SessionId.starts_with(format!("{scope}:context:"))),
                )
                .filter(receipts::Column::CreatedAt.lte(deletion.through.timestamp_millis()))
                .col_expr(receipts::Column::ContextJson, Expr::value(None::<String>))
                .col_expr(receipts::Column::InputJson, Expr::value(None::<String>))
                .col_expr(receipts::Column::OutputJson, Expr::value(None::<String>))
                .col_expr(receipts::Column::Error, Expr::value(None::<String>))
                .exec(&tx)
                .await?;
            let private_approvals = || {
                approvals::Entity::update_many()
                    .filter(
                        Condition::any()
                            .add(approvals::Column::SessionId.eq(scope))
                            .add(
                                approvals::Column::SessionId
                                    .starts_with(format!("{scope}:context:")),
                            ),
                    )
                    .filter(approvals::Column::CreatedAt.lte(deletion.through.timestamp_millis()))
            };
            private_approvals()
                .filter(approvals::Column::Status.is_in(["pending", "approved", "executing"]))
                .col_expr(approvals::Column::Status, Expr::value("interrupted"))
                .exec(&tx)
                .await?;
            private_approvals()
                .col_expr(approvals::Column::ContextJson, Expr::value(None::<String>))
                .col_expr(approvals::Column::InputJson, Expr::value(None::<String>))
                .col_expr(
                    approvals::Column::ExecutionJson,
                    Expr::value(None::<String>),
                )
                .col_expr(approvals::Column::Reason, Expr::value(None::<String>))
                .col_expr(
                    approvals::Column::DecisionReason,
                    Expr::value(None::<String>),
                )
                .exec(&tx)
                .await?;
            let old_receipts = || {
                assistant_turns::Entity::update_many()
                    .filter(assistant_turns::Column::UserId.eq(deletion.user_id))
                    .filter(assistant_turns::Column::ScopeKey.eq(scope))
                    .filter(
                        assistant_turns::Column::CreatedAt
                            .lte((deletion.through).timestamp_millis()),
                    )
            };
            if !matches!(deletion.kind, DeletionKind::Forget { .. }) {
                let conv_ids = matching_scope_conversation_ids_on(&tx, scope).await?;
                if !conv_ids.is_empty() {
                    let through_ms = deletion.through.timestamp_millis();
                    messages::Entity::delete_many()
                        .filter(messages::Column::ConversationId.is_in(conv_ids.clone()))
                        .filter(messages::Column::OccurredAtMs.lte(through_ms))
                        .exec(&tx)
                        .await?;
                    ai_runs::Entity::delete_many()
                        .filter(ai_runs::Column::ConversationId.is_in(conv_ids.clone()))
                        .filter(ai_runs::Column::CreatedAtMs.lte(through_ms))
                        .exec(&tx)
                        .await?;
                    conversations::Entity::update_many()
                        .filter(conversations::Column::Id.is_in(conv_ids))
                        .col_expr(
                            conversations::Column::SummaryText,
                            Expr::value(Option::<String>::None),
                        )
                        .col_expr(
                            conversations::Column::Title,
                            Expr::value(Option::<String>::None),
                        )
                        .exec(&tx)
                        .await?;
                }
                old_receipts()
                    .col_expr(assistant_turns::Column::Text, Expr::value(""))
                    .col_expr(
                        assistant_turns::Column::Reply,
                        Expr::value(Option::<String>::None),
                    )
                    .col_expr(assistant_turns::Column::PromptSnapshot, Expr::value(""))
                    .col_expr(assistant_turns::Column::Redacted, Expr::value(true))
                    .col_expr(assistant_turns::Column::Delivery, Expr::value("none"))
                    .exec(&tx)
                    .await?;
            } else {
                // 清空未成功的旧请求原文与提示词快照，防止用户后续点击重试把已要求遗忘的文本重新带回上下文。
                old_receipts()
                    .filter(assistant_turns::Column::Status.ne("succeeded"))
                    .col_expr(assistant_turns::Column::Text, Expr::value(""))
                    .col_expr(assistant_turns::Column::PromptSnapshot, Expr::value(""))
                    .col_expr(assistant_turns::Column::Redacted, Expr::value(true))
                    .col_expr(assistant_turns::Column::Delivery, Expr::value("none"))
                    .exec(&tx)
                    .await?;
            }
            let memory_query = || {
                agent_memories::Entity::delete_many()
                    .filter(agent_memories::Column::ScopeType.eq("user"))
                    .filter(agent_memories::Column::ScopeKey.eq(scope))
                    .filter(
                        agent_memories::Column::UpdatedAt
                            .lte((deletion.through).timestamp_millis()),
                    )
            };
            match &deletion.kind {
                DeletionKind::All => {
                    memory_query().exec(&tx).await?;
                }
                DeletionKind::Forget { memory_ids } => {
                    // 派生的经历摘要（episode）可能在其他 memory_key 下复述同一事实；
                    // 必须把关联同一对话来源（source_id）的所有派生记忆一并擦除，避免被遗忘的内容残留在摘要里。
                    use crate::entity::companion_memory_sources as sources;
                    let selected_sources = sea_orm::sea_query::Query::select()
                        .column(sources::Column::SourceId)
                        .from(sources::Entity)
                        .and_where(sources::Column::ScopeKey.eq(scope))
                        .and_where(sources::Column::MemoryId.is_in(memory_ids.iter().copied()))
                        .to_owned();
                    // 关联的来源边数可能超过数据库单条语句的参数绑定上限；
                    // 因此先在 SQL 内用子查询完成关联并查出所有去重后的 memory_id，再分批执行级联删除。
                    let mut derived = sources::Entity::find()
                        .select_only()
                        .column(sources::Column::MemoryId)
                        .distinct()
                        .filter(sources::Column::ScopeKey.eq(scope))
                        .filter(sources::Column::SourceId.in_subquery(selected_sources))
                        .into_tuple::<i64>()
                        .all(&tx)
                        .await?;
                    derived.extend(memory_ids.iter().copied());
                    derived.sort_unstable();
                    derived.dedup();
                    for ids in derived.chunks(256) {
                        memory_query()
                            .filter(agent_memories::Column::Id.is_in(ids.iter().copied()))
                            .exec(&tx)
                            .await?;
                    }
                }
                DeletionKind::Conversation => {
                    use crate::entity::companion_memory_sources as sources;
                    sources::Entity::update_many()
                        .filter(sources::Column::ScopeKey.eq(scope))
                        .filter(
                            sources::Column::ObservedAt.lte(deletion.through.timestamp_millis()),
                        )
                        .col_expr(sources::Column::Original, Expr::value("reclaimed"))
                        .col_expr(sources::Column::Detail, Expr::value("summary"))
                        .exec(&tx)
                        .await?;
                }
            }
            // 记忆图谱缓存中直接内嵌了派生的明文片段，仅使指纹失效不够安全，必须直接物理删除整张图谱缓存。
            agent_memory_graphs::Entity::delete_many()
                .filter(agent_memory_graphs::Column::ScopeType.eq("user"))
                .filter(agent_memory_graphs::Column::ScopeKey.eq(scope))
                .exec(&tx)
                .await?;
        }
        let epochs_json = serde_json::to_string(&epochs)?;
        if let Some(row) = account {
            account_assistant::ActiveModel {
                user_id: Set(row.user_id),
                request_ids: Set(row.request_ids),
                context_epochs_json: Set(epochs_json),
                ..Default::default()
            }
            .update(&tx)
            .await?;
        } else if !epochs.is_empty() {
            account_assistant::ActiveModel {
                user_id: Set(deletion.user_id),
                context_epochs_json: Set(epochs_json),
                ..Default::default()
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn companion_epoch(&self, scope: &str) -> Result<i64> {
        if !self.is_ai_module_installed().await? {
            return Ok(0);
        }
        let Some(user_id) = nuono_app::agent::account_chat_owner(scope) else {
            return Ok(0);
        };
        super::assistant::companion_epoch_on(self.orm(), user_id, scope).await
    }

    pub async fn running_assistant_turn_ids(&self, user_id: i64) -> Result<Vec<String>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        use crate::entity::assistant_turns as turns;
        turns::Entity::find()
            .select_only()
            .column(turns::Column::Id)
            .filter(turns::Column::UserId.eq(user_id))
            .filter(turns::Column::Status.eq("running"))
            .into_tuple()
            .all(self.orm())
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/db/companion_privacy.rs"]
mod companion_privacy;
