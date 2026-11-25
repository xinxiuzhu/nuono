//! Agent 记忆召回统计元数据（置顶、召回次数与最近召回时间）的原子更新，以及经过校验、可整体替换的记忆图谱持久化。
use crate::{
    Database,
    entity::{agent_memories as memories, agent_memory_graphs as graphs},
    error::{NuonoError, Result},
};
use nuono_app::agent::{
    memory::{MemoryGraphRecord, MemoryProfile, MemoryScope, fingerprint},
    store::MemoryEntry,
};
use sea_orm::{
    ColumnTrait, EntityTrait, ExprTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect,
    Set,
    sea_query::{Expr, OnConflict},
};

#[derive(FromQueryResult)]
struct StoredMemoryProfile {
    id: i64,
    pinned: bool,
    recall_count: i64,
    last_recalled_at: Option<crate::clock::UtcMillis>,
}

impl From<StoredMemoryProfile> for MemoryProfile {
    fn from(row: StoredMemoryProfile) -> Self {
        Self {
            memory_id: row.id,
            pinned: row.pinned,
            recall_count: row.recall_count,
            last_recalled_at: row.last_recalled_at.map(Into::into),
        }
    }
}

fn stored_profiles() -> sea_orm::Select<memories::Entity> {
    memories::Entity::find()
        .select_only()
        .columns([
            memories::Column::Id,
            memories::Column::Pinned,
            memories::Column::RecallCount,
            memories::Column::LastRecalledAt,
        ])
        .filter(memories::Column::Pinned.is_not_null())
}

impl Database {
    pub async fn recall_memory_candidates(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<MemoryEntry>> {
        if scopes.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let mut condition = sea_orm::Condition::any();
        for (kind, key) in scopes {
            condition = condition.add(
                sea_orm::Condition::all()
                    .add(memories::Column::ScopeType.eq(kind))
                    .add(memories::Column::ScopeKey.eq(key)),
            );
        }
        let now = crate::clock::db_now();
        // 置顶（pinned）或带 explicit/pinned 标签的记忆优先于普通近期记忆进入有界候选池。
        let priority = Expr::cust(
            r#"CASE WHEN pinned = TRUE OR (pinned IS NULL AND (tags_json LIKE '%"explicit"%' OR tags_json LIKE '%"pinned"%')) THEN 1 ELSE 0 END"#,
        );
        let rows = memories::Entity::find()
            .filter(condition)
            .filter(memories::Column::Enabled.eq(true))
            .filter(
                sea_orm::Condition::any()
                    .add(memories::Column::ExpiresAt.is_null())
                    .add(memories::Column::ExpiresAt.gt((now).timestamp_millis())),
            )
            .order_by_desc(priority)
            .order_by_desc(memories::Column::UpdatedAt)
            .order_by_desc(memories::Column::Id)
            .limit(std::cmp::min(limit, 4096))
            .all(self.orm())
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
    pub async fn memory_profiles(&self, ids: &[i64]) -> Result<Vec<MemoryProfile>> {
        if ids.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        Ok(stored_profiles()
            .filter(memories::Column::Id.is_in(ids.iter().copied()))
            .into_model::<StoredMemoryProfile>()
            .all(self.orm())
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }
    pub async fn pin_memory(&self, id: i64, pinned: bool) -> Result<Option<MemoryProfile>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let rows = locked_memories(&tx, &[id]).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        memories::Entity::update_many()
            .col_expr(memories::Column::Pinned, Expr::value(pinned))
            .filter(memories::Column::Id.eq(id))
            .exec(&tx)
            .await?;
        let row = stored_profiles()
            .filter(memories::Column::Id.eq(id))
            .into_model::<StoredMemoryProfile>()
            .one(&tx)
            .await?;
        tx.commit().await?;
        Ok(row.map(Into::into))
    }
    pub async fn reinforce_memories(
        &self,
        ids: &[i64],
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        if ids.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(());
        }
        let tx = self.begin_write().await?;
        let rows = locked_memories(&tx, ids).await?;
        let ids: Vec<_> = rows.iter().map(|memory| memory.id).collect();
        let mut inherited_pins = Vec::new();
        let mut inherited_unpins = Vec::new();
        for memory in rows {
            if memory.recall_count == i64::MAX {
                return Err(NuonoError::Validation("记忆召回次数已达到上限".into()));
            }
            if memory.pinned.is_none() {
                let profile = MemoryProfile::for_memory(&memory.into());
                if profile.pinned {
                    inherited_pins.push(profile.memory_id);
                } else {
                    inherited_unpins.push(profile.memory_id);
                }
            }
        }
        // 首次被召回时将从标签继承的默认置顶状态固化到 pinned 列，确保后续用户显式取消置顶时能持续覆盖标签默认值；
        // 整批更新共用同一批账号锁与记忆行锁。
        for (pinned, inherited_ids) in [(true, inherited_pins), (false, inherited_unpins)] {
            if !inherited_ids.is_empty() {
                memories::Entity::update_many()
                    .col_expr(memories::Column::Pinned, Expr::value(pinned))
                    .filter(memories::Column::Id.is_in(inherited_ids))
                    .exec(&tx)
                    .await?;
            }
        }
        memories::Entity::update_many()
            .col_expr(
                memories::Column::RecallCount,
                Expr::col(memories::Column::RecallCount).add(1),
            )
            .col_expr(
                memories::Column::LastRecalledAt,
                Expr::value(at.timestamp_millis()),
            )
            .filter(memories::Column::Id.is_in(ids))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn memory_scopes(&self) -> Result<Vec<MemoryScope>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let scopes = memories::Entity::find()
            .select_only()
            .columns([memories::Column::ScopeType, memories::Column::ScopeKey])
            .filter(memories::Column::Enabled.eq(true))
            .distinct()
            .into_tuple::<(String, String)>()
            .all(self.orm())
            .await?;
        Ok(scopes
            .into_iter()
            .map(|(scope_type, scope_key)| MemoryScope {
                scope_type,
                scope_key,
            })
            .collect())
    }
    pub async fn memory_graph(&self, scope: &MemoryScope) -> Result<Option<MemoryGraphRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let Some(row) =
            graphs::Entity::find_by_id((scope.scope_type.clone(), scope.scope_key.clone()))
                .one(self.orm())
                .await?
        else {
            return Ok(None);
        };
        Ok(Some(MemoryGraphRecord {
            scope: scope.clone(),
            source_fingerprint: row.source_fingerprint,
            source_limit: row.source_limit as u32,
            graph: serde_json::from_str(&row.graph_json)?,
            updated_at: crate::clock::load_time(row.updated_at),
        }))
    }
    pub async fn save_memory_graph(&self, record: MemoryGraphRecord) -> Result<bool> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        if let Some(user_id) = nuono_app::agent::account_chat_owner(&record.scope.scope_key) {
            let mut owner = crate::entity::users::orm::Entity::find_by_id(user_id);
            if self.backend() == sea_orm::DbBackend::Postgres {
                owner = owner.lock_exclusive();
            }
            if !owner
                .one(&tx)
                .await?
                .is_some_and(|user| user.status == "active")
            {
                return Ok(false);
            }
        }
        let now = crate::clock::db_now();
        let source: Vec<MemoryEntry> = memories::Entity::find()
            .filter(memories::Column::ScopeType.eq(&record.scope.scope_type))
            .filter(memories::Column::ScopeKey.eq(&record.scope.scope_key))
            .filter(memories::Column::Enabled.eq(true))
            .filter(
                sea_orm::Condition::any()
                    .add(memories::Column::ExpiresAt.is_null())
                    .add(memories::Column::ExpiresAt.gt((now).timestamp_millis())),
            )
            .order_by_desc(memories::Column::UpdatedAt)
            .order_by_desc(memories::Column::Id)
            .limit(record.source_limit as u64)
            .all(&tx)
            .await?
            .into_iter()
            .map(Into::into)
            .collect();
        if fingerprint(&source) != record.source_fingerprint {
            tx.rollback().await?;
            return Ok(false);
        }
        record.graph.validate(&source)?;
        graphs::Entity::insert(graphs::ActiveModel {
            scope_type: Set(record.scope.scope_type),
            scope_key: Set(record.scope.scope_key),
            source_fingerprint: Set(record.source_fingerprint),
            source_limit: Set(record.source_limit as i32),
            graph_json: Set(serde_json::to_string(&record.graph)?),
            updated_at: Set(crate::clock::store_time(record.updated_at)),
        })
        .on_conflict(
            OnConflict::columns([graphs::Column::ScopeType, graphs::Column::ScopeKey])
                .update_columns([
                    graphs::Column::SourceFingerprint,
                    graphs::Column::SourceLimit,
                    graphs::Column::GraphJson,
                    graphs::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(&tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}

// 始终先按账号 ID 升序锁定所属账号行，再按记忆 ID 升序锁定记忆行（即使单次批量操作跨多个账号也严格遵守该顺序）。
async fn locked_memories(
    tx: &sea_orm::DatabaseTransaction,
    ids: &[i64],
) -> Result<Vec<memories::Model>> {
    use sea_orm::ConnectionTrait;
    let observed = memories::Entity::find()
        .filter(memories::Column::Id.is_in(ids.iter().copied()))
        .order_by_asc(memories::Column::Id)
        .all(tx)
        .await?;
    let mut accounts = observed
        .iter()
        .filter_map(|memory| nuono_app::agent::account_chat_owner(&memory.scope_key))
        .collect::<Vec<_>>();
    accounts.sort_unstable();
    accounts.dedup();
    for account in accounts {
        crate::db::account_state::locked_user(tx, account).await?;
    }
    let mut query = memories::Entity::find()
        .filter(memories::Column::Id.is_in(ids.iter().copied()))
        .order_by_asc(memories::Column::Id);
    if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    let rows = query.all(tx).await?;
    for row in &rows {
        if !observed.iter().any(|old| {
            old.id == row.id && old.scope_type == row.scope_type && old.scope_key == row.scope_key
        }) {
            return Err(crate::error::NuonoError::Conflict(
                "记忆归属已经改变，请重新读取".into(),
            ));
        }
    }
    Ok(rows)
}

#[cfg(test)]
#[path = "../tests/unit/memory_store.rs"]
mod tests;
