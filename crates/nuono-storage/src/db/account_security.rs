//! 账号安全凭据管理：令牌原子核销与两步验证（MFA）变更。所有凭据修改仅在事务提交后对外生效。
use super::Database;
#[cfg(test)]
use super::account_state;
use super::account_state::{
    AuditEvent, append_audit_on, cancel_proofs_on, revoke_sessions_on, session_hash_from_key,
    session_on, sessions,
};
#[cfg(test)]
use super::users::{hash_password, verify_password};
use crate::{
    entity::users,
    error::{NuonoError, Result},
};
use nuono_app::account_security::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter,
    QuerySelect, Set,
    sea_query::{Expr, ExprTrait},
};

impl Database {
    /// 根据不可变的账号 ID 与当前凭据世代号（`auth_epoch`）解析有效会话对应的活跃账号。
    pub async fn session_account(
        &self,
        user_id: i64,
        auth_epoch: i64,
    ) -> Result<Option<users::Model>> {
        Ok(users::orm::Entity::find_by_id(user_id)
            .one(self.orm())
            .await?
            .filter(|user| {
                user.status == "active"
                    && user.account_kind == users::ACCOUNT_KIND_REGISTERED
                    && user.password_hash.is_some()
                    && user.auth_epoch == auth_epoch
            })
            .map(Into::into))
    }

    pub async fn session_account_username(
        &self,
        user_id: i64,
        auth_epoch: i64,
    ) -> Result<Option<String>> {
        Ok(self
            .session_account(user_id, auth_epoch)
            .await?
            .map(|user| user.username))
    }

    /// 更换已认证账号的密码、撤销全部会话及写审计在同一事务提交。
    /// 旧密码在锁外校验；账号快照和本次会话在锁内复核，避免并发改密覆盖或旧凭据登录。
    /// `None` 表示认证快照或会话已经失效，调用方必须要求重新认证。
    pub async fn change_password_for_verified_user(
        &self,
        verified: &users::Model,
        session_key: &str,
        new_password: &str,
    ) -> Result<Option<u64>> {
        if !valid_session_key(session_key) {
            return Err(NuonoError::Validation("会话参数无效".into()));
        }
        let password_hash = self.password_work.hash(new_password).await?;
        let tx = self.begin_write().await?;
        let mut query = users::orm::Entity::find_by_id(verified.id);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let Some(user) = query.one(&tx).await? else {
            return Ok(None);
        };
        if !same_verified_account(&user, verified) {
            return Ok(None);
        }
        let Some(session) = session_on(&tx, session_hash_from_key(session_key)?, true).await?
        else {
            return Ok(None);
        };
        if session.user_id != user.id || session.auth_epoch != user.auth_epoch {
            return Ok(None);
        }
        let now = crate::clock::db_now();
        users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(user.id))
            .col_expr(users::orm::Column::PasswordHash, Expr::value(password_hash))
            .col_expr(
                users::orm::Column::AuthEpoch,
                Expr::col(users::orm::Column::AuthEpoch).add(1),
            )
            .col_expr(
                users::orm::Column::Revision,
                Expr::col(users::orm::Column::Revision).add(1),
            )
            .col_expr(
                users::orm::Column::UpdatedAtMs,
                Expr::value(now.timestamp_millis()),
            )
            .exec(&tx)
            .await?;
        let revoked_sessions = revoke_sessions_on(&tx, user.id).await?;
        let revoked_reset_tokens = revoke_reset_tokens(&tx, user.id).await?;
        crate::audit::insert_event_on(
            &tx,
            "account.password_changed",
            None,
            user.username.as_deref(),
            serde_json::json!({
                "revoked_all_sessions": true,
                "revoked_sessions": revoked_sessions,
                "revoked_reset_tokens": revoked_reset_tokens,
            }),
            now,
        )
        .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(user.id),
                actor_plugin_id: None,
                action: "account.password_changed",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({"revoked_sessions":revoked_sessions.to_string()}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Some(revoked_sessions))
    }

    /// 密码校验在锁外完成；这里确认认证期间账号没有变化，再原子建立会话。
    /// 与本机救援、令牌改密和 MFA 修改共用账号行锁（SQLite 为写事务）。
    pub async fn create_session_for_verified_user(
        &self,
        verified: &users::Model,
        session_key: &str,
        expires: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        if !valid_session_key(session_key) || expires <= crate::clock::db_now() {
            return Err(NuonoError::Validation("会话参数无效".into()));
        }
        let tx = self.begin_write().await?;
        let mut query = users::orm::Entity::find_by_id(verified.id);
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let Some(user) = query.one(&tx).await? else {
            return Ok(false);
        };
        // TOTP 时间步和恢复码可能已被本次登录消费，不比较这些一次性数据。
        if !same_verified_account(&user, verified) {
            return Ok(false);
        }
        let now = crate::clock::db_now();
        sessions::ActiveModel {
            id: Set(uuid::Uuid::new_v4().to_string()),
            user_id: Set(user.id),
            token_hash: Set(session_hash_from_key(session_key)?),
            auth_epoch: Set(user.auth_epoch),
            created_at_ms: Set(now.timestamp_millis()),
            expires_at_ms: Set(expires.timestamp_millis()),
            last_seen_at_ms: Set(Some(now.timestamp_millis())),
            revoked_at_ms: Set(None),
        }
        .insert(&tx)
        .await?;
        users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(user.id))
            .col_expr(
                users::orm::Column::LastLoginAtMs,
                Expr::value(now.timestamp_millis()),
            )
            .exec(&tx)
            .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(user.id),
                actor_plugin_id: None,
                action: "session.created",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 本机救援入口。调用方必须拥有服务器访问权；所有凭据变更、会话撤销与审计原子提交。
    /// 不启用已停用账号、不提升角色，也不自动关闭两步验证。
    pub async fn recover_admin_credentials(
        &self,
        username: &str,
        new_password: &str,
        disable_two_factor: bool,
    ) -> Result<()> {
        let password_hash = self.password_work.hash(new_password).await?;
        let tx = self.begin_write().await?;
        let mut query = users::orm::Entity::find()
            .filter(users::orm::Column::UsernameKey.eq(username.trim().to_ascii_lowercase()));
        if self.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let user = query
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::NotFound("找不到指定账号".into()))?;
        if !matches!(user.role.as_str(), "owner" | "admin")
            || user.status != "active"
            || user.account_kind != users::ACCOUNT_KIND_REGISTERED
        {
            return Err(NuonoError::PermissionDenied(
                "本机救援只用于已启用的 owner 或 admin 账号".into(),
            ));
        }
        let now = crate::clock::db_now();
        let mut update = users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(user.id))
            .col_expr(users::orm::Column::PasswordHash, Expr::value(password_hash))
            .col_expr(
                users::orm::Column::AuthEpoch,
                Expr::col(users::orm::Column::AuthEpoch).add(1),
            )
            .col_expr(
                users::orm::Column::Revision,
                Expr::col(users::orm::Column::Revision).add(1),
            )
            .col_expr(
                users::orm::Column::UpdatedAtMs,
                Expr::value(now.timestamp_millis()),
            );
        if disable_two_factor {
            update = update
                .col_expr(users::orm::Column::TotpEnabled, Expr::value(false))
                .col_expr(
                    users::orm::Column::TotpSecret,
                    Expr::value(Option::<Vec<u8>>::None),
                )
                .col_expr(users::orm::Column::TotpRecoveryJson, Expr::value("[]"))
                .col_expr(
                    users::orm::Column::TotpLastCounter,
                    Expr::value(Option::<i64>::None),
                );
        }
        update.exec(&tx).await?;
        let revoked_sessions = revoke_sessions_on(&tx, user.id).await?;
        let revoked_tokens = cancel_proofs_on(&tx, user.id).await?;
        crate::audit::insert_event_on(
            &tx,
            "account.local_recovery",
            None,
            Some("local-console"),
            serde_json::json!({
                "username": user.username,
                "user_id": user.id,
                "two_factor_disabled": disable_two_factor,
                "revoked_sessions": revoked_sessions,
                "revoked_tokens": revoked_tokens,
            }),
            now,
        )
        .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: None,
                actor_plugin_id: None,
                action: "account.local_recovery",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({"two_factor_disabled":disable_two_factor}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

fn valid_session_key(key: &str) -> bool {
    key.strip_prefix("auth_session_")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn same_verified_account(user: &users::orm::Model, verified: &users::Model) -> bool {
    user.status == "active"
        && user.account_kind == users::ACCOUNT_KIND_REGISTERED
        && !verified.password_hash.is_empty()
        && user.auth_epoch == verified.auth_epoch
        && user.password_hash.as_deref() == Some(verified.password_hash.as_str())
        && user.username.as_deref() == Some(verified.username.as_str())
        && user.role == verified.role
        && user.email == verified.email
        && user.email_verified_at_ms.is_some() == verified.email_verified
        && user.totp_enabled == verified.totp_enabled
        && user.totp_secret.as_deref()
            == verified.totp_secret.as_ref().map(|value| value.as_bytes())
}

#[cfg(test)]
fn test_key(key: &str) -> String {
    if valid_session_key(key) {
        key.into()
    } else {
        format!(
            "auth_session_{}",
            hex::encode(account_state::token_hash(key))
        )
    }
}
#[cfg(test)]
async fn test_session(db: &Database, key: &str) -> Result<Option<sessions::Model>> {
    session_on(db.orm(), session_hash_from_key(&test_key(key))?, false).await
}
#[cfg(test)]
async fn test_issue_session(db: &Database, key: &str, username: &str) {
    let user = db.find_user_by_username(username).await.unwrap().unwrap();
    assert!(
        db.create_session_for_verified_user(
            &user,
            &test_key(key),
            chrono::Utc::now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap()
    );
}

#[cfg(test)]
#[path = "../../tests/unit/db/account_password_tests.rs"]
mod password_tests;

#[cfg(test)]
#[path = "../../tests/unit/db/password_reset_tests.rs"]
mod password_reset_tests;

#[async_trait::async_trait]
impl AccountSecurityStore for Database {
    async fn redeem_account_token(
        &self,
        token: &str,
        operation: TokenOperation,
    ) -> Result<Option<TokenRedemption>> {
        self.redeem_challenge_token(token, operation).await
    }
    async fn change_account_mfa(&self, user_id: i64, operation: MfaOperation) -> Result<MfaResult> {
        // 密码哈希校验在写事务外执行；拿到账号行锁后会复核密码哈希与 auth_epoch，防止并发密码重置绕过本次校验。
        let verified_credentials = if let Some(password) = operation.password() {
            let user = users::orm::Entity::find_by_id(user_id)
                .one(self.orm())
                .await?
                .ok_or_else(|| NuonoError::PermissionDenied("账号不存在".into()))?;
            if user.status != "active"
                || user.account_kind != users::ACCOUNT_KIND_REGISTERED
                || user.password_hash.is_none()
            {
                return Err(NuonoError::PermissionDenied("账号不可用".into()));
            }
            if !self
                .password_work
                .verify(password, user.password_hash.as_deref().unwrap_or_default())
                .await?
            {
                return Err(NuonoError::PermissionDenied("当前密码不正确".into()));
            }
            Some((user.password_hash, user.auth_epoch))
        } else {
            None
        };
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
            || user.account_kind != users::ACCOUNT_KIND_REGISTERED
            || user.password_hash.is_none()
        {
            return Err(NuonoError::PermissionDenied("账号不可用".into()));
        }
        if verified_credentials
            .as_ref()
            .is_some_and(|(hash, epoch)| hash != &user.password_hash || *epoch != user.auth_epoch)
        {
            return Err(NuonoError::PermissionDenied("当前密码不正确".into()));
        }
        let mut plan = plan_mfa(
            MfaState {
                enabled: user.totp_enabled,
                secret: user
                    .totp_secret
                    .map(String::from_utf8)
                    .transpose()
                    .map_err(|_| NuonoError::Validation("两步验证密钥无效".into()))?,
                last_counter: user.totp_last_counter,
            },
            &operation,
            chrono::Utc::now().timestamp() as u64,
            user.username.as_deref().unwrap_or_default(),
        )?;
        let mut update = users::orm::Entity::update_many()
            .col_expr(
                users::orm::Column::Revision,
                Expr::col(users::orm::Column::Revision).add(1),
            )
            .col_expr(users::orm::Column::TotpEnabled, Expr::value(plan.enabled))
            .col_expr(
                users::orm::Column::TotpSecret,
                Expr::value(plan.secret.map(String::into_bytes)),
            )
            .col_expr(
                users::orm::Column::TotpLastCounter,
                Expr::value(plan.counter),
            )
            .col_expr(
                users::orm::Column::UpdatedAtMs,
                Expr::value(crate::clock::db_now().timestamp_millis()),
            )
            .filter(users::orm::Column::Id.eq(user.id));
        // 绑定阶段（Enroll）在扫码确认前仅处于草稿状态，不递增 auth_epoch，以保留当前登录会话供第二步确认使用。
        if !matches!(&operation, MfaOperation::Enroll { .. }) {
            update = update.col_expr(
                users::orm::Column::AuthEpoch,
                Expr::col(users::orm::Column::AuthEpoch).add(1),
            );
        }
        if let Some(hashes) = plan.recovery_hashes {
            update = update.col_expr(
                users::orm::Column::TotpRecoveryJson,
                Expr::value(serde_json::to_string(&hashes)?),
            );
        } else {
            update = update.col_expr(users::orm::Column::TotpRecoveryJson, Expr::value("[]"));
        }
        update.exec(&tx).await?;
        if !matches!(&operation, MfaOperation::Enroll { .. }) {
            revoke_sessions_on(&tx, user.id).await?;
            cancel_proofs_on(&tx, user.id).await?;
        }
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(user.id),
                actor_plugin_id: None,
                action: "account.mfa_changed",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: None,
                details: serde_json::json!({"enabled": plan.enabled}),
            },
        )
        .await?;
        tx.commit().await?;
        plan.result.user_id = user.id;
        Ok(plan.result)
    }
}
async fn revoke_reset_tokens(tx: &impl ConnectionTrait, user_id: i64) -> Result<u64> {
    cancel_proofs_on(tx, user_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{NewUser, TOKEN_CHANGE_EMAIL, TOKEN_RESET_PASSWORD},
        entity::audit_events,
        test_support::test_db,
    };
    use sea_orm::{ActiveValue::Set, Statement};

    async fn account(db: &Database, name: &str) -> crate::entity::users::Model {
        db.create_user(NewUser {
            username: name.into(),
            email: Some(format!("{name}@example.com")),
            password: "Original-password-123!".into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn reset_is_single_use_and_revokes_only_its_users_sessions() {
        let db = test_db(4).await;
        let user = account(&db, "reset-user").await;
        account(&db, "other").await;
        test_issue_session(&db, "auth_session_target", "reset-user").await;
        test_issue_session(&db, "auth_session_other", "other").await;
        let token = db
            .issue_user_token(user.id, TOKEN_RESET_PASSWORD, 30, None)
            .await
            .unwrap()
            .token;
        let (first, second) = tokio::join!(
            db.redeem_account_token(
                &token,
                TokenOperation::ResetPassword {
                    password: "New-password-123!".into()
                }
            ),
            db.redeem_account_token(
                &token,
                TokenOperation::ResetPassword {
                    password: "New-password-123!".into()
                }
            )
        );
        let results = [first.unwrap(), second.unwrap()];
        assert_eq!(results.iter().filter(|r| r.is_some()).count(), 1);
        assert_eq!(
            results
                .into_iter()
                .flatten()
                .next()
                .unwrap()
                .revoked_sessions,
            1
        );
        assert!(
            test_session(&db, "auth_session_target")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            test_session(&db, "auth_session_other")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.verify_user_password("reset-user", "New-password-123!")
                .await
                .unwrap()
                .is_some()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn failed_session_revocation_rolls_back_password_and_token() {
        let db = test_db(2).await;
        let user = account(&db, "rollback-user").await;
        let token = db
            .issue_user_token(user.id, TOKEN_RESET_PASSWORD, 30, None)
            .await
            .unwrap()
            .token;
        db.orm()
            .execute_raw(Statement::from_string(
                db.backend(),
                "ALTER TABLE nuono_sessions RENAME TO unavailable_sessions",
            ))
            .await
            .unwrap();
        assert!(
            db.redeem_account_token(
                &token,
                TokenOperation::ResetPassword {
                    password: "New-password-123!".into()
                }
            )
            .await
            .is_err()
        );
        assert!(
            db.verify_user_password("rollback-user", "Original-password-123!")
                .await
                .unwrap()
                .is_some()
        );
        db.orm()
            .execute_raw(Statement::from_string(
                db.backend(),
                "ALTER TABLE unavailable_sessions RENAME TO nuono_sessions",
            ))
            .await
            .unwrap();
        assert!(
            db.redeem_account_token(
                &token,
                TokenOperation::ResetPassword {
                    password: "New-password-123!".into()
                }
            )
            .await
            .unwrap()
            .is_some()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn email_conflict_does_not_consume_the_confirmation_link() {
        let db = test_db(2).await;
        let first = account(&db, "first-user").await;
        let second = account(&db, "second-user").await;
        let token = db
            .issue_user_token(
                first.id,
                TOKEN_CHANGE_EMAIL,
                30,
                Some("second-user@example.com"),
            )
            .await
            .unwrap()
            .token;
        assert!(matches!(
            db.redeem_account_token(&token, TokenOperation::ChangeEmail)
                .await,
            Err(NuonoError::Conflict(_))
        ));
        users::orm::Entity::update(users::orm::ActiveModel {
            id: Set(second.id),
            email: Set(Some("available@example.com".into())),
            email_key: Set(Some("available@example.com".into())),
            ..Default::default()
        })
        .exec(db.orm())
        .await
        .unwrap();
        assert!(
            db.redeem_account_token(&token, TokenOperation::ChangeEmail)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            db.find_user_by_id(first.id)
                .await
                .unwrap()
                .unwrap()
                .email
                .as_deref(),
            Some("second-user@example.com")
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn mfa_enrollment_keeps_the_session_for_its_confirmation_step() {
        let db = test_db(2).await;
        let user = account(&db, "mfa_draft").await;
        let result = db
            .change_account_mfa(
                user.id,
                MfaOperation::Enroll {
                    password: "Original-password-123!".into(),
                },
            )
            .await
            .unwrap();
        assert!(result.secret.is_some());
        assert!(db.session_account(user.id, 1).await.unwrap().is_some());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn mfa_rechecks_current_password_and_clears_all_credentials_on_disable() {
        let db = test_db(2).await;
        let user = account(&db, "mfa-user").await;
        db.set_totp_secret(user.id, Some("JBSWY3DPEHPK3PXP"))
            .await
            .unwrap();
        db.set_totp_state(user.id, true, Some(&["old-hash".into()]))
            .await
            .unwrap();
        assert!(
            db.change_account_mfa(
                user.id,
                MfaOperation::Disable {
                    password: "Wrong-password-123!".into()
                }
            )
            .await
            .is_err()
        );
        assert!(
            db.find_user_by_id(user.id)
                .await
                .unwrap()
                .unwrap()
                .totp_enabled
        );
        db.change_account_mfa(
            user.id,
            MfaOperation::Disable {
                password: "Original-password-123!".into(),
            },
        )
        .await
        .unwrap();
        let row = users::orm::Entity::find_by_id(user.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(!row.totp_enabled);
        assert!(
            row.totp_secret.is_none()
                && row.totp_recovery_json == "[]"
                && row.totp_last_counter.is_none()
        );
        db.shutdown().await.unwrap();
    }
    async fn make_admin(db: &Database, name: &str) -> users::Model {
        let user = account(db, name).await;
        users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(user.id))
            .col_expr(users::orm::Column::Role, Expr::value("owner"))
            .col_expr(users::orm::Column::TotpEnabled, Expr::value(true))
            .col_expr(
                users::orm::Column::TotpSecret,
                Expr::value(b"test-secret".to_vec()),
            )
            .col_expr(
                users::orm::Column::TotpRecoveryJson,
                Expr::value("[\"hash\"]"),
            )
            .col_expr(users::orm::Column::TotpLastCounter, Expr::value(123_i64))
            .exec(db.orm())
            .await
            .unwrap();
        user
    }

    fn session_key(byte: u8) -> String {
        format!("auth_session_{}", hex::encode([byte; 32]))
    }

    #[tokio::test]
    async fn login_session_and_local_recovery_serialize_credentials() {
        let db = test_db(3).await;
        make_admin(&db, "login-race").await;
        let verified = db
            .verify_user_password("login-race", "Original-password-123!")
            .await
            .unwrap()
            .unwrap();
        let key = session_key(1);
        let expires = chrono::Utc::now() + chrono::Duration::hours(1);
        let (created, recovered) = tokio::join!(
            db.create_session_for_verified_user(&verified, &key, expires),
            db.recover_admin_credentials("login-race", "Rescued-password-456!", true),
        );
        created.unwrap();
        recovered.unwrap();
        // 无论谁先拿到锁，救援完成后不能留下一条用旧凭据创建的会话。
        assert!(test_session(&db, &key).await.unwrap().is_none());
        assert!(
            !db.create_session_for_verified_user(&verified, &session_key(2), expires)
                .await
                .unwrap()
        );
        let current = db
            .verify_user_password("login-race", "Rescued-password-456!")
            .await
            .unwrap()
            .unwrap();
        assert!(
            db.create_session_for_verified_user(&current, &session_key(3), expires)
                .await
                .unwrap()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn login_session_waits_for_account_lock_and_rechecks_snapshot() {
        let db = test_db(3).await;
        let verified = account(&db, "locked-login").await;
        let new_hash = hash_password("Changed-password-456!").unwrap();
        let tx = db.begin_write().await.unwrap();
        let mut query = users::orm::Entity::find_by_id(verified.id);
        if db.backend() == DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        query.one(&tx).await.unwrap().unwrap();
        users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(verified.id))
            .col_expr(users::orm::Column::PasswordHash, Expr::value(new_hash))
            .exec(&tx)
            .await
            .unwrap();
        let connection = db.clone();
        let pending = tokio::spawn(async move {
            connection
                .create_session_for_verified_user(
                    &verified,
                    &session_key(4),
                    chrono::Utc::now() + chrono::Duration::hours(1),
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "创建会话必须等账号凭据事务结束");
        tx.commit().await.unwrap();
        assert!(!pending.await.unwrap().unwrap());
        assert!(test_session(&db, &session_key(4)).await.unwrap().is_none());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn login_session_rejects_changed_mfa_snapshot() {
        let db = test_db(2).await;
        let verified = account(&db, "mfa-login").await;
        let expires = chrono::Utc::now() + chrono::Duration::hours(1);
        db.set_totp_secret(verified.id, Some("FIRST-SECRET"))
            .await
            .unwrap();
        db.set_totp_state(verified.id, true, None).await.unwrap();
        assert!(
            !db.create_session_for_verified_user(&verified, &session_key(5), expires)
                .await
                .unwrap()
        );
        let with_mfa = db.find_user_by_id(verified.id).await.unwrap().unwrap();
        db.set_totp_secret(verified.id, Some("REPLACEMENT-SECRET"))
            .await
            .unwrap();
        assert!(
            !db.create_session_for_verified_user(&with_mfa, &session_key(6), expires)
                .await
                .unwrap()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn local_recovery_keeps_mfa_by_default_and_revokes_only_target_credentials() {
        let db = test_db(2).await;
        let admin = make_admin(&db, "recover-owner").await;
        let other = account(&db, "untouched").await;
        for name in [&admin.username, &other.username] {
            test_issue_session(&db, &format!("auth_session_{name}"), name).await;
        }
        let token = db
            .issue_user_token(admin.id, TOKEN_CHANGE_EMAIL, 30, Some("new@example.com"))
            .await
            .unwrap();
        let other_token = db
            .issue_user_token(other.id, TOKEN_RESET_PASSWORD, 30, None)
            .await
            .unwrap();
        db.recover_admin_credentials(&admin.username, "Rescued-password-456!", false)
            .await
            .unwrap();
        assert!(
            db.verify_user_password(&admin.username, "Rescued-password-456!")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            test_session(&db, "auth_session_recover-owner")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            test_session(&db, "auth_session_untouched")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.redeem_account_token(&token.token, TokenOperation::ChangeEmail)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.redeem_account_token(
                &other_token.token,
                TokenOperation::ResetPassword {
                    password: "Unrelated-password-456!".into()
                }
            )
            .await
            .unwrap()
            .is_some()
        );
        let row = users::orm::Entity::find_by_id(admin.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(row.totp_enabled && row.totp_secret.is_some() && row.totp_recovery_json != "[]");
        let audit = audit_events::Entity::find()
            .filter(audit_events::Column::Category.eq(crate::audit::CATEGORY_EVENT))
            .filter(audit_events::Column::Action.eq("account.local_recovery"))
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let data = audit.details_json;
        assert!(data.contains("recover-owner"));
        assert!(!data.contains("Rescued-password") && !data.contains("test-secret"));
        db.recover_admin_credentials(&admin.username, "Final-password-456!", true)
            .await
            .unwrap();
        let row = users::orm::Entity::find_by_id(admin.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !row.totp_enabled
                && row.totp_secret.is_none()
                && row.totp_recovery_json == "[]"
                && row.totp_last_counter.is_none()
        );
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn local_recovery_rejects_non_admin_and_inactive_accounts() {
        let db = test_db(2).await;
        let user = account(&db, "regular-account").await;
        assert!(
            db.recover_admin_credentials(&user.username, "Rescued-password-456!", true)
                .await
                .is_err()
        );
        let admin = make_admin(&db, "inactive-owner").await;
        users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(admin.id))
            .col_expr(users::orm::Column::Status, Expr::value("disabled"))
            .exec(db.orm())
            .await
            .unwrap();
        assert!(
            db.recover_admin_credentials(&admin.username, "Rescued-password-456!", true)
                .await
                .is_err()
        );
        assert!(
            db.recover_admin_credentials("missing", "Rescued-password-456!", true)
                .await
                .is_err()
        );
        let row = users::orm::Entity::find_by_id(admin.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(verify_password(
            "Original-password-123!",
            row.password_hash.as_deref().unwrap_or_default()
        ));
        assert!(row.totp_enabled);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn local_recovery_audit_failure_rolls_back_every_change() {
        let db = test_db(2).await;
        let admin = make_admin(&db, "atomic-owner").await;
        test_issue_session(&db, "auth_session_atomic", &admin.username).await;
        let token = db
            .issue_user_token(admin.id, TOKEN_RESET_PASSWORD, 30, None)
            .await
            .unwrap();
        db.orm()
            .execute_raw(Statement::from_string(
                db.backend(),
                "ALTER TABLE nuono_audit_events RENAME TO unavailable_audit",
            ))
            .await
            .unwrap();
        assert!(
            db.recover_admin_credentials(&admin.username, "Rescued-password-456!", true)
                .await
                .is_err()
        );
        let row = users::orm::Entity::find_by_id(admin.id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        assert!(verify_password(
            "Original-password-123!",
            row.password_hash.as_deref().unwrap_or_default()
        ));
        assert!(row.totp_enabled && row.totp_last_counter == Some(123));
        assert!(
            test_session(&db, "auth_session_atomic")
                .await
                .unwrap()
                .is_some()
        );
        db.orm()
            .execute_raw(Statement::from_string(
                db.backend(),
                "ALTER TABLE unavailable_audit RENAME TO nuono_audit_events",
            ))
            .await
            .unwrap();
        assert!(
            db.redeem_account_token(
                &token.token,
                TokenOperation::ResetPassword {
                    password: "Original-password-123!".into()
                }
            )
            .await
            .unwrap()
            .is_some()
        );
        db.shutdown().await.unwrap();
    }
}
