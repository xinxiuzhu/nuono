//! 陪伴角色历史对话的定期压缩与原文回收：在校验通过并生成精简的经历摘要后，分批回收过期对话原文。
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use nuono_app::agent::companion_memory::{
    CompanionExperience, CompanionSummary, parse_companion_summary,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::Expr,
};

use super::Database;
use crate::{
    entity::{
        agent_memories as memories, agent_memory_embeddings as embeddings,
        agent_memory_graphs as graphs, agent_runs, ai_runs, assistant_turns as turns,
        companion_memory_sources as sources, conversations, messages,
    },
    error::{NuonoError, Result},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompanionRetentionScope {
    pub user_id: i64,
    pub card_id: String,
    pub scope_key: String,
}

#[derive(Clone, Debug)]
pub struct CompanionRetentionBatch {
    pub scope: CompanionRetentionScope,
    pub epoch: i64,
    pub turns: Vec<turns::Model>,
}

impl CompanionRetentionBatch {
    pub fn experiences(&self) -> Vec<CompanionExperience> {
        self.turns
            .iter()
            .map(|turn| CompanionExperience {
                turn_id: turn.id.clone(),
                user_text: turn.text.clone(),
                assistant_text: turn.reply.clone().unwrap_or_default(),
            })
            .collect()
    }
}

fn retention_days(days: u16) -> Result<i64> {
    if !(1..=365).contains(&days) {
        return Err(NuonoError::Validation(
            "原始对话保留天数须为 1 至 365 天".into(),
        ));
    }
    Ok(i64::from(days))
}

fn eligible(
    turn: &turns::Model,
    now: crate::clock::UtcMillis,
    base: i64,
    importance: f64,
    recalled: Option<DateTime<Utc>>,
) -> bool {
    let maximum = turn.created_at + Duration::days(base * 4);
    let importance = if importance.is_finite() {
        importance.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let weighted = turn.created_at
        + Duration::seconds((base as f64 * 86400.0 * (1.0 + 3.0 * importance)).ceil() as i64);
    let threshold = recalled
        .map_or(weighted, |at| {
            weighted.max((at + Duration::days(base)).into())
        })
        .min(maximum);
    turn.created_at + Duration::days(base) <= now && threshold <= now
}

async fn eligibility_metadata<C: sea_orm::ConnectionTrait>(
    connection: &C,
    scope: &str,
    turns: &[turns::Model],
) -> Result<HashMap<String, (f64, Option<DateTime<Utc>>)>> {
    if turns.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<_> = turns
        .iter()
        .map(|turn| format!("turn:{}", turn.id))
        .collect();
    let evidence = sources::Entity::find()
        .filter(sources::Column::ScopeKey.eq(scope))
        .filter(sources::Column::SourceId.is_in(ids))
        .all(connection)
        .await?;
    let memory_ids: Vec<_> = evidence.iter().map(|source| source.memory_id).collect();
    let recalls: HashMap<_, _> = memories::Entity::find()
        .select_only()
        .columns([memories::Column::Id, memories::Column::LastRecalledAt])
        .filter(memories::Column::Id.is_in(memory_ids))
        .into_tuple::<(i64, Option<crate::clock::UtcMillis>)>()
        .all(connection)
        .await?
        .into_iter()
        .map(|(id, recalled_at)| (id, recalled_at.map(Into::into)))
        .collect();
    let mut result = HashMap::<String, (f64, Option<DateTime<Utc>>)>::new();
    for source in evidence {
        let Some(turn_id) = source.source_id.strip_prefix("turn:") else {
            continue;
        };
        let entry = result.entry(turn_id.into()).or_insert((0.0, None));
        entry.0 = entry.0.max(source.importance);
        entry.1 = entry
            .1
            .max(recalls.get(&source.memory_id).copied().flatten());
    }
    Ok(result)
}

impl Database {
    /// 按 `scope_key` 字典序轮询下一个待压缩的作用域（游标由外部调用方维护，防止单个账号摘要生成失败卡死其他账号的清理）。
    pub async fn next_companion_retention_scope(
        &self,
        after: Option<&str>,
        raw_days: u16,
    ) -> Result<Option<CompanionRetentionScope>> {
        let cutoff = crate::clock::db_now() - Duration::days(retention_days(raw_days)?);
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let query = || {
            turns::Entity::find()
                .select_only()
                .column(turns::Column::UserId)
                .column(turns::Column::CardId)
                .column(turns::Column::ScopeKey)
                .distinct()
                .filter(turns::Column::Status.eq("succeeded"))
                .filter(turns::Column::Redacted.eq(false))
                .filter(turns::Column::Control.eq(false))
                .filter(turns::Column::Compacted.eq(false))
                .filter(turns::Column::CreatedAt.lte((cutoff).timestamp_millis()))
                .order_by_asc(turns::Column::ScopeKey)
                .limit(1)
        };
        let row: Option<(i64, String, String)> = if let Some(after) = after {
            match query()
                .filter(turns::Column::ScopeKey.gt(after))
                .into_tuple()
                .one(self.orm())
                .await?
            {
                Some(row) => Some(row),
                None => query().into_tuple().one(self.orm()).await?,
            }
        } else {
            query().into_tuple().one(self.orm()).await?
        };
        Ok(
            row.map(|(user_id, card_id, scope_key)| CompanionRetentionScope {
                user_id,
                card_id,
                scope_key,
            }),
        )
    }

    pub async fn companion_retention_batch(
        &self,
        scope: &CompanionRetentionScope,
        raw_days: u16,
    ) -> Result<CompanionRetentionBatch> {
        self.companion_card(scope.user_id, &scope.card_id).await?;
        if scope.scope_key != nuono_app::agent::companion_scope_key(scope.user_id, &scope.card_id) {
            return Err(NuonoError::Validation("角色记忆作用域无效".into()));
        }
        let base = retention_days(raw_days)?;
        let now = crate::clock::db_now();
        let epoch = self.companion_epoch(&scope.scope_key).await?;
        let candidates = turns::Entity::find()
            .filter(turns::Column::UserId.eq(scope.user_id))
            .filter(turns::Column::ScopeKey.eq(&scope.scope_key))
            .filter(turns::Column::ContextEpoch.eq(epoch))
            .filter(turns::Column::Status.eq("succeeded"))
            .filter(turns::Column::Redacted.eq(false))
            .filter(turns::Column::Control.eq(false))
            .filter(turns::Column::Compacted.eq(false))
            .filter(turns::Column::CreatedAt.lte((now - Duration::days(base)).timestamp_millis()))
            .order_by_asc(turns::Column::CreatedAt)
            .order_by_asc(turns::Column::Id)
            .limit(64)
            .all(self.orm())
            .await?;
        let metadata = eligibility_metadata(self.orm(), &scope.scope_key, &candidates).await?;
        let mut bytes = 0usize;
        let selected = candidates
            .into_iter()
            .filter(|turn| {
                let (importance, recalled) = metadata.get(&turn.id).copied().unwrap_or_default();
                let size = turn.text.len() + turn.reply.as_ref().map_or(0, String::len);
                let valid = eligible(turn, now, base, importance, recalled)
                    && !turn.text.trim().is_empty()
                    && turn
                        .reply
                        .as_ref()
                        .is_some_and(|reply| !reply.trim().is_empty() && reply.len() <= 32 * 1024)
                    && turn.text.len() <= 32 * 1024
                    && size <= (96 * 1024usize).saturating_sub(bytes);
                if valid {
                    bytes += size;
                }
                valid
            })
            .take(16)
            .collect();
        Ok(CompanionRetentionBatch {
            scope: scope.clone(),
            epoch,
            turns: selected,
        })
    }

    pub async fn commit_companion_retention(
        &self,
        batch: &CompanionRetentionBatch,
        summary: &CompanionSummary,
        raw_days: u16,
    ) -> Result<usize> {
        let base = retention_days(raw_days)?;
        if batch.turns.is_empty() || batch.turns.len() > 16 {
            return Err(NuonoError::Validation("原始对话压缩批次无效".into()));
        }
        parse_companion_summary(&serde_json::to_string(summary)?, &batch.experiences())?;
        if !self.is_ai_module_installed().await? {
            return Ok(0);
        }
        let tx = self.begin_write().await?;
        self.lock_companion_data_owner(&tx, batch.scope.user_id)
            .await?;
        if batch.scope.scope_key
            != nuono_app::agent::companion_scope_key(batch.scope.user_id, &batch.scope.card_id)
        {
            return Err(NuonoError::Validation("角色记忆作用域无效".into()));
        }
        let current_epoch =
            super::assistant::companion_epoch_on(&tx, batch.scope.user_id, &batch.scope.scope_key)
                .await?;
        if current_epoch != batch.epoch {
            return Ok(0);
        }
        let ids: Vec<_> = batch.turns.iter().map(|turn| turn.id.clone()).collect();
        let current = turns::Entity::find()
            .filter(turns::Column::Id.is_in(ids.clone()))
            .filter(turns::Column::UserId.eq(batch.scope.user_id))
            .all(&tx)
            .await?;
        if current.len() != batch.turns.len()
            || batch.turns.iter().any(|original| {
                !current.contains(original)
                    || original.scope_key != batch.scope.scope_key
                    || original.card_id != batch.scope.card_id
                    || original.context_epoch != batch.epoch
                    || original.redacted
                    || original.control
                    || original.compacted
                    || original.status != "succeeded"
            })
        {
            return Ok(0);
        }
        let metadata = eligibility_metadata(&tx, &batch.scope.scope_key, &current).await?;
        let now = crate::clock::db_now();
        if current.iter().any(|turn| {
            let (importance, recalled) = metadata.get(&turn.id).copied().unwrap_or_default();
            !eligible(turn, now, base, importance, recalled)
        }) {
            return Ok(0);
        }
        let first = &batch.turns[0];
        let episode = memories::ActiveModel {
            scope_type: Set("user".into()),
            scope_key: Set(batch.scope.scope_key.clone()),
            memory_key: Set(format!("episode:{}", first.id)),
            content: Set(summary.summary.clone()),
            tags_json: Set(Some("[\"companion\",\"episode\",\"summary\"]".into())),
            enabled: Set(true),
            created_by: Set(Some("companion.retention".into())),
            updated_by: Set(Some("companion.retention".into())),
            created_at: Set(crate::clock::store_time(now)),
            updated_at: Set(crate::clock::store_time(now)),
            ..Default::default()
        }
        .insert(&tx)
        .await?;
        let source_ids: Vec<_> = ids.iter().map(|id| format!("turn:{id}")).collect();
        let affected: HashSet<_> = sources::Entity::find()
            .filter(sources::Column::ScopeKey.eq(&batch.scope.scope_key))
            .filter(sources::Column::SourceId.is_in(source_ids.clone()))
            .all(&tx)
            .await?
            .into_iter()
            .map(|row| row.memory_id)
            .collect();
        if !affected.is_empty() {
            embeddings::Entity::delete_many()
                .filter(embeddings::Column::MemoryId.is_in(affected))
                .exec(&tx)
                .await?;
        }
        sources::Entity::update_many()
            .filter(sources::Column::ScopeKey.eq(&batch.scope.scope_key))
            .filter(sources::Column::SourceId.is_in(source_ids))
            .col_expr(sources::Column::Original, Expr::value("reclaimed"))
            .col_expr(sources::Column::Detail, Expr::value("summary"))
            .exec(&tx)
            .await?;
        for turn in &batch.turns {
            let importance = metadata.get(&turn.id).map_or(0.0, |value| value.0);
            sources::ActiveModel {
                memory_id: Set(episode.id),
                source_id: Set(format!("turn:{}", turn.id)),
                scope_key: Set(batch.scope.scope_key.clone()),
                detail: Set("summary".into()),
                original: Set("reclaimed".into()),
                importance: Set(importance),
                observed_at: Set(crate::clock::store_time(turn.created_at)),
            }
            .insert(&tx)
            .await?;
            turns::ActiveModel {
                id: Set(turn.id.clone()),
                text: Set(if turn.id == first.id {
                    "旧对话摘要".into()
                } else {
                    String::new()
                }),
                reply: Set((turn.id == first.id).then(|| summary.summary.clone())),
                prompt_snapshot: Set(String::new()),
                compacted: Set(true),
                redacted: Set(turn.id != first.id),
                delivery: Set("none".into()),
                delivery_key: Set(None),
                ..Default::default()
            }
            .update(&tx)
            .await?;
        }
        let settled_runs = ai_runs::Entity::find()
            .filter(ai_runs::Column::Id.is_in(ids.clone()))
            .all(&tx)
            .await?;
        let mut msg_ids = Vec::new();
        let mut conv_ids = HashSet::new();
        for run in &settled_runs {
            if let Some(mid) = run.trigger_message_id {
                msg_ids.push(mid);
            }
            if let Some(mid) = run.reply_message_id {
                msg_ids.push(mid);
            }
            conv_ids.insert(run.conversation_id.clone());
        }
        if !msg_ids.is_empty() {
            messages::Entity::delete_many()
                .filter(messages::Column::Id.is_in(msg_ids))
                .exec(&tx)
                .await?;
        }
        if !settled_runs.is_empty() {
            ai_runs::Entity::delete_many()
                .filter(ai_runs::Column::Id.is_in(ids))
                .exec(&tx)
                .await?;
        }
        if !conv_ids.is_empty() {
            let conv_id_list: Vec<String> = conv_ids.into_iter().collect();
            conversations::Entity::update_many()
                .filter(conversations::Column::Id.is_in(conv_id_list))
                .col_expr(
                    conversations::Column::SummaryText,
                    Expr::value(Some(summary.summary.clone())),
                )
                .col_expr(
                    conversations::Column::UpdatedAtMs,
                    Expr::value(now.timestamp_millis()),
                )
                .exec(&tx)
                .await?;
        }
        // 后续已完成的运行检查点 JSON 中仍可能拷贝了较早的对话历史；
        // 因此将该空闲上下文的所有非运行中检查点一并回收，仅以持久化的成功回复回执作为唯一可信记录。
        agent_runs::Entity::delete_many()
            .filter(
                agent_runs::Column::SessionId.eq(nuono_app::agent::companion_history_key(
                    &batch.scope.scope_key,
                    batch.epoch,
                )),
            )
            .filter(agent_runs::Column::Status.ne("running"))
            .exec(&tx)
            .await?;
        graphs::Entity::delete_many()
            .filter(graphs::Column::ScopeType.eq("user"))
            .filter(graphs::Column::ScopeKey.eq(&batch.scope.scope_key))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(batch.turns.len())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/db/companion_retention.rs"]
mod tests;
