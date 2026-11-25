use super::*;
use crate::{db::NewUser, test_support::test_db};
use sea_orm::Statement;

const OLD: &str = "Original-password-123!";
const NEW: &str = "Changed-password-456!";

fn key(byte: u8) -> String {
    format!("auth_session_{}", hex::encode([byte; 32]))
}

async fn account(db: &Database, name: &str, byte: u8) -> users::Model {
    let user = db
        .create_user(NewUser {
            username: name.into(),
            email: Some(format!("{name}@example.com")),
            password: OLD.into(),
            role: "user".into(),
            status: "active".into(),
            email_verified: true,
        })
        .await
        .unwrap();
    assert!(
        db.create_session_for_verified_user(
            &user,
            &key(byte),
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap()
    );
    user
}

async fn audits(db: &Database) -> Vec<crate::entity::audit_events::Model> {
    crate::entity::audit_events::Entity::find()
        .filter(crate::entity::audit_events::Column::Category.eq(crate::audit::CATEGORY_EVENT))
        .filter(crate::entity::audit_events::Column::Action.eq("account.password_changed"))
        .all(db.orm())
        .await
        .unwrap()
}

#[tokio::test]
async fn password_change_commits_password_sessions_and_audit_together() {
    let db = test_db(3).await;
    let user = account(&db, "changed", 1).await;
    account(&db, "untouched", 2).await;
    db.create_session_for_verified_user(
        &user,
        &key(3),
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(
        db.change_password_for_verified_user(&user, &key(1), NEW)
            .await
            .unwrap(),
        Some(2)
    );
    assert!(
        db.verify_user_password("changed", NEW)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db.verify_user_password("changed", OLD)
            .await
            .unwrap()
            .is_none()
    );
    assert!(test_session(&db, &key(1)).await.unwrap().is_none());
    assert!(test_session(&db, &key(3)).await.unwrap().is_none());
    assert!(test_session(&db, &key(2)).await.unwrap().is_some());
    let rows = audits(&db).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor_label.as_deref(), Some("changed"));
    let data = &rows[0].details_json;
    assert!(!data.contains(NEW) && !data.contains(OLD));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(data).unwrap()["revoked_sessions"],
        2
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn password_change_rechecks_revocation_expiry_and_account_snapshot() {
    let db = test_db(2).await;
    let user = account(&db, "recheck", 1).await;
    sessions::Entity::delete_many()
        .filter(sessions::Column::TokenHash.eq(session_hash_from_key(&key(1)).unwrap()))
        .exec(db.orm())
        .await
        .unwrap();
    assert_eq!(
        db.change_password_for_verified_user(&user, &key(1), NEW)
            .await
            .unwrap(),
        None
    );
    for payload in [
        serde_json::json!({"username":"recheck", "expires":(chrono::Utc::now()-chrono::Duration::seconds(1)).to_rfc3339()}),
        serde_json::json!({"username":"someone-else", "expires":(chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339()}),
        serde_json::json!({"username":"recheck", "expires":"invalid"}),
    ] {
        db.set_global_config(&key(1), payload).await.unwrap();
        assert_eq!(
            db.change_password_for_verified_user(&user, &key(1), NEW)
                .await
                .unwrap(),
            None
        );
    }
    sessions::Entity::delete_many()
        .filter(sessions::Column::TokenHash.eq(session_hash_from_key(&key(1)).unwrap()))
        .exec(db.orm())
        .await
        .unwrap();
    db.create_session_for_verified_user(
        &user,
        &key(1),
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    users::orm::Entity::update_many()
        .filter(users::orm::Column::Id.eq(user.id))
        .col_expr(users::orm::Column::Status, Expr::value("disabled"))
        .exec(db.orm())
        .await
        .unwrap();
    assert_eq!(
        db.change_password_for_verified_user(&user, &key(1), NEW)
            .await
            .unwrap(),
        None
    );
    let current = db.find_user_by_id(user.id).await.unwrap().unwrap();
    assert!(verify_password(OLD, &current.password_hash));
    assert!(audits(&db).await.is_empty());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_password_changes_cannot_overwrite_each_other() {
    let db = test_db(3).await;
    let user = account(&db, "concurrent", 1).await;
    let session = key(1);
    let (first, second) = tokio::join!(
        db.change_password_for_verified_user(&user, &session, NEW),
        db.change_password_for_verified_user(&user, &session, "Another-password-789!"),
    );
    assert_eq!(
        [first.unwrap(), second.unwrap()]
            .into_iter()
            .flatten()
            .count(),
        1
    );
    assert_eq!(audits(&db).await.len(), 1);
    assert!(test_session(&db, &session).await.unwrap().is_none());
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn password_change_serializes_with_login_using_old_credentials() {
    let db = test_db(3).await;
    let user = account(&db, "login-race", 1).await;
    let current_key = key(1);
    let new_key = key(2);
    let expires = chrono::Utc::now() + chrono::Duration::hours(1);
    let (changed, login) = tokio::join!(
        db.change_password_for_verified_user(&user, &current_key, NEW),
        db.create_session_for_verified_user(&user, &new_key, expires),
    );
    assert!(changed.unwrap().is_some());
    login.unwrap();
    assert!(test_session(&db, &new_key).await.unwrap().is_none());
    assert!(
        !db.create_session_for_verified_user(&user, &key(3), expires)
            .await
            .unwrap()
    );
    let current = db
        .verify_user_password("login-race", NEW)
        .await
        .unwrap()
        .unwrap();
    assert!(
        db.create_session_for_verified_user(&current, &key(4), expires)
            .await
            .unwrap()
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn password_change_audit_failure_rolls_back_password_and_session_deletion() {
    let db = test_db(2).await;
    let user = account(&db, "atomic", 1).await;
    db.orm()
        .execute_raw(Statement::from_string(
            db.backend(),
            "ALTER TABLE nuono_audit_events RENAME TO unavailable_password_audit",
        ))
        .await
        .unwrap();
    assert!(
        db.change_password_for_verified_user(&user, &key(1), NEW)
            .await
            .is_err()
    );
    assert!(
        db.verify_user_password("atomic", OLD)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db.verify_user_password("atomic", NEW)
            .await
            .unwrap()
            .is_none()
    );
    assert!(test_session(&db, &key(1)).await.unwrap().is_some());
    db.orm()
        .execute_raw(Statement::from_string(
            db.backend(),
            "ALTER TABLE unavailable_password_audit RENAME TO nuono_audit_events",
        ))
        .await
        .unwrap();
    assert!(audits(&db).await.is_empty());
    // 写入失败时不会销毁当前会话，数据库恢复正常后原请求可以直接重试。
    assert_eq!(
        db.change_password_for_verified_user(&user, &key(1), NEW)
            .await
            .unwrap(),
        Some(1)
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_generation_is_invalidated_by_password_change() {
    let db = test_db(2).await;
    let user = account(&db, "epoch_owner", 8).await;
    assert_eq!(
        db.session_account_username(user.id, 1).await.unwrap(),
        Some(user.username.clone())
    );
    db.set_user_password(user.id, NEW).await.unwrap();
    assert_eq!(db.session_account_username(user.id, 1).await.unwrap(), None);
    assert_eq!(
        db.session_account_username(user.id, 2).await.unwrap(),
        Some(user.username)
    );
    db.shutdown().await.unwrap();
}
