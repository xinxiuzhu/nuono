//! 账号管理服务门面；业务校验规则位于应用层，原子写入实现位于存储层。
use crate::{auth::Role, db::Database, error::Result};
use nuono_app::accounts::{AccountMutation, AccountMutationResult, AccountStore};
pub use nuono_app::accounts::{RoleChange, StatusChange, UserDeletion};
pub async fn set_user_role(
    db: &Database,
    actor: i64,
    target: i64,
    role: Role,
) -> Result<RoleChange> {
    match db
        .mutate_account(actor, target, AccountMutation::Role(role))
        .await?
    {
        AccountMutationResult::Role(change) => Ok(change),
        _ => unreachable!(),
    }
}
pub async fn set_user_status(
    db: &Database,
    actor: i64,
    target: i64,
    status: &str,
) -> Result<StatusChange> {
    match db
        .mutate_account(actor, target, AccountMutation::Status(status.into()))
        .await?
    {
        AccountMutationResult::Status(change) => Ok(change),
        _ => unreachable!(),
    }
}
pub async fn delete_user(db: &Database, actor: i64, target: i64) -> Result<UserDeletion> {
    match db
        .mutate_account(actor, target, AccountMutation::Delete)
        .await?
    {
        AccountMutationResult::Delete(change) => Ok(change),
        _ => unreachable!(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NuonoError;
    use sea_orm::{ConnectionTrait, Statement};

    use crate::db::{NewUser, STATUS_ACTIVE, STATUS_DISABLED};

    async fn db() -> Database {
        crate::test_support::test_db(2).await
    }

    async fn user(db: &Database, name: &str, role: Role) -> i64 {
        db.create_user(NewUser {
            username: name.to_string(),
            email: Some(format!("{name}@example.com")),
            password: "correct-horse-battery".to_string(),
            role: role.as_str().to_string(),
            status: STATUS_ACTIVE.to_string(),
            email_verified: true,
        })
        .await
        .expect("建号")
        .id
    }

    #[tokio::test]
    async fn promotes_a_user_to_admin() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        let change = set_user_role(&db, owner, target, Role::Admin)
            .await
            .expect("应当成功");

        assert_eq!(change.previous, Role::User);
        assert_eq!(change.current, Role::Admin);
        let stored = db.find_user_by_id(target).await.unwrap().unwrap();
        assert_eq!(Role::from_db_value(&stored.role), Role::Admin);
    }

    #[tokio::test]
    async fn rejects_changing_own_role() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let other = user(&db, "owner2", Role::Owner).await;

        // 有第二个 owner 在，所以拒绝的原因只能是「改的是自己」，不是「最后一个 owner」。
        let _ = other;
        let err = set_user_role(&db, owner, owner, Role::User)
            .await
            .expect_err("应当拒绝");

        assert!(matches!(err, NuonoError::PermissionDenied(_)), "{err:?}");
        let stored = db.find_user_by_id(owner).await.unwrap().unwrap();
        assert_eq!(Role::from_db_value(&stored.role), Role::Owner);
    }

    #[tokio::test]
    async fn rejects_demoting_the_last_owner() {
        let db = db().await;
        let sole_owner = user(&db, "owner1", Role::Owner).await;
        let admin = user(&db, "staff", Role::Admin).await;

        // 由 admin 发起，绕开自我降级那条禁令，单独验证「最后一个 owner」这条。
        let err = set_user_role(&db, admin, sole_owner, Role::Admin)
            .await
            .expect_err("应当拒绝");

        assert!(
            matches!(err, NuonoError::PermissionValidation(_)),
            "{err:?}"
        );
        let stored = db.find_user_by_id(sole_owner).await.unwrap().unwrap();
        assert_eq!(Role::from_db_value(&stored.role), Role::Owner);
    }

    #[tokio::test]
    async fn allows_demoting_an_owner_when_another_remains() {
        let db = db().await;
        let keeper = user(&db, "owner1", Role::Owner).await;
        let leaving = user(&db, "owner2", Role::Owner).await;

        let change = set_user_role(&db, keeper, leaving, Role::User)
            .await
            .expect("应当成功");

        assert_eq!(change.previous, Role::Owner);
        assert_eq!(change.current, Role::User);
        assert_eq!(db.owner_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn same_role_is_a_no_op() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        let change = set_user_role(&db, owner, target, Role::User)
            .await
            .expect("应当成功");

        // 关键断言是没有撤会话：重复点同一个角色不该把人踢下线。
        assert_eq!(change.revoked_sessions, 0);
        assert_eq!(change.previous, change.current);
    }

    #[tokio::test]
    async fn rejects_unknown_target() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;

        let err = set_user_role(&db, owner, 9999, Role::Admin)
            .await
            .expect_err("应当拒绝");

        assert!(matches!(err, NuonoError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn disabling_a_user_revokes_its_sessions() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        for (id, byte) in [(target, 1_u8), (owner, 2_u8)] {
            let account = db.find_user_by_id(id).await.unwrap().unwrap();
            db.create_session_for_verified_user(
                &account,
                &format!("auth_session_{}", hex::encode([byte; 32])),
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        }

        let change = set_user_status(&db, owner, target, STATUS_DISABLED)
            .await
            .expect("应当成功");

        assert_eq!(change.previous, STATUS_ACTIVE);
        assert_eq!(change.current, STATUS_DISABLED);
        assert_eq!(change.revoked_sessions, 1);
        let stored = db.find_user_by_id(target).await.unwrap().unwrap();
        assert_eq!(stored.status, STATUS_DISABLED);
    }

    #[tokio::test]
    async fn reenabling_a_user_does_not_revoke_sessions() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        set_user_status(&db, owner, target, STATUS_DISABLED)
            .await
            .expect("停用应当成功");
        let change = set_user_status(&db, owner, target, STATUS_ACTIVE)
            .await
            .expect("恢复应当成功");

        assert_eq!(change.current, STATUS_ACTIVE);
        assert_eq!(change.revoked_sessions, 0);
    }

    #[tokio::test]
    async fn rejects_disabling_oneself() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let _other = user(&db, "owner2", Role::Owner).await;

        let err = set_user_status(&db, owner, owner, STATUS_DISABLED)
            .await
            .expect_err("应当拒绝");

        assert!(matches!(err, NuonoError::PermissionDenied(_)), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_disabling_the_last_owner() {
        let db = db().await;
        let sole_owner = user(&db, "owner1", Role::Owner).await;
        let admin = user(&db, "staff", Role::Admin).await;

        let err = set_user_status(&db, admin, sole_owner, STATUS_DISABLED)
            .await
            .expect_err("应当拒绝");

        assert!(
            matches!(err, NuonoError::PermissionValidation(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_unsupported_target_status() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        let err = set_user_status(&db, owner, target, "pending")
            .await
            .expect_err("应当拒绝");

        assert!(matches!(err, NuonoError::ConfigValidation(_)), "{err:?}");
    }

    #[tokio::test]
    async fn same_status_is_a_no_op() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        let change = set_user_status(&db, owner, target, STATUS_ACTIVE)
            .await
            .expect("应当成功");

        assert_eq!(change.revoked_sessions, 0);
        assert_eq!(change.previous, change.current);
    }

    #[tokio::test]
    async fn deleting_a_user_removes_related_rows_and_sessions() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        // 给目标造关联数据：未消费令牌、登录会话。
        db.issue_user_token(target, crate::db::TOKEN_RESET_PASSWORD, 60, None)
            .await
            .unwrap();
        let account = db.find_user_by_id(target).await.unwrap().unwrap();
        db.create_session_for_verified_user(
            &account,
            &format!("auth_session_{}", hex::encode([3_u8; 32])),
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();

        let deletion = delete_user(&db, owner, target)
            .await
            .expect("无资产的账号应可删除");
        assert_eq!(deletion.username, "member");
        assert_eq!(deletion.revoked_sessions, 1);

        // 注销释放登录名，但保留编号，历史关联不会归到新人。
        let retired = db.find_user_by_id(target).await.unwrap().unwrap();
        assert_eq!(retired.status, "deleted");
        assert!(retired.username.is_empty());
        assert!(retired.password_hash.is_empty());
        let replacement = user(&db, "member", Role::User).await;
        assert!(replacement > target);
        assert_eq!(
            db.find_user_by_id(target).await.unwrap().unwrap().status,
            "deleted"
        );
        let tokens=db.orm().query_one_raw(Statement::from_sql_and_values(db.backend(),"SELECT COUNT(*) AS n FROM nuono_account_challenges WHERE subject_user_id=$1 AND state IN ('pending','candidate')",[target.into()])).await.unwrap().unwrap().try_get::<i64>("","n").unwrap();
        assert_eq!(tokens, 0);
    }

    #[tokio::test]
    async fn deleting_a_user_preserves_the_independent_group() {
        let db = db().await;
        let owner = user(&db, "owner1", Role::Owner).await;
        let target = user(&db, "member", Role::User).await;

        let group = db
            .ensure_group(
                &nuono_storage::groups::GroupIdentity::onebot("999"),
                "测试群",
            )
            .await
            .unwrap();

        delete_user(&db, owner, target)
            .await
            .expect("注销不删除群对象");
        assert_eq!(
            db.find_user_by_id(target).await.unwrap().unwrap().status,
            "deleted"
        );
        assert!(db.group_by_id(group.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rejects_deleting_oneself_and_last_owner() {
        let db = db().await;
        let sole_owner = user(&db, "owner1", Role::Owner).await;

        let err = delete_user(&db, sole_owner, sole_owner)
            .await
            .expect_err("应拒绝删除自己");
        assert!(matches!(err, NuonoError::PermissionDenied(_)), "{err:?}");

        let admin = user(&db, "staff", Role::Admin).await;
        let err = delete_user(&db, admin, sole_owner)
            .await
            .expect_err("应拒绝删除最后一位 owner");
        assert!(
            matches!(err, NuonoError::PermissionValidation(_)),
            "{err:?}"
        );
        assert!(db.find_user_by_id(sole_owner).await.unwrap().is_some());
    }
}
