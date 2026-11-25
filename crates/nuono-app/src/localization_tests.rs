use super::*;
use nuono_domain::{IdentityScheme, Platform};

fn identity(
    platform: Platform,
    kind: IdentityKind,
    id: &str,
    namespace: Option<&str>,
) -> IdentityRef {
    IdentityRef::new(platform, kind, IdentityScheme::Native, id, namespace).unwrap()
}

fn bot(platform: Platform, id: &str, instance: &str, namespace: Option<&str>) -> BotIdentity {
    BotIdentity::new(
        instance,
        identity(platform, IdentityKind::Bot, id, namespace),
        None,
    )
    .unwrap()
}

fn scopes() -> (LocaleScope, LocaleScope) {
    let bot = bot(Platform::OneBot, "bot", "instance", None);
    let user = LocaleScope::user(
        bot.clone(),
        identity(Platform::OneBot, IdentityKind::User, "42", None),
    )
    .unwrap();
    let group = LocaleScope::conversation(
        bot,
        ConversationRef::group(identity(Platform::OneBot, IdentityKind::Group, "42", None))
            .unwrap(),
    )
    .unwrap();
    (user, group)
}

#[test]
fn explicit_locale_values_are_strict_and_language_hints_are_optional() {
    assert_eq!(Locale::default(), Locale::ZhCn);
    for locale in Locale::ALL {
        assert_eq!(Locale::parse_explicit(locale.as_str()).unwrap(), locale);
        assert_eq!(locale.as_str().parse::<Locale>().unwrap(), locale);
        assert_eq!(
            serde_json::from_value::<Locale>(serde_json::to_value(locale).unwrap()).unwrap(),
            locale
        );
    }
    for invalid in [
        "", "auto", "EN", "en-US", "ja-JP", "zh", "zh-cn", " en", "fr",
    ] {
        assert!(Locale::parse_explicit(invalid).is_err(), "{invalid:?}");
        assert!(serde_json::from_value::<Locale>(serde_json::json!(invalid)).is_err());
    }
    for (hint, expected) in [
        ("zh", Locale::ZhCn),
        ("zh-Hans-CN", Locale::ZhCn),
        ("en-US", Locale::En),
        ("EN_gb", Locale::En),
        ("ja-JP", Locale::Ja),
        ("ko-KR", Locale::Ko),
    ] {
        assert_eq!(Locale::from_language_tag(hint), Some(expected));
    }
    for invalid in [
        "fr",
        "zh-TW",
        "zh-Hant",
        "en-",
        "en-US;q=1",
        "en-US-bad",
        "english",
        "auto",
        "en-pirate",
    ] {
        assert_eq!(Locale::from_language_tag(invalid), None, "{invalid:?}");
    }
}

#[test]
fn scope_keys_separate_every_native_identity_component() {
    let (user, group) = scopes();
    let mut all = vec![user, group];
    for (platform, bot_id, instance, namespace) in [
        (Platform::Telegram, "bot", "instance", None),
        (Platform::OneBot, "other-bot", "instance", None),
        (Platform::OneBot, "bot", "other-instance", None),
        (Platform::OneBot, "bot", "instance", Some("app-a")),
        (Platform::OneBot, "bot", "instance", Some("app-b")),
    ] {
        all.push(
            LocaleScope::user(
                bot(platform.clone(), bot_id, instance, namespace),
                identity(platform, IdentityKind::User, "42", namespace),
            )
            .unwrap(),
        );
    }
    for parent in [None, Some("parent-a"), Some("parent-b")] {
        all.push(
            LocaleScope::conversation(
                bot(Platform::OneBot, "bot", "instance", None),
                ConversationRef::channel(
                    identity(Platform::OneBot, IdentityKind::Channel, "42", None),
                    parent
                        .map(|id| identity(Platform::OneBot, IdentityKind::Organization, id, None)),
                )
                .unwrap(),
            )
            .unwrap(),
        );
    }
    let keys: std::collections::HashSet<_> = all
        .iter()
        .map(|scope| scope.canonical_key().unwrap())
        .collect();
    assert_eq!(keys.len(), all.len());
    for scope in all {
        let json = scope.canonical_json().unwrap();
        let restored: LocaleScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, restored);
        assert_eq!(
            scope.canonical_key().unwrap(),
            restored.canonical_key().unwrap()
        );
    }
}

#[test]
fn malformed_scope_identity_is_rejected_including_direct_enum_construction() {
    let bot = bot(Platform::OneBot, "bot", "instance", None);
    assert!(
        LocaleScope::user(
            bot.clone(),
            identity(Platform::OneBot, IdentityKind::Group, "42", None)
        )
        .is_err()
    );
    assert!(
        LocaleScope::user(
            bot.clone(),
            identity(Platform::Telegram, IdentityKind::User, "42", None)
        )
        .is_err()
    );
    let invalid = LocaleScope::Conversation {
        bot,
        conversation: ConversationRef::Group {
            group: identity(Platform::OneBot, IdentityKind::User, "42", None),
        },
    };
    assert!(invalid.validate().is_err());
    assert!(invalid.canonical_key().is_err());
}

#[tokio::test]
async fn locale_resolution_precedence_removal_and_snapshots_preserve_inheritance() {
    let service = LocaleService::default();
    let (user, group) = scopes();
    let initial = service
        .resolve(None, Some(&user), Some(&group))
        .await
        .unwrap();
    assert_eq!(initial, LocaleContext::default());
    service.set_system_locale(Locale::En);
    service
        .set_preference(&group, Some(Locale::Ja))
        .await
        .unwrap();
    assert_eq!(
        service
            .resolve(None, Some(&user), Some(&group))
            .await
            .unwrap(),
        LocaleContext {
            locale: Locale::Ja,
            source: LocaleSource::Conversation
        }
    );
    service
        .set_preference(&user, Some(Locale::Ko))
        .await
        .unwrap();
    let snapshot = service
        .resolve(None, Some(&user), Some(&group))
        .await
        .unwrap();
    assert_eq!(
        snapshot,
        LocaleContext {
            locale: Locale::Ko,
            source: LocaleSource::User
        }
    );
    assert_eq!(
        service
            .resolve(Some(Locale::ZhCn), Some(&user), Some(&group))
            .await
            .unwrap(),
        LocaleContext {
            locale: Locale::ZhCn,
            source: LocaleSource::Explicit
        }
    );
    service.set_preference(&user, None).await.unwrap();
    assert_eq!(service.get_preference(&user).await.unwrap(), None);
    assert_eq!(
        service
            .resolve(None, Some(&user), Some(&group))
            .await
            .unwrap()
            .locale,
        Locale::Ja
    );
    service.set_preference(&group, None).await.unwrap();
    assert_eq!(
        service
            .resolve(None, Some(&user), Some(&group))
            .await
            .unwrap(),
        LocaleContext {
            locale: Locale::En,
            source: LocaleSource::System
        }
    );
    assert_eq!(snapshot.locale, Locale::Ko);
    assert_eq!(initial.locale, Locale::ZhCn);
}

#[tokio::test]
async fn push_resolution_uses_recipient_and_independent_service_stores_do_not_leak() {
    let service = LocaleService::default();
    let other = LocaleService::default();
    let (user, group) = scopes();
    service
        .set_preference(&user, Some(Locale::Ko))
        .await
        .unwrap();
    service
        .set_preference(&group, Some(Locale::Ja))
        .await
        .unwrap();
    let (direct, group_push, isolated) = tokio::join!(
        service.resolve_target(None, &user),
        service.resolve_target(None, &group),
        other.resolve_target(None, &user),
    );
    assert_eq!(direct.unwrap().locale, Locale::Ko);
    assert_eq!(group_push.unwrap().locale, Locale::Ja);
    assert_eq!(isolated.unwrap(), LocaleContext::default());
    assert_eq!(
        service
            .resolve_target(Some(Locale::En), &group)
            .await
            .unwrap()
            .source,
        LocaleSource::Explicit
    );
}

#[tokio::test]
async fn mismatched_scope_roles_and_bot_contexts_are_rejected() {
    let service = LocaleService::default();
    let (user, group) = scopes();
    assert!(service.resolve(None, Some(&group), None).await.is_err());
    assert!(service.resolve(None, None, Some(&user)).await.is_err());
    let LocaleScope::Conversation { conversation, .. } = group else {
        unreachable!()
    };
    let different_bot = LocaleScope::conversation(
        bot(Platform::OneBot, "other", "instance", None),
        conversation,
    )
    .unwrap();
    assert!(
        service
            .resolve(None, Some(&user), Some(&different_bot))
            .await
            .is_err()
    );
}

struct DynamicDefault(RwLock<Locale>);
#[async_trait::async_trait]
impl SystemLocaleProvider for DynamicDefault {
    async fn system_locale(&self) -> Result<Locale> {
        Ok(*self.0.read().unwrap())
    }
}

#[tokio::test]
async fn background_sends_observe_live_system_default_without_new_inbound_events() {
    let provider = Arc::new(DynamicDefault(RwLock::new(Locale::Ja)));
    let service = LocaleService::default().with_system_locale_provider(provider.clone());
    let background = service.clone();
    let (_, group) = scopes();
    assert_eq!(
        background
            .resolve_target(None, &group)
            .await
            .unwrap()
            .locale,
        Locale::Ja
    );
    *provider.0.write().unwrap() = Locale::Ko;
    service.set_system_locale(Locale::En);
    assert_eq!(
        background
            .resolve_target(None, &group)
            .await
            .unwrap()
            .locale,
        Locale::Ko
    );
    service
        .set_preference(&group, Some(Locale::En))
        .await
        .unwrap();
    assert_eq!(
        background
            .resolve_target(None, &group)
            .await
            .unwrap()
            .locale,
        Locale::En
    );
}

struct FailingStore;

#[async_trait::async_trait]
impl LocalePreferenceStore for FailingStore {
    async fn get_locale_preference(&self, _: &LocaleScope) -> Result<Option<Locale>> {
        Err(NuonoError::DatabaseQuery("fixture unavailable".into()))
    }

    async fn set_locale_preference(&self, _: &LocaleScope, _: Option<Locale>) -> Result<()> {
        Err(NuonoError::DatabaseQuery("fixture unavailable".into()))
    }
}

#[tokio::test]
async fn failed_preferences_are_reported_without_turning_errors_into_inherited_values() {
    let service = LocaleService::new(Arc::new(FailingStore));
    let (user, group) = scopes();
    assert!(
        service
            .resolve(None, Some(&user), Some(&group))
            .await
            .is_err()
    );
    assert!(service.resolve_target(None, &group).await.is_err());
    assert!(
        service
            .set_preference(&user, Some(Locale::Ko))
            .await
            .is_err()
    );
    // 显式指定语言时无需读取存储层偏好；存储故障也不妨碍调用方直接读取系统默认语言并自行上报存储异常。
    assert_eq!(
        service
            .resolve(Some(Locale::Ja), Some(&user), Some(&group))
            .await
            .unwrap()
            .locale,
        Locale::Ja
    );
    assert_eq!(service.system_locale().await.unwrap(), Locale::ZhCn);
}
