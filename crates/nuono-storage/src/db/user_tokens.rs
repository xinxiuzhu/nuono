//! 邮件验证、密码重置与更换邮箱令牌：统一存入 `nuono_account_challenges` 表，并在核销时与对应业务变更同事务原子提交。
use super::{
    Database,
    account_state::{self, AuditEvent, append_audit_on, challenges},
};
use crate::{
    entity::users,
    error::{NuonoError, Result},
};
use nuono_app::account_security::{TokenOperation, TokenRedemption};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::{Expr, ExprTrait},
};
pub const TOKEN_VERIFY_EMAIL: &str = "email_verification";
pub const TOKEN_RESET_PASSWORD: &str = "password_reset";
pub const TOKEN_CHANGE_EMAIL: &str = "email_change";
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub token: String,
    pub expires_at: String,
}

pub(super) fn new_token_plaintext() -> String {
    nuono_app::auth::generate_secret()
}
pub(super) fn hash_token(token: &str) -> Vec<u8> {
    account_state::token_hash(token)
}

pub(super) async fn issue_proof_on(
    tx: &impl ConnectionTrait,
    purpose: &str,
    requester: Option<i64>,
    subject: Option<(i64, i64)>,
    ttl: i64,
    context: serde_json::Value,
) -> Result<(String, IssuedToken)> {
    let token = new_token_plaintext();
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::clock::db_now().timestamp_millis();
    let end = now + ttl.clamp(1, 43_200) * 60_000;
    if let Some((user_id, _)) = subject {
        challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("cancelled"))
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .filter(challenges::Column::SubjectUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq(purpose))
            .filter(challenges::Column::State.eq("pending"))
            .exec(tx)
            .await?;
    }
    challenges::Entity::insert(challenges::ActiveModel {
        id: Set(id.clone()),
        purpose: Set(purpose.into()),
        requester_user_id: Set(requester),
        subject_user_id: Set(subject.map(|v| v.0)),
        subject_auth_epoch: Set(subject.map(|v| v.1)),
        session_id: Set(None),
        token_hash: Set(hash_token(&token)),
        context_json: Set(context.to_string()),
        candidate_identity_id: Set(None),
        state: Set("pending".into()),
        attempts: Set(0),
        expires_at_ms: Set(end),
        consumed_at_ms: Set(None),
        revision: Set(1),
        created_at_ms: Set(now),
        updated_at_ms: Set(now),
    })
    .exec(tx)
    .await?;
    Ok((
        id,
        IssuedToken {
            token,
            expires_at: end.to_string(),
        },
    ))
}
impl Database {
    pub async fn issue_user_token(
        &self,
        user_id: i64,
        purpose: &str,
        ttl_minutes: i64,
        new_email: Option<&str>,
    ) -> Result<IssuedToken> {
        if ![TOKEN_VERIFY_EMAIL, TOKEN_RESET_PASSWORD, TOKEN_CHANGE_EMAIL].contains(&purpose) {
            return Err(NuonoError::Validation("该用途必须使用专用证明流程".into()));
        }
        let tx = self.begin_write().await?;
        let user = account_state::locked_user(&tx, user_id).await?;
        let (_, issued) = issue_proof_on(
            &tx,
            purpose,
            Some(user_id),
            Some((user_id, user.auth_epoch)),
            ttl_minutes,
            serde_json::json!({"email":new_email.map(str::to_owned).or(user.email)}),
        )
        .await?;
        tx.commit().await?;
        Ok(issued)
    }
    pub async fn issue_password_reset_token(
        &self,
        user_id: i64,
        email: &str,
        ttl: i64,
    ) -> Result<Option<IssuedToken>> {
        let tx = self.begin_write().await?;
        let user = match account_state::locked_user(&tx, user_id).await {
            Ok(user) => user,
            Err(NuonoError::PermissionDenied(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if user.email_verified_at_ms.is_none() || user.email_key.as_deref() != Some(email) {
            return Ok(None);
        }
        let now = crate::clock::db_now().timestamp_millis();
        let recent = challenges::Entity::find()
            .filter(challenges::Column::SubjectUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq(TOKEN_RESET_PASSWORD))
            .filter(challenges::Column::CreatedAtMs.gt(now - 60_000))
            .one(&tx)
            .await?;
        if recent.is_some() {
            return Ok(None);
        }
        let (_, issued) = issue_proof_on(
            &tx,
            TOKEN_RESET_PASSWORD,
            Some(user_id),
            Some((user_id, user.auth_epoch)),
            ttl,
            serde_json::json!({"email":email}),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(issued))
    }
    pub async fn revoke_user_token(&self, token: &str) -> Result<u64> {
        Ok(challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("cancelled"))
            .col_expr(
                challenges::Column::UpdatedAtMs,
                Expr::value(crate::clock::db_now().timestamp_millis()),
            )
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .filter(challenges::Column::TokenHash.eq(hash_token(token)))
            .filter(challenges::Column::State.eq("pending"))
            .exec(self.orm())
            .await?
            .rows_affected)
    }
    pub async fn revoke_user_tokens(&self, user_id: i64, purpose: &str) -> Result<u64> {
        let tx = self.begin_write().await?;
        account_state::locked_user(&tx, user_id).await?;
        let n = challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("cancelled"))
            .col_expr(
                challenges::Column::UpdatedAtMs,
                Expr::value(crate::clock::db_now().timestamp_millis()),
            )
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .filter(challenges::Column::SubjectUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq(purpose))
            .filter(challenges::Column::State.eq("pending"))
            .exec(&tx)
            .await?
            .rows_affected;
        tx.commit().await?;
        Ok(n)
    }
    pub async fn delete_stale_user_tokens(&self) -> Result<u64> {
        Ok(
            challenges::Entity::update_many().col_expr(challenges::Column::State,Expr::value("expired")).col_expr(challenges::Column::ContextJson,Expr::value("{}"))
   .col_expr(challenges::Column::UpdatedAtMs,Expr::value(crate::clock::db_now().timestamp_millis()))
   .col_expr(challenges::Column::Revision,Expr::col(challenges::Column::Revision).add(1))
   // 自助注册（registration）挑战记录中的近期发信历史需保留到统一的 24 小时过期清理任务，
   // 若在 10 分钟 OTP 过期时就清空 context_json，会导致单邮箱的每小时/每日发信限额被提前重置。
   .filter(challenges::Column::Purpose.is_in([TOKEN_VERIFY_EMAIL,TOKEN_RESET_PASSWORD,TOKEN_CHANGE_EMAIL]))
   .filter(challenges::Column::ExpiresAtMs.lte(crate::clock::db_now().timestamp_millis())).filter(challenges::Column::State.eq("pending"))
   .exec(self.orm()).await?.rows_affected,
        )
    }
    pub async fn latest_user_token_issued_at(
        &self,
        user_id: i64,
        purpose: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let row = challenges::Entity::find()
            .filter(challenges::Column::SubjectUserId.eq(user_id))
            .filter(challenges::Column::Purpose.eq(purpose))
            .order_by_desc(challenges::Column::CreatedAtMs)
            .one(self.orm())
            .await?;
        Ok(row.and_then(|v| chrono::DateTime::from_timestamp_millis(v.created_at_ms)))
    }
    pub async fn redeem_challenge_token(
        &self,
        token: &str,
        operation: TokenOperation,
    ) -> Result<Option<TokenRedemption>> {
        let password_hash = if let Some(password) = operation.password() {
            nuono_app::auth::validate_password(password).map_err(NuonoError::Validation)?;
            Some(self.password_work.hash(password).await?)
        } else {
            None
        };
        let tx = self.begin_write().await?;
        let initial = challenges::Entity::find()
            .filter(challenges::Column::TokenHash.eq(hash_token(token)))
            .one(&tx)
            .await?;
        let Some(initial) = initial else {
            return Ok(None);
        };
        let mut ids = initial
            .subject_user_id
            .into_iter()
            .chain(initial.requester_user_id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        crate::group_authorizations::lock_accounts(&tx, &ids).await?;
        let mut query = challenges::Entity::find_by_id(&initial.id);
        if self.backend() == sea_orm::DbBackend::Postgres {
            query = query.lock_exclusive();
        }
        let Some(proof) = query.one(&tx).await? else {
            return Ok(None);
        };
        let now = crate::clock::db_now().timestamp_millis();
        if proof.purpose != operation.purpose()
            || proof.state != "pending"
            || proof.expires_at_ms <= now
        {
            return Ok(None);
        }
        let context = nuono_domain::scalar::parse_json(&proof.context_json)?;
        let Some(user_id) = proof.subject_user_id else {
            return Ok(None);
        };
        let user = account_state::locked_user(&tx, user_id).await?;
        if Some(user.auth_epoch) != proof.subject_auth_epoch {
            return Ok(None);
        }
        let mut update = users::orm::Entity::update_many()
            .filter(users::orm::Column::Id.eq(user_id))
            .col_expr(users::orm::Column::UpdatedAtMs, Expr::value(now))
            .col_expr(
                users::orm::Column::Revision,
                Expr::col(users::orm::Column::Revision).add(1),
            );
        match operation {
            TokenOperation::ResetPassword { .. } => {
                if user.email_verified_at_ms.is_none()
                    || user.email.as_deref() != context["email"].as_str()
                {
                    return Ok(None);
                }
                update =
                    update.col_expr(users::orm::Column::PasswordHash, Expr::value(password_hash));
            }
            TokenOperation::ChangeEmail | TokenOperation::VerifyEmail => {
                let email = context["email"]
                    .as_str()
                    .ok_or_else(|| NuonoError::Validation("验证目标邮箱不存在".into()))?;
                nuono_app::auth::validate_email(email).map_err(NuonoError::Validation)?;
                update = update
                    .col_expr(users::orm::Column::Email, Expr::value(email))
                    .col_expr(
                        users::orm::Column::EmailKey,
                        Expr::value(email.to_ascii_lowercase()),
                    )
                    .col_expr(users::orm::Column::EmailVerifiedAtMs, Expr::value(now));
            }
        }
        update
            .col_expr(
                users::orm::Column::AuthEpoch,
                Expr::col(users::orm::Column::AuthEpoch).add(1),
            )
            .exec(&tx)
            .await
            .map_err(super::users::account_unique_conflict)?;
        let account = users::orm::Entity::find_by_id(user_id)
            .one(&tx)
            .await?
            .ok_or_else(|| NuonoError::Internal("兑换后的账号不存在".into()))?;
        let revoked = account_state::revoke_sessions_on(&tx, user_id).await?;
        account_state::cancel_proofs_on(&tx, user_id).await?;
        challenges::Entity::update_many()
            .col_expr(challenges::Column::State, Expr::value("consumed"))
            .col_expr(challenges::Column::ConsumedAtMs, Expr::value(now))
            .col_expr(challenges::Column::ContextJson, Expr::value("{}"))
            .col_expr(challenges::Column::SubjectUserId, Expr::value(user_id))
            .col_expr(
                challenges::Column::SubjectAuthEpoch,
                Expr::value(account.auth_epoch),
            )
            .col_expr(challenges::Column::UpdatedAtMs, Expr::value(now))
            .col_expr(
                challenges::Column::Revision,
                Expr::col(challenges::Column::Revision).add(1),
            )
            .filter(challenges::Column::Id.eq(&proof.id))
            .exec(&tx)
            .await?;
        append_audit_on(
            &tx,
            AuditEvent {
                actor_user_id: Some(user_id),
                actor_plugin_id: None,
                action: "account.proof_redeemed",
                result: "success",
                subject_user_id: Some(user_id),
                group_id: None,
                operation_id: Some(&proof.id),
                details: serde_json::json!({"purpose":proof.purpose}),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Some(TokenRedemption {
            user_id,
            revoked_sessions: revoked,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_app::{
        account_provisioning::{AccountProvisioningStore, NewManagedAccount},
        auth::Role,
    };
    async fn initialized() -> (Database, i64) {
        let db = crate::test_support::test_db(3).await;
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
        (db, owner.id)
    }
    fn managed_account(name: &str, email: &str) -> NewManagedAccount {
        NewManagedAccount {
            username: name.into(),
            email: Some(email.into()),
            password: "Unused-random-password-123!".into(),
            role: Role::User,
        }
    }
    #[tokio::test]
    async fn mail_proofs_are_single_use_and_bound_to_current_email_and_epoch() {
        let (db, owner) = initialized().await;
        let account = db
            .create_managed_account(owner, managed_account("mail-proof-user", "old@example.com"))
            .await
            .unwrap();
        let user = db.find_user_by_id(account.id).await.unwrap().unwrap();
        let session = "mail-proof-session";
        db.create_session_for_verified_user(
            &user,
            &format!(
                "auth_session_{}",
                hex::encode(account_state::token_hash(session))
            ),
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let reset = db
            .issue_password_reset_token(user.id, "old@example.com", 10)
            .await
            .unwrap()
            .unwrap();
        let change = db
            .prepare_email_change(
                user.id,
                "Unused-random-password-123!",
                "NEW@EXAMPLE.COM",
                10,
            )
            .await
            .unwrap()
            .unwrap();
        let redeemed = db
            .redeem_challenge_token(&change.token, TokenOperation::ChangeEmail)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(redeemed.revoked_sessions, 1);
        assert!(db.authenticate_session(session).await.unwrap().is_none());
        assert!(
            db.redeem_challenge_token(&change.token, TokenOperation::ChangeEmail)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.redeem_challenge_token(
                &reset.token,
                TokenOperation::ResetPassword {
                    password: "Attacker-password-123!".into()
                }
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            db.issue_password_reset_token(user.id, "old@example.com", 10)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.find_user_by_id(user.id)
                .await
                .unwrap()
                .unwrap()
                .email
                .as_deref(),
            Some("new@example.com")
        );
        db.shutdown().await.unwrap();
    }
}
