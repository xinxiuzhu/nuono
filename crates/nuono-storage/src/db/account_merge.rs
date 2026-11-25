//! 双重身份验证的账号合并：预检快照、核心归属迁移、插件数据迁移与完成回执在同一事务内原子提交。
use super::{
    Database,
    account_state::{self, AuditEvent, append_audit_on, challenges, session_on, token_hash},
};
use crate::{
    entity::{account_assistant, plugin_overrides, users},
    error::{NuonoError, Result},
};
use nuono_app::{account_merge::*, plugin_manifest::AccountMergePolicy};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, Statement,
    sea_query::{Expr, ExprTrait},
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

mod jobs {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "nuono_account_merge_jobs")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub source_user_id: i64,
        pub target_user_id: i64,
        pub source_proof_id: Option<String>,
        pub target_proof_id: Option<String>,
        pub status: String,
        pub plan_json: String,
        pub failure_code: Option<String>,
        pub completed_at_ms: Option<i64>,
        pub revision: i64,
        pub created_at_ms: i64,
        pub updated_at_ms: i64,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// 源账号与目标账号的密码及两步验证码仅作为临时校验输入，绝不写入合并计划或审计日志。
pub struct MergeCredentials<'a> {
    pub source_username: &'a str,
    pub source_password: &'a str,
    pub source_second_factor: Option<&'a str>,
    pub target_password: &'a str,
    pub target_second_factor: Option<&'a str>,
}
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct AccountMergeView {
    pub id: String,
    pub source_user_id: String,
    pub target_user_id: String,
    pub status: String,
    pub revision: String,
    pub plan_digest: String,
    pub plan: Value,
    pub completed_at_ms: Option<String>,
}
impl From<jobs::Model> for AccountMergeView {
    fn from(job: jobs::Model) -> Self {
        Self {
            id: job.id,
            source_user_id: job.source_user_id.to_string(),
            target_user_id: job.target_user_id.to_string(),
            status: job.status,
            revision: job.revision.to_string(),
            plan_digest: digest(job.plan_json.as_bytes()),
            plan: serde_json::from_str(&job.plan_json).unwrap_or(Value::Null),
            completed_at_ms: job.completed_at_ms.map(|v| v.to_string()),
        }
    }
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn rejected(message: impl Into<String>) -> NuonoError {
    NuonoError::Conflict(message.into())
}

/// 插件合并参与方受限 SQL 校验：仅允许针对单张已登记表的简单 SELECT 或 DML 语句。
/// 刻意禁止子查询、JOIN、CTE、SQL 注释以及任意 SQL 函数调用。
fn allowed_statement(statement: &Statement, tables: &BTreeSet<String>, write: bool) -> Result<()> {
    let sql = statement.sql.trim();
    if sql.contains(';')
        || sql.contains('\0')
        || sql.contains('"')
        || sql.contains('`')
        || sql.contains('\'')
        || sql.contains("--")
        || sql.contains("/*")
        || sql.contains('\\')
    {
        return Err(rejected("合并参与者 SQL 必须使用绑定参数与简单登记表语句"));
    }
    let tokens: Vec<_> = sql
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .collect();
    let upper: Vec<_> = tokens.iter().map(|s| s.to_ascii_uppercase()).collect();
    let first = upper.first().map(String::as_str).unwrap_or("");
    if (!write && first != "SELECT") || (write && !matches!(first, "UPDATE" | "DELETE" | "INSERT"))
    {
        return Err(rejected("合并参与者不允许此 SQL 操作"));
    }
    for prohibited in [
        "WITH",
        "JOIN",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "PRAGMA",
        "ATTACH",
        "DETACH",
        "COPY",
        "CALL",
        "DO",
        "INTO",
        "RETURNING",
        "CONFLICT",
        "CREATE",
        "ALTER",
        "DROP",
        "TRIGGER",
        "VACUUM",
    ] {
        if upper.iter().any(|t| t == prohibited) && !(prohibited == "INTO" && first == "INSERT") {
            return Err(rejected("合并参与者 SQL 超出受限事务能力"));
        }
    }
    if upper.iter().filter(|t| t.as_str() == "SELECT").count() > usize::from(first == "SELECT") {
        return Err(rejected("合并参与者不允许子查询"));
    }
    let at = match first {
        "SELECT" | "DELETE" => upper.iter().position(|s| s == "FROM").map(|i| i + 1),
        "UPDATE" => Some(1),
        "INSERT" => upper.iter().position(|s| s == "INTO").map(|i| i + 1),
        _ => None,
    }
    .ok_or_else(|| rejected("缺少合并表名"))?;
    let table = tokens.get(at).ok_or_else(|| rejected("缺少合并表名"))?;
    if !tables.contains(*table) {
        return Err(rejected("合并参与者只能访问自己登记的表"));
    }
    if upper.iter().filter(|t| t.as_str() == "FROM").count() > 1 || sql.contains('.') {
        return Err(rejected("合并参与者不允许跨表引用"));
    }
    // 拒绝任意 SQL 函数调用（防止调用带副作用的 PostgreSQL 内置函数）。
    let function = regex::Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*\(")
        .map_err(|e| NuonoError::Internal(e.to_string()))?;
    for item in function.captures_iter(sql) {
        let name = &item[1];
        if !matches!(
            name.to_ascii_uppercase().as_str(),
            "COUNT" | "IN" | "VALUES"
        ) && !(first == "INSERT" && name == *table)
        {
            return Err(rejected("合并参与者不允许 SQL 函数"));
        }
    }
    // 禁止在 SELECT/DELETE 的 FROM 子句中使用表别名或逗号连接多表，防止绕过单表访问边界。
    if matches!(first, "SELECT" | "DELETE") {
        let after = &upper[at + 1..];
        if after
            .first()
            .is_some_and(|v| !matches!(v.as_str(), "WHERE" | "ORDER" | "LIMIT" | "GROUP"))
        {
            return Err(rejected("合并参与者不允许表别名或多表查询"));
        }
        let from = sql
            .to_ascii_uppercase()
            .find("FROM")
            .ok_or_else(|| rejected("缺少FROM"))?;
        let end = sql[from + 4..].find(',').map(|p| p + from + 4);
        let clause = [" WHERE ", " ORDER ", " GROUP ", " LIMIT "]
            .iter()
            .filter_map(|v| sql.to_ascii_uppercase().find(v))
            .min()
            .unwrap_or(sql.len());
        if end.is_some_and(|v| v < clause) {
            return Err(rejected("合并参与者不允许多表查询"));
        }
    }
    Ok(())
}
pub(crate) struct ParticipantTransaction<'a> {
    pub(crate) tx: &'a DatabaseTransaction,
    pub(crate) tables: BTreeSet<String>,
    pub(crate) plugin_id: String,
    pub(crate) source: i64,
    pub(crate) target: i64,
}
#[async_trait::async_trait]
impl AccountMergeRead for ParticipantTransaction<'_> {
    fn backend(&self) -> sea_orm::DbBackend {
        self.tx.get_database_backend()
    }
    async fn account_state(&self, user_id: i64) -> Result<Vec<MergeStateRecord>> {
        self.require_account(user_id)?;
        let rows = crate::entity::plugin_kv::Entity::find()
            .filter(crate::entity::plugin_kv::Column::PluginId.eq(&self.plugin_id))
            .filter(crate::entity::plugin_kv::Column::ScopeKey.eq(format!("account:{user_id}")))
            .filter(crate::plugin_kv::live(
                crate::clock::db_now().timestamp_millis(),
            ))
            .order_by_asc(crate::entity::plugin_kv::Column::Collection)
            .order_by_asc(crate::entity::plugin_kv::Column::RecordKey)
            .all(self.tx)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(MergeStateRecord {
                    collection: row.collection,
                    key: row.record_key,
                    value_version: row.value_version,
                    value: serde_json::from_str(&row.value_json)?,
                })
            })
            .collect()
    }
    async fn query(&self, statement: Statement) -> Result<Vec<sea_orm::QueryResult>> {
        allowed_statement(&statement, &self.tables, false)?;
        Ok(self.tx.query_all_raw(statement).await?)
    }
}
#[async_trait::async_trait]
impl AccountMergeTransaction for ParticipantTransaction<'_> {
    async fn put_account_state(&self, user_id: i64, record: MergeStateRecord) -> Result<()> {
        self.require_account(user_id)?;
        let scope = nuono_app::plugin_data::PluginStateScope::account(user_id)?;
        nuono_app::plugin_data::PluginStateCollection::new(
            &self.plugin_id,
            &record.collection,
            scope,
        )?;
        if record.key.is_empty()
            || record.key.len() > 200
            || record.key.contains('\0')
            || record.value_version <= 0
        {
            return Err(rejected("插件账号状态键或版本无效"));
        }
        let value = serde_json::to_string(&record.value)?;
        nuono_domain::scalar::parse_json(&value)?;
        if value.len() > 65_536 {
            return Err(rejected("插件账号状态值过大"));
        }
        self.tx.execute_raw(Statement::from_sql_and_values(self.backend(),"INSERT INTO nuono_plugin_kv (plugin_id,collection,scope_key,record_key,value_json,value_version,revision,created_at_ms,updated_at_ms,generation) VALUES ($1,$2,$3,$4,$5,$6,1,$7,$7,$8) ON CONFLICT(plugin_id,collection,scope_key,record_key) DO UPDATE SET value_json=excluded.value_json,value_version=excluded.value_version,revision=nuono_plugin_kv.revision+1,updated_at_ms=excluded.updated_at_ms",[self.plugin_id.clone().into(),record.collection.into(),format!("account:{user_id}").into(),record.key.into(),value.into(),record.value_version.into(),crate::clock::db_now().timestamp_millis().into(),uuid::Uuid::new_v4().to_string().into()])).await?;
        Ok(())
    }
    async fn delete_account_state(&self, user_id: i64, collection: &str, key: &str) -> Result<()> {
        self.require_account(user_id)?;
        self.tx.execute_raw(Statement::from_sql_and_values(self.backend(),"DELETE FROM nuono_plugin_kv WHERE plugin_id=$1 AND collection=$2 AND scope_key=$3 AND record_key=$4",[self.plugin_id.clone().into(),collection.into(),format!("account:{user_id}").into(),key.into()])).await?;
        Ok(())
    }
    async fn execute(&self, statement: Statement) -> Result<u64> {
        allowed_statement(&statement, &self.tables, true)?;
        Ok(self.tx.execute_raw(statement).await?.rows_affected())
    }
}

impl ParticipantTransaction<'_> {
    fn require_account(&self, user_id: i64) -> Result<()> {
        if user_id != self.source && user_id != self.target {
            return Err(rejected("合并参与者只能访问本次两个账号的状态"));
        }
        Ok(())
    }
}

async fn second_factor(
    tx: &DatabaseTransaction,
    user: &users::orm::Model,
    code: Option<&str>,
) -> Result<()> {
    if !user.totp_enabled {
        return Ok(());
    }
    let code = code
        .filter(|v| !v.is_empty())
        .ok_or_else(|| NuonoError::PermissionDenied("账号合并需要两步验证码或恢复码".into()))?;
    let secret = std::str::from_utf8(user.totp_secret.as_deref().unwrap_or_default())
        .map_err(|_| rejected("两步验证密钥无效"))?;
    if let Some(step) =
        nuono_app::auth::validate_code_counter(secret, code, chrono::Utc::now().timestamp() as u64)
            .filter(|v| user.totp_last_counter.is_none_or(|old| *v > old as u64))
    {
        users::orm::Entity::update_many()
            .col_expr(
                users::orm::Column::TotpLastCounter,
                Expr::value(step as i64),
            )
            .filter(users::orm::Column::Id.eq(user.id))
            .exec(tx)
            .await?;
        return Ok(());
    }
    let hash = nuono_app::auth::recovery_code_hash(code);
    let mut recovery: Vec<String> = serde_json::from_str(&user.totp_recovery_json)?;
    let index = recovery
        .iter()
        .position(|value| value == &hash)
        .ok_or_else(|| NuonoError::PermissionDenied("两步验证码不正确或已使用".into()))?;
    recovery.remove(index);
    users::orm::Entity::update_many()
        .col_expr(
            users::orm::Column::TotpRecoveryJson,
            Expr::value(serde_json::to_string(&recovery)?),
        )
        .filter(users::orm::Column::Id.eq(user.id))
        .exec(tx)
        .await?;
    Ok(())
}
async fn proof(
    tx: &DatabaseTransaction,
    user: &users::orm::Model,
    requester: i64,
    session: &str,
    operation: &str,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::clock::db_now().timestamp_millis();
    challenges::ActiveModel {
        id: Set(id.clone()),
        purpose: Set("merge_proof".into()),
        requester_user_id: Set(Some(requester)),
        subject_user_id: Set(Some(user.id)),
        subject_auth_epoch: Set(Some(user.auth_epoch)),
        session_id: Set(Some(session.into())),
        token_hash: Set(token_hash(&uuid::Uuid::new_v4().to_string())),
        context_json: Set(json!({"operation_id":operation}).to_string()),
        candidate_identity_id: Set(None),
        state: Set("pending".into()),
        attempts: Set(0),
        expires_at_ms: Set(now + 600_000),
        consumed_at_ms: Set(None),
        revision: Set(1),
        created_at_ms: Set(now),
        updated_at_ms: Set(now),
    }
    .insert(tx)
    .await?;
    Ok(id)
}
async fn validate_proof(
    tx: &DatabaseTransaction,
    id: Option<&str>,
    user: &users::orm::Model,
    session: &str,
    operation: &str,
) -> Result<()> {
    let row = challenges::Entity::find_by_id(id.ok_or_else(|| rejected("缺少双账号证明"))?)
        .one(tx)
        .await?
        .ok_or_else(|| rejected("账号证明已清理"))?;
    if row.purpose != "merge_proof"
        || row.state != "pending"
        || row.subject_user_id != Some(user.id)
        || row.subject_auth_epoch != Some(user.auth_epoch)
        || row.session_id.as_deref() != Some(session)
        || row.expires_at_ms <= crate::clock::db_now().timestamp_millis()
        || serde_json::from_str::<Value>(&row.context_json)?["operation_id"].as_str()
            != Some(operation)
    {
        return Err(rejected("账号证明已失效，请重新验证两个账号"));
    }
    Ok(())
}

impl Database {
    pub async fn prepare_account_merge(
        &self,
        target_id: i64,
        session_token: &str,
        credentials: MergeCredentials<'_>,
    ) -> Result<AccountMergeView> {
        let source = self
            .verify_user_password(credentials.source_username, credentials.source_password)
            .await?
            .ok_or_else(|| NuonoError::PermissionDenied("来源账号凭据不正确".into()))?;
        let target = self
            .authenticate_session(session_token)
            .await?
            .filter(|u| u.id == target_id)
            .ok_or_else(|| NuonoError::PermissionDenied("会话已失效".into()))?;
        if !self
            .password_work
            .verify(credentials.target_password, &target.password_hash)
            .await?
        {
            return Err(NuonoError::PermissionDenied("保留账号的密码不正确".into()));
        }
        if source.id == target_id {
            return Err(rejected("不能合并同一个账号"));
        }
        let tx = self.begin_write().await?;
        let (source_row, target_row) = lock_pair(&tx, source.id, target_id).await?;
        if source_row.status != "active" || target_row.status != "active" {
            return Err(rejected("账号不可用"));
        }
        if source_row.auth_epoch != source.auth_epoch
            || target_row.auth_epoch != target.auth_epoch
            || source_row.password_hash.as_deref() != Some(source.password_hash.as_str())
            || target_row.password_hash.as_deref() != Some(target.password_hash.as_str())
        {
            return Err(rejected("凭据已变化，请重新验证"));
        }
        if source_row.role == "owner" || target_row.role == "owner" {
            return Err(rejected("owner 账号须先单独完成角色转移，再进行账号合并"));
        }
        let session = session_on(&tx, token_hash(session_token), true)
            .await?
            .filter(|s| s.user_id == target_id && s.auth_epoch == target_row.auth_epoch)
            .ok_or_else(|| rejected("会话已失效"))?;
        let now = crate::clock::db_now().timestamp_millis();
        let recent = jobs::Entity::find()
            .filter(jobs::Column::TargetUserId.eq(target_id))
            .filter(jobs::Column::CreatedAtMs.gt(now - 600_000))
            .all(&tx)
            .await?;
        if recent.len() >= 5 {
            return Err(rejected("账号合并请求过于频繁，请稍后重试"));
        }
        second_factor(&tx, &source_row, credentials.source_second_factor).await?;
        second_factor(&tx, &target_row, credentials.target_second_factor).await?;
        let context = AccountMergeContext {
            operation_id: uuid::Uuid::new_v4().to_string(),
            source_user_id: source.id,
            target_user_id: target_id,
        };
        let plan = merge_plan(&tx, &context, &source_row, &target_row).await?;
        let source_proof = proof(
            &tx,
            &source_row,
            target_id,
            &session.id,
            &context.operation_id,
        )
        .await?;
        let target_proof = proof(
            &tx,
            &target_row,
            target_id,
            &session.id,
            &context.operation_id,
        )
        .await?;
        let job = jobs::ActiveModel {
            id: Set(context.operation_id.clone()),
            source_user_id: Set(source.id),
            target_user_id: Set(target_id),
            source_proof_id: Set(Some(source_proof)),
            target_proof_id: Set(Some(target_proof)),
            status: Set("ready".into()),
            plan_json: Set(plan.to_string()),
            failure_code: Set(None),
            completed_at_ms: Set(None),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
        }
        .insert(&tx)
        .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(target_id),
                actor_plugin_id: None,
                action: "account.merge_prepared",
                result: "success",
                subject_user_id: Some(source.id),
                group_id: None,
                operation_id: Some(&context.operation_id),
                details: json!({"target_user_id":target_id.to_string()}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(job.into())
    }
    pub async fn cancel_account_merge(
        &self,
        target_id: i64,
        session_token: &str,
        id: &str,
    ) -> Result<()> {
        let tx = self.begin_write().await?;
        let user = account_state::locked_user(&tx, target_id).await?;
        let session = session_on(&tx, token_hash(session_token), true)
            .await?
            .filter(|s| s.user_id == target_id && s.auth_epoch == user.auth_epoch)
            .ok_or_else(|| rejected("会话已失效"))?;
        let job = jobs::Entity::find_by_id(id)
            .filter(jobs::Column::TargetUserId.eq(target_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("合并请求不存在".into()))?;
        if job.status == "completed" {
            return Err(rejected("已完成的合并不能撤销"));
        }
        if job.status != "cancelled" {
            let now = crate::clock::db_now().timestamp_millis();
            for proof_id in [job.source_proof_id, job.target_proof_id]
                .into_iter()
                .flatten()
            {
                challenges::Entity::update_many()
                    .col_expr(challenges::Column::State, Expr::value("cancelled"))
                    .col_expr(
                        challenges::Column::Revision,
                        Expr::col(challenges::Column::Revision).add(1),
                    )
                    .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
                    .filter(challenges::Column::Id.eq(proof_id))
                    .filter(challenges::Column::SessionId.eq(&session.id))
                    .filter(challenges::Column::State.eq("pending"))
                    .exec(&tx)
                    .await?;
            }
            jobs::Entity::update_many()
                .col_expr(jobs::Column::Status, Expr::value("cancelled"))
                .col_expr(
                    jobs::Column::Revision,
                    Expr::col(jobs::Column::Revision).add(1),
                )
                .col_expr(jobs::Column::UpdatedAtMs, Expr::value(now))
                .filter(jobs::Column::Id.eq(id))
                .filter(jobs::Column::Status.ne("completed"))
                .exec(&tx)
                .await?;
            append_audit_on(
                &tx,
                AuditEvent {
                    actor_user_id: Some(target_id),
                    actor_plugin_id: None,
                    action: "account.merge_cancelled",
                    result: "success",
                    subject_user_id: Some(job.source_user_id),
                    group_id: None,
                    operation_id: Some(id),
                    details: json!({}),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn account_merge_history(&self, target_id: i64) -> Result<Vec<AccountMergeView>> {
        Ok(jobs::Entity::find()
            .filter(jobs::Column::TargetUserId.eq(target_id))
            .order_by_desc(jobs::Column::CreatedAtMs)
            .limit(20)
            .all(self.orm())
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }
    pub async fn account_merge_status(&self, target_id: i64, id: &str) -> Result<AccountMergeView> {
        Ok(jobs::Entity::find_by_id(id)
            .filter(jobs::Column::TargetUserId.eq(target_id))
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::NotFound("合并请求不存在".into()))?
            .into())
    }
    pub async fn confirm_account_merge(
        &self,
        target_id: i64,
        session_token: &str,
        id: &str,
        revision: i64,
        plan_digest: &str,
    ) -> Result<AccountMergeView> {
        let outcome = self
            .apply_account_merge(target_id, session_token, id, revision, plan_digest)
            .await;
        if outcome.is_err() {
            // 失败的合并写事务已被销毁回滚；此处另起事务记录失败状态，不会意外提交半途的合并变更。
            let tx = self.begin_write().await?;
            if let Some(job) = jobs::Entity::find_by_id(id)
                .filter(jobs::Column::TargetUserId.eq(target_id))
                .one(&tx)
                .await?
                && job.status == "ready"
            {
                jobs::Entity::update_many()
                    .col_expr(jobs::Column::Status, Expr::value("failed"))
                    .col_expr(
                        jobs::Column::FailureCode,
                        Expr::value("precondition_or_apply_failed"),
                    )
                    .col_expr(
                        jobs::Column::Revision,
                        Expr::col(jobs::Column::Revision).add(1),
                    )
                    .col_expr(
                        jobs::Column::UpdatedAtMs,
                        Expr::value(crate::clock::db_now().timestamp_millis()),
                    )
                    .filter(jobs::Column::Id.eq(id))
                    .filter(jobs::Column::Status.eq("ready"))
                    .exec(&tx)
                    .await?;
                append_audit_on(
                    &tx,
                    AuditEvent {
                        actor_user_id: Some(target_id),
                        actor_plugin_id: None,
                        action: "account.merge",
                        result: "failed",
                        subject_user_id: Some(job.source_user_id),
                        group_id: None,
                        operation_id: Some(id),
                        details: json!({"code":"precondition_or_apply_failed"}),
                    },
                )
                .await?;
            }
            tx.commit().await?;
        }
        outcome
    }
    async fn apply_account_merge(
        &self,
        target_id: i64,
        session_token: &str,
        id: &str,
        revision: i64,
        plan_digest: &str,
    ) -> Result<AccountMergeView> {
        let tx = self.begin_write().await?;
        let initial = jobs::Entity::find_by_id(id)
            .filter(jobs::Column::TargetUserId.eq(target_id))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("合并请求不存在".into()))?;
        if initial.status == "completed" {
            return Ok(initial.into());
        }
        let (source, target) = lock_pair(&tx, initial.source_user_id, target_id).await?;
        let mut query = jobs::Entity::find_by_id(id);
        if self.backend() == sea_orm::DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let job = query
            .one(&tx)
            .await?
            .ok_or_else(|| rejected("合并请求已不存在"))?;
        if job.status == "completed" {
            return Ok(job.into());
        }
        if source.status != "active" || target.status != "active" {
            return Err(rejected("账号不可用"));
        }
        if job.status != "ready"
            || job.revision != revision
            || digest(job.plan_json.as_bytes()) != plan_digest
        {
            return Err(rejected("合并预览已变化，请重新确认"));
        }
        let session = session_on(&tx, token_hash(session_token), true)
            .await?
            .filter(|s| s.user_id == target_id && s.auth_epoch == target.auth_epoch)
            .ok_or_else(|| rejected("会话已失效"))?;
        validate_proof(
            &tx,
            job.source_proof_id.as_deref(),
            &source,
            &session.id,
            id,
        )
        .await?;
        validate_proof(
            &tx,
            job.target_proof_id.as_deref(),
            &target,
            &session.id,
            id,
        )
        .await?;
        let context = AccountMergeContext {
            operation_id: id.into(),
            source_user_id: source.id,
            target_user_id: target_id,
        };
        let plan = merge_plan(&tx, &context, &source, &target).await?;
        if plan != nuono_domain::scalar::parse_json(&job.plan_json)? {
            return Err(rejected("账号数据或插件版本已变化，请重新预览"));
        }
        merge_core(&tx, &context).await?;
        for plugin in crate::portable::registered_plugins(&tx).await? {
            let Some(item) = plan["plugins"].get(&plugin.plugin_id) else {
                continue;
            };
            let participant = account_merge_participant(&plugin.plugin_id)?
                .ok_or_else(|| rejected("插件合并参与者已卸载"))?;
            let port = ParticipantTransaction {
                tx: &tx,
                tables: crate::portable::manifest_tables(&plugin.manifest)?,
                plugin_id: plugin.plugin_id.clone(),
                source: context.source_user_id,
                target: context.target_user_id,
            };
            let retained = retained_plugin_references(&tx, &plugin.manifest, source.id).await?;
            participant.apply(&context, &item["plan"], &port).await?;
            if retained_plugin_references(&tx, &plugin.manifest, source.id).await? != retained {
                return Err(rejected(format!(
                    "插件 {} 修改了声明保留的历史账号归属",
                    plugin.plugin_id
                )));
            }
            for reference in &plugin.manifest.account_references {
                if reference.merge_policy == AccountMergePolicy::FollowAccount
                    && reference_count(&tx, &reference.table, &reference.column, source.id).await?
                        > 0
                {
                    return Err(rejected(format!(
                        "插件 {} 未完成账号数据转移",
                        plugin.plugin_id
                    )));
                }
            }
        }
        merge_plugin_state(&tx, source.id, target.id).await?;
        for participant in registered_user_merge_participants()? {
            participant.merge_in_tx(&tx, source.id, target.id).await?;
        }
        for plugin in crate::portable::registered_plugins(&tx).await? {
            crate::plugin_global_settings::purge_unused_plugin_secrets_on(&tx, &plugin.plugin_id)
                .await?;
        }
        account_state::revoke_sessions_on(&tx, source.id).await?;
        account_state::revoke_sessions_on(&tx, target.id).await?;
        let now = crate::clock::db_now().timestamp_millis();
        for proof_id in [job.source_proof_id, job.target_proof_id]
            .into_iter()
            .flatten()
        {
            challenges::Entity::update_many()
                .col_expr(challenges::Column::State, Expr::value("consumed"))
                .col_expr(challenges::Column::ConsumedAtMs, Expr::value(now))
                .col_expr(
                    challenges::Column::Revision,
                    Expr::col(challenges::Column::Revision).add(1),
                )
                .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
                .filter(challenges::Column::Id.eq(proof_id))
                .exec(&tx)
                .await?;
        }
        account_state::cancel_proofs_on(&tx, source.id).await?;
        account_state::cancel_proofs_on(&tx, target.id).await?;
        tx.execute_raw(Statement::from_sql_and_values(self.backend(),"UPDATE nuono_users SET username=NULL,username_key=NULL,email=NULL,email_key=NULL,email_verified_at_ms=NULL,password_hash=NULL,totp_secret=NULL,totp_enabled=FALSE,totp_recovery_json='[]',totp_last_counter=NULL,display_name='',birthday='',gender='',bio='',use_for_ai=FALSE,role='user',status='merged',merged_into_user_id=$2,auth_epoch=auth_epoch+1,revision=revision+1,updated_at_ms=$3 WHERE id=$1",[source.id.into(),target.id.into(),now.into()])).await?;
        users::orm::Entity::update_many()
            .col_expr(
                users::orm::Column::AuthEpoch,
                Expr::col(users::orm::Column::AuthEpoch).add(1),
            )
            .col_expr(
                users::orm::Column::Revision,
                Expr::col(users::orm::Column::Revision).add(1),
            )
            .col_expr(users::orm::Column::UpdatedAtMs, Expr::value(now))
            .filter(users::orm::Column::Id.eq(target.id))
            .exec(&tx)
            .await?;
        jobs::Entity::update_many()
            .col_expr(jobs::Column::Status, Expr::value("completed"))
            .col_expr(jobs::Column::CompletedAtMs, Expr::value(now))
            .col_expr(jobs::Column::UpdatedAtMs, Expr::value(now))
            .col_expr(
                jobs::Column::Revision,
                Expr::col(jobs::Column::Revision).add(1),
            )
            .filter(jobs::Column::Id.eq(id))
            .exec(&tx)
            .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(target.id),
                actor_plugin_id: None,
                action: "account.merge",
                result: "success",
                subject_user_id: Some(source.id),
                group_id: None,
                operation_id: Some(id),
                details: json!({"target_user_id":target.id.to_string(),"plan_digest":plan_digest}),
            },
        )
        .await?;
        let receipt = jobs::Entity::find_by_id(id)
            .one(&tx)
            .await?
            .ok_or_else(|| rejected("合并回执丢失"))?;
        tx.commit().await?;
        self.invalidate_identity_cache_for_user(source.id);
        self.invalidate_identity_cache_for_user(target.id);
        Ok(receipt.into())
    }

    /// 获取统一的账号合并引擎句柄（编排核心参与者、插件合并参与者和外部注册参与者）。
    pub fn account_merge_service(&self) -> AccountMergeService {
        AccountMergeService::new(self.clone())
    }
}

/// 账号合并执行选项。
#[derive(Clone, Copy, Debug, Default)]
pub struct MergeExecutionOptions {
    /// 是否同时吊销目标账号会话并递增其 `auth_epoch`（双正式账号合并为 `true`，分支 C 占位合并为 `false`）。
    pub revoke_target_sessions: bool,
    /// `account:{source}` 作用域 KV 冲突时是否严格拒绝（双正式账号双端确认合并为 `true`，占位合并自动按更新时间保留较新者）。
    pub strict_account_kv_conflicts: bool,
}

/// 统一合并引擎执行回执。
#[derive(Clone, Debug, Serialize)]
pub struct AccountMergeExecution {
    pub operation_id: String,
    pub source_user_id: i64,
    pub target_user_id: i64,
    pub steps: Vec<MergeStepSummary>,
}

/// 统一账号合并编排引擎：在单事务内按优先级串行调用核心与插件 `UserMergeParticipant`，
/// 并在提交后立即清除源账号与目标账号的身份解析 LRU 缓存。
#[derive(Clone)]
pub struct AccountMergeService {
    db: Database,
    extra_participants: Vec<std::sync::Arc<dyn UserMergeParticipant>>,
}

impl AccountMergeService {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            extra_participants: Vec::new(),
        }
    }

    /// 附加仅对本次引擎实例生效的合并参与者（常用于集成测试或独立宿主装配）。
    pub fn with_participant(
        mut self,
        participant: std::sync::Arc<dyn UserMergeParticipant>,
    ) -> Self {
        self.extra_participants.push(participant);
        self
    }

    /// 在独立写事务中原子合并 `source_user_id` 到 `target_user_id`，并在提交后立即失效 LRU 缓存。
    pub async fn merge(
        &self,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<AccountMergeExecution> {
        let tx = self.db.begin_write().await?;
        let operation_id = uuid::Uuid::new_v4().to_string();
        let execution = execute_unified_merge_on(
            &tx,
            source_user_id,
            target_user_id,
            &operation_id,
            &self.extra_participants,
            None,
        )
        .await?;
        tx.commit().await?;
        self.db
            .invalidate_identity_cache_for_user(execution.source_user_id);
        self.db
            .invalidate_identity_cache_for_user(execution.target_user_id);
        Ok(execution)
    }
}

struct CorePlatformIdentitiesParticipant;
#[async_trait::async_trait]
impl UserMergeParticipant for CorePlatformIdentitiesParticipant {
    fn participant_id(&self) -> &'static str {
        "core:platform_identities"
    }
    fn priority(&self) -> i32 {
        10
    }
    async fn merge_in_tx(
        &self,
        tx: &DatabaseTransaction,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<MergeStepSummary> {
        let now = crate::clock::db_now().timestamp_millis();
        let affected = tx
            .execute_raw(Statement::from_sql_and_values(
                tx.get_database_backend(),
                "UPDATE nuono_platform_identities SET user_id=$2,revision=revision+1,updated_at_ms=$3 WHERE user_id=$1",
                [source_user_id.into(), target_user_id.into(), now.into()],
            ))
            .await?
            .rows_affected();
        Ok(MergeStepSummary {
            participant_id: self.participant_id().to_owned(),
            migrated_rows: affected,
            deduplicated_rows: 0,
            details: json!({}),
        })
    }
}

struct CoreConversationsParticipant;
#[async_trait::async_trait]
impl UserMergeParticipant for CoreConversationsParticipant {
    fn participant_id(&self) -> &'static str {
        "core:conversations"
    }
    fn priority(&self) -> i32 {
        20
    }
    async fn merge_in_tx(
        &self,
        tx: &DatabaseTransaction,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<MergeStepSummary> {
        let backend = tx.get_database_backend();
        let now = crate::clock::db_now().timestamp_millis();
        move_scope_column(
            tx,
            "nuono_conversations",
            "native_chat_id",
            source_user_id,
            target_user_id,
        )
        .await?;
        let convs = tx
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE nuono_conversations SET owner_user_id=$2, updated_at_ms=$3 WHERE owner_user_id=$1",
                [source_user_id.into(), target_user_id.into(), now.into()],
            ))
            .await?
            .rows_affected();
        // 仅更新归属用户 ID，保留原 sender_identity_id 不可变平台身份快照。
        let msgs = tx
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE nuono_messages SET sender_user_id=$2 WHERE sender_user_id=$1",
                [source_user_id.into(), target_user_id.into()],
            ))
            .await?
            .rows_affected();
        let runs = tx
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE nuono_ai_runs SET user_id=$2 WHERE user_id=$1",
                [source_user_id.into(), target_user_id.into()],
            ))
            .await?
            .rows_affected();
        Ok(MergeStepSummary {
            participant_id: self.participant_id().to_owned(),
            migrated_rows: convs + msgs + runs,
            deduplicated_rows: 0,
            details: json!({
                "conversations": convs,
                "messages": msgs,
                "ai_runs": runs,
            }),
        })
    }
}

struct CoreEntitlementsParticipant;
#[async_trait::async_trait]
impl UserMergeParticipant for CoreEntitlementsParticipant {
    fn participant_id(&self) -> &'static str {
        "core:entitlements"
    }
    fn priority(&self) -> i32 {
        30
    }
    async fn merge_in_tx(
        &self,
        tx: &DatabaseTransaction,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<MergeStepSummary> {
        let now = crate::clock::db_now().timestamp_millis();
        let (ent_moved, ent_dedup) =
            merge_user_entitlements(tx, source_user_id, target_user_id, now).await?;
        let (auth_moved, auth_dedup) =
            merge_group_authorizations(tx, source_user_id, target_user_id, now).await?;
        let ov_moved = merge_plugin_overrides(tx, source_user_id, target_user_id, now).await?;
        Ok(MergeStepSummary {
            participant_id: self.participant_id().to_owned(),
            migrated_rows: ent_moved + auth_moved + ov_moved,
            deduplicated_rows: ent_dedup + auth_dedup,
            details: json!({
                "entitlements_migrated": ent_moved,
                "entitlements_deduplicated": ent_dedup,
                "group_authorizations_migrated": auth_moved,
                "group_authorizations_deduplicated": auth_dedup,
                "plugin_overrides_migrated": ov_moved,
            }),
        })
    }
}

struct CoreAssistantNotificationsParticipant {
    operation_id: String,
}
#[async_trait::async_trait]
impl UserMergeParticipant for CoreAssistantNotificationsParticipant {
    fn participant_id(&self) -> &'static str {
        "core:assistant_notifications"
    }
    fn priority(&self) -> i32 {
        40
    }
    async fn merge_in_tx(
        &self,
        tx: &DatabaseTransaction,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<MergeStepSummary> {
        let migrated = merge_assistant_and_notifications(
            tx,
            &self.operation_id,
            source_user_id,
            target_user_id,
        )
        .await?;
        Ok(MergeStepSummary {
            participant_id: self.participant_id().to_owned(),
            migrated_rows: migrated,
            deduplicated_rows: 0,
            details: json!({}),
        })
    }
}

struct CorePluginKvParticipant {
    strict_account_conflicts: bool,
}
#[async_trait::async_trait]
impl UserMergeParticipant for CorePluginKvParticipant {
    fn participant_id(&self) -> &'static str {
        "core:plugin_kv"
    }
    fn priority(&self) -> i32 {
        50
    }
    async fn merge_in_tx(
        &self,
        tx: &DatabaseTransaction,
        source_user_id: i64,
        target_user_id: i64,
    ) -> Result<MergeStepSummary> {
        let (moved_acc, dedup_acc) = merge_plugin_kv_scope(
            tx,
            &format!("account:{source_user_id}"),
            &format!("account:{target_user_id}"),
            self.strict_account_conflicts,
        )
        .await?;
        let (moved_usr, dedup_usr) = merge_plugin_kv_scope(
            tx,
            &format!("user:{source_user_id}"),
            &format!("user:{target_user_id}"),
            false,
        )
        .await?;
        Ok(MergeStepSummary {
            participant_id: self.participant_id().to_owned(),
            migrated_rows: moved_acc + moved_usr,
            deduplicated_rows: dedup_acc + dedup_usr,
            details: json!({
                "account_scope_migrated": moved_acc,
                "user_scope_migrated": moved_usr,
            }),
        })
    }
}

/// 分支 C 占位账号合并预检：仅当占位账号名下存在插件数据或 KV 时返回需确认的插件合并计划。
pub(crate) async fn shadow_merge_plan_on(
    tx: &DatabaseTransaction,
    shadow_user_id: i64,
    target_user_id: i64,
    operation_id: &str,
) -> Result<Value> {
    if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
        tx.query_all_raw(Statement::from_string(
            tx.get_database_backend(),
            "SELECT plugin_id FROM nuono_plugins ORDER BY plugin_id FOR UPDATE",
        ))
        .await?;
    }
    let context = AccountMergeContext {
        operation_id: operation_id.to_owned(),
        source_user_id: shadow_user_id,
        target_user_id,
    };
    let mut plugins = serde_json::Map::new();
    for plugin in crate::portable::registered_plugins(tx).await? {
        let mut affected = false;
        for reference in &plugin.manifest.account_references {
            if reference_count(tx, &reference.table, &reference.column, shadow_user_id).await? > 0 {
                if reference.merge_policy == AccountMergePolicy::Reject {
                    return Err(rejected(format!(
                        "插件 {} 的 {} 暂不支持账号合并",
                        plugin.plugin_id, reference.table
                    )));
                }
                affected = true;
            }
        }
        let kv = tx
            .query_one_raw(Statement::from_sql_and_values(
                tx.get_database_backend(),
                "SELECT COUNT(*) AS n FROM nuono_plugin_kv WHERE plugin_id=$1 AND scope_key IN ($2,$3) AND (expires_at_ms IS NULL OR expires_at_ms>$4)",
                [
                    plugin.plugin_id.clone().into(),
                    format!("account:{shadow_user_id}").into(),
                    format!("user:{shadow_user_id}").into(),
                    crate::clock::db_now().timestamp_millis().into(),
                ],
            ))
            .await?
            .ok_or_else(|| rejected("无法读取插件账号状态"))?
            .try_get::<i64>("", "n")?;
        if !affected && kv == 0 {
            continue;
        }
        if affected {
            let participant = account_merge_participant(&plugin.plugin_id)?
                .filter(|_| plugin.installed)
                .ok_or_else(|| {
                    rejected(format!(
                        "插件 {} 存有占位账号数据，须安装支持合并的插件版本",
                        plugin.plugin_id
                    ))
                })?;
            if plugin.manifest.merge_participant_version != Some(participant.version()) {
                return Err(rejected(format!(
                    "插件 {} 合并实现与登记版本不一致",
                    plugin.plugin_id
                )));
            }
            let port = ParticipantTransaction {
                tx,
                tables: crate::portable::manifest_tables(&plugin.manifest)?,
                plugin_id: plugin.plugin_id.clone(),
                source: context.source_user_id,
                target: context.target_user_id,
            };
            let plan = participant.preflight(&context, &port).await?;
            nuono_domain::scalar::parse_json(&plan.to_string())?;
            plugins.insert(
                plugin.plugin_id,
                json!({
                    "participant_version": participant.version().to_string(),
                    "plan": plan,
                }),
            );
        }
    }
    Ok(Value::Object(plugins))
}

pub(crate) fn shadow_merge_plan_digest(plan: &Value) -> Result<String> {
    Ok(digest(&serde_json::to_vec(plan)?))
}

/// 分支 C 事务内合并入口：在外部已开启的绑定写事务中将 `shadow` 占位账号原子并入已登录正式账号，
/// 保留目标账号现有 Web 登录会话。调用方须在提交后立即清除两者的身份 LRU 缓存。
pub(crate) async fn merge_shadow_into_user_on(
    tx: &DatabaseTransaction,
    shadow_user_id: i64,
    target_user_id: i64,
    operation_id: &str,
    precomputed_plugin_plan: Option<&Value>,
) -> Result<AccountMergeExecution> {
    execute_unified_merge_on(
        tx,
        shadow_user_id,
        target_user_id,
        operation_id,
        &[],
        precomputed_plugin_plan,
    )
    .await
}

async fn execute_unified_merge_on(
    tx: &DatabaseTransaction,
    raw_source_id: i64,
    raw_target_id: i64,
    operation_id: &str,
    extra_participants: &[std::sync::Arc<dyn UserMergeParticipant>],
    precomputed_plugin_plan: Option<&Value>,
) -> Result<AccountMergeExecution> {
    let source_id =
        crate::identity_resolver::resolve_canonical_user_id_on(tx, raw_source_id).await?;
    let target_id =
        crate::identity_resolver::resolve_canonical_user_id_on(tx, raw_target_id).await?;
    if source_id == target_id {
        return Err(rejected("不能合并同一个账号"));
    }
    // 始终按账号 ID 升序加排他锁，消除 A->B 与 B->A 并发合并死锁。
    let (source, target) = lock_pair(tx, source_id, target_id).await?;
    if source.status != "active" || target.status != "active" {
        return Err(rejected("源账号或目标账号已失效或已合并"));
    }
    if (source.role == "owner" || target.role == "owner")
        && source.account_kind == users::ACCOUNT_KIND_REGISTERED
    {
        return Err(rejected("owner 账号须先单独完成角色转移，再进行账号合并"));
    }
    let now = crate::clock::db_now().timestamp_millis();
    let job_id = if uuid::Uuid::parse_str(operation_id).is_ok() {
        operation_id.to_owned()
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    let existing_job = jobs::Entity::find_by_id(&job_id).one(tx).await?;
    if existing_job.is_none() {
        jobs::ActiveModel {
            id: Set(job_id.clone()),
            source_user_id: Set(source.id),
            target_user_id: Set(target.id),
            source_proof_id: Set(None),
            target_proof_id: Set(None),
            status: Set("running".into()),
            plan_json: Set("{}".into()),
            failure_code: Set(None),
            completed_at_ms: Set(None),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
        }
        .insert(tx)
        .await?;
    }
    let context = AccountMergeContext {
        operation_id: job_id.clone(),
        source_user_id: source.id,
        target_user_id: target.id,
    };
    let plugin_plan = match precomputed_plugin_plan {
        Some(plan) => plan.clone(),
        None => shadow_merge_plan_on(tx, source.id, target.id, &job_id).await?,
    };
    let mut steps = Vec::new();
    // 1. 按固定优先级执行核心子系统合并参与者。
    let core_participants: Vec<Box<dyn UserMergeParticipant>> = vec![
        Box::new(CorePlatformIdentitiesParticipant),
        Box::new(CoreConversationsParticipant),
        Box::new(CoreEntitlementsParticipant),
        Box::new(CoreAssistantNotificationsParticipant {
            operation_id: job_id.clone(),
        }),
    ];
    for participant in core_participants {
        steps.push(participant.merge_in_tx(tx, source.id, target.id).await?);
    }
    // 2. 执行已声明存储清单的插件 AccountMergeParticipant（先于插件 KV 清理，避免插件仍需读取 account:{source} KV）。
    for plugin in crate::portable::registered_plugins(tx).await? {
        let Some(item) = plugin_plan.get(&plugin.plugin_id) else {
            continue;
        };
        let participant = account_merge_participant(&plugin.plugin_id)?
            .ok_or_else(|| rejected("插件合并参与者已卸载"))?;
        let port = ParticipantTransaction {
            tx,
            tables: crate::portable::manifest_tables(&plugin.manifest)?,
            plugin_id: plugin.plugin_id.clone(),
            source: source.id,
            target: target.id,
        };
        let retained = retained_plugin_references(tx, &plugin.manifest, source.id).await?;
        participant.apply(&context, &item["plan"], &port).await?;
        if retained_plugin_references(tx, &plugin.manifest, source.id).await? != retained {
            return Err(rejected(format!(
                "插件 {} 修改了声明保留的历史账号归属",
                plugin.plugin_id
            )));
        }
        for reference in &plugin.manifest.account_references {
            if reference.merge_policy == AccountMergePolicy::FollowAccount
                && reference_count(tx, &reference.table, &reference.column, source.id).await? > 0
            {
                return Err(rejected(format!(
                    "插件 {} 未完成账号数据转移",
                    plugin.plugin_id
                )));
            }
        }
        steps.push(MergeStepSummary {
            participant_id: format!("plugin:{}", plugin.plugin_id),
            migrated_rows: 1,
            deduplicated_rows: 0,
            details: item["plan"].clone(),
        });
    }
    // 3. 迁移核心剩余的插件 KV（兼容 account:{source} 与 user:{source}）。
    steps.push(
        CorePluginKvParticipant {
            strict_account_conflicts: false,
        }
        .merge_in_tx(tx, source.id, target.id)
        .await?,
    );
    // 4. 执行通过 UserMergeParticipant 全局注册或本地挂载的外部参与者（按 priority 升序）。
    let mut external = registered_user_merge_participants()?;
    external.extend(extra_participants.iter().cloned());
    external.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then_with(|| a.participant_id().cmp(b.participant_id()))
    });
    for participant in external {
        steps.push(participant.merge_in_tx(tx, source.id, target.id).await?);
    }
    for plugin in crate::portable::registered_plugins(tx).await? {
        crate::plugin_global_settings::purge_unused_plugin_secrets_on(tx, &plugin.plugin_id)
            .await?;
    }
    // 5. 吊销源账号会话；若源账号为正式账号（非占位账号），同时吊销目标账号会话强制重登。
    account_state::revoke_sessions_on(tx, source.id).await?;
    account_state::cancel_proofs_on(tx, source.id).await?;
    let revoke_target = source.account_kind == users::ACCOUNT_KIND_REGISTERED;
    if revoke_target {
        account_state::revoke_sessions_on(tx, target.id).await?;
        account_state::cancel_proofs_on(tx, target.id).await?;
    }
    let finished = crate::clock::db_now().timestamp_millis();
    // 6. 将源账号置为 merged 墓碑态并释放所有唯一凭据键。
    tx.execute_raw(Statement::from_sql_and_values(
        tx.get_database_backend(),
        "UPDATE nuono_users SET username=NULL,username_key=NULL,email=NULL,email_key=NULL,email_verified_at_ms=NULL,password_hash=NULL,totp_secret=NULL,totp_enabled=FALSE,totp_recovery_json='[]',totp_last_counter=NULL,display_name='',birthday='',gender='',bio='',use_for_ai=FALSE,role='user',status='merged',merged_into_user_id=$2,auth_epoch=auth_epoch+1,revision=revision+1,updated_at_ms=$3 WHERE id=$1",
        [source.id.into(), target.id.into(), finished.into()],
    ))
    .await?;
    let mut target_update = users::orm::Entity::update_many()
        .col_expr(
            users::orm::Column::Revision,
            Expr::col(users::orm::Column::Revision).add(1),
        )
        .col_expr(users::orm::Column::UpdatedAtMs, Expr::value(finished))
        .filter(users::orm::Column::Id.eq(target.id));
    if revoke_target {
        target_update = target_update.col_expr(
            users::orm::Column::AuthEpoch,
            Expr::col(users::orm::Column::AuthEpoch).add(1),
        );
    }
    target_update.exec(tx).await?;
    let summary_json = json!({
        "source_account_kind": source.account_kind,
        "steps": &steps,
    })
    .to_string();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::Status, Expr::value("completed"))
        .col_expr(jobs::Column::PlanJson, Expr::value(summary_json))
        .col_expr(jobs::Column::CompletedAtMs, Expr::value(finished))
        .col_expr(jobs::Column::UpdatedAtMs, Expr::value(finished))
        .col_expr(
            jobs::Column::Revision,
            Expr::col(jobs::Column::Revision).add(1),
        )
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(tx)
        .await?;
    append_audit_on(
        tx,
        AuditEvent {
            actor_user_id: Some(target.id),
            actor_plugin_id: None,
            action: "account.merge",
            result: "success",
            subject_user_id: Some(source.id),
            group_id: None,
            operation_id: Some(&job_id),
            details: json!({
                "target_user_id": target.id.to_string(),
                "source_account_kind": source.account_kind,
                "steps": steps.len(),
            }),
        },
    )
    .await?;
    Ok(AccountMergeExecution {
        operation_id: job_id,
        source_user_id: source.id,
        target_user_id: target.id,
        steps,
    })
}
async fn lock_pair(
    tx: &DatabaseTransaction,
    source: i64,
    target: i64,
) -> Result<(users::orm::Model, users::orm::Model)> {
    async fn one(tx: &DatabaseTransaction, id: i64) -> Result<users::orm::Model> {
        let mut query = users::orm::Entity::find_by_id(id);
        if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        query.one(tx).await?.ok_or_else(|| rejected("账号不存在"))
    }
    let low = one(tx, std::cmp::min(source, target)).await?;
    let high = one(tx, std::cmp::max(source, target)).await?;
    Ok(if source < target {
        (low, high)
    } else {
        (high, low)
    })
}

async fn reference_count(
    tx: &DatabaseTransaction,
    table: &str,
    column: &str,
    user: i64,
) -> Result<i64> {
    let row = tx
        .query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            format!("SELECT COUNT(*) AS n FROM \"{table}\" WHERE \"{column}\"=$1"),
            [user.into()],
        ))
        .await?
        .ok_or_else(|| rejected("无法读取合并计数"))?;
    Ok(row.try_get("", "n")?)
}
async fn retained_plugin_references(
    tx: &DatabaseTransaction,
    manifest: &nuono_app::plugin_manifest::PortableManifest,
    source: i64,
) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    if !manifest
        .account_references
        .iter()
        .any(|reference| reference.merge_policy == AccountMergePolicy::RetainHistory)
    {
        return Ok(result);
    }
    let schema = crate::portable::Schema::new(vec![crate::portable::Migration {
        name: "account-merge-history".into(),
        postgres_sql: manifest.postgres_schema.clone(),
        sqlite_sql: manifest.sqlite_schema.clone(),
    }])?;
    for reference in manifest
        .account_references
        .iter()
        .filter(|reference| reference.merge_policy == AccountMergePolicy::RetainHistory)
    {
        let mut selected = schema.clone();
        let table = selected
            .tables
            .get_mut(&reference.table)
            .ok_or_else(|| rejected("历史账号引用表未登记"))?;
        table.columns.retain(|column| {
            table.primary_key.contains(&column.name) || column.name == reference.column
        });
        result.insert(
            format!("{}.{}", reference.table, reference.column),
            snapshot_declared(
                tx,
                &selected,
                &reference.table,
                &format!("\"{}\"=$1", reference.column),
                vec![source.into()],
            )
            .await?,
        );
    }
    Ok(result)
}
fn scope_filter(column: &str, source: i64, target: i64) -> (String, Vec<sea_orm::Value>) {
    let prefix = format!("account-chat:v1:{}:", "_".repeat(64));
    (
        format!(
            "(\"{column}\" LIKE $1 OR \"{column}\" LIKE $2 OR \"{column}\" LIKE $3 OR \"{column}\" LIKE $4)"
        ),
        vec![
            format!("{prefix}{source}").into(),
            format!("{prefix}{source}:%").into(),
            format!("{prefix}{target}").into(),
            format!("{prefix}{target}:%").into(),
        ],
    )
}
fn moved_scope(scope: &str, source: i64, target: i64) -> Option<String> {
    if nuono_app::agent::account_chat_owner(scope) != Some(source) {
        return None;
    }
    let tail = scope.strip_prefix(nuono_app::agent::ACCOUNT_CHAT_PREFIX)?;
    let (namespace, rest) = tail.split_once(':')?;
    let suffix = rest
        .split_once(':')
        .map(|(_, tail)| format!(":{tail}"))
        .unwrap_or_default();
    Some(format!(
        "{}{namespace}:{target}{suffix}",
        nuono_app::agent::ACCOUNT_CHAT_PREFIX
    ))
}
pub(crate) async fn snapshot_declared(
    tx: &DatabaseTransaction,
    schema: &crate::portable::Schema,
    table: &str,
    filter: &str,
    values: Vec<sea_orm::Value>,
) -> Result<Value> {
    let declaration = schema
        .tables
        .get(table)
        .ok_or_else(|| rejected(format!("合并表未登记：{table}")))?;
    let projection = declaration
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            if column.kind == crate::portable::LogicalType::Binary {
                if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
                    format!("encode(\"{}\", 'hex') AS c{index}", column.name)
                } else {
                    format!(
                        "CASE WHEN \"{0}\" IS NULL THEN NULL ELSE hex(\"{0}\") END AS c{index}",
                        column.name
                    )
                }
            } else {
                format!("CAST(\"{}\" AS TEXT) AS c{index}", column.name)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let rows = tx
        .query_all_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            format!("SELECT {projection} FROM \"{table}\" WHERE {filter}"),
            values,
        ))
        .await?;
    let mut cells = Vec::new();
    for row in rows {
        cells.push(
            (0..declaration.columns.len())
                .map(|index| row.try_get::<Option<String>>("", &format!("c{index}")))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
    }
    cells.sort();
    Ok(json!({"rows":cells.len().to_string(),"fingerprint":digest(&serde_json::to_vec(&cells)?)}))
}
/// 校验框架内所有指向 `nuono_users` 的外键是否都有显式的归属迁移策略；若新增模块未声明合并策略则默认拒绝（fail-closed）。
async fn verify_core_account_coverage(
    tx: &DatabaseTransaction,
    source: i64,
    schema: &crate::portable::Schema,
) -> Result<()> {
    let mut references = Vec::<(String, String)>::new();
    if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
        let rows=tx.query_all_raw(Statement::from_string(tx.get_database_backend(),"SELECT tc.table_name,kcu.column_name FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage kcu ON tc.constraint_name=kcu.constraint_name AND tc.constraint_schema=kcu.constraint_schema JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name=tc.constraint_name AND ccu.constraint_schema=tc.constraint_schema WHERE tc.constraint_type='FOREIGN KEY' AND tc.table_schema='public' AND ccu.table_name='nuono_users' ORDER BY tc.table_name,kcu.column_name")).await?;
        for row in rows {
            references.push((
                row.try_get("", "table_name")?,
                row.try_get("", "column_name")?,
            ));
        }
    } else {
        for table in schema.tables.keys() {
            for row in tx
                .query_all_raw(Statement::from_string(
                    tx.get_database_backend(),
                    format!("PRAGMA foreign_key_list(\"{table}\")"),
                ))
                .await?
            {
                if row.try_get::<String>("", "table")? == "nuono_users" {
                    references.push((table.clone(), row.try_get("", "from")?));
                }
            }
        }
    }
    for (table, column) in references {
        if !table.starts_with("nuono_") {
            continue;
        }
        let handled = matches!(
            (table.as_str(), column.as_str()),
            ("nuono_users", "merged_into_user_id")
                | ("nuono_platform_identities", "user_id")
                | ("nuono_plugin_overrides", "user_id")
                | ("nuono_group_authorizations", "user_id" | "issuer_user_id")
                | ("nuono_ai_personas", "owner_user_id")
                | (
                    "nuono_account_assistant" | "nuono_assistant_turns" | "nuono_notifications",
                    "user_id"
                )
                | ("nuono_sessions", "user_id")
                | (
                    "nuono_account_challenges",
                    "requester_user_id" | "subject_user_id"
                )
                | (
                    "nuono_account_merge_jobs",
                    "source_user_id" | "target_user_id"
                )
                | ("nuono_conversations", "owner_user_id")
                | ("nuono_messages", "sender_user_id")
                | ("nuono_ai_runs", "user_id")
                | ("nuono_audit_events", "actor_user_id" | "subject_user_id")
        );
        if !handled && reference_count(tx, &table, &column, source).await? > 0 {
            return Err(rejected(format!(
                "框架模块 {table}.{column} 尚未声明安全的账号合并处理"
            )));
        }
    }
    Ok(())
}

async fn merge_plan(
    tx: &DatabaseTransaction,
    context: &AccountMergeContext,
    source: &users::orm::Model,
    target: &users::orm::Model,
) -> Result<Value> {
    if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
        tx.query_all_raw(Statement::from_string(
            tx.get_database_backend(),
            "SELECT plugin_id FROM nuono_plugins ORDER BY plugin_id FOR UPDATE",
        ))
        .await?;
    }
    let ai_installed = super::migrations::is_ai_module_installed(tx).await?;
    let modules = if ai_installed {
        vec!["ai".to_string()]
    } else {
        Vec::new()
    };
    let schema = crate::portable::Schema::framework_with_modules(&modules, vec![])?;
    verify_core_account_coverage(tx, source.id, &schema).await?;
    let mut core = BTreeMap::new();
    core.insert(
        "nuono_users".to_owned(),
        snapshot_declared(
            tx,
            &schema,
            "nuono_users",
            "id IN ($1,$2)",
            vec![source.id.into(), target.id.into()],
        )
        .await?,
    );
    for table in [
        "nuono_platform_identities",
        "nuono_plugin_overrides",
        "nuono_group_authorizations",
        "nuono_notifications",
    ] {
        core.insert(
            table.to_owned(),
            snapshot_declared(
                tx,
                &schema,
                table,
                "user_id IN ($1,$2)",
                vec![source.id.into(), target.id.into()],
            )
            .await?,
        );
    }
    {
        let (scope_sql, mut values) = scope_filter("native_chat_id", source.id, target.id);
        values.push(source.id.into());
        values.push(target.id.into());
        core.insert(
            "nuono_conversations".to_owned(),
            snapshot_declared(
                tx,
                &schema,
                "nuono_conversations",
                &format!("owner_user_id IN ($5,$6) OR {scope_sql}"),
                values,
            )
            .await?,
        );
    }
    core.insert(
        "nuono_messages".to_owned(),
        snapshot_declared(
            tx,
            &schema,
            "nuono_messages",
            "sender_user_id IN ($1,$2)",
            vec![source.id.into(), target.id.into()],
        )
        .await?,
    );
    core.insert(
        "nuono_ai_runs".to_owned(),
        snapshot_declared(
            tx,
            &schema,
            "nuono_ai_runs",
            "user_id IN ($1,$2)",
            vec![source.id.into(), target.id.into()],
        )
        .await?,
    );
    core.insert(
        "nuono_entitlements".to_owned(),
        snapshot_declared(
            tx,
            &schema,
            "nuono_entitlements",
            "subject_type = 'user' AND subject_id IN ($1,$2)",
            vec![source.id.into(), target.id.into()],
        )
        .await?,
    );
    if ai_installed {
        for table in ["nuono_account_assistant", "nuono_assistant_turns"] {
            core.insert(
                table.to_owned(),
                snapshot_declared(
                    tx,
                    &schema,
                    table,
                    "user_id IN ($1,$2)",
                    vec![source.id.into(), target.id.into()],
                )
                .await?,
            );
        }
        core.insert(
            "nuono_ai_personas".to_owned(),
            snapshot_declared(
                tx,
                &schema,
                "nuono_ai_personas",
                "owner_user_id IN ($1,$2)",
                vec![source.id.into(), target.id.into()],
            )
            .await?,
        );
        for (table, column) in [
            ("nuono_agent_memories", "scope_key"),
            ("nuono_agent_memory_graphs", "scope_key"),
            ("nuono_companion_memory_sources", "scope_key"),
        ] {
            let (filter, values) = scope_filter(column, source.id, target.id);
            core.insert(
                table.into(),
                snapshot_declared(tx, &schema, table, &filter, values).await?,
            );
        }
        let busy=tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),"SELECT COUNT(*) AS n FROM nuono_assistant_turns WHERE user_id IN ($1,$2) AND (status='running' OR delivery='pending')",[source.id.into(),target.id.into()])).await?.ok_or_else(||rejected("无法读取正在进行的对话"))?.try_get::<i64>("","n")?;
        if busy > 0 {
            return Err(rejected(
                "请等待两个账号正在进行的对话和消息发送结束后再合并",
            ));
        }
        // 历史记录中的操作者 ID 与检查点内容保持不变，仅将私聊清理作用域（session_id）迁移到目标账号；
        // 任何尚未结束或待审批的可恢复 Agent 任务必须在合并前完成或取消。
        for table in [
            "nuono_agent_runs",
            "nuono_agent_tool_approvals",
            "nuono_agent_tool_receipts",
        ] {
            let (filter, values) = scope_filter("session_id", source.id, target.id);
            core.insert(
                table.into(),
                snapshot_declared(tx, &schema, table, &filter, values.clone()).await?,
            );
            let active = match table {
                "nuono_agent_runs" => {
                    Some("status IN ('queued','running','waiting_approval','waiting_children')")
                }
                "nuono_agent_tool_approvals" => {
                    Some("status IN ('pending','approved','executing')")
                }
                _ => None,
            };
            if let Some(active) = active {
                let count = tx
                    .query_one_raw(Statement::from_sql_and_values(
                        tx.get_database_backend(),
                        format!("SELECT COUNT(*) AS n FROM {table} WHERE ({filter}) AND {active}"),
                        values,
                    ))
                    .await?
                    .ok_or_else(|| rejected("无法核对私人任务状态"))?
                    .try_get::<i64>("", "n")?;
                if count > 0 {
                    return Err(rejected(
                        "请先完成或取消两个账号尚未结束的私人 Agent 任务及审批",
                    ));
                }
            }
        }
    }
    core.insert(
        "nuono_plugin_kv".into(),
        snapshot_declared(
            tx,
            &schema,
            "nuono_plugin_kv",
            "scope_key IN ($1,$2) AND (expires_at_ms IS NULL OR expires_at_ms>$3)",
            vec![
                format!("account:{}", source.id).into(),
                format!("account:{}", target.id).into(),
                crate::clock::db_now().timestamp_millis().into(),
            ],
        )
        .await?,
    );
    let mut configuration_conflicts = Vec::new();
    for old in plugin_overrides::Entity::find()
        .filter(plugin_overrides::Column::UserId.eq(source.id))
        .all(tx)
        .await?
    {
        if let Some(retained) = plugin_overrides::Entity::find()
            .filter(plugin_overrides::Column::UserId.eq(target.id))
            .filter(plugin_overrides::Column::PluginId.eq(&old.plugin_id))
            .one(tx)
            .await?
        {
            let mut fields = Vec::new();
            for (kind, a, b) in [
                (
                    "configuration",
                    &old.config_patch_json,
                    &retained.config_patch_json,
                ),
                (
                    "services",
                    &old.service_patch_json,
                    &retained.service_patch_json,
                ),
            ] {
                let old: Value = serde_json::from_str(a)?;
                let new: Value = serde_json::from_str(b)?;
                for (key, value) in old
                    .as_object()
                    .ok_or_else(|| rejected("个人配置不是对象"))?
                {
                    if new.get(key).is_some_and(|v| v != value) {
                        fields.push(format!("{kind}.{key}"));
                    }
                }
            }
            if old.enabled.is_some()
                && retained.enabled.is_some()
                && old.enabled != retained.enabled
            {
                fields.push("enabled".into());
            }
            if !fields.is_empty() {
                configuration_conflicts.push(
                    json!({"plugin_id":old.plugin_id,"fields":fields,"resolution":"keep_target"}),
                );
            }
        }
    }
    core.insert(
        "account_avatar".into(),
        snapshot_declared(
            tx,
            &schema,
            "nuono_global_configs",
            "key IN ($1,$2)",
            vec![
                format!("admin_profile:{}", source.id).into(),
                format!("admin_profile:{}", target.id).into(),
            ],
        )
        .await?,
    );
    let mut plugins = serde_json::Map::new();
    for plugin in crate::portable::registered_plugins(tx).await? {
        let mut affected = false;
        for reference in &plugin.manifest.account_references {
            if reference_count(tx, &reference.table, &reference.column, source.id).await? > 0 {
                if reference.merge_policy == AccountMergePolicy::Reject {
                    return Err(rejected(format!(
                        "插件 {} 的 {} 暂不支持账号合并",
                        plugin.plugin_id, reference.table
                    )));
                }
                affected = true;
            }
        }
        let kv = tx
            .query_one_raw(Statement::from_sql_and_values(
                tx.get_database_backend(),
                "SELECT COUNT(*) AS n FROM nuono_plugin_kv WHERE plugin_id=$1 AND scope_key=$2 AND (expires_at_ms IS NULL OR expires_at_ms>$3)",
                [
                    plugin.plugin_id.clone().into(),
                    format!("account:{}", source.id).into(),
                    crate::clock::db_now().timestamp_millis().into(),
                ],
            ))
            .await?
            .ok_or_else(|| rejected("无法读取插件账号状态"))?
            .try_get::<i64>("", "n")?;
        if !affected && kv == 0 {
            continue;
        }
        let participant = account_merge_participant(&plugin.plugin_id)?
            .filter(|_| plugin.installed)
            .ok_or_else(|| {
                rejected(format!(
                    "插件 {} 存有账号数据，须安装支持合并的插件版本",
                    plugin.plugin_id
                ))
            })?;
        if plugin.manifest.merge_participant_version != Some(participant.version()) {
            return Err(rejected(format!(
                "插件 {} 合并实现与登记版本不一致",
                plugin.plugin_id
            )));
        }
        let port = ParticipantTransaction {
            tx,
            tables: crate::portable::manifest_tables(&plugin.manifest)?,
            plugin_id: plugin.plugin_id.clone(),
            source: context.source_user_id,
            target: context.target_user_id,
        };
        let plan = participant.preflight(context, &port).await?;
        nuono_domain::scalar::parse_json(&plan.to_string())?;
        let mut snapshots = BTreeMap::new();
        if !port.tables.is_empty() {
            let schema = crate::portable::Schema::new(vec![crate::portable::Migration {
                name: plugin.plugin_id.clone(),
                postgres_sql: plugin.manifest.postgres_schema.clone(),
                sqlite_sql: plugin.manifest.sqlite_schema.clone(),
            }])?;
            for table in &port.tables {
                let predicates = plugin
                    .manifest
                    .account_references
                    .iter()
                    .filter(|reference| &reference.table == table)
                    .map(|reference| format!("\"{}\" IN ($1,$2)", reference.column))
                    .collect::<Vec<_>>();
                if !predicates.is_empty() {
                    snapshots.insert(
                        table.clone(),
                        snapshot_declared(
                            tx,
                            &schema,
                            table,
                            &predicates.join(" OR "),
                            vec![source.id.into(), target.id.into()],
                        )
                        .await?,
                    );
                }
            }
        }
        plugins.insert(plugin.plugin_id,json!({"storage_version":plugin.version.map(|version|version.to_string()),"participant_version":participant.version().to_string(),"code_version":plugin.code_version,"snapshots":snapshots,"plan":plan}));
    }
    // 没有登记存储声明的插件账号状态不得在预检中被静默忽略。
    let unregistered=tx.query_one_raw(Statement::from_sql_and_values(tx.get_database_backend(),"SELECT COUNT(*) AS n FROM nuono_plugin_kv k WHERE k.scope_key=$1 AND (k.expires_at_ms IS NULL OR k.expires_at_ms>$2) AND NOT EXISTS (SELECT 1 FROM nuono_plugins r WHERE r.plugin_id=k.plugin_id AND r.table_prefix IS NOT NULL)",[format!("account:{}",source.id).into(),crate::clock::db_now().timestamp_millis().into()])).await?.ok_or_else(||rejected("无法核对插件登记"))?.try_get::<i64>("","n")?;
    if unregistered > 0 {
        return Err(rejected("存在未登记的插件账号状态，无法确认合并边界"));
    }
    Ok(
        json!({"format":"1","source_revision":source.revision.to_string(),"target_revision":target.revision.to_string(),"source_auth_epoch":source.auth_epoch.to_string(),"target_auth_epoch":target.auth_epoch.to_string(),"core":core,"plugins":plugins,"configuration_conflicts":configuration_conflicts,"rules":{"profile":"keep_target","roles":"keep_target","personal_configuration":"target_explicit_fields_win","groups":"unchanged","authorizations":"preserve_sources","history":"preserve_tombstones","agent_execution_history":"move_cleanup_scope_keep_actor","sessions":"revoke_both"}}),
    )
}
fn overlay(source: &str, target: &str) -> Result<String> {
    let source = nuono_domain::scalar::parse_json(source)?;
    let target = nuono_domain::scalar::parse_json(target)?;
    let mut combined = source
        .as_object()
        .ok_or_else(|| rejected("个人配置不是对象"))?
        .clone();
    for (key, value) in target
        .as_object()
        .ok_or_else(|| rejected("个人配置不是对象"))?
    {
        combined.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(combined).to_string())
}
async fn move_scope_column(
    tx: &DatabaseTransaction,
    table: &str,
    column: &str,
    source: i64,
    target: i64,
) -> Result<()> {
    let (filter, values) = scope_filter(column, source, source);
    let rows = tx
        .query_all_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            format!("SELECT DISTINCT \"{column}\" AS scope FROM \"{table}\" WHERE {filter}"),
            values,
        ))
        .await?;
    for row in rows {
        let old: String = row.try_get("", "scope")?;
        if let Some(new) = moved_scope(&old, source, target) {
            tx.execute_raw(Statement::from_sql_and_values(
                tx.get_database_backend(),
                format!("UPDATE \"{table}\" SET \"{column}\"=$2 WHERE \"{column}\"=$1"),
                [old.into(), new.into()],
            ))
            .await?;
        }
    }
    Ok(())
}
async fn merge_core(tx: &DatabaseTransaction, context: &AccountMergeContext) -> Result<()> {
    let source = context.source_user_id;
    let target = context.target_user_id;
    CorePlatformIdentitiesParticipant
        .merge_in_tx(tx, source, target)
        .await?;
    CoreConversationsParticipant
        .merge_in_tx(tx, source, target)
        .await?;
    CoreEntitlementsParticipant
        .merge_in_tx(tx, source, target)
        .await?;
    CoreAssistantNotificationsParticipant {
        operation_id: context.operation_id.clone(),
    }
    .merge_in_tx(tx, source, target)
    .await?;
    Ok(())
}

async fn merge_plugin_overrides(
    tx: &DatabaseTransaction,
    source: i64,
    target: i64,
    now: i64,
) -> Result<u64> {
    let mut moved = 0_u64;
    for patch in plugin_overrides::Entity::find()
        .filter(plugin_overrides::Column::UserId.eq(source))
        .order_by_asc(plugin_overrides::Column::Id)
        .all(tx)
        .await?
    {
        moved += 1;
        if let Some(retained) = plugin_overrides::Entity::find()
            .filter(plugin_overrides::Column::UserId.eq(target))
            .filter(plugin_overrides::Column::PluginId.eq(&patch.plugin_id))
            .one(tx)
            .await?
        {
            plugin_overrides::Entity::update_many()
                .col_expr(
                    plugin_overrides::Column::ConfigPatchJson,
                    Expr::value(overlay(
                        &patch.config_patch_json,
                        &retained.config_patch_json,
                    )?),
                )
                .col_expr(
                    plugin_overrides::Column::ServicePatchJson,
                    Expr::value(overlay(
                        &patch.service_patch_json,
                        &retained.service_patch_json,
                    )?),
                )
                .col_expr(
                    plugin_overrides::Column::Enabled,
                    Expr::value(retained.enabled.or(patch.enabled)),
                )
                .col_expr(
                    plugin_overrides::Column::Revision,
                    Expr::col(plugin_overrides::Column::Revision).add(1),
                )
                .col_expr(plugin_overrides::Column::UpdatedAtMs, Expr::value(now))
                .filter(plugin_overrides::Column::Id.eq(retained.id))
                .exec(tx)
                .await?;
            plugin_overrides::Entity::delete_by_id(patch.id)
                .exec(tx)
                .await?;
        } else {
            plugin_overrides::Entity::update_many()
                .col_expr(plugin_overrides::Column::UserId, Expr::value(target))
                .col_expr(
                    plugin_overrides::Column::Revision,
                    Expr::col(plugin_overrides::Column::Revision).add(1),
                )
                .col_expr(plugin_overrides::Column::UpdatedAtMs, Expr::value(now))
                .filter(plugin_overrides::Column::Id.eq(patch.id))
                .exec(tx)
                .await?;
        }
    }
    Ok(moved)
}

async fn merge_assistant_and_notifications(
    tx: &DatabaseTransaction,
    operation_id: &str,
    source: i64,
    target: i64,
) -> Result<u64> {
    let backend = tx.get_database_backend();
    // 核心账号资料保留目标账号的设置；源账号的登录凭据与个人资料在合并时显式清理释放。
    tx.execute_raw(Statement::from_sql_and_values(
        backend,
        "DELETE FROM nuono_global_configs WHERE key=$1",
        [format!("admin_profile:{source}").into()],
    ))
    .await?;
    let mut moved = 0_u64;
    let ai_installed = super::migrations::is_ai_module_installed(tx).await?;
    if ai_installed {
        // 磁盘上的 UUID 资源文件保持原有不可变文件名不动，仅在数据库内变更归属账号。
        // 历史对话轮次保留原 ID，但给源账号的 request_key 加上合并命名空间前缀，防止与目标账号的旧幂等键冲突。
        tx.execute_raw(Statement::from_sql_and_values(
            backend,
            "UPDATE nuono_assistant_turns SET request_key=$2 || request_key WHERE user_id=$1",
            [source.into(), format!("merged:{operation_id}:").into()],
        ))
        .await?;
        // 两个账号下同名的账号级共享记忆全部保留：目标账号维持原 memory_key，源账号冲突项重命名后并入。
        let (filter, values) = scope_filter("scope_key", source, source);
        let memories = tx
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                format!(
                    "SELECT id,scope_key,memory_key,scope_type FROM nuono_agent_memories WHERE {filter}"
                ),
                values,
            ))
            .await?;
        for memory in memories {
            let old: String = memory.try_get("", "scope_key")?;
            if let Some(new) = moved_scope(&old, source, target) {
                let key: String = memory.try_get("", "memory_key")?;
                let kind: String = memory.try_get("", "scope_type")?;
                let collision=tx.query_one_raw(Statement::from_sql_and_values(backend,"SELECT id FROM nuono_agent_memories WHERE scope_key=$1 AND scope_type=$2 AND memory_key=$3",[new.clone().into(),kind.into(),key.into()])).await?.is_some();
                if collision {
                    let id: i64 = memory.try_get("", "id")?;
                    tx.execute_raw(Statement::from_sql_and_values(
                        backend,
                        "UPDATE nuono_agent_memories SET memory_key=$2 WHERE id=$1",
                        [id.into(), format!("merged:{source}:memory:{id}").into()],
                    ))
                    .await?;
                }
            }
        }
        // 记忆图谱是可重建的派生缓存，合并时直接清除；底层的记忆条目、来源可见性与遗忘纪元（epoch）完整迁移。
        let (filter, values) = scope_filter("scope_key", source, target);
        tx.execute_raw(Statement::from_sql_and_values(
            backend,
            format!("DELETE FROM nuono_agent_memory_graphs WHERE {filter}"),
            values,
        ))
        .await?;
        for (table, column) in [
            ("nuono_assistant_turns", "scope_key"),
            ("nuono_agent_memories", "scope_key"),
            ("nuono_companion_memory_sources", "scope_key"),
            ("nuono_agent_runs", "session_id"),
            ("nuono_agent_tool_approvals", "session_id"),
            ("nuono_agent_tool_receipts", "session_id"),
        ] {
            move_scope_column(tx, table, column, source, target).await?;
        }
        moved += tx
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE nuono_ai_personas SET owner_user_id=$2 WHERE owner_user_id=$1",
                [source.into(), target.into()],
            ))
            .await?
            .rows_affected();
        moved += tx
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE nuono_assistant_turns SET user_id=$2 WHERE user_id=$1",
                [source.into(), target.into()],
            ))
            .await?
            .rows_affected();
        merge_assistant(tx, source, target).await?;
    }
    moved += tx
        .execute_raw(Statement::from_sql_and_values(
            backend,
            "UPDATE nuono_notifications SET user_id=$2 WHERE user_id=$1",
            [source.into(), target.into()],
        ))
        .await?
        .rows_affected();
    Ok(moved)
}

/// 合并群配置委派授权：同一群、同一授权范围与来源键发生冲突时，保留有效态且到期时间更晚（`NULL` 永久优先）的一条并删除重复项；
/// 不同 `source_key` 的独立历史授权记录全部随目标账号保留。
async fn merge_group_authorizations(
    tx: &DatabaseTransaction,
    source: i64,
    target: i64,
    now: i64,
) -> Result<(u64, u64)> {
    let backend = tx.get_database_backend();
    let rows = tx
        .query_all_raw(Statement::from_sql_and_values(
            backend,
            "SELECT id, group_id, capability, scope_kind, plugin_id, source_key, valid_from_ms, expires_at_ms, revoked_at_ms \
             FROM nuono_group_authorizations WHERE user_id=$1 ORDER BY id ASC",
            [source.into()],
        ))
        .await?;
    let mut migrated = 0_u64;
    let mut deduplicated = 0_u64;
    for row in rows {
        let old_id: i64 = row.try_get("", "id")?;
        let group_id: i64 = row.try_get("", "group_id")?;
        let capability: String = row.try_get("", "capability")?;
        let scope_kind: String = row.try_get("", "scope_kind")?;
        let plugin_id: Option<String> = row.try_get("", "plugin_id")?;
        let source_key: String = row.try_get("", "source_key")?;
        let old_valid_from: i64 = row.try_get("", "valid_from_ms")?;
        let old_expires: Option<i64> = row.try_get("", "expires_at_ms")?;
        let old_revoked: Option<i64> = row.try_get("", "revoked_at_ms")?;

        let existing = if let Some(pid) = &plugin_id {
            tx.query_one_raw(Statement::from_sql_and_values(
                backend,
                "SELECT id, valid_from_ms, expires_at_ms, revoked_at_ms \
                 FROM nuono_group_authorizations \
                 WHERE user_id=$1 AND group_id=$2 AND capability=$3 AND scope_kind=$4 AND plugin_id=$5 AND source_key=$6 \
                 ORDER BY CASE WHEN revoked_at_ms IS NULL THEN 0 ELSE 1 END, id ASC LIMIT 1",
                [
                    target.into(),
                    group_id.into(),
                    capability.into(),
                    scope_kind.into(),
                    pid.clone().into(),
                    source_key.into(),
                ],
            ))
            .await?
        } else {
            tx.query_one_raw(Statement::from_sql_and_values(
                backend,
                "SELECT id, valid_from_ms, expires_at_ms, revoked_at_ms \
                 FROM nuono_group_authorizations \
                 WHERE user_id=$1 AND group_id=$2 AND capability=$3 AND scope_kind=$4 AND plugin_id IS NULL AND source_key=$5 \
                 ORDER BY CASE WHEN revoked_at_ms IS NULL THEN 0 ELSE 1 END, id ASC LIMIT 1",
                [
                    target.into(),
                    group_id.into(),
                    capability.into(),
                    scope_kind.into(),
                    source_key.into(),
                ],
            ))
            .await?
        };

        let Some(retained) = existing else {
            tx.execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE nuono_group_authorizations SET user_id=$2,revision=revision+1,updated_at_ms=$3 WHERE id=$1",
                [old_id.into(), target.into(), now.into()],
            ))
            .await?;
            migrated += 1;
            continue;
        };

        let retained_id: i64 = retained.try_get("", "id")?;
        let retained_valid_from: i64 = retained.try_get("", "valid_from_ms")?;
        let retained_expires: Option<i64> = retained.try_get("", "expires_at_ms")?;
        let retained_revoked: Option<i64> = retained.try_get("", "revoked_at_ms")?;

        let (winner_valid_from, winner_expires, winner_revoked) =
            match (retained_revoked.is_none(), old_revoked.is_none()) {
                (true, true) => {
                    let starts = std::cmp::min(retained_valid_from, old_valid_from);
                    let expires = match (retained_expires, old_expires) {
                        (None, _) | (_, None) => None,
                        (Some(a), Some(b)) => Some(std::cmp::max(a, b)),
                    };
                    (starts, expires, None)
                }
                (true, false) => (retained_valid_from, retained_expires, None),
                (false, true) => (old_valid_from, old_expires, None),
                (false, false) => (retained_valid_from, retained_expires, retained_revoked),
            };
        tx.execute_raw(Statement::from_sql_and_values(
            backend,
            "UPDATE nuono_group_authorizations \
             SET valid_from_ms=$2, expires_at_ms=$3, revoked_at_ms=$4, revision=revision+1, updated_at_ms=$5 \
             WHERE id=$1",
            [
                retained_id.into(),
                winner_valid_from.into(),
                winner_expires.into(),
                winner_revoked.into(),
                now.into(),
            ],
        ))
        .await?;
        tx.execute_raw(Statement::from_sql_and_values(
            backend,
            "DELETE FROM nuono_group_authorizations WHERE id=$1",
            [old_id.into()],
        ))
        .await?;
        deduplicated += 1;
    }
    Ok((migrated, deduplicated))
}

/// 合并个人维度功能权益：同一 `(feature_key, source)` 冲突时保留有效态、较早生效时间及更长（或永久）有效期。
async fn merge_user_entitlements(
    tx: &DatabaseTransaction,
    source: i64,
    target: i64,
    now: i64,
) -> Result<(u64, u64)> {
    use crate::entity::entitlements;
    let mut migrated = 0_u64;
    let mut deduplicated = 0_u64;
    let source_rows = entitlements::Entity::find()
        .filter(entitlements::Column::SubjectType.eq("user"))
        .filter(entitlements::Column::SubjectId.eq(source))
        .all(tx)
        .await?;
    for old in source_rows {
        let existing = entitlements::Entity::find()
            .filter(entitlements::Column::SubjectType.eq("user"))
            .filter(entitlements::Column::SubjectId.eq(target))
            .filter(entitlements::Column::FeatureKey.eq(&old.feature_key))
            .filter(entitlements::Column::Source.eq(&old.source))
            .one(tx)
            .await?;
        let Some(retained) = existing else {
            entitlements::Entity::update_many()
                .col_expr(entitlements::Column::SubjectId, Expr::value(target))
                .col_expr(entitlements::Column::UpdatedAtMs, Expr::value(now))
                .filter(entitlements::Column::Id.eq(old.id))
                .exec(tx)
                .await?;
            migrated += 1;
            continue;
        };
        let (status, starts_at_ms, expires_at_ms, source_ref, limits_json) =
            match (retained.status.as_str(), old.status.as_str()) {
                ("active", "active") => {
                    let starts = std::cmp::min(retained.starts_at_ms, old.starts_at_ms);
                    let (expires, ref_winner) = match (retained.expires_at_ms, old.expires_at_ms) {
                        (None, _) => (None, retained.source_ref.or(old.source_ref)),
                        (_, None) => (None, old.source_ref.or(retained.source_ref)),
                        (Some(a), Some(b)) if b > a => {
                            (Some(b), old.source_ref.or(retained.source_ref))
                        }
                        (Some(a), _) => (Some(a), retained.source_ref.or(old.source_ref)),
                    };
                    let limits = merge_entitlement_limits(&retained.limits_json, &old.limits_json)?;
                    ("active".to_owned(), starts, expires, ref_winner, limits)
                }
                ("active", _) => (
                    retained.status,
                    retained.starts_at_ms,
                    retained.expires_at_ms,
                    retained.source_ref,
                    retained.limits_json,
                ),
                (_, "active") => (
                    old.status,
                    old.starts_at_ms,
                    old.expires_at_ms,
                    old.source_ref,
                    old.limits_json,
                ),
                _ => (
                    retained.status,
                    retained.starts_at_ms,
                    retained.expires_at_ms,
                    retained.source_ref,
                    retained.limits_json,
                ),
            };
        entitlements::Entity::update_many()
            .col_expr(entitlements::Column::Status, Expr::value(status))
            .col_expr(entitlements::Column::StartsAtMs, Expr::value(starts_at_ms))
            .col_expr(
                entitlements::Column::ExpiresAtMs,
                Expr::value(expires_at_ms),
            )
            .col_expr(entitlements::Column::SourceRef, Expr::value(source_ref))
            .col_expr(entitlements::Column::LimitsJson, Expr::value(limits_json))
            .col_expr(entitlements::Column::UpdatedAtMs, Expr::value(now))
            .filter(entitlements::Column::Id.eq(retained.id))
            .exec(tx)
            .await?;
        entitlements::Entity::delete_by_id(old.id).exec(tx).await?;
        deduplicated += 1;
    }
    Ok((migrated, deduplicated))
}

fn merge_entitlement_limits(retained_json: &str, old_json: &str) -> Result<String> {
    let mut merged = serde_json::from_str::<serde_json::Map<String, Value>>(retained_json)?;
    let old = serde_json::from_str::<serde_json::Map<String, Value>>(old_json)?;
    for (key, value) in old {
        match merged.get(&key) {
            Some(current) => {
                if let (Some(a), Some(b)) = (current.as_i64(), value.as_i64())
                    && b > a
                {
                    merged.insert(key, value);
                }
            }
            None => {
                merged.insert(key, value);
            }
        }
    }
    Ok(serde_json::to_string(&merged)?)
}
fn merge_context_epochs(
    source_json: &str,
    target_json: &str,
    source: i64,
    target: i64,
) -> Result<String> {
    let mut epochs: BTreeMap<String, i64> = serde_json::from_str(target_json)?;
    let source_epochs: BTreeMap<String, i64> = serde_json::from_str(source_json)?;
    for (scope, epoch) in source_epochs {
        let moved = moved_scope(&scope, source, target).unwrap_or(scope);
        epochs
            .entry(moved)
            .and_modify(|current| *current = std::cmp::max(*current, epoch))
            .or_insert(epoch);
    }
    Ok(serde_json::to_string(&epochs)?)
}
async fn merge_assistant(tx: &DatabaseTransaction, source: i64, target: i64) -> Result<()> {
    let Some(old) = account_assistant::Entity::find_by_id(source)
        .one(tx)
        .await?
    else {
        return Ok(());
    };
    let retained = account_assistant::Entity::find_by_id(target)
        .one(tx)
        .await?;
    if let Some(retained) = retained {
        let day = std::cmp::max(old.usage_day, retained.usage_day);
        let mut requests = BTreeSet::new();
        for record in [&old, &retained] {
            if record.usage_day == day {
                requests.extend(serde_json::from_str::<Vec<String>>(&record.request_ids)?);
            }
        }
        let context_epochs = merge_context_epochs(
            &old.context_epochs_json,
            &retained.context_epochs_json,
            source,
            target,
        )?;
        account_assistant::Entity::update_many()
            .col_expr(
                account_assistant::Column::ActiveCardId,
                Expr::value(retained.active_card_id.or(old.active_card_id)),
            )
            .col_expr(
                account_assistant::Column::GrantUntil,
                Expr::value(std::cmp::max(retained.grant_until, old.grant_until)),
            )
            .col_expr(
                account_assistant::Column::GrantDailyLimit,
                Expr::value(std::cmp::max(
                    retained.grant_daily_limit,
                    old.grant_daily_limit,
                )),
            )
            .col_expr(account_assistant::Column::UsageDay, Expr::value(day))
            .col_expr(
                account_assistant::Column::RequestIds,
                Expr::value(serde_json::to_string(&requests)?),
            )
            .col_expr(
                account_assistant::Column::ContextEpochsJson,
                Expr::value(context_epochs),
            )
            .filter(account_assistant::Column::UserId.eq(target))
            .exec(tx)
            .await?;
        account_assistant::Entity::delete_by_id(source)
            .exec(tx)
            .await?;
    } else {
        let context_epochs = merge_context_epochs(&old.context_epochs_json, "{}", source, target)?;
        account_assistant::Entity::update_many()
            .col_expr(account_assistant::Column::UserId, Expr::value(target))
            .col_expr(
                account_assistant::Column::ContextEpochsJson,
                Expr::value(context_epochs),
            )
            .filter(account_assistant::Column::UserId.eq(source))
            .exec(tx)
            .await?;
    }
    Ok(())
}

async fn merge_plugin_state(tx: &DatabaseTransaction, source: i64, target: i64) -> Result<()> {
    CorePluginKvParticipant {
        strict_account_conflicts: true,
    }
    .merge_in_tx(tx, source, target)
    .await?;
    Ok(())
}

async fn merge_plugin_kv_scope(
    tx: &DatabaseTransaction,
    source_scope: &str,
    target_scope: &str,
    strict_conflicts: bool,
) -> Result<(u64, u64)> {
    use crate::entity::plugin_kv as kv;
    let now_ms = crate::clock::db_now().timestamp_millis();
    // 已过期的临时票据与租约不包含有效的账号状态，合并前直接清理。
    kv::Entity::delete_many()
        .filter(kv::Column::ScopeKey.is_in([source_scope.to_owned(), target_scope.to_owned()]))
        .filter(kv::Column::ExpiresAtMs.lte(now_ms))
        .exec(tx)
        .await?;
    let rows = kv::Entity::find()
        .filter(kv::Column::ScopeKey.eq(source_scope))
        .all(tx)
        .await?;
    let mut migrated = 0_u64;
    let mut deduplicated = 0_u64;
    for row in rows {
        let destination = kv::Entity::find_by_id((
            row.plugin_id.clone(),
            row.collection.clone(),
            target_scope.to_owned(),
            row.record_key.clone(),
        ))
        .one(tx)
        .await?;
        if let Some(existing) = destination {
            let values_differ = existing.expires_at_ms != row.expires_at_ms
                || existing.value_version != row.value_version
                || serde_json::from_str::<Value>(&existing.value_json)?
                    != serde_json::from_str::<Value>(&row.value_json)?;
            if values_differ && strict_conflicts {
                return Err(rejected(format!(
                    "插件 {} 未解决账号状态冲突",
                    row.plugin_id
                )));
            }
            // 同名 record_key：当非严格模式且源记录更新时间严格更新时覆盖目标记录，否则默认保留目标记录。
            if values_differ && row.updated_at > existing.updated_at {
                kv::Entity::update_many()
                    .col_expr(kv::Column::ValueJson, Expr::value(row.value_json))
                    .col_expr(kv::Column::ValueVersion, Expr::value(row.value_version))
                    .col_expr(kv::Column::ExpiresAtMs, Expr::value(row.expires_at_ms))
                    .col_expr(kv::Column::Revision, Expr::col(kv::Column::Revision).add(1))
                    .col_expr(kv::Column::UpdatedAt, Expr::value(row.updated_at))
                    .filter(kv::Column::PluginId.eq(&row.plugin_id))
                    .filter(kv::Column::Collection.eq(&row.collection))
                    .filter(kv::Column::ScopeKey.eq(target_scope))
                    .filter(kv::Column::RecordKey.eq(&row.record_key))
                    .exec(tx)
                    .await?;
            }
            kv::Entity::delete_by_id((
                row.plugin_id,
                row.collection,
                row.scope_key,
                row.record_key,
            ))
            .exec(tx)
            .await?;
            deduplicated += 1;
        } else {
            kv::Entity::update_many()
                .col_expr(kv::Column::ScopeKey, Expr::value(target_scope))
                .col_expr(kv::Column::Revision, Expr::col(kv::Column::Revision).add(1))
                .col_expr(kv::Column::UpdatedAt, Expr::value(crate::clock::db_now()))
                .filter(kv::Column::PluginId.eq(row.plugin_id))
                .filter(kv::Column::Collection.eq(row.collection))
                .filter(kv::Column::RecordKey.eq(row.record_key))
                .filter(kv::Column::ScopeKey.eq(source_scope))
                .exec(tx)
                .await?;
            migrated += 1;
        }
    }
    Ok((migrated, deduplicated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::NewUser, test_support::test_db};
    use std::sync::Arc;
    const PASSWORD: &str = "Merge-password-123!";
    async fn account(db: &Database, name: &str) -> users::Model {
        db.create_user(NewUser {
            username: name.into(),
            email: Some(format!("{name}@example.com")),
            password: PASSWORD.into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap()
    }
    async fn session(db: &Database, user: &users::Model, token: &str) {
        let key = format!("auth_session_{}", hex::encode(token_hash(token)));
        assert!(
            db.create_session_for_verified_user(
                user,
                &key,
                chrono::Utc::now() + chrono::Duration::hours(1)
            )
            .await
            .unwrap()
        );
    }
    fn credentials(name: &str) -> MergeCredentials<'_> {
        MergeCredentials {
            source_username: name,
            source_password: PASSWORD,
            source_second_factor: None,
            target_password: PASSWORD,
            target_second_factor: None,
        }
    }
    async fn plugin(
        db: &Database,
        id: &str,
        participant: Option<Arc<dyn AccountMergeParticipant>>,
    ) {
        let mut manifest = nuono_app::plugin_manifest::PortableManifest::empty(id);
        manifest.merge_participant_version = participant.as_ref().map(|p| p.version());
        db.register_plugin_storage(
            id,
            "1.0.0",
            &nuono_app::plugin_manifest::PluginStorageDeclaration {
                manifest,
                versions: Vec::new(),
            },
        )
        .await
        .unwrap();
        if let Some(participant) = participant {
            register_account_merge_participant(id, participant).unwrap();
        }
    }
    async fn patch(db: &Database, plugin: &str, user: i64, value: Value) {
        plugin_overrides::ActiveModel {
            id: Default::default(),
            plugin_id: Set(plugin.into()),
            user_id: Set(Some(user)),
            group_id: Set(None),
            enabled: Set(None),
            config_patch_json: Set(value.to_string()),
            service_patch_json: Set("{}".into()),
            revision: Set(1),
            created_at_ms: Set(1),
            updated_at_ms: Set(1),
        }
        .insert(db.orm())
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn merge_moves_private_data_preserves_forgetting_and_releases_only_source_login() {
        let db = test_db(3).await;
        let source = account(&db, "merge-source").await;
        let target = account(&db, "merge-target").await;
        session(&db, &source, "source-token").await;
        session(&db, &target, "target-token").await;
        plugin(&db, "merge_config", None).await;
        patch(
            &db,
            "merge_config",
            source.id,
            json!({"only_source":1,"same":{"source":true}}),
        )
        .await;
        patch(
            &db,
            "merge_config",
            target.id,
            json!({"same":{"target":true}}),
        )
        .await;
        let card = db.ensure_default_companion_card(source.id).await.unwrap();
        let old_scope = nuono_app::agent::companion_scope_key(source.id, &card.id);
        tx_exec(
            &db,
            "INSERT INTO nuono_account_assistant(user_id,context_epochs_json) VALUES($1,$2) ON CONFLICT(user_id) DO UPDATE SET context_epochs_json=excluded.context_epochs_json",
            vec![source.id.into(), json!({ old_scope: 7 }).to_string().into()],
        )
        .await;
        let preview = db
            .prepare_account_merge(target.id, "target-token", credentials(&source.username))
            .await
            .unwrap();
        assert_eq!(
            preview.plan["configuration_conflicts"][0]["fields"][0],
            "configuration.same"
        );
        let receipt = db
            .confirm_account_merge(
                target.id,
                "target-token",
                &preview.id,
                1,
                &preview.plan_digest,
            )
            .await
            .unwrap();
        assert_eq!(receipt.status, "completed");
        assert!(
            db.authenticate_session("source-token")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.authenticate_session("target-token")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.find_user_by_username(&source.username)
                .await
                .unwrap()
                .is_none()
        );
        let tombstone = users::orm::Entity::find_by_id(source.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tombstone.status, "merged");
        assert_eq!(tombstone.merged_into_user_id, Some(target.id));
        assert!(tombstone.password_hash.is_none());
        let transferred = db.companion_card(target.id, &card.id).await.unwrap();
        assert_eq!(transferred.id, card.id);
        let new_scope = nuono_app::agent::companion_scope_key(target.id, &card.id);
        let assistant = account_assistant::Entity::find_by_id(target.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let epochs: BTreeMap<String, i64> =
            serde_json::from_str(&assistant.context_epochs_json).unwrap();
        assert_eq!(epochs.get(&new_scope), Some(&7));
        let value = plugin_overrides::Entity::find()
            .filter(plugin_overrides::Column::UserId.eq(target.id))
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&value.config_patch_json).unwrap(),
            json!({"only_source":1,"same":{"target":true}})
        );
        let new = account(&db, "merge-source").await;
        assert!(new.id > target.id && new.id != source.id);
        // 合并完成状态是一份持久化幂等回执，重复调用直接返回已完成回执，不会重放合并事务。
        assert_eq!(
            db.confirm_account_merge(
                target.id,
                "expired-token",
                &preview.id,
                1,
                &preview.plan_digest
            )
            .await
            .unwrap()
            .id,
            receipt.id
        );
        db.shutdown().await.unwrap();
    }
    async fn tx_exec(db: &Database, sql: &str, values: Vec<sea_orm::Value>) {
        db.orm()
            .execute_raw(Statement::from_sql_and_values(db.backend(), sql, values))
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn changed_preview_or_revoked_proof_cannot_merge() {
        let db = test_db(3).await;
        let source = account(&db, "preview-source").await;
        let target = account(&db, "preview-target").await;
        session(&db, &target, "preview-token").await;
        let preview = db
            .prepare_account_merge(target.id, "preview-token", credentials(&source.username))
            .await
            .unwrap();
        db.ensure_default_companion_card(source.id).await.unwrap();
        assert!(
            db.confirm_account_merge(
                target.id,
                "preview-token",
                &preview.id,
                1,
                &preview.plan_digest
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.account_merge_status(target.id, &preview.id)
                .await
                .unwrap()
                .status,
            "failed"
        );
        assert!(
            db.verify_user_password(&source.username, PASSWORD)
                .await
                .unwrap()
                .is_some()
        );
        let preview = db
            .prepare_account_merge(target.id, "preview-token", credentials(&source.username))
            .await
            .unwrap();
        db.revoke_session_token("preview-token").await.unwrap();
        assert!(
            db.confirm_account_merge(
                target.id,
                "preview-token",
                &preview.id,
                1,
                &preview.plan_digest
            )
            .await
            .is_err()
        );
        assert!(
            db.verify_user_password(&source.username, PASSWORD)
                .await
                .unwrap()
                .is_some()
        );
        db.shutdown().await.unwrap();
    }
    struct Failure;
    #[async_trait::async_trait]
    impl AccountMergeParticipant for Failure {
        fn version(&self) -> i64 {
            1
        }
        async fn preflight(
            &self,
            context: &AccountMergeContext,
            data: &dyn AccountMergeRead,
        ) -> Result<Value> {
            Ok(json!({"state":data.account_state(context.source_user_id).await?}))
        }
        async fn apply(
            &self,
            context: &AccountMergeContext,
            _: &Value,
            tx: &dyn AccountMergeTransaction,
        ) -> Result<()> {
            tx.put_account_state(
                context.target_user_id,
                MergeStateRecord {
                    collection: "data".into(),
                    key: "moved".into(),
                    value_version: 1,
                    value: json!(true),
                },
            )
            .await?;
            Err(rejected("injected plugin failure"))
        }
    }
    #[tokio::test]
    async fn unsupported_or_failing_plugin_blocks_the_entire_merge() {
        let db = test_db(3).await;
        let source = account(&db, "plugin-source").await;
        let target = account(&db, "plugin-target").await;
        session(&db, &target, "plugin-token").await;
        plugin(&db, "merge_failure", Some(Arc::new(Failure))).await;
        tx_exec(&db,"INSERT INTO nuono_plugin_kv(plugin_id,collection,scope_key,record_key,value_json,value_version,created_at_ms,updated_at_ms,generation) VALUES('merge_failure','data',$1,'original','true',1,1,1,'00000000-0000-0000-0000-000000000001')",vec![format!("account:{}",source.id).into()]).await;
        let card = db.ensure_default_companion_card(source.id).await.unwrap();
        let preview = db
            .prepare_account_merge(target.id, "plugin-token", credentials(&source.username))
            .await
            .unwrap();
        assert!(
            db.confirm_account_merge(
                target.id,
                "plugin-token",
                &preview.id,
                1,
                &preview.plan_digest
            )
            .await
            .is_err()
        );
        assert!(db.companion_card(source.id, &card.id).await.is_ok());
        assert!(db.companion_card(target.id, &card.id).await.is_err());
        assert!(
            crate::entity::plugin_kv::Entity::find_by_id((
                "merge_failure".into(),
                "data".into(),
                format!("account:{}", target.id),
                "moved".into()
            ))
            .one(db.orm())
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            db.authenticate_session("plugin-token")
                .await
                .unwrap()
                .is_some()
        );
        unregister_account_merge_participant("merge_failure");
        assert!(
            db.prepare_account_merge(target.id, "plugin-token", credentials(&source.username))
                .await
                .is_err()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn both_accounts_require_fresh_second_factor_proof() {
        let db = test_db(3).await;
        let source = account(&db, "mfa-source").await;
        let target = account(&db, "mfa-target").await;
        session(&db, &target, "mfa-token").await;
        db.set_totp_secret(source.id, Some("JBSWY3DPEHPK3PXP"))
            .await
            .unwrap();
        db.set_totp_state(
            source.id,
            true,
            Some(&[nuono_app::auth::recovery_code_hash("source-recovery")]),
        )
        .await
        .unwrap();
        assert!(
            db.prepare_account_merge(target.id, "mfa-token", credentials(&source.username))
                .await
                .is_err()
        );
        let mut input = credentials(&source.username);
        input.source_second_factor = Some("source-recovery");
        assert!(
            db.prepare_account_merge(target.id, "mfa-token", input)
                .await
                .is_ok()
        );
        let mut input = credentials(&source.username);
        input.source_second_factor = Some("source-recovery");
        assert!(
            db.prepare_account_merge(target.id, "mfa-token", input)
                .await
                .is_err()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn merge_preserves_independent_grant_sources_expiry_revocation_and_issuer_history() {
        let db = test_db(3).await;
        let source = account(&db, "grant-source").await;
        let target = account(&db, "grant-target").await;
        db.set_user_role(source.id, nuono_app::auth::Role::Admin)
            .await
            .unwrap();
        session(&db, &target, "grant-merge-token").await;
        let now = crate::clock::db_now().timestamp_millis();
        let group = crate::entity::groups::ActiveModel {
            id: Default::default(),
            platform: Set("qq".into()),
            namespace: Set("fixture".into()),
            conversation_kind: Set("group".into()),
            native_parent_id: Set(String::new()),
            native_group_id: Set("merge-grant-group".into()),
            display_name: Set("Preserved group".into()),
            enabled: Set(true),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
        }
        .insert(db.orm())
        .await
        .unwrap();
        let first = db
            .grant_group_configuration(
                source.id,
                &nuono_app::group_authorization::GroupGrantRequest {
                    user_id: source.id,
                    group_id: group.id,
                    plugin_id: None,
                    source_key: "first-purchase".into(),
                    valid_from_ms: now - 1_000,
                    expires_at_ms: Some(now + 86_400_000),
                },
            )
            .await
            .unwrap();
        let second = db
            .grant_group_configuration(
                source.id,
                &nuono_app::group_authorization::GroupGrantRequest {
                    user_id: source.id,
                    group_id: group.id,
                    plugin_id: None,
                    source_key: "refunded-purchase".into(),
                    valid_from_ms: now - 1_000,
                    expires_at_ms: Some(now + 172_800_000),
                },
            )
            .await
            .unwrap();
        db.revoke_group_configuration(source.id, group.id, second.id)
            .await
            .unwrap();
        let before = db.group_configuration_grants(group.id).await.unwrap();
        let preview = db
            .prepare_account_merge(
                target.id,
                "grant-merge-token",
                credentials(&source.username),
            )
            .await
            .unwrap();
        db.confirm_account_merge(
            target.id,
            "grant-merge-token",
            &preview.id,
            1,
            &preview.plan_digest,
        )
        .await
        .unwrap();
        let after = db.group_configuration_grants(group.id).await.unwrap();
        assert_eq!(before.len(), after.len());
        for (old, new) in before.iter().zip(&after) {
            assert_eq!(new.user_id, target.id);
            assert_eq!(new.issuer_user_id, Some(source.id));
            assert_eq!(new.id, old.id);
            assert_eq!(new.source_key, old.source_key);
            assert_eq!(new.valid_from_ms, old.valid_from_ms);
            assert_eq!(new.expires_at_ms, old.expires_at_ms);
            assert_eq!(new.revoked_at_ms, old.revoked_at_ms);
        }
        assert!(
            after
                .iter()
                .any(|grant| grant.id == first.id && grant.revoked_at_ms.is_none())
        );
        assert!(
            db.has_group_configuration_grant(target.id, group.id, None)
                .await
                .unwrap()
        );
        assert_eq!(
            db.find_user_by_id(target.id).await.unwrap().unwrap().role,
            "user"
        );
        assert_eq!(
            crate::entity::groups::Entity::find_by_id(group.id)
                .one(db.orm())
                .await
                .unwrap()
                .unwrap(),
            group
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn unfinished_private_agent_runs_block_merge_and_history_cannot_resume_as_old_account() {
        let db = test_db(3).await;
        let source = account(&db, "run-source").await;
        let target = account(&db, "run-target").await;
        session(&db, &target, "run-merge-token").await;
        let scope = nuono_app::agent::assistant_chat_key(source.id);
        let mut run = nuono_app::agent::store::AgentRunRecord {
            id: "merge-private-run".into(),
            parent_id: None,
            session_id: scope.clone(),
            status: "waiting_approval".into(),
            state_json: "{}".into(),
            approval_id: None,
            revision: 0,
        };
        db.create_agent_run(run.clone()).await.unwrap();
        assert!(
            db.prepare_account_merge(target.id, "run-merge-token", credentials(&source.username))
                .await
                .is_err()
        );
        run.status = "cancelled".into();
        assert!(db.update_agent_run(run.clone(), 0).await.unwrap());
        let preview = db
            .prepare_account_merge(target.id, "run-merge-token", credentials(&source.username))
            .await
            .unwrap();
        db.confirm_account_merge(
            target.id,
            "run-merge-token",
            &preview.id,
            1,
            &preview.plan_digest,
        )
        .await
        .unwrap();
        let retained = db.get_agent_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            retained.session_id,
            nuono_app::agent::assistant_chat_key(target.id)
        );
        assert_eq!(retained.status, "cancelled");
        run.id = "new-old-account-run".into();
        run.status = "running".into();
        assert!(db.create_agent_run(run.clone()).await.is_err());
        let mut resumed = retained;
        resumed.status = "running".into();
        assert!(db.update_agent_run(resumed, 1).await.is_err());
        run.id = "new-retained-account-run".into();
        run.session_id = nuono_app::agent::assistant_chat_key(target.id);
        db.create_agent_run(run).await.unwrap();
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn merged_private_history_is_readable_and_forgetting_clears_source_checkpoints() {
        use nuono_app::companion_data::{CompanionDeletion, DeletionKind};
        let db = test_db(3).await;
        let source = account(&db, "privacy-source").await;
        let target = account(&db, "privacy-target").await;
        session(&db, &target, "privacy-merge-token").await;
        let card = db.ensure_default_companion_card(source.id).await.unwrap();
        let source_scope = nuono_app::agent::companion_scope_key(source.id, &card.id);
        let history = nuono_app::agent::companion_history_key(&source_scope, 0);
        let memory_record = nuono_app::agent::store::AgentMemoryRecord {
            scope_type: "user".into(),
            scope_key: source_scope.clone(),
            memory_key: "private-memory".into(),
            content: "private memory marker".into(),
            tags_json: None,
            enabled: true,
            expires_at: None,
            actor: None,
        };
        let memory = db.create_agent_memory(memory_record.clone()).await.unwrap();
        db.save_ai_conversation(&history, "user", "private conversation marker")
            .await
            .unwrap();
        db.create_agent_run(nuono_app::agent::store::AgentRunRecord {
            id: "private-history-before-merge".into(),
            parent_id: None,
            session_id: history.clone(),
            status: "completed".into(),
            state_json:
                json!({"private":"checkpoint marker","original_actor":source.id.to_string()})
                    .to_string(),
            approval_id: None,
            revision: 0,
        })
        .await
        .unwrap();
        db.save_agent_tool_receipt(nuono_app::agent::store::AgentToolReceiptRecord {
            context_json: Some(json!({"private":"context marker"}).to_string()),
            session_id: history,
            bot_id: 0,
            platform: nuono_app::agent::ACCOUNT_CHAT_PLATFORM.into(),
            message_type: "private".into(),
            user_id: source.id,
            group_id: None,
            channel_id: None,
            message_id: None,
            tool_name: "fixture".into(),
            risk_level: "low".into(),
            status: "succeeded".into(),
            input_json: Some("{}".into()),
            output_json: Some("{\"private\":\"tool marker\"}".into()),
            error: None,
            elapsed_ms: 1,
        })
        .await
        .unwrap();
        let plan = db
            .prepare_account_merge(
                target.id,
                "privacy-merge-token",
                credentials(&source.username),
            )
            .await
            .unwrap();
        db.confirm_account_merge(
            target.id,
            "privacy-merge-token",
            &plan.id,
            1,
            &plan.plan_digest,
        )
        .await
        .unwrap();
        assert_eq!(
            db.companion_conversations(target.id, &card.id, 20)
                .await
                .unwrap()
                .len(),
            1
        );
        let target_scope = nuono_app::agent::companion_scope_key(target.id, &card.id);
        assert_eq!(
            db.get_agent_memory(memory.id)
                .await
                .unwrap()
                .unwrap()
                .scope_key,
            target_scope
        );
        assert!(db.create_agent_memory(memory_record).await.is_err());
        assert!(
            db.update_agent_memory_in_scopes(
                memory.id,
                &[("user".into(), source_scope.clone())],
                nuono_app::agent::store::AgentMemoryUpdateRecord {
                    content: Some("stale write".into()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert!(
            db.delete_agent_memory_in_scopes(memory.id, &[("user".into(), source_scope)])
                .await
                .is_err()
        );
        let through = chrono::Utc::now() + chrono::Duration::seconds(1);
        db.apply_companion_deletion(&CompanionDeletion {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: target.id,
            scope_keys: vec![target_scope.clone()],
            through,
            kind: DeletionKind::Forget { memory_ids: vec![] },
        })
        .await
        .unwrap();
        assert!(
            db.get_agent_run("private-history-before-merge")
                .await
                .unwrap()
                .is_none()
        );
        let receipts = db.list_agent_tool_receipts(10).await.unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].user_id, source.id);
        assert!(
            receipts[0].context_json.is_none()
                && receipts[0].input_json.is_none()
                && receipts[0].output_json.is_none()
        );
        db.apply_companion_deletion(&CompanionDeletion {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: target.id,
            scope_keys: vec![target_scope],
            through,
            kind: DeletionKind::All,
        })
        .await
        .unwrap();
        assert!(
            db.companion_conversations(target.id, &card.id, 20)
                .await
                .unwrap()
                .is_empty()
        );
        db.shutdown().await.unwrap();
    }
    #[test]
    fn transaction_port_rejects_cross_table_and_side_effecting_sql() {
        let tables = BTreeSet::from(["example_rows".into()]);
        for sql in [
            "SELECT * FROM nuono_users",
            "SELECT pg_read_file($1) FROM example_rows",
            "SELECT * FROM example_rows, nuono_users",
            "SELECT * FROM example_rows WHERE id IN (SELECT id FROM nuono_users)",
            "SELECT * FROM example_rows; DELETE FROM nuono_users",
            "SELECT * FROM example_rows JOIN nuono_users ON true",
        ] {
            assert!(
                allowed_statement(
                    &Statement::from_string(sea_orm::DbBackend::Postgres, sql),
                    &tables,
                    false
                )
                .is_err(),
                "{sql}"
            );
        }
        assert!(
            allowed_statement(
                &Statement::from_string(
                    sea_orm::DbBackend::Postgres,
                    "SELECT id,user_id FROM example_rows WHERE user_id=$1"
                ),
                &tables,
                false
            )
            .is_ok()
        );
        assert!(
            allowed_statement(
                &Statement::from_string(
                    sea_orm::DbBackend::Postgres,
                    "UPDATE example_rows SET user_id=$1 WHERE user_id=$2"
                ),
                &tables,
                true
            )
            .is_ok()
        );
    }
}
