//! 有容量上限、可随时重建替换的记忆向量嵌入缓存；底层记忆表（`nuono_agent_memories`）始终是唯一权威数据源。
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Database,
    entity::{agent_memories as memories, agent_memory_embeddings as embeddings},
    error::{NuonoError, Result},
};
use nuono_ai::embedding::{EmbeddingModel, validate_vectors};
use nuono_app::agent::store::MemoryEmbeddingRecord;
use sea_orm::{
    ColumnTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::OnConflict,
};
use sha2::{Digest, Sha256};

const MAX_RECORDS: usize = 4096;
const READ_BATCH: usize = 128;
const MAX_VECTOR_JSON_BYTES: usize = 64 * 1024;
const MAX_WRITE_JSON_BYTES: usize = 16 * 1024 * 1024;

fn validate_model(model: &EmbeddingModel) -> Result<()> {
    if model.id <= 0
        || model.model.trim().is_empty()
        || model.model.len() > 512
        || model.model.contains('\0')
        || model.revision.trim().is_empty()
        || model.revision.len() > 128
        || model.revision.contains('\0')
    {
        return Err(NuonoError::Validation(
            "Memory embedding model exceeds the bounded cache contract".into(),
        ));
    }
    Ok(())
}

fn content_fingerprint(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

impl Database {
    /// 按调用方已鉴权筛选出的候选记忆 ID 批量读取向量缓存（不开放全表缓存扫描；内容指纹已过期、已停用、已超期或格式异常的缓存行均视为未命中）。
    pub async fn memory_embeddings(
        &self,
        ids: &[i64],
        model: &EmbeddingModel,
    ) -> Result<Vec<MemoryEmbeddingRecord>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        validate_model(model)?;
        if ids.len() > MAX_RECORDS || ids.iter().any(|id| *id <= 0) {
            return Err(NuonoError::Validation(
                "Memory embedding IDs exceed the bounded cache contract".into(),
            ));
        }
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let ids: Vec<_> = ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut result = Vec::new();
        for batch in ids.chunks(READ_BATCH) {
            let now = crate::clock::db_now();
            let rows = embeddings::Entity::find()
                .filter(embeddings::Column::MemoryId.is_in(batch.iter().copied()))
                .filter(embeddings::Column::ModelId.eq(model.id))
                .filter(embeddings::Column::ModelName.eq(&model.model))
                .filter(embeddings::Column::ModelRevision.eq(&model.revision))
                .filter(memories::Column::Enabled.eq(true))
                .filter(
                    sea_orm::Condition::any()
                        .add(memories::Column::ExpiresAt.is_null())
                        .add(memories::Column::ExpiresAt.gt((now).timestamp_millis())),
                )
                .order_by_asc(embeddings::Column::MemoryId)
                .find_also_related(memories::Entity)
                .all(self.orm())
                .await?;
            for (cached, memory) in rows {
                let Some(memory) = memory else { continue };
                if cached.vector_json.len() > MAX_VECTOR_JSON_BYTES
                    || !(1..=4096).contains(&cached.dimensions)
                    || content_fingerprint(&memory.content) != cached.content_fingerprint
                {
                    continue;
                }
                let Ok(vector) = serde_json::from_str::<Vec<f32>>(&cached.vector_json) else {
                    continue;
                };
                if vector.len() != cached.dimensions as usize
                    || validate_vectors(std::slice::from_ref(&vector), 1).is_err()
                {
                    continue;
                }
                result.push(MemoryEmbeddingRecord {
                    memory_id: cached.memory_id,
                    model: model.clone(),
                    content_fingerprint: cached.content_fingerprint,
                    vector,
                });
            }
        }
        Ok(result)
    }

    /// 在写事务内锁定并复核底层权威记忆的内容指纹后再写入向量缓存；
    /// 若在向量模型推理期间原记忆已被并发编辑，则跳过该旧向量的写入（下次读取按缓存未命中处理）。
    pub async fn save_memory_embeddings(&self, records: &[MemoryEmbeddingRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        if records.len() > MAX_RECORDS {
            return Err(NuonoError::Validation(
                "Memory embedding batch exceeds the bounded cache contract".into(),
            ));
        }
        let mut identities = BTreeSet::new();
        let mut dimensions = BTreeMap::new();
        let mut serialized = Vec::with_capacity(records.len());
        let mut json_bytes = 0;
        for record in records {
            validate_model(&record.model)?;
            if record.memory_id <= 0
                || !identities.insert((record.memory_id, record.model.id))
                || record.content_fingerprint.len() != 64
                || !record
                    .content_fingerprint
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(NuonoError::Validation(
                    "Memory embedding identity or content fingerprint is invalid".into(),
                ));
            }
            validate_vectors(std::slice::from_ref(&record.vector), 1)?;
            let key = (record.model.id, &record.model.model, &record.model.revision);
            if dimensions
                .insert(key, record.vector.len())
                .is_some_and(|previous| previous != record.vector.len())
            {
                return Err(NuonoError::Validation(
                    "Memory embeddings for the same model have inconsistent dimensions".into(),
                ));
            }
            let json = serde_json::to_string(&record.vector)?;
            json_bytes += json.len();
            if json.len() > MAX_VECTOR_JSON_BYTES || json_bytes > MAX_WRITE_JSON_BYTES {
                return Err(NuonoError::Validation(
                    "Memory embedding JSON exceeds the bounded cache contract".into(),
                ));
            }
            serialized.push(json);
        }

        if !self.is_ai_module_installed().await? {
            return Ok(0);
        }
        let tx = self.begin_write().await?;
        let mut saved = 0;
        // 按 (memory_id, model_id) 升序加锁，避免多个并发写入批次涵盖重叠记忆时发生死锁。
        let mut ordered: Vec<_> = records.iter().zip(serialized).collect();
        ordered.sort_by_key(|(record, _)| (record.memory_id, record.model.id));
        for (record, vector_json) in ordered {
            let mut query = memories::Entity::find_by_id(record.memory_id);
            if self.backend() == DbBackend::Postgres {
                query = query.lock_exclusive();
            }
            let Some(memory) = query.one(&tx).await? else {
                continue;
            };
            let now = crate::clock::db_now();
            if !memory.enabled
                || memory.expires_at.is_some_and(|expires| expires <= now)
                || content_fingerprint(&memory.content) != record.content_fingerprint
            {
                continue;
            }
            embeddings::Entity::insert(embeddings::ActiveModel {
                memory_id: Set(record.memory_id),
                model_id: Set(record.model.id),
                model_name: Set(record.model.model.clone()),
                model_revision: Set(record.model.revision.clone()),
                content_fingerprint: Set(record.content_fingerprint.clone()),
                dimensions: Set(record.vector.len() as i32),
                vector_json: Set(vector_json),
                updated_at: Set(crate::clock::store_time(now)),
            })
            .on_conflict(
                OnConflict::columns([embeddings::Column::MemoryId, embeddings::Column::ModelId])
                    .update_columns([
                        embeddings::Column::ModelName,
                        embeddings::Column::ModelRevision,
                        embeddings::Column::ContentFingerprint,
                        embeddings::Column::Dimensions,
                        embeddings::Column::VectorJson,
                        embeddings::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&tx)
            .await?;
            saved += 1;
        }
        tx.commit().await?;
        Ok(saved)
    }
}
