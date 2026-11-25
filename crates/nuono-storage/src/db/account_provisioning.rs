//! 账号开通与邮箱更换：与角色/状态变更共用同一把账号管理锁。
use super::{Database, user_tokens::issue_proof_on, users::new_user_active_model};
use crate::{
    entity::users,
    error::{NuonoError, Result},
};
use nuono_app::{account_provisioning::*, auth::Role};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, QuerySelect, Statement,
};

impl From<users::Model> for ManagedAccount {
    fn from(user: users::Model) -> Self {
        Self {
            id: user.id,
            username: user.username,
            email: user.email,
            email_verified: user.email_verified,
            role: user.role,
            status: user.status,
            last_login_at: user.last_login_at.map(crate::clock::format_wire_ts),
            created_at: crate::clock::format_wire_ts(user.created_at),
        }
    }
}
#[async_trait::async_trait]
impl AccountProvisioningStore for Database {
    async fn create_managed_account(
        &self,
        actor_id: i64,
        input: NewManagedAccount,
    ) -> Result<ManagedAccount> {
        let input = validate_input(input)?;
        let hash = self.password_work.hash(&input.password).await?;
        let tx = self.begin_write().await?;
        lock_administration(self, &tx, actor_id, input.role).await?;
        let user = insert_account(
            &tx,
            new_user_active_model(
                &input.username,
                input.email.as_deref(),
                true,
                &hash,
                input.role.as_str(),
                "active",
                crate::clock::db_now(),
            ),
        )
        .await?;
        super::append_audit_on(
            &tx,
            super::AuditEvent {
                actor_user_id: Some(actor_id),
                actor_plugin_id: None,
                action: "account.created",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({"role":user.role}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(user.into())
    }
    async fn prepare_email_change(
        &self,
        user_id: i64,
        password: &str,
        email: &str,
        ttl: i64,
    ) -> Result<Option<AccountLink>> {
        let normalized_email = email.trim().to_ascii_lowercase();
        let email = normalized_email.as_str();
        nuono_app::auth::validate_email(email).map_err(NuonoError::Validation)?;
        // 密码哈希校验属于 CPU 密集型工作，校验期间不持有账号写锁；
        // 下方进入写事务并加锁后，会重新读取账号记录以确认校验期间密码未被并发修改。
        let verified = users::orm::Entity::find_by_id(user_id)
            .one(self.orm())
            .await?
            .ok_or_else(|| NuonoError::PermissionDenied("账号不存在".into()))?;
        if verified.status != "active"
            || !self
                .password_work
                .verify(
                    password,
                    verified.password_hash.as_deref().unwrap_or_default(),
                )
                .await?
        {
            return Err(NuonoError::PermissionDenied("当前密码不正确".into()));
        }
        let tx = self.begin_write().await?;
        let mut query = users::orm::Entity::find_by_id(user_id);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let user = query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::PermissionDenied("账号不存在".into()))?;
        if user.status != "active"
            || user.auth_epoch != verified.auth_epoch
            || user.password_hash != verified.password_hash
        {
            return Err(NuonoError::PermissionDenied("当前密码不正确".into()));
        }
        if user.email.as_deref() == Some(email) {
            return Err(NuonoError::ConfigValidation("新邮箱与当前邮箱相同".into()));
        }
        if users::orm::Entity::find()
            .filter(users::orm::Column::EmailKey.eq(email))
            .one(&tx)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        let (id, issued) = issue_proof_on(
            &tx,
            "email_change",
            Some(user.id),
            Some((user.id, user.auth_epoch)),
            ttl,
            serde_json::json!({"email":email}),
        )
        .await?;
        super::append_audit_on(
            &tx,
            super::AuditEvent {
                actor_user_id: Some(user.id),
                actor_plugin_id: None,
                action: "account.email_change_requested",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: Some(&id),
                details: serde_json::json!({}),
            },
        )
        .await?;
        let token = issued.token;
        tx.commit().await?;
        Ok(Some(AccountLink {
            account: users::Model::from(user).into(),
            token,
        }))
    }
}
fn validate_input(mut input: NewManagedAccount) -> Result<NewManagedAccount> {
    input.username = input.username.trim().to_owned();
    input.email = input
        .email
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| !email.is_empty());
    nuono_app::auth::validate_username(&input.username).map_err(NuonoError::Validation)?;
    nuono_app::auth::validate_password(&input.password).map_err(NuonoError::Validation)?;
    if let Some(email) = &input.email {
        nuono_app::auth::validate_email(email).map_err(NuonoError::Validation)?;
    }
    Ok(input)
}
async fn current_actor_role(tx: &impl ConnectionTrait, actor_id: i64) -> Result<Role> {
    let actor = users::orm::Entity::find_by_id(actor_id)
        .one(tx)
        .await?
        .ok_or_else(|| NuonoError::PermissionDenied("账号不存在".into()))?;
    if actor.status != "active" {
        return Err(NuonoError::PermissionDenied("账号不可用".into()));
    }
    Ok(Role::from_db_value(&actor.role))
}
async fn lock_administration(
    db: &Database,
    tx: &impl ConnectionTrait,
    actor_id: i64,
    role: Role,
) -> Result<()> {
    if db.backend() == DbBackend::Postgres {
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_advisory_xact_lock($1)",
            [crate::account_store::ACCOUNT_ADMIN_LOCK_KEY.into()],
        ))
        .await?;
    }
    let mut actor = users::orm::Entity::find_by_id(actor_id);
    if db.backend() == DbBackend::Postgres {
        actor = actor.lock_exclusive();
    }
    actor.one(tx).await?;
    validate_provisioning_role(role, current_actor_role(tx, actor_id).await?)
}
fn conflict(error: sea_orm::DbErr) -> NuonoError {
    if matches!(
        error.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    ) {
        NuonoError::Conflict("用户名或邮箱已经被使用".into())
    } else {
        error.into()
    }
}
async fn insert_account(
    tx: &impl ConnectionTrait,
    model: users::orm::ActiveModel,
) -> Result<users::Model> {
    let id = users::orm::Entity::insert(model)
        .exec(tx)
        .await
        .map_err(conflict)?
        .last_insert_id;
    users::orm::Entity::find_by_id(id)
        .one(tx)
        .await?
        .map(Into::into)
        .ok_or_else(|| NuonoError::NotFound("账号插入后不存在".into()))
}
