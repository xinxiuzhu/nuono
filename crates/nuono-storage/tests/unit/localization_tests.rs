use super::*;
use nuono_app::localization::{LocaleService, LocaleSource};
use nuono_domain::{
    BotIdentity, ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform,
};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use std::sync::Arc;

fn identity(
    platform: Platform,
    kind: IdentityKind,
    id: &str,
    namespace: Option<&str>,
) -> IdentityRef {
    IdentityRef::new(platform, kind, IdentityScheme::Native, id, namespace).unwrap()
}

fn bot(platform: Platform, id: &str, namespace: Option<&str>) -> BotIdentity {
    BotIdentity::new(
        "instance",
        identity(platform, IdentityKind::Bot, id, namespace),
        None,
    )
    .unwrap()
}

fn user_scope(platform: Platform, bot_id: &str, namespace: Option<&str>) -> LocaleScope {
    LocaleScope::user(
        bot(platform.clone(), bot_id, namespace),
        identity(platform, IdentityKind::User, "42", namespace),
    )
    .unwrap()
}

fn group_scope() -> LocaleScope {
    LocaleScope::conversation(
        bot(Platform::OneBot, "bot", None),
        ConversationRef::group(identity(Platform::OneBot, IdentityKind::Group, "42", None))
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn locale_preferences_persist_update_and_delete_overrides_without_freezing_inheritance() {
    let db = crate::test_support::test_db(2).await;
    let user = user_scope(Platform::OneBot, "bot", None);
    let group = group_scope();
    let service = LocaleService::new(Arc::new(db.clone()));
    service.set_system_locale(Locale::En);
    assert_eq!(db.get_locale_preference(&user).await.unwrap(), None);
    service
        .set_preference(&group, Some(Locale::Ja))
        .await
        .unwrap();
    service
        .set_preference(&user, Some(Locale::Ko))
        .await
        .unwrap();
    assert_eq!(
        service
            .resolve(None, Some(&user), Some(&group))
            .await
            .unwrap()
            .locale,
        Locale::Ko
    );
    // 全新创建的 LocaleService 实例应直接从数据库读取已持久化的语言偏好，而非依赖进程内的本地快照。
    let fresh = LocaleService::new(Arc::new(db.clone()));
    assert_eq!(
        fresh.resolve_target(None, &user).await.unwrap().locale,
        Locale::Ko
    );
    service
        .set_preference(&user, Some(Locale::En))
        .await
        .unwrap();
    assert_eq!(
        crate::entity::global_configs::Entity::find()
            .filter(crate::entity::global_configs::Column::Key.starts_with(LOCALE_PREFIX))
            .count(db.orm())
            .await
            .unwrap(),
        2
    );
    service.set_preference(&user, None).await.unwrap();
    assert_eq!(fresh.get_preference(&user).await.unwrap(), None);
    let inherited = service
        .resolve(None, Some(&user), Some(&group))
        .await
        .unwrap();
    assert_eq!(inherited.locale, Locale::Ja);
    assert_eq!(inherited.source, LocaleSource::Conversation);
    service.set_preference(&group, None).await.unwrap();
    assert_eq!(
        crate::entity::global_configs::Entity::find()
            .filter(crate::entity::global_configs::Column::Key.starts_with(LOCALE_PREFIX))
            .count(db.orm())
            .await
            .unwrap(),
        0
    );
    service.set_system_locale(Locale::Ko);
    assert_eq!(
        service.resolve_target(None, &group).await.unwrap().locale,
        Locale::Ko
    );
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn locale_preferences_isolate_platform_bot_namespace_and_conversation_type() {
    let db = crate::test_support::test_db(2).await;
    let original = user_scope(Platform::OneBot, "bot", None);
    db.set_locale_preference(&original, Some(Locale::Ja))
        .await
        .unwrap();
    let mut unrelated = vec![
        user_scope(Platform::Telegram, "bot", None),
        user_scope(Platform::OneBot, "other-bot", None),
        user_scope(Platform::OneBot, "bot", Some("app-a")),
        user_scope(Platform::OneBot, "bot", Some("app-b")),
        group_scope(),
    ];
    for parent in ["parent-a", "parent-b"] {
        unrelated.push(
            LocaleScope::conversation(
                bot(Platform::OneBot, "bot", None),
                ConversationRef::channel(
                    identity(Platform::OneBot, IdentityKind::Channel, "42", None),
                    Some(identity(
                        Platform::OneBot,
                        IdentityKind::Organization,
                        parent,
                        None,
                    )),
                )
                .unwrap(),
            )
            .unwrap(),
        );
    }
    for scope in &unrelated {
        assert_eq!(db.get_locale_preference(scope).await.unwrap(), None);
        db.set_locale_preference(scope, Some(Locale::Ko))
            .await
            .unwrap();
    }
    assert_eq!(
        db.get_locale_preference(&original).await.unwrap(),
        Some(Locale::Ja)
    );
    assert_eq!(
        crate::entity::global_configs::Entity::find()
            .filter(crate::entity::global_configs::Column::Key.starts_with(LOCALE_PREFIX))
            .count(db.orm())
            .await
            .unwrap(),
        (unrelated.len() + 1) as u64
    );
    db.set_locale_preference(&unrelated[0], None).await.unwrap();
    for scope in &unrelated[1..] {
        assert_eq!(
            db.get_locale_preference(scope).await.unwrap(),
            Some(Locale::Ko)
        );
    }
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn locale_preferences_reject_corrupt_identity_and_database_rejects_unknown_language() {
    let db = crate::test_support::test_db(2).await;
    let user = user_scope(Platform::OneBot, "bot", None);
    db.set_locale_preference(&user, Some(Locale::Ja))
        .await
        .unwrap();
    let key = config_key(&user.canonical_key().unwrap());
    db.set_global_config(
        &key,
        serde_json::json!({
            "scope_key": user.canonical_key().unwrap(),
            "scope": user,
            "locale": "unknown",
        }),
    )
    .await
    .unwrap();
    assert!(db.get_locale_preference(&user).await.is_err());
    db.set_global_config(
        &key,
        serde_json::json!({
            "scope_key": user.canonical_key().unwrap(),
            "scope": group_scope(),
            "locale": "ja",
        }),
    )
    .await
    .unwrap();
    assert!(db.get_locale_preference(&user).await.is_err());
    db.shutdown().await.unwrap();
}
