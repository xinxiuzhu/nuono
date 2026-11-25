//! 持久化计划任务查询及其管理投影。
use nuono_app::scheduled_tasks::{
    MAX_ACTION_PARAMS_BYTES, MAX_ACTION_TYPE_BYTES, MAX_BOT_ID_BYTES, MAX_CRON_BYTES,
    MAX_NAME_BYTES, validate_definition_size,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, DbBackend, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};

use crate::db::Database;
use crate::entity::scheduled_tasks;
use crate::error::Result;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ScheduledTask {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub id: i64,
    pub name: String,
    pub cron_expr: String,
    pub timezone: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub revision: i64,
    pub action_type: String,
    pub action_params: Option<String>,
    /// 投递 Bot 的 OneBot self_id；NULL = 老任务，执行时广播并告警
    pub bot_id: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    /// 列表查询按当前时间、cron 和时区计算，不持久化会过期的预估值。
    pub next_run_at: Option<String>,
    pub created_at: Option<String>,
}

impl From<scheduled_tasks::Model> for ScheduledTask {
    fn from(m: scheduled_tasks::Model) -> Self {
        ScheduledTask {
            id: m.id,
            name: m.name,
            cron_expr: m.cron_expr,
            timezone: m.timezone,
            revision: m.revision,
            action_type: m.action_type,
            action_params: m.action_params,
            bot_id: m.bot_id,
            enabled: m.enabled.unwrap_or(true),
            last_run_at: m.last_run_at.map(crate::clock::format_wire_ts),
            next_run_at: None,
            created_at: m.created_at.map(crate::clock::format_wire_ts),
        }
    }
}

pub async fn list(db: &Database) -> Result<Vec<ScheduledTask>> {
    let rows = scheduled_tasks::Entity::find().all(db.orm()).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let mut task: ScheduledTask = row.into();
            task.next_run_at = if task.enabled && task.definition().validate().is_ok() {
                nuono_app::scheduled_tasks::next_occurrences(
                    &task.cron_expr,
                    &task.timezone,
                    chrono::Utc::now(),
                    1,
                )
                .ok()
                .and_then(|dates| dates.first().copied())
                .map(crate::clock::format_wire_ts)
            } else {
                None
            };
            task
        })
        .collect())
}

impl ScheduledTask {
    pub fn definition(&self) -> nuono_app::scheduled_tasks::TaskDefinition {
        nuono_app::scheduled_tasks::TaskDefinition {
            name: self.name.clone(),
            cron_expr: self.cron_expr.clone(),
            timezone: self.timezone.clone(),
            action_type: self.action_type.clone(),
            action_params: self.action_params.clone(),
            bot_id: self.bot_id.clone(),
        }
    }
}

pub async fn get(db: &Database, id: i64) -> Result<ScheduledTask> {
    scheduled_tasks::Entity::find_by_id(id)
        .one(db.orm())
        .await?
        .map(Into::into)
        .ok_or_else(|| crate::error::NuonoError::NotFound(format!("定时任务 {id} 不存在")))
}

/// 查询调度器需要加载的已启用定时任务列表（在 SQL 层直接限制条数与单字段读取长度；调用方可传入 `上限 + 1` 以检测任务总数是否超限）。
pub async fn list_for_scheduler(db: &Database, limit: u64) -> Result<Vec<ScheduledTask>> {
    let mut query = scheduled_tasks::Entity::find()
        .select_only()
        .columns([
            scheduled_tasks::Column::Id,
            scheduled_tasks::Column::Enabled,
            scheduled_tasks::Column::LastRunAt,
            scheduled_tasks::Column::CreatedAt,
            scheduled_tasks::Column::Revision,
        ])
        .filter(
            Condition::any()
                .add(scheduled_tasks::Column::Enabled.eq(true))
                .add(scheduled_tasks::Column::Enabled.is_null()),
        )
        .order_by_asc(scheduled_tasks::Column::Id)
        .limit(limit);
    for (column, sql_name, max_bytes) in [
        (scheduled_tasks::Column::Name, "name", MAX_NAME_BYTES),
        (
            scheduled_tasks::Column::CronExpr,
            "cron_expr",
            MAX_CRON_BYTES,
        ),
        (
            scheduled_tasks::Column::ActionType,
            "action_type",
            MAX_ACTION_TYPE_BYTES,
        ),
        (
            scheduled_tasks::Column::ActionParams,
            "action_params",
            MAX_ACTION_PARAMS_BYTES,
        ),
        (scheduled_tasks::Column::BotId, "bot_id", MAX_BOT_ID_BYTES),
        (scheduled_tasks::Column::Timezone, "timezone", 64),
    ] {
        // PostgreSQL 与 SQLite 的 SUBSTR 均按字符数截取：最多取 max_bytes + 1 个字符即可限制单字段读入内存的 UTF-8 分配上限；
        // 下方的 validate_definition_size 字节校验会拒绝所有被截断的超长字段以及字符数未超但 UTF-8 字节数超限的多字节文本。
        query = query.column_as(
            Expr::cust(format!("substr({sql_name}, 1, {})", max_bytes + 1)),
            column,
        );
    }
    let rows = query
        .into_model::<scheduled_tasks::Model>()
        .all(db.orm())
        .await?;
    rows.into_iter()
        .map(|row| {
            validate_definition_size(
                &row.name,
                &row.cron_expr,
                &row.action_type,
                row.action_params.as_deref(),
                row.bot_id.as_deref(),
            )?;
            Ok(row.into())
        })
        .collect()
}

pub async fn create(
    db: &Database,
    name: &str,
    cron_expr: &str,
    action_type: &str,
    action_params: Option<&str>,
    bot_id: Option<&str>,
) -> Result<ScheduledTask> {
    create_definition(
        db,
        &nuono_app::scheduled_tasks::TaskDefinition {
            name: name.into(),
            cron_expr: cron_expr.into(),
            timezone: nuono_app::scheduled_tasks::default_timezone(),
            action_type: action_type.into(),
            action_params: action_params.map(str::to_owned),
            bot_id: bot_id.map(str::to_owned),
        },
    )
    .await
}

pub async fn create_definition(
    db: &Database,
    definition: &nuono_app::scheduled_tasks::TaskDefinition,
) -> Result<ScheduledTask> {
    definition.validate()?;
    let nuono_app::scheduled_tasks::TaskDefinition {
        name,
        cron_expr,
        timezone,
        action_type,
        action_params,
        bot_id,
    } = definition;
    let now = crate::clock::db_now();
    let am = scheduled_tasks::ActiveModel {
        name: Set(name.to_string()),
        cron_expr: Set(cron_expr.to_string()),
        action_type: Set(action_type.to_string()),
        timezone: Set(timezone.clone()),
        revision: Set(0),
        action_params: Set(action_params.clone()),
        bot_id: Set(bot_id.clone().filter(|s| !s.trim().is_empty())),
        enabled: Set(Some(true)),
        created_at: Set(crate::clock::store_time(Some(now))),
        // last_run_at / id 走数据库默认值与自增。
        ..Default::default()
    };

    // 将插入新任务与按主键回读完整记录放在同一写事务内，确保回读失败时不会在报错背后留下已提交的孤儿任务。
    let tx = db.begin_write().await?;
    let inserted = scheduled_tasks::Entity::insert(am).exec(&tx).await?;
    let row = scheduled_tasks::Entity::find_by_id(inserted.last_insert_id)
        .one(&tx)
        .await?
        .ok_or_else(|| {
            crate::error::NuonoError::DatabaseQuery(format!(
                "定时任务 {} 插入后回读失败",
                inserted.last_insert_id
            ))
        })?;

    tx.commit().await?;
    Ok(row.into())
}

pub async fn update(
    db: &Database,
    id: i64,
    revision: i64,
    definition: &nuono_app::scheduled_tasks::TaskDefinition,
) -> Result<ScheduledTask> {
    definition.validate()?;
    let next_revision = revision
        .checked_add(1)
        .filter(|_| revision >= 0)
        .ok_or_else(|| crate::error::NuonoError::Validation("任务版本无效".into()))?;
    let tx = db.begin_write().await?;
    let changed = scheduled_tasks::Entity::update_many()
        .set(scheduled_tasks::ActiveModel {
            name: Set(definition.name.clone()),
            cron_expr: Set(definition.cron_expr.clone()),
            timezone: Set(definition.timezone.clone()),
            revision: Set(next_revision),
            action_type: Set(definition.action_type.clone()),
            action_params: Set(definition.action_params.clone()),
            bot_id: Set(definition
                .bot_id
                .clone()
                .filter(|value| !value.trim().is_empty())),
            ..Default::default()
        })
        .filter(scheduled_tasks::Column::Id.eq(id))
        .filter(scheduled_tasks::Column::Revision.eq(revision))
        .exec(&tx)
        .await?;
    if changed.rows_affected != 1 {
        return Err(crate::error::NuonoError::Conflict(
            "任务已被修改或删除，请重新加载后保存".into(),
        ));
    }
    let row = scheduled_tasks::Entity::find_by_id(id)
        .one(&tx)
        .await?
        .ok_or_else(|| crate::error::NuonoError::NotFound("任务不存在".into()))?;
    tx.commit().await?;
    Ok(row.into())
}

pub async fn toggle(db: &Database, id: i64, enabled: bool) -> Result<()> {
    let tx = db.begin_write().await?;
    let mut query = scheduled_tasks::Entity::find_by_id(id);
    if db.backend() == DbBackend::Postgres {
        query = query.lock_exclusive();
    }
    let row = query
        .one(&tx)
        .await?
        .ok_or_else(|| crate::error::NuonoError::NotFound(format!("定时任务 {} 不存在", id)))?;
    // 历史遗留的不受支持或格式异常的旧任务仍允许读取和停用；但重新启用时必须通过与新建任务一致的完整合法性校验。
    if enabled {
        ScheduledTask::from(row.clone()).definition().validate()?;
    }
    let result = scheduled_tasks::Entity::update_many()
        .set(scheduled_tasks::ActiveModel {
            enabled: Set(Some(enabled)),
            revision: Set(row
                .revision
                .checked_add(1)
                .ok_or_else(|| crate::error::NuonoError::Conflict("任务版本已耗尽".into()))?),
            ..Default::default()
        })
        .filter(scheduled_tasks::Column::Id.eq(id))
        .exec(&tx)
        .await?;

    if result.rows_affected == 0 {
        return Err(crate::error::NuonoError::NotFound(format!(
            "定时任务 {} 不存在",
            id
        )));
    }
    tx.commit().await?;
    Ok(())
}

pub async fn remove(db: &Database, id: i64) -> Result<()> {
    scheduled_tasks::Entity::delete_by_id(id)
        .exec(db.orm())
        .await?;
    Ok(())
}

/// 记录一次已触发的调度事件；已删除任务仍视为成功的空操作。
pub async fn record_last_run(db: &Database, id: i64) -> Result<()> {
    let now = crate::clock::db_now();
    scheduled_tasks::Entity::update_many()
        .set(scheduled_tasks::ActiveModel {
            last_run_at: Set(crate::clock::store_time(Some(now))),
            ..Default::default()
        })
        .filter(scheduled_tasks::Column::Id.eq(id))
        .exec(db.orm())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, PaginatorTrait};

    const SEND_PARAMS: &str = r#"{"target_type":"group","target_id":"1","message":"hello"}"#;

    async fn test_db() -> Database {
        crate::test_support::test_db(1).await
    }

    #[tokio::test]
    async fn scheduler_query_filters_and_bounds_enabled_rows_including_legacy_nulls() {
        let db = test_db().await;
        let enabled = create(
            &db,
            "enabled",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            None,
        )
        .await
        .unwrap();
        let disabled = create(
            &db,
            "disabled",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            None,
        )
        .await
        .unwrap();
        toggle(&db, disabled.id, false).await.unwrap();
        let legacy = scheduled_tasks::ActiveModel {
            name: Set("legacy".into()),
            cron_expr: Set("0 0 * * * *".into()),
            action_type: Set("send_message".into()),
            action_params: Set(Some(SEND_PARAMS.into())),
            enabled: Set(None),
            ..Default::default()
        }
        .insert(db.orm())
        .await
        .unwrap();
        let selected = list_for_scheduler(&db, 10).await.unwrap();
        assert_eq!(
            selected.iter().map(|row| row.id).collect::<Vec<_>>(),
            [enabled.id, legacy.id]
        );
        assert_eq!(list_for_scheduler(&db, 1).await.unwrap()[0].id, enabled.id);
        assert_eq!(list_for_scheduler(&db, 1).await.unwrap().len(), 1);
        assert!(list_for_scheduler(&db, 0).await.unwrap().is_empty());
        assert_eq!(
            list(&db).await.unwrap().len(),
            3,
            "management projection still includes disabled rows"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn scheduler_query_rejects_oversized_legacy_fields_without_executing_truncations() {
        let db = test_db().await;
        for (field, oversized) in [
            ("name", "好".repeat(MAX_NAME_BYTES / 3 + 1)),
            ("cron_expr", "好".repeat(MAX_CRON_BYTES / 3 + 1)),
            ("action_type", "好".repeat(MAX_ACTION_TYPE_BYTES / 3 + 1)),
            ("bot_id", "1".repeat(MAX_BOT_ID_BYTES + 1)),
            // 即使截取后的前 65537 个字符恰好构成合法 JSON（例如尾部追加了大量空格），字节长度校验也必须拒绝，绝不能静默执行被截断的任务定义。
            ("action_params", format!("{}{}", r#"{"topic":"news","message":"hello"}"#, " ".repeat(MAX_ACTION_PARAMS_BYTES * 2))),
            // 字符数未超过 65537 但 UTF-8 字节数超过 65536 的多字节文本：即使 SQL 的 SUBSTR 未截断它，Rust 侧的字节长度校验也必须拒绝。
            ("action_params", serde_json::json!({"topic":"news", "message":"好".repeat(MAX_ACTION_PARAMS_BYTES / 3 + 1)}).to_string()),
        ] {
            let mut row = scheduled_tasks::ActiveModel {
                name: Set("legacy oversized".into()),
                cron_expr: Set("0 0 * * * *".into()),
                action_type: Set("push_subscribe".into()),
                action_params: Set(Some(r#"{"topic":"news","message":"hello"}"#.into())),
                bot_id: Set(None),
                enabled: Set(Some(true)),
                ..Default::default()
            };
            match field {
                "name" => row.name = Set(oversized),
                "cron_expr" => row.cron_expr = Set(oversized),
                "action_type" => row.action_type = Set(oversized),
                "bot_id" => row.bot_id = Set(Some(oversized)),
                "action_params" => row.action_params = Set(Some(oversized)),
                _ => unreachable!(),
            }
            let before = row.insert(db.orm()).await.unwrap();
            assert!(matches!(
                list_for_scheduler(&db, 10).await,
                Err(crate::error::NuonoError::Validation(message)) if message.contains(field)
            ));
            let after = scheduled_tasks::Entity::find_by_id(before.id)
                .one(db.orm()).await.unwrap().unwrap();
            assert_eq!(after, before, "query must not replace oversized persisted data");
            toggle(&db, before.id, false).await.unwrap();
            assert!(list_for_scheduler(&db, 10).await.unwrap().is_empty());
            assert!(matches!(
                toggle(&db, before.id, true).await,
                Err(crate::error::NuonoError::Validation(message)) if message.contains(field)
            ));
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_definitions_are_rejected_without_persisting_tasks() {
        let db = test_db().await;
        let long_name = "n".repeat(101);
        let oversized_params = format!("{}{}", SEND_PARAMS, " ".repeat(MAX_ACTION_PARAMS_BYTES));
        for (name, cron, action, params, bot_id) in [
            ("  ", "0 0 * * * *", "send_message", Some(SEND_PARAMS), None),
            (
                long_name.as_str(),
                "0 0 * * * *",
                "send_message",
                Some(SEND_PARAMS),
                None,
            ),
            (
                "oversized params",
                "0 0 * * * *",
                "send_message",
                Some(oversized_params.as_str()),
                None,
            ),
            (
                "invalid cron",
                "not cron",
                "send_message",
                Some(SEND_PARAMS),
                None,
            ),
            (
                "missing seconds",
                "0 * * * *",
                "send_message",
                Some(SEND_PARAMS),
                None,
            ),
            (
                "past cron",
                "0 0 0 * * * 2000",
                "send_message",
                Some(SEND_PARAMS),
                None,
            ),
            ("retired action", "0 0 * * * *", "run_sop", Some("{}"), None),
            ("no params", "0 0 * * * *", "send_message", None, None),
            (
                "bad json",
                "0 0 * * * *",
                "send_message",
                Some("bad json"),
                None,
            ),
            (
                "empty action",
                "0 0 * * * *",
                "send_message",
                Some("{}"),
                None,
            ),
            (
                "bad target",
                "0 0 * * * *",
                "send_message",
                Some(r#"{"target_type":"group","target_id":"bad","message":"hello"}"#),
                None,
            ),
            (
                "bad topic",
                "0 0 * * * *",
                "push_subscribe",
                Some(r#"{"topic":" ","message":"hello"}"#),
                None,
            ),
            (
                "bad bot",
                "0 0 * * * *",
                "send_message",
                Some(SEND_PARAMS),
                Some("bad"),
            ),
        ] {
            assert!(
                matches!(
                    create(&db, name, cron, action, params, bot_id).await,
                    Err(crate::error::NuonoError::Validation(_))
                ),
                "{name}"
            );
            assert!(list(&db).await.unwrap().is_empty(), "{name}");
        }
        // 拒绝不会毒化连接，也不会挡住下一次有效写入。
        create(
            &db,
            "valid",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            Some("42"),
        )
        .await
        .unwrap();
        assert_eq!(list(&db).await.unwrap().len(), 1);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_legacy_tasks_cannot_be_enabled_but_remain_readable_and_disableable() {
        let db = test_db().await;
        for (cron, action, params, bot_id, enabled) in [
            (
                "bad cron",
                "send_message",
                Some(SEND_PARAMS),
                None,
                Some(false),
            ),
            ("0 0 * * * *", "run_sop", Some("{}"), None, Some(true)),
            ("0 0 * * * *", "send_message", None, None, None),
            (
                "0 0 * * * *",
                "send_message",
                Some(SEND_PARAMS),
                Some("bad"),
                Some(false),
            ),
        ] {
            let now = crate::clock::db_now();
            let before = scheduled_tasks::ActiveModel {
                name: Set("legacy".into()),
                cron_expr: Set(cron.into()),
                action_type: Set(action.into()),
                action_params: Set(params.map(str::to_owned)),
                bot_id: Set(bot_id.map(str::to_owned)),
                enabled: Set(enabled),
                created_at: Set(crate::clock::store_time(Some(now))),
                last_run_at: Set(crate::clock::store_time(Some(now))),
                ..Default::default()
            }
            .insert(db.orm())
            .await
            .unwrap();
            assert!(matches!(
                toggle(&db, before.id, true).await,
                Err(crate::error::NuonoError::Validation(_))
            ));
            let after = scheduled_tasks::Entity::find_by_id(before.id)
                .one(db.orm())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                after, before,
                "rejected enable must not mutate the legacy row"
            );
            assert!(
                list(&db)
                    .await
                    .unwrap()
                    .iter()
                    .any(|task| task.id == before.id)
            );
            toggle(&db, before.id, false).await.unwrap();
            let after = scheduled_tasks::Entity::find_by_id(before.id)
                .one(db.orm())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                after,
                scheduled_tasks::Model {
                    enabled: Some(false),
                    revision: before.revision + 1,
                    ..before
                }
            );
        }
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_readback_failure_rolls_back_all_insert_side_effects() {
        let db = test_db().await;
        db.orm()
            .execute_unprepared("CREATE TABLE task_insert_probe (task_id INTEGER NOT NULL)")
            .await
            .unwrap();
        // 用 AFTER INSERT 触发器模拟在 INSERT 成功与按主键回读之间任务行被意外删除的异常场景；
        // 同时向探测表 task_insert_probe 写入一行，用于验证回读失败后整个写事务（含探测表写入）都会完整回滚。
        let statements: &[&str] = if db.backend() == DbBackend::Postgres {
            &[
                "CREATE FUNCTION discard_new_task() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO task_insert_probe VALUES (NEW.id); DELETE FROM nuono_scheduled_tasks WHERE id = NEW.id; RETURN NEW; END $$",
                "CREATE TRIGGER discard_new_task AFTER INSERT ON nuono_scheduled_tasks FOR EACH ROW EXECUTE FUNCTION discard_new_task()",
            ]
        } else {
            &[
                "CREATE TRIGGER discard_new_task AFTER INSERT ON nuono_scheduled_tasks BEGIN INSERT INTO task_insert_probe VALUES (NEW.id); DELETE FROM nuono_scheduled_tasks WHERE id = NEW.id; END",
            ]
        };
        for statement in statements {
            db.orm().execute_unprepared(statement).await.unwrap();
        }
        assert!(matches!(
            create(
                &db,
                "disappearing",
                "0 0 * * * *",
                "send_message",
                Some(SEND_PARAMS),
                None
            )
            .await,
            Err(crate::error::NuonoError::DatabaseQuery(_))
        ));
        assert_eq!(
            scheduled_tasks::Entity::find()
                .count(db.orm())
                .await
                .unwrap(),
            0
        );
        let probe = db
            .orm()
            .query_one_raw(sea_orm::Statement::from_string(
                db.backend(),
                "SELECT COUNT(*) AS count FROM task_insert_probe",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.try_get::<i64>("", "count").unwrap(), 0);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn create_roundtrips_bot_id() {
        let db = test_db().await;

        let bound = create(
            &db,
            "bound",
            "0 0 0 * * *",
            "send_message",
            Some(r#"{"target_type":"group","target_id":"1","message":"x"}"#),
            Some("12345"),
        )
        .await
        .unwrap();
        assert_eq!(bound.bot_id.as_deref(), Some("12345"));

        let listed = list(&db).await.unwrap();
        assert_eq!(listed[0].bot_id.as_deref(), Some("12345"));

        // 空白 bot_id 归一为 NULL，与「未绑定」同义。
        let legacy = create(
            &db,
            "legacy",
            "0 0 0 * * *",
            "send_message",
            Some(SEND_PARAMS),
            Some("  "),
        )
        .await
        .unwrap();
        assert!(legacy.bot_id.is_none());

        // 既有非空值予以保留，包括两侧空白。
        let padded = create(
            &db,
            "padded",
            "0 0 0 * * *",
            "send_message",
            Some(SEND_PARAMS),
            Some(" 12345 "),
        )
        .await
        .unwrap();
        assert_eq!(padded.bot_id.as_deref(), Some(" 12345 "));

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn toggle_reports_missing_tasks_and_targets_only_the_selected_id() {
        let db = test_db().await;
        assert!(list(&db).await.unwrap().is_empty());
        let selected = create(
            &db,
            "selected",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            None,
        )
        .await
        .unwrap();
        let other = create(
            &db,
            "other",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            None,
        )
        .await
        .unwrap();
        assert!(selected.enabled);
        assert!(other.enabled);
        assert!(selected.created_at.is_some());
        assert!(selected.last_run_at.is_none());
        assert!(selected.next_run_at.is_none());

        toggle(&db, selected.id, false).await.unwrap();
        let rows = list(&db).await.unwrap();
        assert!(
            !rows
                .iter()
                .find(|row| row.id == selected.id)
                .unwrap()
                .enabled
        );
        assert!(rows.iter().find(|row| row.id == other.id).unwrap().enabled);
        let missing_error = toggle(&db, i64::MAX, false).await.unwrap_err();
        assert!(matches!(
            missing_error,
            crate::error::NuonoError::NotFound(message)
                if message == format!("定时任务 {} 不存在", i64::MAX)
        ));
        toggle(&db, selected.id, true).await.unwrap();
        assert!(list(&db).await.unwrap().iter().all(|row| row.enabled));

        remove(&db, selected.id).await.unwrap();
        remove(&db, selected.id).await.unwrap();
        let remaining = list(&db).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, other.id);
        assert!(matches!(
            toggle(&db, selected.id, true).await.unwrap_err(),
            crate::error::NuonoError::NotFound(message)
                if message == format!("定时任务 {} 不存在", selected.id)
        ));
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_null_enabled_and_timestamps_keep_their_read_projection() {
        let db = test_db().await;
        let last_run_at = chrono::DateTime::parse_from_rfc3339("2026-10-04T08:09:10+08:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let legacy = scheduled_tasks::ActiveModel {
            name: Set("legacy".into()),
            cron_expr: Set("0 0 * * * *".into()),
            action_type: Set("send_message".into()),
            action_params: Set(None),
            bot_id: Set(None),
            enabled: Set(None),
            last_run_at: Set(crate::clock::store_time(Some(last_run_at))),
            created_at: Set(None),
            ..Default::default()
        }
        .insert(db.orm())
        .await
        .unwrap();

        let rows = list(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, legacy.id);
        assert!(rows[0].enabled);
        assert!(rows[0].bot_id.is_none());
        assert!(rows[0].action_params.is_none());
        assert!(rows[0].created_at.is_none());
        assert_eq!(
            rows[0].last_run_at,
            Some(last_run_at.timestamp_millis().to_string())
        );
        assert!(
            rows[0].next_run_at.is_none(),
            "invalid legacy actions are not scheduled"
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn recording_last_run_changes_only_the_selected_task_and_ignores_missing_ids() {
        let db = test_db().await;
        let selected = create(
            &db,
            "selected",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            None,
        )
        .await
        .unwrap();
        let other = create(
            &db,
            "other",
            "0 0 * * * *",
            "send_message",
            Some(SEND_PARAMS),
            None,
        )
        .await
        .unwrap();
        let before = scheduled_tasks::Entity::find_by_id(selected.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let earliest = crate::clock::db_now();

        record_last_run(&db, selected.id).await.unwrap();
        record_last_run(&db, i64::MAX).await.unwrap();

        let latest = crate::clock::db_now();
        let updated = scheduled_tasks::Entity::find_by_id(selected.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let recorded = updated.last_run_at.unwrap();
        // PostgreSQL 存微秒精度，因此容忍亚微秒截断。
        assert!(recorded >= earliest - chrono::Duration::microseconds(1));
        assert!(recorded <= latest);
        assert_eq!(updated.created_at, before.created_at);
        assert_eq!(updated.enabled, before.enabled);
        let unchanged = scheduled_tasks::Entity::find_by_id(other.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(unchanged.last_run_at.is_none());
        db.shutdown().await.unwrap();
    }
}
