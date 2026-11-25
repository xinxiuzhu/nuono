use crate::{
    db::{NewUser, TOKEN_RESET_PASSWORD},
    test_support::test_db,
};
use nuono_app::{
    account_provisioning::AccountProvisioningStore,
    account_security::{AccountSecurityService, TokenOperation},
};

const OLD: &str = "Original-password-123!";
const NEW: &str = "Owner-new-password-456!";
const REPLACED: &str = "Token-holder-password-789!";

async fn verified_account(db: &crate::Database, name: &str) -> crate::entity::users::Model {
    db.create_user(NewUser {
        username: name.into(),
        email: Some(format!("{name}@example.invalid")),
        password: OLD.into(),
        role: "user".into(),
        status: "active".into(),
        email_verified: true,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn old_reset_token_must_not_override_a_completed_password_change() {
    let db = test_db(2).await;
    let user = db
        .create_user(NewUser {
            username: "reset-review-password".into(),
            email: Some("old@example.invalid".into()),
            password: OLD.into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let token = db
        .issue_user_token(user.id, TOKEN_RESET_PASSWORD, 10, None)
        .await
        .unwrap();
    let key = format!("auth_session_{}", "ab".repeat(32));
    assert!(
        db.create_session_for_verified_user(
            &user,
            &key,
            chrono::Utc::now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap()
    );
    assert_eq!(
        db.change_password_for_verified_user(&user, &key, NEW)
            .await
            .unwrap(),
        Some(1)
    );
    let accepted = AccountSecurityService::new(&db)
        .redeem(
            &token.token,
            TokenOperation::ResetPassword {
                password: REPLACED.into(),
            },
        )
        .await
        .unwrap()
        .is_some();
    let password_replaced = db
        .verify_user_password(&user.username, REPLACED)
        .await
        .unwrap()
        .is_some();
    db.shutdown().await.unwrap();
    assert!(
        !accepted && !password_replaced,
        "old reset token accepted={accepted}, superseded owner's new password={password_replaced}"
    );
}

#[tokio::test]
async fn old_mailbox_reset_token_must_not_survive_confirmed_email_change() {
    let db = test_db(2).await;
    let user = db
        .create_user(NewUser {
            username: "reset-review-email".into(),
            email: Some("old@example.invalid".into()),
            password: OLD.into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    let token = db
        .issue_user_token(user.id, TOKEN_RESET_PASSWORD, 10, None)
        .await
        .unwrap();
    let change = db
        .prepare_email_change(user.id, OLD, "new@example.invalid", 10)
        .await
        .unwrap()
        .unwrap();
    assert!(
        AccountSecurityService::new(&db)
            .redeem(&change.token, TokenOperation::ChangeEmail)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db.find_user_by_id(user.id)
            .await
            .unwrap()
            .unwrap()
            .email
            .as_deref(),
        Some("new@example.invalid")
    );
    let accepted = AccountSecurityService::new(&db)
        .redeem(
            &token.token,
            TokenOperation::ResetPassword {
                password: REPLACED.into(),
            },
        )
        .await
        .unwrap()
        .is_some();
    let password_replaced = db
        .verify_user_password(&user.username, REPLACED)
        .await
        .unwrap()
        .is_some();
    db.shutdown().await.unwrap();
    assert!(
        !accepted && !password_replaced,
        "old mailbox reset token accepted={accepted}, changed password={password_replaced}"
    );
}

#[tokio::test]
async fn reset_issuance_rechecks_recipient_after_waiting_for_email_change() {
    use crate::entity::users::orm::{Column, Entity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
    let db = test_db(3).await;
    let user = verified_account(&db, "issuance-lock").await;
    let old_email = user.email.unwrap();
    let tx = db.begin_write().await.unwrap();
    Entity::update_many()
        .filter(Column::Id.eq(user.id))
        .col_expr(Column::Email, Expr::value("new@example.invalid"))
        .col_expr(Column::EmailKey, Expr::value("new@example.invalid"))
        .exec(&tx)
        .await
        .unwrap();
    let mut issuance = tokio::spawn({
        let db = db.clone();
        async move { db.issue_password_reset_token(user.id, &old_email, 10).await }
    });
    let waited = tokio::time::timeout(std::time::Duration::from_millis(100), &mut issuance).await;
    assert!(
        waited.is_err(),
        "issuance must wait for the account write lock"
    );
    tx.commit().await.unwrap();
    assert!(
        issuance.await.unwrap().unwrap().is_none(),
        "old recipient must be rejected after the lock wait"
    );
    let issued = db
        .issue_password_reset_token(user.id, "new@example.invalid", 10)
        .await
        .unwrap()
        .unwrap();
    assert!(
        AccountSecurityService::new(&db)
            .redeem(
                &issued.token,
                TokenOperation::ResetPassword {
                    password: NEW.into()
                }
            )
            .await
            .unwrap()
            .is_some()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_password_change_preserves_reset_token_and_other_accounts_are_untouched() {
    use sea_orm::{ConnectionTrait, Statement};
    let db = test_db(2).await;
    let user = verified_account(&db, "reset-rollback").await;
    let other = verified_account(&db, "reset-unrelated").await;
    let issued = db
        .issue_password_reset_token(user.id, user.email.as_deref().unwrap(), 10)
        .await
        .unwrap()
        .unwrap();
    let other_token = db
        .issue_password_reset_token(other.id, other.email.as_deref().unwrap(), 10)
        .await
        .unwrap()
        .unwrap();
    let key = format!("auth_session_{}", "cd".repeat(32));
    assert!(
        db.create_session_for_verified_user(
            &user,
            &key,
            chrono::Utc::now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap()
    );
    db.orm()
        .execute_raw(Statement::from_string(
            db.backend(),
            "ALTER TABLE nuono_audit_events RENAME TO reset_review_unavailable_audit",
        ))
        .await
        .unwrap();
    assert!(
        db.change_password_for_verified_user(&user, &key, NEW)
            .await
            .is_err()
    );
    db.orm()
        .execute_raw(Statement::from_string(
            db.backend(),
            "ALTER TABLE reset_review_unavailable_audit RENAME TO nuono_audit_events",
        ))
        .await
        .unwrap();
    assert!(
        db.verify_user_password(&user.username, OLD)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        AccountSecurityService::new(&db)
            .redeem(
                &issued.token,
                TokenOperation::ResetPassword {
                    password: NEW.into()
                }
            )
            .await
            .unwrap()
            .is_some(),
        "failed transaction must preserve the original token"
    );
    let current = db.find_user_by_id(user.id).await.unwrap().unwrap();
    let key = format!("auth_session_{}", "ef".repeat(32));
    assert!(
        db.create_session_for_verified_user(
            &current,
            &key,
            chrono::Utc::now() + chrono::Duration::hours(1)
        )
        .await
        .unwrap()
    );
    assert_eq!(
        db.change_password_for_verified_user(&current, &key, REPLACED)
            .await
            .unwrap(),
        Some(1)
    );
    assert!(
        AccountSecurityService::new(&db)
            .redeem(
                &other_token.token,
                TokenOperation::ResetPassword {
                    password: NEW.into()
                }
            )
            .await
            .unwrap()
            .is_some(),
        "changing one account must not revoke another account's reset token"
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_email_change_preserves_reset_token() {
    let db = test_db(2).await;
    let user = verified_account(&db, "email-rollback").await;
    let issued = db
        .issue_password_reset_token(user.id, user.email.as_deref().unwrap(), 10)
        .await
        .unwrap()
        .unwrap();
    let change = db
        .prepare_email_change(user.id, OLD, "claimed@example.invalid", 10)
        .await
        .unwrap()
        .unwrap();
    verified_account(&db, "claimed").await;
    assert!(
        AccountSecurityService::new(&db)
            .redeem(&change.token, TokenOperation::ChangeEmail)
            .await
            .is_err()
    );
    assert!(
        AccountSecurityService::new(&db)
            .redeem(
                &issued.token,
                TokenOperation::ResetPassword {
                    password: NEW.into()
                }
            )
            .await
            .unwrap()
            .is_some(),
        "failed email update must not revoke recovery credentials"
    );
    db.shutdown().await.unwrap();
}
