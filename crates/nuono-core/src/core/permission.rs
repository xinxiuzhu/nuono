pub use nuono_plugin::permission::*;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use nuono_app::auth::Role;
    use nuono_domain::{IdentityKind, IdentityRef, IdentityScheme, Platform};
    use sea_orm::ConnectionTrait;

    async fn test_db() -> Database {
        crate::test_support::test_db(1).await
    }

    #[test]
    fn test_resolve_everyone() {
        let level = resolve_chat_level(None, None);
        assert_eq!(level, PermissionLevel::Everyone);
    }

    #[test]
    fn test_resolve_group_admin() {
        let level = resolve_chat_level(Some("admin"), None);
        assert_eq!(level, PermissionLevel::GroupAdmin);
    }

    #[test]
    fn test_resolve_group_owner() {
        let level = resolve_chat_level(Some("owner"), None);
        assert_eq!(level, PermissionLevel::GroupOwner);
    }

    #[test]
    fn test_bound_account_role_grants_chat_admin_automatically() {
        assert_eq!(
            resolve_chat_level(None, Some(Role::Owner)),
            PermissionLevel::BotOwner
        );
        assert_eq!(
            resolve_chat_level(None, Some(Role::Admin)),
            PermissionLevel::BotAdmin
        );
        assert_eq!(
            resolve_chat_level(Some("owner"), Some(Role::User)),
            PermissionLevel::GroupOwner
        );
    }

    #[test]
    fn test_has_permission_monotonic_and_banned_denied() {
        assert!(PermissionLevel::BotOwner.has_permission(PermissionLevel::Everyone));
        assert!(PermissionLevel::GroupOwner.has_permission(PermissionLevel::GroupAdmin));
        assert!(!PermissionLevel::GroupAdmin.has_permission(PermissionLevel::GroupOwner));
        assert!(!PermissionLevel::Everyone.has_permission(PermissionLevel::BotOwner));
        assert!(!ActorPermission::Banned.has_permission(PermissionLevel::Everyone));
        assert!(
            ActorPermission::Allowed(PermissionLevel::BotAdmin)
                .has_permission(PermissionLevel::GroupOwner)
        );
    }

    #[tokio::test]
    async fn bound_owner_account_inherits_bot_owner_and_isolates_platforms_and_namespaces() {
        let db = test_db().await;
        let owner = db
            .create_user(crate::db::NewUser {
                username: "perm_owner".into(),
                email: None,
                password: "perm-owner-password".into(),
                role: "owner".into(),
                status: "active".into(),
                email_verified: false,
            })
            .await
            .unwrap();
        let admin = db
            .create_user(crate::db::NewUser {
                username: "perm_admin".into(),
                email: None,
                password: "perm-admin-password".into(),
                role: "admin".into(),
                status: "active".into(),
                email_verified: false,
            })
            .await
            .unwrap();

        let onebot_id = IdentityRef::new(
            Platform::OneBot,
            IdentityKind::User,
            IdentityScheme::Native,
            "778899",
            None::<String>,
        )
        .unwrap();
        let bound_onebot = db
            .admin_bind_platform_identity(owner.id, owner.id, &onebot_id)
            .await
            .unwrap();

        let qq_c2c = IdentityRef::new(
            Platform::QqOfficial,
            IdentityKind::User,
            IdentityScheme::OpenId,
            "OPENID_OWNER",
            Some("1020001"),
        )
        .unwrap();
        db.admin_bind_platform_identity(owner.id, owner.id, &qq_c2c)
            .await
            .unwrap();

        let feishu_admin = IdentityRef::new(
            Platform::Feishu,
            IdentityKind::User,
            IdentityScheme::OpenId,
            "ou_admin",
            Some("cli_app_a"),
        )
        .unwrap();
        db.admin_bind_platform_identity(owner.id, admin.id, &feishu_admin)
            .await
            .unwrap();

        let manager = PermissionManager::new(db.clone());

        // OneBot 已绑定站长身份自动继承 BotOwner；不同平台相同数字 ID 不应跨平台越权。
        let onebot_event = serde_json::json!({
            "platform": "onebot",
            "user_id": 778899,
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&onebot_event).await,
            ActorPermission::Allowed(PermissionLevel::BotOwner)
        );
        let telegram_same_id = serde_json::json!({
            "platform": "telegram",
            "user_id": 778899,
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&telegram_same_id).await,
            ActorPermission::Allowed(PermissionLevel::Everyone)
        );

        // 官 Q 私聊命名空间精确匹配；不同应用或群聊命名空间必须严格隔离。
        let qq_c2c_event = serde_json::json!({
            "platform": "qq_official",
            "message_type": "private",
            "route": { "platform": "qq_official", "bot_self_id": "1020001" },
            "user_id": "OPENID_OWNER",
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&qq_c2c_event).await,
            ActorPermission::Allowed(PermissionLevel::BotOwner)
        );
        let qq_other_app = serde_json::json!({
            "platform": "qq_official",
            "message_type": "private",
            "route": { "platform": "qq_official", "bot_self_id": "1029999" },
            "user_id": "OPENID_OWNER",
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&qq_other_app).await,
            ActorPermission::Allowed(PermissionLevel::Everyone)
        );
        let qq_group_event = serde_json::json!({
            "platform": "qq_official",
            "message_type": "group",
            "route": { "platform": "qq_official", "bot_self_id": "1020001" },
            "group_openid": "GROUP_1",
            "user_id": "OPENID_OWNER",
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&qq_group_event).await,
            ActorPermission::Allowed(PermissionLevel::Everyone)
        );

        // 飞书按应用隔离命名空间，且管理员账号继承 BotAdmin。
        let feishu_ok = serde_json::json!({
            "platform": "feishu",
            "self_id": "cli_app_a",
            "user_id": "ou_admin",
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&feishu_ok).await,
            ActorPermission::Allowed(PermissionLevel::BotAdmin)
        );
        let feishu_other_app = serde_json::json!({
            "platform": "feishu",
            "self_id": "cli_app_b",
            "user_id": "ou_admin",
            "sender": { "role": "member" }
        });
        assert_eq!(
            manager.evaluate(&feishu_other_app).await,
            ActorPermission::Allowed(PermissionLevel::Everyone)
        );

        // 解绑后聊天权限立即收回。
        db.admin_unbind_platform_identity(
            owner.id,
            owner.id,
            bound_onebot.id.parse().unwrap(),
            bound_onebot.revision.parse().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            manager.evaluate(&onebot_event).await,
            ActorPermission::Allowed(PermissionLevel::Everyone)
        );

        drop(manager);
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn permission_query_error_is_distinct_and_runtime_denies() {
        let db = test_db().await;
        // 测试桩：通过显式 DDL 删除 global_configs 表，模拟查询该表失败的场景。
        db.orm()
            .execute_raw(sea_orm::Statement::from_string(
                db.backend(),
                "DROP TABLE nuono_global_configs",
            ))
            .await
            .unwrap();
        let manager = PermissionManager::new(db.clone());
        let event = serde_json::json!({
            "user_id": 12345,
            "sender": { "role": "owner" }
        });

        assert!(manager.try_evaluate(&event).await.is_err());
        assert_eq!(manager.evaluate(&event).await, ActorPermission::Banned);

        drop(manager);
        db.shutdown().await.unwrap();
    }
}
