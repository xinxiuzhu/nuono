//! 单实例宿主启动时的 Agent 运行状态收敛与带版本号校验（revision fence）的运行时中断清理。
use super::*;
use crate::entity::audit_events;
use crate::error::NuonoError;
use nuono_app::agent::store::AgentRunRecoverySummary;
use sea_orm::{DatabaseTransaction, DbBackend, PaginatorTrait};

const ACTIVE: [&str; 4] = ["queued", "running", "waiting_approval", "waiting_children"];
const MAX_REASON_BYTES: usize = 4096;
const MAX_CHECKPOINT_BYTES: usize = 1024 * 1024;

#[derive(sea_orm::FromQueryResult)]
struct RunIdentity {
    id: String,
    revision: i64,
}

impl Database {
    /// 修改任务图中的任何行之前，先比对目标运行记录的期望版本号（`expected_revision`）。
    /// 若 CAS 版本比对失败，则绝不会修改父任务、子任务、检查点或写入审计记录。
    pub async fn interrupt_agent_run(
        &self,
        id: &str,
        expected_revision: i64,
        reason: &str,
    ) -> Result<bool> {
        validate_reason(reason)?;
        if !self.is_ai_module_installed().await? {
            return Ok(false);
        }
        let tx = self.begin_write().await?;
        let changed = interrupt_owned(
            &tx,
            self.backend(),
            id,
            expected_revision,
            reason,
            "runtime_cleanup",
            &mut AgentRunRecoverySummary::default(),
        )
        .await?;
        tx.commit().await?;
        Ok(changed)
    }

    /// 宿主启动并在接收新请求前调用一次，用于收敛上次退出的遗留运行状态。
    /// 仅收敛运行状态，不会触发工具执行、不会篡改审批结果，也不涉及多实例分布式租约或故障探测。
    pub async fn reconcile_agent_runs_before_start(
        &self,
        reason: &str,
    ) -> Result<AgentRunRecoverySummary> {
        validate_reason(reason)?;
        if !self.is_ai_module_installed().await? {
            return Ok(AgentRunRecoverySummary::default());
        }
        let tx = self.begin_write().await?;
        let backend = self.backend();
        let mut summary = AgentRunRecoverySummary::default();

        // 先处理根任务，再处理剩余子任务。候选列表仅查询 ID 与版本号；
        // 体积较大的对话检查点 JSON 在加锁后逐条按需读取。
        for roots_only in [true, false] {
            let mut query = agent_runs::Entity::find()
                .select_only()
                .columns([agent_runs::Column::Id, agent_runs::Column::Revision])
                .filter(agent_runs::Column::Status.eq("running"))
                .order_by_asc(agent_runs::Column::Id);
            if roots_only {
                query = query.filter(agent_runs::Column::ParentId.is_null());
            }
            for candidate in query.into_model::<RunIdentity>().all(&tx).await? {
                interrupt_owned(
                    &tx,
                    backend,
                    &candidate.id,
                    candidate.revision,
                    reason,
                    "startup_recovery",
                    &mut summary,
                )
                .await?;
            }
        }

        // 上次清理可能在父任务汇总前留下了已中断（interrupted）的子任务；
        // 这类卡在 waiting_children 的任务图必须一并中断，避免永久占用用户会话。
        // 正常等待人工审批（waiting_approval）的任务图及其排队中的依赖节点则保持不动。
        let waiting = agent_runs::Entity::find()
            .select_only()
            .columns([agent_runs::Column::Id, agent_runs::Column::Revision])
            .filter(agent_runs::Column::ParentId.is_null())
            .filter(agent_runs::Column::Status.eq("waiting_children"))
            .order_by_asc(agent_runs::Column::Id)
            .into_model::<RunIdentity>()
            .all(&tx)
            .await?;
        for candidate in waiting {
            let Some(parent) = locked_run(&tx, backend, &candidate.id).await? else {
                continue;
            };
            if parent.status != "waiting_children" || parent.revision != candidate.revision {
                continue;
            }
            let mut interrupted = agent_runs::Entity::find()
                .filter(agent_runs::Column::ParentId.eq(&parent.id))
                .filter(agent_runs::Column::Status.eq("interrupted"));
            if let Some(ids) = delegation_children(&parent.state_json) {
                interrupted = interrupted.filter(agent_runs::Column::Id.is_in(ids));
            }
            let interrupted = interrupted.count(&tx).await?;
            if interrupted > 0
                && interrupt_row(
                    &tx,
                    parent.clone(),
                    reason,
                    "startup_recovery",
                    &mut summary,
                )
                .await?
            {
                interrupt_children(
                    &tx,
                    backend,
                    &parent.id,
                    reason,
                    "startup_recovery",
                    &mut summary,
                )
                .await?;
            }
        }

        // 已进入终态的父任务不可能再调度其下属仍处于排队或挂起状态的子任务；
        // 保留父任务的终态结果，同时将这些孤儿活跃子节点收敛为 interrupted，防止已停止的任务图下残留看似可运行或可审批的子任务。
        let orphans = agent_runs::Entity::find()
            .select_only()
            .columns([agent_runs::Column::Id, agent_runs::Column::Revision])
            .filter(agent_runs::Column::Status.is_in(ACTIVE))
            .filter(
                agent_runs::Column::ParentId.in_subquery(
                    Query::select()
                        .column(agent_runs::Column::Id)
                        .from(agent_runs::Entity)
                        .and_where(agent_runs::Column::Status.is_not_in(ACTIVE))
                        .to_owned(),
                ),
            )
            .order_by_asc(agent_runs::Column::Id)
            .into_model::<RunIdentity>()
            .all(&tx)
            .await?;
        for candidate in orphans {
            let Some(child) = agent_runs::Entity::find_by_id(candidate.id.clone())
                .one(&tx)
                .await?
            else {
                continue;
            };
            let Some(parent_id) = &child.parent_id else {
                continue;
            };
            let Some(parent) = locked_run(&tx, backend, parent_id).await? else {
                continue;
            };
            if ACTIVE.contains(&parent.status.as_str()) {
                continue;
            }
            let Some(child) = locked_run(&tx, backend, &candidate.id).await? else {
                continue;
            };
            if child.revision == candidate.revision && ACTIVE.contains(&child.status.as_str()) {
                interrupt_row(&tx, child, reason, "startup_recovery", &mut summary).await?;
            }
        }
        summary.unresolved_executing_approvals = agent_tool_approvals::Entity::find()
            .filter(agent_tool_approvals::Column::Status.eq("executing"))
            .count(&tx)
            .await?;
        tx.commit().await?;
        Ok(summary)
    }
}

fn validate_reason(reason: &str) -> Result<()> {
    if reason.trim().is_empty() || reason.len() > MAX_REASON_BYTES {
        return Err(NuonoError::Validation(
            "Agent 中断理由须为 1..=4096 字节".into(),
        ));
    }
    Ok(())
}

async fn locked_run(
    tx: &DatabaseTransaction,
    backend: DbBackend,
    id: &str,
) -> Result<Option<agent_runs::Model>> {
    let mut query = agent_runs::Entity::find_by_id(id);
    if backend == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    Ok(query.one(tx).await?)
}

async fn interrupt_owned(
    tx: &DatabaseTransaction,
    backend: DbBackend,
    id: &str,
    expected_revision: i64,
    reason: &str,
    source: &str,
    summary: &mut AgentRunRecoverySummary,
) -> Result<bool> {
    // 先无锁读取不可变的 parent_id，再统一按「先父节点、后子节点」的顺序加锁，避免父子加锁顺序反转导致死锁。
    let Some(candidate) = agent_runs::Entity::find_by_id(id).one(tx).await? else {
        return Ok(false);
    };
    let root_id = candidate.parent_id.as_deref().unwrap_or(id);
    let Some(parent) = locked_run(tx, backend, root_id).await? else {
        return Ok(false);
    };
    let row = if candidate.parent_id.is_none() {
        parent.clone()
    } else {
        match locked_run(tx, backend, id).await? {
            Some(row) => row,
            None => return Ok(false),
        }
    };
    if row.revision != expected_revision || row.status != "running" {
        return Ok(false);
    }
    let is_root = row.parent_id.is_none();
    if !interrupt_row(tx, row, reason, source, summary).await? {
        return Ok(false);
    }
    if is_root {
        interrupt_children(tx, backend, id, reason, source, summary).await?;
    } else if parent.status == "waiting_children"
        && delegation_children(&parent.state_json)
            .is_none_or(|ids| ids.iter().any(|child| child == id))
        && interrupt_row(tx, parent.clone(), reason, source, summary).await?
    {
        interrupt_children(tx, backend, &parent.id, reason, source, summary).await?;
    }
    Ok(true)
}

/// 根任务在生命周期内可能分多批委派子任务：早已汇总完成的历史批次中若有曾被中断的旧子节点，
/// 不应连坐打断当前正在正常等待审批的新批次。若遇到旧格式或损坏的检查点无法解析当前批次子列表，则保守地按全部子任务处理。
fn delegation_children(state: &str) -> Option<Vec<String>> {
    let state: serde_json::Value = serde_json::from_str(state).ok()?;
    state
        .get("delegation")?
        .get("children")?
        .as_array()?
        .iter()
        .map(|id| id.as_str().map(str::to_owned))
        .collect()
}

async fn interrupt_children(
    tx: &DatabaseTransaction,
    backend: DbBackend,
    parent_id: &str,
    reason: &str,
    source: &str,
    summary: &mut AgentRunRecoverySummary,
) -> Result<()> {
    // Agent 委派图固定为单层父子结构，且子节点数量受应用层上限约束；
    // 此处先只查出子节点 ID 与版本号，避免一次性把所有子节点的检查点大字段同时读入内存。
    let children = agent_runs::Entity::find()
        .select_only()
        .columns([agent_runs::Column::Id, agent_runs::Column::Revision])
        .filter(agent_runs::Column::ParentId.eq(parent_id))
        .filter(agent_runs::Column::Status.is_in(ACTIVE))
        .order_by_asc(agent_runs::Column::Id)
        .into_model::<RunIdentity>()
        .all(tx)
        .await?;
    for child in children {
        if let Some(row) = locked_run(tx, backend, &child.id).await?
            && ACTIVE.contains(&row.status.as_str())
        {
            interrupt_row(tx, row, reason, source, summary).await?;
        }
    }
    Ok(())
}

async fn interrupt_row(
    tx: &DatabaseTransaction,
    row: agent_runs::Model,
    reason: &str,
    source: &str,
    summary: &mut AgentRunRecoverySummary,
) -> Result<bool> {
    let revision = row
        .revision
        .checked_add(1)
        .ok_or_else(|| NuonoError::Validation("Agent 检查点版本已达到上限".into()))?;
    let now = crate::clock::db_now();
    let (state_json, annotated) =
        interrupted_checkpoint(&row.state_json, &row.status, reason, source, now.into());
    let result = agent_runs::Entity::update_many()
        .set(agent_runs::ActiveModel {
            status: Set("interrupted".into()),
            state_json: Set(state_json),
            revision: Set(revision),
            updated_at: Set(crate::clock::store_time(now)),
            ..Default::default()
        })
        .filter(agent_runs::Column::Id.eq(&row.id))
        .filter(agent_runs::Column::Revision.eq(row.revision))
        .filter(agent_runs::Column::Status.eq(&row.status))
        .exec(tx)
        .await?;
    if result.rows_affected != 1 {
        return Ok(false);
    }
    // 即便原检查点 JSON 损坏或已达大小上限而无法内联写入中断原因，原检查点数据也会原样保留（绝不会用伪造的空检查点覆盖），
    // 同时中断原因仍会完整记录在下方的审计事件中；若审计写入失败，整个任务图的状态变更将随事务一并回滚。
    audit_events::ActiveModel {
        category: Set(crate::audit::CATEGORY_EVENT.to_string()),
        occurred_at_ms: Set(now.timestamp_millis()),
        action: Set("agent.run_interrupted".into()),
        bot_id: Set(None),
        actor_label: Set(None),
        result: Set("ok".into()),
        details_json: Set(
            serde_json::json!({"run_id":row.id,"parent_id":row.parent_id,
            "previous_status":row.status,"previous_revision":row.revision,"reason":reason,
            "source":source,"checkpoint_annotated":annotated})
            .to_string(),
        ),
        ..Default::default()
    }
    .insert(tx)
    .await?;
    summary.interrupted_runs += 1;
    if row.parent_id.is_none() {
        summary.interrupted_roots += 1;
    }
    Ok(true)
}

fn interrupted_checkpoint(
    original: &str,
    previous_status: &str,
    reason: &str,
    source: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> (String, bool) {
    if original.len() <= MAX_CHECKPOINT_BYTES
        && let Ok(serde_json::Value::Object(mut object)) = serde_json::from_str(original)
    {
        let prior_reply = object
            .get("reply")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        object.insert(
            "interruption".into(),
            serde_json::json!({
                "reason":reason,"source":source,"previous_status":previous_status,
                "previous_reply":prior_reply,"at":at.to_rfc3339(),
            }),
        );
        object.insert("reply".into(), serde_json::Value::String(reason.into()));
        let json = serde_json::Value::Object(object).to_string();
        if json.len() <= MAX_CHECKPOINT_BYTES {
            return (json, true);
        }
    }
    (original.to_owned(), false)
}

#[cfg(test)]
#[path = "../../../tests/unit/db/agent_runs/recovery/tests.rs"]
mod tests;
