use crate::{
    Database,
    entity::{agent_runs, agent_tool_approvals},
    error::Result,
};
use nuono_app::agent::store::{AgentRunListRecord, AgentRunRecord};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    Set, sea_query::Query,
};

mod recovery;

impl From<agent_runs::Model> for AgentRunRecord {
    fn from(row: agent_runs::Model) -> Self {
        Self {
            id: row.id,
            parent_id: row.parent_id,
            session_id: row.session_id,
            status: row.status,
            state_json: row.state_json,
            approval_id: row.approval_id,
            revision: row.revision,
        }
    }
}

impl Database {
    pub async fn create_agent_run(&self, run: AgentRunRecord) -> Result<()> {
        self.ensure_ai_module().await?;
        let tx = self.begin_write().await?;
        super::account_state::lock_private_account_on(&tx, &run.session_id).await?;
        let conv =
            crate::conversation_store::ensure_session_conversation_on(&tx, &run.session_id).await?;
        agent_runs::ActiveModel {
            id: Set(run.id),
            conversation_id: Set(Some(conv.id)),
            parent_id: Set(run.parent_id),
            session_id: Set(run.session_id),
            status: Set(run.status),
            state_json: Set(run.state_json),
            approval_id: Set(run.approval_id),
            revision: Set(run.revision),
            updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn update_agent_run(
        &self,
        run: AgentRunRecord,
        expected_revision: i64,
    ) -> Result<bool> {
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        let Some(saved) = agent_runs::Entity::find_by_id(&run.id).one(&tx).await? else {
            return Ok(false);
        };
        super::account_state::lock_private_account_on(&tx, &saved.session_id).await?;
        if !matches!(
            saved.status.as_str(),
            "queued" | "running" | "waiting_approval" | "waiting_children"
        ) && matches!(
            run.status.as_str(),
            "queued" | "running" | "waiting_approval" | "waiting_children"
        ) {
            return Err(nuono_error::NuonoError::Conflict(
                "已结束的 Agent 检查点不能重新进入执行状态".into(),
            ));
        }
        let result = agent_runs::Entity::update_many()
            .set(agent_runs::ActiveModel {
                status: Set(run.status),
                state_json: Set(run.state_json),
                approval_id: Set(run.approval_id),
                revision: Set(expected_revision + 1),
                updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
                ..Default::default()
            })
            .filter(agent_runs::Column::Id.eq(run.id))
            .filter(agent_runs::Column::Revision.eq(expected_revision))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected == 1)
    }
    pub async fn get_agent_run(&self, id: &str) -> Result<Option<AgentRunRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        Ok(agent_runs::Entity::find_by_id(id.to_string())
            .one(self.orm())
            .await?
            .map(Into::into))
    }
    pub async fn list_agent_runs(
        &self,
        status: Option<&str>,
        limit: u64,
    ) -> Result<Vec<AgentRunListRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        let mut query = agent_runs::Entity::find().filter(agent_runs::Column::ParentId.is_null());
        if let Some(status) = status {
            query = query.filter(agent_runs::Column::Status.eq(status));
        }
        let rows = query
            .order_by_desc(agent_runs::Column::UpdatedAt)
            .order_by_desc(agent_runs::Column::Id)
            .limit(limit.clamp(1, 100))
            .all(self.orm())
            .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        #[derive(sea_orm::FromQueryResult)]
        struct ChildCount {
            parent_id: String,
            child_count: i64,
        }
        // 仅聚合统计当前页各父任务的子任务数量，避免加载子任务庞大的检查点 JSON。
        let counts = agent_runs::Entity::find()
            .select_only()
            .column(agent_runs::Column::ParentId)
            .column_as(agent_runs::Column::Id.count(), "child_count")
            .filter(agent_runs::Column::ParentId.is_in(rows.iter().map(|row| row.id.clone())))
            .group_by(agent_runs::Column::ParentId)
            .into_model::<ChildCount>()
            .all(self.orm())
            .await?
            .into_iter()
            .map(|row| (row.parent_id, row.child_count.max(0) as u64))
            .collect::<std::collections::HashMap<_, _>>();
        Ok(rows
            .into_iter()
            .map(|row| AgentRunListRecord {
                child_count: counts.get(&row.id).copied().unwrap_or(0),
                updated_at: crate::clock::format_wire_ts(row.updated_at),
                run: row.into(),
            })
            .collect())
    }
    pub async fn create_agent_children(
        &self,
        parent: AgentRunRecord,
        expected_revision: i64,
        children: Vec<AgentRunRecord>,
    ) -> Result<bool> {
        if children.is_empty()
            || children.len() > 8
            || parent.parent_id.is_some()
            || children.iter().any(|child| {
                child.parent_id.as_deref() != Some(parent.id.as_str())
                    || child.revision != 0
                    || !matches!(child.status.as_str(), "queued" | "running")
            })
        {
            return Err(nuono_error::NuonoError::Validation(
                "无效的子任务批次".into(),
            ));
        }
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        let Some(saved) = agent_runs::Entity::find_by_id(&parent.id).one(&tx).await? else {
            return Ok(false);
        };
        let mut accounts = children
            .iter()
            .filter_map(|child| nuono_app::agent::account_chat_owner(&child.session_id))
            .chain(nuono_app::agent::account_chat_owner(&saved.session_id))
            .collect::<Vec<_>>();
        accounts.sort_unstable();
        accounts.dedup();
        for account in accounts {
            super::account_state::locked_user(&tx, account).await?;
        }
        let changed = agent_runs::Entity::update_many()
            .set(agent_runs::ActiveModel {
                state_json: Set(parent.state_json),
                revision: Set(expected_revision + 1),
                updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
                ..Default::default()
            })
            .filter(agent_runs::Column::Id.eq(parent.id))
            .filter(agent_runs::Column::ParentId.is_null())
            .filter(agent_runs::Column::Status.eq("running"))
            .filter(agent_runs::Column::Revision.eq(expected_revision))
            .exec(&tx)
            .await?;
        if changed.rows_affected != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        for child in children {
            let child_conversation_id =
                if child.session_id == saved.session_id && saved.conversation_id.is_some() {
                    saved.conversation_id.clone()
                } else {
                    Some(
                        crate::conversation_store::ensure_session_conversation_on(
                            &tx,
                            &child.session_id,
                        )
                        .await?
                        .id,
                    )
                };
            agent_runs::ActiveModel {
                id: Set(child.id),
                conversation_id: Set(child_conversation_id),
                parent_id: Set(child.parent_id),
                session_id: Set(child.session_id),
                status: Set(child.status),
                state_json: Set(child.state_json),
                approval_id: Set(child.approval_id),
                revision: Set(child.revision),
                updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }
    pub async fn claim_agent_child(
        &self,
        child: AgentRunRecord,
        expected_revision: i64,
        parent_revision: i64,
    ) -> Result<bool> {
        let Some(parent_id) = child.parent_id else {
            return Err(nuono_error::NuonoError::Validation(
                "子任务缺少父任务".into(),
            ));
        };
        if child.status != "running" {
            return Err(nuono_error::NuonoError::Validation(
                "领取子任务必须进入运行状态".into(),
            ));
        }
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        let Some(saved) = agent_runs::Entity::find_by_id(&child.id).one(&tx).await? else {
            return Ok(false);
        };
        super::account_state::lock_private_account_on(&tx, &saved.session_id).await?;
        let result = agent_runs::Entity::update_many()
            .set(agent_runs::ActiveModel {
                status: Set("running".to_string()),
                state_json: Set(child.state_json),
                revision: Set(expected_revision + 1),
                updated_at: Set(crate::clock::store_time(crate::clock::db_now())),
                ..Default::default()
            })
            .filter(agent_runs::Column::Id.eq(child.id))
            .filter(agent_runs::Column::ParentId.eq(parent_id))
            .filter(agent_runs::Column::Revision.eq(expected_revision))
            .filter(agent_runs::Column::Status.eq("queued"))
            .filter(
                agent_runs::Column::ParentId.in_subquery(
                    Query::select()
                        .column(agent_runs::Column::Id)
                        .from(agent_runs::Entity)
                        .and_where(agent_runs::Column::ParentId.is_null())
                        .and_where(agent_runs::Column::Status.eq("running"))
                        .and_where(agent_runs::Column::Revision.eq(parent_revision))
                        .to_owned(),
                ),
            )
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected == 1)
    }
    pub async fn list_agent_children(&self, parent_id: &str) -> Result<Vec<AgentRunRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(Vec::new());
        }
        Ok(agent_runs::Entity::find()
            .filter(agent_runs::Column::ParentId.eq(parent_id))
            .order_by_asc(agent_runs::Column::Id)
            .all(self.orm())
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }
    pub async fn active_agent_run(&self, session_id: &str) -> Result<Option<AgentRunRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        Ok(agent_runs::Entity::find()
            .filter(agent_runs::Column::SessionId.eq(session_id))
            .filter(agent_runs::Column::Status.is_in([
                "queued",
                "running",
                "waiting_approval",
                "waiting_children",
            ]))
            .one(self.orm())
            .await?
            .map(Into::into))
    }
    pub async fn agent_run_for_approval(&self, id: i64) -> Result<Option<AgentRunRecord>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        Ok(agent_runs::Entity::find()
            .filter(
                Condition::any()
                    .add(agent_runs::Column::ApprovalId.eq(id))
                    .add(
                        agent_runs::Column::Id.in_subquery(
                            Query::select()
                                .column(agent_tool_approvals::Column::RunId)
                                .from(agent_tool_approvals::Entity)
                                .and_where(agent_tool_approvals::Column::Id.eq(id))
                                .to_owned(),
                        ),
                    ),
            )
            .one(self.orm())
            .await?
            .map(Into::into))
    }
    pub async fn complete_agent_tool_approval(
        &self,
        id: i64,
        status: &str,
        execution_json: String,
    ) -> Result<Option<agent_tool_approvals::Model>> {
        if !self.is_ai_module_installed().await? {
            return Ok(None);
        }
        let tx = self.begin_write().await?;
        let Some(saved) = agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?
        else {
            return Ok(None);
        };
        super::account_state::lock_private_account_on(&tx, &saved.session_id).await?;
        agent_tool_approvals::Entity::update_many()
            .set(agent_tool_approvals::ActiveModel {
                status: Set(status.to_string()),
                execution_json: Set(Some(execution_json)),
                ..Default::default()
            })
            .filter(agent_tool_approvals::Column::Id.eq(id))
            .filter(agent_tool_approvals::Column::Status.eq("executing"))
            .exec(&tx)
            .await?;
        let result = agent_tool_approvals::Entity::find_by_id(id)
            .one(&tx)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
}
