//! 基于数据库向量缓存的长期记忆语义召回。
//! 对话过程中仅实时计算查询文本的向量；记忆条目的向量缓存由空闲维护任务按批（每次最多 32 条）异步预热。
use super::{
    memory::{MemoryPolicy, MemoryScope},
    runtime::AgentRuntime,
    store::{MemoryEmbeddingRecord, MemoryEntry},
};
use nuono_ai::embedding::{EmbeddingModel, validate_vectors};
use nuono_error::{NuonoError, Result};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, time::Duration};
use tokio::sync::Mutex;

const CALL_BUDGET: Duration = Duration::from_secs(5);
const CANDIDATES: u64 = 4096;

#[derive(Default)]
pub(super) struct SemanticMemoryState {
    job: Mutex<()>,
    cursor: Mutex<Option<MemoryScope>>,
    // 记录当前向量模型及其输出维度；一旦观测到维度变化则使旧维度的缓存失效。
    dimensions: Mutex<Option<(EmbeddingModel, usize)>>,
}

fn content_fingerprint(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn cache_for_candidates(
    rows: &[MemoryEntry],
    model: &EmbeddingModel,
    cached: Vec<MemoryEmbeddingRecord>,
) -> HashMap<i64, Vec<f32>> {
    let source: HashMap<_, _> = rows.iter().map(|row| (row.id, row)).collect();
    cached
        .into_iter()
        .filter_map(|cache| {
            let row = source.get(&cache.memory_id)?;
            if cache.model != *model
                || cache.content_fingerprint != content_fingerprint(&row.content)
                || validate_vectors(std::slice::from_ref(&cache.vector), 1).is_err()
            {
                return None;
            }
            Some((cache.memory_id, cache.vector))
        })
        .collect()
}

impl AgentRuntime {
    pub(super) async fn recall_memories(
        &self,
        scopes: &[(String, String)],
        query: &str,
        policy: &MemoryPolicy,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        if limit == 0 || scopes.is_empty() || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        // 在发起任何数据库查询或模型请求前，先校验查询文本格式并确认其包含有效检索词。
        let query_shape = nuono_rag::Bm25Index::new(&[""])
            .and_then(|index| index.scores(query))
            .map_err(|_| NuonoError::Validation("记忆查询超过预算或包含无效字符".into()))?;
        if query_shape.iter().all(|score| score.query_terms == 0) {
            return Ok(Vec::new());
        }
        let mut rows = self.db.recall_memory_candidates(scopes, CANDIDATES).await?;
        rows.retain(|row| {
            row.enabled
                && row.expires_at.is_none_or(|at| at > chrono::Utc::now())
                && scopes
                    .iter()
                    .any(|(kind, key)| *kind == row.scope_type && *key == row.scope_key)
        });
        let semantic = if let Some(model_id) = policy.embedding_model_id {
            match tokio::time::timeout(
                CALL_BUDGET,
                self.cached_semantic_scores(&rows, query, model_id),
            )
            .await
            {
                Ok(Ok(scores)) => Some(scores),
                _ => {
                    // 底层错误可能携带敏感凭据或内部地址，日志中仅输出固定的降级提示。
                    tracing::warn!("记忆语义召回不可用，本次使用关键词检索");
                    None
                }
            }
        } else {
            None
        };
        let current = self.config.ai_settings().await;
        if !current.agent_memory_enabled {
            return Ok(Vec::new());
        }
        // 若异步请求期间全局配置的向量模型已切换，则丢弃基于旧模型计算的相似度分数。
        let semantic = semantic.filter(|_| {
            current.agent_memory_policy.embedding_model_id == policy.embedding_model_id
        });
        let ranked = super::memory::rank_recall_candidates(
            self.db.as_ref(),
            scopes,
            query,
            &current.agent_memory_policy,
            limit,
            rows,
            semantic.as_ref(),
        )
        .await?;
        // 计算向量与查询图谱期间，部分记忆可能已被并发修改、禁用、过期或变更了作用域；
        // 在注入提示词前，对最终入选的少量记忆逐条回查数据库核实最新状态。
        let mut current_rows = Vec::new();
        for row in ranked {
            if let Some(current) = self.db.get_agent_memory(row.id).await?
                && current.enabled
                && current.expires_at.is_none_or(|at| at > chrono::Utc::now())
                && current.content == row.content
                && current.memory_key == row.memory_key
                && current.scope_type == row.scope_type
                && current.scope_key == row.scope_key
            {
                current_rows.push(current);
            }
        }
        let latest = self.config.ai_settings().await;
        if !latest.agent_memory_enabled {
            return Ok(Vec::new());
        }
        // 已在二次核查中被剔除或被并发修改的记忆不计入有效召回；
        // 强化计数更新放在排序函数外部，确保仅在最终权威核验通过后执行一次。
        if latest.agent_memory_policy.forgetting_enabled && !current_rows.is_empty() {
            self.db
                .reinforce_memories(
                    &current_rows
                        .iter()
                        .map(|memory| memory.id)
                        .collect::<Vec<_>>(),
                    chrono::Utc::now(),
                )
                .await?;
        }
        Ok(current_rows)
    }

    async fn cached_semantic_scores(
        &self,
        rows: &[MemoryEntry],
        query: &str,
        model_id: i64,
    ) -> Result<HashMap<i64, f64>> {
        let embeddings = self
            .memory_embeddings
            .as_ref()
            .ok_or_else(|| NuonoError::Validation("记忆 Embedding 服务未装配".into()))?;
        if rows.is_empty() || query.trim().is_empty() {
            return Ok(HashMap::new());
        }
        let selected = embeddings.describe(model_id).await?;
        let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
        let cached = cache_for_candidates(
            rows,
            &selected,
            self.db.memory_embeddings(&ids, &selected).await?,
        );
        if cached.is_empty() {
            return Ok(HashMap::new());
        }
        let batch = embeddings.embed(model_id, vec![query.to_string()]).await?;
        validate_vectors(&batch.vectors, 1)?;
        if batch.model != selected || embeddings.describe(model_id).await? != selected {
            return Err(NuonoError::Validation("记忆 Embedding 模型已改变".into()));
        }
        let query_vector = &batch.vectors[0];
        *self.semantic_memory.dimensions.lock().await =
            Some((selected.clone(), query_vector.len()));
        let mut ids = Vec::new();
        let mut vectors = Vec::new();
        for (id, vector) in cached {
            if vector.len() == query_vector.len() {
                ids.push(id);
                vectors.push(vector);
            }
        }
        if vectors.is_empty() {
            return Ok(HashMap::new());
        }
        let scores = nuono_rag::cosine_scores(&vectors, query_vector)
            .map_err(|_| NuonoError::Validation("记忆向量缓存无效".into()))?;
        Ok(ids.into_iter().zip(scores).collect())
    }

    /// 在系统空闲时预热一批记忆的向量缓存（单次最多处理 32 条），返回本次新写入缓存的条目数；原始记忆内容绝不会被修改。
    pub async fn maintain_semantic_memories(&self, require_idle: bool) -> Result<usize> {
        let Some(_job) = self.semantic_memory.job.try_lock().ok() else {
            return Ok(0);
        };
        let cfg = self.config.ai_settings().await;
        let policy = &cfg.agent_memory_policy;
        let Some(model_id) = policy.embedding_model_id else {
            return Ok(0);
        };
        let Some(embeddings) = &self.memory_embeddings else {
            return Ok(0);
        };
        if !cfg.enabled
            || !cfg.agent_memory_enabled
            || !self.runs.is_empty()
            || !self
                .memory_activity
                .idle_for(if require_idle { policy.idle_secs } else { 0 })
        {
            return Ok(0);
        }
        policy.validate().map_err(NuonoError::Validation)?;
        let generation = self.memory_activity.generation();
        let changed = self.memory_activity.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let selected = tokio::time::timeout(CALL_BUDGET, embeddings.describe(model_id))
            .await
            .map_err(|_| NuonoError::Validation("记忆 Embedding 请求超时".into()))??;
        let mut scopes = self.db.memory_scopes().await?;
        scopes.sort();
        let previous = self.semantic_memory.cursor.lock().await.clone();
        // 按游标轮询各个作用域，避免单个大体量用户的作用域长期独占预热配额。
        let scope = scopes
            .iter()
            .find(|scope| previous.as_ref().is_none_or(|p| *scope > p))
            .or(scopes.first())
            .cloned();
        let Some(scope) = scope else {
            return Ok(0);
        };
        *self.semantic_memory.cursor.lock().await = Some(scope.clone());
        let mut rows = self
            .db
            .recall_memory_candidates(
                &[(scope.scope_type.clone(), scope.scope_key.clone())],
                CANDIDATES,
            )
            .await?;
        rows.retain(|row| {
            row.scope_type == scope.scope_type
                && row.scope_key == scope.scope_key
                && row.enabled
                && row.expires_at.is_none_or(|at| at > chrono::Utc::now())
        });
        let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
        let mut cached = cache_for_candidates(
            &rows,
            &selected,
            self.db.memory_embeddings(&ids, &selected).await?,
        );
        if let Some((model, dimensions)) = self.semantic_memory.dimensions.lock().await.as_ref()
            && *model == selected
        {
            cached.retain(|_, vector| vector.len() == *dimensions);
        }
        let mut input = Vec::new();
        let mut source = Vec::new();
        let mut bytes = 0;
        for row in rows {
            if cached.contains_key(&row.id)
                || row.content.trim().is_empty()
                || row.content.len() > 8192
                || row.content.contains('\0')
            {
                continue;
            }
            if input.len() == 32 || bytes + row.content.len() > 128 * 1024 {
                break;
            }
            bytes += row.content.len();
            input.push(row.content.clone());
            source.push(row);
        }
        if input.is_empty() {
            return Ok(0);
        }
        let current = self.config.ai_settings().await;
        if self.memory_activity.generation() != generation
            || !current.enabled
            || !current.agent_memory_enabled
            || current.agent_memory_policy.embedding_model_id != Some(model_id)
        {
            return Ok(0);
        }
        let batch = tokio::select! {
            _ = &mut changed => return Ok(0),
            result = tokio::time::timeout(Duration::from_secs(35), embeddings.embed(model_id, input)) => {
                result.map_err(|_| NuonoError::Validation("记忆 Embedding 请求超时".into()))??
            }
        };
        validate_vectors(&batch.vectors, source.len())?;
        let confirmed = tokio::select! {
            _ = &mut changed => return Ok(0),
            result = tokio::time::timeout(CALL_BUDGET, embeddings.describe(model_id)) => {
                result.map_err(|_| NuonoError::Validation("记忆 Embedding 请求超时".into()))??
            }
        };
        let latest = self.config.ai_settings().await;
        if batch.model != selected
            || confirmed != selected
            || self.memory_activity.generation() != generation
            || !latest.enabled
            || !latest.agent_memory_enabled
            || latest.agent_memory_policy.embedding_model_id != Some(model_id)
        {
            return Ok(0);
        }
        *self.semantic_memory.dimensions.lock().await =
            Some((selected.clone(), batch.vectors[0].len()));
        let records: Vec<_> = source
            .into_iter()
            .zip(batch.vectors)
            .map(|(row, vector)| MemoryEmbeddingRecord {
                memory_id: row.id,
                model: selected.clone(),
                content_fingerprint: content_fingerprint(&row.content),
                vector,
            })
            .collect();
        self.db.save_memory_embeddings(&records).await
    }
}
