//! 账号变更与会话失效处理（兼容 PostgreSQL 与 SQLite 双后端，加锁串行化执行）。
use crate::db::account_state::{AuditEvent, append_audit_on, cancel_proofs_on, revoke_sessions_on};
use crate::{
    Database,
    entity::{notifications, users},
    error::{NuonoError, Result},
};
use nuono_app::{accounts::*, auth::Role};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect,
    Statement,
    sea_query::{Expr, ExprTrait},
};
pub(crate) const ACCOUNT_ADMIN_LOCK_KEY: i64 = 0x6E75_6F6E_6F04;

#[async_trait::async_trait]
impl AccountStore for Database {
    async fn mutate_account(
        &self,
        actor_id: i64,
        target_id: i64,
        mutation: AccountMutation,
    ) -> Result<AccountMutationResult> {
        self.apply_account_mutation(actor_id, target_id, mutation, false)
            .await
    }
    async fn mutate_account_authorized(
        &self,
        caller: &nuono_app::auth::CallerIdentity,
        target_id: i64,
        mutation: AccountMutation,
    ) -> Result<AccountMutationResult> {
        self.apply_account_mutation(caller.user_id, target_id, mutation, true)
            .await
    }
}
impl Database {
    async fn apply_account_mutation(
        &self,
        actor_id: i64,
        target_id: i64,
        mutation: AccountMutation,
        check_actor: bool,
    ) -> Result<AccountMutationResult> {
        let tx = self.begin_write().await?;
        if self.backend() == DbBackend::Postgres {
            tx.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_advisory_xact_lock($1)",
                [ACCOUNT_ADMIN_LOCK_KEY.into()],
            ))
            .await?;
        }
        // 操作者与目标账号按照与账号合并一致的 ID 顺序加锁；拿齐两把锁后再复核操作者权限。
        for id in if actor_id < target_id {
            vec![actor_id, target_id]
        } else if actor_id > target_id {
            vec![target_id, actor_id]
        } else {
            vec![actor_id]
        } {
            let mut query = users::orm::Entity::find_by_id(id);
            if self.backend() == DbBackend::Postgres {
                query = query.lock_exclusive();
            }
            query.one(&tx).await?;
        }
        if check_actor {
            let actor = users::orm::Entity::find_by_id(actor_id)
                .one(&tx)
                .await?
                .ok_or_else(|| NuonoError::PermissionDenied("账号不存在".into()))?;
            if actor.status != "active"
                || !Role::from_db_value(&actor.role).has(nuono_app::auth::Capability::AssignRoles)
            {
                return Err(NuonoError::PermissionDenied("assign_roles".into()));
            }
        }
        let mut target_query = users::orm::Entity::find_by_id(target_id);
        if self.backend() == DbBackend::Postgres {
            target_query = target_query.lock_exclusive();
        }
        let target = target_query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound(format!("账号 {target_id} 不存在")))?;
        let state = AccountState {
            id: target.id,
            username: target.username.clone().unwrap_or_default(),
            role: Role::from_db_value(&target.role),
            status: target.status.clone(),
        };
        let owners = users::orm::Entity::find()
            .filter(users::orm::Column::Role.eq("owner"))
            .filter(users::orm::Column::Status.eq("active"))
            .count(&tx)
            .await?;
        validate_mutation(actor_id, &state, &mutation, owners, 0)?;
        let changed = match &mutation {
            AccountMutation::Role(role) => *role != state.role,
            AccountMutation::Status(status) => status != &state.status,
            AccountMutation::Delete => true,
        };
        let revoke =
            changed && (!matches!(&mutation,AccountMutation::Status(status) if status=="active"));
        let mut revoked = 0;
        if revoke {
            revoked = revoke_sessions_on(&tx, target_id).await?;
            cancel_proofs_on(&tx, target_id).await?;
        }
        if changed {
            match &mutation {
                AccountMutation::Role(role) => {
                    users::orm::Entity::update_many()
                        .col_expr(users::orm::Column::Role, Expr::value(role.as_str()))
                        .col_expr(
                            users::orm::Column::AuthEpoch,
                            Expr::col(users::orm::Column::AuthEpoch).add(1),
                        )
                        .col_expr(
                            users::orm::Column::UpdatedAtMs,
                            Expr::value(crate::clock::db_now().timestamp_millis()),
                        )
                        .filter(users::orm::Column::Id.eq(target_id))
                        .exec(&tx)
                        .await?;
                }
                AccountMutation::Status(status) => {
                    users::orm::Entity::update_many()
                        .col_expr(users::orm::Column::Status, Expr::value(status))
                        .col_expr(
                            users::orm::Column::AuthEpoch,
                            Expr::col(users::orm::Column::AuthEpoch).add(1),
                        )
                        .col_expr(
                            users::orm::Column::UpdatedAtMs,
                            Expr::value(crate::clock::db_now().timestamp_millis()),
                        )
                        .filter(users::orm::Column::Id.eq(target_id))
                        .exec(&tx)
                        .await?;
                }
                AccountMutation::Delete => {
                    notifications::Entity::delete_many()
                        .filter(notifications::Column::UserId.eq(target_id))
                        .exec(&tx)
                        .await?;
                    tx.execute_raw(Statement::from_sql_and_values(
                        self.backend(),
                        "DELETE FROM nuono_global_configs WHERE key=$1",
                        [format!("admin_profile:{target_id}").into()],
                    ))
                    .await?;
                    crate::db::platform_identities::detach_all_identities_for_user_on(
                        &tx,
                        target_id,
                        crate::clock::db_now().timestamp_millis(),
                    )
                    .await?;
                    users::orm::Entity::update_many()
                        .col_expr(users::orm::Column::Username, Expr::value(None::<String>))
                        .col_expr(users::orm::Column::UsernameKey, Expr::value(None::<String>))
                        .col_expr(users::orm::Column::Email, Expr::value(None::<String>))
                        .col_expr(users::orm::Column::EmailKey, Expr::value(None::<String>))
                        .col_expr(
                            users::orm::Column::EmailVerifiedAtMs,
                            Expr::value(None::<i64>),
                        )
                        .col_expr(
                            users::orm::Column::PasswordHash,
                            Expr::value(None::<String>),
                        )
                        .col_expr(users::orm::Column::TotpSecret, Expr::value(None::<Vec<u8>>))
                        .col_expr(users::orm::Column::TotpRecoveryJson, Expr::value("[]"))
                        .col_expr(users::orm::Column::TotpEnabled, Expr::value(false))
                        .col_expr(
                            users::orm::Column::TotpLastCounter,
                            Expr::value(None::<i64>),
                        )
                        .col_expr(users::orm::Column::DisplayName, Expr::value(""))
                        .col_expr(users::orm::Column::Birthday, Expr::value(""))
                        .col_expr(users::orm::Column::Gender, Expr::value(""))
                        .col_expr(users::orm::Column::Bio, Expr::value(""))
                        .col_expr(users::orm::Column::UseForAi, Expr::value(false))
                        .col_expr(users::orm::Column::Role, Expr::value("user"))
                        .col_expr(users::orm::Column::Status, Expr::value("deleted"))
                        .col_expr(
                            users::orm::Column::AuthEpoch,
                            Expr::col(users::orm::Column::AuthEpoch).add(1),
                        )
                        .col_expr(
                            users::orm::Column::UpdatedAtMs,
                            Expr::value(crate::clock::db_now().timestamp_millis()),
                        )
                        .filter(users::orm::Column::Id.eq(target_id))
                        .exec(&tx)
                        .await?;
                }
            }
        }
        if changed {
            users::orm::Entity::update_many()
                .col_expr(
                    users::orm::Column::Revision,
                    Expr::col(users::orm::Column::Revision).add(1),
                )
                .filter(users::orm::Column::Id.eq(target_id))
                .exec(&tx)
                .await?;
            append_audit_on(
                &tx,
                AuditEvent {
                    actor_user_id: Some(actor_id),
                    actor_plugin_id: None,
                    action: match &mutation {
                        AccountMutation::Delete => "account.deleted",
                        AccountMutation::Role(_) => "account.role_changed",
                        AccountMutation::Status(_) => "account.status_changed",
                    },
                    result: "success",
                    subject_user_id: Some(target_id),
                    group_id: None,
                    operation_id: None,
                    details: serde_json::json!({"revoked_sessions":revoked.to_string()}),
                },
            )
            .await?;
        }
        tx.commit().await?;
        if changed {
            self.invalidate_identity_cache_for_user(target_id);
        }
        Ok(match mutation {
            AccountMutation::Role(role) => AccountMutationResult::Role(RoleChange {
                user_id: state.id,
                username: state.username,
                previous: state.role,
                current: role,
                revoked_sessions: revoked,
            }),
            AccountMutation::Status(status) => AccountMutationResult::Status(StatusChange {
                user_id: state.id,
                username: state.username,
                previous: state.status,
                current: status,
                revoked_sessions: revoked,
            }),
            AccountMutation::Delete => AccountMutationResult::Delete(UserDeletion {
                user_id: state.id,
                username: state.username,
                revoked_sessions: revoked,
            }),
        })
    }
}
