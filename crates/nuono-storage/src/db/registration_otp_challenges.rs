//! 公开邮箱注册的 OTP 验证码挑战流程：在正确的邮箱验证码被核销之前，绝不提前在 `users` 表中分配账号记录。
use super::{
    Database,
    account_state::{AuditEvent, append_audit_on, challenges},
    users::{insert_user, new_user_active_model},
};
use crate::error::{NuonoError, Result};
use hmac::{Hmac, KeyInit, Mac};
use nuono_app::account_registration::RegistrationReceipt;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    Statement,
    sea_query::{Expr, ExprTrait},
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
pub const REGISTRATION_OTP_LENGTH: usize = 8;
pub const REGISTRATION_OTP_MAX_ATTEMPTS: i64 = 5;
pub const REGISTRATION_OTP_TTL_MINUTES: i64 = 10;
const REGISTRATION_LOCK: i64 = 0x4e554f524547;
#[derive(Debug, Clone)]
pub struct IssuedRegistrationOtp {
    pub id: String,
    pub code: String,
    pub expires_at: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOtpIssueOutcome {
    Issued,
    Cooldown,
    HourlyLimit,
    DailyLimit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOtpVerification {
    Verified { user_id: i64 },
    Invalid,
}
#[derive(Serialize, Deserialize)]
struct Context {
    username: String,
    username_key: String,
    email: String,
    email_key: String,
    password_hash: String,
    sends: Vec<i64>,
}

async fn lock(tx: &sea_orm::DatabaseTransaction) -> Result<()> {
    if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
        tx.execute_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "SELECT pg_advisory_xact_lock($1)",
            [REGISTRATION_LOCK.into()],
        ))
        .await?;
    }
    Ok(())
}
// 公开自助注册绝不能抢建系统的首个 root（owner）账号；
// 在 PostgreSQL 下对活跃 owner 行加共享锁，防止并发停用或降级最后一个 owner 导致初始化状态倒退。
async fn require_initialized(tx: &impl ConnectionTrait) -> Result<()> {
    let mut query = crate::entity::users::orm::Entity::find()
        .filter(crate::entity::users::orm::Column::Role.eq("owner"))
        .filter(crate::entity::users::orm::Column::Status.eq("active"))
        .order_by_asc(crate::entity::users::orm::Column::Id);
    if tx.get_database_backend() == sea_orm::DbBackend::Postgres {
        query = query.lock_shared();
    }
    if query.one(tx).await?.is_none() {
        return Err(NuonoError::PermissionDenied(
            "请先完成 root 账号初始化".into(),
        ));
    }
    Ok(())
}
fn email_query(backend: sea_orm::DbBackend) -> String {
    let email = if backend == sea_orm::DbBackend::Postgres {
        "context_json::jsonb->>'email_key'"
    } else {
        "json_extract(context_json,'$.email_key')"
    };
    format!(
        "SELECT id FROM nuono_account_challenges WHERE purpose='registration' AND state='pending' AND {email}=$1 ORDER BY created_at_ms DESC LIMIT 1"
    )
}
async fn pending_on(tx: &impl ConnectionTrait, email: &str) -> Result<Option<challenges::Model>> {
    let id = tx
        .query_one_raw(Statement::from_sql_and_values(
            tx.get_database_backend(),
            email_query(tx.get_database_backend()),
            [email.into()],
        ))
        .await?
        .map(|row| row.try_get::<String>("", "id"))
        .transpose()?;
    if let Some(id) = id {
        Ok(challenges::Entity::find_by_id(id).one(tx).await?)
    } else {
        Ok(None)
    }
}
impl Database {
    pub async fn prepare_registration(
        &self,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<RegistrationReceipt> {
        let username = username.trim();
        let email = email.trim().to_ascii_lowercase();
        nuono_app::auth::validate_username(username).map_err(NuonoError::Validation)?;
        nuono_app::auth::validate_email(&email).map_err(NuonoError::Validation)?;
        nuono_app::auth::validate_password(password).map_err(NuonoError::Validation)?;
        require_initialized(self.orm()).await?;
        let password_hash = self.password_work.hash(password).await?;
        let tx = self.begin_write().await?;
        lock(&tx).await?;
        require_initialized(&tx).await?;
        let email = email.as_str();
        let now = crate::clock::db_now().timestamp_millis();
        // 每次提交注册密码都生成一条全新的挑战记录并作废同邮箱的旧待验证请求（但继承 24 小时内的发信计数）：
        // 若直接复用旧的匿名注册请求而不更新密码，攻击者就能用自己预设的密码抢先登记他人邮箱。
        let email_field = if self.backend() == sea_orm::DbBackend::Postgres {
            "context_json::jsonb->>'email_key'"
        } else {
            "json_extract(context_json,'$.email_key')"
        };
        let history=tx.query_all_raw(Statement::from_sql_and_values(self.backend(),format!("SELECT context_json FROM nuono_account_challenges WHERE purpose='registration' AND {email_field}=$1 AND created_at_ms>$2"),[email.into(),(now-86_400_000).into()])).await?;
        let mut sends = Vec::new();
        for row in history {
            let context: Context =
                serde_json::from_str(&row.try_get::<String>("", "context_json")?)?;
            sends.extend(context.sends);
        }
        sends.sort_unstable();
        sends.dedup();
        sends.retain(|sent| *sent > now - 86_400_000);
        if let Some(existing) = pending_on(&tx, email).await? {
            challenges::Entity::update_many()
                .col_expr(challenges::Column::State, Expr::value("cancelled"))
                .col_expr(
                    challenges::Column::Revision,
                    Expr::col(challenges::Column::Revision).add(1),
                )
                .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
                .filter(challenges::Column::Id.eq(existing.id))
                .exec(&tx)
                .await?;
        }
        let occupied = crate::entity::users::orm::Entity::find()
            .filter(
                sea_orm::Condition::any()
                    .add(
                        crate::entity::users::orm::Column::UsernameKey
                            .eq(username.to_ascii_lowercase()),
                    )
                    .add(crate::entity::users::orm::Column::EmailKey.eq(email)),
            )
            .one(&tx)
            .await?;
        if occupied.is_some() {
            return Err(NuonoError::Conflict("用户名或邮箱已被使用".into()));
        }
        let context = Context {
            username: username.into(),
            username_key: username.to_ascii_lowercase(),
            email: email.into(),
            email_key: email.into(),
            password_hash,
            sends,
        };
        let id = uuid::Uuid::new_v4().to_string();
        challenges::Entity::insert(challenges::ActiveModel {
            id: Set(id.clone()),
            purpose: Set("registration".into()),
            requester_user_id: Set(None),
            subject_user_id: Set(None),
            subject_auth_epoch: Set(None),
            session_id: Set(None),
            token_hash: Set(super::account_state::token_hash(
                &nuono_app::auth::generate_secret(),
            )),
            context_json: Set(serde_json::to_string(&context)?),
            candidate_identity_id: Set(None),
            state: Set("pending".into()),
            attempts: Set(0),
            expires_at_ms: Set(now + 600_000),
            consumed_at_ms: Set(None),
            revision: Set(1),
            created_at_ms: Set(now),
            updated_at_ms: Set(now),
        })
        .exec(&tx)
        .await?;
        tx.commit().await?;
        Ok(RegistrationReceipt {
            request_id: id,
            created: true,
        })
    }
    pub async fn pending_registration_id(&self, email: &str) -> Result<Option<String>> {
        require_initialized(self.orm()).await?;
        Ok(pending_on(self.orm(), &email.trim().to_ascii_lowercase())
            .await?
            .map(|row| row.id))
    }
    pub async fn issue_registration_otp(
        &self,
        request_id: &str,
        _ttl_minutes: i64,
        pepper: &str,
    ) -> Result<(RegistrationOtpIssueOutcome, Option<IssuedRegistrationOtp>)> {
        if pepper.is_empty() {
            return Err(NuonoError::Internal("注册验证码 pepper 未配置".into()));
        }
        let tx = self.begin_write().await?;
        lock(&tx).await?;
        require_initialized(&tx).await?;
        let row = challenges::Entity::find_by_id(request_id)
            .filter(challenges::Column::Purpose.eq("registration"))
            .filter(challenges::Column::State.eq("pending"))
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::Validation("注册请求无效".into()))?;
        let mut context: Context = serde_json::from_str(&row.context_json)?;
        let now = crate::clock::db_now().timestamp_millis();
        if row.expires_at_ms <= now {
            return Err(NuonoError::Validation(
                "注册请求已过期，请重新提交注册".into(),
            ));
        }
        context.sends.retain(|sent| *sent > now - 86_400_000);
        let outcome = if context.sends.len() >= 10 {
            RegistrationOtpIssueOutcome::DailyLimit
        } else if context
            .sends
            .iter()
            .filter(|sent| **sent > now - 3_600_000)
            .count()
            >= 5
        {
            RegistrationOtpIssueOutcome::HourlyLimit
        } else if context
            .sends
            .last()
            .is_some_and(|sent| *sent > now - 60_000)
        {
            RegistrationOtpIssueOutcome::Cooldown
        } else {
            RegistrationOtpIssueOutcome::Issued
        };
        if outcome != RegistrationOtpIssueOutcome::Issued {
            tx.commit().await?;
            return Ok((outcome, None));
        }
        let code = format!("{:08}", rand::random_range(0u32..100_000_000));
        context.sends.push(now);
        let expires = now + REGISTRATION_OTP_TTL_MINUTES * 60_000;
        challenges::Entity::update_many()
            .col_expr(
                challenges::Column::TokenHash,
                Expr::value(verifier(request_id, &code, pepper)),
            )
            .col_expr(
                challenges::Column::ContextJson,
                Expr::value(serde_json::to_string(&context)?),
            )
            .col_expr(challenges::Column::ExpiresAtMs, Expr::value(expires))
            .col_expr(challenges::Column::Attempts, Expr::value(0i64))
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .filter(challenges::Column::Id.eq(request_id))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        // 将递增后的版本号（revision）一并编入投递句柄，防止前一次迟到的 SMTP 发信失败回滚误撤销后续新发出的验证码。
        Ok((
            outcome,
            Some(IssuedRegistrationOtp {
                id: format!("{}:{}", request_id, row.revision + 1),
                code,
                expires_at: expires.to_string(),
            }),
        ))
    }
    pub async fn revoke_registration_otp(&self, handle: &str) -> Result<bool> {
        let Some((id, revision)) = handle.rsplit_once(':') else {
            return Ok(false);
        };
        let Ok(revision) = revision.parse::<i64>() else {
            return Ok(false);
        };
        Ok(challenges::Entity::update_many()
            .col_expr(
                challenges::Column::TokenHash,
                Expr::value(super::account_state::token_hash(
                    &nuono_app::auth::generate_secret(),
                )),
            )
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .filter(challenges::Column::Id.eq(id))
            .filter(challenges::Column::Revision.eq(revision))
            .filter(challenges::Column::State.eq("pending"))
            .exec(self.orm())
            .await?
            .rows_affected
            == 1)
    }
    pub async fn verify_registration_otp(
        &self,
        email: &str,
        code: &str,
        pepper: &str,
    ) -> Result<RegistrationOtpVerification> {
        if pepper.is_empty() || code.len() != 8 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(RegistrationOtpVerification::Invalid);
        }
        let tx = self.begin_write().await?;
        lock(&tx).await?;
        require_initialized(&tx).await?;
        let Some(row) = pending_on(&tx, &email.trim().to_ascii_lowercase()).await? else {
            return Ok(RegistrationOtpVerification::Invalid);
        };
        let now = crate::clock::db_now().timestamp_millis();
        if row.expires_at_ms <= now || row.attempts >= REGISTRATION_OTP_MAX_ATTEMPTS {
            return Ok(RegistrationOtpVerification::Invalid);
        }
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(pepper.as_bytes())
            .map_err(|_| NuonoError::Internal("验证码密钥无效".into()))?;
        mac.update(row.id.as_bytes());
        mac.update(b"\0");
        mac.update(code.as_bytes());
        if mac.verify_slice(&row.token_hash).is_err() {
            challenges::Entity::update_many()
                .col_expr(
                    challenges::Column::Attempts,
                    Expr::col(challenges::Column::Attempts).add(1),
                )
                .col_expr(
                    challenges::Column::Revision,
                    Expr::col(challenges::Column::Revision).add(1),
                )
                .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
                .filter(challenges::Column::Id.eq(&row.id))
                .exec(&tx)
                .await?;
            tx.commit().await?;
            return Ok(RegistrationOtpVerification::Invalid);
        }
        let context: Context = serde_json::from_str(&row.context_json)?;
        let user = insert_user(
            &tx,
            new_user_active_model(
                &context.username,
                Some(&context.email),
                true,
                &context.password_hash,
                "user",
                "active",
                crate::clock::db_now(),
            ),
        )
        .await?;
        challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("consumed"))
            .col_expr(challenges::Column::ConsumedAtMs, Expr::value(now))
            .col_expr(challenges::Column::ContextJson, Expr::value("{}"))
            .col_expr(challenges::Column::SubjectUserId, Expr::value(user.id))
            .col_expr(
                challenges::Column::SubjectAuthEpoch,
                Expr::value(user.auth_epoch),
            )
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .filter(challenges::Column::Id.eq(&row.id))
            .exec(&tx)
            .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(user.id),
                actor_plugin_id: None,
                action: "account.registered",
                result: "success",
                subject_user_id: Some(user.id),
                group_id: None,
                operation_id: Some(&row.id),
                details: serde_json::json!({}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(RegistrationOtpVerification::Verified { user_id: user.id })
    }
    pub async fn delete_stale_registration_otps(&self) -> Result<u64> {
        let now = crate::clock::db_now().timestamp_millis();
        Ok(challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("expired"))
            .col_expr(challenges::Column::ContextJson, Expr::value("{}"))
            .filter(challenges::Column::Purpose.eq("registration"))
            .filter(challenges::Column::State.eq("pending"))
            .filter(challenges::Column::ExpiresAtMs.lt(now - 86_400_000))
            .exec(self.orm())
            .await?
            .rows_affected)
    }
}
fn verifier(id: &str, code: &str, pepper: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(pepper.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(id.as_bytes());
    mac.update(b"\0");
    mac.update(code.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) const PEPPER: &str = "fixture-only-pepper";
    pub(super) async fn initialized_db(connections: u32) -> Database {
        let db = crate::test_support::test_db(connections).await;
        let owner = db
            .create_first_owner(super::super::NewUser {
                username: "root".into(),
                email: None,
                email_verified: false,
                password: "Root-password-123!".into(),
                role: "owner".into(),
                status: "active".into(),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.id, 1);
        db
    }
    #[tokio::test]
    async fn account_id_is_allocated_only_after_one_successful_mailbox_proof() {
        let db = initialized_db(3).await;
        let request = db
            .prepare_registration(
                "registered-user",
                "user@example.com",
                "Chosen-password-123!",
            )
            .await
            .unwrap();
        assert_eq!(db.user_count().await.unwrap(), 1);
        let (_, code) = db
            .issue_registration_otp(&request.request_id, 10, PEPPER)
            .await
            .unwrap();
        let code = code.unwrap();
        let (a, b) = tokio::join!(
            db.verify_registration_otp("user@example.com", &code.code, PEPPER),
            db.verify_registration_otp("user@example.com", &code.code, PEPPER)
        );
        let results = [a.unwrap(), b.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, RegistrationOtpVerification::Verified { .. }))
                .count(),
            1
        );
        assert_eq!(db.user_count().await.unwrap(), 2);
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn resubmission_does_not_preserve_an_attacker_password_or_reset_mail_quota() {
        let db = initialized_db(2).await;
        let first = db
            .prepare_registration(
                "mailbox-user",
                "proof@example.com",
                "Attacker-password-123!",
            )
            .await
            .unwrap();
        let (_, old) = db
            .issue_registration_otp(&first.request_id, 10, PEPPER)
            .await
            .unwrap();
        let old = old.unwrap();
        let second = db
            .prepare_registration(
                "mailbox-user",
                "proof@example.com",
                "Recipient-password-123!",
            )
            .await
            .unwrap();
        assert_ne!(first.request_id, second.request_id);
        assert_eq!(
            db.verify_registration_otp("proof@example.com", &old.code, PEPPER)
                .await
                .unwrap(),
            RegistrationOtpVerification::Invalid
        );
        assert_eq!(
            db.issue_registration_otp(&second.request_id, 10, PEPPER)
                .await
                .unwrap()
                .0,
            RegistrationOtpIssueOutcome::Cooldown
        );
        let row = challenges::Entity::find_by_id(&second.request_id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let mut context: Context = serde_json::from_str(&row.context_json).unwrap();
        context.sends = vec![crate::clock::db_now().timestamp_millis() - 61_000];
        challenges::Entity::update_many()
            .col_expr(
                challenges::Column::ContextJson,
                Expr::value(serde_json::to_string(&context).unwrap()),
            )
            .filter(challenges::Column::Id.eq(&row.id))
            .exec(db.orm())
            .await
            .unwrap();
        let (_, code) = db
            .issue_registration_otp(&second.request_id, 10, PEPPER)
            .await
            .unwrap();
        let code = code.unwrap();
        assert!(matches!(
            db.verify_registration_otp("proof@example.com", &code.code, PEPPER)
                .await
                .unwrap(),
            RegistrationOtpVerification::Verified { .. }
        ));
        let user = db
            .find_user_by_username("mailbox-user")
            .await
            .unwrap()
            .unwrap();
        assert!(
            db.password_work
                .verify("Recipient-password-123!", &user.password_hash)
                .await
                .unwrap()
        );
        assert!(
            !db.password_work
                .verify("Attacker-password-123!", &user.password_hash)
                .await
                .unwrap()
        );
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn five_wrong_attempts_lock_the_proof() {
        let db = initialized_db(2).await;
        let request = db
            .prepare_registration(
                "attempt-user",
                "attempt@example.com",
                "Fixture-password-123!",
            )
            .await
            .unwrap();
        let (_, code) = db
            .issue_registration_otp(&request.request_id, 10, PEPPER)
            .await
            .unwrap();
        let code = code.unwrap();
        let wrong = if code.code == "00000000" {
            "11111111"
        } else {
            "00000000"
        };
        for _ in 0..5 {
            assert_eq!(
                db.verify_registration_otp("attempt@example.com", wrong, PEPPER)
                    .await
                    .unwrap(),
                RegistrationOtpVerification::Invalid
            );
        }
        assert_eq!(
            db.verify_registration_otp("attempt@example.com", &code.code, PEPPER)
                .await
                .unwrap(),
            RegistrationOtpVerification::Invalid
        );
        assert!(!db.revoke_registration_otp(&code.id).await.unwrap());
        assert_eq!(db.user_count().await.unwrap(), 1);
        db.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::tests::{PEPPER, initialized_db};
    use super::*;
    #[tokio::test]
    async fn public_registration_cannot_take_the_root_identifier() {
        let db = crate::test_support::test_db(2).await;
        assert!(
            db.prepare_registration("early-user", "early@example.com", "Fixture-password-123!")
                .await
                .is_err()
        );
        assert!(
            db.verify_registration_otp("early@example.com", "12345678", PEPPER)
                .await
                .is_err()
        );
        assert_eq!(db.user_count().await.unwrap(), 0);
        let root = db
            .create_first_owner(super::super::NewUser {
                username: "root".into(),
                email: None,
                email_verified: false,
                password: "Root-password-123!".into(),
                role: "owner".into(),
                status: "active".into(),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root.id, 1);
        let request = db
            .prepare_registration("Late-User", " Late@Example.COM ", "Fixture-password-123!")
            .await
            .unwrap();
        let (_, issued) = db
            .issue_registration_otp(&request.request_id, 10, PEPPER)
            .await
            .unwrap();
        db.set_user_role(root.id, nuono_app::auth::Role::User)
            .await
            .unwrap();
        assert!(
            db.verify_registration_otp("late@example.com", &issued.unwrap().code, PEPPER)
                .await
                .is_err()
        );
        assert_eq!(db.user_count().await.unwrap(), 1);
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn expired_proof_cleanup_preserves_cross_request_mail_quota() {
        let db = initialized_db(2).await;
        let first = db
            .prepare_registration("Quota-User", "quota@example.com", "Fixture-password-123!")
            .await
            .unwrap();
        let (_, issued) = db
            .issue_registration_otp(&first.request_id, 10, PEPPER)
            .await
            .unwrap();
        assert!(issued.is_some());
        let now = crate::clock::db_now().timestamp_millis();
        let row = challenges::Entity::find_by_id(&first.request_id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let mut context: Context = serde_json::from_str(&row.context_json).unwrap();
        context.sends = (1..=5).map(|n| now - n * 120_000).collect();
        context.sends.sort_unstable();
        challenges::Entity::update_many()
            .col_expr(
                challenges::Column::ContextJson,
                Expr::value(serde_json::to_string(&context).unwrap()),
            )
            .col_expr(challenges::Column::ExpiresAtMs, Expr::value(now - 1))
            .filter(challenges::Column::Id.eq(&first.request_id))
            .exec(db.orm())
            .await
            .unwrap();
        db.delete_stale_user_tokens().await.unwrap();
        let second = db
            .prepare_registration("Quota-User", "QUOTA@EXAMPLE.COM", "Recipient-password-123!")
            .await
            .unwrap();
        assert_eq!(
            db.issue_registration_otp(&second.request_id, 10, PEPPER)
                .await
                .unwrap()
                .0,
            RegistrationOtpIssueOutcome::HourlyLimit
        );
        let row = challenges::Entity::find_by_id(&second.request_id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let mut context: Context = serde_json::from_str(&row.context_json).unwrap();
        context.sends = (1..=10).map(|n| now - 3_600_000 - n * 120_000).collect();
        context.sends.sort_unstable();
        challenges::Entity::update_many()
            .col_expr(
                challenges::Column::ContextJson,
                Expr::value(serde_json::to_string(&context).unwrap()),
            )
            .filter(challenges::Column::Id.eq(&second.request_id))
            .exec(db.orm())
            .await
            .unwrap();
        let third = db
            .prepare_registration("Quota-User", "quota@example.com", "Recipient-password-123!")
            .await
            .unwrap();
        assert_eq!(
            db.issue_registration_otp(&third.request_id, 10, PEPPER)
                .await
                .unwrap()
                .0,
            RegistrationOtpIssueOutcome::DailyLimit
        );
        assert_eq!(db.user_count().await.unwrap(), 1);
        db.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn a_late_delivery_failure_does_not_revoke_a_newer_code() {
        let db = initialized_db(2).await;
        let request = db
            .prepare_registration("Retry-User", "retry@example.com", "Fixture-password-123!")
            .await
            .unwrap();
        let (_, old) = db
            .issue_registration_otp(&request.request_id, 10, PEPPER)
            .await
            .unwrap();
        let old = old.unwrap();
        let row = challenges::Entity::find_by_id(&request.request_id)
            .one(db.orm())
            .await
            .unwrap()
            .unwrap();
        let mut context: Context = serde_json::from_str(&row.context_json).unwrap();
        context.sends = vec![crate::clock::db_now().timestamp_millis() - 61_000];
        challenges::Entity::update_many()
            .col_expr(
                challenges::Column::ContextJson,
                Expr::value(serde_json::to_string(&context).unwrap()),
            )
            .filter(challenges::Column::Id.eq(&request.request_id))
            .exec(db.orm())
            .await
            .unwrap();
        let (_, new) = db
            .issue_registration_otp(&request.request_id, 10, PEPPER)
            .await
            .unwrap();
        let new = new.unwrap();
        assert!(!db.revoke_registration_otp(&old.id).await.unwrap());
        assert_eq!(
            db.verify_registration_otp("retry@example.com", &new.code, PEPPER)
                .await
                .unwrap(),
            RegistrationOtpVerification::Verified { user_id: 2 }
        );
        db.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod setup_sequence_tests {
    use super::*;
    #[tokio::test]
    async fn first_root_is_id_one_without_rewinding_an_allocated_sequence() {
        let db = crate::test_support::test_db(2).await;
        if db.backend() == sea_orm::DbBackend::Postgres {
            for _ in 0..7 {
                db.orm()
                    .query_one_raw(Statement::from_string(
                        db.backend(),
                        "SELECT nextval(pg_get_serial_sequence('nuono_users','id'))",
                    ))
                    .await
                    .unwrap();
            }
        } else {
            db.orm()
                .execute_raw(Statement::from_string(
                    db.backend(),
                    "INSERT INTO sqlite_sequence(name,seq) VALUES('nuono_users',7)",
                ))
                .await
                .unwrap();
        }
        let root = db
            .create_first_owner(super::super::NewUser {
                username: "root".into(),
                email: None,
                email_verified: false,
                password: "Root-password-123!".into(),
                role: "owner".into(),
                status: "active".into(),
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root.id, 1);
        let request = db
            .prepare_registration("after-root", "after@example.com", "Fixture-password-123!")
            .await
            .unwrap();
        let (_, proof) = db
            .issue_registration_otp(&request.request_id, 10, super::tests::PEPPER)
            .await
            .unwrap();
        assert_eq!(
            db.verify_registration_otp(
                "after@example.com",
                &proof.unwrap().code,
                super::tests::PEPPER
            )
            .await
            .unwrap(),
            RegistrationOtpVerification::Verified { user_id: 8 }
        );
        db.shutdown().await.unwrap();
    }
}
