//! Agent 工具回执、审批、策略与记忆持久化（SeaORM 双后端：PostgreSQL / SQLite）。

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set,
    sea_query::{Expr, Query},
};

use crate::error::{NuonoError, Result};

use super::{
    AgentMemoryRecord, AgentMemoryUpdateRecord, AgentToolApprovalRecord, AgentToolReceiptRecord,
    Database,
};

impl Database {
    #[tracing::instrument(skip(self, receipt), err(Debug))]
    pub async fn save_agent_tool_receipt(&self, receipt: AgentToolReceiptRecord) -> Result<()> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        super::account_state::lock_private_account_on(&tx, &receipt.session_id).await?;
        let now = crate::clock::db_now();
        let elapsed_ms = receipt.elapsed_ms.min(i32::MAX as u64) as i32;

        crate::entity::agent_tool_receipts::ActiveModel {
            context_json: Set(receipt.context_json),
            session_id: Set(receipt.session_id),
            bot_id: Set(receipt.bot_id),
            platform: Set(receipt.platform),
            message_type: Set(receipt.message_type),
            user_id: Set(receipt.user_id),
            group_id: Set(receipt.group_id),
            channel_id: Set(receipt.channel_id),
            message_id: Set(receipt.message_id),
            tool_name: Set(receipt.tool_name),
            risk_level: Set(receipt.risk_level),
            status: Set(receipt.status),
            input_json: Set(receipt.input_json),
            output_json: Set(receipt.output_json),
            error: Set(receipt.error),
            elapsed_ms: Set(elapsed_ms),
            created_at: Set(crate::clock::store_time(now)),
            ..Default::default()
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn list_agent_tool_receipts(
        &self,
        limit: u64,
    ) -> Result<Vec<crate::entity::agent_tool_receipts::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let rows = crate::entity::agent_tool_receipts::Entity::find()
            .order_by_desc(crate::entity::agent_tool_receipts::Column::Id)
            .limit(limit)
            .all(self.orm())
            .await?;
        Ok(rows)
    }

    #[tracing::instrument(skip(self, approval), err(Debug))]
    pub async fn create_agent_tool_approval(
        &self,
        approval: AgentToolApprovalRecord,
    ) -> Result<i64> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        super::account_state::lock_private_account_on(&tx, &approval.session_id).await?;
        let now = crate::clock::db_now();

        let row = crate::entity::agent_tool_approvals::ActiveModel {
            run_id: Set(approval.run_id),
            context_json: Set(approval.context_json),
            session_id: Set(approval.session_id),
            bot_id: Set(approval.bot_id),
            platform: Set(approval.platform),
            message_type: Set(approval.message_type),
            user_id: Set(approval.user_id),
            group_id: Set(approval.group_id),
            channel_id: Set(approval.channel_id),
            message_id: Set(approval.message_id),
            tool_name: Set(approval.tool_name),
            risk_level: Set(approval.risk_level),
            status: Set("pending".to_string()),
            input_json: Set(approval.input_json),
            reason: Set(approval.reason),
            created_at: Set(crate::clock::store_time(now)),
            ..Default::default()
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(row.id)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn list_agent_tool_approvals(
        &self,
        status: Option<&str>,
        limit: u64,
    ) -> Result<Vec<crate::entity::agent_tool_approvals::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let mut select = crate::entity::agent_tool_approvals::Entity::find();
        if let Some(status) = status.filter(|s| !s.trim().is_empty()) {
            select = select.filter(crate::entity::agent_tool_approvals::Column::Status.eq(status));
        }
        let rows = select
            .order_by_desc(crate::entity::agent_tool_approvals::Column::Id)
            .limit(limit)
            .all(self.orm())
            .await?;
        Ok(rows)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn get_agent_tool_approval(
        &self,
        id: i64,
    ) -> Result<Option<crate::entity::agent_tool_approvals::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let row = crate::entity::agent_tool_approvals::Entity::find_by_id(id)
            .one(self.orm())
            .await?;
        Ok(row)
    }

    #[tracing::instrument(skip(self, decision_by, decision_reason), err(Debug))]
    pub async fn decide_agent_tool_approval(
        &self,
        id: i64,
        approved: bool,
        decision_by: Option<String>,
        decision_reason: Option<String>,
    ) -> Result<Option<crate::entity::agent_tool_approvals::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let Some(saved) = crate::entity::agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?
        else {
            return Ok(None);
        };
        super::account_state::lock_private_account_on(&tx, &saved.session_id).await?;
        let now = crate::clock::db_now();
        crate::entity::agent_tool_approvals::Entity::update_many()
            .set(crate::entity::agent_tool_approvals::ActiveModel {
                status: Set(if approved { "approved" } else { "rejected" }.into()),
                decision_by: Set(decision_by),
                decision_reason: Set(decision_reason),
                decided_at: Set(crate::clock::store_time(Some(now))),
                ..Default::default()
            })
            .filter(crate::entity::agent_tool_approvals::Column::Id.eq(id))
            .filter(crate::entity::agent_tool_approvals::Column::Status.eq("pending"))
            .exec(&tx)
            .await?;
        let result = crate::entity::agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn mark_agent_tool_approval_status(
        &self,
        id: i64,
        status: &str,
    ) -> Result<Option<crate::entity::agent_tool_approvals::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let Some(row) = crate::entity::agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?
        else {
            return Ok(None);
        };

        super::account_state::lock_private_account_on(&tx, &row.session_id).await?;
        let mut active: crate::entity::agent_tool_approvals::ActiveModel = row.into();
        active.status = Set(status.to_string());
        let updated = active.update(&tx).await?;
        tx.commit().await?;
        Ok(Some(updated))
    }

    /// 将已批准的工具审批请求原子抢占为执行中（`executing`）。
    ///
    /// 仅对成功抢到执行权的调用方返回 `Some`。
    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn claim_agent_tool_approval_for_execution(
        &self,
        id: i64,
    ) -> Result<Option<crate::entity::agent_tool_approvals::Model>> {
        use sea_orm::sea_query::ExprTrait;
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let Some(saved) = crate::entity::agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?
        else {
            return Ok(None);
        };
        super::account_state::lock_private_account_on(&tx, &saved.session_id).await?;
        // 单条条件 UPDATE（CAS）：只有 status 仍为 approved 的行才会被改写，
        // rows_affected == 1 的那个调用方赢得执行权。
        let result = crate::entity::agent_tool_approvals::Entity::update_many()
            .set(crate::entity::agent_tool_approvals::ActiveModel {
                status: Set("executing".to_string()),
                ..Default::default()
            })
            .filter(crate::entity::agent_tool_approvals::Column::Id.eq(id))
            .filter(crate::entity::agent_tool_approvals::Column::Status.eq("approved"))
            // 若关联的 Agent 运行在抢占前已被取消或终止，则拒绝进入执行。
            .filter(Expr::exists(
                Query::select()
                    .expr(Expr::val(1))
                    .from(crate::entity::agent_runs::Entity)
                    .cond_where(Condition::any()
                        .add(crate::entity::agent_runs::Column::ApprovalId.eq(id))
                        .add(crate::entity::agent_runs::Column::Id.in_subquery(Query::select()
                            .column(crate::entity::agent_tool_approvals::Column::RunId)
                            .from(crate::entity::agent_tool_approvals::Entity)
                            .and_where(crate::entity::agent_tool_approvals::Column::Id.eq(id))
                            .to_owned())))
                    .cond_where(Condition::any()
                        .add(crate::entity::agent_runs::Column::Status.is_not_in(["running", "waiting_approval"]))
                        .add(crate::entity::agent_runs::Column::ParentId.in_subquery(Query::select()
                            .column(crate::entity::agent_runs::Column::Id)
                            .from(crate::entity::agent_runs::Entity)
                            .and_where(crate::entity::agent_runs::Column::Status.is_not_in(["running", "waiting_approval", "waiting_children"]))
                            .to_owned())))
                    .to_owned(),
            ).not())
            .exec(&tx)
            .await?;

        if result.rows_affected != 1 {
            return Ok(None);
        }

        let row = crate::entity::agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    #[tracing::instrument(skip(self, record), err(Debug))]
    pub async fn create_agent_memory(
        &self,
        record: AgentMemoryRecord,
    ) -> Result<crate::entity::agent_memories::Model> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        super::account_state::lock_private_account_on(&tx, &record.scope_key).await?;
        let now = crate::clock::db_now();
        let row = crate::entity::agent_memories::ActiveModel {
            scope_type: Set(record.scope_type),
            scope_key: Set(record.scope_key),
            memory_key: Set(record.memory_key),
            content: Set(record.content),
            tags_json: Set(record.tags_json),
            enabled: Set(record.enabled),
            expires_at: Set(crate::clock::store_time(record.expires_at)),
            created_by: Set(record.actor.clone()),
            updated_by: Set(record.actor),
            created_at: Set(crate::clock::store_time(now)),
            updated_at: Set(crate::clock::store_time(now)),
            ..Default::default()
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn get_agent_memory(
        &self,
        id: i64,
    ) -> Result<Option<crate::entity::agent_memories::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let row = crate::entity::agent_memories::Entity::find_by_id(id)
            .one(self.orm())
            .await?;
        Ok(row)
    }

    #[tracing::instrument(skip(self, scope_type, scope_key, memory_key), err(Debug))]
    pub async fn get_agent_memory_by_key(
        &self,
        scope_type: &str,
        scope_key: &str,
        memory_key: &str,
    ) -> Result<Option<crate::entity::agent_memories::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let row = crate::entity::agent_memories::Entity::find()
            .filter(crate::entity::agent_memories::Column::ScopeType.eq(scope_type))
            .filter(crate::entity::agent_memories::Column::ScopeKey.eq(scope_key))
            .filter(crate::entity::agent_memories::Column::MemoryKey.eq(memory_key))
            .order_by_desc(crate::entity::agent_memories::Column::UpdatedAt)
            .order_by_desc(crate::entity::agent_memories::Column::Id)
            .limit(1)
            .one(self.orm())
            .await?;
        Ok(row)
    }

    #[tracing::instrument(skip(self, record), err(Debug))]
    pub async fn upsert_agent_memory(
        &self,
        record: AgentMemoryRecord,
    ) -> Result<(crate::entity::agent_memories::Model, bool)> {
        self.ensure_ai_module().await?;
        if let Some(existing) = self
            .get_agent_memory_by_key(&record.scope_type, &record.scope_key, &record.memory_key)
            .await?
        {
            let scopes = [(record.scope_type, record.scope_key)];
            let row = self
                .update_agent_memory_in_scopes(
                    existing.id,
                    &scopes,
                    AgentMemoryUpdateRecord {
                        content: Some(record.content),
                        tags_json: record.tags_json,
                        enabled: Some(record.enabled),
                        expires_at: Some(record.expires_at),
                        actor: record.actor,
                        ..Default::default()
                    },
                )
                .await?
                .ok_or_else(|| {
                    NuonoError::DatabaseQuery(format!(
                        "memory {} disappeared during upsert",
                        existing.id
                    ))
                })?;
            Ok((row, false))
        } else {
            self.create_agent_memory(record)
                .await
                .map(|row| (row, true))
        }
    }

    #[tracing::instrument(skip(self, scope_type, scope_key), err(Debug))]
    pub async fn list_agent_memories(
        &self,
        scope_type: Option<&str>,
        scope_key: Option<&str>,
        enabled: Option<bool>,
        active_only: bool,
        limit: u64,
    ) -> Result<Vec<crate::entity::agent_memories::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let now = crate::clock::db_now();
        let mut select = crate::entity::agent_memories::Entity::find();
        if let Some(scope_type) = scope_type {
            select = select.filter(crate::entity::agent_memories::Column::ScopeType.eq(scope_type));
        }
        if let Some(scope_key) = scope_key {
            select = select.filter(crate::entity::agent_memories::Column::ScopeKey.eq(scope_key));
        }
        if let Some(enabled) = enabled {
            select = select.filter(crate::entity::agent_memories::Column::Enabled.eq(enabled));
        }
        if active_only {
            select = select.filter(
                Condition::any()
                    .add(crate::entity::agent_memories::Column::ExpiresAt.is_null())
                    .add(
                        crate::entity::agent_memories::Column::ExpiresAt
                            .gt((now).timestamp_millis()),
                    ),
            );
        }
        let rows = select
            .order_by_desc(crate::entity::agent_memories::Column::UpdatedAt)
            .order_by_desc(crate::entity::agent_memories::Column::Id)
            .limit(limit)
            .all(self.orm())
            .await?;
        Ok(rows)
    }

    #[tracing::instrument(skip(self, scopes), err(Debug))]
    pub async fn list_agent_memories_for_scopes(
        &self,
        scopes: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<crate::entity::agent_memories::Model>> {
        if scopes.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }

        let now = crate::clock::db_now();
        let mut any_scope = Condition::any();
        for (scope_type, scope_key) in scopes {
            any_scope = any_scope.add(
                Condition::all()
                    .add(crate::entity::agent_memories::Column::ScopeType.eq(scope_type))
                    .add(crate::entity::agent_memories::Column::ScopeKey.eq(scope_key)),
            );
        }

        let rows = crate::entity::agent_memories::Entity::find()
            .filter(crate::entity::agent_memories::Column::Enabled.eq(true))
            .filter(
                Condition::any()
                    .add(crate::entity::agent_memories::Column::ExpiresAt.is_null())
                    .add(
                        crate::entity::agent_memories::Column::ExpiresAt
                            .gt((now).timestamp_millis()),
                    ),
            )
            .filter(any_scope)
            .order_by_desc(crate::entity::agent_memories::Column::UpdatedAt)
            .order_by_desc(crate::entity::agent_memories::Column::Id)
            .limit(limit)
            .all(self.orm())
            .await?;
        Ok(rows)
    }

    #[tracing::instrument(skip(self, record), err(Debug))]
    pub async fn update_agent_memory(
        &self,
        id: i64,
        record: AgentMemoryUpdateRecord,
    ) -> Result<Option<crate::entity::agent_memories::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let Some(observed) = crate::entity::agent_memories::Entity::find_by_id(id)
            .one(&tx)
            .await?
        else {
            return Ok(None);
        };

        let scopes = [
            observed.scope_key.as_str(),
            record.scope_key.as_deref().unwrap_or(&observed.scope_key),
        ];
        lock_memory_scope_accounts(&tx, scopes).await?;
        let mut query = crate::entity::agent_memories::Entity::find_by_id(id);
        if self.backend() == sea_orm::DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let Some(existing) = query.one(&tx).await? else {
            return Ok(None);
        };
        if existing.scope_key != observed.scope_key || existing.scope_type != observed.scope_type {
            return Err(NuonoError::Conflict("记忆归属已经改变，请重新读取".into()));
        }
        let now = crate::clock::db_now();
        let scope_type = record
            .scope_type
            .unwrap_or_else(|| existing.scope_type.clone());
        let scope_key = record
            .scope_key
            .unwrap_or_else(|| existing.scope_key.clone());
        let memory_key = record
            .memory_key
            .unwrap_or_else(|| existing.memory_key.clone());
        let content = record.content.unwrap_or_else(|| existing.content.clone());
        let tags_json = record
            .tags_json
            .map(Some)
            .unwrap_or_else(|| existing.tags_json.clone());
        let enabled = record.enabled.unwrap_or(existing.enabled);
        let expires_at = record
            .expires_at
            .unwrap_or_else(|| crate::clock::load_time(existing.expires_at));
        let actor = record.actor;

        let mut active: crate::entity::agent_memories::ActiveModel = existing.into();
        active.scope_type = Set(scope_type);
        active.scope_key = Set(scope_key);
        active.memory_key = Set(memory_key);
        active.content = Set(content);
        active.tags_json = Set(tags_json);
        active.enabled = Set(enabled);
        active.expires_at = Set(crate::clock::store_time(expires_at));
        active.updated_by = Set(actor);
        active.updated_at = Set(crate::clock::store_time(now));
        let row = active.update(&tx).await?;
        tx.commit().await?;
        Ok(Some(row))
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn delete_agent_memory(&self, id: i64) -> Result<bool> {
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        let Some(observed) = crate::entity::agent_memories::Entity::find_by_id(id)
            .one(&tx)
            .await?
        else {
            return Ok(false);
        };
        super::account_state::lock_private_account_on(&tx, &observed.scope_key).await?;
        // 删除条件中携带预读到的作用域：防止加锁前并发完成的账号合并已将该记录迁移到其他作用域。
        let result = crate::entity::agent_memories::Entity::delete_many()
            .filter(crate::entity::agent_memories::Column::Id.eq(id))
            .filter(crate::entity::agent_memories::Column::ScopeType.eq(observed.scope_type))
            .filter(crate::entity::agent_memories::Column::ScopeKey.eq(observed.scope_key))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected > 0)
    }

    /// 在单条带作用域过滤的 UPDATE 语句中应用已通过权限检查的记忆修改，防止并发越权。
    ///
    /// # Errors
    /// 若尝试篡改记忆作用域或数据库更新失败，则返回错误。
    pub async fn update_agent_memory_in_scopes(
        &self,
        id: i64,
        scopes: &[(String, String)],
        record: AgentMemoryUpdateRecord,
    ) -> Result<Option<crate::entity::agent_memories::Model>> {
        use crate::entity::agent_memories::{ActiveModel, Column, Entity};
        if record.scope_type.is_some() || record.scope_key.is_some() {
            return Err(NuonoError::Validation("记忆工具不能修改记录作用域".into()));
        }
        if scopes.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        lock_memory_scope_accounts(&tx, scopes.iter().map(|(_, key)| key.as_str())).await?;
        let mut patch = ActiveModel {
            updated_by: Set(record.actor),
            updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
            ..Default::default()
        };
        if let Some(value) = record.memory_key {
            patch.memory_key = Set(value);
        }
        if let Some(value) = record.content {
            patch.content = Set(value);
        }
        if let Some(value) = record.tags_json {
            patch.tags_json = Set(Some(value));
        }
        if let Some(value) = record.enabled {
            patch.enabled = Set(value);
        }
        if let Some(value) = record.expires_at {
            patch.expires_at = Set(crate::clock::store_time(value));
        }
        let mut rows = Entity::update_many()
            .set(patch)
            .filter(Column::Id.eq(id))
            .filter(memory_scope_condition(scopes))
            .exec_with_returning(&tx)
            .await?;
        tx.commit().await?;
        Ok(rows.pop())
    }

    /// 在 DELETE 语句中直接附带授权作用域过滤条件，安全删除工具指定的记忆记录。
    ///
    /// # Errors
    /// 若数据库删除操作失败则返回错误。
    pub async fn delete_agent_memory_in_scopes(
        &self,
        id: i64,
        scopes: &[(String, String)],
    ) -> Result<bool> {
        use crate::entity::agent_memories::{Column, Entity};
        if scopes.is_empty() || !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        lock_memory_scope_accounts(&tx, scopes.iter().map(|(_, key)| key.as_str())).await?;
        let deleted = Entity::delete_many()
            .filter(Column::Id.eq(id))
            .filter(memory_scope_condition(scopes))
            .exec(&tx)
            .await?
            .rows_affected
            > 0;
        tx.commit().await?;
        Ok(deleted)
    }

    #[tracing::instrument(skip(self), err(Debug))]
    pub async fn delete_expired_agent_memories(&self) -> Result<u64> {
        use crate::entity::agent_memories::{Column, Entity};
        if !self.is_ai_module_installed().await? {
            return Ok(0);
        }
        let tx = self.begin_write().await?;
        let now = crate::clock::db_now().timestamp_millis();
        let keys = Entity::find()
            .select_only()
            .column(Column::ScopeKey)
            .filter(Column::ExpiresAt.lte(now))
            .distinct()
            .into_tuple::<String>()
            .all(&tx)
            .await?;
        let mut accounts = keys
            .iter()
            .filter_map(|key| nuono_app::agent::account_chat_owner(key))
            .collect::<Vec<_>>();
        accounts.sort_unstable();
        accounts.dedup();
        for account in accounts {
            // 定期清理过期记忆的任务可能触及已停用或已删除账号的数据，但仍按同一顺序获取账号生命周期锁。
            let mut query = crate::entity::users::orm::Entity::find_by_id(account);
            if self.backend() == sea_orm::DbBackend::Postgres {
                query = query.lock_exclusive();
            }
            query.one(&tx).await?;
        }
        let result = Entity::delete_many()
            .filter(Column::ExpiresAt.lte(now))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected)
    }
}

async fn lock_memory_scope_accounts<'a>(
    tx: &sea_orm::DatabaseTransaction,
    scopes: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let mut accounts = scopes
        .into_iter()
        .filter_map(nuono_app::agent::account_chat_owner)
        .collect::<Vec<_>>();
    accounts.sort_unstable();
    accounts.dedup();
    for account in accounts {
        super::account_state::locked_user(tx, account).await?;
    }
    Ok(())
}

fn memory_scope_condition(scopes: &[(String, String)]) -> Condition {
    use crate::entity::agent_memories::Column;
    scopes
        .iter()
        .fold(Condition::any(), |condition, (kind, key)| {
            condition.add(
                Condition::all()
                    .add(Column::ScopeType.eq(kind))
                    .add(Column::ScopeKey.eq(key)),
            )
        })
}

#[cfg(test)]
#[path = "../../tests/unit/db/agent/tests.rs"]
mod tests;
