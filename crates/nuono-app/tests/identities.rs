use nuono_app::agent::{AgentContext, AgentNativeContext};
use nuono_domain::{
    BotIdentity, ConversationRef, IdentityKind, IdentityRef, IdentityScheme, Platform,
};
fn context(instance: &str, namespace: &str, actor_id: &str) -> AgentContext {
    let actor = IdentityRef::new(
        Platform::Feishu,
        IdentityKind::User,
        IdentityScheme::OpenId,
        actor_id,
        Some(namespace),
    )
    .unwrap();
    let group = IdentityRef::new(
        Platform::Feishu,
        IdentityKind::Group,
        IdentityScheme::ChatId,
        "oc_group",
        Some(namespace),
    )
    .unwrap();
    let bot = BotIdentity::new(
        instance,
        IdentityRef::new(
            Platform::Feishu,
            IdentityKind::Bot,
            IdentityScheme::AppId,
            "cli_bot",
            None::<String>,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    AgentContext {
        account_chat: None,
        images: Vec::new(),
        reply_to_message_id: None,
        persona: None,
        native: Some(AgentNativeContext {
            bot,
            actor,
            conversation: Some(ConversationRef::group(group).unwrap()),
            mentioned: false,
        }),
        bot_id: 0,
        user_id: 0,
        group_id: None,
        platform: "feishu".into(),
        message_type: "group".into(),
        channel_id: None,
        message_id: None,
        raw_message: "test".into(),
        content: "test".into(),
    }
}
#[test]
fn native_agent_identity_and_memory_are_separated_by_namespace_and_instance() {
    let a = context("one", "app-a", "ou_native");
    let other_instance = context("two", "app-a", "ou_native");
    let other_app = context("one", "app-b", "ou_native");
    let other_user = context("one", "app-a", "ou_other");
    assert_eq!(a.user_identity(), "ou_native");
    assert_eq!(a.group_identity().as_deref(), Some("oc_group"));
    assert!(a.session_id().starts_with("group:"));
    assert_ne!(a.session_id(), other_instance.session_id());
    assert_eq!(a.user_scope_key(), other_instance.user_scope_key());
    assert_ne!(a.user_scope_key(), other_app.user_scope_key());
    assert_ne!(a.group_scope_key(), other_app.group_scope_key());
    assert_eq!(a.session_id(), other_user.session_id());
    assert_ne!(a.base_session_id(), other_user.base_session_id());
    let restored: AgentContext = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
    assert_eq!(restored.session_id(), a.session_id());
}

#[test]
fn persona_binding_survives_reconnection_but_preserves_native_namespaces() {
    use nuono_app::persona::PersonaBindingKey;
    let first = PersonaBindingKey::from_context(&context("one", "app-a", "ou_native")).unwrap();
    let reconnect = PersonaBindingKey::from_context(&context("two", "app-a", "ou_other")).unwrap();
    let other_app = PersonaBindingKey::from_context(&context("one", "app-b", "ou_native")).unwrap();
    assert_eq!(first, reconnect);
    assert_ne!(first.canonical_key(), other_app.canonical_key());
    assert_eq!(first.bot_id, "cli_bot");
    assert_eq!(first.group_id, "oc_group");
    assert_eq!(first.namespace.as_deref(), Some("app-a"));
    first.validate().unwrap();
}
