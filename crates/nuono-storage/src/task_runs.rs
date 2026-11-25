//! 定时调度与手动触发任务的持久化执行回执（防重放与状态跟踪）。
use crate::{
    Database,
    entity::scheduled_task_runs as runs,
    error::{NuonoError, Result},
    tasks::ScheduledTask,
};
pub use runs::Model as TaskRun;
use sea_orm::{
    ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set, TryInsertResult,
};
use sha2::{Digest, Sha256};

pub async fn reserve(
    db: &Database,
    id: &str,
    task: &ScheduledTask,
    trigger_kind: &str,
) -> Result<(TaskRun, bool)> {
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err(NuonoError::Validation("执行请求编号必须为 UUID".into()));
    }
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&task.definition())?));
    let tx = db.begin_write().await?;
    let inserted = matches!(runs::Entity::insert(runs::ActiveModel {
        id: Set(id.into()), task_id: Set(task.id), task_name: Set(task.name.clone()),
        definition_digest: Set(digest.clone()), trigger_kind: Set(trigger_kind.into()),
        status: Set("queued".into()), detail: Set(None), created_at: Set(crate::clock::store_time(crate::clock::db_now())), finished_at: Set(None),
    }).on_conflict_do_nothing().exec_without_returning(&tx).await?, TryInsertResult::Inserted(count) if count > 0);
    let row = runs::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .ok_or_else(|| NuonoError::DatabaseQuery("无法读取任务执行回执".into()))?;
    if row.task_id != task.id || row.definition_digest != digest || row.trigger_kind != trigger_kind
    {
        return Err(NuonoError::Conflict(
            "执行请求编号已用于其他任务或配置，请先核对原执行结果".into(),
        ));
    }
    runs::Entity::delete_many()
        .filter(
            runs::Column::CreatedAt
                .lt((crate::clock::db_now() - chrono::Duration::days(30)).timestamp_millis()),
        )
        .filter(runs::Column::Status.is_not_in(["queued", "running"]))
        .filter(runs::Column::TriggerKind.ne("manual"))
        .exec(&tx)
        .await?;
    // 手动触发请求的 ID 在超过 30 天历史保留期后仍保留精简墓碑行（仅清空详情与任务名），
    // 防止超过 30 天的旧手动触发 POST 请求因重放再次触发消息发送。
    runs::Entity::update_many()
        .set(runs::ActiveModel {
            detail: Set(None),
            task_name: Set(String::new()),
            ..Default::default()
        })
        .filter(
            runs::Column::CreatedAt
                .lt((crate::clock::db_now() - chrono::Duration::days(30)).timestamp_millis()),
        )
        .filter(runs::Column::TriggerKind.eq("manual"))
        .filter(runs::Column::Status.is_not_in(["queued", "running"]))
        .filter(runs::Column::TaskName.ne(""))
        .exec(&tx)
        .await?;
    tx.commit().await?;
    Ok((row, inserted))
}

/// 原子抢占（CAS）处于 `queued` 状态的任务执行权：确保同一执行编号最多只有一个消费者开始执行，已进入运行或终态的任务绝不会被重复执行。
pub async fn claim(db: &Database, id: &str) -> Result<bool> {
    Ok(runs::Entity::update_many()
        .set(runs::ActiveModel {
            status: Set("running".into()),
            ..Default::default()
        })
        .filter(runs::Column::Id.eq(id))
        .filter(runs::Column::Status.eq("queued"))
        .exec(db.orm())
        .await?
        .rows_affected
        == 1)
}

pub async fn finish(db: &Database, id: &str, status: &str, detail: &str) -> Result<()> {
    runs::Entity::update_many()
        .set(runs::ActiveModel {
            status: Set(status.into()),
            detail: Set(Some(detail.chars().take(1024).collect())),
            finished_at: Set(crate::clock::store_time(Some(crate::clock::db_now()))),
            ..Default::default()
        })
        .filter(runs::Column::Id.eq(id))
        .filter(runs::Column::Status.is_in(["queued", "running"]))
        .exec(db.orm())
        .await?;
    Ok(())
}

pub async fn interrupt_unfinished(db: &Database) -> Result<()> {
    runs::Entity::update_many()
        .set(runs::ActiveModel {
            status: Set("unknown".into()),
            detail: Set(Some(
                "服务已停止，投递结果未确认；核对平台记录后再决定是否补发".into(),
            )),
            finished_at: Set(crate::clock::store_time(Some(crate::clock::db_now()))),
            ..Default::default()
        })
        .filter(runs::Column::Status.is_in(["queued", "running"]))
        .exec(db.orm())
        .await?;
    Ok(())
}

pub async fn list(db: &Database, task_id: i64) -> Result<Vec<TaskRun>> {
    Ok(runs::Entity::find()
        .filter(runs::Column::TaskId.eq(task_id))
        .filter(
            runs::Column::CreatedAt
                .gte((crate::clock::db_now() - chrono::Duration::days(30)).timestamp_millis()),
        )
        .order_by_desc(runs::Column::CreatedAt)
        .order_by_desc(runs::Column::Id)
        .limit(100)
        .all(db.orm())
        .await?)
}

#[cfg(test)]
#[path = "../tests/unit/task_runs/tests.rs"]
mod tests;
