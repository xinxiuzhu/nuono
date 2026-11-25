//! 陪伴角色私聊轮次成功结束后的长期记忆原子更新（要求每条记忆必须附带对话原文依据）。
use super::Database;
use crate::{
    entity::{
        agent_memories as memories, agent_memory_embeddings as embeddings,
        agent_memory_graphs as graphs, assistant_turns as turns,
        companion_memory_sources as sources, users::orm as users,
    },
    error::{NuonoError, Result},
};
use nuono_app::agent::companion_memory::MemoryUpsert;
use nuono_app::agent::memory as nuono_memory;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbBackend, EntityTrait, QueryFilter, QuerySelect, Set,
    sea_query::OnConflict,
};

impl Database {
    pub(super) async fn lock_companion_data_owner(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        user_id: i64,
    ) -> Result<()> {
        let mut query = users::Entity::find_by_id(user_id);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        if query.one(tx).await?.is_none() {
            return Err(NuonoError::NotFound("账号不存在".into()));
        }
        Ok(())
    }

    pub async fn save_companion_memories(
        &self,
        user_id: i64,
        turn_id: &str,
        epoch: i64,
        updates: &[MemoryUpsert],
    ) -> Result<bool> {
        if updates.len() > 8 {
            return Err(NuonoError::Validation("记忆整理数量超限".into()));
        }
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        self.lock_companion_data_owner(&tx, user_id).await?;
        let Some(turn) = turns::Entity::find_by_id(turn_id)
            .filter(turns::Column::UserId.eq(user_id))
            .one(&tx)
            .await?
        else {
            return Ok(false);
        };
        if turn.status != "succeeded"
            || turn.redacted
            || turn.compacted
            || turn.context_epoch != epoch
        {
            return Ok(false);
        }
        let current = super::assistant::companion_epoch_on(&tx, user_id, &turn.scope_key).await?;
        if current != epoch {
            return Ok(false);
        }
        write_updates_on(&tx, &turn, updates).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 记忆修正/遗忘等控制指令的回执仅保留操作结果（「记忆调整」），绝不把用户要求遗忘的原话作为新对话经历落盘。
    pub async fn complete_companion_control(
        &self,
        user_id: i64,
        turn_id: &str,
        epoch: i64,
        reply: &str,
        updates: &[MemoryUpsert],
    ) -> Result<()> {
        if !self.is_ai_module_installed().await? {
            return Err(NuonoError::NotFound("对话请求不存在".into()));
        }
        let tx = self.begin_write().await?;
        self.lock_companion_data_owner(&tx, user_id).await?;
        let row = turns::Entity::find_by_id(turn_id)
            .filter(turns::Column::UserId.eq(user_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("对话请求不存在".into()))?;
        let current = super::assistant::companion_epoch_on(&tx, user_id, &row.scope_key).await?;
        if !row.control || row.redacted || row.status != "running" || current != epoch {
            return Err(NuonoError::Conflict("个人数据状态已变化".into()));
        }
        if updates.len() > 8 {
            return Err(NuonoError::Validation("记忆整理数量超限".into()));
        }
        write_updates_on(&tx, &row, updates).await?;
        turns::ActiveModel {
            id: Set(row.id),
            text: Set("记忆调整".into()),
            prompt_snapshot: Set(String::new()),
            reply: Set(Some(reply.into())),
            status: Set("succeeded".into()),
            failure: Set(None),
            context_epoch: Set(epoch),
            ..Default::default()
        }
        .update(&tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn write_updates_on(
    tx: &sea_orm::DatabaseTransaction,
    turn: &turns::Model,
    updates: &[MemoryUpsert],
) -> Result<()> {
    for update in updates {
        if update.key.is_empty()
            || update.key.len() > 128
            || update.content.trim().is_empty()
            || update.content.len() > 2048
            || !update.importance.is_finite()
            || !(0.0..=1.0).contains(&update.importance)
            || update.evidence.is_empty()
            || !turn.text.contains(&update.evidence)
        {
            return Err(NuonoError::Validation("记忆整理缺少有效原文依据".into()));
        }
        if update.key.starts_with("episode:") {
            return Err(NuonoError::Validation("事实不能覆盖经历摘要".into()));
        }
        let key = update.key.clone();
        let existing = memories::Entity::find()
            .filter(memories::Column::ScopeType.eq("user"))
            .filter(memories::Column::ScopeKey.eq(&turn.scope_key))
            .filter(memories::Column::MemoryKey.eq(&key))
            .one(tx)
            .await?;
        if turn.control && existing.is_some() {
            return Err(NuonoError::Conflict(
                "新记忆与未选中的记忆冲突，请明确纠正目标".into(),
            ));
        }
        let now = chrono::Utc::now();
        let record = if let Some(existing) = existing {
            // 记忆内容被修正后，立即删除基于旧表述生成的向量嵌入缓存，避免旧语义被还原或命中。
            embeddings::Entity::delete_many()
                .filter(embeddings::Column::MemoryId.eq(existing.id))
                .exec(tx)
                .await?;
            memories::ActiveModel {
                id: Set(existing.id),
                content: Set(update.content.clone()),
                updated_at: Set(crate::clock::store_time(now)),
                enabled: Set(true),
                updated_by: Set(Some("companion.auto".into())),
                ..Default::default()
            }
            .update(tx)
            .await?
        } else {
            memories::ActiveModel {
                scope_type: Set("user".into()),
                scope_key: Set(turn.scope_key.clone()),
                memory_key: Set(key),
                content: Set(update.content.clone()),
                tags_json: Set(Some("[\"companion\",\"auto\"]".into())),
                enabled: Set(true),
                created_by: Set(Some("companion.auto".into())),
                updated_by: Set(Some("companion.auto".into())),
                created_at: Set(crate::clock::store_time(now)),
                updated_at: Set(crate::clock::store_time(now)),
                ..Default::default()
            }
            .insert(tx)
            .await?
        };
        sources::Entity::insert(sources::ActiveModel {
            memory_id: Set(record.id),
            source_id: Set(format!("turn:{}", turn.id)),
            scope_key: Set(turn.scope_key.clone()),
            detail: Set(if turn.control { "summary" } else { "full" }.into()),
            original: Set(if turn.control {
                "reclaimed"
            } else {
                "available"
            }
            .into()),
            importance: Set(update.importance),
            observed_at: Set(crate::clock::store_time(turn.created_at)),
        })
        .on_conflict(
            OnConflict::columns([sources::Column::MemoryId, sources::Column::SourceId])
                .update_columns([sources::Column::Importance])
                .to_owned(),
        )
        .exec(tx)
        .await?;
    }
    if !updates.is_empty() {
        graphs::Entity::delete_many()
            .filter(graphs::Column::ScopeType.eq("user"))
            .filter(graphs::Column::ScopeKey.eq(&turn.scope_key))
            .exec(tx)
            .await?;
    }
    Ok(())
}

impl Database {
    pub async fn companion_memory_sources(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<nuono_memory::MemorySource>>> {
        use sea_orm::{QueryTrait, Statement, sea_query::Expr};
        if ids.len() > 128 || ids.iter().any(|id| *id <= 0) {
            return Err(NuonoError::Validation("记忆来源读取数量超限".into()));
        }
        let mut map: std::collections::HashMap<i64, Vec<nuono_memory::MemorySource>> =
            std::collections::HashMap::new();
        if ids.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(map);
        }
        // PostgreSQL 与 SQLite 均支持窗口函数：用 ROW_NUMBER() 在单次查询内将每条记忆返回的来源数限制在 32 条以内，
        // 既不删减底层持久化的完整来源记录，也避免了 N+1 次循环查询。
        let query = sources::Entity::find()
            .column_as(Expr::cust("ROW_NUMBER() OVER (PARTITION BY memory_id ORDER BY observed_at DESC, source_id ASC)"), "source_rank")
            .filter(sources::Column::MemoryId.is_in(ids.iter().copied())).build(self.backend());
        let query = Statement {
            sql: format!(
                "SELECT * FROM ({}) AS ranked_sources WHERE source_rank <= 32",
                query.sql
            ),
            ..query
        };
        let rows = sources::Entity::find()
            .from_raw_sql(query)
            .all(self.orm())
            .await?;
        for row in rows {
            map.entry(row.memory_id)
                .or_default()
                .push(nuono_memory::MemorySource {
                    source_id: row.source_id,
                    scope: nuono_memory::MemoryScope {
                        scope_type: "user".into(),
                        scope_key: row.scope_key,
                    },
                    detail: serde_json::from_value(serde_json::Value::String(row.detail))?,
                    original: serde_json::from_value(serde_json::Value::String(row.original))?,
                    media: None,
                });
        }
        Ok(map)
    }

    /// 针对当前作用域内的全量记忆键做冲突预检（而非仅检查模型召回的有限子集），确认记忆替换目标不存在歧义。
    pub async fn companion_replacement_unambiguous(
        &self,
        user_id: i64,
        turn_id: &str,
        removals: &[i64],
        updates: &[MemoryUpsert],
    ) -> Result<bool> {
        if !self.is_ai_module_installed().await? {
            return Err(NuonoError::NotFound("对话请求不存在".into()));
        }
        let row = turns::Entity::find_by_id(turn_id)
            .filter(turns::Column::UserId.eq(user_id))
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::NotFound("对话请求不存在".into()))?;
        let rows = memories::Entity::find()
            .filter(memories::Column::ScopeType.eq("user"))
            .filter(memories::Column::ScopeKey.eq(&row.scope_key))
            .filter(memories::Column::MemoryKey.is_in(updates.iter().map(|u| u.key.clone())))
            .all(self.orm())
            .await?;
        Ok(updates.iter().all(|u| !u.key.starts_with("episode:"))
            && rows.iter().all(|m| removals.contains(&m.id)))
    }
}
